//! memory の役割 (06 §2.1・§3・§3.1・§3.3、FR-515): 区切りの呼び出しを出す先、記録と照合、点検、縮退。
//! llama-server は記録の再生で置き換える (09 §3.3)。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evorag_core::{Core, CoreConfig, MemoryConfig, MemoryRoute, PromptLanguage, StartReport};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, ServerInfo};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, MemoryFallback, MemoryState, RoleStatus};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

/// 型縛りの課題に型どおりに答え、速さの課題に答える (点検に通る)。
const GOOD: &str = r#"
  {"events": [{"delta": "{\"n\": 3}"}, {"done": {}}]},
  {"events": [{"delta": "1"}, {"wait_ms": 20}, {"delta": " 2 3"},
              {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#;

/// 思考を出し、型縛りの出力が空になる。思考を切る指定を足しても変わらない (点検に通らない)。
const NEVER_OFF: &str = r#"
  {"events": [{"reasoning": "考え中"}, {"done": {}}]},
  {"events": [{"reasoning": "考え中"}, {"done": {}}]},
  {"events": [{"reasoning": "考え中"}, {"delta": "1 2 3"},
              {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#;

/// `file` のモデルを動かしている llama-server。`size` を変えると別のモデルになる。
fn server(file: &str, size: u64, slots: u32, healthy: bool, chats: &str) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/{file}", "total_slots": {slots},
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "{file}", "meta": {{"n_params": 1, "size": {size}, "n_vocab": 3}}}}]}},
          "health": {healthy},
          "chats": [{chats}]
        }}"#
    ))
    .unwrap()
}

fn chat_server(chats: &str) -> Arc<ReplayClient> {
    Arc::new(server("chat.gguf", 2, 1, true, chats))
}

fn memory_config(client: Option<Arc<dyn LlmClient>>) -> MemoryConfig {
    let mut request_extra = evorag_llm::RequestExtra::new();
    request_extra.insert("x_memory".into(), serde_json::json!(1));
    MemoryConfig {
        client,
        request_extra,
    }
}

fn config(root: &Path, seed: u64, memory: MemoryConfig, probe_models: bool) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    // chat の補助の呼び出しには思考を切る指定を書いておく (chat の点検を 2 回の呼び出しで終えるため)。
    let mut aux = evorag_llm::RequestExtra::new();
    aux.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({"enable_thinking": false}),
    );
    CoreConfig {
        root_format: evorag_core::RootFormat::V04Bench,
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
        rerank: None,
        memory,
        aux_request_extra: aux,
        probe_models,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

async fn start(
    root: &Path,
    seed: u64,
    chat: Arc<ReplayClient>,
    memory: Option<Arc<dyn LlmClient>>,
    probe_models: bool,
) -> (Core, UnboundedReceiver<Event>, StartReport) {
    let r = Core::start(
        config(root, seed, memory_config(memory), probe_models),
        chat as Arc<dyn LlmClient>,
    )
    .await
    .unwrap();
    r.0.settle().await;
    if probe_models {
        r.0.settle_probe().await;
    }
    r
}

/// 届いたイベントのうち、memory の役割の状態 (順に)。
fn memory_events(rx: &mut UnboundedReceiver<Event>) -> Vec<MemoryState> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::MemoryRole { state } = ev {
            out.push(state);
        }
    }
    out
}

fn call(route: MemoryRoute) -> evorag_core::MemoryCall {
    match route {
        MemoryRoute::Call(c) => c,
        other => panic!("expected a call, got {other:?}"),
    }
}

