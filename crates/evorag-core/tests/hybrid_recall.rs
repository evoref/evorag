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
        prompt_language_explicit: true,
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
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
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

/// 埋め込み・リランカー・しきい値の有無の組み合わせで run を回し、応答のターンを返す。
async fn run_with(dir: &TempDir, embed: bool, rerank: bool) -> (TurnBody, Id) {
    let llm = chat(3);
    let mut cfg = config(dir.path(), rerank.then(reranker));
    if embed {
        cfg.embed = Some(Arc::new(embedder()));
    }
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm)).await.unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    say(&core, &mut rx, CAR).await;
    core.settle_background().await;
    session(&core, &mut rx, "今日").await;
    let answer = say(&core, &mut rx, ASK).await;
    core.shutdown().await;
    (turn_body(dir.path(), answer.id), plant)
}

/// verifies: AC-FR-404-2 (埋め込み・リランカー・較正の値のそれぞれが無い構成で、05 §3.5 の表のとおりに縮退し、
/// 理由が観測値に残る)
#[tokio::test(flavor = "multi_thread")]
async fn each_missing_part_degrades_as_specified() {
    let (sp, sc) = (score(PLANT), score(CAR));
    for (case, embed, rerank, threshold, reasons, decision) in [
        ("all", true, true, true, vec![], RecallDecision::Fire),
        (
            "no-embed",
            false,
            true,
            true,
            vec!["no_embed"],
            RecallDecision::Fire,
        ),
        (
            "no-rerank",
            true,
            false,
            true,
            vec!["no_rerank"],
            RecallDecision::Fire,
        ),
        (
            "neither",
            false,
            false,
            true,
            vec!["no_embed", "no_rerank"],
            RecallDecision::Fire,
        ),
        (
            "no-threshold",
            true,
            true,
            false,
            vec!["no_threshold"],
            RecallDecision::Abstain,
        ),
    ] {
        let dir = TempDir::new(&format!("degrade-{case}"));
        prepare(
            dir.path(),
            threshold.then_some((true, sp - 0.5, sc - 1.0)),
            None,
        );
        let (answer, plant) = run_with(&dir, embed, rerank).await;
        let obs = answer.obs.unwrap();
        assert_eq!(obs.degraded, reasons, "{case}");
        assert_eq!(answer.recall.unwrap().decision, decision, "{case}");
        if decision == RecallDecision::Fire {
            assert!(answer.attached.contains(&plant), "{case}");
        }
    }
}

/// verifies: AC-FR-404-1 (同じ索引と問いで、RRF の順位・往復へのまとめ・添える 1 件が毎回同じ)
#[tokio::test(flavor = "multi_thread")]
async fn the_same_data_and_question_give_the_same_choice() {
    let (sp, sc) = (score(PLANT), score(CAR));
    let mut seen = Vec::new();
    for round in 0..2 {
        let dir = TempDir::new(&format!("determinism-{round}"));
        prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
        let (answer, _) = run_with(&dir, true, true).await;
        let obs = answer.obs.unwrap();
        // 裏の処理と ID の振られる順が前後するので、ID ではなく本文で比べる。
        let text = |id: &Id| turn_body(dir.path(), *id).text;
        seen.push((
            answer.attached.iter().map(text).collect::<Vec<_>>(),
            obs.reranked
                .iter()
                .map(|r| (text(&r.id), r.score.to_bits()))
                .collect::<Vec<_>>(),
            obs.band.iter().map(text).collect::<Vec<_>>(),
        ));
    }
    assert_eq!(seen[0], seen[1]);
}

/// 文の窓の数値を差し替える (このプロセスだけ。nextest は試験ごとにプロセスを分ける)。
fn windows_on(floor: f64) {
    evorag_core::set_tuning(evorag_core::RecallTuning {
        windows: true,
        floor,
        ..evorag_core::RecallTuning::default()
    });
}

