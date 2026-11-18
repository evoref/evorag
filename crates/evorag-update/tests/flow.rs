//! `evorag update check`・`apply`・`rollback` の流れ (12 §7.2)。
//!
//! 通信は偽の Releases ([`Server`]、試験の鍵で署名したもの)、`--version` は偽物 ([`Probe`]) が、版を
//! 書いた小さな実行ファイルの中身から答える。通信も子のプロセスも使わない。

mod common;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{TestKey, comment_for, key, keyring, zip_archive};
use evorag_store::testutil::TempDir;
use evorag_update::flow::{
    self, ApplyOutcome, CheckOutcome, Context, DataVerdict, FlowError, Notice,
};
use evorag_update::net::{Fetcher, NetError, Resource, policy};
use evorag_update::swap::{self, Build, EXE_NAME, Env, ProbeError, Startup, VersionProbe};
use evorag_update::{Keyring, Sha256Digest, UpdateError, Version};

const TARGET: &str = "x86_64-pc-windows-msvc";
const CURRENT: &str = "0.3.0";
const RELEASE_DATE: &str = "2026-11-01";

fn v(s: &str) -> Version {
    s.parse().expect("a test version")
}

/// インストール先に実行ファイルを置く。Unix では権限を `0o755` に決める (umask に頼らない。group か
/// other が書けると、インストール先の確かめが断る)。
fn put(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write an executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// 版を書いた、試験の実行ファイルの中身。
fn exe(version: &str) -> Vec<u8> {
    format!("evorag-fixture {version}\n")
        .repeat(40)
        .into_bytes()
}

/// `--version` の偽物。中身の版を答える。`crash_on` の版を動かすと panic する (強制終了の模擬)。
#[derive(Default)]
struct Probe {
    crash_on: Option<&'static str>,
    calls: Cell<usize>,
}

impl VersionProbe for Probe {
    fn run_version(&self, exe: &Path) -> Result<String, ProbeError> {
        self.calls.set(self.calls.get() + 1);
        assert!(exe.is_absolute(), "{}", exe.display());
        let bytes = std::fs::read(exe).map_err(ProbeError::Spawn)?;
        let text = String::from_utf8_lossy(&bytes);
        let version = text
            .strip_prefix("evorag-fixture ")
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("?")
            .to_owned();
        assert!(
            self.crash_on != Some(version.as_str()),
            "the machine stopped while checking {version}"
        );
        Ok(format!("evorag {version}\ncommit: test\n"))
    }
}

/// 試験の `release.json` と配布物。
struct Rel {
    version: String,
    json: serde_json::Value,
    archive: Vec<u8>,
}

impl Rel {
    fn new(version: &str) -> Self {
        let name = format!("evorag-{version}-{TARGET}.zip");
        let entry = format!("evorag-{version}-{TARGET}/evorag.exe");
        let exe = exe(version);
        let archive = zip_archive(&[(entry.as_str(), exe.as_slice())]);
        let json = serde_json::json!({
            "schema": 1,
            "name": "evorag",
            "version": version,
            "date": "2026-12-01",
            "upgrade_from": "0.2.0",
            "notes": "要点\r\n- 速くした\u{1b}[31m\u{202E}",
            "artifacts": [{
                "target": TARGET,
                "name": name,
                "sha256": Sha256Digest::of(&archive).to_string(),
                "size": archive.len(),
                "exe": entry,
                "exe_sha256": Sha256Digest::of(&exe).to_string(),
            }],
            "db_format": "0.3",
            "db_min_write": "0.1",
            "llama_server_min_build": null,
        });
        Self {
            version: version.to_owned(),
            json,
            archive,
        }
    }

    fn name(&self) -> String {
        self.json["artifacts"][0]["name"]
            .as_str()
            .expect("name")
            .to_owned()
    }

    fn signed(&self, key: &TestKey) -> (Vec<u8>, Vec<u8>) {
        let json = serde_json::to_vec_pretty(&self.json).expect("json");
        let sig = key.sign(&json, &comment_for(&self.version, &json));
        (json, sig)
    }
}

/// 偽の Releases。固定の URL だけに答え、受けた要求を記録する。大きさの上限は本番と同じく守る。
#[derive(Default)]
struct Server {
    files: RefCell<HashMap<String, Vec<u8>>>,
    requests: RefCell<Vec<String>>,
    preflights: Cell<usize>,
    cpu_missing: Option<&'static str>,
}

impl Server {
    fn put(&self, url: String, bytes: Vec<u8>) {
        self.files.borrow_mut().insert(url, bytes);
    }

    fn list(&self, tags: &[(&str, bool, bool)]) {
        let items: Vec<_> = tags
            .iter()
            .map(|(t, draft, pre)| {
                serde_json::json!({
                    "tag_name": t, "draft": draft, "prerelease": pre,
                    "body": "\u{1b}[2J not shown", "published_at": "2026-12-02T00:00:00Z",
                })
            })
            .collect();
        self.put(
            policy::RELEASES_URL.to_owned(),
            serde_json::to_vec(&items).expect("json"),
        );
    }

    /// `rel` を `key` で署名して置く (`release.json`・`.minisig`・配布物)。
    fn publish(&self, rel: &Rel, key: &TestKey) {
        let (json, sig) = rel.signed(key);
        self.publish_raw(rel, json, sig);
    }

    fn publish_raw(&self, rel: &Rel, json: Vec<u8>, sig: Vec<u8>) {
        let version = v(&rel.version);
        self.put(policy::download_url(version, "release.json"), json);
        self.put(policy::download_url(version, "release.json.minisig"), sig);
        self.put(
            policy::download_url(version, &rel.name()),
            rel.archive.clone(),
        );
    }

    fn requested(&self) -> Vec<String> {
        self.requests.borrow().clone()
    }
}

impl Fetcher for Server {
    fn preflight(&self) -> Result<(), NetError> {
        self.preflights.set(self.preflights.get() + 1);
        match self.cpu_missing {
            Some(f) => Err(NetError::CpuUnsupported { missing: vec![f] }),
            None => Ok(()),
        }
    }

    fn fetch(&self, url: &str, resource: Resource) -> Result<Vec<u8>, NetError> {
        assert!(self.preflights.get() > 0, "preflight runs before network");
        self.requests.borrow_mut().push(url.to_owned());
        policy::check_url(url)?;
        let bytes = self
            .files
            .borrow()
            .get(url)
            .cloned()
            .ok_or(NetError::NotFound)?;
        if bytes.len() as u64 > resource.max_bytes() {
            return Err(NetError::TooLarge {
                limit: resource.max_bytes(),
            });
        }
        Ok(bytes)
    }
}

/// インストール先・状態のフォルダ・データ根。
struct Fx {
    _root: TempDir,
    install: PathBuf,
    exe: PathBuf,
    state: PathBuf,
    data: PathBuf,
    config: PathBuf,
    build: Build,
    keys: Keyring,
}

impl Fx {
    fn new() -> Self {
        Self::with_db_format("0.3")
    }

    /// 動いている版 ([`CURRENT`]) が書く DB の形式の版を `db_format` にする。
    fn with_db_format(db_format: &str) -> Self {
        let root = TempDir::new("update-flow");
        let install = root.path().join("install");
        // umask に頼らず、所有者だけが書けるフォルダにする (インストール先の確かめに通るように)。
        evorag_store::fsutil::create_private_dir(&install).expect("mkdir");
        put(&install.join(EXE_NAME), &exe(CURRENT));
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("mkdir");
        Self {
            exe: install.join(EXE_NAME),
            install,
            state: root.path().join("state"),
            data: root.path().join("data"),
            config: config_dir.join("config.toml"),
            build: Build::new(v(CURRENT), Some(db_format.to_owned())),
            keys: keyring(&[&key()]),
            _root: root,
        }
    }

    fn exe_path(&self) -> &Path {
        &self.exe
    }

    fn ctx<'a>(&'a self, server: &'a Server, probe: &'a Probe, channel: &'a str) -> Context<'a> {
        Context {
            current: &self.build,
            target: TARGET,
            channel,
            release_date: Some(RELEASE_DATE),
            keys: &self.keys,
            fetcher: server,
            probe,
            running_exe: &self.exe,
            state_dir: &self.state,
            data_root: &self.data,
            backup_dir: None,
            config_file: Some(&self.config),
            config_version: Some(1),
        }
    }

    fn digest(&self, name: &str) -> Option<Sha256Digest> {
        std::fs::read(self.install.join(name))
            .ok()
            .map(|b| Sha256Digest::of(&b))
    }

    /// 試験のデータ根を、`format.min_read`・`format.min_write` を決めて作る。
    fn make_data_root(&self, min_read: &str, min_write: &str) {
        let clock = Arc::new(evorag_types::SystemClock);
        let mut plan = evorag_store::migrate::FormatPlan::current();
        plan.min_read = min_read.to_owned();
        plan.min_write = min_write.to_owned();
        let (store, _) = evorag_store::Store::open_with(
            evorag_store::OpenOptions {
                data_root: self.data.clone(),
                evorag_version: CURRENT.into(),
                kinds: Vec::new(),
                clock: clock.clone(),
                ids: Arc::new(evorag_types::UuidV7Gen::new(clock)),
                config_file: None,
            },
            plan,
        )
        .expect("open the test data root");
        drop(store);
    }
}

/// フォルダの中身 (相対パス → 中身)。データ根のロックのファイル (`lock`・`lock.pid`) は除く。
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).expect("inside").to_path_buf();
                if rel == Path::new("lock") || rel == Path::new("lock.pid") {
                    continue;
                }
                out.insert(rel, std::fs::read(&path).expect("read"));
            }
        }
    }
    out
}

