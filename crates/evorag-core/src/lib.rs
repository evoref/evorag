//! コアの API の実装と、sleep-time の進行 (02 §3、§5)。下の層の crate を組み立てる。
//!
//! v0.1 の M1.3 はヘッドレスのチャット経路 (04 §3):
//! 1. 発話を、LLM に送る前にターン (応答待ち) として確定する。確定できなければ送らない。
//! 2. 文脈を組み立てて (05 §4.1) 逐次表示で生成し、断片をイベントで届ける。
//! 3. 応答は、完了・中断・失敗のいずれかのターンとして、受け取ったところまでの本文と理由を書く。
//!
//! 命令は 1 本の待ち行列で順に処理する。生成は別の task で走らせ、中断の命令で取り消す。
//! 書き手のスレッドはここで作る (02 §4.3 の R7)。tokio の実行器の中で使う。

mod context;
pub mod localtime;
pub mod logging;
mod prompt;
mod recall;
mod views;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use evorag_llm::{
    ChatEvent, ChatMessage, ChatRequest, Generation, LlmClient, LlmError, Role, ServerInfo,
};
use evorag_memory::{AssistantTurn, GenerationRecord, SignalKind, TurnRole, TurnState, Writes};
use evorag_store::{
    Envelope, OpenError, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle,
    meta,
};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, Id, IdGen, Message, MessageKey};
use tokio::sync::{mpsc, oneshot};

pub use context::{TokenCache, estimate};
pub use prompt::PromptLanguage;

/// コアの設定。
pub struct CoreConfig {
    pub data_root: PathBuf,
    /// `config.toml` (移行の前の退避に含める、03 §7.2)。
    pub config_file: Option<PathBuf>,
    pub evorag_version: String,
    /// 設定 `prompt.language`。データ根に記録が無いときだけ使う (07 §6.1)。
    pub prompt_language: PromptLanguage,
    pub generation: Generation,
    /// 設定 `llm.chat.request_extra` (07 §4.2)。思考を切る `chat_template_kwargs` など。
    pub request_extra: evorag_llm::RequestExtra,
    /// 設定 `context.reserve_for_output` (05 §4.1)。
    pub reserve_for_output: Option<usize>,
    /// 設定 `storage.backup_dir` (03 §7.1)。省くとデータ根の `backups/`。
    pub backup_dir: Option<PathBuf>,
    /// 設定 `storage.allow_unsafe_location` (03 §2.2)。危ない置き場所でも警告だけにする。
    pub allow_unsafe_location: bool,
    /// 置き場所と空きを調べる関数 (試験で差し替える)。
    pub probe: Probe,
    /// 今の接続先の名前 (状態の行と `/model` の表示)。
    pub model_name: String,
    /// `/model` で切り替える候補と、繋ぐ関数 (FR-108)。
    pub models: Vec<ModelChoice>,
    pub connect: Option<Connector>,
    /// 設定 `memory.enabled` (05 §3、FR-409)。セッションごとに `/sources` で変えられる。
    pub memory_enabled: bool,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// 置き場所と空きを調べる関数 (03 §2.2、§8)。
#[derive(Clone, Copy)]
pub struct Probe {
    pub location: fn(&std::path::Path) -> evorag_store::location::Verdict,
    pub free_bytes: fn(&std::path::Path) -> Option<u64>,
}

impl Probe {
    /// OS に問い合わせる。
    pub fn system() -> Self {
        Self {
            location: evorag_store::location::check,
            free_bytes: evorag_store::location::free_bytes,
        }
    }
}

/// `/model` の候補 (設定 `[[llm.chat.models]]`、07 §4.2)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub name: String,
    pub url: String,
}

/// 接続先の URL から `LlmClient` を作る関数 (接続先の検査を含む)。
pub type Connector = Arc<dyn Fn(&str) -> Result<Arc<dyn LlmClient>, LlmError> + Send + Sync>;

/// 語彙の索引の状態 (doctor、03 §10)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    /// まだ無い (初回の起動で作る)。
    Missing,
    Ok {
        segments: usize,
        docs: u64,
    },
    /// 読めない・合わない (次の起動で作り直す)。
    NeedsRebuild(String),
}

/// 語彙の索引を読むだけで調べる。
pub fn inspect_index(data_root: &std::path::Path) -> IndexState {
    match evorag_index::lex::LexIndex::inspect(
        &recall::index_dir(data_root),
        evorag_retrieval::normalize::RULES_VERSION,
    ) {
        Ok(None) => IndexState::Missing,
        Ok(Some((segments, docs))) => IndexState::Ok { segments, docs },
        Err(e) => IndexState::NeedsRebuild(e.to_string()),
    }
}

