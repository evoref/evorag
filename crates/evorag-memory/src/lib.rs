//! メモリの書き込み・統合・読み出し、事実の入口、訂正の検証。
//!
//! 仕様: 04, 03 §3.3。v0.1 は会話の層 (セッション・ターン・信号) だけを持つ。
//!
//! 書き込みは、書き手 (`evorag_store::Writer`) に送る操作の列 (`WriteOp`) として組み立てる。
//! 記録とその関係・副索引は、1 つの依頼 (= 1 つのトランザクション) にまとめる (03 §3.4)。

mod facts;
mod fold;
#[cfg(test)]
mod fold_cases;
mod kinds;
mod lexical;
mod note;
mod read;
mod rules;
mod value_key;
mod write;

pub use facts::{
    FactBody, FactCtx, FactDirectiveBody, HiddenBody, KIND_FACT, KIND_FACT_DIRECTIVE, LoadedFacts,
    REL_CANCELS, REL_FACT_SRC, RedactionBody, SourceBody, add_directive, consolidate, diff_ops,
    load_directives, load_facts, rebuild,
};
pub use fold::{
    Candidate, Choice, Directive, DirectiveKind, Directives, Event, Fact, FactSet, FactState,
    FoldEnv, Mood, Report, Scope, Source, Tense, VerifiedCorrection, fold, refresh_private,
    table_order,
};
pub use kinds::{
    ChunkBody, DEGRADED_EMBED_DEADLINE, DEGRADED_NO_EMBED, DEGRADED_NO_INDEX, DEGRADED_NO_RERANK,
    DEGRADED_NO_THRESHOLD, DEGRADED_RERANK_DEADLINE, DEGRADED_RERANK_ERROR, DEGRADED_RERANK_SLOW,
    EmbeddingBody, FEEDBACK_ATTACH, FEEDBACK_IRRELEVANT, GenerationRecord, JOB_PURGE_BACKUPS,
    JobBody, KIND_CHUNK, KIND_EMBEDDING, KIND_JOB, KIND_MODEL_BINDING, KIND_MODEL_PROBE, KIND_NOTE,
    KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_SETTING, KIND_SIGNAL, KIND_THRESHOLD, KIND_TURN,
    METHOD_BUNDLED, ModelBindingBody, ModelOrigin, ModelProbeBody, NoteBody,
    RECALL_BEFORE_UTTERANCE, REL_CHUNK_OF, REL_EMBEDDING_OF, REL_FEEDBACK_ANSWER,
    REL_FEEDBACK_TARGET, REL_IN_SESSION, REL_NOTE_OF, REL_REPLY_TO, REL_TARGET, ROLE_CHAT,
    ROLE_EMBED, ROLE_RERANK, RecallDecision, RecallFeedbackBody, RecallRecord, Reranked,
    SETTING_ANSWER_FORMAT, SETTING_ANSWER_LANGUAGE, SETTING_ANSWER_LENGTH, SETTING_IGNORED_CONFIG,
    SessionBody, SessionState, SettingBody, SignalBody, SignalKind, TARGET_CONVERSATION,
    ThresholdBody, TurnBody, TurnObs, TurnRole, TurnState, UNIT_CHUNK, UNIT_KEY, kinds,
};
pub use lexical::{Signals, evidence_language, signals};
pub use note::{
    NOTE_ANSWER_CHARS, NOTE_KEY_CHARS, NOTE_KEYS, NOTE_RULES, NOTE_USER_CHARS, NoteExchange,
    SEGMENT_CHARS, SEGMENT_EXCHANGES, SEGMENT_QUIET_MS, Segment, sanitize_keys, segments,
};
pub use read::{
    Binding, Bindings, MemoryError, Session, SessionTurns, Setting, Settings, Turn, chunks_of,
    embedding_bodies, embedding_vector, embeddings_of_chunk, embeddings_of_model,
    feedback_for_answer, list_sessions, load_bindings, load_probe, load_session, load_settings,
    load_threshold, pending_user_turns, session_turns, session_turns_counted,
};
pub use rules::{
    Attribute, Attributes, Cardinality, CommonRules, CorrectionRules, LanguageRules,
    MAX_VALUE_CHARS, Prefilter, Quote, RULE_LANGUAGES, ReadingRules, SafetyRules,
    all_language_rules, attributes, common_rules, language_rules,
};
pub use value_key::{BlockMark, ForgetKey, NORM_V1, ValueKey, block_mark, normalize_v1, value_key};
pub use write::{
    AssistantTurn, Writes, delete_session, deleted_ids, pending_purge_jobs, purge_job,
};
