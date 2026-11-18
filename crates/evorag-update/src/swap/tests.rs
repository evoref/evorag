//! 置き換え・巻き戻し・続き・片付けの試験 (09 §3.6・§4、12 §7.2・§7.3)。
//!
//! 実行ファイルの代わりに、版を書いた小さなファイルを置き、`--version` は偽物 ([`FakeProbe`]) が
//! ファイルの中身から答える。Windows では両方の方式 (自分を改名する方式と、上から置く方式) を、
//! Linux では上から置く方式を試す。

use std::cell::Cell;
use std::path::{Path, PathBuf};

use evorag_store::testutil::TempDir;

use super::installation::{Build, Env, Event, Installation, Purpose, ResumeOutcome, Strictness};
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

/// インストール先と状態のフォルダ。
struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new(prev: Option<&str>) -> Self {
        let root = TempDir::new("swap");
        std::fs::create_dir_all(root.path().join("install")).expect("mkdir");
        let fx = Self { root };
        std::fs::write(fx.path(FileName::Exe), exe(CURRENT)).expect("write exe");
        if let Some(p) = prev {
            std::fs::write(fx.path(FileName::Prev), exe(p)).expect("write prev");
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

    std::fs::write(fx.path(FileName::Prev), b"not an evorag").expect("write");
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
    std::fs::write(fx.path(FileName::Exe), b"someone else's file").expect("write");
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
    std::fs::write(fx.path(FileName::Rejected), b"user file").expect("write");
    std::fs::write(fx.path(FileName::New), b"half written").expect("write");
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
    std::fs::write(fx.path(FileName::Tmp), exe(OLD_PREV)).expect("write");
    for name in [FileName::New, FileName::Rejected, FileName::Probe] {
        std::fs::write(fx.path(name), b"x").expect("write");
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
    let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
    assert!(matches!(
        inst.check_install_dir(),
        Err(SwapError::NotRegularFile {
            name: FileName::Exe
        })
    ));
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
        let inst = fx.open(Method::HOST, Purpose::Update).expect("open");
        assert!(matches!(
            inst.check_install_dir(),
            Err(SwapError::NotRegularFile { .. })
        ));
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

    /// 新しいファイルの権限は、今の evorag から group と other の書き込みを除いたもの。名前に置いた
    /// シンボリックリンクの先には書かない。
    #[test]
    fn new_files_drop_group_and_other_write_and_do_not_follow_symlinks() {
        let fx = Fixture::new(None);
        std::fs::set_permissions(
            fx.path(FileName::Exe),
            std::fs::Permissions::from_mode(0o775),
        )
        .expect("chmod");
        let outside = fx.root.path().join("outside");
        std::fs::write(&outside, b"keep").expect("write");
        std::os::unix::fs::symlink(&outside, fx.path(FileName::New)).expect("symlink");
        std::os::unix::fs::symlink(&outside, fx.path(FileName::Tmp)).expect("symlink");
        run(&fx, Method::HOST, Scenario::ApplyFresh, None).expect("apply");
        assert_eq!(std::fs::read(&outside).expect("read"), b"keep");
        let mode = |n| {
            std::fs::metadata(fx.path(n))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(FileName::Exe), 0o755);
        assert_eq!(mode(FileName::Prev), 0o755);
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
        std::fs::write(fx.path(FileName::New), b"x").expect("write");
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
}
