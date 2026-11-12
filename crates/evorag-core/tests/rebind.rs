//! `evorag setup` でのモデルの記録と切り替え (06 §3.2、FR-513)。DB の側だけを確かめる
//! (対話と llama-server の探索は bin の setup が行う)。

use std::path::Path;
use std::sync::Arc;

use evorag_core::rebind::{DataRootModels, RoleDiff, diff};
use evorag_core::{Core, CoreConfig, PromptLanguage, StartError};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::ModelOrigin;
use evorag_store::testutil::TempDir;
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

fn server(file: &str, size: u64) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/{file}", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": {size}, "n_vocab": 3}}}}]}}
        }}"#
    ))
    .unwrap()
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
        rerank: None,
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

fn open(root: &Path, seed: u64) -> Result<DataRootModels, evorag_store::OpenError> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_100_000)));
    DataRootModels::open(
        root,
        "0.0.6",
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, seed)),
    )
}

/// `server(file, size)` が返すモデルの元の値 (model_key は llama-server の値から同じ式で作る)。
fn origin(file: &str, size: u64) -> ModelOrigin {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(1),
        size: Some(size),
        n_vocab: Some(3),
        n_ctx_train: None,
    };
    ModelOrigin {
        model_key: evorag_llm::model_key(&format!("/m/{file}"), &meta, "test"),
        file_name: file.to_owned(),
        model_name: "test".into(),
        n_params: meta.n_params,
        size: meta.size,
        n_vocab: meta.n_vocab,
        build_info: "b1-test".into(),
    }
}
/// verifies: AC-FR-513-2 (「同じモデルとして扱う」を出す条件)
#[test]
fn the_same_model_choice_is_offered_only_when_the_values_match() {
    let a = origin("qwen-a.gguf", 2);
    assert_eq!(diff("chat", None, &a), RoleDiff::New);
    assert_eq!(diff("chat", Some(&a), &a), RoleDiff::Same);
    let renamed = origin("renamed.gguf", 2);
    assert!(matches!(
        diff("chat", Some(&a), &renamed),
        RoleDiff::Different {
            maybe_same: true,
            ..
        }
    ));
    let other = origin("qwen-a.gguf", 9);
    assert!(matches!(
        diff("chat", Some(&a), &other),
        RoleDiff::Different {
            maybe_same: false,
            ..
        }
    ));
    // リランカーには出さない (しきい値は求め直せる)。
    assert!(matches!(
        diff("rerank", Some(&a), &renamed),
        RoleDiff::Different {
            maybe_same: false,
            ..
        }
    ));
}

/// verifies: AC-FR-513-1 (DB の側), AC-FR-513-3 (TUI が動いている間は書かない)
#[tokio::test(flavor = "multi_thread")]
async fn setup_switches_the_recorded_model_only_while_the_tui_is_closed() {
    let dir = TempDir::new("rebind");
    let (core, _, _) = Core::start(config(dir.path(), 1), Arc::new(server("qwen-a.gguf", 2)))
        .await
        .unwrap();
    // TUI (コア) が動いている間は、データ根を開けない。
    assert!(matches!(
        open(dir.path(), 2),
        Err(evorag_store::OpenError::Lock(_))
    ));
    core.shutdown().await;

    // 閉じた後は開ける。記録は qwen-a。
    {
        let m = open(dir.path(), 3).unwrap();
        assert_eq!(m.recorded()["chat"].file_name, "qwen-a.gguf");
        m.bind("chat", origin("qwen-b.gguf", 5)).unwrap();
        assert_eq!(m.recorded()["chat"].file_name, "qwen-b.gguf");
    }

    // 切り替えた後は、新しいモデルで起動でき、前のモデルでは止まる。
    let (core, _, report) = Core::start(config(dir.path(), 4), Arc::new(server("qwen-b.gguf", 5)))
        .await
        .unwrap();
    assert_eq!(report.chat_model_recorded, None);
    core.shutdown().await;
    assert!(matches!(
        Core::start(config(dir.path(), 5), Arc::new(server("qwen-a.gguf", 2))).await,
        Err(StartError::ModelMismatch { .. })
    ));

    // 前のモデルは履歴に残る (前のモデルに戻したときに見分ける、06 §3.2 の 8)。
    let m = open(dir.path(), 6).unwrap();
    m.bind("chat", origin("qwen-a.gguf", 2)).unwrap();
    drop(m);
    let (core, _, _) = Core::start(config(dir.path(), 7), Arc::new(server("qwen-a.gguf", 2)))
        .await
        .unwrap();
    core.shutdown().await;
}
