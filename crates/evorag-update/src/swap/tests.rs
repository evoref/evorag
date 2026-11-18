//! 置き換え・巻き戻し・続き・片付けの試験 (09 §3.6・§4、12 §7.2・§7.3)。
//!
//! 実行ファイルの代わりに、版を書いた小さなファイルを置き、`--version` は偽物 ([`FakeProbe`]) が
//! ファイルの中身から答える。Windows では両方の方式 (自分を改名する方式と、上から置く方式) を、
//! Linux では上から置く方式を試す。

use std::cell::Cell;
use std::path::{Path, PathBuf};

use evorag_store::testutil::TempDir;

use super::env::{Env, Event};
use super::installation::{Build, Installation, Purpose, ResumeOutcome, Strictness};
use super::names::{EXE_NAME, FileName, GapRecovery, Method, Op};
use super::probe::{ProbeError, VersionProbe};
use super::{CheckFailure, Failure, SwapError};
use crate::{Sha256Digest, Version};

const CURRENT: &str = "0.3.0";
const NEXT: &str = "0.4.0";
const OLD_PREV: &str = "0.2.0";

fn exe(version: &str) -> Vec<u8> {
    format!("evorag-fixture {version}\n").into_bytes()
}

fn sha(version: &str) -> Sha256Digest {
    Sha256Digest::of(&exe(version))
}

fn build(version: &str) -> Build {
    Build::new(version.parse().expect("version"), Some("0.3".to_owned()))
}

/// 偽の `--version`: ファイルの中身の版を `evorag <版>` として答える。
#[derive(Default)]
struct FakeProbe {
    mode: Cell<ProbeMode>,
    calls: Cell<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ProbeMode {
    #[default]
    Honest,
    /// 違う版を答える。
    Wrong,
    /// 時間切れ。
    Hang,
}

impl FakeProbe {
    fn with(mode: ProbeMode) -> Self {
        let p = Self::default();
        p.mode.set(mode);
        p
    }
}

impl VersionProbe for FakeProbe {
    fn run_version(&self, exe: &Path) -> Result<String, ProbeError> {
        self.calls.set(self.calls.get() + 1);
        assert!(exe.is_absolute(), "{}", exe.display());
        let bytes = std::fs::read(exe).map_err(ProbeError::Spawn)?;
        let text = String::from_utf8_lossy(&bytes);
        let version = text
            .strip_prefix("evorag-fixture ")
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("?");
        match self.mode.get() {
            ProbeMode::Honest => Ok(format!("evorag {version}\ncommit: x\n")),
            ProbeMode::Wrong => Ok("evorag 9.9.9\n".to_owned()),
            ProbeMode::Hang => Err(ProbeError::TimedOut(std::time::Duration::from_secs(30))),
        }
    }
}

/// インストール先にファイルを書く。Unix では権限を 0o755 にする (試験を走らせる利用者の umask が
/// 002 でも、group が書けるファイルにならないように。インストール先の確かめが断るため)。
fn put(path: PathBuf, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(&path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// インストール先と状態のフォルダ。
struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new(prev: Option<&str>) -> Self {
        let root = TempDir::new("swap");
        // インストール先は今の利用者だけが書ける (Windows: 保護した DACL。継承する一時フォルダの権限に
        // よらず、インストール先の確かめに通るように)。
        evorag_store::fsutil::create_private_dir(&root.path().join("install")).expect("mkdir");
        let fx = Self { root };
        put(fx.path(FileName::Exe), exe(CURRENT)).expect("write exe");
        if let Some(p) = prev {
            put(fx.path(FileName::Prev), exe(p)).expect("write prev");
        }
        fx
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("install")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn path(&self, name: FileName) -> PathBuf {
        self.dir().join(name.as_str())
    }

    fn digest(&self, name: FileName) -> Option<Sha256Digest> {
        std::fs::read(self.path(name))
            .ok()
            .map(|b| Sha256Digest::of(&b))
    }

    fn open(&self, method: Method, purpose: Purpose) -> Result<Installation, SwapError> {
        // Unix では、同じプロセスの別の試験が子のプロセスを作る間 (fork から exec まで)、閉じたロックの
        // ファイルを子が持ち続けることがある (flock は開いたファイルの記述に付く)。少し待って試し直す。
        let mut tries = 0;
        #[cfg_attr(not(windows), expect(unused_mut, reason = "one method on Unix"))]
        let mut inst = loop {
            match Installation::open(&self.dir().join(EXE_NAME), &self.state_dir(), purpose) {
                Err(SwapError::Busy) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                r => break r?,
            }
        };
        #[cfg(windows)]
        inst.set_method(method);
        #[cfg(not(windows))]
        assert_eq!(method, Method::HOST);
        Ok(inst)
    }

    fn state_json(&self) -> serde_json::Value {
        let path = self.state_dir().join("update").join("state.json");
        std::fs::read(path).map_or(serde_json::Value::Null, |b| {
            serde_json::from_slice(&b).expect("state json")
        })
    }

    fn has_plan(&self) -> bool {
        !self.state_json()["plan"].is_null()
    }

    /// インストール先のファイルの名前と中身。
    fn listing(&self) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(self.dir())
            .expect("read dir")
            .map(|e| {
                let e = e.expect("entry");
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap_or_default(),
                )
            })
            .collect();
        v.sort();
        v
    }
}

fn methods() -> Vec<Method> {
    #[cfg(windows)]
    {
        vec![Method::TwoStep, Method::Replace]
    }
    #[cfg(not(windows))]
    {
        vec![Method::Replace]
    }
}

/// 試す操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// 前回の `evorag.prev` がある apply。
    ApplyWithPrev,
    /// `evorag.prev` の無い apply。
    ApplyFresh,
    /// 前回の `evorag.prev` が新しい版と同じ中身 (apply → rollback → apply)。
    ApplySameAsOldPrev,
    /// 置き換えの後の確かめに通らない apply。
    ApplyRejected,
    /// rollback。
    Rollback,
}

const SCENARIOS: [Scenario; 5] = [
    Scenario::ApplyWithPrev,
    Scenario::ApplyFresh,
    Scenario::ApplySameAsOldPrev,
    Scenario::ApplyRejected,
    Scenario::Rollback,
];

impl Scenario {
    fn op(self) -> Op {
        if self == Self::Rollback {
            Op::Rollback
        } else {
            Op::Apply
        }
    }

    fn prev(self) -> Option<&'static str> {
        match self {
            Self::ApplyFresh => None,
            Self::ApplySameAsOldPrev => Some(NEXT),
            _ => Some(OLD_PREV),
        }
    }

    fn probe(self) -> ProbeMode {
        if self == Self::ApplyRejected {
            ProbeMode::Wrong
        } else {
            ProbeMode::Honest
        }
    }

    /// 置き換えた後に `evorag(.exe)` に来る版。
    fn next(self) -> &'static str {
        if self == Self::Rollback {
            OLD_PREV
        } else {
            NEXT
        }
    }
}

/// 強制終了の点の数え手。`at` 回目の操作で止める。
fn crash_hook(at: Option<usize>) -> impl FnMut(Event) -> bool {
    let mut n = 0;
    move |_| {
        n += 1;
        Some(n) == at
    }
}

fn run(
    fx: &Fixture,
    method: Method,
    s: Scenario,
    crash_at: Option<usize>,
) -> Result<(), SwapError> {
    let probe = FakeProbe::with(s.probe());
    let mut hook = crash_hook(crash_at);
    let mut env = Env::new(&probe).with_crash_hook(&mut hook);
    let mut inst = fx.open(method, Purpose::Update)?;
    match s.op() {
        Op::Apply => inst
            .apply_bytes(
                &exe(NEXT),
                sha(NEXT),
                &build(CURRENT),
                &build(NEXT),
                &mut env,
            )
            .map(drop),
        Op::Rollback => {
            let prev = inst.identify_prev(&mut env)?;
            inst.rollback(&prev, &build(CURRENT), &mut env)
        }
    }
}

