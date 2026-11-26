//! メモリの書き込み・統合・読み出し、事実の入口、訂正の検証。
//!
//! 仕様: 04, 03 §3.3。v0.1 は会話の層 (セッション・ターン・信号) だけを持つ。
//!
//! 書き込みは、書き手 (`evorag_store::Writer`) に送る操作の列 (`WriteOp`) として組み立てる。
//! 記録とその関係・副索引は、1 つの依頼 (= 1 つのトランザクション) にまとめる (03 §3.4)。

mod chars;
mod correction;
mod derived;
pub use derived::RecordError;
mod directive;
mod extract;
mod facts;
mod fold;
#[cfg(test)]
mod fold_cases;
mod forget;
mod gates;
mod import_v04;
mod kinds;
mod ledger;
mod lexical;
mod mask;
mod note;
mod prefilter;
mod read;
mod rules;
mod segment;
mod table;
mod unit;
mod value_key;
mod view;
mod write;

pub use correction::{
    CandidateSkip, CorrectionBody, CorrectionCandidate, DIR_MAX_CHARS, LoadedCorrections,
    MATCH_MAX_CHARS, PatternMatch, Rejection, Target, Unverifiable, VERIFY_EXCERPT_CHARS,
    VERIFY_MAX_TOKENS, VERIFY_VERSION, Verdict, VerifyContext, VerifyOutput, candidate_ops,
    checked_op, correction_sources, find_correction, load_corrections, match_correction,
    parse_verify, previous_answer, tail_excerpt, utterance_excerpt, verify, verify_input,
    verify_schema, verify_system,
};
pub use directive::{
    DirectiveBody, DirectiveOp, DirectiveOrigin, DirectiveRedaction, KIND_DIRECTIVE,
    LOOKUP_DIRECTIVE_ENT, LOOKUP_DIRECTIVE_TURN, LOOKUP_DIRECTIVE_VK, PrivateScope,
    REL_DIRECTIVE_CANCELS, TurnSpan,
};
pub use extract::{
    BUDGET_MARGIN, Batch, Counting, EXTRACT_MAX_TOKENS, EXTRACT_VERSION, ExtractJobBody,
    ExtractProgress, ExtractionOutput, JOB_EXTRACT_FACTS, Packing, REQUEST_TOKENS, UserUtterance,
    extraction_fits, extraction_input, extraction_schema, extraction_system, load_extract_progress,
    pack, parse_extraction, turn_rev, turn_rev_checked, user_utterances_after, utterance_budget,
};
pub use facts::{
    DirectivesAdded, FactCtx, ForgetRecord, LoadedFacts, NewDirective, SourcesDropped,
    add_directive, add_directives, consolidate, diff_ops, drop_turns, is_forget_directive,
    kept_after_delete, load_directives, load_fact, load_facts, load_forgets, load_hidden, rebuild,
};
pub use fold::{
    Candidate, Choice, Cover, Directive, DirectiveKind, Directives, Dropped, Event, Fact, FactSet,
    FactState, FoldEnv, Mood, Report, Retraction, Scope, Source, Tense, VerifiedCorrection,
    drop_sources, fold, refresh_private, retract, table_order,
};
pub use forget::{
    ForgetAbort, ForgetEnv, ForgetOptions, ForgetPlan, ForgetPreview, ForgetSample, ForgetScope,
    PlanHash, Replay, TurnForgets, forget_backups_for_all, forget_backups_job, journal_cuts,
    latest_forget, plan_forget, rebuild_directive_indexes, replay_forgets,
};
pub use gates::{
    Accepted, EvidenceMatch, GateEnv, GateReason, MAX_SUBJECT_CHARS, RawCandidate, RawMood, Safety,
    Utterance, check_candidate, check_candidates, check_subject, check_text, check_value,
};
pub use import_v04::{
    FactBody, FactDirectiveBody, HiddenBody, KIND_CORRECTION, KIND_FACT, KIND_FACT_DIRECTIVE,
    KIND_NOTE, OP_FORGET, OP_UNBLOCK, ORIGIN_DELETE, ORIGIN_USER, Origin, REL_CANCELS,
    REL_CORRECTION_OF, REL_FACT_SRC, REL_NOTE_OF, RedactionBody, SourceBody, V04_ONLY_KINDS,
    kinds_v04,
};
pub use kinds::{
    ChunkBody, DEGRADED_EMBED_DEADLINE, DEGRADED_NO_EMBED, DEGRADED_NO_INDEX, DEGRADED_NO_RERANK,
    DEGRADED_NO_THRESHOLD, DEGRADED_RERANK_DEADLINE, DEGRADED_RERANK_ERROR, DEGRADED_RERANK_SLOW,
    EmbeddingBody, FEEDBACK_ATTACH, FEEDBACK_IRRELEVANT, GenerationRecord, JOB_FORGET_BACKUPS,
    JOB_PURGE_BACKUPS, JOB_PURGE_INDEXES, JobBody, KIND_CHUNK, KIND_EMBEDDING, KIND_JOB,
    KIND_MODEL_BINDING, KIND_MODEL_PROBE, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_SETTING,
    KIND_SIGNAL, KIND_THRESHOLD, KIND_TURN, METHOD_BUNDLED, ModelBindingBody, ModelOrigin,
    ModelProbeBody, NoteBody, PREFETCH_NONE, Packed, PrefetchObs, QueryObs,
    RECALL_BEFORE_UTTERANCE, RECALL_SEPARATE_MESSAGE, REL_CHUNK_OF, REL_EMBEDDING_OF,
    REL_FEEDBACK_ANSWER, REL_FEEDBACK_TARGET, REL_IN_SESSION, REL_REPLY_TO, REL_TARGET, ROLE_CHAT,
    ROLE_EMBED, ROLE_RERANK, RecallCandidate, RecallDecision, RecallFeedbackBody, RecallRecord,
    Reranked, SETTING_ANSWER_FORMAT, SETTING_ANSWER_LANGUAGE, SETTING_ANSWER_LENGTH,
    SETTING_IGNORED_CONFIG, SessionBody, SessionState, SettingBody, SignalBody, SignalKind, SrcRev,
    TARGET_CONVERSATION, TURN_REV_FIELD, ThresholdBody, TurnBody, TurnObs, TurnRole, TurnState,
    UNIT_CHUNK, UNIT_KEY, WindowRef, carried_on_restore, kinds, kinds_v05, v05_root,
};
pub use ledger::{
    ByteRange, CALL_SEGMENT, CorrInput, KIND_PROPOSAL, LEDGER_SCHEMA, LOOKUP_LEDGER_SEGMENT,
    LOOKUP_LEDGER_SELECTION, LedgerBody, LedgerCorrection, LedgerCorrectionOut, LedgerError,
    LedgerHead, LedgerInputs, LedgerLayer, LedgerMood, LedgerOutput, LedgerOutputError, LedgerRead,
    LedgerStatus, LedgerTense, LedgerTokens, LedgerUnit, LedgerUnitOut, LedgerVerdict,
    REL_LEDGER_SEGMENT, REL_LEDGER_TURN, Section, SegmentUpdate, SentenceSpan, Through,
    read_ledger, selection_key,
};
pub use lexical::{Signals, evidence_language, signals};
pub use mask::{
    Masker, REDACTION_MARKERS, TurnText, apply_ranges, hidden_marker, redaction_marker,
};
pub use note::{
    NOTE_ANSWER_CHARS, NOTE_KEY_CHARS, NOTE_KEYS, NOTE_RULES, NOTE_USER_CHARS, NoteExchange,
    SEGMENT_CHARS, SEGMENT_EXCHANGES, SEGMENT_QUIET_MS, Segment, note_is_current, note_matches,
    sanitize_keys, segments,
};
pub use prefilter::{MODE_CHAT, Prefiltered, Screened, SkipReason, prefilter};
pub use read::{
    Binding, Bindings, MemoryError, Session, SessionTurns, Setting, Settings, Turn, chunks_of,
    embedding_bodies, embedding_vector, embeddings_of_chunk, embeddings_of_model,
    feedback_for_answer, list_sessions, load_bindings, load_probe, load_session, load_settings,
    load_threshold, pending_user_turns, session_turns, session_turns_counted,
    turns_without_session,
};
pub use rules::{
    Attribute, Attributes, Cardinality, CommonRules, CorrectionRules, LanguageRules,
    MAX_VALUE_CHARS, Prefilter, Quote, RULE_LANGUAGES, ReadingRules, SafetyRules,
    all_language_rules, attributes, common_rules, language_rules,
};
pub use segment::{
    KIND_SEGMENT, LOOKUP_SEGMENT_FIRST, LOOKUP_SEGMENT_SESSION, REL_SEGMENT_SESSION,
    REL_SEGMENT_TURN, SegmentBody, SegmentError, segment_id,
};
pub use table::{
    FactKey, FactTable, SELF_LABELS, TABLE_BUDGET, TableRow, fact_key, fact_keys, fact_table,
    table_head, table_rows, table_version,
};
pub use unit::{
    Anchor, AnchorKind, KIND_UNIT, LOOKUP_UNIT_DATE, LOOKUP_UNIT_EA, LOOKUP_UNIT_SRC,
    LOOKUP_UNIT_VK, REL_UNIT_ANCHOR, UnitBody, UnitLayer, UnitState, UnitTense, UnitWhen,
    WhenBasis, entity_aspect_key, unit_delete_ops, unit_ops, units_request,
};
pub use value_key::{
    BlockMark, ForgetKey, NORM_V1, ValueKey, block_mark, loose_key, normalize_loose, normalize_v1,
    value_key,
};
pub use view::{
    fact_history, fact_list, find_row, private_info, row_key, rule_only, subject_name, subject_of,
};
pub use write::{
    AssistantTurn, CleanupJob, Writes, delete_session, deleted_ids, expect_record, expect_turn,
    index_job, pending_cleanup_jobs, pending_purge_jobs, purge_job,
};
