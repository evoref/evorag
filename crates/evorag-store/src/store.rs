//! データ根を開き、`evorag.redb` の記録を読み書きする (03 §1、§3、§4.2、§8、§9)。
//!
//! この段階では、書き込みはトランザクションの単位で直接行う。書き手スレッドと `WriteToken`
//! (02 §5) は、この上に載せる。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ciborium::Value;
use evorag_types::{Clock, Id, IdGen, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::envelope::{DecodeError, Envelope};
use crate::fsutil;
use crate::journal::{Journal, LineOp};
use crate::key::{self, KeyBuf, OwnedPart};
use crate::lock::{DataRootLock, LockError};
use crate::meta;
use crate::migrate::{self, FormatPlan};
use crate::version::Version;
use crate::writer::WriteOp;

/// DB のファイルの名前。
pub const DB_FILE: &str = "evorag.redb";

/// 会話の追記のための予備のファイル (03 §8、既定 16 MiB)。
pub const RESERVE_FILE: &str = "reserve.space";
pub(crate) const RESERVE_BYTES: u64 = 16 * 1024 * 1024;

/// この evorag が書く DB の形式の版 (03 §4.1。v0.1〜v0.4 は `0.<n>`)。
pub const FORMAT_VERSION: &str = "0.3";

/// この形式の DB を読み書きするのに要る、evorag が知る形式の版の下限 (`format.min_read`・
/// `format.min_write`、03 §3.1.1)。0.2 と 0.3 は種類と任意の項目を足しただけなので、0.1 のまま
/// にし、v0.1.2・v0.2.x に戻しても読み書きできるようにする (03 §12.1)。
pub const FORMAT_MIN: &str = "0.1";

/// v0.5 の新しいデータ根の形式の版 (03 §3.1.1・§12.4)。`format.min_read`・`format.min_write` も同じ値にし、
/// v0.4 以前の版には読むだけでも開かせない (台帳と指示を知らずに会話だけを見せ、忘れる・private を当てずに
/// 使うのを防ぐため)。v0.4 の根 (形式の版 `0.3`) は在り場所で移行せず、`import-v04` で新しい根へ取り込む。
pub const FORMAT_V05: &str = "0.5";

pub(crate) type Raw = TableDefinition<'static, &'static [u8], &'static [u8]>;
pub(crate) const META: Raw = TableDefinition::new("meta");
pub(crate) const RECORDS: Raw = TableDefinition::new("records");
pub(crate) const LINKS: Raw = TableDefinition::new("links");
pub(crate) const LOOKUP: Raw = TableDefinition::new("lookup");
pub(crate) const BLOBS: Raw = TableDefinition::new("blobs");
/// すべての表 (03 §3.1)。退避と移行は、型を知らずにこの表を写す。
pub(crate) const TABLES: [Raw; 5] = [META, RECORDS, LINKS, LOOKUP, BLOBS];

/// 書き込みの耐久性 (03 §3.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// コミットのたびにディスクへ書き切る。会話の追記と明示の操作。
    Immediate,
    /// 書き切らない。プロセスの強制終了でも失われる。job から作り直せる裏の処理の結果だけ。
    None,
}

impl From<Durability> for redb::Durability {
    fn from(d: Durability) -> Self {
        match d {
            Durability::Immediate => redb::Durability::Immediate,
            Durability::None => redb::Durability::None,
        }
    }
}

/// 種類の名前: quarantine (03 §3.3)。
pub const KIND_QUARANTINE: &str = "quarantine";

/// 封筒から、その記録の関係と副索引を作る関数。ジャーナルから取り込むときに使う
/// (関係と副索引は記録から作り直せる、03 §6・§10 の repair)。
pub type DeriveFn = fn(&Envelope) -> Vec<crate::writer::WriteOp>;

/// 種類の定め (03 §3.3)。種類を持つ crate が、開くときに渡す。
#[derive(Debug, Clone)]
pub struct KindSpec {
    pub name: &'static str,
    /// この版が書く、その種類の形式の版。
    pub current_v: u32,
    /// 新しい `v` の記録があるときも、新しい記録を今の `v` で作ってよいか (03 §4.2 の規則 6。既定は作らない)。
    pub create_when_newer_present: bool,
    /// ジャーナルにも書くか (会話と明示の操作、03 §6)。
    pub journaled: bool,
    /// 記録の関係と副索引を作る関数。ジャーナルの取り込み・差し替え (`Patch`)・`repair` が使う。
    ///
    /// `repair` (03 §10) が記録から作り直す関係と副索引は、この関数の出力だけ。作り直すときに消すのは、
    /// [`KindSpec::derived_rels`] と [`KindSpec::derived_lookups`] に宣言した名前の行だけなので、この関数が作る
    /// 関係の名前と副索引の項目は、すべてそこに宣言する (宣言の無い名前の行は、新しい版が書いたものとして残す、
    /// 03 §4.2 の規則 6・11)。`None` の種類の関係と副索引は、`repair` は消さない。
    pub derive: Option<DeriveFn>,
    /// [`KindSpec::derive`] が作りうる関係の名前 (順向き。逆向きの `~名前` の行も含めて扱う)。
    pub derived_rels: &'static [&'static str],
    /// [`KindSpec::derive`] が作りうる副索引の項目 (この種類の名前の下の `field`)。
    pub derived_lookups: &'static [&'static str],
    /// 単調に増える本体の整数の項目 (ターンの改訂の番号 `rev`、04 §7.3)。宣言した種類では、ジャーナルの取り込み
    /// (と repair) は、DB の記録のこの値を小さくする `put` の行を当てない (03 §6 の「突き合わせ」)。ジャーナルを
    /// 開くときに、この値を ID ごとに集める (04 §7.2 の「ジャーナルの層の見つけ方」)。
    pub monotonic: Option<&'static str>,
    /// 記録の ID の作り方 (03 §3.3)。中身から作る種類 (v0.5 の `segment`・`unit`) の ID は時刻の順に並ばないので、
    /// `IdGen` の下限 ([`Store::max_record_id`]) に入れない。
    pub ids: RecordIds,
}

/// 記録の ID の作り方 ([`KindSpec::ids`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordIds {
    /// `IdGen` が出す、時刻の順に並ぶ UUID v7 (02 §6)。
    #[default]
    Generated,
    /// 中身から決定論で作るハッシュ (04 §2 の区切りの ID、§5.1 の単位の ID)。時刻の順に並ばない。
    Content,
}

impl KindSpec {
    pub const fn new(name: &'static str, current_v: u32) -> Self {
        Self {
            name,
            current_v,
            create_when_newer_present: false,
            journaled: false,
            derive: None,
            derived_rels: &[],
            derived_lookups: &[],
            monotonic: None,
            ids: RecordIds::Generated,
        }
    }

    /// `old` の上に `new` を書くと、宣言した単調に増える項目が小さくなるか (03 §6)。`new` の値が読めないときも、
    /// `old` の値が 0 より大きければ小さくなるとみなす (伏せ字の前の本文を戻さない側に倒す)。`old` の値が
    /// 読めないときは、比べられないので小さくならないとみなす。
    pub fn lowers_monotonic(&self, old: &Envelope, new: &Envelope) -> bool {
        let Some(field) = self.monotonic else {
            return false;
        };
        match (old.body_uint(field), new.body_uint(field)) {
            (Some(o), Some(n)) => n < o,
            (Some(o), None) => o > 0,
            (None, _) => false,
        }
    }
}

/// ストアが自分で持つ種類。
const BUILTIN_KINDS: [KindSpec; 1] = [KindSpec::new(KIND_QUARANTINE, 1)];

/// 開くときの指定。
pub struct OpenOptions {
    pub data_root: PathBuf,
    /// この evorag の版 (semver)。
    pub evorag_version: String,
    pub kinds: Vec<KindSpec>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    /// 設定のファイル (`config.toml`)。移行の前の退避に含める (03 §7.2)。
    pub config_file: Option<PathBuf>,
}

/// 書き込みを断る理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// DB が、この版が書けない新しい形式の版 (`format.min_write`) で書かれている。
    NewerFormat { min_write: String },
    /// 古い形式の DB を移行できなかった。元の DB のまま開いている (03 §12.1)。
    MigrationFailed { reason: String },
}

/// 開いたときに分かったこと。画面と `doctor` が利用者に知らせる。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// 新しく作った。
    pub created: bool,
    /// 読むだけで開いた理由。
    pub read_only: Option<ReadOnlyReason>,
    /// これまでに、この版より新しい版が書き込みで開いた (巻き戻し、03 §4.2 の規則 6)。
    pub newer_writer_seen: Option<String>,
    /// 移行した (元の形式の版)。
    pub migrated_from: Option<String>,
    /// 移行の前に作った退避。
    pub pre_op_backup: Option<PathBuf>,
    /// 起動時にジャーナルから取り込んだ行の数 (03 §6)。
    pub journal_replayed: usize,
    /// ジャーナルの読めない行 (チェックサムが合わない) の数。`doctor` が報告する。
    pub journal_bad_lines: usize,
    /// 書きかけで切り詰めたジャーナルの末尾のバイト数。
    pub journal_truncated: usize,
    /// ジャーナルの新しい版の行 (チェックサムは合うが読めない) の数。
    pub journal_newer_lines: usize,
    /// ジャーナルの末尾で組の行がそろわず (書きかけの追記の途中で止まった)、当てずに捨てた組の数 (03 §6 の
    /// 「組の行」、v0.5)。DB にはコミットしていないので、操作は無かったことになる。
    pub journal_torn_groups: usize,
    /// ジャーナルの行の欠けた組 (末尾の書きかけを除く) の数。読める行は当てる (`doctor` が報告する。
    /// [`Store::partial_groups`])。
    pub journal_incomplete_groups: usize,
    /// DB に無かった `memory.forget_key` を、ジャーナルの行から入れた (03 §3.1.1)。
    pub forget_key_recovered: bool,
    /// ジャーナルの `memory.forget_key` が 2 つ以上あるか、DB の値と違う。書き換えていない。
    /// 事実の抽出・統合・事実の表を止める (フェイルクローズ、03 §3.1.1)。
    pub forget_key_conflict: bool,
    /// 読むだけで開いたので、ジャーナルにだけある `memory.forget_key` を入れていない。
    pub forget_key_pending: bool,
    /// 開くときにジャーナルの削除の行から消した記録の ID (取り込んだ削除の行と、削除の行があるのに DB に
    /// 残っていた記録)。索引と退避から除く ID として `meta` にも残す ([`Store::pending_deletes`])。
    pub journal_deletes: Vec<Id>,
    /// 削除の行のある ID の `put` の行をジャーナルから除く書き直しが、開くときに済まなかった (次に開くときに
    /// もう一度試す。`doctor` が報告する)。
    pub journal_rewrite_failed: Option<crate::journal::RewriteError>,
    /// 起動時の compact の結果 (03 §8)。[`Store::compact_if_pending`] が書く。
    pub compaction: crate::compact::Compaction,
}