/// 起動の片付けと同じ: 計画があれば続きを、無ければ名前で片付ける。
fn recover(
    fx: &Fixture,
    method: Method,
    probe: ProbeMode,
    crash_at: Option<usize>,
) -> Result<(), SwapError> {
    let probe = FakeProbe::with(probe);
    let mut hook = crash_hook(crash_at);
    let mut env = Env::new(&probe).with_crash_hook(&mut hook);
    let mut inst = fx.open(method, Purpose::Startup)?;
    if inst.resume(&mut env)?.is_none() {
        let report = inst.cleanup_by_name(Strictness::Strict)?;
        assert!(report.left.is_empty(), "{report:?}");
    }
    Ok(())
}

/// 置き換えの前か後のどちらかで、`evorag.prev` が役割どおりで、残りのファイルが無い (AC-FR-805-2)。
#[track_caller]
fn assert_consistent(fx: &Fixture, s: Scenario, context: &str) {
    let exe_now = fx.digest(FileName::Exe);
    let prev_now = fx.digest(FileName::Prev);
    let listing = || format!("{context}: {:?}", fx.listing());
    if exe_now == Some(sha(s.next())) {
        assert_eq!(prev_now, Some(sha(CURRENT)), "{}", listing());
    } else {
        assert_eq!(exe_now, Some(sha(CURRENT)), "{}", listing());
        assert_eq!(prev_now, s.prev().map(sha), "{}", listing());
    }
    for name in FileName::LEFTOVERS {
        assert!(!fx.path(name).exists(), "{name} is left: {}", listing());
    }
    assert!(!fx.has_plan(), "{}", listing());
}

#[test]
fn apply_and_rollback_place_each_role() {
    for method in methods() {
        let fx = Fixture::new(Some(OLD_PREV));
        run(&fx, method, Scenario::ApplyWithPrev, None).expect("apply");
        assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(CURRENT)));
        let done = &fx.state_json()["done"];
        assert_eq!(done["op"], "apply");
        assert_eq!(done["next"]["sha256"], sha(NEXT).to_string());
        assert_eq!(done["current"]["version"], CURRENT);
        assert!(fx.state_json()["plan"].is_null());
        assert_consistent(&fx, Scenario::ApplyWithPrev, "apply");
    }
}

/// verifies: AC-FR-805-1 (置き換えの核: apply → rollback → apply で、各ファイルの SHA-256 が役割と合い、
/// 状態のファイルの記録が合う。記録の無い `evorag.prev` にも戻せる)
#[test]
fn apply_rollback_apply_round_trips_by_sha256() {
    for method in methods() {
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let mut inst = fx.open(method, Purpose::Update).expect("open");
        inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        )
        .expect("apply");
        let calls_after_apply = probe.calls.get();
        // rollback は、最後に済んだ操作の記録から `evorag.prev` の版を知る (`--version` を動かさない)。
        let prev = inst.identify_prev(&mut env).expect("prev");
        assert_eq!(prev.source(), super::PrevSource::Record);
        assert_eq!(prev.version(), CURRENT.parse::<Version>().expect("v"));
        assert_eq!(prev.db_format(), Some("0.3"));
        assert_eq!(probe.calls.get(), calls_after_apply);
        inst.rollback(&prev, &build(NEXT), &mut env)
            .expect("rollback");
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(NEXT)));
        let done = &fx.state_json()["done"];
        assert_eq!(done["op"], "rollback");
        assert_eq!(done["current"]["sha256"], sha(NEXT).to_string());
        // もう一度 apply できる (前回の evorag.prev が新しい版と同じ中身)。
        inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        )
        .expect("apply again");
        assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(CURRENT)));
        assert_consistent(&fx, Scenario::ApplySameAsOldPrev, "apply again");
    }
}

/// verifies: AC-FR-805-1 (記録の無い `evorag.prev` は `--version` で版を知る)
#[test]
fn a_prev_without_a_record_is_identified_by_its_version_output() {
    let fx = Fixture::new(Some("0.2.4"));
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let prev = inst.identify_prev(&mut env).expect("prev");
    assert_eq!(prev.source(), super::PrevSource::Probe);
    assert_eq!(prev.version(), Version::new(0, 2, 4));
    // `db format:` の行が無ければ、過去の版の表から決める。
    assert_eq!(prev.db_format(), Some("0.2"));

    put(fx.path(FileName::Prev), b"not an evorag").expect("write");
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    assert!(matches!(
        inst.identify_prev(&mut env),
        Err(SwapError::PrevNotEvorag { .. })
    ));
    std::fs::remove_file(fx.path(FileName::Prev)).expect("rm");
    assert!(matches!(
        inst.identify_prev(&mut env),
        Err(SwapError::NoPrev)
    ));
}

/// `--version` が違う版を出す・時間切れのときは、元に戻して断る (Windows は `evorag.rejected` を経る)。
#[test]
fn a_new_exe_failing_the_version_check_is_rejected() {
    for method in methods() {
        for mode in [ProbeMode::Wrong, ProbeMode::Hang] {
            let fx = Fixture::new(Some(OLD_PREV));
            let probe = FakeProbe::with(mode);
            let mut events = Vec::new();
            let mut hook = |e| {
                events.push(e);
                false
            };
            let mut env = Env::new(&probe).with_crash_hook(&mut hook);
            let mut inst = fx.open(method, Purpose::Update).expect("open");
            let r = inst.apply_bytes(
                &exe(NEXT),
                sha(NEXT),
                &build(CURRENT),
                &build(NEXT),
                &mut env,
            );
            match (mode, r) {
                (
                    ProbeMode::Wrong,
                    Err(SwapError::Reverted(Failure::Check(CheckFailure::WrongVersion {
                        first_line,
                        ..
                    }))),
                ) => assert_eq!(first_line, "evorag 9.9.9"),
                (
                    ProbeMode::Hang,
                    Err(SwapError::Reverted(Failure::Check(CheckFailure::Probe(
                        ProbeError::TimedOut(_),
                    )))),
                ) => {}
                (_, r) => panic!("{method:?} {mode:?}: {r:?}"),
            }
            assert_eq!(SwapError::SameFile.exit_code(), 1);
            assert_consistent(&fx, Scenario::ApplyRejected, "rejected");
            assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
            #[cfg(windows)]
            if method == Method::TwoStep {
                assert!(events.contains(&Event::Renamed(FileName::Exe, FileName::Rejected)));
                assert!(events.contains(&Event::Removed(FileName::Rejected)));
            }
        }
    }
}

/// verifies: AC-FR-805-2 (置き換えと巻き戻しの各段で止めても、次の update か起動の片付けの後に、
/// 置き換えの前か後の版で起動でき、`evorag.prev` が役割どおり。続きの途中で止めたときも同じ)
#[test]
fn crashing_after_any_step_is_recovered_on_the_next_run() {
    for method in methods() {
        for s in SCENARIOS {
            let mut k = 1;
            loop {
                let fx = Fixture::new(s.prev());
                match run(&fx, method, s, Some(k)) {
                    Err(SwapError::Crashed) => {}
                    other => {
                        // 止める点がもう無い: 止めずに通したときの結果。
                        assert!(k > 3, "{method:?} {s:?}: only {k} steps");
                        if s == Scenario::ApplyRejected {
                            assert!(matches!(other, Err(SwapError::Reverted(_))), "{other:?}");
                        } else {
                            other.expect("the run without a crash");
                        }
                        assert_consistent(&fx, s, &format!("{method:?} {s:?} no crash"));
                        break;
                    }
                }
                // 続きの途中でも止める。
                let mut j = 1;
                loop {
                    let fx = Fixture::new(s.prev());
                    assert!(matches!(
                        run(&fx, method, s, Some(k)),
                        Err(SwapError::Crashed)
                    ));
                    let ctx = format!("{method:?} {s:?} crash {k} then {j}");
                    match recover(&fx, method, s.probe(), Some(j)) {
                        Err(SwapError::Crashed) => {
                            recover(&fx, method, s.probe(), None).expect(&ctx);
                            assert_consistent(&fx, s, &ctx);
                            j += 1;
                        }
                        r => {
                            r.expect(&ctx);
                            assert_consistent(&fx, s, &ctx);
                            break;
                        }
                    }
                }
                k += 1;
            }
        }
    }
}

