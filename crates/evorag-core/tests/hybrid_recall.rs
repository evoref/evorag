//! 語彙とベクトルの候補を合わせる想起 (05 §3.2〜§3.5、FR-402)。llama-server は記録の再生で置き換える。
//! 再生の埋め込みは文字の bi-gram から作り、再生のリランカーは共通の文字の数 − 10 を値にする。

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

fn embedder() -> ReplayClient {
    ReplayClient::from_json(
        r#"{
          "props": {"build_info": "b1-test", "model_path": "/m/emb.gguf", "total_slots": 4,
                    "default_generation_settings": {"n_ctx": 8192}},
          "models": {"data": [{"id": "emb", "meta": {"n_params": 5, "size": 9, "n_vocab": 7}}]},
          "embed": {"dim": 16}
        }"#,
    )
    .unwrap()
}

/// 1 件だけの埋め込み (問いの埋め込み) を `delay` 遅らせる。
struct SlowQuery(ReplayClient, Duration);

impl LlmClient for SlowQuery {
    fn health(&self) -> evorag_llm::BoxFuture<'_, Result<(), evorag_llm::LlmError>> {
        self.0.health()
    }
    fn server_info(
        &self,
    ) -> evorag_llm::BoxFuture<'_, Result<evorag_llm::ServerInfo, evorag_llm::LlmError>> {
        self.0.server_info()
    }
    fn count_tokens<'a>(
        &'a self,
        text: &'a str,
    ) -> evorag_llm::BoxFuture<'a, Result<usize, evorag_llm::LlmError>> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        req: evorag_llm::ChatRequest,
    ) -> evorag_llm::BoxFuture<'_, Result<Box<dyn evorag_llm::ChatStream>, evorag_llm::LlmError>>
    {
        self.0.chat(req)
    }
    fn embed<'a>(
        &'a self,
        inputs: &'a [String],
    ) -> evorag_llm::BoxFuture<'a, Result<Vec<Vec<f32>>, evorag_llm::LlmError>> {
        Box::pin(async move {
            if inputs.len() == 1 && inputs[0].contains("もう一度") {
                tokio::time::sleep(self.1).await;
            }
            self.0.embed(inputs).await
        })
    }
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
        precompute: true,
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
}

/// 2 つの以前のセッションを作り、埋め込みが済むのを待ってから、新しいセッションで ASK を送る。
async fn run(dir: &TempDir, embed: Arc<dyn LlmClient>) -> Run {
    let llm = chat(3);
    let mut cfg = config(dir.path(), Some(reranker()));
    cfg.embed = Some(embed);
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    let car = say(&core, &mut rx, CAR).await.reply_to.unwrap();
    core.settle_background().await;
    session(&core, &mut rx, "今日").await;
    let answer = say(&core, &mut rx, ASK).await;
    core.shutdown().await;
    Run {
        plant,
        car,
        answer: turn_body(dir.path(), answer.id),
    }
}

/// verifies: AC-FR-402-1 (v0.2: 語彙とベクトルの候補を合わせてリランクする。埋め込みがあれば no_embed にならない)
#[tokio::test(flavor = "multi_thread")]
async fn vector_candidates_join_the_lexical_ones() {
    let (sp, sc) = (score(PLANT), score(CAR));
    let dir = TempDir::new("hybrid");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let r = run(&dir, Arc::new(embedder())).await;
    assert_eq!(r.answer.recall.unwrap().decision, RecallDecision::Fire);
    assert!(r.answer.attached.contains(&r.plant));
    let obs = r.answer.obs.unwrap();
    assert!(obs.degraded.is_empty(), "{:?}", obs.degraded);
    let ids: Vec<Id> = obs.reranked.iter().map(|x| x.id).collect();
    assert!(ids.contains(&r.plant) && ids.contains(&r.car), "{ids:?}");
}

/// verifies: AC-FR-402-1 (問いの埋め込みが締切に間に合わなければ、その要求だけ語彙の候補で答える)
#[tokio::test(flavor = "multi_thread")]
async fn a_late_query_embedding_falls_back_to_lexical_candidates() {
    let (sp, sc) = (score(PLANT), score(CAR));
    let dir = TempDir::new("hybrid-late");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let slow = SlowQuery(embedder(), Duration::from_millis(600));
    let r = run(&dir, Arc::new(slow)).await;
    assert_eq!(r.answer.recall.unwrap().decision, RecallDecision::Fire);
    let obs = r.answer.obs.unwrap();
    assert_eq!(obs.degraded, ["embed_deadline"]);
}

/// `/memory` で、直前の応答に添えた往復と出典の番号、帯 (または下)、リランクの値としきい値との差、
/// 縮退の理由が出る。
/// verifies: AC-FR-310-1
#[tokio::test(flavor = "multi_thread")]
async fn memory_shows_what_the_last_answer_used() {
    use evorag_types::api::MemoryPlace;
    let (sp, sc) = (score(PLANT), score(CAR));
    let dir = TempDir::new("hybrid-memory");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let llm = chat(3);
    // 埋め込みは無い (縮退の理由 no_embed が出る)。
    let (core, mut rx, _) = Core::start(config(dir.path(), Some(reranker())), Arc::new(llm))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    let car = say(&core, &mut rx, CAR).await.reply_to.unwrap();
    session(&core, &mut rx, "今日").await;
    let answer = say(&core, &mut rx, ASK).await;
    core.command(Command::Memory);
    let ev = until(&mut rx, |e| matches!(e, Event::MemoryInfo { .. })).await;
    core.shutdown().await;
    let Some(Event::MemoryInfo { report }) = ev.last() else {
        panic!()
    };
    assert_eq!(report.answer, Some(answer.id));
    let p = report.items.iter().find(|i| i.target == plant).unwrap();
    assert_eq!(p.place, MemoryPlace::Attached);
    assert_eq!(p.n, Some(1));
    assert_eq!(p.title, "観葉植物");
    assert!(p.delta_milli.unwrap() > 0, "{p:?}");
    let c = report.items.iter().find(|i| i.target == car).unwrap();
    assert_eq!(c.place, MemoryPlace::Band);
    assert!(c.delta_milli.unwrap() < 0, "{c:?}");
    assert_eq!(report.degraded, ["no_embed"]);
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].n, 1);
}