/// 起動できない理由。
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// chat の llama-server に繋がらない (06 §3、FR-503。TUI を開かずに案内を出す)。
    #[error("llama-server is not reachable: {0}")]
    LlmUnreachable(LlmError),
    #[error(transparent)]
    Store(#[from] OpenError),
    #[error("the data root cannot be written: {0}")]
    ReadOnly(String),
    /// データ根か退避先が危ない置き場所 (ネットワーク・同期フォルダ、03 §2.2、FR-216)。
    #[error("unsafe location: {0}")]
    UnsafeLocation(String),
    #[error("storage error: {0}")]
    Storage(String),
}

/// 起動して分かったこと。
#[derive(Debug, Clone)]
pub struct StartReport {
    pub server: ServerInfo,
    pub store: evorag_store::OpenReport,
    pub prompt_language: PromptLanguage,
    /// 設定の `prompt.language` が記録と違った (記録した言語のまま動く、07 §6.1)。
    pub prompt_language_mismatch: bool,
    /// 置き場所の注意 (FAT / exFAT、または設定で許した危ない置き場所)。
    pub location_warnings: Vec<String>,
}

/// 生成の取り消しの理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cancel {
    User,
    Shutdown,
}

struct Running {
    cancel: Option<oneshot::Sender<Cancel>>,
    task: tokio::task::JoinHandle<()>,
    /// 応答を書き終えた (task はまだ後始末をしていることがある。終了のときは task を待つ)。
    done: bool,
}

#[derive(Default)]
struct State {
    session: Option<Id>,
    running: Option<Running>,
}

struct Inner {
    store: Arc<Store>,
    writer: WriterHandle,
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 今の接続先と、その能力 (`/model` で切り替える、FR-108)。
    conn: std::sync::RwLock<(Arc<dyn LlmClient>, ServerInfo, String)>,
    /// 切り替えの候補 (設定 `[[llm.chat.models]]`)。
    models: Vec<ModelChoice>,
    /// 候補の接続先に繋ぐ関数。
    connect: Option<Connector>,
    /// 語彙の索引 (開けなければ `None`。想起は Skip になる)。
    index: Option<Arc<evorag_index::lex::LexIndex>>,
    memory_enabled: bool,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    events: mpsc::UnboundedSender<Event>,
    state: Mutex<State>,
    tokens: TokenCache,
    generation: Generation,
    request_extra: evorag_llm::RequestExtra,
    reserve: Option<usize>,
    language: PromptLanguage,
    backup_dir: Option<PathBuf>,
    probe: Probe,
    /// 裏で走らせている退避・退避の整理の task (終了のときに待つ。ストアを持ったまま戻らないように)。
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// 命令の待ち行列を回す task と、その止め方。
    command_loop: Mutex<Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>>,
}

/// 動いているコア。
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
    commands: mpsc::UnboundedSender<Command>,
}

fn llm_message(e: &LlmError) -> Message {
    match e {
        LlmError::Connect(d) => Message::new(MessageKey::LlmConnect).arg("detail", d.clone()),
        LlmError::Http { status, body } => Message::new(MessageKey::LlmHttp)
            .arg("status", status.to_string())
            .arg("detail", body.clone()),
        LlmError::Cut(d) => Message::new(MessageKey::LlmStreamCut).arg("detail", d.clone()),
        LlmError::Server(d) => Message::new(MessageKey::LlmServer).arg("detail", d.clone()),
        other => Message::new(MessageKey::LlmProtocol).arg("detail", other.to_string()),
    }
}