/// verifies: AC-FR-805-2 (Windows の隙間で止めたときは、案内の手順で起動できる状態に戻せて、
/// その後の片付けが続きを終える)
#[test]
fn the_gap_is_recovered_by_the_displayed_steps() {
    let mut gaps = 0;
    for method in methods() {
        for s in SCENARIOS {
            for k in 1.. {
                let fx = Fixture::new(s.prev());
                if !matches!(run(&fx, method, s, Some(k)), Err(SwapError::Crashed)) {
                    break;
                }
                if fx.path(FileName::Exe).exists() {
                    continue;
                }
                gaps += 1;
                #[cfg(windows)]
                assert_eq!(method, Method::TwoStep, "{s:?} {k}: no gap on Unix");
                let steps = GapRecovery::for_op(s.op());
                let from = steps
                    .from_first_of
                    .iter()
                    .find(|n| fx.path(**n).exists())
                    .unwrap_or_else(|| panic!("{s:?} {k}: nothing to rename: {:?}", fx.listing()));
                std::fs::rename(fx.path(*from), fx.path(steps.to)).expect("manual rename");
                let ctx = format!("{method:?} {s:?} gap at {k}");
                recover(&fx, method, s.probe(), None).expect(&ctx);
                assert_consistent(&fx, s, &ctx);
            }
        }
    }
    assert_eq!(gaps > 0, cfg!(windows), "gaps: {gaps}");
}

/// 計画を残して止めた状態を作る (置き換えの前、段 1)。
fn crashed_after_plan(fx: &Fixture) {
    let mut k = 1;
    loop {
        let fx2 = Fixture::new(Some(OLD_PREV));
        assert!(matches!(
            run(&fx2, Method::HOST, Scenario::ApplyWithPrev, Some(k)),
            Err(SwapError::Crashed)
        ));
        if fx2.has_plan() {
            break;
        }
        k += 1;
    }
    assert!(matches!(
        run(fx, Method::HOST, Scenario::ApplyWithPrev, Some(k)),
        Err(SwapError::Crashed)
    ));
    assert!(fx.has_plan());
}

#[test]
fn a_plan_for_another_folder_is_refused_without_touching_files() {
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    let path = fx.state_dir().join("update").join("state.json");
    let mut v = fx.state_json();
    v["plan"]["dir"] = serde_json::json!("/somewhere/else");
    std::fs::write(&path, v.to_string()).expect("write");
    let before = fx.listing();
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    match inst.pending() {
        Err(SwapError::ForeignPlan { dir }) => assert_eq!(dir, Path::new("/somewhere/else")),
        r => panic!("{r:?}"),
    }
    assert!(matches!(
        inst.resume(&mut env),
        Err(SwapError::ForeignPlan { .. })
    ));
    assert!(matches!(
        inst.cleanup_by_name(Strictness::Strict),
        Err(SwapError::PlanPending)
    ));
    assert_eq!(fx.listing(), before);
    assert!(fx.has_plan());
}

#[test]
fn an_exe_matching_no_role_is_left_alone_with_the_plan() {
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    put(fx.path(FileName::Exe), b"someone else's file").expect("write");
    let before = fx.listing();
    let plan_before = fx.state_json();
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let e = inst.resume(&mut env).expect_err("refused");
    assert!(matches!(e, SwapError::UnknownRole), "{e:?}");
    assert!(e.needs_manual_recovery());
    assert_eq!(fx.listing(), before);
    assert_eq!(fx.state_json(), plan_before);
}

#[test]
fn an_unreadable_state_file_is_refused_with_manual_steps() {
    let fx = Fixture::new(Some(OLD_PREV));
    let path = fx.state_dir().join("update").join("state.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{ broken").expect("write");
    let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let e = inst.pending().expect_err("refused");
    assert!(matches!(e, SwapError::StateUnreadable { .. }), "{e:?}");
    assert!(e.needs_manual_recovery());
}

/// 役割に当たらないファイルは、続きの後も残し、報告する。段 1 の役割に当たらない evorag.new は消してよい。
#[test]
fn files_matching_no_role_are_reported_and_kept() {
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    put(fx.path(FileName::Rejected), b"user file").expect("write");
    put(fx.path(FileName::New), b"half written").expect("write");
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let report = inst.resume(&mut env).expect("resume").expect("a plan");
    assert!(matches!(
        report.outcome,
        ResumeOutcome::Reverted { check: None }
    ));
    assert_eq!(report.untouched, vec![FileName::Rejected]);
    assert_eq!(
        std::fs::read(fx.path(FileName::Rejected)).expect("kept"),
        b"user file"
    );
    assert!(!fx.path(FileName::New).exists());
    assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
    assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
}

#[test]
fn leftovers_are_cleaned_up_by_name_without_a_plan() {
    let fx = Fixture::new(None);
    put(fx.path(FileName::Tmp), exe(OLD_PREV)).expect("write");
    for name in [FileName::New, FileName::Rejected, FileName::Probe] {
        put(fx.path(name), b"x").expect("write");
    }
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let report = inst.cleanup_by_name(Strictness::Strict).expect("cleanup");
    assert_eq!(report.restored_prev, Some(FileName::Tmp));
    assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
    assert_eq!(
        report.removed,
        vec![FileName::New, FileName::Rejected, FileName::Probe]
    );
    assert_eq!(
        fx.listing().into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
        {
            let mut v = vec![EXE_NAME.to_owned(), "evorag.prev".to_owned()];
            v.sort();
            v
        }
    );
}

/// 残った evorag.tmp は、中身が evorag(.exe) か evorag.prev と同じときだけ消す。違えば残し、apply・
/// rollback の前の片付けは断り、起動の片付けは報告する。evorag.prev.old と evorag.tmp の両方があれば
/// evorag.prev.old を evorag.prev に戻す。
#[test]
fn an_unrelated_tmp_is_kept_and_blocks_updates() {
    let fx = Fixture::new(None);
    put(fx.path(FileName::PrevOld), exe(OLD_PREV)).expect("write");
    put(fx.path(FileName::Tmp), b"someone else's file").expect("write");
    let before = fx.listing();
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    assert!(matches!(
        inst.cleanup_by_name(Strictness::Strict),
        Err(SwapError::UnrelatedLeftover {
            name: FileName::Tmp
        })
    ));
    assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)), "restored");
    let report = inst.cleanup_by_name(Strictness::Lenient).expect("lenient");
    assert_eq!(report.kept, vec![FileName::Tmp]);
    assert_eq!(
        std::fs::read(fx.path(FileName::Tmp)).expect("kept"),
        before
            .iter()
            .find(|(n, _)| n == "evorag.tmp")
            .map(|(_, b)| b.clone())
            .expect("tmp")
    );
    // apply・rollback は自分で片付けを行い、断る (インストール先は変わらない)。
    let after_cleanup = fx.listing();
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    assert!(matches!(
        inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env
        ),
        Err(SwapError::UnrelatedLeftover { .. })
    ));
    assert_eq!(fx.listing(), after_cleanup);
    // evorag.prev と同じ中身なら消す。
    put(fx.path(FileName::Tmp), exe(OLD_PREV)).expect("write");
    let report = inst.cleanup_by_name(Strictness::Strict).expect("strict");
    assert_eq!(report.removed, vec![FileName::Tmp]);
}

/// apply・rollback は、呼ぶ側が始める前の確かめを飛ばしても、自分で片付けとインストール先の確かめを行う。
#[test]
fn apply_runs_the_prechecks_itself() {
    let fx = Fixture::new(Some(OLD_PREV));
    put(fx.path(FileName::Probe), b"x").expect("write");
    put(fx.path(FileName::Rejected), b"x").expect("write");
    run(&fx, Method::HOST, Scenario::ApplyWithPrev, None).expect("apply");
    assert_consistent(&fx, Scenario::ApplyWithPrev, "prechecked");
}

/// 計画の続きで、置く先に役割に当たらないファイルがあるか、`evorag(.exe)` に置けるものが無いときは、
/// 何も動かさず、計画を残し、手で戻す手順を示す。
#[test]
fn a_foreign_file_at_a_target_moves_nothing() {
    // 元に戻す向き: 前回の evorag.prev は evorag.prev.old にあり、evorag.prev に利用者のファイルがある。
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    std::fs::rename(fx.path(FileName::Prev), fx.path(FileName::PrevOld)).expect("mv");
    put(fx.path(FileName::Prev), b"user file").expect("write");
    assert_refused_untouched(&fx, |e| {
        matches!(
            e,
            SwapError::ForeignFile {
                name: FileName::Prev
            }
        )
    });
    // 進める向き: evorag.exe が無く、evorag.new を置けるが、evorag.prev に利用者のファイルがある
    // (先に evorag.exe を置いてしまわない)。
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    std::fs::rename(fx.path(FileName::Exe), fx.path(FileName::Tmp)).expect("mv");
    put(fx.path(FileName::Prev), b"user file").expect("write");
    assert_refused_untouched(&fx, |e| {
        matches!(
            e,
            SwapError::ForeignFile {
                name: FileName::Prev
            }
        )
    });
    // evorag.exe に置けるものが無い。
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    std::fs::remove_file(fx.path(FileName::Exe)).expect("rm");
    std::fs::remove_file(fx.path(FileName::New)).expect("rm");
    assert_refused_untouched(&fx, |e| matches!(e, SwapError::NothingToRestore));
}

