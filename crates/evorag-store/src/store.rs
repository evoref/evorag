//! データ根を開き、`evorag.redb` の記録を読み書きする (03 §1、§3、§4.2、§8、§9)。
//!
//! この段階では、書き込みはトランザクションの単位で直接行う。書き手スレッドと `WriteToken`
//! (02 §5) は、この上に載せる。

use std::collections::{BTreeMap, BTreeSet};
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
const RESERVE_BYTES: u64 = 16 * 1024 * 1024;

/// この evorag が書く DB の形式の版 (03 §4.1。v0.1〜v0.4 は `0.<n>`)。
pub const FORMAT_VERSION: &str = "0.1";

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
    /// ジャーナルから取り込むときに、関係と副索引を作る関数。
    pub derive: Option<DeriveFn>,
}

impl KindSpec {
    pub const fn new(name: &'static str, current_v: u32) -> Self {
        Self {
            name,
            current_v,
            create_when_newer_present: false,
            journaled: false,
            derive: None,
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
    db: Database,
    root: PathBuf,
    version: String,
    plan: FormatPlan,
    kinds: BTreeMap<&'static str, KindSpec>,
    read_only: Option<ReadOnlyReason>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    /// 新しい `v` の記録が見つかった種類。
    newer_kinds: Mutex<BTreeSet<String>>,
    /// 読んだときに壊れていた記録のキー (書き込みのときに quarantine で扱う)。
    suspects: Mutex<BTreeSet<Vec<u8>>>,
    /// 書き込みの権利 (`WriteToken`) を渡したか。1 つのストアにつき 1 つだけ (02 §5)。
    token_issued: AtomicBool,
    /// ジャーナル (書き込みで開いたときだけ)。書き手だけが使う。
    pub(crate) journal: Mutex<Option<Journal>>,
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

fn split_record_key(k: &[u8]) -> Option<(String, Id)> {
    match key::decode(k).ok()?.as_slice() {
        [OwnedPart::Str(kind), OwnedPart::Id(id)] => Some((kind.clone(), *id)),
        _ => None,
    }
}

fn classify(kind: &str, id: Id, k: &[u8], v: &[u8]) -> Read {
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

fn map_db_error(e: redb::DatabaseError) -> OpenError {
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
            // 作った直後 (中身を書く前) に止まった空のファイル。
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

    /// 書く形式と移行を指定して開く (移行の試験のため)。
    pub fn open_with(opts: OpenOptions, plan: FormatPlan) -> Result<(Self, OpenReport), OpenError> {
        fsutil::create_private_dir(&opts.data_root)?;
        let lock = DataRootLock::acquire(&opts.data_root)?;
        migrate::recover(&opts.data_root)?;
        let mut database = match open_db(&opts.data_root) {
            // 開けない DB のファイルは broken/ へ移し (消さない)、戻すかを尋ねる (03 §8、FR-205)。
            Err(OpenError::Unreadable(reason)) if opts.data_root.join(DB_FILE).is_file() => {
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
        }
        Ok((store, report))
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
        let (journal, scan) = Journal::open(&self.root, last)?;
        report.journal_bad_lines = scan.bad;
        report.journal_newer_lines = scan.newer;
        report.journal_truncated = scan.truncated;
        if let Some(last_line) = scan.lines.last() {
            let last_seq = last_line.seq;
            let unreadable = |e: StoreError| OpenError::Unreadable(e.to_string());
            let mut tx = self
                .write_internal(Durability::Immediate)
                .map_err(unreadable)?;
            for line in &scan.lines {
                self.replay_line(&mut tx, &line.op).map_err(unreadable)?;
            }
            tx.set_journal_seq(last_seq).map_err(unreadable)?;
            tx.commit().map_err(unreadable)?;
            report.journal_replayed = scan.lines.len();
        }
        *self.journal.lock().unwrap_or_else(|e| e.into_inner()) = Some(journal);
        Ok(())
    }

    /// ジャーナルの 1 行を、何度当てても同じになるように当てる。
    fn replay_line(&self, tx: &mut WriteTxn<'_>, op: &LineOp) -> Result<(), StoreError> {
        match op {
            LineOp::Put(env) => {
                // 通常の書き込みと同じ規則で当てる (03 §6): 新しい `v` の記録と読めない記録は上書き
                // しない。順序は連番で決まるので、時刻は比べない。
                let spec = self.kinds.get(env.kind.as_str());
                match tx.get(&env.kind, env.id)? {
                    Some(Read::Ok(old)) => {
                        if spec.is_some_and(|s| old.v > s.current_v) {
                            return Ok(());
                        }
                        self.undo_derived(tx, &old)?;
                    }
                    Some(Read::Newer { .. } | Read::Corrupt { .. }) => return Ok(()),
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
                tx.delete(kind, *id)?;
                tx.delete_links_of(*id)?;
                tx.delete_blobs(*id)?;
            }
            LineOp::Meta { key, value } => match tx.set_meta_once(key, value) {
                Ok(()) | Err(StoreError::MetaConflict { .. } | StoreError::MetaNotWritable(_)) => {}
                Err(e) => return Err(e),
            },
        }
        Ok(())
    }

    /// 記録から作った関係と副索引を消す (上書き・削除の前に)。
    fn undo_derived(&self, tx: &mut WriteTxn<'_>, old: &Envelope) -> Result<(), StoreError> {
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
    fn write_internal(&self, durability: Durability) -> Result<WriteTxn<'_>, StoreError> {
        let mut tx = self.db.begin_write().map_err(db)?;
        tx.set_durability(durability.into()).map_err(db)?;
        Ok(WriteTxn { store: self, tx })
    }

    /// 1 日 1 回までの自動の退避 (03 §7.1)。`backups` を省くとデータ根の `backups/`。
    /// 読み取りのトランザクションで写すので、書き手と並んで動かしてよい。
    pub fn daily_backup(&self, backups: Option<&Path>) -> Result<Option<PathBuf>, StoreError> {
        let default = self.root.join(crate::backup::BACKUPS_DIR);
        crate::backup::daily(
            &self.db,
            backups.unwrap_or(&default),
            self.clock.now().as_millis(),
        )
    }

    /// その種類をジャーナルにも書くか。
    pub(crate) fn is_journaled(&self, kind: &str) -> bool {
        self.kinds.get(kind).is_some_and(|k| k.journaled)
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
        pub fn for_each(
            &self,
            kind: &str,
            mut f: impl FnMut(Read) -> bool,
        ) -> Result<(), StoreError> {
            let table = self.tx.open_table(RECORDS).map_err(db)?;
            let start = kind_prefix(kind);
            let end = key::prefix_end(&start);
            let range = match &end {
                Some(e) => table.range::<&[u8]>(start.as_slice()..e.as_slice()),
                None => table.range::<&[u8]>(start.as_slice()..),
            }
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
        let suspects = std::mem::take(
            &mut *self
                .store
                .suspects
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let mut moved = 0;
        for k in suspects {
            let Some((kind, id)) = split_record_key(&k) else {
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
            let Read::Corrupt { reason, .. } = classify(&kind, id, &k, &raw) else {
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
    /// 書いてよいのは、一度書いたら変えないキー (`prompt.language`、07 §6.1) だけ。
    pub fn set_meta_once(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        if key != meta::PROMPT_LANGUAGE {
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

    pub fn commit(self) -> Result<(), StoreError> {
        self.tx.commit().map_err(db)
    }

    pub fn now(&self) -> Timestamp {
        self.store.clock.now()
    }
}