fn collect(log: &RefCell<Vec<String>>) -> impl FnMut(Notice) + '_ {
    move |n| log.borrow_mut().push(format!("{n:?}"))
}

/// verifies: AC-FR-803-1 (偽の Releases に今より新しい版があるとき、semver でいちばん大きい版の検証した
/// 版・日付・除いた要点を示して 0。ロックを取らず、データ根・インストール先・`update/` に触れず、固定の
/// 接続先だけに繋ぐ)
#[test]
fn check_shows_the_newest_verified_release_and_touches_nothing() {
    let fx = Fx::new();
    let server = Server::default();
    server.list(&[
        ("v0.3.0", false, false),
        ("v0.10.0", false, false),
        ("v0.4.0", false, false),
        ("v0.11.0", true, false),
        ("v0.12.0", false, true),
        ("v0.13.0-rc.1", false, false),
    ]);
    let k = key();
    server.publish(&Rel::new("0.10.0"), &k);
    server.publish(&Rel::new("0.4.0"), &k);
    let install_before = snapshot(&fx.install);
    let probe = Probe::default();

    let outcome = flow::check(&fx.ctx(&server, &probe, "zip")).expect("check");
    let CheckOutcome::Available(info) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(info.version, v("0.10.0"));
    assert_eq!(info.date.to_string(), "2026-12-01");
    assert_eq!(info.notes.as_deref(), Some("要点\n- 速くした[31m"));
    assert_eq!(info.added_key, Some(k.trusted().id()));
    assert_eq!(info.db_min_write_warning, None);
    assert_eq!(
        server.requested(),
        [
            policy::RELEASES_URL.to_owned(),
            policy::download_url(v("0.10.0"), "release.json"),
            policy::download_url(v("0.10.0"), "release.json.minisig"),
        ]
    );
    assert_eq!(probe.calls.get(), 0);
    assert!(!fx.state.exists(), "check made the state folder");
    assert!(!fx.data.exists(), "check made the data root");
    assert_eq!(snapshot(&fx.install), install_before);
}