/// `llm.memory` を設定しなければ、区切りの呼び出しは chat のサーバとモデルに出る。memory の記録は作らない。
/// verifies: AC-FR-515-1 (設定しないと chat に出る)
#[tokio::test(flavor = "multi_thread")]
async fn without_llm_memory_segment_calls_go_to_the_chat_model() {
    let dir = TempDir::new("memory-default");
    let chat = chat_server(GOOD);
    let (core, mut rx, report) = start(dir.path(), 1, chat.clone(), None, false).await;
    assert_eq!(report.memory, MemoryState::SameAsChat);
    assert!(report.recorded_roles.is_empty());
    assert!(report.roles.iter().all(|r| r.role != "memory"));
    // chat の点検がまだなら、済むまで呼ばない。
    assert!(matches!(core.memory_route(), MemoryRoute::Pending));
    // `/model` には「chat と同じ」(未設定) の行が出る。
    core.command(Command::Model { name: None });
    let roles = loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let Event::ModelInfo { roles, .. } = ev {
            break roles;
        }
    };
    let memory = roles.iter().find(|r| r.role == "memory").unwrap();
    assert_eq!(memory.status, RoleStatus::Unset);
    core.shutdown().await;

    // 点検が済めば、chat のモデルで呼ぶ (縮退ではない)。
    let (core, _, _) = start(dir.path(), 2, chat_server(GOOD), None, true).await;
    let c = call(core.memory_route());
    let chat_key = core.server().model_key;
    assert_eq!(c.model_key, chat_key);
    assert!(!c.is_fallback());
    assert_eq!(c.role_model_key.as_deref(), Some(chat_key.as_str()));
    // 補助の呼び出しの指定 (chat の aux_request_extra) が効く。memory の指定は効かない。
    assert!(c.extra.contains_key("chat_template_kwargs"));
    assert!(!c.extra.contains_key("x_memory"));
    assert_eq!(c.slot, None);
    assert_eq!(core.memory_state(), MemoryState::SameAsChat);
    core.shutdown().await;
}

/// 別に置いた memory のサーバは、最初の起動で記録し、点検に通れば区切りの呼び出しに使う。
/// verifies: AC-FR-515-1 (設定すると memory のサーバに出る), AC-FR-512-2 (memory の記録)
#[tokio::test(flavor = "multi_thread")]
async fn a_separate_memory_server_is_recorded_probed_and_used() {
    let dir = TempDir::new("memory-own");
    let memory = Arc::new(server("qwen3-4b.gguf", 9, 4, true, GOOD));
    let (core, mut rx, report) = start(
        dir.path(),
        1,
        chat_server(GOOD),
        Some(memory.clone() as Arc<dyn LlmClient>),
        true,
    )
    .await;
    assert_eq!(report.recorded_roles, ["memory"]);
    let info = report.roles.iter().find(|r| r.role == "memory").unwrap();
    assert_eq!(info.status, RoleStatus::Ready);
    assert_eq!(report.memory, MemoryState::Own);
    let p = core.probe_result("memory").expect("memory probed");
    assert_eq!((p.structured_ok, p.thinking_off), (Some(true), Some(true)));
    // 点検は memory の指定を足し、最後のスロットを使う。
    let reqs = memory.requests();
    assert!(!reqs.is_empty());
    assert!(reqs.iter().all(|r| r.extra.contains_key("x_memory")));
    assert!(reqs.iter().all(|r| r.id_slot == Some(3)));

    let c = call(core.memory_route());
    assert_ne!(c.model_key, core.server().model_key);
    assert_eq!(c.role_model_key.as_deref(), Some(c.model_key.as_str()));
    assert!(!c.is_fallback());
    assert!(c.extra.contains_key("x_memory"));
    assert!(!c.extra.contains_key("chat_template_kwargs"));
    assert_eq!(c.slot, Some(3));
    // 状態は変わっていないので、知らせない。
    assert!(memory_events(&mut rx).is_empty());
    core.shutdown().await;
}

