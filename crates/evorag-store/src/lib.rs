//! redb の表、封筒、スキーマの版と移行、書き手スレッド、`WriteToken`、ジャーナル、退避、ロック。
//!
//! 仕様: 03, 02 §4.1。redb を直接使ってよいのはこの crate だけ (02 §4.3 の R2)。

pub mod envelope;
pub mod key;
pub mod lock;
pub mod meta;
pub mod relations;
mod store;
#[doc(hidden)]
pub mod testutil;
pub mod version;
pub mod writer;

pub use envelope::{BodyError, DecodeError, Envelope};
pub use relations::{LookupEntry, LookupRange};
pub use store::{
    Census, DB_FILE, Durability, FORMAT_VERSION, KIND_QUARANTINE, KindCount, KindSpec, OpenError,
    OpenOptions, OpenReport, Read, ReadOnlyReason, ReadTxn, Store, StoreError, WriteToken,
    WriteTxn,
};
pub use writer::{
    Priority, Reply, WriteError, WriteOp, WriteOutcome, WriteRequest, Writer, WriterHandle,
};
