//! `evorag import-v04` の確認・終了コード・断るときの文言と、TUI の起動の前の止まった入れ替えの続き (03 §12.4、
//! 07 §5)。実行ファイルを起動して確かめる。見本 `tests/fixtures/db/0.4.0/root` を一時のフォルダの `data` に写して使う。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use evorag_core::import_v04::{ImportOptions, Imported, run};
use evorag_store::import_v04::import_dir;
use evorag_store::location::Verdict;
use evorag_store::lock::DataRootLock;
use evorag_store::meta;
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

fn fixture_root() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/db/0.4.0/root")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for e in std::fs::read_dir(from).expect("read the fixture") {
        let e = e.expect("an entry");
        let target = to.join(e.file_name());
        if e.file_type().expect("a file type").is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy a file");
        }
    }
}

/// 見本を `<一時>/data` に写す。
fn fixture(label: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new(label);
    let data = tmp.path().join("data");
    copy_dir(&fixture_root(), &data);
    (tmp, data)
}

/// 何も受け付けていないループバックのポート (TUI の起動が llama-server に繋がらずに止まるように)。
fn closed_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", l.local_addr().expect("addr"));
    drop(l);
    url
}

fn evorag(dir: &Path, config: &str, args: &[&str], stdin: &str) -> Output {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, config).expect("write the config");
    let exe = std::env::var_os("NEXTEST_BIN_EXE_evorag")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_evorag").into());
    let mut child = Command::new(exe)
        .arg("--config")
        .arg(&cfg)
        .arg("--data-root")
        .arg(dir.join("data"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start evorag");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `<一時>` の中の、改名した古い根 (`data-v0.4-*`)。
fn renamed_roots(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .expect("read")
        .map(|e| e.expect("an entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("data-v0.4-"))
        })
        .collect()
}

fn format_of(root: &Path) -> Option<String> {
    evorag_store::root_rename::read_meta(root)
        .ok()?
        .get(meta::FORMAT_VERSION)
        .cloned()
}

/// 確かめてから取り込み、入れ替える。y 以外 (入力の終わりを含む) は何も変えずに 1。取り込んだ後は、取り込みは
/// 要らないと示して 1。
///
/// verifies: AC-FR-219-3 (CLI の層: 入れ替えて 0)
#[test]
fn import_v04_asks_first_then_imports_and_swaps() {
    let (tmp, data) = fixture("cli-import");
    for answer in ["n\n", ""] {
        let out = evorag(tmp.path(), "", &["import-v04"], answer);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        let t = text(&out);
        assert!(t.contains("取り込みますか"), "{t}");
        assert!(t.contains("何も変えていません"), "{t}");
        assert!(!import_dir(&data).expect("named").exists());
        assert_eq!(format_of(&data).as_deref(), Some("0.3"));
    }

    let out = evorag(tmp.path(), "", &["import-v04"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("取り込みました"), "{t}");
    assert_eq!(format_of(&data).as_deref(), Some("0.5"));
    let renamed = renamed_roots(tmp.path());
    assert_eq!(renamed.len(), 1, "{renamed:?}");
    assert_eq!(format_of(&renamed[0]).as_deref(), Some("0.3"));
    assert!(t.contains(&renamed[0].display().to_string()), "{t}");
    assert!(!import_dir(&data).expect("named").exists());

    let out = evorag(tmp.path(), "", &["import-v04"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("取り込みは要りません"), "{t}");

    // --from で、改名した古い根 (v0.4 の根のまま) を指せば、それを取り込むかを尋ねる (ここでは断る)。
    let from = renamed[0].display().to_string();
    let out = evorag(tmp.path(), "", &["import-v04", "--from", &from], "n\n");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}

/// 取り込めない根は、理由と次にすることを示して、尋ねずに 1 で終わる。
///
/// verifies: AC-FR-219-2 (CLI の層)
#[test]
fn import_v04_refuses_with_the_reason_and_the_next_step() {
    let (tmp, data) = fixture("cli-import-refused");
    let missing = tmp.path().join("nothing-here").display().to_string();
    let out = evorag(tmp.path(), "", &["import-v04", "--from", &missing], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(
        t.contains("データ根ではありません") && t.contains("--from"),
        "{t}"
    );
    assert!(!t.contains("取り込みますか"), "{t}");

    raw_set_meta(&data, meta::MAX_WRITER, "0.3.5");
    let out = evorag(tmp.path(), "", &["import-v04"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(
        t.contains("v0.4 の evorag で一度起動して終了してから"),
        "{t}"
    );
    assert!(!t.contains("取り込みますか"), "{t}");
    assert!(!import_dir(&data).expect("named").exists());
}

/// 古い根をほかの evorag が使っていれば、2 で終わる。
#[test]
fn import_v04_exits_with_2_while_the_root_is_in_use() {
    let (tmp, data) = fixture("cli-import-locked");
    let held = DataRootLock::acquire(&data).expect("another evorag");
    let out = evorag(tmp.path(), "", &["import-v04"], "y\n");
    let t = text(&out);
    assert_eq!(out.status.code(), Some(2), "{t}");
    assert!(t.contains("ほかの evorag"), "{t}");
    drop(held);
    assert!(!import_dir(&data).expect("named").exists());
}

/// 2 つの改名の間で止まった取り込みは、TUI を開く前に入れ替えを終える (尋ねない)。
///
/// verifies: AC-FR-219-3 (実行ファイルの層: 次の起動が続ける)
#[test]
fn the_next_start_finishes_an_interrupted_swap() {
    let (tmp, data) = fixture("cli-import-resume");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_795_000_000_000)));
    let Imported {
        renamed_old_root,
        old_root_lock,
        new_root_lock,
        ..
    } = run(&ImportOptions {
        old_root: data.clone(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
        before_verify: None,
        swap_hooks: Default::default(),
    })
    .expect("import");
    drop((old_root_lock, new_root_lock));
    std::fs::rename(&data, &renamed_old_root).expect("the first rename");

    let config = format!("[llm.chat]\nurl = \"{}\"\n", closed_port());
    let out = evorag(tmp.path(), &config, &[], "");
    let t = text(&out);
    assert!(t.contains("入れ替えを終えました"), "{t}");
    assert_eq!(format_of(&data).as_deref(), Some("0.5"), "{t}");
    assert_eq!(format_of(&renamed_old_root).as_deref(), Some("0.3"));
    assert!(!import_dir(&data).expect("named").exists());
}

/// 2 つの改名の間で止まっている間は、`rebuild` もデータ根を作らずに断る (03 §12.4 の (h))。
#[test]
fn rebuild_never_creates_the_original_while_the_swap_is_unfinished() {
    let (tmp, data) = fixture("cli-import-no-create");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_795_000_000_000)));
    let Imported {
        renamed_old_root,
        old_root_lock,
        new_root_lock,
        ..
    } = run(&ImportOptions {
        old_root: data.clone(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
        before_verify: None,
        swap_hooks: Default::default(),
    })
    .expect("import");
    drop((old_root_lock, new_root_lock));
    std::fs::rename(&data, &renamed_old_root).expect("the first rename");
    let out = evorag(tmp.path(), "", &["rebuild", "--index"], "");
    let t = text(&out);
    assert_ne!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("import-v04"), "{t}");
    assert!(!data.exists(), "no empty root is created: {t}");
}

/// `/health`・`/props`・`/v1/models` に答える、ループバックの偽の llama-server。モデルは見本の根の記録 (`chat.gguf`)
/// と違う (TUI を開く前に、応答のモデルの違いで 6 で終わるように)。
fn fake_llama_server() -> String {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for mut sock in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.starts_with("GET /props") {
                r#"{"build_info":"b1-test","model_path":"/m/other.gguf","total_slots":1,"default_generation_settings":{"n_ctx":4096}}"#
            } else if req.starts_with("GET /v1/models") {
                r#"{"data":[{"id":"other"}]}"#
            } else {
                r#"{"status":"ok"}"#
            };
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

/// データ根のファイルの (大きさ・中身)。ロックのファイルは除く (03 §12.4 の「読み方」)。
fn files(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).expect("read a directory") {
            let path = e.expect("an entry").path();
            if path.is_dir() {
                walk(base, &path, out);
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .expect("under the base")
                .to_string_lossy()
                .replace('\\', "/");
            if ["lock", "lock.pid", "lock.pid.tmp"].contains(&rel.as_str()) {
                continue;
            }
            out.insert(rel, std::fs::read(&path).expect("read"));
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// TUI の起動で v0.4 の根を開こうとすると、在り場所で移行せずに取り込むかを尋ねる。y 以外 (入力の終わりを含む) は
/// 何も変えずに 2 で終わる。
///
/// verifies: 03 §12.4 の「きっかけ」(y 以外なら何も変えずに 2 で終わる)
#[test]
fn the_tui_asks_to_import_a_v04_root_and_exits_with_2_when_declined() {
    let (tmp, data) = fixture("cli-startup-declined");
    let url = fake_llama_server();
    let config = format!("[llm.chat]\nurl = \"{url}\"\n");
    let before = files(&data);
    for answer in ["n\n", ""] {
        let out = evorag(tmp.path(), &config, &[], answer);
        let t = text(&out);
        assert_eq!(out.status.code(), Some(2), "{t}");
        assert!(t.contains("v0.4 の形式 (形式の版 0.3)"), "{t}");
        assert!(t.contains("取り込みますか"), "{t}");
        assert!(t.contains("取り込まなかったので"), "{t}");
        assert_eq!(before, files(&data), "the v0.4 root is unchanged");
        assert!(!import_dir(&data).expect("named").exists());
        assert!(renamed_roots(tmp.path()).is_empty());
    }
}

/// y なら `import-v04` と同じ手順で取り込んで入れ替え、新しい根で続ける (ここでは、新しい根の応答のモデルの記録と
/// 繋がっているモデルが違うので、TUI を開く前に 6 で終わる。新しい根を開いたことの確かめ)。
///
/// verifies: 03 §12.4 の「きっかけ」(y なら同じ手順を行い、終えたら新しい根で続ける)
#[test]
fn the_tui_imports_a_v04_root_on_y_and_opens_the_new_root() {
    let (tmp, data) = fixture("cli-startup-import");
    let url = fake_llama_server();
    let config = format!("[llm.chat]\nurl = \"{url}\"\n");
    let out = evorag(tmp.path(), &config, &[], "y\n");
    let t = text(&out);
    assert!(t.contains("取り込みました"), "{t}");
    assert_eq!(out.status.code(), Some(6), "the new root was opened: {t}");
    assert!(
        t.contains("chat.gguf"),
        "the recorded model of the new root: {t}"
    );
    assert_eq!(format_of(&data).as_deref(), Some("0.5"));
    let renamed = renamed_roots(tmp.path());
    assert_eq!(renamed.len(), 1, "{renamed:?}");
    assert_eq!(format_of(&renamed[0]).as_deref(), Some("0.3"));
    assert!(!import_dir(&data).expect("named").exists());
}

/// データ根に書くほかのコマンドは、v0.4 の根に何も書かずに `import-v04` を案内して 2 で終わる。読むだけの `doctor`・
/// `backups list`・`export` は、v0.4 の根を読むだけで扱う。
///
/// verifies: 03 §8 (v0.4 のデータ根を v0.5 で開いた)
#[test]
fn commands_that_write_refuse_a_v04_root_and_point_to_import() {
    let (tmp, data) = fixture("cli-v04-refused");
    let config = format!("[llm.chat]\nurl = \"{}\"\n", closed_port());
    let before = files(&data);
    let backup = data.join("evorag.redb").display().to_string();
    let cases: [&[&str]; 5] = [
        &["repair"],
        &["rebuild"],
        &["rebuild", "--index"],
        &["restore", &backup],
        &["backups", "purge", "--yes"],
    ];
    for args in cases {
        let out = evorag(tmp.path(), &config, args, "");
        let t = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {t}");
        assert!(t.contains("v0.4 の形式 (形式の版 0.3)"), "{args:?}: {t}");
        assert!(t.contains("evorag import-v04"), "{args:?}: {t}");
        assert_eq!(before, files(&data), "{args:?}: the v0.4 root is unchanged");
    }
    for args in [&["doctor"][..], &["backups", "list"][..]] {
        let out = evorag(tmp.path(), &config, args, "");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", text(&out));
        assert_eq!(before, files(&data), "{args:?}: the v0.4 root is unchanged");
    }
    let out_dir = tmp.path().join("out");
    let out = evorag(
        tmp.path(),
        &config,
        &["export", out_dir.to_str().expect("utf-8")],
        "",
    );
    assert_eq!(out.status.code(), Some(0), "export: {}", text(&out));
    assert_eq!(before, files(&data), "export: the v0.4 root is unchanged");
}