/// 記録と違う memory のモデルは使わず、chat で代行する。台帳には意図した (記録の) model_key を書けるようにする。
/// verifies: AC-FR-515-1 (記録と違うときは chat に出る), AC-FR-512-1 (記録と違う memory は使わない)
#[tokio::test(flavor = "multi_thread")]
async fn a_memory_model_that_differs_from_the_record_falls_back_to_chat() {
    let dir = TempDir::new("memory-mismatch");
    let a = Arc::new(server("qwen3-4b.gguf", 9, 1, true, GOOD));
    let (core, _, _) = start(
        dir.path(),
        1,
        chat_server(GOOD),
        Some(a.clone() as Arc<dyn LlmClient>),
        true,
    )
    .await;
    let recorded = call(core.memory_route()).model_key;
    core.shutdown().await;

    let b = Arc::new(server("other.gguf", 5, 1, true, GOOD));
    let (core, _, report) = start(
        dir.path(),
        2,
        chat_server(""),
        Some(b.clone() as Arc<dyn LlmClient>),
        true,
    )
    .await;
    let info = report.roles.iter().find(|r| r.role == "memory").unwrap();
    assert_eq!(info.status, RoleStatus::Mismatch);
    assert_eq!(info.recorded.as_ref().unwrap().0, "qwen3-4b.gguf");
    assert_eq!(
        report.memory,
        MemoryState::Fallback(MemoryFallback::Mismatch)
    );
    let c = call(core.memory_route());
    assert_eq!(c.model_key, core.server().model_key);
    assert_eq!(c.fallback, Some(MemoryFallback::Mismatch));
    assert!(c.is_fallback());
    assert_eq!(c.role_model_key, Some(recorded));
    // 記録と違うモデルは点検もしない。
    assert!(b.requests().is_empty());
    core.shutdown().await;
}

/// 起動のときに繋がらない memory は chat で代行し、繋がったら記録して使い始める。使っている間に繋がらなくなったら
/// (B2 の呼び出しが接続の失敗で終わったら)、また chat で代行する。
/// verifies: AC-FR-515-1 (繋がらないときは chat に出て、状態の行に縮退が出る)
#[tokio::test(flavor = "multi_thread")]
async fn a_memory_server_that_is_down_falls_back_and_comes_back() {
    let dir = TempDir::new("memory-down");
    let memory = Arc::new(Swappable(Mutex::new(Arc::new(server(
        "qwen3-4b.gguf",
        9,
        1,
        false,
        "",
    )))));
    let (core, mut rx, report) = start(
        dir.path(),
        1,
        chat_server(GOOD),
        Some(memory.clone() as Arc<dyn LlmClient>),
        true,
    )
    .await;
    assert_eq!(
        report.memory,
        MemoryState::Fallback(MemoryFallback::Waiting)
    );
    let c = call(core.memory_route());
    assert_eq!(c.model_key, core.server().model_key);
    assert_eq!(c.fallback, Some(MemoryFallback::Waiting));
    // まだ記録していないので、意図した model_key は分からない。
    assert_eq!(c.role_model_key, None);
    assert!(core.recheck_roles().await, "still down");
    assert!(memory_events(&mut rx).is_empty());

    *memory.0.lock().unwrap() = Arc::new(server("qwen3-4b.gguf", 9, 1, true, GOOD));
    assert!(!core.recheck_roles().await);
    assert_eq!(memory_events(&mut rx), [MemoryState::Own]);
    assert_eq!(core.memory_state(), MemoryState::Own);
    // 点検が済むまでは呼ばない。済めば memory で呼ぶ。
    core.settle_probe().await;
    let c = call(core.memory_route());
    assert!(!c.is_fallback());
    let key = c.model_key.clone();
    assert_ne!(key, core.server().model_key);

    // 使っている間に繋がらなくなった: chat で代行し、意図した model_key は記録のもの。
    core.memory_unreachable();
    assert_eq!(
        memory_events(&mut rx),
        [MemoryState::Fallback(MemoryFallback::Waiting)]
    );
    let c = call(core.memory_route());
    assert_eq!(c.role_model_key, Some(key));
    assert!(c.is_fallback());
    // 繋がれば戻る (記録し直さない)。
    assert!(!core.recheck_roles().await);
    assert_eq!(memory_events(&mut rx), [MemoryState::Own]);
    core.shutdown().await;
}

