//! 応答の経路のリランクと縮退 (05 §3.3〜§3.6、FR-402・FR-416)。llama-server は記録の再生で置き換える。
//! 再生のリランカーは、問いと文書で共通の文字の数 − 10 を値にする (決定的)。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::{ReplayClient, toy_rerank};
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{
    KIND_TURN, METHOD_BUNDLED, ModelProbeBody, RecallDecision, TARGET_CONVERSATION, ThresholdBody,
    TurnBody,
};
use evorag_retrieval::calibrate::exchange_text;
use evorag_store::Read;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, TurnView};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const PLANT: &str = "モンステラの水やりは、どれくらいの頻度がいい？";
/// 問いと「頻度」だけが共通 (語彙の候補には入るが、リランクの値は低い)。
const CAR: &str = "車のオイル交換の頻度は、どのくらいがいい？";
const ASK: &str = "前に聞いたモンステラの水やりの頻度、もう一度教えて";

fn chat(replies: usize) -> ReplayClient {
    let one = format!(r#"{{"events": [{{"delta": "{REPLY}"}}, {{"done": {{}}}}]}}"#);
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![one; replies].join(", ")
    ))
    .unwrap()
}

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

fn config(root: &Path, rerank: Option<Arc<ReplayClient>>) -> CoreConfig {
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
        rerank: rerank.map(|r| r as Arc<dyn LlmClient>),
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
    }
}

/// 起動の前に、データ根にしきい値と点検の記録を書いておく。
fn prepare(root: &Path, threshold: Option<(bool, f64, f64)>, rerank6_ms: Option<u32>) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_640_000_000)));
    let m = DataRootModels::open(
        root,
        "0.0.6",
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, 3)),
    )
    .unwrap();
    if let Some((passed, thr, band)) = threshold {
        m.save_threshold(&ThresholdBody {
            reranker_key: rr_key(),
            target: TARGET_CONVERSATION.into(),
            version: 1,
            method: METHOD_BUNDLED.into(),
            cases: evorag_retrieval::calibrate::cases_id("ja"),
            passed,
            threshold: Some(thr),
            band_low: Some(band),
            stats: Default::default(),
        })
        .unwrap();
    }
    if let Some(ms) = rerank6_ms {
        m.save_probe(&ModelProbeBody {
            role: "rerank".into(),
            model_key: rr_key(),
            build_info: "b1-test".into(),
            rerank6_ms: Some(ms),
            ..ModelProbeBody::default()
        })
        .unwrap();
    }
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

/// 往復の値 (再生のリランカーと同じ式)。
fn score(user: &str) -> f64 {
    f64::from(toy_rerank(ASK, &exchange_text("ja", user, REPLY)))
}

struct Run {
    plant: Id,
    car: Id,
    answer: TurnBody,
    /// 最後の要求の今の発話 (引用の枠はここに入る)。
    current: String,
}

/// 2 つの以前のセッションを作り、新しいセッションで ASK を送る。
async fn run(dir: &TempDir, rerank: Option<Arc<ReplayClient>>) -> Run {
    let llm = chat(3);
    let (core, mut rx, _) = Core::start(config(dir.path(), rerank), Arc::new(llm.clone()))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    let car = say(&core, &mut rx, CAR).await.reply_to.unwrap();
    session(&core, &mut rx, "今日").await;
    let answer = say(&core, &mut rx, ASK).await;
    core.shutdown().await;
    let current = llm.requests()[2].messages.last().unwrap().content.clone();
    Run {
        plant,
        car,
        answer: turn_body(dir.path(), answer.id),
        current,
    }
}