impl Core {
    /// llama-server を確かめ (06 §3)、データ根を開き、書き手のスレッドを作る。
    pub async fn start(
        cfg: CoreConfig,
        llm: Arc<dyn LlmClient>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>, StartReport), StartError> {
        llm.health().await.map_err(StartError::LlmUnreachable)?;
        let server = llm
            .server_info()
            .await
            .map_err(StartError::LlmUnreachable)?;

        // 危ない置き場所は、既定で起動を拒否する (03 §2.2)。データ根と退避先の両方を調べる。
        let mut location_warnings = Vec::new();
        let mut places = vec![cfg.data_root.clone()];
        places.extend(cfg.backup_dir.clone());
        for place in &places {
            match (cfg.probe.location)(place) {
                evorag_store::location::Verdict::Ok => {}
                evorag_store::location::Verdict::Warn(w) => location_warnings.push(w),
                evorag_store::location::Verdict::Unsafe(u) if cfg.allow_unsafe_location => {
                    location_warnings.push(u);
                }
                evorag_store::location::Verdict::Unsafe(u) => {
                    return Err(StartError::UnsafeLocation(u));
                }
            }
        }

        let mut kinds = evorag_memory::kinds();
        kinds.sort_by_key(|k| k.name);
        let (store, open_report) = Store::open(OpenOptions {
            data_root: cfg.data_root,
            evorag_version: cfg.evorag_version,
            kinds,
            clock: cfg.clock.clone(),
            ids: cfg.ids.clone(),
            config_file: cfg.config_file,
        })?;
        if let Some(r) = store.read_only() {
            return Err(StartError::ReadOnly(format!("{r:?}")));
        }
        let store = Arc::new(store);
        let (writer, handle) =
            Writer::new(store.clone()).map_err(|e| StartError::Storage(e.to_string()))?;
        let writer_thread = std::thread::Builder::new()
            .name("evorag-writer".into())
            .spawn(move || writer.run())
            .map_err(|e| StartError::Storage(e.to_string()))?;

        // プロンプトの言語: 記録があればそれを使い、無ければ設定の値を記録する (07 §6.1)。
        let recorded = store
            .meta(meta::PROMPT_LANGUAGE)
            .map_err(|e| StartError::Storage(e.to_string()))?
            .and_then(|s| PromptLanguage::parse(&s));
        let language = match recorded {
            Some(l) => l,
            None => {
                handle
                    .submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaOnce {
                        key: meta::PROMPT_LANGUAGE.into(),
                        value: cfg.prompt_language.as_str().into(),
                    }]))
                    .map_err(|e| StartError::Storage(e.to_string()))?;
                cfg.prompt_language
            }
        };

        // 語彙の索引 (05 §2.3)。DB との突き合わせは裏で行う。
        let (index, rebuild_index) = recall::open_index(store.data_root());

        let (events, rx) = mpsc::unbounded_channel();
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            store,
            writer: handle,
            writer_thread: Mutex::new(Some(writer_thread)),
            conn: std::sync::RwLock::new((llm, server.clone(), cfg.model_name)),
            models: cfg.models,
            connect: cfg.connect,
            index,
            memory_enabled: cfg.memory_enabled,
            clock: cfg.clock,
            ids: cfg.ids,
            events,
            state: Mutex::default(),
            tokens: TokenCache::default(),
            generation: cfg.generation,
            request_extra: cfg.request_extra,
            reserve: cfg.reserve_for_output,
            language,
            backup_dir: cfg.backup_dir,
            probe: cfg.probe,
            background: Mutex::new(Vec::new()),
            command_loop: Mutex::new(None),
        });
        let core = Self { inner, commands };
        let worker = core.inner.clone();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    cmd = cmd_rx.recv() => match cmd {
                        Some(cmd) => handle_command(&worker, cmd).await,
                        None => break,
                    },
                }
            }
        });
        *core
            .inner
            .command_loop
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((stop_tx, task));
        // 自動の退避: v0.1 は起動時と終了時の空き時間に作る (03 §7.1)。会話の邪魔をしないよう裏で。
        // 前の起動で終わらなかった退避の整理を続けてから、今日の退避を作る (03 §7.3)。
        let worker = core.inner.clone();
        let task = tokio::task::spawn_blocking(move || {
            if let Some(ix) = &worker.index {
                let t = std::time::Instant::now();
                match recall::sync(&worker.store, ix, rebuild_index) {
                    Ok(()) => tracing::info!(
                        docs = ix.snapshot().len(),
                        rebuilt = rebuild_index,
                        ms = t.elapsed().as_millis() as u64,
                        "lexical index synced"
                    ),
                    Err(e) => tracing::warn!(error = %e.kind(), "lexical index sync failed"),
                }
            }
            let jobs = worker
                .store
                .read()
                .ok()
                .and_then(|r| evorag_memory::pending_purge_jobs(&r).ok())
                .unwrap_or_default();
            if !enough_space(&worker) {
                return;
            }
            for (job, ids) in jobs {
                purge(&worker, job, &ids);
            }
            match worker.store.daily_backup(worker.backup_dir.as_deref()) {
                Ok(Some(_)) => tracing::info!("daily backup written"),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "daily backup failed"),
            }
        });
        core.inner
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
        tracing::info!(
            version = %core.inner.store.evorag_version(),
            model_key = %server.model_key,
            n_ctx = server.n_ctx,
            slots = server.total_slots,
            index = core.inner.index.is_some(),
            "core started"
        );
        let report = StartReport {
            server,
            store: open_report,
            prompt_language: language,
            prompt_language_mismatch: language != cfg.prompt_language,
            location_warnings,
        };
        Ok((core, rx, report))
    }

    /// 命令を送る。結果はイベントで届く。
    pub fn command(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// 今の接続先の能力。
    pub fn server(&self) -> ServerInfo {
        self.inner.conn().1
    }

    /// 照会: セッションの一覧 (更新の新しい順)。
    pub fn sessions(&self, limit: usize) -> Vec<evorag_types::api::SessionView> {
        let Ok(rtx) = self.inner.store.read() else {
            return Vec::new();
        };
        evorag_memory::list_sessions(&rtx, limit)
            .unwrap_or_default()
            .iter()
            .map(views::session)
            .collect()
    }

    /// 照会: セッションのターン (元の順)。
    pub fn turns(&self, session: Id) -> Vec<evorag_types::api::TurnView> {
        let Ok(rtx) = self.inner.store.read() else {
            return Vec::new();
        };
        evorag_memory::session_turns(&rtx, session)
            .unwrap_or_default()
            .iter()
            .map(views::turn)
            .collect()
    }

    /// 試験のための注入: 次のジャーナルの書き込みを失敗させる (AC-FR-202-1)。
    #[doc(hidden)]
    pub fn inject_journal_failure(&self) {
        self.inner.store.fail_next_journal_append();
    }

    /// 試験のため: 裏の処理 (索引の更新・退避) が終わるまで待つ。
    #[doc(hidden)]
    pub async fn settle(&self) {
        loop {
            let tasks: Vec<_> = self
                .inner
                .background
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect();
            if tasks.is_empty() {
                return;
            }
            for t in tasks {
                let _ = t.await;
            }
        }
    }

    /// `/tokenize` を呼んだ回数 (キャッシュの試験のため)。
    pub fn tokenize_calls(&self) -> usize {
        self.inner.tokens.calls()
    }

    /// 終了する。生成中なら中断として書き、書き手の待ち行列の会話の追記を書き切ってから戻る (02 §5)。
    /// 戻った後、ほかに複製した `Core` が無ければ、データ根のロックも外れている。
    pub async fn shutdown(self) {
        // 新しい命令を受け付けない (処理中の命令は終わるまで待つ)。
        let command_loop = self
            .inner
            .command_loop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((stop, task)) = command_loop {
            let _ = stop.send(());
            let _ = task.await;
        }
        let running = {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.running.take()
        };
        if let Some(mut r) = running {
            if let Some(c) = r.cancel.take() {
                let _ = c.send(Cancel::Shutdown);
            }
            let _ = r.task.await;
        }
        let background: Vec<_> = self
            .inner
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for t in background {
            let _ = t.await;
        }
        // 今日の退避がまだなら作る (読み取りのトランザクションで写すので、書き手と並んでよい)。
        let worker = self.inner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if enough_space(&worker) {
                let _ = worker.store.daily_backup(worker.backup_dir.as_deref());
            }
        })
        .await;
        self.inner.writer.shutdown();
        let thread = self
            .inner
            .writer_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(t) = thread {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
    }
}

