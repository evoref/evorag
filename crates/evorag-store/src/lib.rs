//! redb の表、封筒、スキーマの版と移行、書き手スレッド、`WriteToken`、ジャーナル、退避、ロック。
//!
//! 仕様: 03, 02 §4.1。redb を直接使ってよいのはこの crate だけ (02 §4.3 の R2)。

pub mod backup;
mod compact;
pub mod envelope;
pub mod export;
pub mod forget;
pub mod fsutil;
pub mod import_v04;
pub mod inspect;
mod journal;
mod json;
pub mod key;
pub mod location;
pub mod lock;
pub mod meta;
pub mod migrate;
pub mod recopy;
pub mod relations;
mod repair;
pub mod reset;
pub mod restore;
pub mod root_probe;
pub mod root_rename;
mod store;
#[doc(hidden)]
pub mod testutil;
pub mod version;
#[cfg(windows)]
mod winacl;
pub mod writer;

pub use compact::{CompactStop, Compaction};
pub use envelope::{BodyError, BodyPatch, DecodeError, Envelope, RawValue};
pub use journal::{JOURNAL_DIR, JournalCut, RevSpan, RewriteError, RewriteFault, RewriteReport};
pub use json::{envelope_from_json, envelope_to_json};
pub use relations::{LookupEntry, LookupRange};
pub use repair::Repaired;
pub use store::{
    Census, DB_FILE, DeriveFn, Durability, FORMAT_MIN, FORMAT_V05, FORMAT_VERSION, KIND_QUARANTINE,
    KindCount, KindSpec, OpenError, OpenOptions, OpenReport, PendingDeletes, RESERVE_FILE, Read,
    ReadOnlyReason, ReadTxn, RecordIds, Store, StoreError, WriteToken, WriteTxn,
};
pub use writer::{
    Priority, Reply, WriteError, WriteOp, WriteOutcome, WriteRequest, Writer, WriterHandle,
};
