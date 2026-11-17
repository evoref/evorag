//! `memory.forget_key` を作る時機とフェイルクローズ (03 §3.1.1)。llama-server は記録の再生で置き換える。

use std::path::Path;
use std::sync::Arc;

use evorag_core::forget_key::FactsKeyProblem;
use evorag_core::{Core, CoreConfig, PromptLanguage, StartReport};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_store::meta;
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

fn cassette() -> ReplayClient {
    ReplayClient::from_json(
        r#"{
          "props": {"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 1,
                    "default_generation_settings": {"n_ctx": 8192}},
          "models": {"data": [{"id": "test", "meta": {"n_params": 1, "size": 2, "n_vocab": 3}}]},
          "chats": []
        }"#,
    )
    .unwrap()
}

async fn start(root: &Path) -> (Core, StartReport) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let config = CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.3.0".into(),
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
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
    };
    let (core, _rx, report) = Core::start(config, Arc::new(cassette())).await.unwrap();
    core.settle().await;
    (core, report)
}

fn key_in(root: &Path) -> Option<String> {
    let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.3.0".into(),
        kinds: evorag_memory::kinds(),
        clock: Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000))),
        ids: Arc::new(UuidV7Gen::with_seed(
            Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000))),
            1,
        )),
        config_file: None,
    })
    .unwrap();
    store.meta(meta::FORGET_KEY).unwrap()
}

/// 新しいデータ根では作り、次の起動では作り直さない。
#[tokio::test(flavor = "multi_thread")]
async fn the_key_is_created_once_on_the_first_writable_open() {
    let dir = TempDir::new("forget-key-core");
    let (core, report) = start(dir.path()).await;
    assert!(report.forget_key_created);
    assert_eq!(report.facts_disabled, None);
    assert!(core.facts_enabled());
    core.shutdown().await;
    let first = key_in(dir.path()).expect("a key");
    assert_eq!(first.len(), 64);
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );

    let (core, report) = start(dir.path()).await;
    assert!(!report.forget_key_created);
    assert!(core.facts_enabled());
    core.shutdown().await;
    assert_eq!(key_in(dir.path()), Some(first));
}

/// DB の鍵とジャーナルの鍵が違えば、作り直さずに事実の機能を止める (フェイルクローズ)。
#[tokio::test(flavor = "multi_thread")]
async fn a_disagreeing_key_disables_facts_instead_of_making_a_new_one() {
    let dir = TempDir::new("forget-key-core-conflict");
    let (core, _) = start(dir.path()).await;
    core.shutdown().await;
    let other = "0".repeat(64);
    raw_set_meta(dir.path(), meta::FORGET_KEY, &other);

    let (core, report) = start(dir.path()).await;
    assert_eq!(report.facts_disabled, Some(FactsKeyProblem::Conflict));
    assert!(!report.forget_key_created);
    assert!(!core.facts_enabled());
    core.shutdown().await;
    assert_eq!(key_in(dir.path()), Some(other));
}
