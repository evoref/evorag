//! 障害の注入の試験: 書き込みの任意の時点での強制終了 (09 §4、NFR-301)。
//!
//! この試験のプログラムは、自分自身を子のプロセスとして起動する。子は会話を書き続け、発話と応答を
//! 保存するたびに (保存が書き切られた後に) 標準出力に知らせる。親は、種を固定した乱数で決めた時点で
//! 子を強制終了し (Windows は TerminateProcess、Unix は SIGKILL)、データ根を開き直して次を確かめる。
//!
//! - 子が保存したと知らせた発話と応答が、すべて残っている (確定したターンを失わない)。
//! - 壊れた記録・読めない記録が無い (書きかけが無い)。
//! - 応答の対象の発話がある。
//!
//! 回数は `EVORAG_FAULT_RUNS` (既定 100。v0.1 のリリースゲートは 1,000、09 §8.2)。
//! 強制終了の時点は、起動 (退避・ジャーナルの取り込みを含む)・発話の保存・応答の保存の最中に散らばる。
//!
//! verifies: AC-NFR-301-1

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evorag_core::{Core, CoreConfig, Probe, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store};
use evorag_types::api::{Command, Event, TurnStatus};
use evorag_types::{SystemClock, UuidV7Gen};

const CHILD_ENV: &str = "EVORAG_FAULT_CHILD";

fn cassette() -> String {
    let chat = r#"{"events": [{"delta": "は"}, {"wait_ms": 1}, {"delta": "い"}, {"done": {}}]}"#;
    format!(
        r#"{{"props": {{"build_info": "b1-test", "model_path": "/m/t.gguf", "total_slots": 1,
             "default_generation_settings": {{"n_ctx": 4096}}}},
           "models": {{"data": [{{"id": "t"}}]}},
           "chats": [{}]}}"#,
        vec![chat; 2000].join(",")
    )
}

/// 子のプロセスの中身。`EVORAG_FAULT_CHILD` が無ければ何もしない。
#[test]
fn fault_child() {
    let Some(root) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(1);
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let clock = Arc::new(SystemClock);
        let cfg = CoreConfig {
            data_root: root.into(),
            config_file: None,
            evorag_version: "0.0.5".into(),
            prompt_language: PromptLanguage::Ja,
            generation: Generation::default(),
            request_extra: Default::default(),
            reserve_for_output: None,
            backup_dir: None,
            allow_unsafe_location: true,
            probe: Probe::system(),
            model_name: "test".into(),
            models: Vec::new(),
            connect: None,
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
        };
        let llm = Arc::new(ReplayClient::from_json(&cassette()).unwrap());
        let (core, mut rx, _) = Core::start(cfg, llm).await.unwrap();
        let out = |s: String| {
            use std::io::Write;
            let mut o = std::io::stdout().lock();
            // libtest が `test fault_child ... ` を改行なしで書くので、行の頭に改行を足す。
            let _ = writeln!(o, "\n{s}");
            let _ = o.flush();
        };
        // 前の回で作ったセッションがあれば、それを続ける。
        match core.sessions(1).first() {
            Some(s) => core.command(Command::OpenSession { id: s.id }),
            None => core.command(Command::NewSession {
                title: "fault".into(),
            }),
        }
        let mut n = 0u32;
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::SessionOpened { .. } => {
                    out("READY".into());
                    core.command(Command::Send {
                        text: format!("u{}-{n}", std::process::id()),
                    });
                }
                Event::UserTurnSaved { turn } => out(format!("SAVED {}", turn.text)),
                Event::ResponseFinished { turn, .. } => {
                    if turn.status == TurnStatus::Completed {
                        out(format!("ANSWERED {}", turn.reply_to.unwrap()));
                    }
                    n += 1;
                    core.command(Command::Send {
                        text: format!("u{}-{n}", std::process::id()),
                    });
                }
                _ => {}
            }
        }
    });
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 子を起動し、`kill_after` (READY から) で強制終了する。子が知らせた行を返す。
fn run_and_kill(root: &Path, kill_after: Duration) -> Vec<String> {
    let exe = std::env::current_exe().unwrap();
    let mut child = KillOnDrop(
        Process::new(exe)
            .args([
                "fault_child",
                "--exact",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(CHILD_ENV, root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reader = {
        let lines = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let line = line.trim().to_owned();
                if line == "READY" {
                    let _ = ready_tx.send(());
                }
                if !line.is_empty() {
                    lines.lock().unwrap().push(line);
                }
            }
        })
    };
    // 起動の最中に殺す回もある (READY を待たずに時間で殺す)。
    let started = Instant::now();
    let _ = ready_rx.recv_timeout(kill_after);
    let rest = kill_after.saturating_sub(started.elapsed());
    std::thread::sleep(rest.min(kill_after));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    reader.join().unwrap();
    lines.lock().unwrap().clone()
}

fn reopen(root: &Path) -> Store {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let clock = Arc::new(SystemClock);
        match Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.5".into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
            config_file: None,
        }) {
            Ok((s, _)) => return s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("could not reopen after the kill: {e}"),
        }
    }
}

/// 種を固定した擬似乱数 (同じ回数なら同じ時点で殺す)。
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

#[test]
fn committed_turns_survive_kills_at_random_points() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let runs: u32 = std::env::var("EVORAG_FAULT_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    // データ根は回をまたいで使い続ける (前の回の強制終了の後始末も試す)。
    let dir = TempDir::new("fault");
    let mut seed = 0x6576_6f72_6167_u64;
    let (mut saved_total, mut answered_total) = (0usize, 0usize);
    for run in 0..runs {
        let kill_after = Duration::from_millis(next(&mut seed) % 400);
        let lines = run_and_kill(dir.path(), kill_after);
        let saved: BTreeSet<String> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("SAVED ").map(str::to_owned))
            .collect();
        let answered: BTreeSet<String> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("ANSWERED ").map(str::to_owned))
            .collect();
        saved_total += saved.len();
        answered_total += answered.len();

        let store = reopen(dir.path());
        let census = store.census().unwrap();
        for (kind, c) in &census.known {
            assert_eq!(
                c.corrupt + c.unreadable,
                0,
                "run {run}: half-written records of {kind}"
            );
        }
        let rtx = store.read().unwrap();
        let sessions = evorag_memory::list_sessions(&rtx, 10).unwrap();
        let mut texts = BTreeSet::new();
        let mut users = BTreeSet::new();
        let mut replied = BTreeSet::new();
        for s in &sessions {
            for t in evorag_memory::session_turns(&rtx, s.envelope.id).unwrap() {
                match t.body.role {
                    evorag_memory::TurnRole::User => {
                        texts.insert(t.body.text.clone());
                        users.insert(t.envelope.id);
                    }
                    _ => {
                        let to = t.body.reply_to.expect("a reply has its utterance");
                        replied.insert(to.to_string());
                        assert!(
                            users.contains(&to),
                            "run {run}: a reply without its utterance"
                        );
                    }
                }
            }
        }
        for s in &saved {
            assert!(
                texts.contains(s),
                "run {run} (kill after {kill_after:?}): lost utterance {s}"
            );
        }
        for a in &answered {
            assert!(replied.contains(a), "run {run}: lost reply to {a}");
        }
    }
    // 殺す時点が、保存の前後に散らばっていること (試験そのものの確かめ)。
    assert!(
        saved_total > 0 && answered_total > 0,
        "the kills never hit a write"
    );
}
