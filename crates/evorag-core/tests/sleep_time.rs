//! 裏の処理 (sleep-time、04 §4、FR-301・FR-504)。llama-server は記録の再生で置き換える。
//! 手順には、リランカーの較正 (05 §3.7) を使う (60 問いのリランク。1 回ごとに少し待たせる)。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use evorag_core::power::Power;
use evorag_core::{Core, CoreConfig, OnBattery, PromptLanguage, SleepConfig};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, ServerInfo};
use evorag_store::testutil::TempDir;
use evorag_types::api::{BackgroundState, Event};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

fn chat() -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(
            r#"{
              "props": {"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                        "default_generation_settings": {"n_ctx": 8192}},
              "models": {"data": [{"id": "test", "meta": {"n_params": 1, "size": 2, "n_vocab": 3}}]}
            }"#,
        )
        .unwrap(),
    )
}

/// 1 回のリランクに `delay` かかるリランカー。動いているリランクの数と、終わった数を数える。
struct Slow {
    inner: ReplayClient,
    delay: Duration,
    started: AtomicUsize,
    finished: AtomicUsize,
    in_flight: AtomicBool,
}

impl Slow {
    fn new(delay_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: ReplayClient::from_json(
                r#"{
                  "props": {"build_info": "b1-test", "model_path": "/m/rr.gguf", "total_slots": 4,
                            "default_generation_settings": {"n_ctx": 8192}},
                  "models": {"data": [{"id": "rr", "meta": {"n_params": 5, "size": 6, "n_vocab": 7}}]},
                  "rerank": true
                }"#,
            )
            .unwrap(),
            delay: Duration::from_millis(delay_ms),
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            in_flight: AtomicBool::new(false),
        })
    }
}

/// 落とされたら「動いていない」に戻す。
struct Flight<'a>(&'a AtomicBool);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl LlmClient for Slow {
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
        self.inner.chat(req)
    }
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.in_flight.store(true, Ordering::SeqCst);
            let _flight = Flight(&self.in_flight);
            tokio::time::sleep(self.delay).await;
            let r = self.inner.rerank(query, documents).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            r
        })
    }
}

fn config(root: &Path, rerank: Arc<Slow>, sleep: SleepConfig, power: fn() -> Power) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.6".into(),
        prompt_language: PromptLanguage::Ja,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe {
            power,
            ..evorag_core::Probe::system()
        },
        model_name: "test".into(),
        ignored_config: Vec::new(),
        embed: None,
        rerank: Some(rerank as Arc<dyn LlmClient>),
        aux_request_extra: Default::default(),
        probe_models: true,
        sleep,
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 5)),
    }
}

fn on_ac() -> Power {
    Power::Ac
}

fn on_battery() -> Power {
    Power::Battery
}

fn sleep(idle_ms: u64, on_battery: OnBattery) -> SleepConfig {
    SleepConfig {
        enabled: true,
        idle: Duration::from_millis(idle_ms),
        on_battery,
    }
}

async fn wait_until(pred: impl Fn() -> bool) {
    for _ in 0..500 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

/// 較正を止めずに通したときの記録。
async fn uninterrupted() -> evorag_memory::ThresholdBody {
    let dir = TempDir::new("sleep-straight");
    let rr = Slow::new(0);
    let (core, _, _) = Core::start(
        config(dir.path(), rr, SleepConfig::immediate(), on_ac),
        chat(),
    )
    .await
    .unwrap();
    core.settle_background().await;
    let t = core.threshold().expect("calibrated");
    core.shutdown().await;
    t
}

/// 利用者の操作 `cut` で、動いているリランクが 1 秒以内に落とされ、次が出ないことを確かめる。
async fn cut_within_a_second(rr: &Slow, cut: impl FnOnce()) {
    wait_until(|| rr.in_flight.load(Ordering::SeqCst)).await;
    let t = std::time::Instant::now();
    cut();
    wait_until(|| !rr.in_flight.load(Ordering::SeqCst)).await;
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    let stopped_at = rr.started.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rr.started.load(Ordering::SeqCst), stopped_at);
}

