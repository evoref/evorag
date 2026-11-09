//! 強制終了の試験 (AC-FR-201-1)。
//!
//! この試験のプログラムは、自分自身を子のプロセスとして起動する (`EVORAG_CRASH_CHILD` にデータ根を
//! 渡す)。子は発話を送り、発話を保存したことを標準出力に書いたら、応答を待ち続ける。親は、それを
//! 読んだ直後に子を強制終了し (Windows は TerminateProcess、Unix は SIGKILL)、データ根を開き直して
//! 発話が「応答なし」(pending) で残っていることを確かめる。
//!
//! verifies: AC-FR-201-1

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store};
use evorag_types::api::{Command, Event};
use evorag_types::{Id, SystemClock, UuidV7Gen};

const CHILD_ENV: &str = "EVORAG_CRASH_CHILD";
const TEXT: &str = "強制終了の前の発話";

const CASSETTE: &str = r#"{
  "props": {"build_info": "b1-test", "model_path": "/m/t.gguf", "total_slots": 1,
            "default_generation_settings": {"n_ctx": 4096}},
  "models": {"data": [{"id": "t"}]},
  "chats": [{"events": [{"hang": true}]}]
}"#;

/// 子のプロセスの中身。`EVORAG_CRASH_CHILD` が無ければ何もしない (普通の試験としては素通り)。
#[test]
fn crash_child() {
    let Some(root) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    // 親に止められなかったときの保険。
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(1);
    });
    rt.block_on(async move {
        let clock = Arc::new(SystemClock);
        let cfg = CoreConfig {
            data_root: root.into(),
            config_file: None,
            evorag_version: "0.0.3".into(),
            prompt_language: PromptLanguage::Ja,
            generation: Generation::default(),
            reserve_for_output: None,
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
        };
        let llm = Arc::new(ReplayClient::from_json(CASSETTE).unwrap());
        let (core, mut rx, _) = Core::start(cfg, llm).await.unwrap();
        core.command(Command::NewSession {
            title: "crash".into(),
        });
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::SessionOpened { .. } => core.command(Command::Send { text: TEXT.into() }),
                Event::UserTurnSaved { turn } => {
                    println!("SAVED {}", turn.session);
                    use std::io::Write;
                    std::io::stdout().flush().unwrap();
                }
                _ => {}
            }
        }
    });
}

fn run_and_kill(root: &Path) -> Id {
    let exe = std::env::current_exe().unwrap();
    let child = Process::new(exe)
        .args([
            "crash_child",
            "--exact",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_ENV, root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // どの経路でも子を止める (残った子が出力のパイプをつかみ続けると、試験の実行器が終わらない)。
    let mut child = KillOnDrop(child);
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // libtest が `test crash_child ... ` を改行なしで先に書くので、行の途中も探す。
            if let Some((_, id)) = line.split_once("SAVED ") {
                let _ = tx.send(id.trim().to_owned());
                return;
            }
        }
    });
    let session = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the child did not save the utterance");
    // 発話の保存を知らせた直後に、強制終了する。
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    session.parse().unwrap()
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn pending_after_restart(root: &Path, session: Id) -> Vec<String> {
    // 強制終了の直後は、OS がロックを外すまで少し待つことがある。
    let deadline = Instant::now() + Duration::from_secs(10);
    let store = loop {
        let clock = Arc::new(SystemClock);
        match Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.3".into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
            config_file: None,
        }) {
            Ok((s, _)) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("could not reopen after the crash: {e}"),
        }
    };
    let rtx = store.read().unwrap();
    evorag_memory::pending_user_turns(&rtx, session)
        .unwrap()
        .into_iter()
        .map(|t| t.body.text)
        .collect()
}

#[test]
fn an_utterance_survives_a_forced_kill_before_the_reply() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    for round in 0..3 {
        let dir = TempDir::new("crash");
        let session = run_and_kill(dir.path());
        assert_eq!(
            pending_after_restart(dir.path(), session),
            [TEXT],
            "round {round}: the utterance was lost or not pending"
        );
    }
}
