//! モデルの記録と照合 (06 §3.1、FR-512、ADR-0018)。llama-server は記録の再生で置き換える (09 §3.3)。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evorag_core::{Core, CoreConfig, PromptLanguage, StartError, StartReport};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, ServerInfo};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, RoleStatus};
use evorag_types::{ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const OK: &str = r#"{"events": [{"delta": "了解"}, {"done": {}}]}"#;

/// `file` のモデルを動かしている llama-server。`size` を変えると別のモデルになる。
fn server(file: &str, size: u64, replies: usize) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/{file}", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": {size}, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![OK; replies].join(", ")
    ))
    .unwrap()
}

fn config(root: &Path, seed: u64, ignored: Vec<(String, String)>) -> CoreConfig {
    config_with(root, seed, ignored, None, None)
}

fn config_with(
    root: &Path,
    seed: u64,
    ignored: Vec<(String, String)>,
    embed: Option<Arc<dyn LlmClient>>,
    rerank: Option<Arc<dyn LlmClient>>,
) -> CoreConfig {
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
        ignored_config: ignored,
        embed,
        rerank,
        aux_request_extra: Default::default(),
        probe_models: false,
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

async fn start(
    root: &Path,
    llm: Arc<dyn LlmClient>,
    seed: u64,
    ignored: Vec<(String, String)>,
) -> Result<(Core, UnboundedReceiver<Event>, StartReport), StartError> {
    let r = Core::start(config(root, seed, ignored), llm).await;
    if let Ok((core, _, _)) = &r {
        core.settle().await;
    }
    r
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

/// verifies: AC-FR-512-2, AC-FR-108-2
#[tokio::test(flavor = "multi_thread")]
async fn the_first_start_records_the_chat_model_and_model_only_shows_it() {
    let dir = TempDir::new("binding-first");
    let llm = Arc::new(server("qwen-a.gguf", 2, 0));
    let (core, mut rx, report) = start(dir.path(), llm.clone(), 1, Vec::new()).await.unwrap();
    assert_eq!(report.chat_model_recorded, Some(false));

    core.command(Command::Model { name: None });
    let ev = until(&mut rx, |e| matches!(e, Event::ModelInfo { .. })).await;
    let Some(Event::ModelInfo { roles, n_ctx }) = ev.last() else {
        panic!()
    };
    assert_eq!(*n_ctx, 8192);
    // chat・embed・rerank の順。embed と rerank は設定していない。
    assert_eq!(roles.len(), 3);
    assert_eq!(roles[0].role, "chat");
    assert_eq!(roles[1].status, RoleStatus::Unset);
    assert_eq!(roles[0].recorded, roles[0].connected);
    assert_eq!(roles[0].recorded.as_ref().unwrap().0, "qwen-a.gguf");

    // 名前を付けても切り替えない。切り替えは setup で行うことを示す。
    core.command(Command::Model {
        name: Some("other".into()),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    let Some(Event::Rejected { error }) = ev.last() else {
        panic!()
    };
    assert_eq!(error.key, MessageKey::ModelSwitchInSetup);
    core.shutdown().await;

    // 同じモデルで起動し直すと、記録し直さない。
    let (core, _, report) = start(dir.path(), llm, 2, Vec::new()).await.unwrap();
    assert_eq!(report.chat_model_recorded, None);
    core.shutdown().await;
}

/// verifies: AC-FR-512-1 (コアの層。終了コード 6 と案内は bin で出す)
#[tokio::test(flavor = "multi_thread")]
async fn a_different_chat_model_stops_the_start_and_says_whether_it_may_be_the_same() {
    let dir = TempDir::new("binding-mismatch");
    let (core, _, _) = start(
        dir.path(),
        Arc::new(server("qwen-a.gguf", 2, 0)),
        1,
        Vec::new(),
    )
    .await
    .unwrap();
    core.shutdown().await;

    // ファイル名だけが違う (meta は同じ): 同じモデルの可能性が高い。
    match start(
        dir.path(),
        Arc::new(server("renamed.gguf", 2, 0)),
        2,
        Vec::new(),
    )
    .await
    {
        Err(StartError::ModelMismatch {
            recorded,
            connected,
        }) => {
            assert_eq!(recorded.file_name, "qwen-a.gguf");
            assert_eq!(connected.file_name, "renamed.gguf");
            assert!(evorag_core::same_meta(&recorded, &connected));
        }
        Err(e) => panic!("expected ModelMismatch, got {e}"),
        Ok(_) => panic!("expected ModelMismatch, got a core"),
    }
    // 中身も違う。
    match start(
        dir.path(),
        Arc::new(server("qwen-a.gguf", 9, 0)),
        3,
        Vec::new(),
    )
    .await
    {
        Err(StartError::ModelMismatch {
            recorded,
            connected,
        }) => assert!(!evorag_core::same_meta(&recorded, &connected)),
        Err(e) => panic!("expected ModelMismatch, got {e}"),
        Ok(_) => panic!("expected ModelMismatch, got a core"),
    }
    // 記録は書き換わっていない: 元のモデルなら起動できる。
    let (core, _, report) = start(
        dir.path(),
        Arc::new(server("qwen-a.gguf", 2, 0)),
        4,
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.chat_model_recorded, None);
    core.shutdown().await;
}

/// 起動の後に、llama-server が別のモデルで起動し直されたことにする。
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

/// verifies: AC-FR-512-3
#[tokio::test(flavor = "multi_thread")]
async fn a_model_swapped_while_running_is_noticed_before_sending() {
    let dir = TempDir::new("binding-swap");
    let a = Arc::new(server("qwen-a.gguf", 2, 1));
    let llm = Arc::new(Swappable(Mutex::new(a.clone())));
    let (core, mut rx, _) = start(dir.path(), llm.clone(), 1, Vec::new()).await.unwrap();
    core.command(Command::NewSession {
        title: "試験".into(),
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::Send {
        text: "一つ目".into(),
    });
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;

    let b = Arc::new(server("other.gguf", 7, 1));
    *llm.0.lock().unwrap() = b.clone();
    core.command(Command::Send {
        text: "二つ目".into(),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::SendRejected { .. })).await;
    let Some(Event::SendRejected { text, error }) = ev.last() else {
        panic!()
    };
    // 送らず、入力した文章を返す。
    assert_eq!(text, "二つ目");
    assert_eq!(error.key, MessageKey::ModelChanged);
    assert!(b.requests().is_empty());
    core.shutdown().await;
}

/// v0.1 のデータ根 (記録が無い) を、いつもと違うモデルで初めて開くと、そのことを知らせる。
/// verifies: AC-FR-512-2 (いつもと違うモデル)
#[tokio::test(flavor = "multi_thread")]
async fn recording_a_model_unlike_the_usual_one_is_flagged() {
    let dir = TempDir::new("binding-unusual");
    {
        // 応答を 1 つ書いてから、記録を消す (v0.1 で作ったデータ根のふり)。
        let (core, mut rx, _) = start(
            dir.path(),
            Arc::new(server("usual.gguf", 2, 1)),
            1,
            Vec::new(),
        )
        .await
        .unwrap();
        core.command(Command::NewSession {
            title: "試験".into(),
        });
        until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
        core.command(Command::Send {
            text: "やあ".into(),
        });
        until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
        core.shutdown().await;
        drop_bindings(dir.path());
    }
    let (core, _, report) = start(
        dir.path(),
        Arc::new(server("trial.gguf", 5, 0)),
        2,
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.chat_model_recorded, Some(true));
    core.shutdown().await;
}

fn drop_bindings(root: &Path) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 9)),
        config_file: None,
    })
    .unwrap();
    let store = Arc::new(store);
    let ids: Vec<_> = evorag_memory::load_bindings(&store.read().unwrap())
        .unwrap()
        .by_role
        .values()
        .map(|b| b.envelope.id)
        .collect();
    let (writer, handle) = evorag_store::Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    handle
        .submit_wait(evorag_store::WriteRequest::conversation(
            ids.into_iter()
                .map(|id| evorag_store::WriteOp::Delete {
                    kind: evorag_memory::KIND_MODEL_BINDING.into(),
                    id,
                })
                .collect(),
        ))
        .unwrap();
    handle.shutdown();
    t.join().unwrap();
}

