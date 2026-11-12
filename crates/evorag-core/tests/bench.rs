//! ベンチマーク (09 §5.1): BM-101 (起動)、BM-102 (発話から要求まで)、BM-201 (メモリ)。
//!
//! 会話 1 万ターンのデータ根で測る。時間がかかるので `#[ignore]` にし、`cargo xtask bench`
//! (release で、基準機で流す) で動かす。`EVORAG_BENCH_OUT` があれば、結果を
//! `<そこ>/BM-<ID>.json` に書く (docs/perf/<版>/、09 §5.1)。
//!
//! verifies: AC-NFR-101-1, AC-NFR-102-1, AC-NFR-201-1

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, ServerInfo};
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store, WriteRequest, Writer};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, SystemClock, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const TURNS: usize = 10_000;
const PER_SESSION: usize = 50;

/// 作った会話の話題 (ターンの本文の種)。
const TOPICS: &[&str] = &[
    "京都の紅葉と嵐山の朝の散歩",
    "カレーの玉葱を飴色に炒める時間",
    "観葉植物の水やりの頻度と葉の色",
    "ノートパソコンのメモリと SSD の容量",
    "確定申告の医療費控除の計算",
    "ランニングの膝の痛みとシューズ",
    "子猫のワクチンと爪とぎ",
    "Python の venv と辞書の使い方",
    "冷蔵庫の節電と霜取り",
    "引っ越しの初期費用と転入届",
];

fn text(i: usize) -> String {
    let t = TOPICS[i % TOPICS.len()];
    format!(
        "{t}について、{}回目の話です。前に話した内容を踏まえて、もう少し詳しく教えてください。条件は {} と {} です。",
        i / TOPICS.len(),
        i % 97,
        i % 13
    )
}

/// 会話 1 万ターン (200 セッション × 50) のデータ根を作る。
fn build_root(root: &Path) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock.clone())),
        config_file: None,
    })
    .unwrap();
    let store = Arc::new(store);
    let (w, h) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || w.run());
    let writes = evorag_memory::Writes {
        clock: &*clock,
        ids: &**store.ids(),
    };
    let mut n = 0;
    for s in 0..TURNS / PER_SESSION {
        let (sid, ops) = writes.create_session(&format!("会話 {s}")).unwrap();
        h.submit_wait(WriteRequest::conversation(ops)).unwrap();
        let mut ops = Vec::new();
        for _ in 0..PER_SESSION / 2 {
            let session = store
                .read()
                .unwrap()
                .get(evorag_memory::KIND_SESSION, sid)
                .unwrap();
            let Some(evorag_store::Read::Ok(session)) = session else {
                panic!()
            };
            let (uid, mut u) = writes.user_turn(&session, &text(n)).unwrap();
            // 1 つの依頼にまとめる (応答は発話の封筒から作る)。
            let user_env = match &u[0] {
                evorag_store::WriteOp::Put(e) => e.clone(),
                _ => panic!(),
            };
            let (_, mut a) = writes
                .assistant_turn(
                    &session,
                    &user_env,
                    evorag_memory::AssistantTurn {
                        state: evorag_memory::TurnState::Completed,
                        text: text(n + 1),
                        model_key: Some("bench".into()),
                        generation: None,
                        attached: Vec::new(),
                        error: None,
                        recall: None,
                        obs: None,
                    },
                )
                .unwrap();
            let _ = uid;
            ops.append(&mut u);
            ops.append(&mut a);
            n += 2;
            h.submit_wait(WriteRequest::conversation(std::mem::take(&mut ops)))
                .unwrap();
        }
    }
    h.shutdown();
    t.join().unwrap();
}

/// `chat` を呼ばれた時刻を残す llama-server の代わり。
#[derive(Clone)]
struct Timed {
    inner: ReplayClient,
    calls: Arc<Mutex<Vec<Instant>>>,
}

impl LlmClient for Timed {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        self.inner.health()
    }
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        self.inner.server_info()
    }
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        self.inner.count_tokens(text)
    }
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        self.calls.lock().unwrap().push(Instant::now());
        self.inner.chat(req)
    }
}

fn llm(replies: usize) -> Timed {
    let chat = r#"{"events": [{"delta": "はい"}, {"done": {}}]}"#;
    let inner = ReplayClient::from_json(&format!(
        r#"{{"props": {{"build_info": "b1-bench", "model_path": "/m/b.gguf", "total_slots": 1,
             "default_generation_settings": {{"n_ctx": 8192}}}},
           "models": {{"data": [{{"id": "b"}}]}},
           "chats": [{}]}}"#,
        vec![chat; replies.max(1)].join(",")
    ))
    .unwrap();
    Timed {
        inner,
        calls: Arc::default(),
    }
}

fn config(root: &Path) -> CoreConfig {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        prompt_language: PromptLanguage::Ja,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "bench".into(),
        ignored_config: Vec::new(),
        embed: None,
        rerank: None,
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
    }
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out")
            .expect("closed");
        if pred(&ev) {
            return ev;
        }
    }
}

