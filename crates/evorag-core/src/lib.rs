//! コアの API の実装と、sleep-time の進行 (02 §3、§5)。下の層の crate を組み立てる。
//!
//! v0.1 の M1.3 はヘッドレスのチャット経路 (04 §3):
//! 1. 発話を、LLM に送る前にターン (応答待ち) として確定する。確定できなければ送らない。
//! 2. 文脈を組み立てて (05 §4.1) 逐次表示で生成し、断片をイベントで届ける。
//! 3. 応答は、完了・中断・失敗のいずれかのターンとして、受け取ったところまでの本文と理由を書く。
//!
//! 命令は 1 本の待ち行列で順に処理する。生成は別の task で走らせ、中断の命令で取り消す。
//! 書き手のスレッドはここで作る (02 §4.3 の R7)。tokio の実行器の中で使う。

pub mod calibration;
pub(crate) mod embedding;
pub mod power;
mod sleep;
pub use sleep::{OnBattery, SleepConfig};
mod context;
pub mod localtime;
pub mod logging;
pub mod probe;
mod prompt;
pub mod rebind;
mod recall;
mod roles;
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
use evorag_types::api::{Command, Event, RecallState, RoleStatus};
use evorag_types::{Clock, Id, IdGen, Message, MessageKey};
use tokio::sync::{mpsc, oneshot};

pub use context::{TokenCache, estimate};
pub use prompt::{AnswerDefaults, AnswerFormat, AnswerLanguage, AnswerLength, PromptLanguage};

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
    /// 読んで無視した設定のキーと、その内容のハッシュ (`llm.chat.models` など、07 §4.1)。
    /// 同じ内容を前に知らせていれば、知らせない。
    pub ignored_config: Vec<(String, String)>,
    /// embed と rerank の役割の llama-server (設定 `llm.embed`・`llm.rerank`、06 §2)。無ければ `None`。
    pub embed: Option<Arc<dyn LlmClient>>,
    pub rerank: Option<Arc<dyn LlmClient>>,
    /// 設定 `llm.chat.aux_request_extra` (補助の呼び出しに足す JSON、06 §6)。
    pub aux_request_extra: evorag_llm::RequestExtra,
    /// 裏の処理で、モデルの点検とリランカーの較正を行う (06 §3.3、05 §3.7)。試験では切る。
    pub probe_models: bool,
    /// 裏の処理の設定 (07 §4.2 の `memory.sleep_time`・`idle_seconds`・`on_battery`)。
    pub sleep: SleepConfig,
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
    /// 電源の状態 (裏の処理の電池の設定、04 §4)。
    pub power: fn() -> power::Power,
}

impl Probe {
    /// OS に問い合わせる。
    pub fn system() -> Self {
        Self {
            location: evorag_store::location::check,
            free_bytes: evorag_store::location::free_bytes,
            power: power::system,
        }
    }
}

/// 繋がっているモデルの元の値 (06 §3.1)。
pub fn origin(server: &ServerInfo) -> evorag_memory::ModelOrigin {
    evorag_memory::ModelOrigin {
        model_key: server.model_key.clone(),
        file_name: evorag_llm::file_name(&server.model_path).to_owned(),
        model_name: server.model_name.clone(),
        n_params: server.meta.n_params,
        size: server.meta.size,
        n_vocab: server.meta.n_vocab,
        build_info: server.build_info.clone(),
    }
}

/// ファイル名だけが違い、`meta` の値がすべて同じか (同じモデルの可能性が高い、06 §3)。
pub fn same_meta(a: &evorag_memory::ModelOrigin, b: &evorag_memory::ModelOrigin) -> bool {
    a.n_params.is_some()
        && a.size.is_some()
        && a.n_vocab.is_some()
        && (a.n_params, a.size, a.n_vocab) == (b.n_params, b.size, b.n_vocab)
}

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
    /// 応答のモデルが、データ根に記録したモデルと違う (06 §3、FR-512)。画面を開かずに案内を出す。
    #[error("the chat model differs from the recorded one ({} != {})", connected.model_key, recorded.model_key)]
    ModelMismatch {
        recorded: Box<evorag_memory::ModelOrigin>,
        connected: Box<evorag_memory::ModelOrigin>,
    },
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
    /// この起動で、応答のモデルを初めて記録した (06 §3.1)。`Some(true)` は、過去の応答で最も
    /// 多く使われたモデルと違う (たまたま試しのモデルで起動したかもしれない)。
    pub chat_model_recorded: Option<bool>,
    /// 知らせる、読んで無視した設定のキー (07 §4.1)。
    pub ignored_config: Vec<String>,
    /// embed と rerank の役割の状態 (06 §3)。
    pub roles: Vec<evorag_types::api::ModelRoleInfo>,
    /// この起動でモデルを初めて記録した役割 (embed・rerank)。
    pub recorded_roles: Vec<String>,
    /// 想起の状態 (07 §2.1)。
    pub recall: evorag_types::api::RecallState,
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
    /// 今の接続先と、その能力と名前。
    conn: std::sync::RwLock<(Arc<dyn LlmClient>, ServerInfo, String)>,
    /// 記録した応答のモデル (06 §3.1)。送る前に、繋がっているモデルと照合する。
    chat_model: evorag_memory::ModelOrigin,
    /// embed と rerank の役割 (06 §3)。
    aux: Mutex<Vec<roles::AuxRole>>,
    /// 繋がらない役割を試し直す task と、点検の task (終了のときに止める)。
    aux_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 裏の処理 (sleep-time) の task (終了のときに止める)。
    sleep_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    sleep: sleep::SleepConfig,
    /// 利用者の操作 (裏の処理を止め、アイドルの判定に使う、04 §4)。
    activity: Activity,
    /// 裏の処理の状態 (状態の行)。
    background_state: Mutex<evorag_types::api::BackgroundState>,
    /// 裏の処理の手順が動いている。
    working: std::sync::atomic::AtomicBool,
    /// 較正が最後に失敗した時刻。
    calib_failed_at: Mutex<Option<std::time::Instant>>,
    /// 起動時の突き合わせと退避が済んだ (sleep-time の退避は、その後で日をまたいだときだけ)。
    startup_done: std::sync::atomic::AtomicBool,
    /// 今の埋め込みのモデルのベクトルの索引 (05 §2.2)。
    vectors: Mutex<Option<embedding::Vectors>>,
    /// ベクトルの索引を DB と突き合わせ終えた埋め込みのモデル (それまでは埋め込みの手順を動かさない)。
    vectors_opened: Mutex<Option<String>>,
    /// 埋め込みの保存の鍵 (保存が終わるまで、次の回の埋め込みを始めない)。
    embed_gate: Arc<tokio::sync::Mutex<()>>,
    /// 終えたのにまだ要るので待たせている裏の処理の手順と、その時刻 (04 §4 の「繰り返しを防ぐ」)。
    backoff: Mutex<std::collections::HashMap<sleep::Step, std::time::Instant>>,
    /// 途中まで済んだ較正 (リランカーの model_key と、済んだ問いの結果)。裏の処理で落とされたら続きから。
    calib_progress: Mutex<Option<(String, CalibProgress)>>,
    aux_request_extra: evorag_llm::RequestExtra,
    /// 裏で点検と較正を行う (06 §3.3、05 §3.7。設定 `probe_models`)。
    background_checks: bool,
    /// 今のリランカーのしきい値の記録 (今の同梱の事例で求めたものだけ、05 §3.7)。
    threshold: Mutex<Option<evorag_memory::ThresholdBody>>,

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
    /// 答え方の既定 (07 §3 の `/style`)。データ根の `setting` から読み、選び直したらすぐ効かせる。
    answer: Mutex<prompt::AnswerDefaults>,
    backup_dir: Option<PathBuf>,
    probe: Probe,
    /// 裏で走らせている退避・退避の整理の task (終了のときに待つ。ストアを持ったまま戻らないように)。
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// 命令の待ち行列を回す task と、その止め方。
    command_loop: Mutex<Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>>,
}