#[track_caller]
fn assert_refused_untouched(fx: &Fixture, expected: impl Fn(&SwapError) -> bool) {
    let before = fx.listing();
    let plan_before = fx.state_json();
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Startup).expect("open");
    let e = inst.resume(&mut env).expect_err("refused");
    assert!(expected(&e), "{e:?}");
    assert!(e.needs_manual_recovery(), "{e:?}");
    assert_eq!(fx.listing(), before);
    assert_eq!(fx.state_json(), plan_before);
    assert_eq!(probe.calls.get(), 0);
}

#[test]
fn the_update_lock_is_exclusive() {
    let fx = Fixture::new(None);
    let _held = fx.open(Method::HOST, Purpose::Update).expect("open");
    assert!(matches!(
        fx.open(Method::HOST, Purpose::Update),
        Err(SwapError::Busy)
    ));
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    assert!(matches!(
        super::startup(&fx.dir().join(EXE_NAME), &fx.state_dir(), &mut env),
        Ok(super::Startup::Busy)
    ));
}

#[test]
fn the_running_exe_must_be_named_evorag() {
    let fx = Fixture::new(None);
    let other = fx.dir().join("evorag-copy.bin");
    std::fs::write(&other, b"x").expect("write");
    assert!(matches!(
        Installation::open(&other, &fx.state_dir(), Purpose::Update),
        Err(SwapError::ExeName { .. })
    ));
    Installation::open(&other, &fx.state_dir(), Purpose::Startup).expect("startup accepts it");
    // パスは正規化してから見る (`..` を含むパスでも、同じインストール先になる)。
    let roundabout = fx.dir().join("..").join("install").join(EXE_NAME);
    let inst = Installation::open(&roundabout, &fx.state_dir(), Purpose::Update).expect("open");
    assert_eq!(
        inst.dir(),
        std::fs::canonicalize(fx.dir())
            .map(|p| {
                let s = p.to_string_lossy().into_owned();
                PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
            })
            .expect("canonical")
    );
    drop(inst);
    // Windows と macOS は大文字と小文字を区別しない。
    if cfg!(any(windows, target_os = "macos")) {
        let upper = fx.dir().join(EXE_NAME.to_ascii_uppercase());
        Installation::open(&upper, &fx.state_dir(), Purpose::Update).expect("case-insensitive");
    }
}

#[test]
fn the_same_file_and_a_pending_plan_are_refused() {
    let fx = Fixture::new(None);
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    assert!(matches!(
        inst.apply_bytes(
            &exe(CURRENT),
            sha(CURRENT),
            &build(CURRENT),
            &build(CURRENT),
            &mut env
        ),
        Err(SwapError::SameFile)
    ));
    drop(inst);
    let fx = Fixture::new(Some(OLD_PREV));
    crashed_after_plan(&fx);
    let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    assert!(matches!(
        inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env
        ),
        Err(SwapError::PlanPending)
    ));
}

#[test]
fn the_state_files_are_private() {
    let fx = Fixture::new(None);
    run(&fx, Method::HOST, Scenario::ApplyFresh, None).expect("apply");
    let update = fx.state_dir().join("update");
    if evorag_store::fsutil::acl_applicable(&update).expect("acl") {
        assert!(evorag_store::fsutil::is_private(&update).expect("dir"));
        assert!(evorag_store::fsutil::is_private(&update.join("state.json")).expect("file"));
    }
}

#[test]
fn an_unwritable_install_dir_is_refused_without_changes() {
    let fx = Fixture::new(Some(OLD_PREV));
    let dir = fx.dir();
    let Some(_restore) = make_unwritable(&dir) else {
        eprintln!("skipped: cannot make the folder unwritable here");
        return;
    };
    let before = fx.listing();
    let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    let e = inst.check_install_dir().expect_err("refused");
    assert!(matches!(e, SwapError::NotWritable { .. }), "{e:?}");
    assert_eq!(fx.listing(), before);
}

/// 落とすと書き込みの権限を戻す。
struct Restore(Box<dyn FnMut()>);

impl Drop for Restore {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[cfg(windows)]
fn make_unwritable(dir: &Path) -> Option<Restore> {
    let icacls = |args: &[&str]| {
        std::process::Command::new("icacls")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // Everyone (S-1-1-0) に、ファイルとフォルダの作成を拒否する。
    if !icacls(&["/deny", "*S-1-1-0:(WD,AD)"]) {
        return None;
    }
    let dir = dir.to_path_buf();
    Some(Restore(Box::new(move || {
        let _ = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/remove:d", "*S-1-1-0"])
            .stdout(std::process::Stdio::null())
            .status();
    })))
}

#[cfg(unix)]
fn make_unwritable(dir: &Path) -> Option<Restore> {
    use std::os::unix::fs::PermissionsExt;
    if evorag_store::fsutil::effective_uid() == 0 {
        return None;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).ok()?;
    let dir = dir.to_path_buf();
    Some(Restore(Box::new(move || {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    })))
}

/// `evorag(.exe)` がリパースポイント (Windows のジャンクション) なら断る。
#[cfg(windows)]
#[test]
fn a_reparse_point_exe_is_refused() {
    let fx = Fixture::new(None);
    std::fs::remove_file(fx.path(FileName::Exe)).expect("rm");
    let target = fx.root.path().join("target");
    std::fs::create_dir_all(&target).expect("mkdir");
    let ok = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(fx.path(FileName::Exe))
        .arg(&target)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: cannot make a junction");
        return;
    }
    let meta = std::fs::symlink_metadata(fx.path(FileName::Exe)).expect("meta");
    assert!(evorag_store::fsutil::is_link_or_reparse_point(&meta));
    assert_link_exe_is_refused(&fx);
}

/// `evorag(.exe)` がリンクのとき: 動いている実行ファイルのパスは正規化する (リンクの先の名前になる) ので
/// apply・rollback は開けず、同じフォルダのほかの名前から開いても、インストール先の確かめが断る。
fn assert_link_exe_is_refused(fx: &Fixture) {
    assert!(matches!(
        fx.open(Method::HOST, Purpose::Update),
        Err(SwapError::ExeName { .. })
    ));
    let other = fx.dir().join("evorag-other");
    std::fs::write(&other, b"x").expect("write");
    let inst = Installation::open(&other, &fx.state_dir(), Purpose::Startup).expect("open");
    assert!(matches!(
        inst.check_install_dir(),
        Err(SwapError::NotRegularFile {
            name: FileName::Exe
        })
    ));
}

/// 計画を `reverting` にした直後に止めた状態を作る (元に戻すのに失敗したのと同じ: 計画は `reverting`、
/// `evorag(.exe)` は `next`)。
fn crashed_while_reverting(fx: &Fixture, method: Method) {
    let probe = FakeProbe::with(ProbeMode::Wrong);
    let mut hook = |e| e == Event::StateSaved && fx.state_json()["plan"]["reverting"] == true;
    let mut env = Env::new(&probe).with_crash_hook(&mut hook);
    let mut inst = fx.open(method, Purpose::Update).expect("open");
    let r = inst.apply_bytes(
        &exe(NEXT),
        sha(NEXT),
        &build(CURRENT),
        &build(NEXT),
        &mut env,
    );
    assert!(matches!(r, Err(SwapError::Crashed)), "{method:?}: {r:?}");
    assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)), "{method:?}");
    assert_eq!(fx.state_json()["plan"]["reverting"], true, "{method:?}");
}