impl Inner {
    /// 今の接続先と能力と名前の写し。
    fn conn(&self) -> (Arc<dyn LlmClient>, ServerInfo, String) {
        self.conn.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn model_info(inner: &Inner) -> Event {
    let (_, server, name) = inner.conn();
    Event::ModelInfo {
        current: name,
        model: server.model_name,
        n_ctx: server.n_ctx,
        choices: inner.models.iter().map(|m| m.name.clone()).collect(),
    }
}

/// `/model` (FR-108): 名前が無ければ今のモデルと候補を示し、あれば候補に繋ぎ直す。
async fn switch_model(inner: &Arc<Inner>, name: Option<String>) {
    let Some(name) = name else {
        emit(inner, model_info(inner));
        return;
    };
    if is_running(inner) {
        reject(inner, MessageKey::Busy);
        return;
    }
    let (Some(choice), Some(connect)) =
        (inner.models.iter().find(|m| m.name == name), &inner.connect)
    else {
        emit(
            inner,
            Event::Rejected {
                error: Message::new(MessageKey::UnknownModel).arg("name", name),
            },
        );
        return;
    };
    let connected = async {
        let llm = connect(&choice.url)?;
        llm.health().await?;
        let server = llm.server_info().await?;
        Ok::<_, LlmError>((llm, server))
    }
    .await;
    match connected {
        Ok((llm, server)) => {
            *inner.conn.write().unwrap_or_else(|e| e.into_inner()) =
                (llm, server, choice.name.clone());
            emit(inner, model_info(inner));
        }
        // 繋がらなければ、今のモデルのまま理由を知らせる。
        Err(e) => emit(
            inner,
            Event::Rejected {
                error: llm_message(&e),
            },
        ),
    }
}

fn emit(inner: &Inner, ev: Event) {
    let _ = inner.events.send(ev);
}

fn reject(inner: &Inner, key: MessageKey) {
    emit(
        inner,
        Event::Rejected {
            error: Message::new(key),
        },
    );
}

/// 書き手に送り、結果を待つ。
async fn write(inner: &Inner, ops: Vec<WriteOp>) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    inner
        .writer
        .submit(
            WriteRequest::conversation(ops),
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        )
        .map_err(|e| e.to_string())?;
    match rx.await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the writer stopped".into()),
    }
}

fn read_env(inner: &Inner, kind: &str, id: Id) -> Option<Envelope> {
    match inner.store.read().ok()?.get(kind, id).ok()? {
        Some(Read::Ok(e)) => Some(e),
        _ => None,
    }
}

fn writes(inner: &Inner) -> Writes<'_> {
    Writes {
        clock: &*inner.clock,
        ids: &*inner.ids,
    }
}

fn is_running(inner: &Inner) -> bool {
    inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running
        .as_ref()
        .is_some_and(|r| !r.done && !r.task.is_finished())
}