/// 開くときにジャーナルの削除の行から消し、まだ整理の job にしていない記録 ([`Store::pending_deletes`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingDeletes {
    pub ids: Vec<Id>,
    /// 読めなかった語の数 (呼び手が知らせる。読めない語は、次に印を書き直すときに落ちる)。
    pub unreadable: usize,
}

/// 空白で区切った ID の並びを読む。読めない語は飛ばして数える。
pub(crate) fn parse_ids(v: &str) -> PendingDeletes {
    let mut out = PendingDeletes::default();
    for w in v.split_whitespace() {
        match w.parse() {
            Ok(id) => out.ids.push(id),
            Err(_) => out.unreadable += 1,
        }
    }
    out
}

/// `memory.forget_key` の形 (ちょうど 64 桁の小文字の 16 進、03 §3.1.1)。
fn is_forget_key(v: &str) -> bool {
    v.len() == 64 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Lock(#[from] LockError),
    /// redb が、ほかのプロセスが開いていると言う (03 §9)。
    #[error("the database is open in another process")]
    DatabaseInUse,
    /// この版では読むこともできない新しい形式の版 (`format.min_read`)。
    #[error(
        "the database needs a newer evorag to read (format {format}, needs at least {min_read})"
    )]
    TooNew { format: String, min_read: String },
    /// v0.5 の形式 ([`FORMAT_V05`]) で開こうとした DB が、それより古い形式の版で書かれている。在り場所で移行せず、
    /// `import-v04` で新しい根へ取り込む (03 §12.4)。何も書いていない。
    #[error("the database is in format {format}; import it into a new data root (import-v04)")]
    NeedsImport { format: String },
    /// `meta` が読めない・形式の版が無い。
    #[error("the database metadata is unreadable: {0}")]
    BadMeta(String),
    /// DB のファイルが開けない (03 §8 の「DB のファイルの破損」)。
    #[error("the database file cannot be opened: {0}")]
    Unreadable(String),
    /// DB のファイルが壊れていたので `broken/` へ移した。退避とジャーナルから戻せる (FR-205)。
    #[error("the database file was broken ({reason}) and was moved to {}", moved_to.display())]
    Broken { moved_to: PathBuf, reason: String },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the database is open read-only: {0:?}")]
    ReadOnly(ReadOnlyReason),
    #[error("unknown kind `{0}` cannot be written")]
    UnknownKind(String),
    #[error("kind `{kind}` must be written at v{expected}, not v{got}")]
    WrongVersion {
        kind: String,
        expected: u32,
        got: u32,
    },
    /// 新しい `v` の記録は更新しない (03 §4.2 の規則 6)。
    #[error("record {kind}/{id} was written in a newer form (v{v}) and is read-only")]
    NewerRecord { kind: String, id: Id, v: u32 },
    /// 新しい `v` の記録がある種類は、新しい記録を作らない (03 §4.2 の規則 6)。
    #[error("kind `{0}` has records in a newer form; new records are not created")]
    KindHasNewerRecords(String),
    /// 新しい形・壊れた記録は、上書きしない (先に quarantine で扱う)。
    #[error("record {kind}/{id} cannot be read and is not overwritten")]
    Unreadable { kind: String, id: Id },
    /// 書き込みの権利はもう渡っている (書き手は 1 つ、02 §5)。
    #[error("the write token has already been taken")]
    TokenTaken,
    #[error(transparent)]
    BadRelation(#[from] crate::relations::BadRelation),
    #[error("database error: {0}")]
    Db(#[from] redb::Error),
    #[error("i/o error: {0}")]
    Io(String),
    /// ジャーナルに書いた後でコミットに失敗したので、開き直して取り込み直すまで書かない (03 §6)。
    #[error("the database must be reopened before further writes")]
    NeedsRestart,
    /// ジャーナルに書けなかった。DB にも書いていない (03 §6、AC-FR-202-1)。
    #[error("the journal could not be written: {0}")]
    Journal(String),
    /// 一度だけ書く `meta` の値が、もう別の値で書かれている (07 §6.1 のプロンプトの言語など)。
    #[error("meta `{key}` is already `{existing}`")]
    MetaConflict { key: String, existing: String },
    /// 書き込みの操作で書いてよい `meta` のキーではない。
    #[error("meta `{0}` cannot be written by a request")]
    MetaNotWritable(String),
    /// 消した記録を `put` で作り直さない (古い封筒を書き戻す経路で、消したものが蘇らないため、04 §7.7 の 3)。
    #[error("record {kind}/{id} was deleted and is not written again")]
    Deleted { kind: String, id: Id },
    /// 依頼の前提 ([`crate::WriteOp::Expect`]・[`crate::WriteOp::Patch`]) が合わない (記録が無い・変わった)。
    /// 依頼の全体を書かない (04 §7.7 の 4)。
    #[error("record {kind}/{id} is gone or has changed")]
    Stale { kind: String, id: Id },
    /// 差し替え ([`crate::WriteOp::Patch`]) を当てられない形の本体 (map でない)。
    #[error("record {kind}/{id} has a body that cannot be patched")]
    BadBody {
        kind: String,
        id: Id,
        #[source]
        source: crate::envelope::BodyError,
    },
}

pub(crate) fn db<E: Into<redb::Error>>(e: E) -> StoreError {
    StoreError::Db(e.into())
}

/// 1 件を読んだ結果 (03 §8)。
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Ok(Envelope),
    /// チェックサムは合っているが読めない (新しい版の記録)。読み飛ばして数える。
    Newer {
        kind: String,
        id: Id,
        reason: String,
    },
    /// 壊れている。読み直して確かめてから quarantine へ移す。
    Corrupt {
        kind: String,
        id: Id,
        reason: &'static str,
    },
}

/// 種類ごとの件数 (`doctor` と、開いたときの調べ)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindCount {
    pub ok: u64,
    /// 知っている種類で、`v` が今の版より新しい記録。
    pub newer_v: u64,
    /// チェックサムは合うが読めない記録。
    pub unreadable: u64,
    pub corrupt: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    pub known: BTreeMap<String, KindCount>,
    /// 知らない種類 (読み飛ばして数える。消さない)。
    pub unknown: BTreeMap<String, u64>,
    /// キーとして読めない記録。
    pub bad_keys: u64,
}

/// 開いたストア。落とすと DB を閉じ、データ根のロックを外す。
pub struct Store {
    pub(crate) db: Database,
    root: PathBuf,
    version: String,
    plan: FormatPlan,
    pub(crate) kinds: BTreeMap<&'static str, KindSpec>,
    read_only: Option<ReadOnlyReason>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    /// 新しい `v` の記録が見つかった種類。
    newer_kinds: Mutex<BTreeSet<String>>,
    /// 読んだときに壊れていた記録のキー (書き込みのときに quarantine で扱う)。
    suspects: Mutex<BTreeSet<Vec<u8>>>,
    /// 書き込みの権利 (`WriteToken`) を渡したか。1 つのストアにつき 1 つだけ (02 §5)。
    pub(crate) token_issued: AtomicBool,
    /// ジャーナル (書き込みで開いたときだけ)。書き手だけが使う。
    pub(crate) journal: Mutex<Option<Journal>>,
    /// `memory.forget_key` が食い違っている (03 §3.1.1)。立っている間は、鍵を書かせない
    /// (呼び出し側が旗を見落としても、3 つ目の鍵を作れないようにする)。
    forget_key_conflict: AtomicBool,
    /// ジャーナルの削除の行 (開くときに集める) と、このプロセスで書き手が消した記録の ID。書き手は、これらを
    /// `put` で作り直さない (04 §7.7 の 3)。
    deleted: Mutex<HashSet<Id>>,
    /// 退避を作る処理と退避の整理を、同じ順序で通す (03 §7.3、04 §7.7 の 5)。
    backups: Mutex<()>,
    // ロックは DB の後に外す (フィールドは宣言の順に落ちる)。
    _lock: DataRootLock,
}

pub(crate) fn record_key(kind: &str, id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind).id(id);
    k.into_bytes()
}

fn kind_prefix(kind: &str) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind);
    k.into_bytes()
}

pub(crate) fn split_record_key(k: &[u8]) -> Option<(String, Id)> {
    match key::decode(k).ok()?.as_slice() {
        [OwnedPart::Str(kind), OwnedPart::Id(id)] => Some((kind.clone(), *id)),
        _ => None,
    }
}

pub(crate) fn classify(kind: &str, id: Id, k: &[u8], v: &[u8]) -> Read {
    match Envelope::decode(k, v) {
        Ok(env) if env.kind == kind && env.id == id => Read::Ok(env),
        Ok(_) => Read::Newer {
            kind: kind.to_owned(),
            id,
            reason: "envelope id or kind does not match its key".into(),
        },
        Err(DecodeError::Newer(reason)) => Read::Newer {
            kind: kind.to_owned(),
            id,
            reason,
        },
        Err(DecodeError::Corrupt(reason)) => Read::Corrupt {
            kind: kind.to_owned(),
            id,
            reason,
        },
    }
}

pub(crate) fn map_db_error(e: redb::DatabaseError) -> OpenError {
    match e {
        redb::DatabaseError::DatabaseAlreadyOpen => OpenError::DatabaseInUse,
        // redb のファイルの形式が新しい (壊れてはいない)。broken/ へ移さない (03 §4.2 の規則 6、§12.2)。
        redb::DatabaseError::UpgradeRequired(v) => OpenError::TooNew {
            format: format!("redb file format {v}"),
            min_read: "a newer evorag".into(),
        },
        // redb は、先頭の印が合わない・中身が読めないファイルを InvalidData の I/O のエラーで返す。
        redb::DatabaseError::Storage(redb::StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::InvalidData =>
        {
            OpenError::Unreadable(io.to_string())
        }
        redb::DatabaseError::Storage(redb::StorageError::Io(io)) => OpenError::Io(io),
        other => OpenError::Unreadable(other.to_string()),
    }
}

/// DB のファイルを開く。新しく作るのは、データ根に DB も移行の跡も無いときだけ
/// (移行の途中のファイルが残っているのに空の DB を作ると、次の後始末で元を失うため)。
fn open_db(root: &Path) -> Result<Database, OpenError> {
    let main = root.join(DB_FILE);
    match std::fs::metadata(&main) {
        Ok(m) if m.len() > 0 => {
            fsutil::prepare_private_file(&main)?;
            Database::open(&main).map_err(map_db_error)
        }
        Ok(_) => {
            // 作った直後 (中身を書く前) に止まった空のファイル。移行や compact の跡が残っていれば、空の DB を
            // 作らずに止める (原子的でない rename の途中で空になった本体かもしれず、跡が元の記録を持つため)。
            if let Some(left) = migrate::leftover(root) {
                return Err(OpenError::Unreadable(format!(
                    "{DB_FILE} is empty but {left} remains; not creating a new database"
                )));
            }
            fsutil::prepare_private_file(&main)?;
            Database::create(&main).map_err(map_db_error)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(left) = migrate::leftover(root) {
                return Err(OpenError::Unreadable(format!(
                    "{DB_FILE} is missing but {left} remains; not creating a new database"
                )));
            }
            fsutil::prepare_private_file(&main)?;
            Database::create(&main).map_err(map_db_error)
        }
        Err(e) => Err(OpenError::Io(e)),
    }
}

pub(crate) fn read_meta_db_any(
    database: &impl ReadableDatabase,
) -> Result<BTreeMap<String, String>, StoreError> {
    let rtx = database.begin_read().map_err(db)?;
    let table = match rtx.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(e) => return Err(db(e)),
    };
    let mut out = BTreeMap::new();
    for item in table.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        out.insert(
            String::from_utf8_lossy(k.value()).into_owned(),
            String::from_utf8_lossy(v.value()).into_owned(),
        );
    }
    Ok(out)
}

