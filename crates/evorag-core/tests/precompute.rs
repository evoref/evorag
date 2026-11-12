//! 先に済ませる処理 (05 §4.5、FR-415)。llama-server は記録の再生で置き換える。
//! 再生の埋め込みは文字の bi-gram から作り、再生のリランカーは共通の文字の数 − 10 を値にする。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::power::Power;
use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, OnBattery, PromptLanguage, SleepConfig};
use evorag_llm::replay::{ReplayClient, toy_rerank};
use evorag_llm::{Generation, LlmClient, Role};
use evorag_memory::{KIND_TURN, METHOD_BUNDLED, TARGET_CONVERSATION, ThresholdBody, TurnBody};
use evorag_retrieval::calibrate::exchange_text;
use evorag_store::Read;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, TurnView};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const PLANT: &str = "モンステラの水やりは、どれくらいの頻度がいい？";
const CAR: &str = "車のオイル交換の頻度は、どのくらいがいい？";
const ASK: &str = "前に聞いたモンステラの水やりの頻度、もう一度教えて";
/// ASK と違う、送らなかった入力。
const OTHER: &str = "モンステラの葉が黄色くなった";

/// 応答の llama-server (スロット 2 つ)。`warm` は、生成を 1 トークンに限った要求への答え。
fn chat(replies: usize, warm: &str) -> ReplayClient {
    let one = format!(r#"{{"events": [{{"delta": "{REPLY}"}}, {{"done": {{}}}}]}}"#);
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}],
          "warm": {warm}
        }}"#,
        vec![one; replies].join(", ")
    ))
    .unwrap()
}

/// すぐ終わる 1 トークンの要求。
const WARM_DONE: &str = r#"[{"done": {}}]"#;
/// 終わらない 1 トークンの要求 (止めたことを確かめる)。
const WARM_HANG: &str = r#"[{"hang": true}]"#;

fn reranker() -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(
            r#"{
              "props": {"build_info": "b1-test", "model_path": "/m/rr.gguf", "total_slots": 4,
                        "default_generation_settings": {"n_ctx": 8192}},
              "models": {"data": [{"id": "rr", "meta": {"n_params": 5, "size": 6, "n_vocab": 7}}]},
              "rerank": true
            }"#,
        )
        .unwrap(),
    )
}

fn rr_key() -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(5),
        size: Some(6),
        n_vocab: Some(7),
        n_ctx_train: None,
    };
    evorag_llm::model_key("/m/rr.gguf", &meta, "rr")
}

fn embedder() -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(
            r#"{
              "props": {"build_info": "b1-test", "model_path": "/m/emb.gguf", "total_slots": 4,
                        "default_generation_settings": {"n_ctx": 8192}},
              "models": {"data": [{"id": "emb", "meta": {"n_params": 5, "size": 9, "n_vocab": 7}}]},
              "embed": {"dim": 16}
            }"#,
        )
        .unwrap(),
    )
}

fn config(root: &Path) -> CoreConfig {
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
        probe: evorag_core::Probe::system(),
        model_name: "test".into(),
        ignored_config: Vec::new(),
        embed: None,
        rerank: None,
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
    }
}

/// 起動の前に、データ根にしきい値を書いておく (PLANT は添え、CAR は添えない)。
fn prepare(root: &Path) {
    let score = |user: &str| f64::from(toy_rerank(ASK, &exchange_text("ja", user, REPLY)));
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_640_000_000)));
    let m = DataRootModels::open(
        root,
        "0.0.6",
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, 3)),
    )
    .unwrap();
    m.save_threshold(&ThresholdBody {
        reranker_key: rr_key(),
        target: TARGET_CONVERSATION.into(),
        version: 1,
        method: METHOD_BUNDLED.into(),
        cases: evorag_retrieval::calibrate::cases_id("ja"),
        passed: true,
        threshold: Some(score(PLANT) - 0.5),
        band_low: Some(score(CAR) - 1.0),
        stats: Default::default(),
    })
    .unwrap();
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut got = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        let stop = pred(&ev);
        got.push(ev);
        if stop {
            return got;
        }
    }
}