/// memory も chat も点検に通らないときは、区切りの呼び出しを止める (会話は続く)。memory の点検で思考を
/// 切れないと分かったら、知らせて chat で代行する。
/// verifies: AC-FR-515-1 (点検に通らないときは chat に出る)
#[tokio::test(flavor = "multi_thread")]
async fn a_memory_model_that_fails_the_probe_falls_back_and_stops_when_chat_fails_too() {
    let dir = TempDir::new("memory-probe");
    let memory = Arc::new(server("lfm.gguf", 9, 1, true, NEVER_OFF));
    let (core, mut rx, _) = start(
        dir.path(),
        1,
        chat_server(GOOD),
        Some(memory.clone() as Arc<dyn LlmClient>),
        true,
    )
    .await;
    let p = core.probe_result("memory").expect("memory probed");
    // 設定に指定が無いので、思考を切る指定を足して確かめ直し、それでも切れなかった。
    assert_eq!(p.thinking_off, Some(false));
    assert_eq!(p.aux_thinking_off, Some(false));
    assert_eq!(memory.requests().len(), 3);
    let mut issues = None;
    let mut states = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            Event::ProbeIssues {
                role,
                thinking_not_off,
                ..
            } if role == "memory" => issues = Some(thinking_not_off),
            Event::MemoryRole { state } => states.push(state),
            _ => {}
        }
    }
    assert_eq!(issues, Some(true));
    assert_eq!(states, [MemoryState::Fallback(MemoryFallback::ProbeFailed)]);
    let c = call(core.memory_route());
    assert_eq!(c.fallback, Some(MemoryFallback::ProbeFailed));
    assert_eq!(c.model_key, core.server().model_key);
    core.shutdown().await;

    // chat も点検に通らない (別のデータ根で、chat が思考を切れない): 止める。
    let dir = TempDir::new("memory-probe-stop");
    let broken_chat = chat_server(
        r#"{"events": [{"reasoning": "考え中"}, {"done": {}}]},
           {"events": [{"delta": "1 2 3"}, {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#,
    );
    let memory = Arc::new(server("lfm.gguf", 9, 1, true, NEVER_OFF));
    let (core, _, _) = start(
        dir.path(),
        1,
        broken_chat,
        Some(memory as Arc<dyn LlmClient>),
        true,
    )
    .await;
    assert!(matches!(core.memory_route(), MemoryRoute::Stopped));
    assert_eq!(core.memory_state(), MemoryState::Stopped);
    core.shutdown().await;
}

/// memory を設定しないときも、chat が点検に通らなければ区切りの呼び出しを止める。
#[tokio::test(flavor = "multi_thread")]
async fn without_llm_memory_a_chat_model_that_fails_the_probe_stops_segment_calls() {
    let dir = TempDir::new("memory-default-stop");
    let broken_chat = chat_server(
        r#"{"events": [{"reasoning": "考え中"}, {"done": {}}]},
           {"events": [{"delta": "1 2 3"}, {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#,
    );
    let (core, mut rx, _) = start(dir.path(), 1, broken_chat, None, true).await;
    assert!(matches!(core.memory_route(), MemoryRoute::Stopped));
    assert_eq!(memory_events(&mut rx), [MemoryState::Stopped]);
    core.shutdown().await;
}

/// 中身を差し替えられる llama-server (繋がらない → 繋がる)。
struct Swappable(Mutex<Arc<ReplayClient>>);

impl Swappable {
    fn now(&self) -> Arc<ReplayClient> {
        self.0.lock().unwrap().clone()
    }
}

impl LlmClient for Swappable {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        let c = self.now();
        Box::pin(async move { c.health().await })
    }
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.server_info().await })
    }
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.count_tokens(text).await })
    }
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.chat(req).await })
    }
}
