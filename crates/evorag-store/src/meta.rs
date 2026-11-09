//! `meta` の表のキー (03 §3.1.1)。キーと値の意味は v0.1 から変えない。足すだけにする。

pub const FORMAT_VERSION: &str = "format.version";
pub const FORMAT_MIN_READ: &str = "format.min_read";
pub const FORMAT_MIN_WRITE: &str = "format.min_write";
pub const KEY_ENCODING_VERSION: &str = "key_encoding.version";
/// 封筒の値の配置の版 (03 §3.2)。記録ごとにも先頭の 1 バイトに持つ。
pub const ENVELOPE_LAYOUT: &str = "envelope.layout";
pub const DATA_ROOT_ID: &str = "data_root.id";
pub const CREATED_BY: &str = "created_by";
pub const LAST_WRITER: &str = "last_writer";
pub const MAX_WRITER: &str = "max_writer";
/// `writer.<semver>`: その版が最初に書き込みで開いた時刻 (UTC、ミリ秒)。
pub const WRITER_PREFIX: &str = "writer.";
pub const MIGRATION_STATE: &str = "migration.state";
pub const MIGRATION_NONE: &str = "none";
pub const JOURNAL_LAST_SEQ: &str = "journal.last_seq";
pub const PROMPT_LANGUAGE: &str = "prompt.language";