/// verifies: AC-FR-417-3 (2 件目の下限以上の往復は添え、下限未満は添えずに帯に出す)
#[tokio::test(flavor = "multi_thread")]
async fn exchanges_above_the_second_floor_are_attached_too() {
    let (sp, sc) = (score(PLANT), score(CAR));
    assert!(sp > sc, "the plant exchange scores higher ({sp} vs {sc})");
    // しきい値は PLANT のすぐ下、帯の下限は CAR の 1 つ下。
    let thr = (sp - 0.5, sc - 1.0);
    // 下限 = 帯の下限 (floor 1): CAR も添える。帯には出さない。
    windows_on(1.0);
    let dir = TempDir::new("windows-floor-low");
    prepare(dir.path(), Some((true, thr.0, thr.1)), None);
    let (answer, plant) = run_with(&dir, false, true).await;
    let text = |id: &Id| turn_body(dir.path(), *id).text;
    let attached: Vec<String> = answer.attached.iter().map(text).collect();
    assert!(answer.attached.contains(&plant), "{attached:?}");
    assert!(attached.iter().any(|t| t == CAR), "{attached:?}");
    let obs = answer.obs.unwrap();
    assert!(
        obs.band.is_empty(),
        "attached exchanges are not in the band"
    );
    assert_eq!(answer.recall.unwrap().decision, RecallDecision::Fire);
    let packed = obs.packed.expect("packed windows are recorded");
    assert!(!packed.windows.is_empty() && packed.tokens > 0);
}

/// verifies: AC-FR-417-3 (下限がしきい値なら、しきい値未満の往復は添えずに帯に出す)
#[tokio::test(flavor = "multi_thread")]
async fn exchanges_below_the_second_floor_stay_in_the_band() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_on(0.0);
    let dir = TempDir::new("windows-floor-high");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let (answer, plant) = run_with(&dir, false, true).await;
    let text = |id: &Id| turn_body(dir.path(), *id).text;
    let attached: Vec<String> = answer.attached.iter().map(text).collect();
    assert!(answer.attached.contains(&plant), "{attached:?}");
    assert!(!attached.iter().any(|t| t == CAR), "{attached:?}");
    let band: Vec<String> = answer.obs.unwrap().band.iter().map(text).collect();
    assert_eq!(band, [CAR]);
}

/// verifies: AC-FR-417-3 (最上位がしきい値未満なら、何も添えない)
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_attached_below_the_threshold() {
    let sp = score(PLANT);
    windows_on(1.0);
    let dir = TempDir::new("windows-none");
    prepare(dir.path(), Some((true, sp + 1.0, sp - 0.5)), None);
    let (answer, _) = run_with(&dir, false, true).await;
    assert!(answer.attached.is_empty());
    assert_eq!(answer.recall.unwrap().decision, RecallDecision::Abstain);
}

/// verifies: AC-FR-417-1 (同じ索引・問い・数値で、採る往復・窓・詰める順が毎回同じ)
#[tokio::test(flavor = "multi_thread")]
async fn windows_are_the_same_for_the_same_data() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_on(1.0);
    let mut seen = Vec::new();
    for round in 0..2 {
        let dir = TempDir::new(&format!("windows-determinism-{round}"));
        prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
        let (answer, _) = run_with(&dir, false, true).await;
        let text = |id: &Id| turn_body(dir.path(), *id).text;
        let packed = answer.obs.unwrap().packed.unwrap();
        seen.push((
            answer.attached.iter().map(text).collect::<Vec<_>>(),
            packed
                .windows
                .iter()
                .map(|w| (text(&w.turn), w.start, w.end))
                .collect::<Vec<_>>(),
            packed.tokens,
        ));
    }
    assert_eq!(seen[0], seen[1]);
}

/// verifies: AC-FR-417-4 (件数・合計・一覧の意図がある問いだけ、候補と予算を広げる)
#[tokio::test(flavor = "multi_thread")]
async fn count_questions_widen_the_candidates() {
    evorag_core::set_tuning(evorag_core::RecallTuning {
        windows: true,
        count_widen: true,
        ..evorag_core::RecallTuning::default()
    });
    let sp = score(PLANT);
    let dir = TempDir::new("windows-widen");
    prepare(dir.path(), Some((true, sp - 0.5, sp - 3.0)), None);
    let llm = chat(4);
    let (core, mut rx, _) = Core::start(config(dir.path(), Some(reranker())), Arc::new(llm))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    say(&core, &mut rx, PLANT).await;
    core.settle_background().await;
    session(&core, &mut rx, "今日").await;
    let plain = say(&core, &mut rx, ASK).await;
    let count = say(&core, &mut rx, "モンステラの水やりは、全部で何回した？").await;
    core.shutdown().await;
    let widened = |id: Id| {
        turn_body(dir.path(), id)
            .obs
            .and_then(|o| o.query)
            .map(|q| (q.count, q.widened))
    };
    assert_eq!(widened(plain.id), Some((false, false)));
    assert_eq!(widened(count.id), Some((true, true)));
}

