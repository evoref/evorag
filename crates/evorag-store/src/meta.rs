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
/// 開くときにジャーナルの削除の行から消し、まだ索引と退避の整理の job にしていない記録の ID (空白区切り、
/// 03 §7.3)。`Store::pending_deletes` が読み、job を書く依頼が消す。
pub const PENDING_DELETES: &str = "cleanup.pending_deletes";
/// 次の起動で compact が要ること (値は `1`、03 §3.1.1)。書き換えてよいキーで、ジャーナルには書かない。
/// 無い・知らない値は「要らない」として読む。`Store::compact_pending` が読む。
pub const COMPACT_PENDING: &str = "compact.pending";
/// 起動時に、すべての忘れるの指示の退避の写し直しの job を作り直すこと (値は `1`、03 §4.2 の規則 12 の (1) と
/// restore の後、03 §10)。`last_writer` が 0.3.0 より古いデータ根を書き込みで開いたときは `last_writer` を書き換える
/// のと同じコミットで、restore は戻した DB に書く。コアが job を書くのと同じ依頼で消す
/// ([`crate::WriteOp::ClearForgetRecheck`])。書き換えてよいキーで、ジャーナルには書かない。compact の写しにも残す。
/// 無い・知らない値は「要らない」として読む。
pub const FORGET_RECHECK: &str = "forget.recheck";
/// [`FORGET_RECHECK`] の値。
pub const FORGET_RECHECK_SET: &str = "1";
/// `writer.<semver>`:その版が最初に書き込みで開いた時刻 (UTC、ミリ秒)。
pub const WRITER_PREFIX: &str = "writer.";
pub const MIGRATION_STATE: &str = "migration.state";
pub const MIGRATION_NONE: &str = "none";
pub const JOURNAL_LAST_SEQ: &str = "journal.last_seq";
pub const PROMPT_LANGUAGE: &str = "prompt.language";
/// 値の鍵と再び覚えない印の HMAC の鍵 (03 §3.1.1、04 §5.6、v0.3)。一度だけ書く。
pub const FORGET_KEY: &str = "memory.forget_key";
/// 今の単位を作った導出の指紋と、起動時の軽い検査に使う成分 (03 §3.1.1、04 §5.3、v0.5)。上書きしてよいキーで、
/// ジャーナルにも退避にも書き出しにも写さない (導き直せる派生、03 §7.1・§11)。
pub const DERIVE_FINGERPRINT: &str = "memory.derive_fingerprint";
