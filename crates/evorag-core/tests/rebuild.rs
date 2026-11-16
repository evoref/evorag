//! `evorag rebuild [--index]` (03 §10、FR-212)。llama-server は記録の再生で置き換える。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::{ReplayClient, toy_rerank};
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{
    KIND_TURN, METHOD_BUNDLED, ModelProbeBody, TARGET_CONVERSATION, ThresholdBody, TurnBody,
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

fn count(root: &Path, kind: &str) -> usize {
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
    let mut n = 0;
    rtx.for_each(kind, |_| {
        n += 1;
        true
    })
    .unwrap();
    n
}

/// セッション `sid` を開き直して ASK を送り、応答のターンを返す (同じセッションのターンは候補から除くので、
/// 候補は前と同じ)。
async fn ask(dir: &TempDir, sid: Id, round: u64) -> TurnBody {
    let llm = chat(1);
    // 起動ごとに時刻と ID の種を変える (前の起動と同じ ID を作らないように)。
    let mut cfg = config(dir.path(), Some(reranker()));
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(
        1_762_670_000_000 + 1_000_000 * round as i64,
    )));
    cfg.ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), 20 + round));
    cfg.clock = clock;
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm)).await.unwrap();
    core.settle().await;
    core.command(Command::OpenSession { id: sid });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    let answer = say(&core, &mut rx, ASK).await;
    core.shutdown().await;
    turn_body(dir.path(), answer.id)
}

fn rebuild(dir: &TempDir, index_only: bool) -> evorag_core::rebuild::RebuildReport {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
    evorag_core::rebuild::run(evorag_core::rebuild::RebuildOptions {
        data_root: dir.path().to_path_buf(),
        evorag_version: "0.0.6".into(),
        config_file: None,
        index_only,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 11)),
    })
    .unwrap()
}

/// verifies: AC-FR-212-1 (rebuild の前に退避が作られ、作り直した派生データで同じ検索の結果になる。
/// モデルの記録・しきい値・想起への操作は変わらない)
#[tokio::test(flavor = "multi_thread")]
async fn rebuild_keeps_the_originals_and_recalls_the_same() {
    let (sp, sc) = (score(PLANT), score(CAR));
    let dir = TempDir::new("rebuild");
    prepare(dir.path(), Some((true, sp - 0.5, sc - 1.0)), None);
    let llm = chat(3);
    let (core, mut rx, _) = Core::start(config(dir.path(), Some(reranker())), Arc::new(llm))
        .await
        .unwrap();
    core.settle().await;
    session(&core, &mut rx, "観葉植物").await;
    let plant = say(&core, &mut rx, PLANT).await.reply_to.unwrap();
    session(&core, &mut rx, "車").await;
    let car = say(&core, &mut rx, CAR).await.reply_to.unwrap();
    session(&core, &mut rx, "今日").await;
    let first = say(&core, &mut rx, ASK).await;
    // 想起への操作 (正本) を 1 つ残す。
    core.command(Command::ToggleIrrelevant {
        answer: first.id,
        target: car,
    });
    until(&mut rx, |e| matches!(e, Event::IrrelevantChanged { .. })).await;
    core.settle().await;
    core.shutdown().await;
    let before = turn_body(dir.path(), first.id);
    assert!(before.attached.contains(&plant));
    let originals = |root: &Path| {
        [
            evorag_memory::KIND_MODEL_BINDING,
            evorag_memory::KIND_THRESHOLD,
            evorag_memory::KIND_RECALL_FEEDBACK,
            evorag_memory::KIND_TURN,
        ]
        .map(|k| count(root, k))
    };
    for index_only in [true, false] {
        // 前の回の問いでターンが増えるので、回ごとに数える。
        let kept = originals(dir.path());
        assert!(kept.iter().all(|n| *n > 0), "{kept:?}");
        let chunks = count(dir.path(), evorag_memory::KIND_CHUNK);
        let r = rebuild(&dir, index_only);
        assert!(r.backup.join("evorag.redb").is_file(), "{:?}", r.backup);
        assert!(r.docs > 0);
        if index_only {
            assert_eq!(r.removed, 0);
        } else {
            assert!(r.removed >= chunks, "{} < {chunks}", r.removed);
            assert_eq!(count(dir.path(), evorag_memory::KIND_MODEL_PROBE), 0);
        }
        assert_eq!(originals(dir.path()), kept, "index_only = {index_only}");
        assert_eq!(count(dir.path(), evorag_memory::KIND_CHUNK), chunks);
        let again = ask(&dir, first.session, u64::from(index_only)).await;
        assert_eq!(
            again.attached, before.attached,
            "the same recall after rebuild (index_only = {index_only})"
        );
    }
}
