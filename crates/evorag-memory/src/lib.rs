//! メモリの書き込み・統合・読み出し、事実の入口、訂正の検証。
//!
//! 仕様: 04, 03 §3.3。v0.1 は会話の層 (セッション・ターン・信号) だけを持つ。
//!
//! 書き込みは、書き手 (`evorag_store::Writer`) に送る操作の列 (`WriteOp`) として組み立てる。
//! 記録とその関係・副索引は、1 つの依頼 (= 1 つのトランザクション) にまとめる (03 §3.4)。

mod kinds;
mod read;
mod write;

pub use kinds::{
    GenerationRecord, JOB_PURGE_BACKUPS, JobBody, KIND_JOB, KIND_MODEL_BINDING, KIND_SESSION,
    KIND_SETTING, KIND_SIGNAL, KIND_TURN, ModelBindingBody, ModelOrigin, RECALL_BEFORE_UTTERANCE,
    REL_IN_SESSION, REL_REPLY_TO, REL_TARGET, ROLE_CHAT, ROLE_EMBED, ROLE_RERANK, RecallDecision,
    RecallRecord, SETTING_ANSWER_FORMAT, SETTING_ANSWER_LANGUAGE, SETTING_ANSWER_LENGTH,
    SETTING_IGNORED_CONFIG, SessionBody, SessionState, SettingBody, SignalBody, SignalKind,
    TurnBody, TurnObs, TurnRole, TurnState, kinds,
};
pub use read::{
    Binding, Bindings, MemoryError, Session, SessionTurns, Setting, Settings, Turn, list_sessions,
    load_bindings, load_session, load_settings, pending_user_turns, session_turns,
    session_turns_counted,
};
pub use write::{
    AssistantTurn, Writes, delete_session, deleted_ids, pending_purge_jobs, purge_job,
};