/// verifies: AC-FR-108-2 (使わなくなった設定のキーは、内容が変わったときだけ知らせる)
#[tokio::test(flavor = "multi_thread")]
async fn ignored_config_keys_are_announced_once_per_content() {
    let dir = TempDir::new("binding-ignored");
    let llm: Arc<dyn LlmClient> = Arc::new(server("qwen-a.gguf", 2, 0));
    let models = || vec![("llm.chat.models".to_owned(), "h1".to_owned())];
    let (core, _, report) = start(dir.path(), llm.clone(), 1, models()).await.unwrap();
    assert_eq!(report.ignored_config, ["llm.chat.models"]);
    core.shutdown().await;
    let (core, _, report) = start(dir.path(), llm.clone(), 2, models()).await.unwrap();
    assert!(report.ignored_config.is_empty());
    core.shutdown().await;
    let changed = vec![("llm.chat.models".to_owned(), "h2".to_owned())];
    let (core, _, report) = start(dir.path(), llm, 3, changed).await.unwrap();
    assert_eq!(report.ignored_config, ["llm.chat.models"]);
    core.shutdown().await;
}

/// embed・rerank の役割のサーバ。`healthy` でなければ繋がらない。
fn aux_server(file: &str, size: u64, healthy: bool) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/{file}", "total_slots": 4,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "aux", "meta": {{"n_params": 5, "size": {size}, "n_vocab": 7}}}}]}},
          "health": {healthy},
          "embed": {{"dim": 8}},
          "rerank": true
        }}"#
    ))
    .unwrap()
}