fn current_session(inner: &Inner) -> Option<Id> {
    inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session
}

async fn handle_command(inner: &Arc<Inner>, cmd: Command) {
    match cmd {
        Command::NewSession { title } => {
            let Ok((id, ops)) = writes(inner).create_session(&title) else {
                return;
            };
            if let Err(e) = write(inner, ops).await {
                emit(
                    inner,
                    Event::Rejected {
                        error: Message::new(MessageKey::ResponseNotSaved).arg("detail", e),
                    },
                );
                return;
            }
            open_session(inner, id);
        }
        Command::OpenSession { id } => open_session(inner, id),
        Command::Send { text } => send(inner, text).await,
        Command::Interrupt => {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = st.running.as_mut().and_then(|r| r.cancel.take()) {
                let _ = c.send(Cancel::User);
            }
        }
        Command::Retry => regenerate(inner, true).await,
        Command::Resend => regenerate(inner, false).await,
        Command::RenameSession { title } => rename(inner, title).await,
        Command::DeleteSession => delete(inner).await,
        Command::Rate { good, reason } => rate(inner, good, reason).await,
        Command::Model { name } => switch_model(inner, name).await,
        Command::Sources { memory } => sources(inner, memory).await,
        Command::ShowSource { turn, n } => show_source(inner, turn, n),
        _ => {}
    }
}

async fn rename(inner: &Arc<Inner>, title: String) {
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let Some(session) = read_env(inner, evorag_memory::KIND_SESSION, sid) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let Ok(ops) = writes(inner).rename_session(&session, &title) else {
        return;
    };
    if let Err(e) = write(inner, ops).await {
        emit(
            inner,
            Event::Rejected {
                error: Message::new(MessageKey::ResponseNotSaved).arg("detail", e),
            },
        );
        return;
    }
    if let Ok(rtx) = inner.store.read()
        && let Ok(s) = evorag_memory::load_session(&rtx, sid)
    {
        emit(
            inner,
            Event::SessionRenamed {
                session: views::session(&s),
            },
        );
    }
}

async fn delete(inner: &Arc<Inner>) {
    if is_running(inner) {
        reject(inner, MessageKey::Busy);
        return;
    }
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let ops = inner.store.read().ok().and_then(|rtx| {
        let s = evorag_memory::load_session(&rtx, sid).ok()?;
        evorag_memory::delete_session(&rtx, &s.envelope).ok()
    });
    let Some(mut ops) = ops else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    // 退避からも除く (03 §7.3)。途中で止まっても続けられるように、job を同じ依頼で書く。
    let ids = evorag_memory::deleted_ids(&ops);
    let Ok((job, put)) = evorag_memory::purge_job(&writes(inner), ids.clone()) else {
        return;
    };
    ops.push(put);
    if let Err(e) = write(inner, ops).await {
        emit(
            inner,
            Event::Rejected {
                error: Message::new(MessageKey::DeleteFailed).arg("detail", e),
            },
        );
        return;
    }
    // 以後の想起に出ないよう、知らせる前に索引から除く (AC-FR-309-1)。
    if let Some(ix) = inner.index.clone() {
        let gone: std::collections::HashSet<Id> = ids.iter().copied().collect();
        let _ = tokio::task::spawn_blocking(move || ix.remove(&gone)).await;
    }
    inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session = None;
    emit(inner, Event::SessionDeleted { id: sid });
    let worker = inner.clone();
    let task = tokio::task::spawn_blocking(move || purge(&worker, job, &ids));
    inner
        .background
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(task);
}

/// 退避と退避の整理の前に、空きが DB の大きさ以上あるかを確かめる (03 §7.1、§8)。足りなければ
/// 裏の処理を止めて知らせる (会話の追記は続ける、FR-217)。
fn enough_space(inner: &Inner) -> bool {
    let root = inner.store.data_root();
    let need = std::fs::metadata(root.join(evorag_store::DB_FILE)).map_or(0, |m| m.len());
    let place = inner
        .backup_dir
        .clone()
        .unwrap_or_else(|| root.to_path_buf());
    match (inner.probe.free_bytes)(&place) {
        Some(free) if free < need => {
            emit(
                inner,
                Event::BackgroundPaused {
                    reason: Message::new(MessageKey::LowSpace)
                        .arg("free", (free / (1024 * 1024)).to_string())
                        .arg("need", need.div_ceil(1024 * 1024).to_string()),
                },
            );
            false
        }
        _ => true,
    }
}

/// 退避の整理の job を行い、終えたら job を消す。失敗したら job を残す (次の起動で続ける)。
fn purge(inner: &Inner, job: Id, ids: &[Id]) {
    if !enough_space(inner) {
        return;
    }
    let root = inner.store.data_root().to_path_buf();
    if evorag_store::restore::purge_backups(&root, inner.backup_dir.as_deref(), ids).is_ok() {
        let _ = inner
            .writer
            .submit_wait(WriteRequest::background(vec![WriteOp::Delete {
                kind: evorag_memory::KIND_JOB.into(),
                id: job,
            }]));
    }
}

