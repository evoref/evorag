//! 候補の帯と、想起への操作 (「添えて作り直す」と「無関係」の印、05 §5.2、FR-412)。
//! llama-server は記録の再生で置き換える。再生のリランカーは、共通の文字の数 − 10 を値にする。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::{ReplayClient, toy_rerank};
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{
    KIND_RECALL_FEEDBACK, KIND_SIGNAL, KIND_TURN, METHOD_BUNDLED, RecallFeedbackBody,
    TARGET_CONVERSATION, ThresholdBody, TurnBody,
};
use evorag_retrieval::calibrate::exchange_text;
use evorag_store::Read;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, TurnView};
use evorag_types::{Id, ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const PLANT: &str = "モンステラの水やりは、どれくらいの頻度がいい？";
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

fn config(root: &Path, seed: u64) -> CoreConfig {
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
        rerank: Some(reranker() as Arc<dyn LlmClient>),
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

fn score(user: &str) -> f64 {
    f64::from(toy_rerank(ASK, &exchange_text("ja", user, REPLY)))
}

/// しきい値を、どちらの往復も帯に入るように置く (添えない)。
fn prepare(root: &Path) {
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
        threshold: Some(score(PLANT) + 1.0),
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

async fn session(core: &Core, rx: &mut UnboundedReceiver<Event>, title: &str) -> Vec<TurnView> {
    core.command(Command::NewSession {
        title: title.into(),
    });
    let ev = until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    match ev.last() {
        Some(Event::SessionOpened { turns, .. }) => turns.clone(),
        _ => panic!(),
    }
}

async fn finished(core: &Core, rx: &mut UnboundedReceiver<Event>) -> TurnView {
    let ev = until(rx, |e| {
        matches!(e, Event::ResponseFinished { .. } | Event::Rejected { .. })
    })
    .await;
    core.settle().await;
    match ev.last() {
        Some(Event::ResponseFinished { turn, .. }) => turn.clone(),
        other => panic!("{other:?}"),
    }
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> TurnView {
    core.command(Command::Send { text: text.into() });
    finished(core, rx).await
}

fn open_store(root: &Path) -> evorag_store::Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap()
    .0
}

/// 種類の記録の数と、想起への操作の本体。
fn count(root: &Path, kind: &str) -> usize {
    let store = open_store(root);
    let rtx = store.read().unwrap();
    let mut n = 0;
    rtx.for_each(kind, |r| {
        n += usize::from(matches!(r, Read::Ok(_)));
        true
    })
    .unwrap();
    n
}

fn feedback(root: &Path) -> Vec<RecallFeedbackBody> {
    let store = open_store(root);
    let rtx = store.read().unwrap();
    let mut out = Vec::new();
    rtx.for_each(KIND_RECALL_FEEDBACK, |r| {
        if let Read::Ok(e) = r {
            out.push(e.body_as().unwrap());
        }
        true
    })
    .unwrap();
    out
}

fn turn_body(root: &Path, id: Id) -> TurnBody {
    let store = open_store(root);
    let rtx = store.read().unwrap();
    match rtx.get(KIND_TURN, id).unwrap() {
        Some(Read::Ok(e)) => e.body_as().unwrap(),
        other => panic!("{other:?}"),
    }
}

struct Setup {
    core: Core,
    rx: UnboundedReceiver<Event>,
    plant: Id,
    car: Id,
    answer: TurnView,
}

async fn setup(dir: &TempDir, replies: usize) -> Setup {
    prepare(dir.path());
    let (core, mut rx, _) = Core::start(config(dir.path(), 7), Arc::new(chat(replies)))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    let car = say(&core, &mut rx, CAR).await.reply_to.unwrap();
    session(&core, &mut rx, "今日").await;
    let answer = say(&core, &mut rx, ASK).await;
    Setup {
        core,
        rx,
        plant,
        car,
        answer,
    }
}

/// verifies: AC-FR-412-1, AC-FR-416-1 (利用者が帯から選んで添えたこと)
#[tokio::test(flavor = "multi_thread")]
async fn attaching_from_the_band_regenerates_with_that_exchange_and_keeps_the_old_answer() {
    let dir = TempDir::new("feedback-attach");
    let Setup {
        core,
        mut rx,
        plant,
        car,
        answer,
    } = setup(&dir, 5).await;
    // 帯: セッション名・日時・発話の冒頭。文脈には入っていない。
    assert_eq!(answer.sources, 0);
    let targets: Vec<Id> = answer.band.iter().map(|b| b.target).collect();
    assert_eq!(targets, [plant, car]);
    assert_eq!(answer.band[0].title, "観葉植物");
    assert!(answer.band[0].head.starts_with("モンステラの水やり"));
    assert!(!answer.band_nearby);

    core.command(Command::AttachBand {
        answer: answer.id,
        target: plant,
    });
    let again = finished(&core, &mut rx).await;
    assert_ne!(again.id, answer.id);
    assert_eq!(again.reply_to, answer.reply_to);
    assert_eq!(again.sources, 1);

    // 古い応答の帯からは作り直せない (最新の応答の帯だけ)。
    core.command(Command::AttachBand {
        answer: answer.id,
        target: car,
    });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    assert!(matches!(
        ev.last(),
        Some(Event::Rejected { error }) if error.key == MessageKey::BandNotLatest
    ));
    core.shutdown().await;

    let body = turn_body(dir.path(), again.id);
    assert!(body.attached.contains(&plant));
    assert_eq!(body.obs.unwrap().user_attached, [plant]);
    // 前の応答も残り、再生成の信号は記録しない。
    assert_eq!(turn_body(dir.path(), answer.id).reply_to, answer.reply_to);
    assert_eq!(count(dir.path(), KIND_SIGNAL), 0);
    let fb = feedback(dir.path());
    assert_eq!(fb.len(), 1);
    assert_eq!(
        (fb[0].op.as_str(), fb[0].answer, fb[0].target),
        ("attach", answer.id, plant)
    );
    assert!((f64::from(fb[0].score.unwrap()) - score(PLANT)).abs() < 1e-6);
    assert_eq!(fb[0].reranker.as_deref(), Some(rr_key().as_str()));
}

/// verifies: AC-FR-412-2
#[tokio::test(flavor = "multi_thread")]
async fn the_irrelevant_mark_is_recorded_toggled_and_survives_a_lost_database() {
    let dir = TempDir::new("feedback-irrelevant");
    let Setup {
        core,
        mut rx,
        plant,
        car,
        answer,
    } = setup(&dir, 4).await;
    let toggle = |target| Command::ToggleIrrelevant {
        answer: answer.id,
        target,
    };
    core.command(toggle(car));
    let ev = until(&mut rx, |e| matches!(e, Event::IrrelevantChanged { .. })).await;
    assert!(matches!(
        ev.last(),
        Some(Event::IrrelevantChanged { irrelevant: true, target, .. }) if *target == car
    ));
    core.command(toggle(plant));
    until(&mut rx, |e| matches!(e, Event::IrrelevantChanged { .. })).await;
    // もう一度選ぶと取り消せる。
    core.command(toggle(plant));
    let ev = until(&mut rx, |e| matches!(e, Event::IrrelevantChanged { .. })).await;
    assert!(matches!(
        ev.last(),
        Some(Event::IrrelevantChanged {
            irrelevant: false,
            ..
        })
    ));
    // 同じセッションの次の応答の帯には、印を付けた往復を出さない (判定は変えない)。
    let next = say(&core, &mut rx, ASK).await;
    let targets: Vec<Id> = next.band.iter().map(|b| b.target).collect();
    assert_eq!(targets, [plant]);
    core.shutdown().await;
    let obs = turn_body(dir.path(), next.id).obs.unwrap();
    assert_eq!(obs.band, [plant, car], "the band itself is unchanged");

    // DB を消してジャーナルから戻しても、印は残り、取り消しも残る。
    std::fs::remove_file(dir.path().join("evorag.redb")).unwrap();
    let (core, mut rx, _) = Core::start(config(dir.path(), 9), Arc::new(chat(0)))
        .await
        .unwrap();
    core.settle().await;
    core.shutdown().await;
    drop(rx.try_recv());
    let fb = feedback(dir.path());
    assert_eq!(fb.len(), 1);
    assert_eq!((fb[0].op.as_str(), fb[0].target), ("irrelevant", car));
}

/// verifies: AC-FR-412-2 (問いか対象の会話を消すと、その操作の記録も消える。03 §4.2 の規則 12), AC-FR-309-2
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_removes_its_feedback() {
    let dir = TempDir::new("feedback-delete");
    let Setup {
        core,
        mut rx,
        car,
        answer,
        ..
    } = setup(&dir, 3).await;
    core.command(Command::ToggleIrrelevant {
        answer: answer.id,
        target: car,
    });
    until(&mut rx, |e| matches!(e, Event::IrrelevantChanged { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.shutdown().await;
    assert!(feedback(dir.path()).is_empty());
}