/// 途中まで済んだ較正の、問いごとの結果。
type CalibProgress = Arc<Mutex<Vec<calibration::CaseResult>>>;

/// 利用者の操作 (04 §4)。最後の操作の時刻と、裏の処理へ知らせる口。
pub(crate) struct Activity {
    last: Mutex<tokio::time::Instant>,
    /// 操作があった (動いている手順を止める)。
    touched: tokio::sync::Notify,
    /// 見直してほしい (役割が繋がった、操作があった)。
    kick: tokio::sync::Notify,
    generation: std::sync::atomic::AtomicU64,
    /// 画面の外で入力している (外部のエディタ)。
    pub(crate) held: std::sync::atomic::AtomicBool,
}

impl Activity {
    fn new() -> Self {
        Self {
            last: Mutex::new(tokio::time::Instant::now()),
            touched: tokio::sync::Notify::new(),
            kick: tokio::sync::Notify::new(),
            generation: std::sync::atomic::AtomicU64::new(0),
            held: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn touch(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = tokio::time::Instant::now();
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.touched.notify_waiters();
        self.kick.notify_one();
    }

    /// 操作の世代 (操作のたびに増える)。手順を始める前に覚え、待つ準備をした後に変わっていれば止める
    /// (`notify_waiters` は待っている者にしか届かないので、その間の操作を取りこぼさないため)。
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn touched_notify(&self) -> &tokio::sync::Notify {
        &self.touched
    }

    fn kick(&self) {
        self.kick.notify_one();
    }

    /// 最後の操作からの時間。
    pub(crate) fn since(&self) -> std::time::Duration {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    pub(crate) async fn kicked(&self) {
        self.kick.notified().await;
    }
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

        // 応答のモデルを、記録と照合する (06 §3.1)。違えば、何も書かずに止める。
        let connected = origin(&server);
        let bindings = store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_bindings(&r).ok())
            .unwrap_or_default();
        let recorded_chat = bindings
            .by_role
            .get(evorag_memory::ROLE_CHAT)
            .map(|b| b.body.current.clone());
        if let Some(r) = &recorded_chat
            && r.model_key != connected.model_key
        {
            return Err(StartError::ModelMismatch {
                recorded: Box::new(r.clone()),
                connected: Box::new(connected),
            });
        }
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

        // 記録が無ければ (新しいデータ根、v0.1 のデータ根)、繋がっているモデルを記録する (06 §3.1)。
        let mut chat_model_recorded = None;
        if recorded_chat.is_none() {
            let unusual = most_used_model_key(&store).is_some_and(|k| k != connected.model_key);
            let w = Writes {
                clock: &*cfg.clock,
                ids: &*cfg.ids,
            };
            let ops = w
                .bind_model(&bindings, evorag_memory::ROLE_CHAT, connected.clone())
                .map_err(|e| StartError::Storage(e.to_string()))?;
            handle
                .submit_wait(WriteRequest::conversation(ops))
                .map_err(|e| StartError::Storage(e.to_string()))?;
            chat_model_recorded = Some(unusual);
        }

        // 読んで無視した設定 (07 §4.1)。同じ内容を前に知らせていれば、知らせない。
        let ignored_config =
            ignored_config_notices(&store, &handle, &cfg.ignored_config, &*cfg.clock, &*cfg.ids);

        // embed と rerank の役割 (06 §3)。記録と違えば使わず、繋がらなければ後で試す。
        let mut aux = vec![
            roles::AuxRole::new(evorag_memory::ROLE_EMBED, cfg.embed.clone(), &bindings),
            roles::AuxRole::new(evorag_memory::ROLE_RERANK, cfg.rerank.clone(), &bindings),
        ];
        let mut recorded_roles = Vec::new();
        for a in &mut aux {
            let Some(client) = a.client.clone() else {
                continue;
            };
            match roles::check(&*client, a.recorded.as_ref()).await {
                roles::Checked::Waiting => a.state = roles::RoleState::Waiting,
                roles::Checked::Mismatch(o) => a.state = roles::RoleState::Mismatch(o),
                roles::Checked::Ready { origin, record } => {
                    if record {
                        let w = Writes {
                            clock: &*cfg.clock,
                            ids: &*cfg.ids,
                        };
                        let ops = w
                            .bind_model(&bindings, a.role, origin.clone())
                            .map_err(|e| StartError::Storage(e.to_string()))?;
                        handle
                            .submit_wait(WriteRequest::conversation(ops))
                            .map_err(|e| StartError::Storage(e.to_string()))?;
                        a.recorded = Some(origin.clone());
                        recorded_roles.push(a.role.to_owned());
                    }
                    a.state = roles::RoleState::Ready(origin);
                }
            }
        }
        let waiting = aux
            .iter()
            .any(|a| matches!(a.state, roles::RoleState::Waiting));
        let role_infos: Vec<_> = aux.iter().map(roles::AuxRole::info).collect();

        // 答え方の既定 (07 §3)。読めなければ既定 (すべて auto)。
        let answer = store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_settings(&r).ok())
            .map(|s| prompt::AnswerDefaults::from_settings(&s))
            .unwrap_or_default();

        // 語彙の索引 (05 §2.3)。DB との突き合わせは裏で行う。
        let (index, rebuild_index) = recall::open_index(store.data_root());

        let (events, rx) = mpsc::unbounded_channel();
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            store,
            writer: handle,
            writer_thread: Mutex::new(Some(writer_thread)),
            conn: std::sync::RwLock::new((llm, server.clone(), cfg.model_name)),
            chat_model: recorded_chat.unwrap_or(connected),
            aux: Mutex::new(aux),
            aux_task: Mutex::new(None),
            sleep_task: Mutex::new(None),
            sleep: cfg.sleep,
            activity: Activity::new(),
            background_state: Mutex::new(evorag_types::api::BackgroundState::Idle),
            working: std::sync::atomic::AtomicBool::new(false),
            calib_failed_at: Mutex::new(None),
            calib_progress: Mutex::new(None),
            vectors: Mutex::new(None),
            vectors_opened: Mutex::new(None),
            embed_gate: Arc::new(tokio::sync::Mutex::new(())),
            backoff: Mutex::new(std::collections::HashMap::new()),
            startup_done: std::sync::atomic::AtomicBool::new(false),
            aux_request_extra: cfg.aux_request_extra,
            background_checks: cfg.probe_models,
            threshold: Mutex::new(None),

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
            answer: Mutex::new(answer),
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
        let task = tokio::spawn(async move {
            // チャンクと語彙の索引を DB と突き合わせ、ベクトルの索引を開く (05 §2.2〜§2.4)。
            sync_chunks(&worker, rebuild_index).await;
            embedding::open(&worker).await;
            let done = worker.clone();
            let _ = tokio::task::spawn_blocking(move || {
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
                backup_now(&worker);
            })
            .await;
            done.startup_done
                .store(true, std::sync::atomic::Ordering::SeqCst);
            done.activity.kick();
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
        core.inner.load_threshold();
        let report = StartReport {
            server,
            store: open_report,
            prompt_language: language,
            prompt_language_mismatch: language != cfg.prompt_language,
            location_warnings,
            chat_model_recorded,
            ignored_config,
            roles: role_infos,
            recorded_roles,
            recall: core.inner.recall_state(),
        };
        // 裏の処理 (04 §4)。利用者が操作していない間に、要る手順を動かす。
        let worker = core.inner.clone();
        let task = tokio::spawn(sleep::run(worker));
        *core
            .inner
            .sleep_task
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(task);
        if waiting {
            let worker = core.inner.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(roles::RETRY).await;
                    if !recheck_roles(&worker).await {
                        break;
                    }
                }
            });
            *core
                .inner
                .aux_task
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(task);
        }
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