/// 文の窓を有効にし、予算の 2 段を見分けやすい値にする (このプロセスだけ)。
fn windows_with_budgets(large: usize, small: usize) {
    evorag_core::set_tuning(evorag_core::RecallTuning {
        windows: true,
        floor: 0.0,
        budget: large,
        small_budget: small,
        ..evorag_core::RecallTuning::default()
    });
}

/// 先読みの試験の流れ: PLANT と CAR のセッションを作り、新しいセッションで `draft` の入力が止まったことを
/// 知らせ (`between` を挟む)、`ask` を送る。(応答のターン, 再生の chat のサーバ)。
async fn prefetched(
    dir: &TempDir,
    llm: ReplayClient,
    cfg: CoreConfig,
    draft: &str,
    between: impl AsyncFnOnce(&Core, &mut UnboundedReceiver<Event>),
    ask: &str,
) -> (TurnBody, ReplayClient) {
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    say(&core, &mut rx, PLANT).await;
    session(&core, &mut rx, "車").await;
    say(&core, &mut rx, CAR).await;
    core.settle_background().await;
    session(&core, &mut rx, "今日").await;
    core.command(Command::InputIdle { text: draft.into() });
    core.settle_precompute().await;
    between(&core, &mut rx).await;
    let answer = say(&core, &mut rx, ask).await;
    core.shutdown().await;
    (turn_body(dir.path(), answer.id), llm)
}

fn frame_of(messages: &[evorag_llm::ChatMessage]) -> Option<&str> {
    let m = messages.get(messages.len().checked_sub(2)?)?;
    m.content.contains("<<引用-").then_some(m.content.as_str())
}

/// verifies: AC-FR-418-1, AC-FR-417-6 (先読みの要求の形と、当たったときに枠のバイト列をそのまま使う。
/// 当たったときは大きな予算)
#[tokio::test(flavor = "multi_thread")]
async fn a_prefetched_frame_is_sent_byte_for_byte_when_the_draft_is_sent() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 120);
    let dir = TempDir::new("prefetch-hit");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let llm = chat(3);
    let cfg = config(dir.path(), Some(reranker()));
    let (answer, llm) = prefetched(&dir, llm, cfg, ASK, async |_, _| {}, ASK).await;
    // 先読みの要求: 履歴 + 引用の枠のメッセージ + 空の発話、生成 1 トークン、応答のスロット。
    let warm = llm.warm_requests();
    let pre = warm
        .iter()
        .find(|r| frame_of(&r.messages).is_some())
        .expect("a prefetch request with the quote frame");
    assert_eq!(pre.messages.last().unwrap().content, "");
    assert_eq!(pre.generation.max_tokens, Some(1));
    // 応答の要求の枠は、先読みの枠と同じバイト列。
    let reqs = llm.requests();
    let sent = reqs.last().unwrap();
    assert_eq!(frame_of(&sent.messages), frame_of(&pre.messages));
    assert!(!sent.messages.last().unwrap().content.contains("<<引用-"));
    let obs = answer.obs.unwrap();
    assert_eq!(obs.prefetch.unwrap().kind, "a");
    assert_eq!(obs.packed.unwrap().budget, 800);
    assert_eq!(
        obs.recall_position.as_deref(),
        Some(evorag_memory::RECALL_SEPARATE_MESSAGE)
    );
}

/// verifies: AC-FR-417-6 (先読みが無いときは小さな予算)
#[tokio::test(flavor = "multi_thread")]
async fn without_a_prefetch_the_small_budget_is_used() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 120);
    let dir = TempDir::new("prefetch-none");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let (answer, _) = run_with(&dir, false, true).await;
    let obs = answer.obs.unwrap();
    assert_eq!(obs.prefetch.unwrap().kind, evorag_memory::PREFETCH_NONE);
    assert_eq!(obs.packed.unwrap().budget, 120);
}