/// verifies: AC-FR-803-1 (新しい版が無いときは、そう示して 0。一覧だけを引く)
#[test]
fn check_without_a_newer_version_says_so() {
    let fx = Fx::new();
    let server = Server::default();
    server.list(&[("v0.3.0", false, false), ("v0.2.9", false, false)]);
    let probe = Probe::default();
    let outcome = flow::check(&fx.ctx(&server, &probe, "zip")).expect("check");
    assert_eq!(
        outcome,
        CheckOutcome::UpToDate {
            current: v(CURRENT)
        }
    );
    assert_eq!(server.requested(), [policy::RELEASES_URL]);
    assert!(!fx.state.exists());
    // 入れ方が zip でなくても、check は使える (AC-FR-806-1)。
    let server = Server::default();
    server.list(&[]);
    assert!(flow::check(&fx.ctx(&server, &probe, "cargo")).is_ok());
}

/// 今の版が `upgrade_from` を下回るときは、断らずに版と先に入れる版を示す (0)。`db_min_write` が今の
/// 版の形式より新しければ警告する。
#[test]
fn check_tells_which_version_to_apply_first() {
    let fx = Fx::new();
    let server = Server::default();
    server.list(&[("v0.5.0", false, false)]);
    let mut rel = Rel::new("0.5.0");
    rel.json["upgrade_from"] = "0.4.0".into();
    rel.json["db_min_write"] = "0.5".into();
    server.publish(&rel, &key());
    let probe = Probe::default();
    let outcome = flow::check(&fx.ctx(&server, &probe, "zip")).expect("check");
    let CheckOutcome::UpgradeFirst {
        release,
        upgrade_from,
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(upgrade_from, v("0.4.0"));
    assert_eq!(release.version, v("0.5.0"));
    assert_eq!(release.db_min_write_warning.as_deref(), Some("0.5"));
}

/// `check` が検証で断ったときも、改ざんを疑うものは 5、使えないものは 1。
#[test]
fn check_refusals_keep_their_exit_codes() {
    let fx = Fx::new();
    let probe = Probe::default();
    let k = key();
    let rel = Rel::new("0.4.0");

    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    let (mut json, sig) = rel.signed(&k);
    json.push(b' ');
    server.publish_raw(&rel, json, sig);
    let e = flow::check(&fx.ctx(&server, &probe, "zip")).expect_err("tampered");
    assert_eq!(e.exit_code(), 5, "{e}");

    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    let mut unknown = Rel::new("0.4.0");
    unknown.json["schema"] = 2.into();
    server.publish(&unknown, &k);
    let e = flow::check(&fx.ctx(&server, &probe, "zip")).expect_err("schema");
    assert!(
        matches!(e, FlowError::Verify(UpdateError::UnknownSchema)),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);

    // 署名が無い (取り下げた版) は 1。
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    let (json, sig) = rel.signed(&k);
    server.put(policy::download_url(v("0.4.0"), "release.json"), json);
    let e = flow::check(&fx.ctx(&server, &probe, "zip")).expect_err("no signature");
    assert!(
        matches!(
            e,
            FlowError::ReleaseFileMissing {
                what: "release.json.minisig",
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);

    // release.json が無い (404。無い版・取り下げた版) も 1。指定した版の apply も同じ。
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    server.put(
        policy::download_url(v("0.4.0"), "release.json.minisig"),
        sig,
    );
    let e = flow::check(&fx.ctx(&server, &probe, "zip")).expect_err("no release.json");
    assert!(
        matches!(
            e,
            FlowError::ReleaseFileMissing {
                what: "release.json",
                ..
            }
        ),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);
    let e = flow::apply(
        &fx.ctx(&server, &probe, "zip"),
        Some(v("0.9.0")),
        &mut |_| {},
    )
    .expect_err("a version that does not exist");
    assert!(
        matches!(e, FlowError::ReleaseFileMissing { version, .. } if version == v("0.9.0")),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);
}

/// verifies: AC-FR-805-1 (`apply` の後に `rollback` すると、`evorag(.exe)` が置き換えの前の実行ファイル
/// に、`evorag.prev` が新しい版に戻り (SHA-256)、データ根の中身が変わらない。戻した版が書けるときは、
/// そう示して 0)
#[test]
fn apply_then_rollback_restores_the_files_and_keeps_the_data() {
    let fx = Fx::new();
    fx.make_data_root("0.1", "0.1");
    let data_before = snapshot(&fx.data);
    put(&fx.install.join("evorag.prev"), &exe("0.2.0"));
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    server.publish(&Rel::new("0.4.0"), &key());
    let probe = Probe::default();
    let ctx = fx.ctx(&server, &probe, "zip");
    let log = RefCell::new(Vec::new());

    let outcome = flow::apply(&ctx, None, &mut collect(&log)).expect("apply");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied {
            from: v(CURRENT),
            to: v("0.4.0"),
            left: Vec::new()
        }
    );
    assert_eq!(fx.digest(EXE_NAME), Some(Sha256Digest::of(&exe("0.4.0"))));
    assert_eq!(
        fx.digest("evorag.prev"),
        Some(Sha256Digest::of(&exe(CURRENT)))
    );
    let shown = log.borrow().join("\n");
    assert!(shown.contains("Release("), "{shown}");
    assert!(shown.contains("Downloading"), "{shown}");
    assert_eq!(cfg!(windows), shown.contains("GapRecovery"), "{shown}");
    // 置き換えの後の確かめで、新しい evorag を動かした。
    assert!(probe.calls.get() >= 1);

    // rollback は通信しない。
    let server_before = server.requested().len();
    let back = flow::rollback(&ctx, &mut collect(&log)).expect("rollback");
    assert_eq!(server.requested().len(), server_before);
    assert_eq!(back.to, v(CURRENT));
    assert_eq!(back.guidance.verdict, DataVerdict::Usable);
    assert!(back.guidance.pre_op_backups.is_empty());
    assert_eq!(fx.digest(EXE_NAME), Some(Sha256Digest::of(&exe(CURRENT))));
    assert_eq!(
        fx.digest("evorag.prev"),
        Some(Sha256Digest::of(&exe("0.4.0")))
    );
    for leftover in [
        "evorag.new",
        "evorag.tmp",
        "evorag.prev.old",
        "evorag.rejected",
        "evorag.probe",
    ] {
        assert!(!fx.install.join(leftover).exists(), "{leftover}");
    }
    assert_eq!(snapshot(&fx.data), data_before, "the data root changed");
}

/// verifies: AC-FR-805-1 (戻した版がデータ根に書けない・開けないときは、移行の前の退避のパス (無ければ
/// 無いこと) を示して 0)
#[test]
fn rollback_tells_how_to_restore_when_the_old_version_cannot_write() {
    for (min_read, min_write, expected) in [
        (
            "0.1",
            "0.3",
            DataVerdict::ReadOnly {
                format: "0.2".into(),
                min_write: "0.3".into(),
            },
        ),
        (
            "0.3",
            "0.3",
            DataVerdict::Unopenable {
                format: "0.2".into(),
                min_read: "0.3".into(),
            },
        ),
    ] {
        // 今の版 (戻す先) が書く形式は 0.2 で、データ根は新しい版で移行した後 (min_write が 0.3)。
        let fx = Fx::with_db_format("0.2");
        fx.make_data_root(min_read, min_write);
        let server = Server::default();
        server.list(&[("v0.4.0", false, false)]);
        server.publish(&Rel::new("0.4.0"), &key());
        let probe = Probe::default();
        let ctx = fx.ctx(&server, &probe, "zip");
        flow::apply(&ctx, None, &mut |_| {}).expect("apply");

        // 移行の前の退避が無いときは、無いことを示す (空の一覧)。
        let back = flow::rollback(&ctx, &mut |_| {}).expect("rollback");
        assert_eq!(back.guidance.verdict, expected);
        assert!(back.guidance.needs_restore());
        assert!(back.guidance.pre_op_backups.is_empty());

        // 退避があれば、新しいものから示す。
        flow::apply(&ctx, None, &mut |_| {}).expect("apply again");
        let pre_op = fx.data.join("backups").join("pre-op");
        for stamp in ["20261201-000000", "20261202-000000"] {
            let dir = pre_op.join(stamp);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::copy(fx.data.join("evorag.redb"), dir.join("evorag.redb")).expect("copy");
        }
        std::fs::write(fx.config.with_file_name("config.toml.bak-v0"), "x").expect("bak");
        let back = flow::rollback(&ctx, &mut |_| {}).expect("rollback again");
        assert_eq!(back.guidance.verdict, expected);
        assert_eq!(back.guidance.pre_op_backups.len(), 2);
        assert_eq!(
            back.guidance.config_backup,
            Some(fx.config.with_file_name("config.toml.bak-v0"))
        );
    }
}

/// 署名の合わない (中身を書き換えた・鍵の違う・prehash でない) `release.json` を、5 で断る。
fn refused_without_change(fx: &Fx, server: &Server, expected_code: u8) -> FlowError {
    let install_before = snapshot(&fx.install);
    let probe = Probe::default();
    let e = flow::apply(&fx.ctx(server, &probe, "zip"), None, &mut |_| {}).expect_err("refused");
    assert_eq!(e.exit_code(), expected_code, "{e}");
    assert_eq!(snapshot(&fx.install), install_before, "{e}");
    assert_eq!(probe.calls.get(), 0);
    e
}

/// verifies: AC-FR-804-1 (署名の合わない配布物を適用しない)、AC-NFR-505-2 (改ざんを疑うものは 5 で、
/// インストール先のファイルを変えない)
#[test]
fn a_tampered_or_wrongly_signed_release_is_refused_with_5() {
    let rel = Rel::new("0.4.0");
    let k = key();
    let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = {
        let (json, sig) = rel.signed(&k);
        let mut tampered = json.clone();
        tampered.extend_from_slice(b"\n");
        let other = TestKey::new(9, [9; 8]);
        let (_, other_sig) = rel.signed(&other);
        let legacy = k.sign_legacy(&json, &comment_for("0.4.0", &json));
        let wrong_comment = k.sign(&json, &comment_for("0.4.1", &json));
        vec![
            ("tampered", tampered, sig),
            ("unknown key", json.clone(), other_sig),
            ("legacy", json.clone(), legacy),
            ("comment", json, wrong_comment),
        ]
    };
    for (what, json, sig) in cases {
        let fx = Fx::new();
        let server = Server::default();
        server.list(&[("v0.4.0", false, false)]);
        server.publish_raw(&rel, json, sig);
        let e = refused_without_change(&fx, &server, 5);
        assert!(matches!(e, FlowError::Verify(_)), "{what}: {e}");
        // 配布物は取得しない。
        assert!(
            !server
                .requested()
                .contains(&policy::download_url(v("0.4.0"), &rel.name())),
            "{what}"
        );
    }
    // 古い版の正しく署名した `release.json` を、新しいタグに置いたもの (版とタグの不一致)。
    let fx = Fx::new();
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    let old = Rel::new("0.2.0");
    let (json, sig) = old.signed(&k);
    server.put(policy::download_url(v("0.4.0"), "release.json"), json);
    server.put(
        policy::download_url(v("0.4.0"), "release.json.minisig"),
        sig,
    );
    let e = refused_without_change(&fx, &server, 5);
    assert!(
        matches!(e, FlowError::Verify(UpdateError::VersionMismatch { .. })),
        "{e}"
    );
}

/// verifies: AC-NFR-505-2 (署名は通るが使えないもの: `schema` を知らない、今の版が `upgrade_from` を
/// 下回る、今の対象の配布物を選べない、は 1 で、インストール先のファイルを変えない)
#[test]
fn unusable_releases_are_refused_with_1() {
    let k = key();
    let mut unknown_schema = Rel::new("0.4.0");
    unknown_schema.json["schema"] = 2.into();
    let mut upgrade_first = Rel::new("0.4.0");
    upgrade_first.json["upgrade_from"] = "0.3.5".into();
    let mut other_target = Rel::new("0.4.0");
    other_target.json["artifacts"][0]["target"] = "aarch64-unknown-linux-musl".into();
    for (rel, check) in [
        (
            unknown_schema,
            (|e: &UpdateError| matches!(e, UpdateError::UnknownSchema)) as fn(&UpdateError) -> bool,
        ),
        (
            upgrade_first,
            |e| matches!(e, UpdateError::BelowUpgradeFrom { upgrade_from, .. } if upgrade_from.to_string() == "0.3.5"),
        ),
        (other_target, |e| {
            matches!(e, UpdateError::NoArtifact { .. })
        }),
    ] {
        let fx = Fx::new();
        let server = Server::default();
        server.list(&[("v0.4.0", false, false)]);
        server.publish(&rel, &k);
        let e = refused_without_change(&fx, &server, 1);
        assert!(matches!(&e, FlowError::Verify(u) if check(u)), "{e}");
    }
}

/// verifies: AC-NFR-505-1 (古い版の配布物を `apply` で入れられない: 指定した版が今と同じか古ければ通信の
/// 前に断り、版を省けば今が最新と示す)
#[test]
fn the_same_or_an_older_version_cannot_be_applied() {
    for requested in ["0.3.0", "0.2.0", "0.1.9"] {
        let fx = Fx::new();
        let server = Server::default();
        server.list(&[("v0.4.0", false, false)]);
        server.publish(&Rel::new(requested), &key());
        let probe = Probe::default();
        let install_before = snapshot(&fx.install);
        let e = flow::apply(
            &fx.ctx(&server, &probe, "zip"),
            Some(v(requested)),
            &mut |_| {},
        )
        .expect_err("older");
        assert!(matches!(e, FlowError::NotNewerRequested { .. }), "{e}");
        assert_eq!(e.exit_code(), 1);
        assert_eq!(
            server.preflights.get(),
            0,
            "refused before the network checks"
        );
        assert!(server.requested().is_empty());
        assert_eq!(snapshot(&fx.install), install_before);
    }
    // 版を省いて、一覧に今より新しい版が無ければ、新しい版は無いと示して 0。
    let fx = Fx::new();
    let server = Server::default();
    server.list(&[("v0.3.0", false, false), ("v0.2.0", false, false)]);
    let probe = Probe::default();
    let outcome =
        flow::apply(&fx.ctx(&server, &probe, "zip"), None, &mut |_| {}).expect("up to date");
    assert_eq!(
        outcome,
        ApplyOutcome::UpToDate {
            current: v(CURRENT)
        }
    );
    assert_eq!(fx.digest(EXE_NAME), Some(Sha256Digest::of(&exe(CURRENT))));
}

/// verifies: AC-NFR-505-2 (大きさ・`sha256` の合わない配布物は 5 で、インストール先を変えない。
/// `size` より長く届いたものも同じ)
#[test]
fn an_archive_that_does_not_match_is_refused_with_5() {
    let k = key();
    let rel = Rel::new("0.4.0");
    let url = policy::download_url(v("0.4.0"), &rel.name());
    let mut flipped = rel.archive.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let short = rel.archive[..rel.archive.len() - 1].to_vec();
    let mut long = rel.archive.clone();
    long.push(0);
    for (what, bytes) in [("sha256", flipped), ("short", short), ("long", long)] {
        let fx = Fx::new();
        let server = Server::default();
        server.list(&[("v0.4.0", false, false)]);
        server.publish(&rel, &k);
        server.put(url.clone(), bytes);
        let e = refused_without_change(&fx, &server, 5);
        let ok = match what {
            // `size` より長く届いたものは、読むのを止めて大きさの不一致として 5 で断る。
            "long" => {
                matches!(e, FlowError::ArchiveLongerThanSigned { size } if size == rel.archive.len() as u64)
            }
            "short" => matches!(
                e,
                FlowError::Verify(UpdateError::ArchiveSizeMismatch { .. })
            ),
            _ => matches!(e, FlowError::Verify(UpdateError::ArchiveDigestMismatch)),
        };
        assert!(ok, "{what}: {e}");
        assert!(server.requested().contains(&url), "{what}");
    }
}

/// verifies: AC-FR-805-2 (CLI の流れで置き換えの途中に止まっても、次の TUI の起動の片付けの後に、
/// `evorag(.exe)` が前か後の版で、新しい版なら前の版が `evorag.prev` にある)
#[test]
fn an_interrupted_apply_is_finished_by_the_startup_cleanup() {
    let fx = Fx::new();
    put(&fx.install.join("evorag.prev"), &exe("0.2.0"));
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    server.publish(&Rel::new("0.4.0"), &key());
    // 置き換えの後の確かめ (新しい evorag の --version) の最中に止まる。
    let crashing = Probe {
        crash_on: Some("0.4.0"),
        ..Probe::default()
    };
    let ctx = fx.ctx(&server, &crashing, "zip");
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        flow::apply(&ctx, None, &mut |_| {})
    }));
    assert!(stopped.is_err(), "the apply must stop in the check");
    assert!(
        fx.state.join("update").join("state.json").exists(),
        "a plan is left"
    );

    // 次の TUI の起動の片付けが、計画に従って続きを済ませる。
    let honest = Probe::default();
    let mut env = Env::new(&honest);
    let report = swap::startup(fx.exe_path(), &fx.state, &mut env).expect("startup cleanup");
    assert!(matches!(report, Startup::Resumed(_)), "{report:?}");
    let exe_now = fx.digest(EXE_NAME);
    let prev_now = fx.digest("evorag.prev");
    let (new, cur, old) = (
        Sha256Digest::of(&exe("0.4.0")),
        Sha256Digest::of(&exe(CURRENT)),
        Sha256Digest::of(&exe("0.2.0")),
    );
    if exe_now == Some(new) {
        assert_eq!(prev_now, Some(cur));
    } else {
        assert_eq!(exe_now, Some(cur));
        assert_eq!(prev_now, Some(old));
    }
    // 計画は済み、残ったファイルも無い。
    let inst =
        swap::Installation::open(fx.exe_path(), &fx.state, swap::Purpose::Update).expect("open");
    assert!(inst.pending().expect("state").is_none());
    for leftover in [
        "evorag.new",
        "evorag.tmp",
        "evorag.prev.old",
        "evorag.rejected",
    ] {
        assert!(!fx.install.join(leftover).exists(), "{leftover}");
    }
}