    /// embed・rerank の役割が使える (記録と同じモデルに繋がっている) か (06 §3)。
    pub fn role_ready(&self, role: &str) -> bool {
        self.inner
            .aux
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|a| a.role == role)
            .and_then(roles::AuxRole::ready_client)
            .is_some()
    }

    /// 利用者が操作した (キー入力、発話の送信)。動いている裏の処理を止め、アイドルの判定を始め直す (04 §4)。
    pub fn touch(&self) {
        self.inner.activity.touch();
    }

    /// 利用者が画面の外で入力している (外部のエディタ) 間は、裏の処理を出さない (06 §4)。
    pub fn hold(&self, on: bool) {
        self.inner
            .activity
            .held
            .store(on, std::sync::atomic::Ordering::SeqCst);
        self.inner.activity.touch();
    }

    /// 試験のため: 裏の処理の要る手順が無くなるまで待つ (10 秒で終わらなければ panic する)。
    #[doc(hidden)]
    pub async fn settle_background(&self) {
        for _ in 0..500 {
            if !sleep::pending(&self.inner)
                && !self.inner.working.load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "background work did not settle within 10 seconds: {:?}",
            sleep::pending_steps(&self.inner)
        );
    }

    /// 試験のため: 較正が終わるまで待つ。
    #[doc(hidden)]
    pub async fn settle_calibration(&self) {
        self.settle_background().await;
    }

    /// 今のリランカーのしきい値の記録 (05 §3.7)。
    pub fn threshold(&self) -> Option<evorag_memory::ThresholdBody> {
        self.inner
            .threshold
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 試験のため: 点検が終わるまで待つ。
    #[doc(hidden)]
    pub async fn settle_probe(&self) {
        self.settle_background().await;
    }

    /// 保存した点検の結果 (今のモデルの分、06 §3.3)。
    pub fn probe_result(&self, role: &str) -> Option<evorag_memory::ModelProbeBody> {
        let (model_key, build_info) = match role {
            evorag_memory::ROLE_CHAT => {
                let s = self.inner.conn().1;
                (s.model_key, s.build_info)
            }
            _ => {
                let aux = self.inner.aux.lock().unwrap_or_else(|e| e.into_inner());
                let a = aux.iter().find(|a| a.role == role)?;
                match &a.state {
                    roles::RoleState::Ready(o) => (o.model_key.clone(), o.build_info.clone()),
                    _ => return None,
                }
            }
        };
        let rtx = self.inner.store.read().ok()?;
        evorag_memory::load_probe(&rtx, role, &model_key, &build_info)
            .ok()
            .flatten()
            .map(|(_, b)| b)
    }

    /// 試験のため: 繋がらない役割を、待たずに今すぐ試し直す (06 §3)。まだ繋がらない役割があれば `true`。
    #[doc(hidden)]
    pub async fn recheck_roles(&self) -> bool {
        recheck_roles(&self.inner).await
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
        for slot in [&self.inner.aux_task, &self.inner.sleep_task] {
            let t = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(t) = t {
                t.abort();
                // 落ちるまで待つ (task が持つストアとデータ根のロックを、戻る前に手放すため)。
                let _ = t.await;
            }
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

/// 過去の応答で最も多く使われたモデル (記録の無いデータ根で、初めて記録するときに比べる、06 §3.1)。
fn most_used_model_key(store: &Store) -> Option<String> {
    let rtx = store.read().ok()?;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let _ = rtx.for_each(evorag_memory::KIND_TURN, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<evorag_memory::TurnBody>()
            && let Some(k) = b.model_key
        {
            *counts.entry(k).or_default() += 1;
        }
        true
    });
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k)
}

/// 読んで無視した設定のうち、前に知らせていない (内容が変わった) キー。知らせたことを記録する。
fn ignored_config_notices(
    store: &Store,
    handle: &evorag_store::WriterHandle,
    ignored: &[(String, String)],
    clock: &dyn Clock,
    ids: &dyn IdGen,
) -> Vec<String> {
    let current = store
        .read()
        .ok()
        .and_then(|r| evorag_memory::load_settings(&r).ok())
        .unwrap_or_default();
    let seen = current
        .by_name
        .get(evorag_memory::SETTING_IGNORED_CONFIG)
        .map(|s| s.body.value.clone())
        .unwrap_or_default();
    let value = ignored
        .iter()
        .map(|(k, h)| format!("{k}={h}"))
        .collect::<Vec<_>>()
        .join(";");
    let fresh: Vec<String> = ignored
        .iter()
        .filter(|(k, h)| !seen.split(';').any(|e| e == format!("{k}={h}")))
        .map(|(k, _)| k.clone())
        .collect();
    if value != seen && !(value.is_empty() && seen.is_empty()) {
        let w = Writes { clock, ids };
        if let Ok(ops) = w.set_setting(&current, evorag_memory::SETTING_IGNORED_CONFIG, &value) {
            let _ = handle.submit_wait(WriteRequest::conversation(ops));
        }
    }
    fresh
}

/// まだ点検していないモデル (役割・model_key・`build_info` ごと) を点検して保存する (06 §3.3、裏の処理の手順)。
/// 使えない機能が見つかれば知らせる。chat の点検は、裏の処理と同じく最後のスロットを使う (06 §4)。
/// 役割ごとに保存するので、途中で落とされても、済んだ役割は次の機会に飛ばす。
pub(crate) async fn probe_unprobed(inner: &Arc<Inner>) {
    let server = inner.conn().1;
    let slot = (server.total_slots > 1).then(|| server.total_slots - 1);
    for (role, client, key, build) in probe_targets(inner) {
        let old = inner
            .store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_probe(&r, role, &key, &build).ok())
            .flatten();
        if old.is_some() {
            continue;
        }
        let body = match role {
            evorag_memory::ROLE_CHAT => {
                probe::chat(&*client, &key, &build, &inner.aux_request_extra, slot).await
            }
            evorag_memory::ROLE_EMBED => probe::embed(&*client, &key, &build).await,
            _ => probe::rerank(&*client, &key, &build).await,
        };
        tracing::info!(
            role,
            prompt_tps = body.prompt_tps,
            gen_tps = body.gen_tps,
            structured_ok = body.structured_ok,
            thinking_off = body.thinking_off,
            dims = body.dims,
            embed_ms = body.embed_ms,
            rerank6_ms = body.rerank6_ms,
            failed = body.error.is_some(),
            "model probed"
        );
        // 保存と知らせは、落とされない task で行う (記録だけが残って、知らせが出ないことが無いように)。
        let worker = inner.clone();
        let _ = tokio::spawn(async move { save_probe_result(&worker, role, body).await }).await;
    }
}