/// verifies: AC-FR-402-1 (v0.2: しきい値以上の最上位の 1 往復だけを添える), AC-FR-416-1, AC-FR-412-1 (帯の選び方)
#[tokio::test(flavor = "multi_thread")]
async fn the_top_exchange_above_the_threshold_is_attached_and_the_next_goes_to_the_band() {
    let (sp, sc) = (score(PLANT), score(CAR));
    assert!(sp > sc + 1.0, "{sp} {sc}");
    let dir = TempDir::new("rerank-fire");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let r = run(&dir, Some(reranker())).await;
    let rec = r.answer.recall.clone().unwrap();
    assert_eq!(rec.decision, RecallDecision::Fire);
    assert_eq!(rec.sources, [2]);
    assert!(r.answer.attached.contains(&r.plant));
    assert!(r.current.contains("モンステラ") && !r.current.contains("オイル"));
    let obs = r.answer.obs.unwrap();
    let ids: Vec<Id> = obs.reranked.iter().map(|x| x.id).collect();
    assert_eq!(ids, [r.plant, r.car]);
    assert!((f64::from(obs.reranked[0].score) - sp).abs() < 1e-6);
    assert_eq!(obs.reranker.as_deref(), Some(rr_key().as_str()));
    assert_eq!(obs.threshold_version, Some(1));
    assert_eq!(obs.band, [r.car]);
    assert_eq!(obs.degraded, ["no_embed"]);
    assert_eq!(obs.recall_position.as_deref(), Some("before_utterance"));
    // 出典の番号の始まり・履歴の始まり・選んでいた既定 (M2.1 から)。
    assert_eq!(obs.source_base, Some(1));
    assert!(obs.history_from.is_some());
    assert_eq!(obs.answer.get("language").map(String::as_str), Some("auto"));
}

/// verifies: AC-FR-402-4 (添える往復が無ければ引用を付けず Abstain), AC-FR-412-1 (しきい値の少し下は帯)
#[tokio::test(flavor = "multi_thread")]
async fn below_the_threshold_nothing_is_attached_and_the_band_is_recorded() {
    let (sp, sc) = (score(PLANT), score(CAR));
    let dir = TempDir::new("rerank-abstain");
    prepare(dir.path(), Some((true, sp + 1.0, sc - 1.0)), None);
    let r = run(&dir, Some(reranker())).await;
    assert_eq!(r.answer.recall.unwrap().decision, RecallDecision::Abstain);
    assert!(r.answer.attached.is_empty());
    assert!(!r.current.contains("<<引用-"));
    let obs = r.answer.obs.unwrap();
    assert_eq!(obs.band, [r.plant, r.car]);
    assert_eq!(obs.recall_position, None);
}

/// verifies: AC-FR-413-1 (しきい値が無い・較正に落ちたリランカーでは添えず、上位を帯にだけ出す)
#[tokio::test(flavor = "multi_thread")]
async fn without_a_passed_threshold_the_top_exchanges_only_go_to_the_band() {
    for (name, threshold) in [
        ("rerank-none", None),
        ("rerank-failed", Some((false, -100.0, -200.0))),
    ] {
        let dir = TempDir::new(name);
        prepare(dir.path(), threshold, None);
        let r = run(&dir, Some(reranker())).await;
        assert_eq!(
            r.answer.recall.unwrap().decision,
            RecallDecision::Abstain,
            "{name}"
        );
        let obs = r.answer.obs.unwrap();
        assert_eq!(obs.band, [r.plant, r.car], "{name}");
        assert_eq!(obs.degraded, ["no_embed", "no_threshold"], "{name}");
        assert_eq!(obs.threshold_version, None);
    }
}

/// verifies: AC-FR-402-1 (縮退: リランカーが無い・遅いときは v0.1 の語彙の規則)
#[tokio::test(flavor = "multi_thread")]
async fn without_a_usable_reranker_the_lexical_rule_is_used() {
    let dir = TempDir::new("rerank-missing");
    let r = run(&dir, None).await;
    assert_eq!(r.answer.recall.unwrap().decision, RecallDecision::Fire);
    let obs = r.answer.obs.unwrap();
    assert_eq!(obs.degraded, ["no_embed", "no_rerank"]);
    assert!(obs.reranked.is_empty() && obs.band.is_empty());

    // 点検で往復 6 件に 5 秒かかったリランカーは、3 往復でも締切に収まらないので使わない。
    let dir = TempDir::new("rerank-slow");
    prepare(dir.path(), Some((true, -100.0, -200.0)), Some(5000));
    let rr = reranker();
    let r = run(&dir, Some(rr.clone())).await;
    let obs = r.answer.obs.unwrap();
    assert_eq!(obs.degraded, ["no_embed", "rerank_slow"]);
    assert_eq!(rr.aux_calls().1, 0);
}
