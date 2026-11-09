//! ID、時刻、エラー、文言キー、コアの API (命令・イベント・照会) の型。
//!
//! 仕様: 02 §3, 02 §4.1。外部の依存は serde だけ (02 §4.3 の R4)。

mod message;

pub use message::MessageKey;