fn read_meta_db(database: &Database) -> Result<Option<BTreeMap<String, String>>, StoreError> {
    let rtx = database.begin_read().map_err(db)?;
    let table = match rtx.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(db(e)),
    };
    let mut out = BTreeMap::new();
    for item in table.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        out.insert(
            String::from_utf8_lossy(k.value()).into_owned(),
            String::from_utf8_lossy(v.value()).into_owned(),
        );
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

fn meta_version(m: &BTreeMap<String, String>, k: &str) -> Result<Version, OpenError> {
    m.get(k)
        .ok_or_else(|| OpenError::BadMeta(format!("missing `{k}`")))?
        .parse()
        .map_err(|e| OpenError::BadMeta(format!("`{k}`: {e}")))
}

/// 全記録を種類ごとに数える (読み取りのトランザクションの上で)。
pub(crate) fn census_of(
    rtx: &redb::ReadTransaction,
    kinds: &BTreeMap<&'static str, KindSpec>,
) -> Result<Census, StoreError> {
    let table = rtx.open_table(RECORDS).map_err(db)?;
    let mut c = Census::default();
    for item in table.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        let Some((kind, id)) = split_record_key(k.value()) else {
            c.bad_keys += 1;
            continue;
        };
        let Some(spec) = kinds.get(kind.as_str()) else {
            *c.unknown.entry(kind).or_default() += 1;
            continue;
        };
        let entry = c.known.entry(kind.clone()).or_default();
        match classify(&kind, id, k.value(), v.value()) {
            Read::Ok(env) if env.v > spec.current_v => entry.newer_v += 1,
            Read::Ok(_) => entry.ok += 1,
            Read::Newer { .. } => entry.unreadable += 1,
            Read::Corrupt { .. } => entry.corrupt += 1,
        }
    }
    Ok(c)
}

/// 消した記録の跡を compact で除くようになった版 (v0.3、03 §3.1.1 の `compact.pending`)。
const COMPACTING_SINCE: &str = "0.3.0";

/// `last_writer` が [`COMPACTING_SINCE`] より古いデータ根を、それ以降の版が書き込みで開いたか (古い版が開いた間に
/// 消した記録の跡が、ファイルの空きに残りうるので、次の compact を立てる、03 §3.1.1)。`last_writer` が無い・
/// 読めないときは立てない。
fn needs_compact_after_old_writer(existing: &BTreeMap<String, String>, ours: &Version) -> bool {
    let Ok(since) = COMPACTING_SINCE.parse::<Version>() else {
        return false;
    };
    *ours >= since
        && existing
            .get(meta::LAST_WRITER)
            .and_then(|w| w.parse::<Version>().ok())
            .is_some_and(|w| w < since)
}

/// v0.5 の形式の版 ([`FORMAT_V05`])。
fn v05() -> Version {
    FORMAT_V05.parse().expect("FORMAT_V05 is a version")
}

/// ストアが自分で持つ種類と、渡された種類。
pub(crate) fn kind_map(kinds: Vec<KindSpec>) -> BTreeMap<&'static str, KindSpec> {
    let mut m: BTreeMap<&'static str, KindSpec> =
        BUILTIN_KINDS.iter().map(|k| (k.name, k.clone())).collect();
    m.extend(kinds.into_iter().map(|k| (k.name, k)));
    m
}

impl Store {
    /// データ根を開く。無ければ作る。古い形式の DB は、この版の移行で今の形式にする。
    pub fn open(opts: OpenOptions) -> Result<(Self, OpenReport), OpenError> {
        Self::open_with(opts, FormatPlan::current())
    }

    /// 呼ぶ側が先に取ったデータ根のロック `lock` を持ったまま開く (起動の更新の片付けから、ロックを放さずに
    /// 続けるため、12 §7.3)。
    ///
    /// # Panics
    ///
    /// `lock` が `opts.data_root` のロックでないとき、debug のビルドでは panic する (呼ぶ側の誤り)。release の
    /// ビルドでは、ログに残し、渡されたロックを放して `opts.data_root` のロックを取り直す。
    pub fn open_locked(
        opts: OpenOptions,
        lock: DataRootLock,
    ) -> Result<(Self, OpenReport), OpenError> {
        Self::open_inner(opts, FormatPlan::current(), Some(lock))
    }

    /// 書く形式と移行を指定して開く (移行の試験のため)。
    pub fn open_with(opts: OpenOptions, plan: FormatPlan) -> Result<(Self, OpenReport), OpenError> {
        Self::open_inner(opts, plan, None)
    }

    fn open_inner(
        opts: OpenOptions,
        plan: FormatPlan,
        held: Option<DataRootLock>,
    ) -> Result<(Self, OpenReport), OpenError> {
        fsutil::create_private_dir(&opts.data_root)?;
        let wanted = opts.data_root.join(crate::lock::LOCK_FILE);
        let lock = match held {
            Some(l) if l.path() == wanted => l,
            other => {
                if let Some(l) = &other {
                    debug_assert!(
                        l.path() == wanted,
                        "the held lock {} is for another data root than {}",
                        l.path().display(),
                        opts.data_root.display()
                    );
                    // パスは利用者の名前を含みうるので、ログには出さない (NFR-502)。
                    tracing::warn!(
                        "the data root lock given to Store::open_locked is for another root; \
                         releasing it and taking the lock of the data root being opened"
                    );
                }
                drop(other);
                DataRootLock::acquire(&opts.data_root)?
            }
        };
        migrate::recover(&opts.data_root)?;
        crate::compact::recover(&opts.data_root, opts.clock.now().as_millis())?;
        let mut database = match open_db(&opts.data_root) {
            // 開けない DB のファイルは broken/ へ移し (消さない)、戻すかを尋ねる (03 §8、FR-205)。移行や compact の
            // 跡が残っているときは移さない (跡の側に元の記録がありうるので、退避から戻させない)。
            Err(OpenError::Unreadable(reason))
                if opts.data_root.join(DB_FILE).is_file()
                    && migrate::leftover(&opts.data_root).is_none() =>
            {
                let moved_to =
                    crate::restore::move_broken(&opts.data_root, opts.clock.now().as_millis())?;
                return Err(OpenError::Broken { moved_to, reason });
            }
            r => r?,
        };

        let ours: Version = plan
            .version
            .parse()
            .map_err(|e| OpenError::BadMeta(format!("format version: {e}")))?;
        let our_writer: Version = opts
            .evorag_version
            .parse()
            .map_err(|e| OpenError::BadMeta(format!("evorag version: {e}")))?;

        let mut report = OpenReport::default();
        let mut read_only = None;
        let mut existing =
            read_meta_db(&database).map_err(|e| OpenError::BadMeta(e.to_string()))?;
        if let Some(m) = &existing {
            let format = meta_version(m, meta::FORMAT_VERSION)?;
            let min_read = meta_version(m, meta::FORMAT_MIN_READ)?;
            if min_read > ours {
                return Err(OpenError::TooNew {
                    format: format.to_string(),
                    min_read: min_read.to_string(),
                });
            }
            // v0.5 の根は、v0.4 以前の形式の根を在り場所で移行しない (03 §4.1・§12.1・§12.4)。
            if format < ours && ours >= v05() && format < v05() {
                return Err(OpenError::NeedsImport {
                    format: format.to_string(),
                });
            }
            if format < ours {
                let now = opts.clock.now().as_millis();
                let extra: Vec<&Path> = opts.config_file.as_deref().into_iter().collect();
                match migrate::run(database, &opts.data_root, &format, &plan, now, &extra) {
                    migrate::Outcome::Migrated { db, backup } => {
                        database = db;
                        report.migrated_from = Some(format.to_string());
                        report.pre_op_backup = Some(backup);
                        existing = read_meta_db(&database)
                            .map_err(|e| OpenError::BadMeta(e.to_string()))?;
                    }
                    migrate::Outcome::Failed { db, reason } => {
                        database = db;
                        read_only = Some(ReadOnlyReason::MigrationFailed { reason });
                    }
                    migrate::Outcome::Fatal(reason) => return Err(OpenError::Unreadable(reason)),
                }
            }
        }

        let mut kinds: BTreeMap<&'static str, KindSpec> =
            BUILTIN_KINDS.iter().map(|k| (k.name, k.clone())).collect();
        kinds.extend(opts.kinds.into_iter().map(|k| (k.name, k)));

        let mut store = Self {
            db: database,
            root: opts.data_root,
            version: opts.evorag_version,
            plan,
            kinds,
            read_only,
            clock: opts.clock,
            ids: opts.ids,
            newer_kinds: Mutex::default(),
            suspects: Mutex::default(),
            token_issued: AtomicBool::new(false),
            journal: Mutex::new(None),
            forget_key_conflict: AtomicBool::new(false),
            deleted: Mutex::default(),
            backups: Mutex::new(()),
            _lock: lock,
        };

        let Some(existing) = existing else {
            store
                .init()
                .map_err(|e| OpenError::Unreadable(e.to_string()))?;
            report.created = true;
            store.open_journal(&mut report)?;
            return Ok((store, report));
        };

        let min_write = meta_version(&existing, meta::FORMAT_MIN_WRITE)?;
        if store.read_only.is_none() && min_write > ours {
            store.read_only = Some(ReadOnlyReason::NewerFormat {
                min_write: min_write.to_string(),
            });
        }
        if let Some(max) = existing.get(meta::MAX_WRITER)
            && max.parse::<Version>().is_ok_and(|m| m > our_writer)
        {
            report.newer_writer_seen = Some(max.clone());
            // 巻き戻しのときだけ全件を調べ、新しい `v` の記録がある種類を覚える。
            let census = store
                .census()
                .map_err(|e| OpenError::Unreadable(e.to_string()))?;
            let mut newer = store.newer_kinds.lock().unwrap_or_else(|e| e.into_inner());
            for (kind, c) in census.known {
                if c.newer_v > 0 {
                    newer.insert(kind);
                }
            }
        }
        report.read_only = store.read_only.clone();
        if store.read_only.is_none() {
            store
                .record_writer(&existing, &our_writer)
                .map_err(|e| OpenError::Unreadable(e.to_string()))?;
            store.open_journal(&mut report)?;
        } else {
            // 読むだけのときは書かずに、鍵の食い違いと「ジャーナルにだけある」状態を調べる。
            let metas = crate::journal::scan_metas(&store.root)?;
            store.check_forget_key(&metas, false, &mut report)?;
        }
        Ok((store, report))
    }