/// 点検の結果を保存し、使えない機能が見つかれば知らせる。
async fn save_probe_result(
    inner: &Arc<Inner>,
    role: &'static str,
    body: evorag_memory::ModelProbeBody,
) {
    {
        if let Ok(ops) = writes(inner).save_probe(None, &body) {
            let _ = write_derived(inner, ops).await;
        }
        let found = probe::issues(&body);
        if !found.is_empty() {
            emit(
                inner,
                Event::ProbeIssues {
                    role: role.to_owned(),
                    thinking_not_off: found.contains(&probe::ProbeIssue::ThinkingNotOff),
                    structured_broken: found.contains(&probe::ProbeIssue::StructuredBroken),
                },
            );
        }
    }
}

/// 繋がらない役割を試し直す (06 §3)。繋がったら記録と照合し、記録が無ければ記録する。
/// まだ繋がらない役割があれば `true`。
async fn recheck_roles(inner: &Arc<Inner>) -> bool {
    let waiting: Vec<(
        usize,
        Arc<dyn LlmClient>,
        Option<evorag_memory::ModelOrigin>,
    )> = inner
        .aux
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a.state, roles::RoleState::Waiting))
        .filter_map(|(i, a)| Some((i, a.client.clone()?, a.recorded.clone())))
        .collect();
    let mut still = false;
    for (i, client, recorded) in waiting {
        let checked = roles::check(&*client, recorded.as_ref()).await;
        let mut recorded_now = false;
        let state = match checked {
            roles::Checked::Waiting => {
                still = true;
                continue;
            }
            roles::Checked::Mismatch(o) => roles::RoleState::Mismatch(o),
            roles::Checked::Ready { origin, record } => {
                if record {
                    let role = inner.aux.lock().unwrap_or_else(|e| e.into_inner())[i].role;
                    let bindings = inner
                        .store
                        .read()
                        .ok()
                        .and_then(|r| evorag_memory::load_bindings(&r).ok())
                        .unwrap_or_default();
                    let ops = writes(inner).bind_model(&bindings, role, origin.clone());
                    let written = match ops {
                        Ok(ops) => write(inner, ops).await.is_ok(),
                        Err(_) => false,
                    };
                    if !written {
                        // 記録できなければ、使わずに次の機会に試す。
                        still = true;
                        continue;
                    }
                    recorded_now = true;
                }
                roles::RoleState::Ready(origin)
            }
        };
        let info = {
            let mut aux = inner.aux.lock().unwrap_or_else(|e| e.into_inner());
            if let roles::RoleState::Ready(o) = &state
                && recorded_now
            {
                aux[i].recorded = Some(o.clone());
            }
            aux[i].state = state;
            aux[i].info()
        };
        let rerank_ready =
            info.role == evorag_memory::ROLE_RERANK && info.status == RoleStatus::Ready;
        if info.role == evorag_memory::ROLE_EMBED && info.status == RoleStatus::Ready {
            embedding::open(inner).await;
            inner.activity.kick();
        }
        emit(inner, Event::RoleChanged { info, recorded_now });
        if rerank_ready {
            inner.load_threshold();
            emit(
                inner,
                Event::RecallStatus {
                    state: inner.recall_state(),
                },
            );
            inner.activity.kick();
        }
    }
    still
}

impl Inner {
    /// 今のリランカー (使えるもの) と、その model_key。
    fn reranker(&self) -> Option<(Arc<dyn LlmClient>, String)> {
        let aux = self.aux.lock().unwrap_or_else(|e| e.into_inner());
        let a = aux.iter().find(|a| a.role == evorag_memory::ROLE_RERANK)?;
        match (&a.state, a.ready_client()) {
            (roles::RoleState::Ready(o), Some(c)) => Some((c, o.model_key.clone())),
            _ => None,
        }
    }

    /// 応答の経路のリランクの使い方 (05 §3.4・§3.5)。点検で測った往復 6 件の時間が締切を超えるときは
    /// 往復の数を減らし、3 件でも超えるならリランクしない (`rerank_slow`)。
    fn rerank_mode(&self) -> recall::RerankMode {
        let found = {
            let aux = self.aux.lock().unwrap_or_else(|e| e.into_inner());
            aux.iter()
                .find(|a| a.role == evorag_memory::ROLE_RERANK)
                .and_then(|a| match (&a.state, a.ready_client()) {
                    (roles::RoleState::Ready(o), Some(c)) => Some((c, o.clone())),
                    _ => None,
                })
        };
        let Some((client, origin)) = found else {
            return recall::RerankMode::Lexical(evorag_memory::DEGRADED_NO_RERANK);
        };
        let ms = self
            .store
            .read()
            .ok()
            .and_then(|r| {
                evorag_memory::load_probe(
                    &r,
                    evorag_memory::ROLE_RERANK,
                    &origin.model_key,
                    &origin.build_info,
                )
                .ok()
            })
            .flatten()
            .and_then(|(_, p)| p.rerank6_ms);
        let deadline = recall::RERANK_DEADLINE.as_millis() as f64;
        let top = match ms {
            Some(ms) if f64::from(ms) > deadline => {
                let n = (deadline / (f64::from(ms) / 6.0)).floor() as usize;
                if n < 3 {
                    return recall::RerankMode::Lexical(evorag_memory::DEGRADED_RERANK_SLOW);
                }
                n
            }
            _ => evorag_retrieval::calibrate::TOP,
        };
        let threshold = self
            .threshold
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|b| calibration::usable(b).map(|(t, band)| (t, band, b.version)));
        recall::RerankMode::Use(recall::RerankUse {
            client,
            key: origin.model_key,
            threshold,
            top,
        })
    }

    /// 今のリランカーのしきい値の記録を読み直す (今の同梱の事例で求めたものだけ)。
    fn load_threshold(&self) {
        let lang = self.language.as_str();
        let body = self.reranker().and_then(|(_, key)| {
            let r = self.store.read().ok()?;
            let (_, b) =
                evorag_memory::load_threshold(&r, &key, evorag_memory::TARGET_CONVERSATION)
                    .ok()??;
            calibration::is_current(&b, lang).then_some(b)
        });
        *self.threshold.lock().unwrap_or_else(|e| e.into_inner()) = body;
    }

    /// 較正を待っているか、している (裏の処理で求める、05 §3.7)。
    fn calibrating(&self) -> bool {
        self.background_checks && sleep::checks_allowed(self) && needs_calibration(self)
    }

    /// 想起の状態 (07 §2.1)。
    fn recall_state(&self) -> RecallState {
        self.recall_state_with(self.calibrating())
    }

    fn recall_state_with(&self, calibrating: bool) -> RecallState {
        if self.reranker().is_none() {
            return RecallState::Simple;
        }
        if let Some(percent) = embedding::rebuilding(self) {
            return RecallState::Rebuilding { percent };
        }
        match &*self.threshold.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(b) if b.passed => RecallState::Standard,
            Some(_) => RecallState::CandidatesOnly {
                calibrating: false,
                failed: true,
            },
            None => RecallState::CandidatesOnly {
                calibrating,
                failed: false,
            },
        }
    }
}