/// verifies: AC-FR-301-1, AC-NFR-105-1, AC-FR-504-1 (裏の要求は利用者の操作で 1 秒以内に止まり、続きから再開した結果は同じ)
#[tokio::test(flavor = "multi_thread")]
async fn background_work_yields_to_the_user_and_resumes_to_the_same_result() {
    let expected = uninterrupted().await;
    let dir = TempDir::new("sleep-yield");
    let rr = Slow::new(30);
    let (core, mut rx, _) = Core::start(
        config(dir.path(), rr.clone(), sleep(300, OnBattery::Normal), on_ac),
        chat(),
    )
    .await
    .unwrap();
    // 入力が途切れると始まる。キー入力で止まる。
    wait_until(|| rr.finished.load(Ordering::SeqCst) >= 5).await;
    cut_within_a_second(&rr, || core.touch()).await;
    assert!(
        core.threshold().is_none(),
        "nothing is saved from a cut run"
    );
    // 発話の送信でも止まる (コアの命令の側)。
    wait_until(|| rr.finished.load(Ordering::SeqCst) >= 20).await;
    cut_within_a_second(&rr, || {
        core.command(evorag_types::api::Command::Send { text: "x".into() })
    })
    .await;
    // また入力が途切れると、続きから求めて、止めずに通したときと同じ結果になる。
    core.settle_background().await;
    let got = core.threshold().expect("calibrated after resuming");
    assert_eq!(
        (got.threshold, got.band_low, got.passed, &got.stats),
        (
            expected.threshold,
            expected.band_low,
            expected.passed,
            &expected.stats
        )
    );
    // 済んだ問いはやり直さない (60 問いと、点検の 1 回だけが最後まで済んだ)。
    assert_eq!(rr.finished.load(Ordering::SeqCst), 61);
    core.shutdown().await;
    let mut states = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let Event::BackgroundState { state } = e {
            states.push(state);
        }
    }
    assert!(states.contains(&BackgroundState::Working), "{states:?}");
}

/// 起動して `wait` の後に、リランクが出たか。
async fn ran(
    name: &str,
    s: SleepConfig,
    power: fn() -> Power,
    wait_ms: u64,
) -> (bool, Vec<BackgroundState>) {
    let dir = TempDir::new(name);
    let rr = Slow::new(0);
    let (core, mut rx, _) = Core::start(config(dir.path(), rr.clone(), s, power), chat())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    let started = rr.started.load(Ordering::SeqCst) > 0;
    core.shutdown().await;
    let mut states = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let Event::BackgroundState { state } = e {
            states.push(state);
        }
    }
    (started, states)
}

/// verifies: AC-FR-301-2 (電池で動いている間は `memory.on_battery` のとおりに。起動後の点検は `stop` でだけ止まる)
#[tokio::test(flavor = "multi_thread")]
async fn on_battery_the_background_work_is_reduced_or_stopped() {
    // stop: 動かない。状態は「電池のため休止」。
    let (started, states) = ran(
        "sleep-battery-stop",
        sleep(0, OnBattery::Stop),
        on_battery,
        500,
    )
    .await;
    assert!(!started);
    assert!(
        states.contains(&BackgroundState::PausedBattery),
        "{states:?}"
    );
    // normal と reduce: 起動後の点検 (較正) は電池でも流れる。reduce が待ち時間を延ばすのは
    // sleep-time の手順 (埋め込みなど、M2.5 から)。
    for (name, on) in [
        ("sleep-battery-normal", OnBattery::Normal),
        ("sleep-battery-reduce", OnBattery::Reduce),
    ] {
        let (started, _) = ran(name, sleep(0, on), on_battery, 500).await;
        assert!(started, "{name}");
    }
    // 設定で裏の処理を切っても、起動後の点検は流れる (状態は「切っている」)。
    let off = SleepConfig {
        enabled: false,
        ..SleepConfig::immediate()
    };
    let (started, states) = ran("sleep-off", off, on_ac, 500).await;
    assert!(started);
    assert!(states.contains(&BackgroundState::Off), "{states:?}");
}