    /// `memory.forget_key` が食い違っているか (03 §3.1.1)。立っていれば、事実の抽出・統合・
    /// 事実の表を止める。
    pub fn forget_key_conflict(&self) -> bool {
        self.forget_key_conflict.load(Ordering::SeqCst)
    }

    /// 会話の追記のための予備のファイル (03 §8)。無ければ作る (0 で埋めて、実際に場所を取る)。
    /// 空きが足りなければ作らない。
    fn ensure_reserve(&self) {
        let path = self.root.join(RESERVE_FILE);
        if path.exists() {
            return;
        }
        let enough = crate::location::free_bytes(&self.root).is_some_and(|f| f > 4 * RESERVE_BYTES);
        if !enough {
            return;
        }
        let made = (|| -> std::io::Result<()> {
            fsutil::prepare_private_file(&path)?;
            let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
            let zeros = vec![0u8; 1024 * 1024];
            for _ in 0..(RESERVE_BYTES / zeros.len() as u64) {
                std::io::Write::write_all(&mut f, &zeros)?;
            }
            f.sync_all()
        })();
        if made.is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// 空きが尽きたときに、予備のファイルを消して場所を空ける (03 §8)。消したら `true`。
    pub(crate) fn release_reserve(&self) -> bool {
        std::fs::remove_file(self.root.join(RESERVE_FILE)).is_ok()
    }

    /// ジャーナルを開き、DB にまだ無い行を取り込む (03 §6)。読むだけで開いたときは呼ばない。
    fn open_journal(&self, report: &mut OpenReport) -> Result<(), OpenError> {
        self.ensure_reserve();
        let last: u64 = self
            .meta(meta::JOURNAL_LAST_SEQ)
            .map_err(|e| OpenError::BadMeta(e.to_string()))?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let monotonic: crate::journal::Monotonic = self
            .kinds
            .values()
            .filter_map(|k| Some((k.name, k.monotonic?)))
            .collect();
        // 末尾の書きかけの組を捨ててよいのは、書き手が使っていた DB だけ (03 §6 の「組の行」)。
        let tail_unknown = self
            .meta(meta::JOURNAL_TAIL_UNKNOWN)
            .map_err(|e| OpenError::BadMeta(e.to_string()))?
            .is_some();
        let tail = if report.created || tail_unknown {
            crate::journal::Tail::Unknown
        } else {
            crate::journal::Tail::Uncommitted
        };
        let (journal, scan) = Journal::open(&self.root, last, monotonic, tail)?;
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(scan.deleted.keys().copied());
        report.journal_bad_lines = scan.bad;
        report.journal_newer_lines = scan.newer;
        report.journal_torn_groups = scan.torn_groups;
        report.journal_incomplete_groups = scan.incomplete_groups;
        report.journal_truncated = scan.truncated;
        report.journal_rewrite_failed = scan.rewrite_failed.clone();
        let unreadable = |e: StoreError| OpenError::Unreadable(e.to_string());
        // 削除の行があるのに DB に残っている記録 (古い版が書き戻したもの) を、読み取りのトランザクションで探す
        // (毎回の起動で空の書き込みをしないため)。見つけた記録は、ほかの記録からつながっていても消す (消したと
        // みなす ID の記録を生きたまま残さない、03 §6)。出典のセッションを失ったターンは消さずに残す (呼び手が
        // 数えて知らせる)。
        let mut stale_alive = Vec::new();
        if !scan.deleted.is_empty() {
            let rtx = self.read().map_err(unreadable)?;
            for (id, kind) in &scan.deleted {
                if rtx.get(kind, *id).map_err(unreadable)?.is_some() {
                    stale_alive.push((*id, kind.clone()));
                }
            }
        }
        if !scan.lines.is_empty() || !stale_alive.is_empty() || tail_unknown {
            let mut tx = self
                .write_internal(Durability::Immediate)
                .map_err(unreadable)?;
            // 記録を消した・伏せ字で書き換えた (次の起動で compact が要る、03 §3.1.1)。
            let mut freed = false;
            for line in &scan.lines {
                match &line.op {
                    // 削除の行のある ID の `put` の行は当てない (古い版が消した後に書き戻した行で、消したものを
                    // 蘇らせないため、04 §7.7 の 3)。
                    LineOp::Put(env) if scan.deleted.contains_key(&env.id) => continue,
                    LineOp::Delete { id, .. } => report.journal_deletes.push(*id),
                    _ => {}
                }
                freed |= self.replay_line(&mut tx, &line.op).map_err(unreadable)?;
            }
            for (id, kind) in stale_alive {
                if report.journal_deletes.contains(&id) {
                    continue;
                }
                freed |= self
                    .replay_line(&mut tx, &LineOp::Delete { kind, id })
                    .map_err(unreadable)?;
                report.journal_deletes.push(id);
            }
            if freed {
                tx.set_compact_pending().map_err(unreadable)?;
            }
            // 開くときに消した ID は、同じトランザクションで「索引と退避から除く ID」として残す (呼び手が整理の
            // job にしてから消す。job を書く前に落ちても失わないため)。
            if !report.journal_deletes.is_empty() {
                tx.add_pending_deletes(&report.journal_deletes)
                    .map_err(unreadable)?;
            }
            // 行の欠けた組の読める行を当てた: 忘れるの `end` を計画し直す印に、その組の記録の ID を残す (03 §6)。
            if !scan.partial.is_empty() {
                tx.add_ids_meta(meta::JOURNAL_PARTIAL_GROUPS, &scan.partial)
                    .map_err(unreadable)?;
            }
            if tail_unknown {
                tx.remove_meta(meta::JOURNAL_TAIL_UNKNOWN)
                    .map_err(unreadable)?;
            }
            if let Some(last_line) = scan.lines.last() {
                tx.set_journal_seq(last_line.seq).map_err(unreadable)?;
            }
            tx.commit().map_err(unreadable)?;
            report.journal_replayed = scan.lines.len();
        }
        self.check_forget_key(&scan.metas, true, report)?;
        *self.journal.lock().unwrap_or_else(|e| e.into_inner()) = Some(journal);
        // ジャーナルの取り込みの後に、記録した ID の最大を ID の下限にする (03 §3.3)。
        if let Some(max) = self
            .max_record_id()
            .map_err(|e| OpenError::Unreadable(e.to_string()))?
        {
            self.ids.raise_floor(max);
        }
        // 消した ID も下限に入れる (消した記録の ID を、新しい記録に使い直さないため)。
        if let Some(max) = scan.deleted.keys().next_back() {
            self.ids.raise_floor(*max);
        }
        Ok(())
    }

    /// 開くときにジャーナルの削除の行から消した記録の ID のうち、まだ整理の job にしていないもの (03 §7.3)。
    /// 呼び手は、job を書く依頼に [`crate::WriteOp::ClearPendingDeletes`] を入れて消す。
    ///
    /// # Errors
    /// `meta` を読めないとき。
    pub fn pending_deletes(&self) -> Result<PendingDeletes, StoreError> {
        Ok(self
            .meta(meta::PENDING_DELETES)?
            .map(|v| parse_ids(&v))
            .unwrap_or_default())
    }

    /// 行の欠けた組の読める行を当てたときの、その組の記録の ID (`meta` の `journal.partial_groups`、03 §6)。忘れるの
    /// 組で `end` が欠けたときに `end` を計画し直す印 (04 §7.4、M5.4)。
    ///
    /// # Errors
    /// `meta` を読めないとき。
    pub fn partial_groups(&self) -> Result<PendingDeletes, StoreError> {
        Ok(self
            .meta(meta::JOURNAL_PARTIAL_GROUPS)?
            .map(|v| parse_ids(&v))
            .unwrap_or_default())
    }

    /// 知っている種類の記録の ID の最大 (種類ごとの最後の記録を見る。全件は読まない、NFR-101)。
    pub fn max_record_id(&self) -> Result<Option<Id>, StoreError> {
        let tx = self.db.begin_read().map_err(db)?;
        let table = tx.open_table(RECORDS).map_err(db)?;
        let mut max = None;
        // 中身から ID を作る種類は時刻の順に並ばないので、下限に入れない (03 §3.3)。
        let generated = self
            .kinds
            .values()
            .filter(|k| k.ids == RecordIds::Generated)
            .map(|k| k.name);
        for kind in generated.chain([KIND_QUARANTINE]) {
            let start = kind_prefix(kind);
            let end = key::prefix_end(&start);
            let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            let mut range = table
                .range::<&[u8]>((Bound::Included(start.as_slice()), end))
                .map_err(db)?;
            if let Some(item) = range.next_back() {
                let (k, _) = item.map_err(db)?;
                if let Some((_, id)) = split_record_key(k.value()) {
                    max = max.max(Some(id));
                }
            }
        }
        Ok(max)
    }

    /// ジャーナルの `memory.forget_key` の行を、連番に関わらず探す (03 §3.1.1)。
    ///
    /// v0.2.x は、この `meta` の行を取り込まずに連番だけを進めるので、v0.2.x で開いた後は、連番での
    /// 取り込みでは二度と入らない。そのため、通常の取り込み (`replay_line`) ではこの行を当てず、ここだけで
    /// 扱う (取り込みの経路で結果が変わらないようにするため)。
    /// - DB に無く、ジャーナルに正しい形の値が 1 つだけあれば、それを入れる (`writable` のときだけ。読む
    ///   だけのときは入れずに `forget_key_pending` を立てる)。
    /// - ジャーナルの値が 2 つ以上ある・形が違う・DB の値と違うときは、書き換えずに食い違いの旗を立てる
    ///   (別の鍵で作ると private と再び覚えない印が黙って効かなくなるので、使う側が止める)。
    fn check_forget_key(
        &self,
        metas: &[(String, String)],
        writable: bool,
        report: &mut OpenReport,
    ) -> Result<(), OpenError> {
        let mut found: Vec<&str> = metas
            .iter()
            .filter(|(k, _)| k == meta::FORGET_KEY)
            .map(|(_, v)| v.as_str())
            .collect();
        found.sort_unstable();
        found.dedup();
        let in_db = self
            .meta(meta::FORGET_KEY)
            .map_err(|e| OpenError::BadMeta(e.to_string()))?;
        let bad_shape = in_db.as_deref().is_some_and(|v| !is_forget_key(v))
            || found.iter().any(|v| !is_forget_key(v));
        let conflict = bad_shape
            || match (in_db.as_deref(), found.as_slice()) {
                (_, []) => false,
                (Some(db), [one]) => db != *one,
                (None, [_]) => false,
                _ => true,
            };
        if conflict {
            report.forget_key_conflict = true;
            self.forget_key_conflict.store(true, Ordering::SeqCst);
            return Ok(());
        }
        if let (None, [one]) = (in_db.as_deref(), found.as_slice()) {
            if !writable {
                report.forget_key_pending = true;
                return Ok(());
            }
            let unreadable = |e: StoreError| OpenError::Unreadable(e.to_string());
            let mut tx = self
                .write_internal(Durability::Immediate)
                .map_err(unreadable)?;
            tx.set_meta_once(meta::FORGET_KEY, one)
                .map_err(unreadable)?;
            tx.commit().map_err(unreadable)?;
            report.forget_key_recovered = true;
        }
        Ok(())
    }

    /// ジャーナルの 1 行を、何度当てても同じになるように当てる。記録を消したか、単調に増える項目 (ターンの改訂の
    /// 番号) を上げて書き換えた (伏せ字の前の本文がファイルの空きに残りうる) ときに `true` を返す。
    fn replay_line(&self, tx: &mut WriteTxn<'_>, op: &LineOp) -> Result<bool, StoreError> {
        let mut freed = false;
        match op {
            LineOp::Put(env) => {
                // 通常の書き込みと同じ規則で当てる (03 §6): 新しい `v` の記録と読めない記録は上書き
                // しない。順序は連番で決まるので、時刻は比べない。
                let spec = self.kinds.get(env.kind.as_str());
                match tx.get(&env.kind, env.id)? {
                    Some(Read::Ok(old)) => {
                        if spec.is_some_and(|s| old.v > s.current_v) {
                            return Ok(false);
                        }
                        // 単調に増える項目 (ターンの改訂の番号) を小さくする行は当てない (伏せ字の前の行で、
                        // 伏せ字を戻さないため、03 §6)。
                        if spec.is_some_and(|s| s.lowers_monotonic(&old, env)) {
                            return Ok(false);
                        }
                        freed = self.raises_monotonic(&old, env);
                        self.undo_derived(tx, &old)?;
                    }
                    Some(Read::Newer { .. } | Read::Corrupt { .. }) => return Ok(false),
                    None => {}
                }
                tx.put_raw(env)?;
                if let Some(derive) = spec.and_then(|k| k.derive) {
                    let mut out = crate::writer::WriteOutcome::default();
                    for op in derive(env) {
                        crate::writer::apply_op(tx, &op, &mut out)?;
                    }
                }
            }
            LineOp::Delete { kind, id } => {
                if let Some(Read::Ok(old)) = tx.get(kind, *id)? {
                    self.undo_derived(tx, &old)?;
                }
                freed = tx.delete(kind, *id)?;
                tx.delete_links_of(*id)?;
                tx.delete_blobs(*id)?;
            }
            // `memory.forget_key` は `check_forget_key` だけで扱う (03 §3.1.1)。
            LineOp::Meta { key, .. } if key == meta::FORGET_KEY => {}
            // 上書きしてよいキーは、連番の最も大きい行の値を採る (行は連番の順に当てる。03 §3.1.1)。
            LineOp::Meta { key, value }
                if key == meta::IMPORT_OLD_ROOT || key == meta::IMPORT_V04_ENDED =>
            {
                match tx.set_meta_latest(key, value) {
                    Ok(()) | Err(StoreError::MetaConflict { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            LineOp::Meta { key, value } => match tx.set_meta_once(key, value) {
                Ok(()) | Err(StoreError::MetaConflict { .. } | StoreError::MetaNotWritable(_)) => {}
                Err(e) => return Err(e),
            },
        }
        Ok(freed)
    }

    /// 記録から関係と副索引を作る (書き手の差し替え `Patch` で、読み直した封筒から作り直すため)。
    pub(crate) fn redo_derived(
        &self,
        tx: &mut WriteTxn<'_>,
        env: &Envelope,
    ) -> Result<(), StoreError> {
        let Some(derive) = self.kinds.get(env.kind.as_str()).and_then(|k| k.derive) else {
            return Ok(());
        };
        let mut out = crate::writer::WriteOutcome::default();
        for op in derive(env) {
            crate::writer::apply_op(tx, &op, &mut out)?;
        }
        Ok(())
    }

    /// 記録から作った関係と副索引を消す (上書き・削除の前に)。
    pub(crate) fn undo_derived(
        &self,
        tx: &mut WriteTxn<'_>,
        old: &Envelope,
    ) -> Result<(), StoreError> {
        let Some(derive) = self.kinds.get(old.kind.as_str()).and_then(|k| k.derive) else {
            return Ok(());
        };
        let mut out = crate::writer::WriteOutcome::default();
        for op in derive(old) {
            let undo = match op {
                WriteOp::PutLink { from, rel, to, .. } => WriteOp::DeleteLink { from, rel, to },
                WriteOp::PutLookup {
                    kind,
                    field,
                    value,
                    id,
                } => WriteOp::DeleteLookup {
                    kind,
                    field,
                    value,
                    id,
                },
                _ => continue,
            };
            crate::writer::apply_op(tx, &undo, &mut out)?;
        }
        Ok(())
    }

    /// 書き込みのトランザクション (書き込みの権利を経ない。開くときの取り込みだけで使う)。
    pub(crate) fn write_internal(
        &self,
        durability: Durability,
    ) -> Result<WriteTxn<'_>, StoreError> {
        let mut tx = self.db.begin_write().map_err(db)?;
        tx.set_durability(durability.into()).map_err(db)?;
        Ok(WriteTxn { store: self, tx })
    }

    /// 1 日 1 回までの自動の退避 (03 §7.1)。`backups` を省くとデータ根の `backups/`。
    /// 読み取りのトランザクションで写すので、書き手と並んで動かしてよい。退避の整理
    /// ([`Self::purge_backups`]) とは同じ順序で通す (03 §7.3)。
    pub fn daily_backup(&self, backups: Option<&Path>) -> Result<Option<PathBuf>, StoreError> {
        let _order = self.backup_order();
        // 前に強制終了で残った作りかけの退避を先に片付ける (消せなければ、次の整理で失敗として残る)。
        let _ = crate::restore::sweep_leftovers(&self.root, backups);
        let default = self.root.join(crate::backup::BACKUPS_DIR);
        crate::backup::daily(
            &self.db,
            backups.unwrap_or(&default),
            self.clock.now().as_millis(),
        )
    }

    /// 消した記録 `ids` を、保持している退避から除いて写し直す (03 §7.3)。写し直した退避の数を返す。
    ///
    /// `stop` が真を返したら、次の退避に移る前に止めて失敗を返す (終了を待たせない。job から続ける)。
    ///
    /// 退避を作る処理と同じ順序で通す: 作りかけの退避があれば、それを作り終えるのを待ってから一覧を作る
    /// (削除より前に写し始め、整理の job を書いた後に作り終えた退避も、整理の対象にするため、04 §7.7 の 5)。
    ///
    /// # Errors
    /// 写し直せない退避、または消せない作りかけの退避があるとき (呼び手は job を残して再試行する)。
    pub fn purge_backups(
        &self,
        backups: Option<&Path>,
        ids: &[Id],
        stop: &dyn Fn() -> bool,
    ) -> Result<usize, StoreError> {
        let _order = self.backup_order();
        // 作りかけの退避 (強制終了で残った写し) は、整理の対象にならないまま本文を持ち続けるので、先に消す。
        let leftovers = crate::restore::sweep_leftovers(&self.root, backups);
        let purged = crate::restore::purge_backups_until(&self.root, backups, ids, stop)?;
        if !leftovers.is_empty() {
            return Err(StoreError::Io(format!(
                "partial backups could not be removed: {}",
                leftovers.join("; ")
            )));
        }
        Ok(purged)
    }

    pub(crate) fn backup_order(&self) -> std::sync::MutexGuard<'_, ()> {
        self.backups.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 消した記録の ID か (ジャーナルの削除の行と、このプロセスで書き手が消したもの、04 §7.7 の 3)。
    pub fn was_deleted(&self, id: Id) -> bool {
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&id)
    }

    /// 書き手が消した記録の ID を覚える (コミットの後に)。
    pub(crate) fn remember_deleted(&self, ids: impl IntoIterator<Item = Id>) {
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(ids);
    }

    /// 作り直しなどの操作の前の退避 (03 §7.2)。DB の写しと `extra_files` (`config.toml` など) を、
    /// `backups/pre-op/` に作る。作ったフォルダを返す。
    pub fn pre_op_backup(
        &self,
        operation: &str,
        extra_files: &[&Path],
    ) -> Result<PathBuf, StoreError> {
        let _order = self.backup_order();
        crate::backup::pre_op(
            &self.db,
            &self.root,
            self.clock.now().as_millis(),
            operation,
            "current",
            extra_files,
        )
    }

    /// その日の日次の退避がまだ無いか (03 §7.1)。
    pub fn daily_backup_due(&self, backups: Option<&Path>) -> bool {
        let default = self.root.join(crate::backup::BACKUPS_DIR);
        crate::backup::daily_due(backups.unwrap_or(&default), self.clock.now().as_millis())
    }

    /// その種類をジャーナルにも書くか。
    /// その種類の ID が中身から作るものか ([`RecordIds::Content`])。消した ID として覚えない (03 §4.2 の規則 0)。
    pub(crate) fn has_content_ids(&self, kind: &str) -> bool {
        self.kinds
            .get(kind)
            .is_some_and(|k| k.ids == RecordIds::Content)
    }

    pub(crate) fn is_journaled(&self, kind: &str) -> bool {
        self.kinds.get(kind).is_some_and(|k| k.journaled)
    }

    /// その種類が単調に増える項目 (ターンの改訂の番号) を宣言しているか。
    pub(crate) fn has_monotonic(&self, kind: &str) -> bool {
        self.kinds.get(kind).is_some_and(|k| k.monotonic.is_some())
    }

    /// `old` を `new` で書き換えると、宣言した単調に増える項目が上がるか (伏せ字で書き換えた。前の本文がファイルの
    /// 空きに残りうるので、次の起動で compact する、03 §3.1.1)。
    pub(crate) fn raises_monotonic(&self, old: &Envelope, new: &Envelope) -> bool {
        self.kinds
            .get(new.kind.as_str())
            .is_some_and(|s| s.lowers_monotonic(new, old))
    }

    /// ジャーナルに集めた、その ID の `put` の行の単調に増える項目の値の範囲 (開くときに集め、このプロセスの
    /// 追記と書き直しで更新する)。忘れるの書き直しが済んでいないこと (`min < 伏せ字の後の番号 <= max`) を
    /// 見つけるのに使う (04 §7.2 の「ジャーナルの層の見つけ方」)。読むだけで開いたときは `None`。
    pub fn journal_rev_span(&self, id: Id) -> Option<crate::journal::RevSpan> {
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .rev_span(id)
    }

    /// 試験のための注入: 次のジャーナルの書き直しを、決まった点で失敗させる (09 §3.5)。
    #[doc(hidden)]
    pub fn inject_journal_rewrite_fault(&self, fault: crate::journal::RewriteFault) {
        if let Some(j) = self
            .journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            j.inject_rewrite_fault(fault);
        }
    }

    /// 試験のための注入: 次のジャーナルの追記を失敗させる (AC-FR-202-1)。
    #[doc(hidden)]
    pub fn fail_next_journal_append(&self) {
        if let Some(j) = self
            .journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            j.fail_next_append();
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.root
    }

    pub fn read_only(&self) -> Option<&ReadOnlyReason> {
        self.read_only.as_ref()
    }

    pub fn evorag_version(&self) -> &str {
        &self.version
    }

    /// この版が書く DB の形式の版 (開くときに渡した [`FormatPlan`] の版)。v0.5 の根なら [`FORMAT_V05`]。
    pub fn format_version(&self) -> &str {
        &self.plan.version
    }

    /// v0.5 の新しいデータ根 (形式の版 [`FORMAT_V05`] 以上) として開いているか (03 §3.3 の「v0.5 の根で作らない
    /// 種類」・§6 の組の行・§7.1・§11 の範囲を決める)。
    pub fn is_v05_root(&self) -> bool {
        self.plan
            .version
            .parse::<Version>()
            .is_ok_and(|v| v >= v05())
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn ids(&self) -> &Arc<dyn IdGen> {
        &self.ids
    }

    fn read_meta(&self) -> Result<Option<BTreeMap<String, String>>, StoreError> {
        read_meta_db(&self.db)
    }

    /// 新しい DB に表と `meta` を作る (03 §3.1.1)。
    fn init(&self) -> Result<(), StoreError> {
        let mut tx = self.db.begin_write().map_err(db)?;
        tx.set_durability(redb::Durability::Immediate).map_err(db)?;
        for t in TABLES {
            tx.open_table(t).map_err(db)?;
        }
        {
            let mut m = tx.open_table(META).map_err(db)?;
            let now = self.clock.now().as_millis().to_string();
            let root_id = self.ids.next_id().to_string();
            let writer_key = format!("{}{}", meta::WRITER_PREFIX, self.version);
            let entries: [(&str, &str); 12] = [
                (meta::FORMAT_VERSION, &self.plan.version),
                (meta::FORMAT_MIN_READ, &self.plan.min_read),
                (meta::FORMAT_MIN_WRITE, &self.plan.min_write),
                (meta::KEY_ENCODING_VERSION, "1"),
                (meta::DATA_ROOT_ID, &root_id),
                (meta::CREATED_BY, &self.version),
                (meta::LAST_WRITER, &self.version),
                (meta::MAX_WRITER, &self.version),
                (&writer_key, &now),
                (meta::MIGRATION_STATE, meta::MIGRATION_NONE),
                (meta::JOURNAL_LAST_SEQ, "0"),
                (meta::ENVELOPE_LAYOUT, "1"),
            ];
            for (k, v) in entries {
                m.insert(k.as_bytes(), v.as_bytes()).map_err(db)?;
            }
        }
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// 書き込みで開いた版を記録する (03 §3.1.1)。
    fn record_writer(
        &self,
        existing: &BTreeMap<String, String>,
        ours: &Version,
    ) -> Result<(), StoreError> {
        let mut tx = self.db.begin_write().map_err(db)?;
        tx.set_durability(redb::Durability::Immediate).map_err(db)?;
        {
            let mut m = tx.open_table(META).map_err(db)?;
            m.insert(meta::LAST_WRITER.as_bytes(), self.version.as_bytes())
                .map_err(db)?;
            let max_is_older = existing
                .get(meta::MAX_WRITER)
                .and_then(|m| m.parse::<Version>().ok())
                .is_none_or(|m| m < *ours);
            if max_is_older {
                m.insert(meta::MAX_WRITER.as_bytes(), self.version.as_bytes())
                    .map_err(db)?;
            }
            if needs_compact_after_old_writer(existing, ours) {
                m.insert(
                    meta::COMPACT_PENDING.as_bytes(),
                    crate::compact::PENDING.as_bytes(),
                )
                .map_err(db)?;
                // 規則 12 の (1): 起動時に、すべての忘れるの指示の退避の写し直しの job を作り直させる (03 §4.2)。
                m.insert(
                    meta::FORGET_RECHECK.as_bytes(),
                    meta::FORGET_RECHECK_SET.as_bytes(),
                )
                .map_err(db)?;
            }
            let wk = format!("{}{}", meta::WRITER_PREFIX, self.version);
            if !existing.contains_key(&wk) {
                let now = self.clock.now().as_millis().to_string();
                m.insert(wk.as_bytes(), now.as_bytes()).map_err(db)?;
            }
        }
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// `meta` の 1 つの値。
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.read_meta()?.and_then(|mut m| m.remove(key)))
    }

    /// 読み取りのトランザクション (一貫した写し)。
    pub fn read(&self) -> Result<ReadTxn<'_>, StoreError> {
        Ok(ReadTxn {
            store: self,
            tx: self.db.begin_read().map_err(db)?,
        })
    }

    /// 書き込みの権利を受け取る。1 つのストアにつき 1 回だけ渡す (2 回目からは `None`)。
    /// 受け取るのは書き手 (`Writer`) だけにする (02 §5 の「書き込みの入口は 1 つ」)。
    pub fn take_write_token(&self) -> Option<WriteToken> {
        if self.token_issued.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(WriteToken { _private: () })
        }
    }

    /// 書き込みのトランザクション。会話の追記と明示の操作は `Durability::Immediate`、
    /// 作り直せる裏の処理の結果は `Durability::None` (03 §3.4)。
    pub fn write(
        &self,
        _token: &WriteToken,
        durability: Durability,
    ) -> Result<WriteTxn<'_>, StoreError> {
        if let Some(r) = &self.read_only {
            return Err(StoreError::ReadOnly(r.clone()));
        }
        let mut tx = self.db.begin_write().map_err(db)?;
        tx.set_durability(durability.into()).map_err(db)?;
        Ok(WriteTxn { store: self, tx })
    }

