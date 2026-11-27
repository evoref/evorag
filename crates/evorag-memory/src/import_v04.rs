//! v0.4 のデータ根の読み手 (03 §12.4、08 §2.6 の M5.1)。`import-v04` が v0.4 の根を読むだけで開き、`fact` と
//! `fact_directive` の本体を読むのに要るものを、ここに集める。
//!
//! - `body`: v0.4 の `fact`・`fact_directive` の本体の型 (保存の形。v0.3〜v0.4 の形のまま変えない)。
//! - `kind_set`: v0.4 の根の種類 ([`kinds_v04`])。v0.4 だけの種類 ([`V04_ONLY_KINDS`]) の名前・版・関係と
//!   副索引の作り方を持つ。
//! - `transfer`: 取り込みで写すもの・除くもの・変えて写すもの ([`Selector`]・[`imported_directive`]・[`v04_ended`])。
//!
//! **依存の規則**: このモジュールは、旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・
//! `table`・`forget`・`view`・`gates` など、v0.4 の抽出・統合・事実の表・ノート・訂正) に依存しない。旧エンジンは
//! v0.5 の判定の前に外す (08 §2.6、02 §4.4) ので、依存の向きは「旧エンジン → `import_v04`」だけにする。依存して
//! よいのは、v0.5 でも残る会話の層の種類 (`crate::kinds`)、v0.5 の指示と値の鍵 (`crate::directive`・
//! `crate::value_key`) と保存の層 (`evorag_store`。忘れるの純粋な核 `evorag_store::forget` を含む) だけ。

mod body;
mod kind_set;
mod transfer;

pub use body::{
    FactBody, FactDirectiveBody, HiddenBody, OP_FORGET, OP_UNBLOCK, ORIGIN_DELETE, ORIGIN_USER,
    Origin, RedactionBody, SourceBody,
};
pub(crate) use body::{OP_ACCEPT, OP_PRIVATE, OP_PRIVATE_OFF, OP_REVERT, origin_user};
pub(crate) use kind_set::{
    CORRECTION_V, FACT_DIRECTIVE_V, FACT_V, MarksField, SourcesField, SubjectAttribute,
    derive_correction, derive_directive, fact_entries,
};
#[cfg(test)]
pub(crate) use kind_set::{IndexEntry, LOOKUP_MARK, LOOKUP_VK, derive_fact, directive_entries};
pub use kind_set::{
    KIND_CORRECTION, KIND_FACT, KIND_FACT_DIRECTIVE, KIND_NOTE, REL_CANCELS, REL_CORRECTION_OF,
    REL_FACT_SRC, REL_NOTE_OF, V04_ONLY_KINDS, kinds_v04,
};
pub use transfer::{
    COPY_KINDS, Disposition, EndedValue, Exclusion, Selector, V04_ENDED_V, V04Ended, copy_ops,
    imported_directive, read_fact_directives, read_facts, read_kind, v04_ended,
};