/// リランカーがあって、しきい値の記録が無いか (05 §3.7 の「いつ」、04 §4 の「要る作り直しは状態から求める」)。
/// 失敗した直後 (5 分) は求め直さない (繋がらないリランカーに繰り返し送らないため)。
pub(crate) fn needs_calibration(inner: &Inner) -> bool {
    let failed_recently = inner
        .calib_failed_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(300));
    !failed_recently
        && inner.reranker().is_some()
        && inner
            .threshold
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
}

/// しきい値を求めて記録する (裏の処理の手順、05 §3.7、AC-FR-413-3)。途中で落とされたら、済んだ問いの
/// 結果を覚えておき、次は続きから求める。保存と反映は落とされない task で行う (記録だけが残り、
/// 反映されずに求め直すことが無いように)。
pub(crate) async fn calibrate_now(inner: &Arc<Inner>) {
    let Some((rerank, key)) = inner.reranker() else {
        return;
    };
    let done = {
        let mut p = inner
            .calib_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &*p {
            Some((k, d)) if *k == key => d.clone(),
            _ => {
                let d = Arc::new(Mutex::new(Vec::new()));
                *p = Some((key.clone(), d.clone()));
                d
            }
        }
    };
    let version = inner
        .store
        .read()
        .ok()
        .and_then(|r| {
            evorag_memory::load_threshold(&r, &key, evorag_memory::TARGET_CONVERSATION).ok()
        })
        .flatten()
        .map_or(1, |(_, b)| b.version + 1);
    let lang = inner.language.as_str();
    let started = std::time::Instant::now();
    let result = calibration::calibrate(&*rerank, &key, lang, version, &done, &mut |_, _| {}).await;
    *inner
        .calib_progress
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    match result {
        Ok(body) => {
            tracing::info!(
                passed = body.passed,
                threshold = body.threshold,
                band_low = body.band_low,
                version = body.version,
                ms = started.elapsed().as_millis() as u64,
                "reranker calibrated"
            );
            // 先に反映してから、落とされない task で保存する。
            *inner.threshold.lock().unwrap_or_else(|e| e.into_inner()) = Some(body.clone());
            let worker = inner.clone();
            let _ = tokio::spawn(async move {
                if let Ok(ops) = writes(&worker).save_threshold(&body) {
                    let _ = write_derived(&worker, ops).await;
                }
            })
            .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "reranker calibration failed");
            *inner
                .calib_failed_at
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        }
    }
    emit(
        inner,
        Event::RecallStatus {
            state: inner.recall_state_with(false),
        },
    );
}

/// まだ点検していないモデルがあるか (06 §3.3)。
pub(crate) fn needs_probe(inner: &Inner) -> bool {
    probe_targets(inner).iter().any(|(role, _, key, build)| {
        inner
            .store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_probe(&r, role, key, build).ok())
            .flatten()
            .is_none()
    })
}

/// 点検する役割 (chat と、使える embed・rerank) と、その接続・model_key・`build_info`。
type ProbeTarget = (&'static str, Arc<dyn LlmClient>, String, String);

fn probe_targets(inner: &Inner) -> Vec<ProbeTarget> {
    let (llm, server, _) = inner.conn();
    let mut targets: Vec<ProbeTarget> = vec![(
        evorag_memory::ROLE_CHAT,
        llm,
        server.model_key.clone(),
        server.build_info.clone(),
    )];
    for a in inner.aux.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if let (Some(c), roles::RoleState::Ready(o)) = (a.ready_client(), &a.state) {
            targets.push((a.role, c, o.model_key.clone(), o.build_info.clone()));
        }
    }
    targets
}
fn role_info(
    role: &str,
    recorded: Option<&evorag_memory::ModelOrigin>,
    connected: Option<&evorag_memory::ModelOrigin>,
) -> evorag_types::api::ModelRoleInfo {
    evorag_types::api::ModelRoleInfo {
        role: role.to_owned(),
        recorded: recorded.map(|o| (o.file_name.clone(), o.model_key.clone())),
        connected: connected.map(|o| (o.file_name.clone(), o.model_key.clone())),
        status: evorag_types::api::RoleStatus::Ready,
    }
}

/// `/model` (FR-108、v0.2): 役割ごとに、記録したモデルと繋がっているモデルを示すだけ。切り替えは
/// `evorag setup` で行う (ADR-0018)。名前を付けて打たれたら、そのことを示す。
fn show_model(inner: &Inner, name: Option<String>) {
    if name.is_some() {
        reject(inner, MessageKey::ModelSwitchInSetup);
        return;
    }
    let (_, server, _) = inner.conn();
    let connected = origin(&server);
    emit(
        inner,
        Event::ModelInfo {
            roles: std::iter::once(role_info(
                evorag_memory::ROLE_CHAT,
                Some(&inner.chat_model),
                Some(&connected),
            ))
            .chain(
                inner
                    .aux
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .map(roles::AuxRole::info),
            )
            .collect(),
            n_ctx: server.n_ctx,
        },
    );
}

/// 送る前に、繋がっている応答のモデルが記録と同じかを確かめる (06 §3、AC-FR-512-3)。
/// 起動中に llama-server が別のモデルで起動し直されたときに気づく。繋がらなければ確かめない
/// (送れば、繋がらないことが失敗として記録される)。
async fn check_chat_model(inner: &Inner) -> Result<(), Message> {
    let (llm, _, name) = inner.conn();
    let Ok(server) = llm.server_info().await else {
        return Ok(());
    };
    if server.model_key != inner.chat_model.model_key {
        let connected = origin(&server);
        return Err(Message::new(MessageKey::ModelChanged)
            .arg("recorded", inner.chat_model.file_name.clone())
            .arg("connected", connected.file_name));
    }
    *inner.conn.write().unwrap_or_else(|e| e.into_inner()) = (llm, server, name);
    Ok(())
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
    write_with(inner, WriteRequest::conversation(ops)).await
}

/// 作り直せる派生の記録 (チャンク・埋め込み・点検・しきい値) を書く。書き切りを待たない
/// (落ちても作り直せる。会話の追記を遅らせないため、03 §3.4)。
pub(crate) async fn write_derived(inner: &Inner, ops: Vec<WriteOp>) -> Result<(), String> {
    write_with(inner, WriteRequest::background(ops)).await
}