/// 12 §7.3 (続きが元に戻して、動いているイメージ (`next`) と `evorag(.exe)` (`current`) が違えば、
/// そう示す。呼ぶ側はデータの移行の前に止まり、もう一度起動するよう示す)
#[test]
fn a_startup_that_reverts_the_running_exe_reports_it_replaced() {
    for method in methods() {
        let fx = Fixture::new(Some(OLD_PREV));
        crashed_while_reverting(&fx, method);
        // 起動の片付けを、`next` (今の evorag(.exe)) として動かす。
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let report = if method == Method::HOST {
            match super::startup(&fx.dir().join(EXE_NAME), &fx.state_dir(), &mut env) {
                Ok(super::Startup::Resumed(report)) => report,
                r => panic!("{method:?}: {r:?}"),
            }
        } else {
            let mut inst = fx.open(method, Purpose::Startup).expect("open");
            inst.resume(&mut env).expect("resume").expect("a plan")
        };
        assert!(
            matches!(report.outcome, ResumeOutcome::Reverted { .. }),
            "{method:?}: {report:?}"
        );
        assert!(report.running_replaced, "{method:?}: {report:?}");
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)), "{method:?}");
        assert_consistent(&fx, Scenario::ApplyRejected, &format!("{method:?}"));
    }
}

/// 12 §7.3 (起動の片付けの続きが途中で失敗しても、失敗した時点の `evorag(.exe)` を動いているイメージ
/// (`next`) と比べて示す)。元に戻す途中のどの操作の後で止まっても、`evorag(.exe)` が `next` でなくなって
/// いれば、置き換わったと示す。
#[test]
fn a_startup_whose_revert_fails_part_way_reports_whether_the_running_exe_was_replaced() {
    let mut replaced = 0;
    for method in methods() {
        for k in 1.. {
            let fx = Fixture::new(Some(OLD_PREV));
            crashed_while_reverting(&fx, method);
            let probe = FakeProbe::default();
            let mut hook = crash_hook(Some(k));
            let mut env = Env::new(&probe).with_crash_hook(&mut hook);
            let mut inst = fx.open(method, Purpose::Startup).expect("open");
            let ctx = format!("{method:?} crash {k}");
            match inst.startup(&mut env) {
                Ok(super::Startup::Resumed(report)) => {
                    assert!(report.running_replaced, "{ctx}: {report:?}");
                    break;
                }
                Ok(super::Startup::ResumeFailed {
                    error: SwapError::Crashed,
                    running_replaced,
                }) => {
                    let exe_is_next = fx.digest(FileName::Exe) == Some(sha(NEXT));
                    assert_eq!(running_replaced, !exe_is_next, "{ctx}: {:?}", fx.listing());
                    replaced += usize::from(running_replaced);
                }
                r => panic!("{ctx}: {r:?}"),
            }
            drop(inst);
            recover(&fx, method, ProbeMode::Honest, None).expect("recover");
            assert_consistent(&fx, Scenario::ApplyRejected, &ctx);
        }
    }
    assert!(replaced > 0);
}

/// 12 §7.3 (元に戻して `evorag(.exe)` を `current` にした後、計画を済みにできないときも、動いている `next`
/// は置いてあるものと違うと示す。呼ぶ側は止まる)。
#[test]
fn a_startup_whose_revert_cannot_be_recorded_reports_the_running_exe_replaced() {
    for method in methods() {
        let fx = Fixture::new(Some(OLD_PREV));
        crashed_while_reverting(&fx, method);
        let probe = FakeProbe::default();
        let mut blocked = None;
        let r = {
            // 続きの最初の操作の後に、状態のファイルを置けなくする (開くときに状態のフォルダの権限を
            // 締め直すので、開く前には塞げない)。
            let mut hook = |_| {
                if blocked.is_none() {
                    blocked = Some(block_state_file(&fx));
                }
                false
            };
            let mut env = Env::new(&probe).with_crash_hook(&mut hook);
            let mut inst = fx.open(method, Purpose::Startup).expect("open");
            inst.startup(&mut env)
        };
        match blocked {
            Some(Some(restore)) => drop(restore),
            _ => {
                eprintln!("skipped: cannot block the state file here");
                return;
            }
        }
        match r {
            Ok(super::Startup::ResumeFailed {
                error: SwapError::RevertedNotRecorded { cause: None, .. },
                running_replaced: true,
            }) => {}
            r => panic!("{method:?}: {r:?}"),
        }
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)), "{method:?}");
        assert!(fx.has_plan(), "{method:?}");
        recover(&fx, method, ProbeMode::Honest, None).expect("recover");
        assert_consistent(&fx, Scenario::ApplyRejected, &format!("{method:?}"));
    }
}

/// 続きが、動いているイメージと同じものを `evorag(.exe)` に残すとき (置き終えた・戻したが元から
/// `current` だった) は、置き換わったとは示さない。動いているイメージの SHA-256 は、計画があるときだけ求める。
#[test]
fn a_resume_that_keeps_the_running_exe_reports_no_replacement() {
    let mut resumed = 0;
    for method in methods() {
        for s in [Scenario::ApplyWithPrev, Scenario::Rollback] {
            for k in 1.. {
                let fx = Fixture::new(s.prev());
                if !matches!(run(&fx, method, s, Some(k)), Err(SwapError::Crashed)) {
                    break;
                }
                if !fx.has_plan() || !fx.path(FileName::Exe).exists() {
                    continue;
                }
                let probe = FakeProbe::default();
                let mut env = Env::new(&probe);
                let mut inst = fx.open(method, Purpose::Startup).expect("open");
                assert!(!inst.running_digest_computed(), "computed on open");
                let report = inst.resume(&mut env).expect("resume").expect("a plan");
                assert!(inst.running_digest_computed());
                let ctx = format!("{method:?} {s:?} crash {k}: {report:?}");
                assert!(!report.running_replaced, "{ctx}");
                assert_consistent(&fx, s, &ctx);
                resumed += 1;
            }
        }
    }
    assert!(resumed > 0);
    // 計画が無ければ求めない。
    let fx = Fixture::new(Some(OLD_PREV));
    let probe = FakeProbe::default();
    let mut env = Env::new(&probe);
    let mut inst = fx.open(Method::HOST, Purpose::Startup).expect("open");
    assert!(inst.resume(&mut env).expect("resume").is_none());
    inst.cleanup_by_name(Strictness::Lenient).expect("cleanup");
    assert!(!inst.running_digest_computed());
}

/// 状態のファイルを置けなくする (Unix: `update` のフォルダを読むだけにする。Windows: `state.json` を
/// 削除の共有なしで開いておく)。落とすと戻す。Unix の root は置けなくできないので `None`。
fn block_state_file(fx: &Fixture) -> Option<Restore> {
    let update = fx.state_dir().join("update");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if evorag_store::fsutil::effective_uid() == 0 {
            return None;
        }
        std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o500)).ok()?;
        Some(Restore(Box::new(move || {
            let _ = std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o700));
        })))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        let mut held = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(update.join("state.json"))
                .ok()?,
        );
        Some(Restore(Box::new(move || drop(held.take()))))
    }
}

/// 元に戻した後に計画を済みにできなければ、「元に戻したが記録できなかった」と示し (戻せなかったとは
/// 言わない)、計画を残す。次の続きが済ませる。
#[test]
fn a_revert_that_cannot_be_recorded_says_the_files_were_put_back() {
    for method in methods() {
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::with(ProbeMode::Wrong);
        let mut armed = false;
        let mut blocked = None;
        let r = {
            let mut hook = |e| {
                if e == Event::StateSaved && fx.state_json()["plan"]["reverting"] == true {
                    armed = true;
                } else if e == Event::StateWritten && armed && blocked.is_none() {
                    blocked = Some(block_state_file(&fx));
                }
                false
            };
            let mut env = Env::new(&probe).with_crash_hook(&mut hook);
            let mut inst = fx.open(method, Purpose::Update).expect("open");
            inst.apply_bytes(
                &exe(NEXT),
                sha(NEXT),
                &build(CURRENT),
                &build(NEXT),
                &mut env,
            )
        };
        match blocked {
            Some(Some(restore)) => drop(restore),
            _ => {
                eprintln!("skipped: cannot block the state file here");
                return;
            }
        }
        let e = r.expect_err("not recorded");
        assert!(
            matches!(
                e,
                SwapError::RevertedNotRecorded {
                    cause: Some(Failure::Check(_)),
                    ..
                }
            ),
            "{method:?}: {e:?}"
        );
        assert!(e.to_string().contains("put back"), "{e}");
        assert_eq!(e.exit_code(), 1);
        assert!(!e.needs_manual_recovery());
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)), "{method:?}");
        assert!(fx.has_plan(), "{method:?}");
        recover(&fx, method, ProbeMode::Wrong, None).expect("recover");
        assert_consistent(&fx, Scenario::ApplyRejected, &format!("{method:?}"));
    }
}

