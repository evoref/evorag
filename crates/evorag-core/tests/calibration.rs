//! リランクのしきい値の較正 (05 §3.7、FR-413)。llama-server は記録の再生で置き換える (09 §3.3)。
//! 再生のリランカーは、問いと文書で共通の文字の数から値を作る (決定的)。

use std::path::Path;
use std::sync::Arc;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{ModelOrigin, TARGET_CONVERSATION};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Event, RecallState};
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

/// `size` を変えると別のリランカーになる。
fn reranker(size: u64) -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(&format!(
            r#"{{
              "props": {{"build_info": "b1-test", "model_path": "/m/rr-{size}.gguf", "total_slots": 4,
                         "default_generation_settings": {{"n_ctx": 8192}}}},
              "models": {{"data": [{{"id": "rr", "meta": {{"n_params": 5, "size": {size}, "n_vocab": 7}}}}]}},
              "rerank": true
            }}"#
        ))
        .unwrap(),
    )
}

fn config(root: &Path, seed: u64, rerank: Arc<ReplayClient>) -> CoreConfig {
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
        rerank: Some(rerank as Arc<dyn LlmClient>),
        aux_request_extra: Default::default(),
        probe_models: true,
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

/// リランカーの model_key (再生のサーバと同じ式)。
fn key(size: u64) -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(5),
        size: Some(size),
        n_vocab: Some(7),
        n_ctx_train: None,
    };
    evorag_llm::model_key(&format!("/m/rr-{size}.gguf"), &meta, "rr")
}

fn origin(size: u64) -> ModelOrigin {
    ModelOrigin {
        model_key: key(size),
        file_name: format!("rr-{size}.gguf"),
        model_name: "rr".into(),
        n_params: Some(5),
        size: Some(size),
        n_vocab: Some(7),
        build_info: "b1-test".into(),
    }
}

async fn start(root: &Path, seed: u64, rr: Arc<ReplayClient>) -> (Core, Vec<Event>, RecallState) {
    let (core, mut rx, report) = Core::start(config(root, seed, rr), chat()).await.unwrap();
    core.settle_calibration().await;
    core.settle_probe().await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    (core, events, report.recall)
}

/// verifies: AC-FR-413-3 (setup を通らずに記録したリランカーは、起動の後に裏で較正され、状態が変わる)
#[tokio::test(flavor = "multi_thread")]
async fn a_new_reranker_is_calibrated_in_the_background_once() {
    let dir = TempDir::new("calibration");
    let rr = reranker(2);
    let (core, events, recall) = start(dir.path(), 1, rr.clone()).await;
    assert_eq!(
        recall,
        RecallState::CandidatesOnly {
            calibrating: true,
            failed: false
        }
    );
    let t = core.threshold().expect("calibrated");
    assert_eq!(
        (
            t.reranker_key.as_str(),
            t.target.as_str(),
            t.version,
            t.cases.as_str()
        ),
        (key(2).as_str(), TARGET_CONVERSATION, 1, "ja-1")
    );
    assert!(t.threshold.is_some() && t.band_low.is_some());
    assert_eq!(t.stats["rank_n"], 36);
    // 60 問いのリランクと、点検の 1 回。
    assert_eq!(rr.aux_calls().1, 61);
    let expected = if t.passed {
        RecallState::Standard
    } else {
        RecallState::CandidatesOnly {
            calibrating: false,
            failed: true,
        }
    };
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::RecallStatus { state } if *state == expected)),
        "{events:?}"
    );
    core.shutdown().await;

    // 次の起動では求め直さない (NFR-101)。
    let again = reranker(2);
    let (core, _, recall) = start(dir.path(), 2, again.clone()).await;
    assert_eq!(again.aux_calls().1, 0);
    assert_eq!(recall, expected);
    core.shutdown().await;
}

/// verifies: AC-FR-413-2 (切り替えると求め直し、前の記録も残る。前のリランカーに戻すと、その記録を使う)
#[tokio::test(flavor = "multi_thread")]
async fn switching_rerankers_keeps_each_threshold() {
    let dir = TempDir::new("calibration-switch");
    let (core, _, _) = start(dir.path(), 1, reranker(2)).await;
    let first = core.threshold().unwrap();
    core.shutdown().await;

    let bind = |size: u64, seed: u64| {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_100_000)));
        let m = DataRootModels::open(
            dir.path(),
            "0.0.6",
            clock.clone(),
            Arc::new(UuidV7Gen::with_seed(clock, seed)),
        )
        .unwrap();
        m.bind("rerank", origin(size)).unwrap();
        m
    };
    drop(bind(9, 3));
    let other = reranker(9);
    let (core, _, _) = start(dir.path(), 4, other.clone()).await;
    let second = core.threshold().unwrap();
    assert_eq!(second.reranker_key, key(9));
    assert_eq!(other.aux_calls().1, 61);
    core.shutdown().await;

    // 前のリランカーの記録は残っていて、戻すとそれを使う (求め直さない)。
    let m = bind(2, 5);
    assert_eq!(m.threshold(&key(2)).unwrap(), first);
    assert_eq!(m.threshold(&key(9)).unwrap(), second);
    drop(m);
    let back = reranker(2);
    let (core, _, _) = start(dir.path(), 6, back.clone()).await;
    assert_eq!(core.threshold().unwrap(), first);
    assert_eq!(back.aux_calls().1, 0);
    core.shutdown().await;
}