async fn write_with(inner: &Inner, req: WriteRequest) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    inner
        .writer
        .submit(
            req,
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
        Command::Send { text } => {
            inner.activity.touch();
            send(inner, text).await;
        }
        Command::Interrupt => {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = st.running.as_mut().and_then(|r| r.cancel.take()) {
                let _ = c.send(Cancel::User);
            }
        }
        Command::Retry => {
            inner.activity.touch();
            regenerate(inner, true).await;
        }
        Command::Resend => {
            inner.activity.touch();
            regenerate(inner, false).await;
        }
        Command::RenameSession { title } => rename(inner, title).await,
        Command::DeleteSession => delete(inner).await,
        Command::Rate { good, reason } => rate(inner, good, reason).await,
        Command::Model { name } => show_model(inner, name),
        Command::Sources { memory } => sources(inner, memory).await,
        Command::ShowSource { turn, n } => show_source(inner, turn, n),
        Command::ShowBand { answer, target } => show_band(inner, answer, target),
        Command::AttachBand { answer, target } => {
            inner.activity.touch();
            attach_band(inner, answer, target).await;
        }
        Command::ToggleIrrelevant { answer, target } => {
            toggle_irrelevant(inner, answer, target).await;
        }
        Command::Style { set } => style(inner, set).await,
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
    let gone: std::collections::HashSet<Id> = ids.iter().copied().collect();
    if let Some(ix) = inner.index.clone() {
        let gone = gone.clone();
        let _ = tokio::task::spawn_blocking(move || ix.remove(&gone)).await;
    }
    if let Some((_, vx)) = embedding::current(inner) {
        let _ = tokio::task::spawn_blocking(move || vx.remove(&gone)).await;
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
    let (free, need) = space(inner);
    match free {
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

/// 裏の処理に足りる空きがあるか (知らせは出さない。裏の処理の判定で毎秒見る、03 §8)。
pub(crate) fn has_space(inner: &Inner) -> bool {
    let (free, need) = space(inner);
    free.is_none_or(|f| f >= need)
}

/// 退避の置き場所の空きと、要る大きさ (DB の大きさ)。
fn space(inner: &Inner) -> (Option<u64>, u64) {
    let root = inner.store.data_root();
    let need = std::fs::metadata(root.join(evorag_store::DB_FILE)).map_or(0, |m| m.len());
    let place = inner
        .backup_dir
        .clone()
        .unwrap_or_else(|| root.to_path_buf());
    ((inner.probe.free_bytes)(&place), need)
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
    // この版では読めないターンは読み飛ばし、件数を画面に知らせる (03 §4.2 の規則 11)。
    let read = evorag_memory::session_turns_counted(&rtx, id).unwrap_or_default();
    inner
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session = Some(id);
    emit(
        inner,
        Event::SessionOpened {
            session: views::session(&s),
            turns: {
                let hidden = irrelevant_in_session(inner, &read.turns);
                read.turns
                    .iter()
                    .map(|t| {
                        let mut v = views::turn(t);
                        decorate(inner, &mut v, &t.body, &hidden);
                        v
                    })
                    .collect()
            },
            unreadable: read.unreadable,
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
    // 0. 応答のモデルが記録と違えば送らない。発話は入力欄に戻す (06 §3)。
    if let Err(error) = check_chat_model(inner).await {
        emit(inner, Event::SendRejected { text, error });
        return;
    }
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
    start_generation(inner, sid, uid, Vec::new());
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
    start_generation(inner, sid, uid, Vec::new());
}

/// 書いたターンを裏でチャンクに分け、チャンクを記録して語彙の索引に足す (05 §2.3・§2.4)。
fn index_turns(inner: &Arc<Inner>, ids: Vec<Id>) {
    if inner.index.is_none() {
        return;
    }
    let worker = inner.clone();
    let task = tokio::spawn(async move { chunk_turns(&worker, &ids).await });
    let mut bg = inner.background.lock().unwrap_or_else(|e| e.into_inner());
    bg.retain(|t| !t.is_finished());
    bg.push(task);
}

/// ターンのチャンクを作り直して記録し、語彙の索引を入れ替える。前と同じ区切りなら何もしない。
async fn chunk_turns(inner: &Arc<Inner>, ids: &[Id]) {
    let Some(ix) = inner.index.clone() else {
        return;
    };
    for &id in ids {
        let Some((body, old)) = inner.store.read().ok().and_then(|r| {
            let e = match r.get(evorag_memory::KIND_TURN, id).ok()?? {
                evorag_store::Read::Ok(e) => e,
                _ => return None,
            };
            let b = e.body_as::<evorag_memory::TurnBody>().ok()?;
            let old = evorag_memory::chunks_of(&r, id).ok()?;
            Some((b, old))
        }) else {
            continue;
        };
        let new = if recall::indexable(&body) {
            recall::chunk_bodies(id, &body)
        } else {
            Vec::new()
        };
        let same = old.len() == new.len() && old.iter().zip(&new).all(|((_, a), b)| a == b);
        if same {
            continue;
        }
        let old_ids: Vec<Id> = old.iter().map(|(i, _)| *i).collect();
        let Ok((made, mut ops)) = writes(inner).chunks(&old_ids, &new) else {
            continue;
        };
        // 前のチャンクの埋め込みも消す (05 §2.2。ベクトルの索引からは下で除く)。
        let embeddings: Vec<Id> = inner
            .store
            .read()
            .ok()
            .map(|r| {
                old_ids
                    .iter()
                    .flat_map(|c| {
                        r.links_to(*c, evorag_memory::REL_EMBEDDING_OF)
                            .unwrap_or_default()
                    })
                    .map(|(e, _)| e)
                    .collect()
            })
            .unwrap_or_default();
        let w = writes(inner);
        ops.extend(embeddings.iter().flat_map(|e| w.delete_embedding(*e)));
        if let Some((_, vx)) = embedding::current(inner)
            && !old_ids.is_empty()
        {
            let gone: std::collections::HashSet<Id> = old_ids.iter().copied().collect();
            let _ = tokio::task::spawn_blocking(move || vx.remove(&gone)).await;
        }
        if write_derived(inner, ops).await.is_err() {
            continue;
        }
        let docs: Vec<_> = made
            .iter()
            .map(|(cid, c)| recall::chunk_doc(*cid, c, &body.text))
            .collect();
        let ix2 = ix.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if !old_ids.is_empty() {
                ix2.remove(&old_ids.into_iter().collect())?;
            }
            ix2.add(docs)
        })
        .await;
    }
}

/// 日次の退避を作る (その日の分が無ければ、03 §7.1)。
pub(crate) fn backup_now(inner: &Inner) {
    if !enough_space(inner) {
        return;
    }
    match inner.store.daily_backup(inner.backup_dir.as_deref()) {
        Ok(Some(_)) => tracing::info!("daily backup written"),
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "daily backup failed"),
    }
}

/// 想起の状態を知らせる (埋め込みの作り直しの進み具合など)。
pub(crate) fn emit_recall_state(inner: &Inner) {
    emit(
        inner,
        Event::RecallStatus {
            state: inner.recall_state(),
        },
    );
}

/// 起動時: チャンクと語彙の索引を DB と突き合わせる。チャンクの無いターンにはチャンクを作り、元の
/// ターンが無いチャンク (古い版が消したターンなど、03 §4.2 の規則 12) は消す。`rebuild` なら索引を作り直す。
async fn sync_chunks(inner: &Arc<Inner>, rebuild: bool) {
    let Some(ix) = inner.index.clone() else {
        return;
    };
    let started = std::time::Instant::now();
    let store = inner.store.clone();
    let Ok(plan) = tokio::task::spawn_blocking(move || recall::sync_plan(&store)).await else {
        return;
    };
    let mut docs = plan.keep;
    let deleted = plan.delete.len();
    let created = plan.create.len();
    if !plan.delete.is_empty() || !plan.create.is_empty() {
        let Ok((made, ops)) = writes(inner).chunks(&plan.delete, &plan.create) else {
            return;
        };
        if write_derived(inner, ops).await.is_err() {
            tracing::warn!("chunks not written");
            return;
        }
        docs.extend(made.iter().filter_map(|(cid, c)| {
            plan.texts
                .get(&c.turn)
                .map(|t| recall::chunk_doc(*cid, c, t))
        }));
    }
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
        if rebuild {
            ix.replace_all(docs)?;
        } else {
            let have = ix.snapshot().ids();
            let want: std::collections::HashSet<Id> = docs.iter().map(|d| d.id).collect();
            let extra: std::collections::HashSet<Id> = have.difference(&want).copied().collect();
            if !extra.is_empty() {
                ix.remove(&extra)?;
            }
            ix.add(docs.into_iter().filter(|d| !have.contains(&d.id)).collect())?;
        }
        Ok(ix.snapshot().len() as usize)
    })
    .await;
    match result {
        Ok(Ok(n)) => tracing::info!(
            docs = n,
            created,
            deleted,
            rebuilt = rebuild,
            ms = started.elapsed().as_millis() as u64,
            "lexical index synced"
        ),
        _ => tracing::warn!("lexical index sync failed"),
    }
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

/// `/style` (FR-111、07 §3)。変えたら、データ根 (とジャーナル) に記録し、次の発話から効かせる。
async fn style(inner: &Arc<Inner>, set: Option<(evorag_types::api::StyleItem, String)>) {
    let mut changed = false;
    if let Some((item, value)) = set {
        let mut next = *inner.answer.lock().unwrap_or_else(|e| e.into_inner());
        if !next.set(item, &value) {
            emit(
                inner,
                Event::Rejected {
                    error: Message::new(MessageKey::StyleUnknownValue)
                        .arg("item", item.as_str())
                        .arg("value", value)
                        .arg("choices", item.choices().join(", ")),
                },
            );
            return;
        }
        let current = inner
            .store
            .read()
            .ok()
            .and_then(|r| evorag_memory::load_settings(&r).ok())
            .unwrap_or_default();
        let ops = match writes(inner).set_setting(&current, prompt::setting_name(item), &value) {
            Ok(ops) => ops,
            Err(e) => {
                emit(
                    inner,
                    Event::Rejected {
                        error: Message::new(MessageKey::ResponseNotSaved)
                            .arg("detail", e.to_string()),
                    },
                );
                return;
            }
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
        let mut cur = inner.answer.lock().unwrap_or_else(|e| e.into_inner());
        changed = *cur != next;
        *cur = next;
    }
    let values = inner
        .answer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values();
    emit(inner, Event::StyleInfo { values, changed });
}

/// 応答の出典の詳細 (FR-110)。
fn show_source(inner: &Inner, turn: Id, n: usize) {
    let marks = irrelevant_marks(inner, turn);
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
    // 往復を代表するターン (利用者の発話) を、「無関係」の印の対象にする。
    let target = turns
        .iter()
        .find(|t| t.body.role == TurnRole::User)
        .unwrap_or(&turns[0])
        .envelope
        .id;
    let lines = turns
        .iter()
        .map(|t| (views::role(t.body.role), t.body.text.clone()))
        .collect();
    emit(
        inner,
        Event::SourceDetail {
            n,
            answer: turn,
            target,
            irrelevant: marks.contains(&target),
            title,
            at: turns[0].envelope.created_at,
            lines,
        },
    );
}

/// 応答 `answer` に付いた「無関係」の印の対象 (05 §5.2)。
fn irrelevant_marks(inner: &Inner, answer: Id) -> Vec<Id> {
    inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::feedback_for_answer(&r, answer).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, b)| b.op == evorag_memory::FEEDBACK_IRRELEVANT)
        .map(|(_, b)| b.target)
        .collect()
}