async fn rate(inner: &Arc<Inner>, good: bool, reason: Option<String>) {
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let turns = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::session_turns(&r, sid).ok())
        .unwrap_or_default();
    let Some(target) = turns
        .iter()
        .rev()
        .find(|t| t.body.role == TurnRole::Assistant)
        .map(|t| t.envelope.id)
    else {
        reject(inner, MessageKey::NothingToRate);
        return;
    };
    let kind = if good {
        SignalKind::Good
    } else {
        SignalKind::Bad
    };
    let Ok((_, ops)) = writes(inner).signal(kind, target, reason) else {
        return;
    };
    if write(inner, ops).await.is_ok() {
        emit(inner, Event::Rated { good });
    }
}

fn open_session(inner: &Inner, id: Id) {
    let Ok(rtx) = inner.store.read() else {
        return;
    };
    let Ok(s) = evorag_memory::load_session(&rtx, id) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let turns = evorag_memory::session_turns(&rtx, id).unwrap_or_default();
    inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session = Some(id);
    emit(
        inner,
        Event::SessionOpened {
            session: views::session(&s),
            turns: turns.iter().map(views::turn).collect(),
        },
    );
}

async fn send(inner: &Arc<Inner>, text: String) {
    if is_running(inner) {
        reject(inner, MessageKey::Busy);
        return;
    }
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let Some(session) = read_env(inner, evorag_memory::KIND_SESSION, sid) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    // 1. 発話を先に確定する (04 §3 の 1、FR-201)。確定できなければ送らない。
    let (uid, ops) = match writes(inner).user_turn(&session, &text) {
        Ok(v) => v,
        Err(e) => {
            emit(
                inner,
                Event::SendRejected {
                    text,
                    error: Message::new(MessageKey::SendNotSaved).arg("detail", e.to_string()),
                },
            );
            return;
        }
    };
    if let Err(e) = write(inner, ops).await {
        emit(
            inner,
            Event::SendRejected {
                text,
                error: Message::new(MessageKey::SendNotSaved).arg("detail", e),
            },
        );
        return;
    }
    let Some(user) = read_env(inner, evorag_memory::KIND_TURN, uid) else {
        return;
    };
    if let Ok(t) = user.body_as::<evorag_memory::TurnBody>() {
        emit(
            inner,
            Event::UserTurnSaved {
                turn: views::turn_of(&user, &t),
            },
        );
    }
    index_turns(inner, vec![uid]);
    start_generation(inner, sid, uid);
}

/// 作り直す (`retry`) か、送り直す (`resend`)。対象は最後の発話。
async fn regenerate(inner: &Arc<Inner>, retry: bool) {
    if is_running(inner) {
        reject(inner, MessageKey::Busy);
        return;
    }
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    let turns = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::session_turns(&r, sid).ok())
        .unwrap_or_default();
    let Some(last_user) = turns.iter().rev().find(|t| t.body.role == TurnRole::User) else {
        reject(inner, MessageKey::NothingToRetry);
        return;
    };
    let uid = last_user.envelope.id;
    let last_answer = turns
        .iter()
        .rev()
        .find(|t| t.body.reply_to == Some(uid))
        .map(|t| t.envelope.id);
    if retry {
        // 前の応答を残し、再生成の信号を記録する (FR-103、13 §2)。
        let Some(target) = last_answer else {
            reject(inner, MessageKey::NothingToRetry);
            return;
        };
        if let Ok((_, ops)) = writes(inner).signal(SignalKind::Retry, target, None) {
            let _ = write(inner, ops).await;
        }
    }
    start_generation(inner, sid, uid);
}

/// 書いたターンを裏で索引に足す。
fn index_turns(inner: &Arc<Inner>, ids: Vec<Id>) {
    let Some(ix) = inner.index.clone() else {
        return;
    };
    let store = inner.store.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _ = recall::add_turns(&store, &ix, &ids);
    });
    let mut bg = inner.background.lock().unwrap_or_else(|e| e.into_inner());
    bg.retain(|t| !t.is_finished());
    bg.push(task);
}

/// このセッションでメモリを添えるか。
fn memory_on(inner: &Inner, session: Id) -> bool {
    read_env(inner, evorag_memory::KIND_SESSION, session)
        .and_then(|e| e.body_as::<evorag_memory::SessionBody>().ok())
        .and_then(|b| b.memory)
        .unwrap_or(inner.memory_enabled)
}

