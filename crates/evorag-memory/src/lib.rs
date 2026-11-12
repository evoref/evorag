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
    DEGRADED_NO_EMBED, DEGRADED_NO_INDEX, DEGRADED_NO_RERANK, DEGRADED_NO_THRESHOLD,
    DEGRADED_RERANK_DEADLINE, DEGRADED_RERANK_ERROR, DEGRADED_RERANK_SLOW, FEEDBACK_ATTACH,
    FEEDBACK_IRRELEVANT, GenerationRecord, JOB_PURGE_BACKUPS, JobBody, KIND_JOB,
    KIND_MODEL_BINDING, KIND_MODEL_PROBE, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_SETTING,
    KIND_SIGNAL, KIND_THRESHOLD, KIND_TURN, METHOD_BUNDLED, ModelBindingBody, ModelOrigin,
    ModelProbeBody, RECALL_BEFORE_UTTERANCE, REL_FEEDBACK_ANSWER, REL_FEEDBACK_TARGET,
    REL_IN_SESSION, REL_REPLY_TO, REL_TARGET, ROLE_CHAT, ROLE_EMBED, ROLE_RERANK, RecallDecision,
    RecallFeedbackBody, RecallRecord, Reranked, SETTING_ANSWER_FORMAT, SETTING_ANSWER_LANGUAGE,
    SETTING_ANSWER_LENGTH, SETTING_IGNORED_CONFIG, SessionBody, SessionState, SettingBody,
    SignalBody, SignalKind, TARGET_CONVERSATION, ThresholdBody, TurnBody, TurnObs, TurnRole,
    TurnState, kinds,
};
pub use read::{
    Binding, Bindings, MemoryError, Session, SessionTurns, Setting, Settings, Turn,
    feedback_for_answer, list_sessions, load_bindings, load_probe, load_session, load_settings,
    load_threshold, pending_user_turns, session_turns, session_turns_counted,
};
pub use write::{
    AssistantTurn, Writes, delete_session, deleted_ids, pending_purge_jobs, purge_job,
};