/// セッションの中で「無関係」の印を付けた往復 (同じセッションの帯には出さない、05 §5.2)。
fn irrelevant_in_session(
    inner: &Inner,
    turns: &[evorag_memory::Turn],
) -> std::collections::HashSet<Id> {
    turns
        .iter()
        .filter(|t| t.body.role == TurnRole::Assistant)
        .flat_map(|t| irrelevant_marks(inner, t.envelope.id))
        .collect()
}

/// 応答の読み取り用の型に、帯と「無関係」の印を足す (05 §5.2)。
fn decorate(
    inner: &Inner,
    view: &mut evorag_types::api::TurnView,
    body: &evorag_memory::TurnBody,
    hidden: &std::collections::HashSet<Id>,
) {
    if body.role != TurnRole::Assistant {
        return;
    }
    view.irrelevant = irrelevant_marks(inner, view.id);
    let Some(obs) = &body.obs else {
        return;
    };
    view.band_nearby = obs
        .degraded
        .iter()
        .any(|d| d == evorag_memory::DEGRADED_NO_THRESHOLD);
    view.band = obs
        .band
        .iter()
        // 同じセッションの別の応答で印を付けた往復は出さない。この応答で付けたものは、取り消せるよう残す。
        .filter(|t| !hidden.contains(t) || view.irrelevant.contains(t))
        .filter_map(|&target| {
            let e = read_env(inner, evorag_memory::KIND_TURN, target)?;
            let b = e.body_as::<evorag_memory::TurnBody>().ok()?;
            let head: String = b
                .text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(30)
                .collect();
            Some(evorag_types::api::BandItem {
                target,
                title: recall::session_title(&inner.store, b.session).unwrap_or_default(),
                at: e.created_at,
                head,
            })
        })
        .collect();
}

/// 帯の往復の詳細 (05 §5.2)。
fn show_band(inner: &Inner, answer: Id, target: Id) {
    let Some(turns) = recall::exchange_turns(&inner.store, target) else {
        reject(inner, MessageKey::BandUnknown);
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
            n: 0,
            answer,
            target,
            irrelevant: irrelevant_marks(inner, answer).contains(&target),
            title,
            at: turns[0].envelope.created_at,
            lines,
        },
    );
}

/// その時のリランクの値とリランカー (想起への操作の記録に残す、03 §3.3)。
fn feedback_body(
    op: &str,
    answer: Id,
    target: Id,
    body: &evorag_memory::TurnBody,
) -> evorag_memory::RecallFeedbackBody {
    let obs = body.obs.as_ref();
    evorag_memory::RecallFeedbackBody {
        op: op.to_owned(),
        answer,
        target,
        score: obs
            .and_then(|o| o.reranked.iter().find(|r| r.id == target))
            .map(|r| r.score),
        reranker: obs.and_then(|o| o.reranker.clone()),
    }
}