/// `/sources [on|off]` (FR-409)。
async fn sources(inner: &Arc<Inner>, memory: Option<bool>) {
    let Some(sid) = current_session(inner) else {
        reject(inner, MessageKey::NoSession);
        return;
    };
    if let Some(on) = memory {
        let Some(session) = read_env(inner, evorag_memory::KIND_SESSION, sid) else {
            reject(inner, MessageKey::NoSession);
            return;
        };
        let Ok(ops) = writes(inner).set_session_memory(&session, on) else {
            return;
        };
        if let Err(e) = write(inner, ops).await {
            emit(
                inner,
                Event::Rejected {
                    error: Message::new(MessageKey::ResponseNotSaved).arg("detail", e),
                },
            );
            return;
        }
    }
    emit(
        inner,
        Event::SourcesInfo {
            memory: memory_on(inner, sid),
        },
    );
}

/// 応答の出典の詳細 (FR-110)。
fn show_source(inner: &Inner, turn: Id, n: usize) {
    let found = read_env(inner, evorag_memory::KIND_TURN, turn)
        .and_then(|e| e.body_as::<evorag_memory::TurnBody>().ok())
        .and_then(|b| recall::source_turns(&inner.store, &b, n));
    let Some(turns) = found else {
        emit(
            inner,
            Event::Rejected {
                error: Message::new(MessageKey::SourceNotFound).arg("n", n.to_string()),
            },
        );
        return;
    };
    let title = recall::session_title(&inner.store, turns[0].body.session).unwrap_or_default();
    let lines = turns
        .iter()
        .map(|t| (views::role(t.body.role), t.body.text.clone()))
        .collect();
    emit(
        inner,
        Event::SourceDetail {
            n,
            title,
            at: turns[0].envelope.created_at,
            lines,
        },
    );
}

fn start_generation(inner: &Arc<Inner>, session: Id, user_turn: Id) {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        generate(&worker, session, user_turn, cancel_rx).await;
    });
    let previous = inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running
        .replace(Running {
            cancel: Some(cancel_tx),
            task,
            done: false,
        });
    // 前の生成の task は終わりかけている。終了のときに待てるように残す。
    if let Some(p) = previous {
        inner
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(p.task);
    }
}

/// 発話 `user_turn` までの会話 (この発話への前の応答は除く) を、LLM に送る発言の列にする。
fn history(inner: &Inner, session: Id, user_turn: Id) -> Vec<ChatMessage> {
    let turns = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::session_turns(&r, session).ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    for t in &turns {
        if t.envelope.id > user_turn {
            break;
        }
        let role = match t.body.role {
            TurnRole::User => Role::User,
            TurnRole::Assistant => Role::Assistant,
            TurnRole::System => continue,
        };
        // 失敗した応答と、今作り直す発話への前の応答は送らない。
        if t.body.state == TurnState::Failed || t.body.reply_to == Some(user_turn) {
            continue;
        }
        out.push(ChatMessage::new(role, t.body.text.clone()));
    }
    out
}

/// ログに出す失敗の分類 (llama-server のエラーの本文は出さない)。
fn outcome_error_kind(o: &Outcome) -> String {
    match o {
        Outcome::Failed(LlmError::Http { status, .. }) => format!("http {status}"),
        Outcome::Failed(LlmError::Connect(_)) => "connect".into(),
        Outcome::Failed(LlmError::Cut(_)) => "cut".into(),
        Outcome::Failed(LlmError::Server(_)) => "server".into(),
        Outcome::Failed(_) => "protocol".into(),
        _ => String::new(),
    }
}

enum Outcome {
    Completed,
    Interrupted(Cancel),
    Failed(LlmError),
}

