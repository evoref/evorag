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
use crate::key::{self, KeyBuf, OwnedPart};
use crate::lock::{DataRootLock, LockError};
use crate::meta;
use crate::version::Version;

/// DB のファイルの名前。
pub const DB_FILE: &str = "evorag.redb";

/// この evorag が書く DB の形式の版 (03 §4.1。v0.1〜v0.4 は `0.<n>`)。
pub const FORMAT_VERSION: &str = "0.1";

type Raw = TableDefinition<'static, &'static [u8], &'static [u8]>;
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

/// 種類の定め (03 §3.3)。種類を持つ crate が、開くときに渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindSpec {
    pub name: &'static str,
    /// この版が書く、その種類の形式の版。
    pub current_v: u32,
    /// 新しい `v` の記録があるときも、新しい記録を今の `v` で作ってよいか (03 §4.2 の規則 6。既定は作らない)。
    pub create_when_newer_present: bool,
}

impl KindSpec {
    pub const fn new(name: &'static str, current_v: u32) -> Self {
        Self {
            name,
            current_v,
            create_when_newer_present: false,
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
}

/// 書き込みを断る理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// DB が、この版が書けない新しい形式の版 (`format.min_write`) で書かれている。
    NewerFormat { min_write: String },
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
    /// 古い形式の版。移行が要る (03 §12)。
    #[error("the database format {found} is older than {current} and must be migrated")]
    MigrationRequired { found: String, current: String },
    /// `meta` が読めない・形式の版が無い。
    #[error("the database metadata is unreadable: {0}")]
    BadMeta(String),
    /// DB のファイルが開けない (03 §8 の「DB のファイルの破損」)。
    #[error("the database file cannot be opened: {0}")]
    Unreadable(String),
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

#[cfg(unix)]
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
}

#[cfg(not(unix))]
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    // Windows の所有者だけの ACL (NFR-503) は、setup のタスクで入れる。既定の置き場所
    // (%LOCALAPPDATA%) は利用者ごとのフォルダ。
    std::fs::create_dir_all(p)
}

#[cfg(unix)]
fn make_private_file(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private_file(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

impl Store {
    /// データ根を開く。無ければ作る。
    pub fn open(opts: OpenOptions) -> Result<(Self, OpenReport), OpenError> {
        create_private_dir(&opts.data_root)?;
        let lock = DataRootLock::acquire(&opts.data_root)?;
        let path = opts.data_root.join(DB_FILE);
        let database = Database::create(&path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => OpenError::DatabaseInUse,
            redb::DatabaseError::Storage(redb::StorageError::Io(io)) => OpenError::Io(io),
            other => OpenError::Unreadable(other.to_string()),
        })?;
        make_private_file(&path)?;

        let ours: Version = FORMAT_VERSION.parse().expect("valid format version");
        let our_writer: Version = opts
            .evorag_version
            .parse()
            .map_err(|e| OpenError::BadMeta(format!("evorag version: {e}")))?;

        let mut kinds: BTreeMap<&'static str, KindSpec> =
            BUILTIN_KINDS.iter().map(|k| (k.name, k.clone())).collect();
        kinds.extend(opts.kinds.into_iter().map(|k| (k.name, k)));

        let mut store = Self {
            db: database,
            root: opts.data_root,
            version: opts.evorag_version,
            kinds,
            read_only: None,
            clock: opts.clock,
            ids: opts.ids,
            newer_kinds: Mutex::default(),
            suspects: Mutex::default(),
            token_issued: AtomicBool::new(false),
            _lock: lock,
        };

        let existing = store
            .read_meta()
            .map_err(|e| OpenError::BadMeta(e.to_string()))?;
        let mut report = OpenReport::default();
        let Some(existing) = existing else {
            store
                .init()
                .map_err(|e| OpenError::Unreadable(e.to_string()))?;
            report.created = true;
            return Ok((store, report));
        };

        let get = |k: &str| -> Result<Version, OpenError> {
            existing
                .get(k)
                .ok_or_else(|| OpenError::BadMeta(format!("missing `{k}`")))?
                .parse()
                .map_err(|e| OpenError::BadMeta(format!("`{k}`: {e}")))
        };
        let format = get(meta::FORMAT_VERSION)?;
        let min_read = get(meta::FORMAT_MIN_READ)?;
        let min_write = get(meta::FORMAT_MIN_WRITE)?;
        if min_read > ours {
            return Err(OpenError::TooNew {
                format: format.to_string(),
                min_read: min_read.to_string(),
            });
        }
        if format < ours {
            return Err(OpenError::MigrationRequired {
                found: format.to_string(),
                current: ours.to_string(),
            });
        }
        if min_write > ours {
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
        }
        Ok((store, report))
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
        let rtx = self.db.begin_read().map_err(db)?;
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
                (meta::FORMAT_VERSION, FORMAT_VERSION),
                (meta::FORMAT_MIN_READ, FORMAT_VERSION),
                (meta::FORMAT_MIN_WRITE, FORMAT_VERSION),
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
        let table = rtx.open_table(RECORDS).map_err(db)?;
        let mut c = Census::default();
        for item in table.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let Some((kind, id)) = split_record_key(k.value()) else {
                c.bad_keys += 1;
                continue;
            };
            let Some(spec) = self.kinds.get(kind.as_str()) else {
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

    pub fn commit(self) -> Result<(), StoreError> {
        self.tx.commit().map_err(db)
    }

    pub fn now(&self) -> Timestamp {
        self.store.clock.now()
    }
}