/// verifies: AC-FR-806-1 (入れ方が zip でなければ、apply と rollback は通信せず、インストール先を変えずに
/// 断る)
#[test]
fn a_non_zip_install_refuses_apply_and_rollback_without_network() {
    let fx = Fx::new();
    put(&fx.install.join("evorag.prev"), &exe("0.2.0"));
    let server = Server::default();
    server.list(&[("v0.4.0", false, false)]);
    server.publish(&Rel::new("0.4.0"), &key());
    let probe = Probe::default();
    let install_before = snapshot(&fx.install);
    for channel in ["cargo", "winget", ""] {
        let ctx = fx.ctx(&server, &probe, channel);
        let e = flow::apply(&ctx, None, &mut |_| {}).expect_err("apply");
        assert!(matches!(e, FlowError::NotZipChannel { .. }), "{e}");
        assert_eq!(e.exit_code(), 1);
        let e = flow::rollback(&ctx, &mut |_| {}).expect_err("rollback");
        assert!(matches!(e, FlowError::NotZipChannel { .. }), "{e}");
    }
    assert_eq!(server.preflights.get(), 0);
    assert!(server.requested().is_empty());
    assert_eq!(probe.calls.get(), 0);
    assert!(!fx.state.exists(), "no update lock was taken");
    assert_eq!(snapshot(&fx.install), install_before);
}