async fn generate(
    inner: &Arc<Inner>,
    session: Id,
    user_turn: Id,
    mut cancel: oneshot::Receiver<Cancel>,
) {
    emit(
        inner,
        Event::ResponseStarted {
            reply_to: user_turn,
        },
    );
    let started_at = std::time::Instant::now();
    // 応答ごとの短い追跡 ID (発話の ID の末尾、02 §7)。
    let trace = format!("{user_turn}")
        .chars()
        .rev()
        .take(8)
        .collect::<String>();
    let (llm, server, _) = inner.conn();
    let n_ctx = server.n_ctx as usize;
    let reserve = context::reserve_for_output(n_ctx, inner.reserve);
    // 以前の会話を引く (05 §3)。このセッションでメモリを切っていれば引かない (FR-409)。
    let memory = if memory_on(inner, session) {
        Some(
            recall::attach(
                &inner.store,
                inner.index.as_deref(),
                inner.language,
                &*llm,
                &inner.tokens,
                &server.model_key,
                n_ctx,
                reserve,
                session,
                user_turn,
            )
            .await,
        )
    } else {
        None
    };
    let mut system_text = prompt::system_prompt(inner.language, inner.clock.now().as_millis());
    if let Some(s) = memory.as_ref().and_then(|m| m.section.as_deref()) {
        system_text.push_str(s);
    }
    let system = ChatMessage::new(Role::System, system_text);
    let messages = context::assemble(
        &*llm,
        &inner.tokens,
        &server.model_key,
        n_ctx,
        reserve,
        system,
        history(inner, session, user_turn),
    )
    .await;
    let req = ChatRequest {
        messages,
        generation: inner.generation.clone(),
        // 応答は最初のスロット (06 §4)。
        id_slot: (server.total_slots > 1).then_some(0),
        model: None,
        extra: inner.request_extra.clone(),
    };

    let mut text = String::new();
    let mut thinking = false;
    let outcome = tokio::select! {
        c = &mut cancel => Outcome::Interrupted(c.unwrap_or(Cancel::User)),
        started = llm.chat(req) => match started {
            Err(e) => Outcome::Failed(e),
            Ok(mut stream) => loop {
                tokio::select! {
                    c = &mut cancel => {
                        // ストリームを落として接続を閉じる (06 §4)。
                        drop(stream);
                        break Outcome::Interrupted(c.unwrap_or(Cancel::User));
                    }
                    ev = stream.next() => match ev {
                        Some(Ok(ChatEvent::Delta(d))) => {
                            text.push_str(&d);
                            emit(inner, Event::Token { reply_to: user_turn, text: d });
                        }
                        Some(Ok(ChatEvent::Reasoning(_))) => {
                            // 思考の本文は見せも保存もしない。考えていることだけを 1 回知らせる (07 §2.2)。
                            if !thinking && text.is_empty() {
                                thinking = true;
                                emit(inner, Event::Thinking { reply_to: user_turn });
                            }
                        }
                        Some(Ok(ChatEvent::Done { .. })) => break Outcome::Completed,
                        Some(Err(e)) => break Outcome::Failed(e),
                        None => break Outcome::Failed(LlmError::Cut("the stream ended".into())),
                    }
                }
            },
        },
    };

    // 本文は出さない。長さ・件数・時間・分類だけ (NFR-502)。
    tracing::info!(
        trace = %trace,
        outcome = match &outcome {
            Outcome::Completed => "completed",
            Outcome::Interrupted(Cancel::User) => "interrupted",
            Outcome::Interrupted(Cancel::Shutdown) => "shutdown",
            Outcome::Failed(_) => "failed",
        },
        error = outcome_error_kind(&outcome),
        decision = memory.as_ref().map_or("off", |m| match m.record.decision {
            evorag_memory::RecallDecision::Fire => "fire",
            evorag_memory::RecallDecision::Abstain => "abstain",
            evorag_memory::RecallDecision::Skip => "skip",
        }),
        attached = memory.as_ref().map_or(0, |m| m.attached.len()),
        reply_chars = text.chars().count(),
        ms = started_at.elapsed().as_millis() as u64,
        "response"
    );
    let (state, notice, error) = match &outcome {
        Outcome::Completed => (TurnState::Completed, None, None),
        Outcome::Interrupted(c) => {
            let key = match c {
                Cancel::User => MessageKey::Interrupted,
                Cancel::Shutdown => MessageKey::InterruptedByShutdown,
            };
            (
                TurnState::Interrupted,
                Some(Message::new(key)),
                Some(format!("interrupted:{c:?}").to_lowercase()),
            )
        }
        Outcome::Failed(e) => (TurnState::Failed, Some(llm_message(e)), Some(e.to_string())),
    };
    let g = &inner.generation;
    let turn = AssistantTurn {
        state,
        text,
        model_key: Some(server.model_key.clone()),
        generation: Some(GenerationRecord {
            temperature: g.temperature,
            top_p: g.top_p,
            max_tokens: g.max_tokens,
            seed: g.seed,
        }),
        attached: memory
            .as_ref()
            .map(|m| m.attached.clone())
            .unwrap_or_default(),
        error,
        recall: memory.map(|m| m.record),
    };
    // 応答を書く。セッションと発話は読み直した最新のものを使う。
    let session_env = read_env(inner, evorag_memory::KIND_SESSION, session);
    let user_env = read_env(inner, evorag_memory::KIND_TURN, user_turn);
    let written = match (session_env, user_env) {
        (Some(s), Some(u)) => match writes(inner).assistant_turn(&s, &u, turn) {
            Ok((id, ops)) => write(inner, ops).await.map(|()| id),
            Err(e) => Err(e.to_string()),
        },
        _ => Err("the session or the utterance is gone".into()),
    };
    // 終わりのイベントを出す前に「生成中」を解く (イベントを受けてすぐ次を送っても断られないように)。
    // task の handle は残し、終了のときに待つ。
    if let Some(r) = inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running
        .as_mut()
    {
        r.done = true;
    }
    match written
        .ok()
        .and_then(|id| read_env(inner, evorag_memory::KIND_TURN, id))
    {
        Some(env) => {
            index_turns(inner, vec![env.id]);
            if let Ok(t) = env.body_as::<evorag_memory::TurnBody>() {
                emit(
                    inner,
                    Event::ResponseFinished {
                        turn: views::turn_of(&env, &t),
                        notice,
                    },
                );
            }
        }
        None => emit(
            inner,
            Event::Rejected {
                error: Message::new(MessageKey::ResponseNotSaved).arg("detail", "write failed"),
            },
        ),
    }
}
