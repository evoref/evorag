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
mod prompt;
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
    /// 設定 `context.reserve_for_output` (05 §4.1)。
    pub reserve_for_output: Option<usize>,
    /// 設定 `storage.backup_dir` (03 §7.1)。省くとデータ根の `backups/`。
    pub backup_dir: Option<PathBuf>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
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
    llm: Arc<dyn LlmClient>,
    server: ServerInfo,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    events: mpsc::UnboundedSender<Event>,
    state: Mutex<State>,
    tokens: TokenCache,
    generation: Generation,
    reserve: Option<usize>,
    language: PromptLanguage,
    backup_dir: Option<PathBuf>,
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

        let (events, rx) = mpsc::unbounded_channel();
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            store,
            writer: handle,
            writer_thread: Mutex::new(Some(writer_thread)),
            llm,
            server: server.clone(),
            clock: cfg.clock,
            ids: cfg.ids,
            events,
            state: Mutex::default(),
            tokens: TokenCache::default(),
            generation: cfg.generation,
            reserve: cfg.reserve_for_output,
            language,
            backup_dir: cfg.backup_dir,
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
            let jobs = worker
                .store
                .read()
                .ok()
                .and_then(|r| evorag_memory::pending_purge_jobs(&r).ok())
                .unwrap_or_default();
            for (job, ids) in jobs {
                purge(&worker, job, &ids);
            }
            let _ = worker.store.daily_backup(worker.backup_dir.as_deref());
        });
        core.inner
            .background
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
        let report = StartReport {
            server,
            store: open_report,
            prompt_language: language,
            prompt_language_mismatch: language != cfg.prompt_language,
        };
        Ok((core, rx, report))
    }

    /// 命令を送る。結果はイベントで届く。
    pub fn command(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    pub fn server(&self) -> &ServerInfo {
        &self.inner.server
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
        let store = self.inner.store.clone();
        let dir = self.inner.backup_dir.clone();
        let _ = tokio::task::spawn_blocking(move || store.daily_backup(dir.as_deref())).await;
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

/// 退避の整理の job を行い、終えたら job を消す。失敗したら job を残す (次の起動で続ける)。
fn purge(inner: &Inner, job: Id, ids: &[Id]) {
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
    let system = ChatMessage::new(
        Role::System,
        prompt::system_prompt(inner.language, inner.clock.now().as_millis()),
    );
    let n_ctx = inner.server.n_ctx as usize;
    let reserve = context::reserve_for_output(n_ctx, inner.reserve);
    let messages = context::assemble(
        &*inner.llm,
        &inner.tokens,
        &inner.server.model_key,
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
        id_slot: (inner.server.total_slots > 1).then_some(0),
        model: None,
    };

    let mut text = String::new();
    let outcome = tokio::select! {
        c = &mut cancel => Outcome::Interrupted(c.unwrap_or(Cancel::User)),
        started = inner.llm.chat(req) => match started {
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
                        Some(Ok(ChatEvent::Reasoning(_))) => {}
                        Some(Ok(ChatEvent::Done { .. })) => break Outcome::Completed,
                        Some(Err(e)) => break Outcome::Failed(e),
                        None => break Outcome::Failed(LlmError::Cut("the stream ended".into())),
                    }
                }
            },
        },
    };

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
        model_key: Some(inner.server.model_key.clone()),
        generation: Some(GenerationRecord {
            temperature: g.temperature,
            top_p: g.top_p,
            max_tokens: g.max_tokens,
            seed: g.seed,
        }),
        attached: Vec::new(),
        error,
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