    /// 全記録を種類ごとに数える (03 §4.2 の規則 6、`doctor`)。
    pub fn census(&self) -> Result<Census, StoreError> {
        let rtx = self.db.begin_read().map_err(db)?;
        census_of(&rtx, &self.kinds)
    }

    fn note_read(&self, r: &Read) {
        match r {
            Read::Ok(env) => {
                if self
                    .kinds
                    .get(env.kind.as_str())
                    .is_some_and(|s| env.v > s.current_v)
                {
                    self.newer_kinds
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(env.kind.clone());
                }
            }
            Read::Corrupt { kind, id, .. } => {
                self.suspects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(record_key(kind, *id));
            }
            Read::Newer { .. } => {}
        }
    }

    /// 読んだときに壊れていた記録のキーを取り出す (quarantine へ移すため)。
    pub(crate) fn take_suspects(&self) -> BTreeSet<Vec<u8>> {
        std::mem::take(&mut *self.suspects.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// 移せなかったキーを戻す (次の書き込みでもう一度試す)。
    pub(crate) fn restore_suspects(&self, keys: BTreeSet<Vec<u8>>) {
        self.suspects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(keys);
    }

    /// 読んだときに壊れていた記録の数 (まだ quarantine で扱っていないもの)。
    pub fn suspect_count(&self) -> usize {
        self.suspects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// 読み取りと書き込みのトランザクションで共通の読み方。
macro_rules! record_reads {
    () => {
        /// 1 件を読む。壊れた記録は覚えておき、書き込みのときに quarantine で扱う。
        pub fn get(&self, kind: &str, id: Id) -> Result<Option<Read>, StoreError> {
            let table = self.tx.open_table(RECORDS).map_err(db)?;
            let k = record_key(kind, id);
            let Some(v) = table.get(k.as_slice()).map_err(db)? else {
                return Ok(None);
            };
            let r = classify(kind, id, &k, v.value());
            self.store.note_read(&r);
            Ok(Some(r))
        }

        /// 1 つの種類の記録を ID の順に読む。`f` が `false` を返したら止める。
        pub fn for_each(&self, kind: &str, f: impl FnMut(Read) -> bool) -> Result<(), StoreError> {
            self.for_each_from(kind, Bound::Included(kind_prefix(kind)), f)
        }

        /// 1 つの種類の記録のうち、ID が `after` より後のものを ID の順に読む。`f` が `false` を返したら
        /// 止める。表を `after` の次のキーから引くので、前の記録は読まない。
        pub fn for_each_after(
            &self,
            kind: &str,
            after: Id,
            f: impl FnMut(Read) -> bool,
        ) -> Result<(), StoreError> {
            self.for_each_from(kind, Bound::Excluded(record_key(kind, after)), f)
        }

        /// `start` から、1 つの種類の記録の終わりまでを ID の順に読む。
        fn for_each_from(
            &self,
            kind: &str,
            start: Bound<Vec<u8>>,
            mut f: impl FnMut(Read) -> bool,
        ) -> Result<(), StoreError> {
            let table = self.tx.open_table(RECORDS).map_err(db)?;
            let end = key::prefix_end(&kind_prefix(kind));
            let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            let range = table
                .range::<&[u8]>((start.as_ref().map(Vec::as_slice), end))
                .map_err(db)?;
            for item in range {
                let (k, v) = item.map_err(db)?;
                let Some((_, id)) = split_record_key(k.value()) else {
                    continue;
                };
                let r = classify(kind, id, k.value(), v.value());
                self.store.note_read(&r);
                if !f(r) {
                    break;
                }
            }
            Ok(())
        }
    };
}

/// 書き込みの権利 (02 §5)。`Store::take_write_token` だけが作る。複製できない。
#[derive(Debug)]
pub struct WriteToken {
    _private: (),
}

pub struct ReadTxn<'s> {
    pub(crate) store: &'s Store,
    pub(crate) tx: redb::ReadTransaction,
}

impl ReadTxn<'_> {
    record_reads!();
}

pub struct WriteTxn<'s> {
    pub(crate) store: &'s Store,
    pub(crate) tx: redb::WriteTransaction,
}

impl WriteTxn<'_> {
    record_reads!();

