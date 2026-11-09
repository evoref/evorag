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
    GenerationRecord, JOB_PURGE_BACKUPS, JobBody, KIND_JOB, KIND_SESSION, KIND_SIGNAL, KIND_TURN,
    REL_IN_SESSION, REL_REPLY_TO, REL_TARGET, SessionBody, SessionState, SignalBody, SignalKind,
    TurnBody, TurnRole, TurnState, kinds,
};
pub use read::{
    MemoryError, Session, Turn, list_sessions, load_session, pending_user_turns, session_turns,
};
pub use write::{
    AssistantTurn, Writes, delete_session, deleted_ids, pending_purge_jobs, purge_job,
};