/// CPU の機能が足りなければ、通信せずに 1 で断る (12 §7.5)。
#[test]
fn missing_cpu_features_refuse_before_network() {
    let fx = Fx::new();
    let server = Server {
        cpu_missing: Some("avx2"),
        ..Server::default()
    };
    let probe = Probe::default();
    let e = flow::apply(&fx.ctx(&server, &probe, "zip"), None, &mut |_| {}).expect_err("cpu");
    assert!(matches!(e, FlowError::Net(ref n) if n.is_platform()), "{e}");
    assert_eq!(e.exit_code(), 1);
    assert!(server.requested().is_empty());
    let e = flow::check(&fx.ctx(&server, &probe, "zip")).expect_err("cpu");
    assert!(matches!(e, FlowError::Net(_)), "{e}");
}

/// evorag が動いている (データ根のロックが取れない) ときは、通信の前に 2 で断る。
#[test]
fn a_running_evorag_blocks_apply_and_rollback() {
    let fx = Fx::new();
    fx.make_data_root("0.1", "0.1");
    put(&fx.install.join("evorag.prev"), &exe("0.2.0"));
    let held = evorag_store::lock::DataRootLock::acquire(&fx.data).expect("lock");
    let server = Server::default();
    let probe = Probe::default();
    let ctx = fx.ctx(&server, &probe, "zip");
    let e = flow::apply(&ctx, None, &mut |_| {}).expect_err("apply");
    assert!(matches!(e, FlowError::DataRootInUse { .. }), "{e}");
    assert_eq!(e.exit_code(), 2);
    let e = flow::rollback(&ctx, &mut |_| {}).expect_err("rollback");
    assert_eq!(e.exit_code(), 2, "{e}");
    assert!(server.requested().is_empty());
    assert_eq!(fx.digest(EXE_NAME), Some(Sha256Digest::of(&exe(CURRENT))));
    drop(held);
}

/// ほかの `update` が動いている (`update/lock`) ときは 1 で断る。
#[test]
fn another_update_blocks_apply() {
    let fx = Fx::new();
    let held =
        swap::Installation::open(fx.exe_path(), &fx.state, swap::Purpose::Update).expect("first");
    let server = Server::default();
    let probe = Probe::default();
    let e = flow::apply(&fx.ctx(&server, &probe, "zip"), None, &mut |_| {}).expect_err("busy");
    assert!(matches!(e, FlowError::Swap(swap::SwapError::Busy)), "{e}");
    assert_eq!(e.exit_code(), 1);
    drop(held);
}