/// 上から置く方式: 写した evorag.tmp を読み直して `current` と合わなければ、改名の前に写しを消して止める。
#[test]
fn a_copy_that_does_not_match_is_removed_before_any_rename() {
    let tamper = |fx: &Fixture| {
        let tmp = fx.path(FileName::Tmp);
        move |e: Event| {
            if e == Event::Wrote(FileName::Tmp) {
                std::fs::write(&tmp, b"corrupted copy").expect("tamper");
            }
            false
        }
    };
    let is_mismatch = |e: &SwapError| {
        matches!(
            e,
            SwapError::CopiedDigestMismatch {
                name: FileName::Tmp
            }
        )
    };
    let assert_untouched = |fx: &Fixture| {
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        for name in FileName::LEFTOVERS {
            assert!(!fx.path(name).exists(), "{name}: {:?}", fx.listing());
        }
        assert!(!fx.has_plan());
    };
    // apply: 段 2 の写し。
    let fx = Fixture::new(Some(OLD_PREV));
    let probe = FakeProbe::default();
    let mut events = Vec::new();
    let inner = tamper(&fx);
    let mut inst = fx.open(Method::Replace, Purpose::Update).expect("open");
    let r = {
        let mut hook = |e| {
            events.push(e);
            inner(e)
        };
        let mut env = Env::new(&probe).with_crash_hook(&mut hook);
        inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        )
    };
    match &r {
        Err(SwapError::Reverted(Failure::Other(e))) if is_mismatch(e) => {}
        r => panic!("{r:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Renamed(_, FileName::Exe))),
        "{events:?}"
    );
    assert_untouched(&fx);
    drop(inst);
    // rollback: 1 の写し。
    let fx = Fixture::new(Some(OLD_PREV));
    let probe = FakeProbe::default();
    let mut inst = fx.open(Method::Replace, Purpose::Update).expect("open");
    let prev = inst.identify_prev(&mut Env::new(&probe)).expect("prev");
    let mut hook = tamper(&fx);
    let mut env = Env::new(&probe).with_crash_hook(&mut hook);
    let e = inst
        .rollback(&prev, &build(CURRENT), &mut env)
        .expect_err("refused");
    assert!(is_mismatch(&e), "{e:?}");
    assert_untouched(&fx);
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_symlinked_exe_is_refused() {
        let fx = Fixture::new(None);
        let real = fx.root.path().join("real-evorag");
        std::fs::rename(fx.path(FileName::Exe), &real).expect("mv");
        std::os::unix::fs::symlink(&real, fx.path(FileName::Exe)).expect("symlink");
        assert_link_exe_is_refused(&fx);
    }

    #[test]
    fn group_writable_folders_are_refused_unless_sticky() {
        let fx = Fixture::new(None);
        let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        let set = |mode| {
            std::fs::set_permissions(fx.dir(), std::fs::Permissions::from_mode(mode))
                .expect("chmod");
        };
        set(0o775);
        assert!(matches!(
            inst.check_install_dir(),
            Err(SwapError::UnsafeOwnership {
                problem: super::super::OwnershipProblem::WritableByOthers,
                ..
            })
        ));
        set(0o1777);
        inst.check_install_dir()
            .expect("a sticky folder is accepted");
        set(0o755);
        std::fs::set_permissions(
            fx.path(FileName::Exe),
            std::fs::Permissions::from_mode(0o757),
        )
        .expect("chmod");
        assert!(matches!(
            inst.check_install_dir(),
            Err(SwapError::UnsafeOwnership { .. })
        ));
    }

    /// 新しいファイルの権限は、今の evorag の権限 (group と other の書き込みを除く) に合わせる。名前に
    /// 置いたシンボリックリンクの先には書かない。リンクの `evorag.tmp` は役割に当たらないので、apply の
    /// 前の片付けが断る。
    #[test]
    fn new_files_keep_the_exe_mode_and_do_not_follow_symlinks() {
        let fx = Fixture::new(None);
        std::fs::set_permissions(
            fx.path(FileName::Exe),
            std::fs::Permissions::from_mode(0o750),
        )
        .expect("chmod");
        let outside = fx.root.path().join("outside");
        std::fs::write(&outside, exe(CURRENT)).expect("write");
        std::os::unix::fs::symlink(&outside, fx.path(FileName::Tmp)).expect("symlink");
        assert!(matches!(
            run(&fx, Method::HOST, Scenario::ApplyFresh, None),
            Err(SwapError::UnrelatedLeftover {
                name: FileName::Tmp
            })
        ));
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        std::fs::remove_file(fx.path(FileName::Tmp)).expect("rm");
        std::os::unix::fs::symlink(&outside, fx.path(FileName::New)).expect("symlink");
        run(&fx, Method::HOST, Scenario::ApplyFresh, None).expect("apply");
        assert_eq!(std::fs::read(&outside).expect("read"), exe(CURRENT));
        let mode = |n| {
            std::fs::metadata(fx.path(n))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(FileName::Exe), 0o750);
        assert_eq!(mode(FileName::Prev), 0o750);
    }

    /// 12 §7.3 (Unix: シンボリックリンクと、ほかの利用者の役割のファイルは、役割に当たらない
    /// ものとして扱い、動かさず、`--version` で動かさない)
    #[test]
    fn symlinked_or_foreign_owned_role_files_match_no_role() {
        let fx = Fixture::new(Some(OLD_PREV));
        crashed_after_plan(&fx);
        // 段 1 の evorag.new を、同じ中身のファイルへのシンボリックリンクにする。
        let outside = fx.root.path().join("outside-new");
        std::fs::write(&outside, exe(NEXT)).expect("write");
        std::fs::remove_file(fx.path(FileName::New)).expect("rm");
        std::os::unix::fs::symlink(&outside, fx.path(FileName::New)).expect("symlink");
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        let report = inst.resume(&mut env).expect("resume").expect("a plan");
        assert_eq!(report.untouched, vec![FileName::New]);
        assert!(
            std::fs::symlink_metadata(fx.path(FileName::New))
                .expect("kept")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&outside).expect("read"), exe(NEXT));
        // リンクの evorag.prev は、版を知るために動かさない。
        std::fs::remove_file(fx.path(FileName::Prev)).expect("rm");
        let target = fx.root.path().join("outside-prev");
        std::fs::write(&target, exe(OLD_PREV)).expect("write");
        std::os::unix::fs::symlink(&target, fx.path(FileName::Prev)).expect("symlink");
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        assert!(matches!(
            inst.identify_prev(&mut env),
            Err(SwapError::NotRegularFile {
                name: FileName::Prev
            })
        ));
        assert_eq!(probe.calls.get(), 0);
        // ほかの利用者の evorag.prev も同じ (持ち主を変えられるのは root だけ)。
        if evorag_store::fsutil::effective_uid() != 0 {
            eprintln!("skipped the foreign-owner case: not root");
            return;
        }
        std::fs::remove_file(fx.path(FileName::Prev)).expect("rm");
        put(fx.path(FileName::Prev), exe(OLD_PREV)).expect("write");
        std::os::unix::fs::chown(fx.path(FileName::Prev), Some(1), None).expect("chown");
        assert!(matches!(
            inst.identify_prev(&mut env),
            Err(SwapError::NotRegularFile {
                name: FileName::Prev
            })
        ));
        assert_eq!(probe.calls.get(), 0);
    }

    /// 12 §7.2 (rollback の 2: インストール先の確かめに通らなければ、evorag.prev を `--version` で
    /// 動かさない)
    #[test]
    fn identify_prev_checks_the_install_dir_before_running_evorag_prev() {
        let fx = Fixture::new(Some(OLD_PREV));
        let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        let set = |mode| {
            std::fs::set_permissions(fx.dir(), std::fs::Permissions::from_mode(mode))
                .expect("chmod");
        };
        let probe = FakeProbe::default();
        set(0o775);
        let r = inst.identify_prev(&mut Env::new(&probe));
        set(0o755);
        assert!(
            matches!(
                r,
                Err(SwapError::UnsafeOwnership {
                    problem: super::super::OwnershipProblem::WritableByOthers,
                    ..
                })
            ),
            "{r:?}"
        );
        assert_eq!(probe.calls.get(), 0);
        inst.identify_prev(&mut Env::new(&probe)).expect("prev");
        assert_eq!(probe.calls.get(), 1);
    }

    /// Linux・macOS の rollback で計画を書けなければ、写した evorag.tmp を消す。
    #[test]
    fn a_rollback_that_cannot_write_its_plan_removes_the_copy() {
        if evorag_store::fsutil::effective_uid() == 0 {
            eprintln!("skipped: root can write anywhere");
            return;
        }
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        let prev = inst.identify_prev(&mut env).expect("prev");
        let update = fx.state_dir().join("update");
        std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let r = inst.rollback(&prev, &build(CURRENT), &mut env);
        std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(matches!(r, Err(SwapError::Io { .. })), "{r:?}");
        assert!(!fx.path(FileName::Tmp).exists());
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        assert!(!fx.has_plan());
    }

    /// 入れ替えが済んだ後に計画を済みにできなければ、そう示し、次の続きが済ませる。
    #[test]
    fn a_swap_that_cannot_be_recorded_says_so_and_converges_later() {
        if evorag_store::fsutil::effective_uid() == 0 {
            eprintln!("skipped: root can write anywhere");
            return;
        }
        let fx = Fixture::new(Some(OLD_PREV));
        let update = fx.state_dir().join("update");
        let probe = FakeProbe::default();
        let mut hook = |e| {
            if e == Event::Renamed(FileName::Tmp, FileName::Prev) {
                std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o500))
                    .expect("chmod");
            }
            false
        };
        let mut env = Env::new(&probe).with_crash_hook(&mut hook);
        let mut inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        let r = inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        );
        std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let e = r.expect_err("not recorded");
        assert!(matches!(e, SwapError::NotRecorded(_)), "{e:?}");
        assert_eq!(e.exit_code(), 1);
        assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)));
        drop(inst);
        recover(&fx, Method::HOST, ProbeMode::Honest, None).expect("recover");
        assert_consistent(&fx, Scenario::ApplyWithPrev, "recorded later");
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::fs::OpenOptionsExt;

    use super::super::FileOp;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// 削除の共有なしでファイルを開いておく (ウイルス対策のソフトの代わり)。
    fn hold(path: PathBuf, for_: Duration) -> std::thread::JoinHandle<()> {
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            const FILE_SHARE_READ: u32 = 1;
            const FILE_SHARE_WRITE: u32 = 2;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&path)
                .expect("open without delete sharing");
            tx.send(()).expect("send");
            std::thread::sleep(for_);
            drop(f);
        });
        rx.recv().expect("held");
        t
    }

    #[test]
    fn a_lock_shorter_than_the_retries_is_waited_out() {
        let fx = Fixture::new(Some(OLD_PREV));
        let t = hold(fx.path(FileName::Exe), Duration::from_secs(1));
        run(&fx, Method::TwoStep, Scenario::ApplyWithPrev, None).expect("apply");
        t.join().expect("join");
        assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(CURRENT)));
    }

    /// 2 秒の再試行より長いロックでは、元に戻して理由を示す。前回の evorag.prev は失わない。
    #[test]
    fn a_lock_longer_than_the_retries_reverts_and_keeps_the_old_prev() {
        let fx = Fixture::new(Some(OLD_PREV));
        let t = hold(fx.path(FileName::Exe), Duration::from_secs(5));
        let r = run(&fx, Method::TwoStep, Scenario::ApplyWithPrev, None);
        assert!(
            matches!(
                r,
                Err(SwapError::Reverted(Failure::File {
                    op: FileOp::Rename,
                    name: FileName::Exe,
                    ..
                }))
            ),
            "{r:?}"
        );
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        assert!(!fx.path(FileName::PrevOld).exists());
        assert!(!fx.path(FileName::New).exists());
        assert!(!fx.has_plan());
        t.join().expect("join");
    }

    /// 消せない残りのファイルがあれば、apply・rollback の前の片付けは断り、起動の片付けは残す。
    #[test]
    fn a_locked_leftover_refuses_updates_but_not_startup() {
        let fx = Fixture::new(None);
        put(fx.path(FileName::New), b"x").expect("write");
        let t = hold(fx.path(FileName::New), Duration::from_secs(5));
        let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        assert!(matches!(
            inst.cleanup_by_name(Strictness::Strict),
            Err(SwapError::Leftover {
                name: FileName::New,
                ..
            })
        ));
        let report = inst.cleanup_by_name(Strictness::Lenient).expect("lenient");
        assert_eq!(report.left, vec![FileName::New]);
        t.join().expect("join");
    }
    /// 12 §7.3 (Windows の隙間の前に、置くファイルが掴まれていないかを確かめる。掴まれたままなら
    /// evorag.exe を空けずに元に戻す)
    #[test]
    fn a_locked_file_to_place_is_refused_before_the_gap() {
        // apply: 段 2 を書いた後に、evorag.new を掴む。
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::default();
        let new = fx.path(FileName::New);
        let mut saved = 0;
        let mut holder = None;
        let mut events = Vec::new();
        let mut hook = |e| {
            events.push(e);
            if e == Event::StateSaved {
                saved += 1;
                if saved == 2 {
                    holder = Some(hold(new.clone(), Duration::from_millis(3500)));
                }
            }
            false
        };
        let mut env = Env::new(&probe).with_crash_hook(&mut hook);
        let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        let r = inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        );
        holder.expect("held").join().expect("join");
        assert!(
            matches!(
                r,
                Err(SwapError::Reverted(Failure::File {
                    op: FileOp::Open,
                    name: FileName::New,
                    ..
                }))
            ),
            "{r:?}"
        );
        assert!(!events.contains(&Event::Renamed(FileName::Exe, FileName::Prev)));
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        assert!(!fx.has_plan());

        // rollback: evorag.prev を掴む。
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        let prev = inst.identify_prev(&mut env).expect("prev");
        let t = hold(fx.path(FileName::Prev), Duration::from_millis(3500));
        let r = inst.rollback(&prev, &build(CURRENT), &mut env);
        t.join().expect("join");
        assert!(
            matches!(
                r,
                Err(SwapError::Reverted(Failure::File {
                    op: FileOp::Open,
                    name: FileName::Prev,
                    ..
                }))
            ),
            "{r:?}"
        );
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        assert!(!fx.has_plan());
    }

    /// 12 §7.3 (隙間の 2 つ目の改名は 1 回だけ試し、失敗したら、まず自分を evorag.exe に戻して
    /// から reverting を書き、残りを戻す。2 つの改名の間に状態を書かない)
    #[test]
    fn a_failed_second_rename_puts_the_exe_back_first() {
        let fx = Fixture::new(Some(OLD_PREV));
        let probe = FakeProbe::default();
        let new = fx.path(FileName::New);
        let mut holder = None;
        let mut events = Vec::new();
        let mut hook = |e| {
            events.push(e);
            if e == Event::Renamed(FileName::Exe, FileName::Prev) && holder.is_none() {
                holder = Some(hold(new.clone(), Duration::from_millis(1000)));
            }
            false
        };
        let mut env = Env::new(&probe).with_crash_hook(&mut hook);
        let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        let r = inst.apply_bytes(
            &exe(NEXT),
            sha(NEXT),
            &build(CURRENT),
            &build(NEXT),
            &mut env,
        );
        holder.expect("held").join().expect("join");
        assert!(
            matches!(
                r,
                Err(SwapError::Reverted(Failure::File {
                    op: FileOp::Rename,
                    name: FileName::New,
                    ..
                }))
            ),
            "{r:?}"
        );
        let gap = events
            .iter()
            .position(|&e| e == Event::Renamed(FileName::Exe, FileName::Prev))
            .expect("gap");
        assert_eq!(
            events[gap + 1..gap + 3],
            [
                Event::Renamed(FileName::Prev, FileName::Exe),
                Event::StateWritten
            ],
            "{events:?}"
        );
        assert_eq!(fx.digest(FileName::Exe), Some(sha(CURRENT)));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(OLD_PREV)));
        assert!(!fx.has_plan());
    }

    /// 12 §7.3 (Windows: ほかの利用者が書き込めるインストール先は断る。TrustedInstaller と、
    /// 継承だけの CREATOR OWNER は許す)
    #[test]
    fn an_install_folder_writable_by_other_users_is_refused() {
        const TRUSTED_INSTALLER: &str =
            "*S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
        let fx = Fixture::new(Some(OLD_PREV));
        let inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        inst.check_install_dir().expect("owner-only folder");
        let icacls = |args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(fx.dir())
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(icacls(&["/grant", &format!("{TRUSTED_INSTALLER}:(F)")]));
        // CREATOR OWNER (S-1-3-0) の継承だけの ACE は、フォルダ自身に効かない。
        assert!(icacls(&["/grant", "*S-1-3-0:(OI)(CI)(IO)(F)"]));
        inst.check_install_dir()
            .expect("TrustedInstaller and inherit-only ACEs are accepted");
        // Users (S-1-5-32-545) に書き込みを許す。
        assert!(icacls(&["/grant", "*S-1-5-32-545:(W)"]));
        let before = fx.listing();
        assert!(matches!(
            inst.check_install_dir(),
            Err(SwapError::WritableByOtherUsers { .. })
        ));
        drop(inst);
        assert!(matches!(
            run(&fx, Method::TwoStep, Scenario::ApplyWithPrev, None),
            Err(SwapError::WritableByOtherUsers { .. })
        ));
        assert_eq!(fx.listing(), before);
        assert!(!fx.has_plan());
    }

    /// 12 §7.3 (Windows: 中に作るファイルへ継承する ACE で、ほかの利用者に書き込みを許すインストール先も
    /// 断る。evorag.new などが、その権限を継承するため)
    #[test]
    fn an_install_folder_whose_new_files_others_can_write_is_refused() {
        let fx = Fixture::new(Some(OLD_PREV));
        let ok = std::process::Command::new("icacls")
            .arg(fx.dir())
            .args(["/grant", "*S-1-5-32-545:(OI)(IO)(M)"])
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "icacls");
        let before = fx.listing();
        assert!(matches!(
            run(&fx, Method::TwoStep, Scenario::ApplyWithPrev, None),
            Err(SwapError::WritableByOtherUsers { .. })
        ));
        assert_eq!(fx.listing(), before);
        assert!(!fx.has_plan());
    }

    /// 12 §7.2 (rollback の 2: ほかの利用者が書き込めるインストール先では、evorag.prev を
    /// `--version` で動かさない)
    #[test]
    fn identify_prev_refuses_a_folder_writable_by_other_users() {
        let fx = Fixture::new(Some(OLD_PREV));
        let ok = std::process::Command::new("icacls")
            .arg(fx.dir())
            .args(["/grant", "*S-1-5-32-545:(W)"])
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "icacls");
        let probe = FakeProbe::default();
        let inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        let r = inst.identify_prev(&mut Env::new(&probe));
        assert!(
            matches!(r, Err(SwapError::WritableByOtherUsers { .. })),
            "{r:?}"
        );
        assert_eq!(probe.calls.get(), 0);
    }

    /// 12 §7.3 (Windows: インストール先の持ち主が、今の利用者・SYSTEM・Administrators・TrustedInstaller
    /// のどれでもなければ断る)。持ち主をほかの相手にするには特権が要るので、できなければ飛ばす (判定は
    /// evorag-store の winacl の試験が、作った SID で確かめる)。
    #[test]
    fn an_install_folder_owned_by_another_user_is_refused() {
        let fx = Fixture::new(Some(OLD_PREV));
        let inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        inst.check_install_dir()
            .expect("a folder we created is ours");
        let ok = std::process::Command::new("icacls")
            .arg(fx.dir())
            .args(["/setowner", "*S-1-5-32-545"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("skipped: cannot give the folder another owner without privileges");
            return;
        }
        assert!(
            matches!(
                inst.check_install_dir(),
                Err(SwapError::OwnedByOtherUser { .. })
            ),
            "owned by Users"
        );
    }

    /// 12 §7.2 (Windows: `evorag.prev.exe` があれば、`evorag.prev` を動かさずに断る)
    #[test]
    fn an_exe_shadowing_evorag_prev_is_refused() {
        let fx = Fixture::new(Some(OLD_PREV));
        let shadow = fx.dir().join("evorag.prev.exe");
        std::fs::write(&shadow, exe(NEXT)).expect("write");
        let probe = FakeProbe::default();
        let mut env = Env::new(&probe);
        let inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        match inst.identify_prev(&mut env) {
            Err(SwapError::BlockingFile { path }) => {
                assert_eq!(path.file_name(), shadow.file_name());
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(probe.calls.get(), 0);
    }

    /// 落とすと子のプロセスを止める。
    struct Running(std::process::Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// 12 §7.3 (Windows: 動いている evorag.exe を改名して置き換え、元に戻すときは動いている
    /// イメージを evorag.exe に戻せる。動いているイメージ (evorag.prev.old) は消せないので、再試行の後に
    /// 残し、次の片付けで消す)
    #[test]
    fn a_running_exe_is_swapped_and_its_image_is_removed_later() {
        let ping = Path::new(r"C:\Windows\System32\PING.EXE");
        if !ping.is_file() {
            eprintln!("skipped: no PING.EXE");
            return;
        }
        let fx = Fixture::new(None);
        std::fs::copy(ping, fx.path(FileName::Exe)).expect("copy");
        let image = fx.digest(FileName::Exe).expect("image");
        // ループバックだけに送る。最後に止める。
        let child = std::process::Command::new(fx.path(FileName::Exe))
            .args(["-n", "60", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start the copy of PING.EXE");
        let mut running = Running(child);
        // 動き出すのを待つ (イメージが写像されるまで)。
        std::thread::sleep(Duration::from_millis(300));
        let apply = |version: &str, mode: ProbeMode| {
            let probe = FakeProbe::with(mode);
            let mut env = Env::new(&probe);
            let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
            inst.apply_bytes(
                &exe(version),
                sha(version),
                &build(CURRENT),
                &build(version),
                &mut env,
            )
        };
        // 1. 確かめに通らない: 動いている evorag.exe を evorag.prev に改名して置き、元に戻すときは、
        //    動いているイメージを evorag.exe に戻す (隙間の前の確かめが、動いているイメージを開ける)。
        let r = apply(NEXT, ProbeMode::Wrong);
        assert!(
            matches!(r, Err(SwapError::Reverted(Failure::Check(_)))),
            "{r:?}"
        );
        assert_eq!(fx.digest(FileName::Exe), Some(image));
        assert!(!fx.path(FileName::Rejected).exists());
        // 2. 置き換える: 動いているものは evorag.prev になる。
        let report = apply(NEXT, ProbeMode::Honest).expect("apply while running");
        assert!(report.left.is_empty());
        assert_eq!(fx.digest(FileName::Exe), Some(sha(NEXT)));
        assert_eq!(fx.digest(FileName::Prev), Some(image));
        // 3. もう一度置き換える: 動いているイメージ (前回の evorag.prev) は evorag.prev.old に改名でき、
        //    消せないので残す。
        let report = apply("0.5.0", ProbeMode::Honest).expect("apply again");
        assert_eq!(report.left, vec![FileName::PrevOld]);
        assert_eq!(fx.digest(FileName::Exe), Some(sha("0.5.0")));
        assert_eq!(fx.digest(FileName::Prev), Some(sha(NEXT)));
        assert_eq!(fx.digest(FileName::PrevOld), Some(image));
        let mut inst = fx.open(Method::TwoStep, Purpose::Update).expect("open");
        assert!(matches!(
            inst.cleanup_by_name(Strictness::Strict),
            Err(SwapError::Leftover {
                name: FileName::PrevOld,
                ..
            })
        ));
        let report = inst.cleanup_by_name(Strictness::Lenient).expect("lenient");
        assert_eq!(report.left, vec![FileName::PrevOld]);
        // 止めた後の片付けで消える。
        let _ = running.0.kill();
        let _ = running.0.wait();
        let report = inst.cleanup_by_name(Strictness::Strict).expect("strict");
        assert_eq!(report.removed, vec![FileName::PrevOld]);
        assert!(!fx.path(FileName::PrevOld).exists());
    }
}