async fn wait_for(what: &str, f: impl Fn() -> bool) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn session(core: &Core, rx: &mut UnboundedReceiver<Event>, title: &str) {
    core.command(Command::NewSession {
        title: title.into(),
    });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> TurnView {
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    turn.clone()
}

async fn idle(core: &Core, text: &str) {
    core.command(Command::InputIdle { text: text.into() });
    core.settle().await;
    core.settle_precompute().await;
}

fn turn_body(root: &Path, id: Id) -> TurnBody {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap();
    let rtx = store.read().unwrap();
    match rtx.get(KIND_TURN, id).unwrap() {
        Some(Read::Ok(e)) => e.body_as().unwrap(),
        other => panic!("{other:?}"),
    }
}

/// verifies: AC-FR-415-1 (応答を書いた後に、生成を 1 トークンに限った要求が応答のスロットへ出て、
/// 利用者が発話を送ると止まる。次のターンの要求は、その要求と同じ先頭で始まる)
#[tokio::test(flavor = "multi_thread")]
async fn the_next_turn_is_processed_ahead_and_stopped_by_a_send() {
    let dir = TempDir::new("pre-warm");
    let llm = chat(2, WARM_HANG);
    let (core, mut rx, _) = Core::start(config(dir.path()), Arc::new(llm.clone()))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "s").await;
    say(&core, &mut rx, "こんにちは").await;
    wait_for("the warm-up request", || llm.warm_requests().len() == 1).await;
    let warm = &llm.warm_requests()[0];
    assert_eq!(warm.generation.max_tokens, Some(1));
    assert_eq!(warm.id_slot, Some(0), "the response slot");
    assert_eq!(llm.requests()[0].id_slot, Some(0));
    let roles: Vec<Role> = warm.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::System, Role::User, Role::Assistant, Role::User]
    );
    assert_eq!(warm.messages[2].content, REPLY);
    assert_eq!(warm.messages[3].content, "");
    assert_eq!(llm.warm_dropped(), 0);

    // 発話を送ると止まる。次のターンの要求は、空の発話の前までが同じ。
    say(&core, &mut rx, "次の質問").await;
    assert_eq!(llm.warm_dropped(), 1);
    let next = &llm.requests()[1];
    assert_eq!(next.messages[..3], warm.messages[..3]);
    assert!(next.messages[3].content.ends_with("次の質問"));
    core.shutdown().await;
}

/// verifies: AC-FR-415-1 (入力が止まると問いの埋め込みとリランクが先に済み、同じ発話を送るとその結果が
/// 使われる。違う入力の結果は使われない)
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_input_is_reranked_ahead_and_reused_when_sent() {
    let dir = TempDir::new("pre-idle");
    prepare(dir.path());
    let rr = reranker();
    let emb = embedder();
    let mut cfg = config(dir.path());
    cfg.rerank = Some(rr.clone() as Arc<dyn LlmClient>);
    cfg.embed = Some(emb.clone() as Arc<dyn LlmClient>);
    let llm = chat(3, WARM_DONE);
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    say(&core, &mut rx, CAR).await;
    core.settle_background().await;
    session(&core, &mut rx, "今日").await;
    // CAR を送ったときの想起でも呼んでいるので、ここからの回数を見る。
    let reranks = || rr.aux_calls().1;
    let before = reranks();

    // 送らなかった入力の分も、先にリランクする。
    idle(&core, OTHER).await;
    let after_other = reranks();
    assert_eq!(after_other, before + 1);
    // 送る入力の分。
    idle(&core, ASK).await;
    assert_eq!(reranks(), after_other + 1);
    // 同じ発話を送ると、先に済ませた結果を使う (リランカーを呼ばない)。
    let answer = say(&core, &mut rx, ASK).await;
    assert_eq!(reranks(), after_other + 1);
    core.shutdown().await;
    let body = turn_body(dir.path(), answer.id);
    assert!(body.attached.contains(&plant));
    let obs = body.obs.unwrap();
    assert!(obs.degraded.is_empty(), "{:?}", obs.degraded);
    assert!(!obs.reranked.is_empty());
}

/// 入力が止まったときの処理を切った設定・電池 (`reduce`) では出さない。電池の `stop` では次のターンの分も
/// 出さない。
/// verifies: AC-FR-415-2 (入力が止まったときの処理を切った設定では、その要求が出ない)
#[tokio::test(flavor = "multi_thread")]
async fn idle_precompute_respects_the_setting_and_the_battery() {
    for (case, precompute, battery, warm) in [
        ("off", false, None, true),
        ("reduce", true, Some(OnBattery::Reduce), true),
        ("stop", true, Some(OnBattery::Stop), false),
    ] {
        let dir = TempDir::new(&format!("pre-{case}"));
        prepare(dir.path());
        let rr = reranker();
        let mut cfg = config(dir.path());
        cfg.rerank = Some(rr.clone() as Arc<dyn LlmClient>);
        cfg.precompute = precompute;
        if let Some(b) = battery {
            cfg.probe.power = || Power::Battery;
            cfg.sleep = SleepConfig {
                on_battery: b,
                ..SleepConfig::immediate()
            };
        }
        let llm = chat(1, WARM_DONE);
        let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
        core.settle().await;
        session(&core, &mut rx, "観葉植物").await;
        say(&core, &mut rx, PLANT).await;
        core.settle_precompute().await;
        assert_eq!(llm.warm_requests().len(), usize::from(warm), "{case}");
        session(&core, &mut rx, "今日").await;
        idle(&core, ASK).await;
        assert_eq!(rr.aux_calls().1, 0, "{case}");
        core.shutdown().await;
    }
}
