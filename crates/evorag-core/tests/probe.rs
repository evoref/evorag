//! モデルの性質の点検 (06 §3.3、FR-514)。llama-server は記録の再生で置き換える (09 §3.3)。

use std::path::Path;
use std::sync::Arc;

use evorag_core::probe::{self, ProbeIssue};
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Generation, LlmClient};
use evorag_store::testutil::TempDir;
use evorag_types::api::Event;
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

/// 型縛りの課題に型どおりに答え、速さの課題に答える chat のサーバ。
const GOOD: &str = r#"
  {"events": [{"delta": "{\"n\": 3}"}, {"done": {}}]},
  {"events": [{"delta": "1"}, {"wait_ms": 20}, {"delta": " 2 3"},
              {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#;

/// 思考を出し、型縛りの出力が空になる chat のサーバ。
const BROKEN: &str = r#"
  {"events": [{"reasoning": "考え中"}, {"done": {}}]},
  {"events": [{"reasoning": "考え中"}, {"delta": "1 2 3"},
              {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#;

fn server(chats: &str, extra: &str) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/qwen.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{chats}]{extra}
        }}"#
    ))
    .unwrap()
}

fn config(root: &Path, seed: u64) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let mut aux = evorag_llm::RequestExtra::new();
    aux.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({"enable_thinking": false}),
    );
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
        aux_request_extra: aux,
        probe_models: true,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

/// verifies: AC-FR-514-1 (chat の点検: 型縛り・思考・速さ)
#[tokio::test]
async fn the_chat_probe_checks_structured_output_thinking_and_speed() {
    let llm = server(GOOD, "");
    let mut aux = evorag_llm::RequestExtra::new();
    aux.insert("x_aux".into(), serde_json::json!(1));
    let p = probe::chat(&llm, "k", "b", &aux, Some(1)).await;
    assert_eq!(p.structured_ok, Some(true));
    assert_eq!(p.thinking_off, Some(true));
    assert!(p.prompt_tps.is_some_and(|v| v > 0.0), "{p:?}");
    assert!(p.gen_tps.is_some_and(|v| v > 0.0), "{p:?}");
    assert_eq!(p.error, None);
    assert!(probe::issues(&p).is_empty());
    // 補助の呼び出しの設定を足し、型縛りの課題には型を付け、最後のスロットを使う。
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs.iter().all(|r| r.extra.contains_key("x_aux")));
    assert!(reqs.iter().all(|r| r.id_slot == Some(1)));
    assert!(reqs[0].extra.contains_key("response_format"));
    assert!(!reqs[1].extra.contains_key("response_format"));

    let p = probe::chat(&server(BROKEN, ""), "k", "b", &aux, None).await;
    assert_eq!(p.structured_ok, Some(false));
    assert_eq!(p.thinking_off, Some(false));
    assert_eq!(
        probe::issues(&p),
        [ProbeIssue::ThinkingNotOff, ProbeIssue::StructuredBroken]
    );
}

/// verifies: AC-FR-514-1 (embed と rerank の点検)
#[tokio::test]
async fn the_embed_and_rerank_probes_measure_dims_and_time() {
    let e = probe::embed(&server("", r#", "embed": {"dim": 8}"#), "k", "b").await;
    assert_eq!(e.dims, Some(8));
    assert!(e.embed_ms.is_some());
    assert_eq!(e.error, None);
    let r = probe::rerank(&server("", r#", "rerank": true"#), "k", "b").await;
    assert!(r.rerank6_ms.is_some());
    assert_eq!(r.error, None);
    // 答えないサーバでは失敗を記録する (埋め込み・リランカーが無いものとして動く)。
    let none = probe::embed(&server("", ""), "k", "b").await;
    assert!(none.dims.is_none() && none.error.is_some());
}

/// verifies: AC-FR-514-1 (起動の後に裏で点検し、結果を保存し、同じモデルでは繰り返さない)
#[tokio::test(flavor = "multi_thread")]
async fn the_core_probes_once_per_model_and_reports_issues() {
    let dir = TempDir::new("probe");
    let llm = Arc::new(server(BROKEN, ""));
    let (core, mut rx, _) = Core::start(config(dir.path(), 1), llm.clone() as Arc<dyn LlmClient>)
        .await
        .unwrap();
    core.settle_probe().await;
    let p = core.probe_result("chat").expect("probe saved");
    assert_eq!(p.thinking_off, Some(false));
    assert_eq!(llm.remaining(), 0);
    let mut issues = None;
    while let Ok(ev) = rx.try_recv() {
        if let Event::ProbeIssues {
            role,
            thinking_not_off,
            structured_broken,
        } = ev
        {
            issues = Some((role, thinking_not_off, structured_broken));
        }
    }
    assert_eq!(issues, Some(("chat".to_owned(), true, true)));
    core.shutdown().await;

    // 同じモデル・同じ build_info では、次の起動で点検しない (NFR-101)。
    let again = Arc::new(server("", ""));
    let (core, _, _) = Core::start(config(dir.path(), 2), again.clone() as Arc<dyn LlmClient>)
        .await
        .unwrap();
    core.settle_probe().await;
    assert!(again.requests().is_empty());
    assert!(core.probe_result("chat").is_some());
    core.shutdown().await;
}