    /// 記録を書く (作る・置き換える)。`writer` はこの版にする。
    pub fn put(&mut self, env: &mut Envelope) -> Result<(), StoreError> {
        let spec = self
            .store
            .kinds
            .get(env.kind.as_str())
            .ok_or_else(|| StoreError::UnknownKind(env.kind.clone()))?;
        if env.v != spec.current_v {
            return Err(StoreError::WrongVersion {
                kind: env.kind.clone(),
                expected: spec.current_v,
                got: env.v,
            });
        }
        match self.get(&env.kind, env.id)? {
            Some(Read::Ok(old)) if old.v > spec.current_v => {
                return Err(StoreError::NewerRecord {
                    kind: env.kind.clone(),
                    id: env.id,
                    v: old.v,
                });
            }
            Some(Read::Ok(_)) => {}
            Some(Read::Newer { .. } | Read::Corrupt { .. }) => {
                return Err(StoreError::Unreadable {
                    kind: env.kind.clone(),
                    id: env.id,
                });
            }
            None => {
                let newer = self
                    .store
                    .newer_kinds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&env.kind);
                if newer && !spec.create_when_newer_present {
                    return Err(StoreError::KindHasNewerRecords(env.kind.clone()));
                }
            }
        }
        env.writer.clone_from(&self.store.version);
        let k = record_key(&env.kind, env.id);
        let bytes = env.encode(&k);
        let mut table = self.tx.open_table(RECORDS).map_err(db)?;
        table.insert(k.as_slice(), bytes.as_slice()).map_err(db)?;
        Ok(())
    }

    /// 記録を消す。新しい `v` の記録も消せる (本体を解釈しないため、03 §4.2 の規則 6)。
    /// 封筒をそのまま書く (検査も `writer` の書き換えもしない。ジャーナルからの取り込みだけで使う)。
    pub(crate) fn put_raw(&mut self, env: &Envelope) -> Result<(), StoreError> {
        let k = record_key(&env.kind, env.id);
        let bytes = env.encode(&k);
        let mut table = self.tx.open_table(RECORDS).map_err(db)?;
        table.insert(k.as_slice(), bytes.as_slice()).map_err(db)?;
        Ok(())
    }

    /// 索引と退避から除く ID を `meta` に足す ([`Store::pending_deletes`])。
    pub(crate) fn add_pending_deletes(&mut self, ids: &[Id]) -> Result<(), StoreError> {
        self.add_ids_meta(meta::PENDING_DELETES, ids)
    }

    /// 空白区切りの ID の並びの `meta` の値 `key` に、`ids` を足す (重ねない)。
    pub(crate) fn add_ids_meta(&mut self, key: &str, ids: &[Id]) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        let mut all: BTreeSet<Id> = m
            .get(key.as_bytes())
            .map_err(db)?
            .map(|v| parse_ids(&String::from_utf8_lossy(v.value())).ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        all.extend(ids.iter().copied());
        let value: Vec<String> = all.iter().map(Id::to_string).collect();
        m.insert(key.as_bytes(), value.join(" ").as_bytes())
            .map_err(db)?;
        Ok(())
    }

    /// 書き換えてよい `meta` の値を消す。
    pub(crate) fn remove_meta(&mut self, key: &str) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        m.remove(key.as_bytes()).map_err(db)?;
        Ok(())
    }

    /// [`Store::pending_deletes`] を消す (整理の job を書いたのと同じトランザクションで)。
    pub(crate) fn clear_pending_deletes(&mut self) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        m.remove(meta::PENDING_DELETES.as_bytes()).map_err(db)?;
        Ok(())
    }

    /// ジャーナルから取り込んだ最後の連番 (`meta` の `journal.last_seq`)。
    pub(crate) fn set_journal_seq(&mut self, seq: u64) -> Result<(), StoreError> {
        let mut m = self.tx.open_table(META).map_err(db)?;
        m.insert(
            meta::JOURNAL_LAST_SEQ.as_bytes(),
            seq.to_string().as_bytes(),
        )
        .map_err(db)?;
        Ok(())
    }

    pub fn delete(&mut self, kind: &str, id: Id) -> Result<bool, StoreError> {
        let k = record_key(kind, id);
        let mut table = self.tx.open_table(RECORDS).map_err(db)?;
        let existed = table.remove(k.as_slice()).map_err(db)?.is_some();
        Ok(existed)
    }

    /// 読んだときに壊れていた記録を、読み直して確かめてから quarantine へ移す (03 §8)。
    /// 移した件数を返す。
    pub fn quarantine_suspects(&mut self) -> Result<usize, StoreError> {
        let suspects = self.store.take_suspects();
        match self.quarantine_keys(&suspects) {
            Ok(n) => Ok(n),
            Err(e) => {
                // 移せなかった (トランザクションごと捨てる) ので、次の書き込みでもう一度試す。
                self.store.restore_suspects(suspects);
                Err(e)
            }
        }
    }

    pub(crate) fn quarantine_keys(
        &mut self,
        suspects: &BTreeSet<Vec<u8>>,
    ) -> Result<usize, StoreError> {
        let mut moved = 0;
        for k in suspects {
            let Some((kind, id)) = split_record_key(k) else {
                continue;
            };
            let raw = {
                let table = self.tx.open_table(RECORDS).map_err(db)?;
                match table.get(k.as_slice()).map_err(db)? {
                    Some(v) => v.value().to_vec(),
                    None => continue,
                }
            };
            // 読み直して、まだ壊れているときだけ移す。
            let Read::Corrupt { reason, .. } = classify(&kind, id, k, &raw) else {
                continue;
            };
            let now = self.store.clock.now();
            let mut q = Envelope::new(
                self.store.ids.next_id(),
                KIND_QUARANTINE,
                1,
                now,
                &self.store.version,
            );
            q.body = Value::Map(vec![
                ("kind".into(), kind.clone().into()),
                ("id".into(), id.to_string().into()),
                ("key".into(), Value::Bytes(k.clone())),
                ("raw".into(), Value::Bytes(raw)),
                ("reason".into(), reason.into()),
                ("at".into(), now.as_millis().into()),
            ]);
            self.put(&mut q)?;
            self.delete(&kind, id)?;
            moved += 1;
        }
        Ok(moved)
    }

    /// `meta` の値を一度だけ書く。同じ値ならそのまま、違う値なら `MetaConflict`。
    /// 書いてよいのは、一度書いたら変えないキー (`prompt.language`、07 §6.1。v0.3 から
    /// `memory.forget_key`、03 §3.1.1。v0.5 から `import.source`・`import.report`、03 §12.4) だけ。
    pub fn set_meta_once(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        if ![
            meta::PROMPT_LANGUAGE,
            meta::FORGET_KEY,
            meta::IMPORT_SOURCE,
            meta::IMPORT_REPORT,
        ]
        .contains(&key)
        {
            return Err(StoreError::MetaNotWritable(key.to_owned()));
        }
        // 鍵は形を確かめ、食い違いがある間は書かせない (03 §3.1.1)。
        if key == meta::FORGET_KEY && (!is_forget_key(value) || self.store.forget_key_conflict()) {
            return Err(StoreError::MetaNotWritable(key.to_owned()));
        }
        let mut m = self.tx.open_table(META).map_err(db)?;
        let existing = m
            .get(key.as_bytes())
            .map_err(db)?
            .map(|v| String::from_utf8_lossy(v.value()).into_owned());
        match existing {
            Some(e) if e == value => Ok(()),
            Some(e) => Err(StoreError::MetaConflict {
                key: key.to_owned(),
                existing: e,
            }),
            None => {
                m.insert(key.as_bytes(), value.as_bytes()).map_err(db)?;
                Ok(())
            }
        }
    }

    /// 上書きしてよい、ジャーナルにも書く `meta` の値 (`import.old_root`・`import.v04_ended`、03 §3.1.1) を書く。
    /// 古い根を処分した印 (`import.old_root` の `disposed:…`) は `kept` に戻さない (`MetaConflict`)。
    ///
    /// # Errors
    /// ほかのキーのとき (`MetaNotWritable`)、処分した印を戻そうとしたとき (`MetaConflict`)、DB を書けないとき。
    pub fn set_meta_latest(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        if key != meta::IMPORT_OLD_ROOT && key != meta::IMPORT_V04_ENDED {
            return Err(StoreError::MetaNotWritable(key.to_owned()));
        }
        let mut m = self.tx.open_table(META).map_err(db)?;
        let existing = m
            .get(key.as_bytes())
            .map_err(db)?
            .map(|v| String::from_utf8_lossy(v.value()).into_owned());
        if key == meta::IMPORT_OLD_ROOT
            && let Some(e) = existing
            && e.starts_with(meta::IMPORT_OLD_ROOT_DISPOSED)
            && !value.starts_with(meta::IMPORT_OLD_ROOT_DISPOSED)
        {
            return Err(StoreError::MetaConflict {
                key: key.to_owned(),
                existing: e,
            });
        }
        m.insert(key.as_bytes(), value.as_bytes()).map_err(db)?;
        Ok(())
    }

    pub fn commit(self) -> Result<(), StoreError> {
        self.tx.commit().map_err(db)
    }

    pub fn now(&self) -> Timestamp {
        self.store.clock.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use evorag_types::{ManualClock, UuidV7Gen};

    #[derive(serde::Serialize)]
    struct Body {
        text: String,
    }

    fn locked_opts(root: &Path) -> OpenOptions {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.5".into(),
            kinds: vec![KindSpec::new("sample", 1)],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 2)),
            config_file: None,
        }
    }

    /// 先に取ったデータ根のロックを、放さずにそのまま持って開く (起動の更新の片付けの後、12 §7.3)。
    #[test]
    fn open_locked_keeps_the_lock_it_is_given() {
        let dir = TempDir::new("store-open-locked");
        let held = DataRootLock::acquire(dir.path()).unwrap();
        let (store, _) = Store::open_locked(locked_opts(dir.path()), held).unwrap();
        assert!(matches!(
            DataRootLock::acquire(dir.path()),
            Err(LockError::InUse { .. })
        ));
        drop(store);
        assert!(DataRootLock::acquire(dir.path()).is_ok());
    }

    /// 別のデータ根のロックを渡すのは呼ぶ側の誤りなので、debug のビルドでは panic する。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "is for another data root")]
    fn open_locked_with_the_lock_of_another_root_panics_in_debug_builds() {
        let dir = TempDir::new("store-open-locked-debug");
        let other = TempDir::new("store-open-locked-debug-other");
        let foreign = DataRootLock::acquire(other.path()).unwrap();
        let _ = Store::open_locked(locked_opts(dir.path()), foreign);
    }

    /// release のビルドでは、別のデータ根のロックを放して取り直す。
    #[cfg(not(debug_assertions))]
    #[test]
    fn open_locked_retakes_the_lock_of_another_root_in_release_builds() {
        let dir = TempDir::new("store-open-locked-release");
        let other = TempDir::new("store-open-locked-release-other");
        let foreign = DataRootLock::acquire(other.path()).unwrap();
        let (store, _) = Store::open_locked(locked_opts(dir.path()), foreign).unwrap();
        assert!(
            DataRootLock::acquire(other.path()).is_ok(),
            "the foreign lock was released"
        );
        assert!(matches!(
            DataRootLock::acquire(dir.path()),
            Err(LockError::InUse { .. })
        ));
        drop(store);
    }

    /// 退避の整理は、作りかけの退避を作り終えるまで待ってから一覧を作る。削除より前に写し始め、整理の job を
    /// 書いた後に作り終えた退避も、消した記録を含んだまま残らない。
    /// verifies: 03 §7.3、04 §7.7 の 5 (退避と整理を直列にする)
    #[test]
    fn a_backup_finished_while_a_purge_waits_is_purged_too() {
        let dir = TempDir::new("store-backup-order");
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        let (store, _) = Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.0.5".into(),
            kinds: vec![KindSpec::new("sample", 1)],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 2)),
            config_file: None,
        })
        .unwrap();
        let tok = store.take_write_token().unwrap();
        let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
        e.set_body(&Body {
            text: "架空の消す本文".into(),
        })
        .unwrap();
        let gone = e.id;
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut e).unwrap();
        tx.commit().unwrap();

        let base = dir.path().join(crate::backup::BACKUPS_DIR);
        // 退避を作り始めた (写しは削除の前の時点)。
        let making = store.backup_order();
        std::thread::scope(|s| {
            let purge = s.spawn(|| store.purge_backups(None, &[gone], &|| false));
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(!purge.is_finished(), "the purge waits for the backup");
            // 整理の一覧を作った後に、退避を作り終える。
            crate::backup::daily(&store.db, &base, clock.now().as_millis())
                .unwrap()
                .unwrap();
            drop(making);
            assert!(
                purge.join().unwrap().unwrap() >= 1,
                "the new backup is rewritten"
            );
        });
        let backups = crate::restore::list_backups(dir.path(), None);
        assert!(!backups.is_empty());
        for b in backups {
            let db = Database::open(b.dir.join(DB_FILE)).unwrap();
            let rtx = db.begin_read().unwrap();
            let t = rtx.open_table(RECORDS).unwrap();
            assert!(
                t.get(record_key("sample", gone).as_slice())
                    .unwrap()
                    .is_none(),
                "{}",
                b.dir.display()
            );
        }
    }

    const NOTED: KindSpec = KindSpec {
        journaled: true,
        ..KindSpec::new("noted", 1)
    };

    fn open_noted(root: &Path) -> (Arc<Store>, OpenReport) {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        let (store, report) = Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.5".into(),
            kinds: vec![KindSpec::new("sample", 1), NOTED],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 2)),
            config_file: None,
        })
        .unwrap();
        (Arc::new(store), report)
    }

    fn noted(store: &Store, id: Id) -> Envelope {
        let mut e = Envelope::new(id, "noted", 1, store.clock().now(), "");
        e.set_body(&Body {
            text: "架空の消した本文".into(),
        })
        .unwrap();
        e
    }

    /// 削除の行の後に古い版が書いた `put` の行は当てず、削除の行があるのに DB に残っている記録は開くときに消す
    /// (消したとみなす ID の記録を生きたまま残さない)。消した ID は開いた結果に入る。
    /// verifies: 03 §6、04 §7.7 の 3
    #[test]
    fn records_deleted_in_the_journal_do_not_come_back_on_open() {
        let dir = TempDir::new("store-journal-deleted");
        let (store, _) = open_noted(dir.path());
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        let id = store.ids().next_id();
        let env = noted(&store, id);
        h.submit_wait(crate::WriteRequest::conversation(vec![
            crate::WriteOp::Put(env.clone()),
        ]))
        .unwrap();
        h.submit_wait(crate::WriteRequest::conversation(vec![
            crate::WriteOp::Delete {
                kind: "noted".into(),
                id,
            },
        ]))
        .unwrap();
        h.shutdown();
        t.join().unwrap();
        drop(store);

        // 古い版 (消したことを知らない書き手) が、DB とジャーナルに書き戻した。
        let (store, _) = open_noted(dir.path());
        let tok = store.take_write_token().unwrap();
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut env.clone()).unwrap();
        tx.commit().unwrap();
        store
            .journal
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .append(store.clock().now(), &[LineOp::Put(env)])
            .unwrap();
        drop(store);

        let (store, report) = open_noted(dir.path());
        assert_eq!(store.read().unwrap().get("noted", id).unwrap(), None);
        assert_eq!(report.journal_deletes, vec![id]);
        assert!(store.was_deleted(id));
        // 消した ID は、新しい記録に使い直さない (ID の下限に入る)。
        assert!(store.ids().next_id() > id);
        // 索引と退避から除く ID は、整理の job にするまで残る (開き直しても失わない)。
        assert_eq!(store.pending_deletes().unwrap().ids, vec![id]);
        drop(store);
        let (store, report) = open_noted(dir.path());
        assert!(report.journal_deletes.is_empty());
        assert_eq!(store.pending_deletes().unwrap().ids, vec![id]);
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        h.submit_wait(crate::WriteRequest::background(vec![
            crate::WriteOp::ClearPendingDeletes,
        ]))
        .unwrap();
        h.shutdown();
        t.join().unwrap();
        assert!(store.pending_deletes().unwrap().ids.is_empty());
    }

    /// 削除の行があるのに DB に残っている記録は、ほかの記録から関係でつながっていても開くときに消す。つないで
    /// いた記録 (消したセッションに古い版が書き足したターンなど) は消さずに残り、ほかの書き込みは続けられる。
    /// verifies: 03 §6、04 §7.7 の 3
    #[test]
    fn a_record_with_a_delete_line_is_deleted_even_when_linked() {
        let dir = TempDir::new("store-journal-deleted-linked");
        let (store, _) = open_noted(dir.path());
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        let id = store.ids().next_id();
        let env = noted(&store, id);
        h.submit_wait(crate::WriteRequest::conversation(vec![
            crate::WriteOp::Put(env.clone()),
        ]))
        .unwrap();
        h.submit_wait(crate::WriteRequest::conversation(vec![
            crate::WriteOp::Delete {
                kind: "noted".into(),
                id,
            },
        ]))
        .unwrap();
        h.shutdown();
        t.join().unwrap();
        drop(store);
        // 古い版が書き戻し、ほかの記録からつないだ。
        let (store, _) = open_noted(dir.path());
        let tok = store.take_write_token().unwrap();
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut env.clone()).unwrap();
        let mut child = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
        child
            .set_body(&Body {
                text: "架空の派生".into(),
            })
            .unwrap();
        tx.put(&mut child).unwrap();
        tx.put_link(child.id, "derived_from", id, b"").unwrap();
        tx.commit().unwrap();
        drop(store);

        let (store, report) = open_noted(dir.path());
        let r = store.read().unwrap();
        assert_eq!(r.get("noted", id).unwrap(), None);
        assert!(matches!(
            r.get("sample", child.id).unwrap(),
            Some(Read::Ok(_))
        ));
        drop(r);
        assert_eq!(report.journal_deletes, vec![id]);
        assert_eq!(store.pending_deletes().unwrap().ids, vec![id]);
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        let other = noted(&store, store.ids().next_id());
        h.submit_wait(crate::WriteRequest::conversation(vec![
            crate::WriteOp::Put(other.clone()),
        ]))
        .unwrap();
        h.shutdown();
        t.join().unwrap();
        assert!(matches!(
            store.read().unwrap().get("noted", other.id).unwrap(),
            Some(Read::Ok(_))
        ));
    }

    /// quarantine の記録を書けない状態 (新しい版の quarantine の記録がある) でも、発話の追記は止まらない。
    /// verifies: 03 §8、03 §4.2 の規則 6
    #[test]
    fn writes_go_on_when_quarantine_cannot_be_written() {
        let dir = TempDir::new("store-quarantine-blocked");
        let (store, _) = open_noted(dir.path());
        // 新しい版の quarantine の記録を置き、読んで覚えさせる。
        let newer = store.ids().next_id();
        let mut q = Envelope::new(newer, KIND_QUARANTINE, 2, store.clock().now(), "9.9.9");
        q.set_body(&Body {
            text: "新しい版".into(),
        })
        .unwrap();
        let broken = store.ids().next_id();
        {
            let tx = store.db.begin_write().unwrap();
            let mut t = tx.open_table(RECORDS).unwrap();
            let k = record_key(KIND_QUARANTINE, newer);
            t.insert(k.as_slice(), q.encode(&k).as_slice()).unwrap();
            t.insert(
                record_key("sample", broken).as_slice(),
                [1u8; 24].as_slice(),
            )
            .unwrap();
            drop(t);
            tx.commit().unwrap();
        }
        let r = store.read().unwrap();
        assert!(matches!(
            r.get(KIND_QUARANTINE, newer).unwrap(),
            Some(Read::Ok(_))
        ));
        assert!(matches!(
            r.get("sample", broken).unwrap(),
            Some(Read::Corrupt { .. })
        ));
        drop(r);
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        let turn = noted(&store, store.ids().next_id());
        let out = h
            .submit_wait(crate::WriteRequest::conversation(vec![
                crate::WriteOp::Put(turn.clone()),
            ]))
            .unwrap();
        assert!(out.quarantine_failed.is_some());
        assert_eq!(out.quarantined, 0);
        // 書けない状態は続くので、試し直さない (次の書き込みも同じく通る)。
        assert_eq!(store.suspect_count(), 0);
        h.shutdown();
        t.join().unwrap();
        assert!(matches!(
            store.read().unwrap().get("noted", turn.id).unwrap(),
            Some(Read::Ok(_))
        ));
    }

    /// 断られた依頼の前に見つけた壊れた記録も、quarantine へ移す (依頼とは別のトランザクション)。差し替えを
    /// 当てられない形の本体は、`Stale` ではなく `BadBody` で断る。
    /// verifies: 03 §8、04 §7.7 の 3
    #[test]
    fn a_refused_request_still_quarantines_and_a_bad_body_is_reported() {
        let dir = TempDir::new("store-refused-quarantine");
        let (store, _) = open_noted(dir.path());
        let (writer, h) = crate::writer::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        // 本体が map でない記録。
        let odd = store.ids().next_id();
        let mut e = Envelope::new(odd, "sample", 1, store.clock().now(), "");
        e.body = Value::Text("map ではない".into());
        h.submit_wait(crate::WriteRequest::background(vec![crate::WriteOp::Put(
            e,
        )]))
        .unwrap();
        let r = h.submit_wait(crate::WriteRequest::background(vec![
            crate::WriteOp::Patch {
                kind: "sample".into(),
                id: odd,
                fields: crate::BodyPatch::new(),
                updated_at: None,
            },
        ]));
        assert!(
            matches!(
                r,
                Err(crate::WriteError::Store(StoreError::BadBody { id, .. })) if id == odd
            ),
            "{r:?}"
        );
        // 壊れた記録を読んで、覚えさせる。
        let broken = store.ids().next_id();
        let key = record_key("sample", broken);
        {
            let tx = store.db.begin_write().unwrap();
            tx.open_table(RECORDS)
                .unwrap()
                .insert(key.as_slice(), [1u8; 24].as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        assert!(matches!(
            store.read().unwrap().get("sample", broken).unwrap(),
            Some(Read::Corrupt { .. })
        ));
        assert_eq!(store.suspect_count(), 1);
        let r = h.submit_wait(crate::WriteRequest::background(vec![
            crate::WriteOp::Expect {
                kind: "sample".into(),
                id: store.ids().next_id(),
                field: None,
            },
        ]));
        assert!(
            matches!(r, Err(crate::WriteError::Store(StoreError::Stale { .. }))),
            "{r:?}"
        );
        h.shutdown();
        t.join().unwrap();
        assert_eq!(store.suspect_count(), 0);
        assert_eq!(store.read().unwrap().get("sample", broken).unwrap(), None);
        let census = store.census().unwrap();
        assert_eq!(census.known.get(KIND_QUARANTINE).map_or(0, |c| c.ok), 1);
    }
}