async fn start_with(
    root: &Path,
    seed: u64,
    embed: Option<Arc<dyn LlmClient>>,
    rerank: Option<Arc<dyn LlmClient>>,
) -> (Core, UnboundedReceiver<Event>, StartReport) {
    let r = Core::start(
        config_with(root, seed, Vec::new(), embed, rerank),
        Arc::new(server("qwen-a.gguf", 2, 0)),
    )
    .await
    .unwrap();
    r.0.settle().await;
    r
}

/// verifies: AC-FR-512-2 (embed・rerank の記録), AC-FR-512-1 (記録と違う embed は使わない), AC-FR-502-1
#[tokio::test(flavor = "multi_thread")]
async fn embed_and_rerank_are_recorded_and_a_different_one_is_not_used() {
    let dir = TempDir::new("binding-aux");
    let embed: Arc<dyn LlmClient> = Arc::new(aux_server("embed-a.gguf", 1, true));
    let rerank: Arc<dyn LlmClient> = Arc::new(aux_server("rerank-a.gguf", 2, true));
    let (core, mut rx, report) =
        start_with(dir.path(), 1, Some(embed.clone()), Some(rerank.clone())).await;
    assert_eq!(report.recorded_roles, ["embed", "rerank"]);
    assert!(core.role_ready("embed") && core.role_ready("rerank"));
    core.command(Command::Model { name: None });
    let ev = until(&mut rx, |e| matches!(e, Event::ModelInfo { .. })).await;
    let Some(Event::ModelInfo { roles, .. }) = ev.last() else {
        panic!()
    };
    let names: Vec<_> = roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(names, ["chat", "embed", "rerank"]);
    assert!(roles.iter().all(|r| r.status == RoleStatus::Ready));
    core.shutdown().await;

    // 埋め込みのモデルを替えても起動は止まらない。替えた役割は使わず、知らせる。
    let other: Arc<dyn LlmClient> = Arc::new(aux_server("embed-b.gguf", 9, true));
    let (core, _, report) = start_with(dir.path(), 2, Some(other), Some(rerank.clone())).await;
    assert!(report.recorded_roles.is_empty());
    let embed_info = report.roles.iter().find(|r| r.role == "embed").unwrap();
    assert_eq!(embed_info.status, RoleStatus::Mismatch);
    assert_eq!(embed_info.recorded.as_ref().unwrap().0, "embed-a.gguf");
    assert_eq!(embed_info.connected.as_ref().unwrap().0, "embed-b.gguf");
    assert!(!core.role_ready("embed"));
    assert!(core.role_ready("rerank"));
    core.shutdown().await;

    // 設定していない役割は「未設定」。
    let (core, _, report) = start_with(dir.path(), 3, None, None).await;
    assert!(report.roles.iter().all(|r| r.status == RoleStatus::Unset));
    core.shutdown().await;
}

/// 起動のときに繋がらなかった役割は、使わずに動き、繋がったら記録して使い始める (06 §3)。
/// verifies: AC-FR-512-2 (後から繋がった役割)
#[tokio::test(flavor = "multi_thread")]
async fn a_role_that_was_down_at_start_is_used_once_it_comes_up() {
    let dir = TempDir::new("binding-aux-wait");
    let rerank = Arc::new(Swappable(Mutex::new(Arc::new(aux_server(
        "rerank-a.gguf",
        2,
        false,
    )))));
    let (core, mut rx, report) = start_with(dir.path(), 1, None, Some(rerank.clone())).await;
    let info = report.roles.iter().find(|r| r.role == "rerank").unwrap();
    assert_eq!(info.status, RoleStatus::Waiting);
    assert!(!core.role_ready("rerank"));
    assert!(core.recheck_roles().await, "still down");

    *rerank.0.lock().unwrap() = Arc::new(aux_server("rerank-a.gguf", 2, true));
    assert!(!core.recheck_roles().await);
    let ev = until(&mut rx, |e| matches!(e, Event::RoleChanged { .. })).await;
    let Some(Event::RoleChanged { info, recorded_now }) = ev.last() else {
        panic!()
    };
    assert_eq!(
        (info.role.as_str(), info.status),
        ("rerank", RoleStatus::Ready)
    );
    assert!(recorded_now);
    assert!(core.role_ready("rerank"));
    core.shutdown().await;
}