/// verifies: AC-FR-418-2 (削除などで先読みの世代が変わったら、前の枠を使わない)
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_discards_the_prefetched_frame() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 120);
    let dir = TempDir::new("prefetch-delete");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let llm = chat(3);
    let cfg = config(dir.path(), Some(reranker()));
    let (answer, _) = prefetched(
        &dir,
        llm,
        cfg,
        ASK,
        async |core: &Core, rx: &mut UnboundedReceiver<Event>| {
            // 別のセッションを作って消す (削除は事実の表を作り直させ、先読みの世代を上げる)。
            session(core, rx, "消す").await;
            core.command(Command::DeleteSession);
            until(rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
            core.settle().await;
            session(core, rx, "今日 2").await;
        },
        ASK,
    )
    .await;
    assert_eq!(
        answer.obs.unwrap().prefetch.unwrap().kind,
        evorag_memory::PREFETCH_NONE
    );
}

/// verifies: AC-FR-418-3 (下書きの本文は DB・ジャーナル・ログ・観測値のどこにも書かれない)
#[tokio::test(flavor = "multi_thread")]
async fn the_draft_is_never_written() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 120);
    let dir = TempDir::new("prefetch-draft");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let marker = "下書きだけの印ズィキュル";
    let draft = format!("モンステラの水やりの頻度 {marker}");
    let llm = chat(3);
    let cfg = config(dir.path(), Some(reranker()));
    let _ = prefetched(&dir, llm, cfg, &draft, async |_, _| {}, ASK).await;
    // データ根のどのファイルにも、下書きの本文が無い。
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir.path(), &mut files);
    assert!(!files.is_empty());
    let needle = marker.as_bytes();
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "the draft was written to {}",
            f.display()
        );
    }
}

/// verifies: AC-FR-418-4 (ループバック以外の chat と、電池で `reduce` のときは、枠の先読みをしない)
#[tokio::test(flavor = "multi_thread")]
async fn no_frame_prefetch_for_a_remote_chat_or_on_battery() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 120);
    // ループバック以外の chat。
    let dir = TempDir::new("prefetch-remote");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let one = format!(r#"{{"events": [{{"delta": "{REPLY}"}}, {{"done": {{}}}}]}}"#);
    let remote = ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{one}, {one}, {one}],
          "remote": true
        }}"#
    ))
    .unwrap();
    let cfg = config(dir.path(), Some(reranker()));
    let (answer, llm) = prefetched(&dir, remote, cfg, ASK, async |_, _| {}, ASK).await;
    assert!(
        llm.warm_requests()
            .iter()
            .all(|r| frame_of(&r.messages).is_none())
    );
    assert_eq!(
        answer.obs.unwrap().prefetch.unwrap().kind,
        evorag_memory::PREFETCH_NONE
    );
    // 電池で `reduce` (既定)。
    let dir = TempDir::new("prefetch-battery");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let mut cfg = config(dir.path(), Some(reranker()));
    cfg.probe.power = || evorag_core::power::Power::Battery;
    cfg.sleep.on_battery = evorag_core::OnBattery::Reduce;
    let (_, llm) = prefetched(&dir, chat(3), cfg, ASK, async |_, _| {}, ASK).await;
    assert!(
        llm.warm_requests()
            .iter()
            .all(|r| frame_of(&r.messages).is_none())
    );
}

/// verifies: AC-FR-417-6 (予算がとても小さくても、最上位の往復は狭い窓で添える)
#[tokio::test(flavor = "multi_thread")]
async fn the_top_exchange_is_kept_with_a_tight_window_when_the_budget_is_small() {
    let (sp, sc) = (score(PLANT), score(CAR));
    windows_with_budgets(800, 90);
    let dir = TempDir::new("windows-tight");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let (answer, plant) = run_with(&dir, false, true).await;
    assert!(
        answer.attached.contains(&plant),
        "the top exchange is attached"
    );
    let packed = answer.obs.unwrap().packed.unwrap();
    assert!(
        packed.tokens as usize <= 90,
        "within the budget: {}",
        packed.tokens
    );
}