/// 結果を書く (`EVORAG_BENCH_OUT` があるとき)。
fn record(id: &str, value: serde_json::Value) {
    println!("{id}: {value}");
    if let Some(dir) = std::env::var_os("EVORAG_BENCH_OUT") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut v = value;
        v["id"] = id.into();
        v["turns"] = TURNS.into();
        v["evorag"] = env!("CARGO_PKG_VERSION").into();
        v["target"] = std::env::consts::ARCH.into();
        v["os"] = std::env::consts::OS.into();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&v).unwrap() + "\n",
        )
        .unwrap();
    }
}

/// データ根を作り、1 回起動して索引を作っておく (2 回目からが普段の起動)。
async fn prepared(dir: &Path) {
    build_root(dir);
    let l = llm(1);
    let (core, _rx, _) = Core::start(config(dir), Arc::new(l)).await.unwrap();
    core.settle().await;
    core.shutdown().await;
}

/// BM-101 / NFR-101: 起動から入力できるまで (最新のセッションを開くまで)。500 ms 以内。
#[ignore = "benchmark: run with cargo xtask bench"]
#[tokio::test(flavor = "multi_thread")]
async fn bm_101_startup() {
    let dir = TempDir::new("bm101");
    prepared(dir.path()).await;
    let mut times = Vec::new();
    for _ in 0..5 {
        let l = llm(1);
        let t = Instant::now();
        let (core, mut rx, _) = Core::start(config(dir.path()), Arc::new(l)).await.unwrap();
        let latest = core.sessions(1)[0].id;
        core.command(Command::OpenSession { id: latest });
        until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        core.settle().await;
        core.shutdown().await;
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    record(
        "BM-101",
        serde_json::json!({"median_ms": median, "runs_ms": times, "target_ms": 500}),
    );
    assert!(median <= 500.0, "startup {median:.1} ms > 500 ms");
}

/// BM-102 / NFR-102: 発話を保存してから llama-server へ要求を出すまでに足す時間
/// (検索と文脈の組み立て)。平均 100 ms 以内。
#[ignore = "benchmark: run with cargo xtask bench"]
#[tokio::test(flavor = "multi_thread")]
async fn bm_102_send_to_request() {
    let dir = TempDir::new("bm102");
    prepared(dir.path()).await;
    const SENDS: usize = 30;
    let l = llm(SENDS);
    let (core, mut rx, _) = Core::start(config(dir.path()), Arc::new(l.clone()))
        .await
        .unwrap();
    core.settle().await;
    core.command(Command::NewSession {
        title: "測る".into(),
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    let mut added = Vec::new();
    for i in 0..SENDS {
        core.command(Command::Send {
            text: format!(
                "前に話した{}のこと、もう一度教えて",
                TOPICS[i % TOPICS.len()]
            ),
        });
        until(&mut rx, |e| matches!(e, Event::UserTurnSaved { .. })).await;
        let saved = Instant::now();
        until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
        let called = *l.calls.lock().unwrap().last().unwrap();
        added.push(called.saturating_duration_since(saved).as_secs_f64() * 1000.0);
        core.settle().await;
    }
    core.shutdown().await;
    let mean = added.iter().sum::<f64>() / added.len() as f64;
    let mut sorted = added.clone();
    sorted.sort_by(f64::total_cmp);
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    record(
        "BM-102",
        serde_json::json!({"mean_ms": mean, "p95_ms": p95, "sends": SENDS, "target_mean_ms": 100}),
    );
    assert!(mean <= 100.0, "send to request {mean:.1} ms > 100 ms");
}

/// このプロセスの最大の常駐の大きさ (バイト)。
fn peak_rss() -> u64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            quota: [usize; 4],
            pagefile: usize,
            peak_pagefile: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn K32GetProcessMemoryInfo(p: *mut std::ffi::c_void, c: *mut Counters, cb: u32) -> i32;
        }
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            ..Default::default()
        };
        // SAFETY: 今のプロセスの擬似ハンドルと、大きさを入れた有効な構造体を渡す。
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        assert_ne!(ok, 0);
        c.peak_working_set as u64
    }
    #[cfg(unix)]
    {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    }
}

/// BM-201 / NFR-201: 会話 1 万ターンで、起動・セッションを開く・送るの後のメモリの最大。300 MB 以下。
/// 最大の常駐の大きさは、同じプロセスでデータ根を作った分も含むので、実際より多めに出る。
#[ignore = "benchmark: run with cargo xtask bench"]
#[tokio::test(flavor = "multi_thread")]
async fn bm_201_memory() {
    let dir = TempDir::new("bm201");
    prepared(dir.path()).await;
    let l = llm(5);
    let (core, mut rx, _) = Core::start(config(dir.path()), Arc::new(l)).await.unwrap();
    core.settle().await;
    for s in core.sessions(20) {
        let _ = core.turns(s.id);
    }
    let latest = core.sessions(1)[0].id;
    core.command(Command::OpenSession { id: latest });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    for i in 0..5 {
        core.command(Command::Send { text: text(i) });
        until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    }
    core.settle().await;
    let peak = peak_rss();
    core.shutdown().await;
    let mb = peak as f64 / (1024.0 * 1024.0);
    record(
        "BM-201",
        serde_json::json!({"peak_rss_mb": mb, "target_mb": 300}),
    );
    assert!(mb <= 300.0, "peak memory {mb:.0} MB > 300 MB");
}