/// 帯の往復を添えて、最新の応答を作り直す (05 §5.2)。再生成の信号は記録しない。
async fn attach_band(inner: &Arc<Inner>, answer: Id, target: Id) {
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
        reject(inner, MessageKey::BandNotLatest);
        return;
    };
    let uid = last_user.envelope.id;
    let latest = turns.iter().rev().find(|t| t.body.reply_to == Some(uid));
    let Some(latest) = latest.filter(|t| t.envelope.id == answer) else {
        reject(inner, MessageKey::BandNotLatest);
        return;
    };
    let obs = latest.body.obs.clone().unwrap_or_default();
    if !obs.band.contains(&target) {
        reject(inner, MessageKey::BandUnknown);
        return;
    }
    let fb = feedback_body(evorag_memory::FEEDBACK_ATTACH, answer, target, &latest.body);
    match writes(inner).recall_feedback(&fb) {
        Ok((_, ops)) => {
            if let Err(e) = write(inner, ops).await {
                tracing::warn!(error = %e, "recall feedback not written");
            }
        }
        Err(e) => tracing::warn!(error = %e, "recall feedback not built"),
    }
    // 前に帯から添えたものに足す (05 §5.2 の「すでに添えた往復に足して」)。
    let mut extra = obs.user_attached;
    if !extra.contains(&target) {
        extra.push(target);
    }
    start_generation(inner, sid, uid, extra);
}

/// 「無関係」の印を付ける・取り消す (05 §5.2)。記録するだけで、判定もしきい値も変えない。
async fn toggle_irrelevant(inner: &Arc<Inner>, answer: Id, target: Id) {
    let Some(body) = read_env(inner, evorag_memory::KIND_TURN, answer)
        .and_then(|e| e.body_as::<evorag_memory::TurnBody>().ok())
    else {
        reject(inner, MessageKey::BandUnknown);
        return;
    };
    let existing = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::feedback_for_answer(&r, answer).ok())
        .unwrap_or_default()
        .into_iter()
        .find(|(_, b)| b.op == evorag_memory::FEEDBACK_IRRELEVANT && b.target == target);
    let w = writes(inner);
    let (ops, irrelevant) = match existing {
        Some((e, _)) => (Ok(w.delete_feedback(e.id)), false),
        None => (
            w.recall_feedback(&feedback_body(
                evorag_memory::FEEDBACK_IRRELEVANT,
                answer,
                target,
                &body,
            ))
            .map(|(_, ops)| ops),
            true,
        ),
    };
    let written = match ops {
        Ok(ops) => write(inner, ops).await.is_ok(),
        Err(_) => false,
    };
    if !written {
        reject(inner, MessageKey::FeedbackNotSaved);
        return;
    }
    emit(
        inner,
        Event::IrrelevantChanged {
            answer,
            target,
            irrelevant,
        },
    );
}

/// 生成を始める。`extra` は利用者が帯から選んで添えた往復 (05 §5.2)。
fn start_generation(inner: &Arc<Inner>, session: Id, user_turn: Id, extra: Vec<Id>) {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        generate(&worker, session, user_turn, extra, cancel_rx).await;
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

/// LLM に送る会話 (05 §4.1)。
struct Conversation {
    /// 発話 `user_turn` より前の発言 (失敗した応答と、この発話への前の応答は除く)。
    past: Vec<context::Past>,
    /// 今の発話の本文と、記録した時刻。
    current: Option<(String, i64)>,
    /// 前のターンが履歴を始めた位置 (05 §4.2)。
    from: Option<Id>,
    /// 次に振る出典の番号 (05 §5.1)。
    next_source: u32,
}

/// 発話の先頭に、記録した時刻から作った日時を付ける (05 §4.1。後のターンでも同じ文字列になる)。
fn stamped(lang: PromptLanguage, at_ms: i64, text: &str) -> String {
    format!("{} {text}", prompt::stamp(lang, at_ms))
}

fn conversation(inner: &Inner, session: Id, user_turn: Id) -> Conversation {
    let turns = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::session_turns(&r, session).ok())
        .unwrap_or_default();
    let mut c = Conversation {
        past: Vec::new(),
        current: None,
        from: None,
        next_source: recall::next_source_number(&turns),
    };
    for t in &turns {
        let id = t.envelope.id;
        if id > user_turn {
            break;
        }
        if id == user_turn {
            c.current = Some((t.body.text.clone(), t.envelope.created_at.as_millis()));
            continue;
        }
        // 今作り直す発話への前の応答は、履歴にも位置の引き継ぎにも使わない。
        if t.body.reply_to == Some(user_turn) {
            continue;
        }
        if let Some(f) = t.body.obs.as_ref().and_then(|o| o.history_from) {
            c.from = Some(f);
        }
        let (role, exchange, content) = match t.body.role {
            TurnRole::User => (
                Role::User,
                id,
                stamped(
                    inner.language,
                    t.envelope.created_at.as_millis(),
                    &t.body.text,
                ),
            ),
            TurnRole::Assistant => (
                Role::Assistant,
                t.body.reply_to.unwrap_or(id),
                t.body.text.clone(),
            ),
            TurnRole::System => continue,
        };
        // 失敗した応答は送らない。
        if t.body.state == TurnState::Failed {
            continue;
        }
        c.past.push(context::Past {
            turn: id,
            exchange,
            message: ChatMessage::new(role, content),
        });
    }
    c
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
    extra: Vec<Id>,
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
    let conv = conversation(inner, session, user_turn);
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
                conv.next_source,
                &inner.rerank_mode(),
                &extra,
            )
            .await,
        )
    } else {
        None
    };
    // システムプロンプトは凍結の核と選べる既定だけ。日時と引用は今の発話の側に置く (ADR-0017)。
    let defaults = *inner.answer.lock().unwrap_or_else(|e| e.into_inner());
    let system = ChatMessage::new(
        Role::System,
        prompt::system_prompt(inner.language, &defaults),
    );
    let quote = memory.as_ref().and_then(|m| m.quote.clone());
    let (text, at) = conv
        .current
        .clone()
        .unwrap_or_else(|| (String::new(), inner.clock.now().as_millis()));
    let mut current = quote.clone().unwrap_or_default();
    current.push_str(&stamped(inner.language, at, &text));
    let assembled = context::assemble(
        &*llm,
        &inner.tokens,
        &server.model_key,
        n_ctx,
        reserve,
        system,
        conv.past,
        (user_turn, ChatMessage::new(Role::User, current)),
        conv.from,
    )
    .await;
    let recall_obs = memory.as_ref().map(|m| m.obs.clone()).unwrap_or_default();
    let obs = evorag_memory::TurnObs {
        source_base: Some(conv.next_source),
        history_from: Some(assembled.history_from),
        recall_position: quote
            .is_some()
            .then(|| evorag_memory::RECALL_BEFORE_UTTERANCE.to_owned()),
        answer: defaults.record(),
        reranked: recall_obs.reranked,
        reranker: recall_obs.reranker,
        threshold_version: recall_obs.threshold_version,
        band: recall_obs.band,
        user_attached: recall_obs.user_attached,
        degraded: recall_obs.degraded,
    };
    let req = ChatRequest {
        messages: assembled.messages,
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
        obs: Some(obs),
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
                let session_turns = inner
                    .store
                    .read()
                    .ok()
                    .and_then(|r| evorag_memory::session_turns(&r, t.session).ok())
                    .unwrap_or_default();
                let hidden = irrelevant_in_session(inner, &session_turns);
                let mut view = views::turn_of(&env, &t);
                decorate(inner, &mut view, &t, &hidden);
                emit(inner, Event::ResponseFinished { turn: view, notice });
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
