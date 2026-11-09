//! redb の表、封筒、スキーマの版と移行、書き手スレッド、`WriteToken`、ジャーナル、退避、ロック。
//!
//! 仕様: 03, 02 §4.1。redb を直接使ってよいのはこの crate だけ (02 §4.3 の R2)。

pub mod envelope;
pub mod key;

pub use envelope::{BodyError, DecodeError, Envelope};
