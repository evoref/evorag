//! v0.4 のデータ根から新しいデータ根への取り込み (`import-v04`、03 §12.4) の保存の層。
//!
//! - [`OldRoot`]: 古い根を、取り込める根か確かめてから読むだけで開く (03 §12.4 の「取り込める根」・「読み方」)。DB は
//!   redb の [`ReadOnlyDatabase`] (読むだけのファイルの取っ手で開き、修復の要る DB は開かずに断る) で読み、ジャーナル・
//!   索引・退避には触れない。書くのは、ロックのファイル (`lock`・`lock.pid`) だけ (03 §9。照合の (4) から除く)。
//! - [`FileSnapshot`]: 古い根のすべてのファイルの大きさ・更新時刻・SHA-256 (照合の (4) と `import.source` の要約)。
//! - [`KindDigest`]: 種類ごとの件数と (ID ‖ 本体の CBOR) の SHA-256 (照合の (1))。
//! - [`rebuild_check`]: 新しい根のジャーナルだけから DB を作り直して、DB と比べる (照合の (3))。
//!
//! 何を写すか・どう変えるかはメモリの層 (`evorag-memory` の `import_v04`)、手順の全体はコア (`evorag-core`) が持つ
//! (02 §4.4)。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use evorag_types::Id;
use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTable};
use sha2::{Digest, Sha256};

use crate::envelope::Envelope;
use crate::export::ExportView;
use crate::fsutil;
use crate::journal::{self, JOURNAL_DIR};
use crate::key::{self, OwnedPart};
use crate::lock::{DataRootLock, LOCK_FILE, LockError, PID_FILE};
use crate::meta;
use crate::migrate::FormatPlan;
use crate::store::{
    DB_FILE, LINKS, LOOKUP, META, OpenError, OpenOptions, RECORDS, Store, StoreError,
    read_meta_db_any, split_record_key,
};
use crate::version::Version;

/// 新しい根のフォルダの名前の終わり (`<古い根の名前>.v05-import`、03 §12.4 の「書き方」)。
pub const IMPORT_DIR_SUFFIX: &str = ".v05-import";

/// v0.4 の根の形式の版 (03 §3.1.1。v0.4 は v0.3 の形式のまま書く)。
const V04_FORMAT: &str = "0.3";
/// 取り込める根を書き込みで開いた版の下限 (含む) と上限 (含まない) (03 §12.4 の「取り込める根」の (1))。
const V04_WRITER_MIN: &str = "0.4.0";
const V04_WRITER_END: &str = "0.5.0";

/// ジャーナルにも `meta` の行として書く、新しい根の `meta` のキー (照合の (3) で、ジャーナルから作り直した DB と比べる)。
pub const JOURNALED_META: [&str; 5] = [
    meta::PROMPT_LANGUAGE,
    meta::FORGET_KEY,
    meta::IMPORT_SOURCE,
    meta::IMPORT_OLD_ROOT,
    meta::IMPORT_V04_ENDED,
];

/// 古い根 `old_root` から作る新しい根のパス (同じ親のフォルダの `<名前>.v05-import`)。名前を持たないパス (ドライブの
/// 根など) なら `None`。
#[must_use]
pub fn import_dir(old_root: &Path) -> Option<PathBuf> {
    let (parent, name) = split_root(old_root)?;
    Some(parent.join(format!("{name}{IMPORT_DIR_SUFFIX}")))
}

/// 入れ替えの後の古い根のパス (同じ親のフォルダの `<名前>-v0.4-<UTC の日時>`、03 §12.4 の「入れ替え」)。`now_ms` は
/// 取り込みを始めた時刻 (UTC、ミリ秒)。名前を持たないパスなら `None`。
#[must_use]
pub fn renamed_old_root(old_root: &Path, now_ms: i64) -> Option<PathBuf> {
    let (parent, name) = split_root(old_root)?;
    let stamp = crate::backup::stamp(now_ms);
    Some(parent.join(format!("{name}-v0.4-{stamp}")))
}

fn split_root(root: &Path) -> Option<(PathBuf, String)> {
    let abs = std::path::absolute(root).ok()?;
    let name = abs.file_name()?.to_str()?.to_owned();
    let parent = abs.parent()?.to_path_buf();
    Some((parent, name))
}

/// 取り込めない根の理由 (03 §12.4 の「取り込める根」)。どれも、古い根にも新しい根にも何も書かずに断る。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Refusal {
    /// `evorag.redb` が無い (データ根でない)。
    #[error("{} is not an evorag data root", path.display())]
    NotDataRoot { path: PathBuf },
    /// (1) v0.4 で書き込みで開いていない根 (形式の版 `0.2` 以前、または v0.3 以前の版だけで使った根)。
    #[error(
        "the data root was not opened by evorag 0.4 (format {format}, last opened by {max_writer:?})"
    )]
    NotOpenedByV04 {
        format: String,
        max_writer: Option<String>,
    },
    /// v0.4 より新しい形式の根 (もう取り込んだ根など)。
    #[error("the data root is already in format {format}")]
    NewerFormat { format: String },
    /// (2) 忘れるの当て直しが残っている (`forget.recheck` が立っている、または DB の本文が当て直しの前)。
    #[error("a forget has not been applied to the data root yet")]
    ForgetPending,
    /// (3) ジャーナルに、まだ DB に取り込んでいない行・書きかけの末尾・書き直しの途中の跡がある。
    #[error("the journal is not settled ({})", journal_why(*lines, *partial_tail, *rewrite_leftover))]
    JournalPending {
        /// まだ DB に取り込んでいない行の数。
        lines: usize,
        /// 書きかけの末尾 (改行で終わらない最後の行・そろわない末尾の組) がある。
        partial_tail: bool,
        /// 書き直しの一時のファイル (`*.jsonl.tmp`) が残っている。
        rewrite_leftover: bool,
    },
    /// (4) `memory.forget_key` が使えない (03 §3.1.1 のフェイルクローズ)。
    #[error("the forget key of the data root cannot be used")]
    ForgetKeyUnusable,
    /// 前の v0.4 が途中で止まった跡がある (修復の要る DB・移行や compact の跡・ジャーナルにだけある鍵)。v0.4 で開けば
    /// 片付く。
    #[error("the data root was not closed cleanly ({what})")]
    Unfinished { what: String },
    /// DB・`meta`・記録を読めない。
    #[error("the data root cannot be read ({what})")]
    Unreadable { what: String },
    /// 古い根のパスがシンボリックリンク・ジャンクション (新しい根がリンクの親の側に作られ、置き場所が黙って移るため)。
    #[error("{} is a link", path.display())]
    Link { path: PathBuf },
    /// 古い根がマウントポイント (改名できないため。Unix で、親のフォルダとファイルシステムが違う)。
    #[error("{} is a mount point", path.display())]
    MountPoint { path: PathBuf },
    /// 古い根のパスが Unicode として読めない (`import.source` に改名の後のパスをそのまま書けないため)。
    #[error("the path {} is not valid Unicode", path.display())]
    NonUnicodePath { path: PathBuf },
}

fn journal_why(lines: usize, partial_tail: bool, rewrite_leftover: bool) -> String {
    let mut why = Vec::new();
    if lines > 0 {
        why.push(format!("{lines} line(s) not yet in the database"));
    }
    if partial_tail {
        why.push("an unfinished last line".to_owned());
    }
    if rewrite_leftover {
        why.push("a rewrite left a temporary file".to_owned());
    }
    why.join(", ")
}

/// 断った後に利用者がすること (画面の文言は CLI と TUI が持つ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NextStep {
    /// 取り込み元のパスを確かめる (`--from`)。
    CheckPath,
    /// v0.4 で一度起動して閉じてから、取り込み直す (当て直し・取り込み・移行・修復を v0.4 に済ませる)。
    OpenOnceWithV04,
    /// v0.4 の `evorag doctor` (と `evorag repair`) で確かめてから、取り込み直す。
    RepairWithV04,
    /// 取り込みは要らない (もう新しい形式の根)。
    NothingToImport,
    /// リンクの先の実際のフォルダを、`--from` (と `--data-root`・`storage.data_root`) に指す。
    PointAtRealFolder,
    /// データ根を、マウントポイントの下のふつうのフォルダへ移してから取り込む。
    MoveIntoFolder,
}

impl Refusal {
    /// 断った後にすること。
    #[must_use]
    pub fn next_step(&self) -> NextStep {
        match self {
            Refusal::NotDataRoot { .. } => NextStep::CheckPath,
            Refusal::NotOpenedByV04 { .. }
            | Refusal::ForgetPending
            | Refusal::JournalPending { .. }
            | Refusal::Unfinished { .. } => NextStep::OpenOnceWithV04,
            Refusal::ForgetKeyUnusable | Refusal::Unreadable { .. } => NextStep::RepairWithV04,
            Refusal::NewerFormat { .. } => NextStep::NothingToImport,
            Refusal::Link { .. } => NextStep::PointAtRealFolder,
            Refusal::MountPoint { .. } => NextStep::MoveIntoFolder,
            Refusal::NonUnicodePath { .. } => NextStep::CheckPath,
        }
    }
}

/// 古い根の置き場所を、触れる前に確かめる (03 §12.4 の「取り込める根」の (8)): リンクでない、マウントポイントでない
/// (Unix で親のフォルダと同じファイルシステム)、パス全体が Unicode として読める。古い根が無ければ何もしない
/// (データ根でないことは [`OldRoot::open`] が断る)。何も書かない。
///
/// # Errors
/// どれかに当たれば、その [`Refusal`]。
pub fn check_root_place(root: &Path) -> Result<(), Refusal> {
    if root.to_str().is_none() {
        return Err(Refusal::NonUnicodePath {
            path: root.to_path_buf(),
        });
    }
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return Ok(());
    };
    if fsutil::is_link_or_reparse_point(&meta) {
        return Err(Refusal::Link {
            path: root.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(parent) = root.parent()
            && let Ok(p) = std::fs::metadata(parent)
            && p.dev() != meta.dev()
        {
            return Err(Refusal::MountPoint {
                path: root.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// 古い根を開けない理由。
#[derive(Debug, thiserror::Error)]
pub enum OldRootError {
    /// 取り込める根でない (何も書いていない)。
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// 古い根のロックが取れない (v0.4 が動いている、03 §9)。
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

fn unreadable(e: impl std::fmt::Display) -> OldRootError {
    OldRootError::Refused(Refusal::Unreadable {
        what: e.to_string(),
    })
}

/// 読むだけで開いた v0.4 のデータ根 (03 §12.4 の「読み方」)。落とすと DB を閉じてからロックを外す。
pub struct OldRoot {
    db: Option<ReadOnlyDatabase>,
    root: PathBuf,
    meta: BTreeMap<String, String>,
    deleted: BTreeMap<Id, String>,
    before: FileSnapshot,
    // ロックは DB の後に外す (フィールドは宣言の順に落ちる)。
    lock: DataRootLock,
}

impl std::fmt::Debug for OldRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // パスと `meta` (忘れるの鍵を含む) は出さない (NFR-502)。
        f.debug_struct("OldRoot")
            .field("files", &self.before.len())
            .field("deleted", &self.deleted.len())
            .finish_non_exhaustive()
    }
}

impl OldRoot {
    /// 古い根 `root` のロックを取り、取り込める根か確かめてから、読むだけで開く (03 §12.4 の「取り込める根」の
    /// (1)〜(5) のうちストアの層で決まるもの。危ない置き場所の検査はコアが行う)。
    ///
    /// 書くのは、ロックのファイルだけ (無ければ作る。PID も書く、03 §9)。DB は読むだけのファイルの取っ手で開き、
    /// 修復の要る DB (前回の終わり方が正しくない) は開かずに断る。ジャーナル・索引・退避は読むだけ。
    ///
    /// # Errors
    /// 取り込める根でないとき ([`OldRootError::Refused`])、ロックが取れないとき、ファイルを読めないとき。
    pub fn open(root: &Path) -> Result<Self, OldRootError> {
        // 移行の途中で止まった根は、本体の DB が無いこともあるので、データ根かを見る前に調べる。
        let left = crate::migrate::leftover(root);
        if left.is_none() && !root.join(DB_FILE).is_file() {
            return Err(Refusal::NotDataRoot {
                path: root.to_path_buf(),
            }
            .into());
        }
        let lock = DataRootLock::acquire(root)?;
        if let Some(left) = left.or_else(|| crate::migrate::leftover(root)) {
            return Err(Refusal::Unfinished {
                what: format!("{left} remains"),
            }
            .into());
        }
        let before = FileSnapshot::take(root)?;
        let db = match ReadOnlyDatabase::open(root.join(DB_FILE)) {
            Ok(db) => db,
            Err(redb::DatabaseError::RepairAborted) => {
                return Err(Refusal::Unfinished {
                    what: "the database needs repair".into(),
                }
                .into());
            }
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(LockError::InUse { pid: None }.into());
            }
            Err(e) => return Err(unreadable(e)),
        };
        let meta = read_meta_db_any(&db).map_err(unreadable)?;
        check_versions(&meta)?;
        if meta.get(meta::FORGET_RECHECK).map(String::as_str) == Some(meta::FORGET_RECHECK_SET) {
            return Err(Refusal::ForgetPending.into());
        }
        let last_seq: u64 = meta
            .get(meta::JOURNAL_LAST_SEQ)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let summary = journal::summarize(root, last_seq, journal::Tail::of(Some(&meta)))?;
        let partial_tail = summary.partial > 0 || summary.torn_groups > 0;
        let rewrite_leftover = journal::has_rewrite_leftover(root)?;
        if summary.pending > 0 || partial_tail || rewrite_leftover {
            return Err(Refusal::JournalPending {
                lines: summary.pending,
                partial_tail,
                rewrite_leftover,
            }
            .into());
        }
        check_forget_key(meta.get(meta::FORGET_KEY), &journal::scan_metas(root)?)?;
        let deleted = journal::scan_deleted(root)?;
        Ok(Self {
            db: Some(db),
            root: root.to_path_buf(),
            meta,
            deleted,
            before,
            lock,
        })
    }

    /// 古い根のパス。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// `meta` の値。
    #[must_use]
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(String::as_str)
    }

    /// ジャーナルから取り込んだ最後の連番 (`journal.last_seq`)。
    #[must_use]
    pub fn journal_last_seq(&self) -> u64 {
        self.meta(meta::JOURNAL_LAST_SEQ)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// ジャーナルに削除の行のある ID と、その種類 (取り込まない、03 §6・§12.4)。
    #[must_use]
    pub fn deleted(&self) -> &BTreeMap<Id, String> {
        &self.deleted
    }

    /// 開いたときのファイルの一覧 (照合の (4) の「取り込みの前」)。
    #[must_use]
    pub fn files_before(&self) -> &FileSnapshot {
        &self.before
    }

    /// 1 つの読み取りのトランザクションで DB を読む。
    ///
    /// # Errors
    /// DB を読めないとき。
    pub fn read<T>(&self, f: impl FnOnce(&ExportView<'_>) -> T) -> Result<T, OldRootError> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| unreadable("the database is closed"))?;
        let rtx = db.begin_read().map_err(unreadable)?;
        Ok(f(&ExportView::new(&rtx)))
    }

    /// DB を閉じてから、今のファイルの一覧を取る (照合の (4) の「取り込みの後」)。ロックは持ったまま。
    ///
    /// # Errors
    /// ファイルを読めないとき。
    pub fn close_and_snapshot(&mut self) -> io::Result<FileSnapshot> {
        if let Some(db) = self.db.take() {
            // 閉じるときも書かない (読むだけの取っ手)。失敗しても落とせば閉じる。
            let _ = db.close();
        }
        FileSnapshot::take(&self.root)
    }

    /// DB を閉じて、古い根のロックを渡す (入れ替えまでロックを持ち続けるため、03 §9)。
    #[must_use]
    pub fn into_lock(mut self) -> DataRootLock {
        if let Some(db) = self.db.take() {
            let _ = db.close();
        }
        self.lock
    }
}

/// (1): 形式の版が `0.3` で、`max_writer` が 0.4.0 以上 0.5.0 未満。
fn check_versions(m: &BTreeMap<String, String>) -> Result<(), Refusal> {
    let bad_meta = |what: &str| Refusal::Unreadable {
        what: format!("meta `{what}`"),
    };
    let format_s = m
        .get(meta::FORMAT_VERSION)
        .ok_or_else(|| bad_meta(meta::FORMAT_VERSION))?;
    let format: Version = format_s
        .parse()
        .map_err(|_| bad_meta(meta::FORMAT_VERSION))?;
    let v04: Version = V04_FORMAT.parse().expect("a version");
    if format > v04 {
        return Err(Refusal::NewerFormat {
            format: format_s.clone(),
        });
    }
    let max_writer = m.get(meta::MAX_WRITER).cloned();
    let min: Version = V04_WRITER_MIN.parse().expect("a version");
    let end: Version = V04_WRITER_END.parse().expect("a version");
    let by_v04 = max_writer
        .as_deref()
        .and_then(|w| w.parse::<Version>().ok())
        .is_some_and(|w| w >= min && w < end);
    if format < v04 || !by_v04 {
        return Err(Refusal::NotOpenedByV04 {
            format: format_s.clone(),
            max_writer,
        });
    }
    Ok(())
}

/// (4): DB の鍵とジャーナルの鍵が食い違わない (03 §3.1.1 のフェイルクローズ。`fact_directive` があるのに鍵が無い
/// ことはメモリの層が確かめる)。ジャーナルにだけ鍵があれば、v0.4 が開くときに入れるので、v0.4 で一度開かせる。
fn check_forget_key(in_db: Option<&String>, metas: &[(String, String)]) -> Result<(), Refusal> {
    let shape =
        |v: &str| v.len() == 64 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let found: BTreeSet<&str> = metas
        .iter()
        .filter(|(k, _)| k == meta::FORGET_KEY)
        .map(|(_, v)| v.as_str())
        .collect();
    if in_db.is_some_and(|v| !shape(v)) || found.iter().any(|v| !shape(v)) {
        return Err(Refusal::ForgetKeyUnusable);
    }
    match (in_db, found.len()) {
        (_, 0) => Ok(()),
        (Some(db), 1) if found.contains(db.as_str()) => Ok(()),
        (None, 1) => Err(Refusal::Unfinished {
            what: "the forget key is only in the journal".into(),
        }),
        _ => Err(Refusal::ForgetKeyUnusable),
    }
}

/// データ根の 1 つのファイルの要約 (照合の (4))。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    /// 更新時刻 (UNIX 時刻からのナノ秒)。取れなければ `None`。
    pub modified_ns: Option<u128>,
    /// 中身の SHA-256。シンボリックリンク・リパースポイントはたどらず、`None`。
    pub sha256: Option<[u8; 32]>,
}

/// データ根のすべてのファイル (`/` で区切った相対パス → 要約)。ロックのファイル (`lock`・`lock.pid` とその一時の
/// ファイル) は除く (取り込みが取るロックで変わり、Windows ではロックを持つ間は読めないため、03 §12.4)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSnapshot {
    files: BTreeMap<String, FileEntry>,
}

/// [`FileSnapshot`] から除く、データ根の直下のファイル。
const SNAPSHOT_SKIP: [&str; 3] = [LOCK_FILE, PID_FILE, "lock.pid.tmp"];

/// OS とファイルの管理の道具が、利用者の操作と関わりなく作るファイル (macOS の `.DS_Store`・`._*`、Windows の
/// `Thumbs.db`・`desktop.ini`)。どの深さでも [`FileSnapshot`] から除き (古い根の比べに含めない)、`import-v04` の
/// 作りかけのフォルダにあっても、evorag の作ったものと同じく消してよい (03 §12.4 の「止まった後の扱い」の (c))。
#[must_use]
pub fn is_os_file(name: &str) -> bool {
    name == ".DS_Store"
        || name.starts_with("._")
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

impl FileSnapshot {
    /// `root` の下のファイルを読んで要約する。何も書かない。
    ///
    /// # Errors
    /// フォルダ・ファイルを読めないとき。
    pub fn take(root: &Path) -> io::Result<Self> {
        let mut out = Self::default();
        walk(root, root, Hashes::Take, &mut out.files)?;
        Ok(out)
    }

    /// 中身のハッシュを取らずに、ファイルの大きさと更新時刻だけを要約する (改名の隙の確かめ、03 §10.2)。
    /// [`FileEntry::sha256`] はすべて `None`。何も書かない。
    ///
    /// # Errors
    /// フォルダを読めないとき。
    pub fn take_listing(root: &Path) -> io::Result<Self> {
        let mut out = Self::default();
        walk(root, root, Hashes::Skip, &mut out.files)?;
        Ok(out)
    }

    /// ファイルの数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// ファイルが 1 つも無い。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// ファイルの要約 (`/` で区切った相対パスの順)。
    pub fn entries(&self) -> impl Iterator<Item = (&str, &FileEntry)> {
        self.files.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// 一覧の全体をまとめたハッシュ (64 桁の小文字の 16 進。`import.source` のファイルの要約、03 §3.1.1)。
    #[must_use]
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for (path, e) in &self.files {
            h.update((path.len() as u64).to_be_bytes());
            h.update(path.as_bytes());
            h.update(e.size.to_be_bytes());
            h.update(e.modified_ns.unwrap_or(0).to_be_bytes());
            h.update(e.sha256.unwrap_or([0; 32]));
        }
        hex(&h.finalize())
    }

    /// `other` と違うファイル (足された・消えた・大きさか更新時刻か中身の変わったもの) の相対パス。
    #[must_use]
    pub fn changed(&self, other: &Self) -> Vec<String> {
        let keys: BTreeSet<&String> = self.files.keys().chain(other.files.keys()).collect();
        keys.into_iter()
            .filter(|k| self.files.get(*k) != other.files.get(*k))
            .cloned()
            .collect()
    }
}

/// [`walk`] が中身のハッシュを取るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hashes {
    Take,
    Skip,
}

fn walk(
    base: &Path,
    dir: &Path,
    hashes: Hashes,
    out: &mut BTreeMap<String, FileEntry>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        let rel = rel_path(base, &path);
        if dir == base && SNAPSHOT_SKIP.contains(&rel.as_str()) {
            continue;
        }
        if is_os_file(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());
        if fsutil::is_link_or_reparse_point(&meta) {
            out.insert(
                rel,
                FileEntry {
                    size: meta.len(),
                    modified_ns,
                    sha256: None,
                },
            );
        } else if meta.is_dir() {
            walk(base, &path, hashes, out)?;
        } else {
            let sha256 = match hashes {
                Hashes::Take => Some(sha256_file(&path)?),
                Hashes::Skip => None,
            };
            out.insert(
                rel,
                FileEntry {
                    size: meta.len(),
                    modified_ns,
                    sha256,
                },
            );
        }
    }
    Ok(())
}

fn rel_path(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// 64 桁の小文字の 16 進。
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
        s
    })
}

/// 1 つの種類の記録の、件数と (ID ‖ 本体の CBOR のバイト列) を ID の順に並べた SHA-256 (03 §12.4 の照合の (1))。
/// 記録は ID の順に足す。
#[derive(Debug, Clone, Default)]
pub struct KindDigest {
    count: u64,
    hasher: Sha256,
}

/// [`KindDigest`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSummary {
    pub count: u64,
    /// 64 桁の小文字の 16 進。
    pub sha256: String,
}

impl KindDigest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 記録を 1 件足す (ID の順に)。
    pub fn add(&mut self, env: &Envelope) {
        self.count += 1;
        self.hasher.update(env.id.as_bytes());
        self.hasher.update(env.body_cbor());
    }

    #[must_use]
    pub fn finish(self) -> DigestSummary {
        DigestSummary {
            count: self.count,
            sha256: hex(&self.hasher.finalize()),
        }
    }
}

/// ジャーナルから作り直した DB と、今の DB の食い違い (照合の (3))。数は行の数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildDiff {
    /// ジャーナルに書く種類の記録で、片方にだけある・中身が違うもの。
    pub records: usize,
    /// それらの記録の関係 (順向きと逆向き) で、片方にだけある・属性が違うもの。
    pub links: usize,
    /// それらの種類の副索引で、片方にだけあるもの。
    pub lookups: usize,
    /// ジャーナルにも書く `meta` のキー ([`JOURNALED_META`]) で、値が違うもの。
    pub meta: Vec<String>,
}

impl RebuildDiff {
    /// 食い違いが無い。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records == 0 && self.links == 0 && self.lookups == 0 && self.meta.is_empty()
    }
}

/// 照合の (3) を行えない理由。
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

/// `store` のジャーナルのファイルを、空のフォルダ `scratch.data_root` に写して開き (DB を新しく作り、ジャーナルの
/// すべての行を通常の取り込みで当てる、03 §6)、`store` の DB と比べる (03 §12.4 の照合の (3))。比べ終えたら
/// `scratch.data_root` を消す (消せなければ失敗)。
///
/// 比べるのは、ジャーナルに書く種類の記録 (封筒のバイト列)、その関係と副索引、ジャーナルにも書く `meta` のキー
/// ([`JOURNALED_META`])。ジャーナルに書かない種類 (`threshold` など) と、データ根ごとの `meta` (ID・版・連番) は
/// 比べない。
///
/// # Errors
/// 写す先が空でないとき、ファイルを写せない・開けない・読めないとき、写す先を消せないとき。
pub fn rebuild_check(
    store: &Store,
    scratch: OpenOptions,
    plan: FormatPlan,
) -> Result<RebuildDiff, RebuildError> {
    let dir = scratch.data_root.clone();
    if dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", dir.display()),
        )
        .into());
    }
    let result = (|| -> Result<RebuildDiff, RebuildError> {
        let from = store.data_root().join(JOURNAL_DIR);
        let to = dir.join(JOURNAL_DIR);
        fsutil::create_private_dir(&dir)?;
        fsutil::create_private_dir(&to)?;
        if from.is_dir() {
            for e in std::fs::read_dir(&from)? {
                let e = e?;
                if e.file_type()?.is_file() {
                    std::fs::copy(e.path(), to.join(e.file_name()))?;
                }
            }
        }
        // ジャーナルの `import.source` で作りかけの取り込みの根になるので、取り込みの照合の入口で開く。
        let lock = DataRootLock::acquire(&dir).map_err(OpenError::from)?;
        let (rebuilt, _) = Store::open_for_import(scratch, plan, lock)?;
        Ok(diff(store, &rebuilt)?)
    })();
    let removed = remove_dir_retry(&dir);
    let diff = result?;
    removed?;
    Ok(diff)
}

/// フォルダ `dir` を中身ごと消す (無ければ何もしない)。開いていたストアを落とした直後は、Windows でファイルの取っ手が
/// 残って消せないことがあるので、短く試し直す (`import-v04` が作りかけの新しい根を消すときにも使う)。
///
/// # Errors
/// 試し直しても消せないとき。
pub fn remove_dir_retry(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    fsutil::retry_transient(|| match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// 表の全体を (キー → 値) で読む。`keep` が残したものだけ。
fn rows(
    rtx: &redb::ReadTransaction,
    def: crate::store::Raw,
    mut keep: impl FnMut(&[u8]) -> bool,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, StoreError> {
    let table = match rtx.open_table(def) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(e) => return Err(crate::store::db(e)),
    };
    let mut out = BTreeMap::new();
    for item in table.iter().map_err(crate::store::db)? {
        let (k, v) = item.map_err(crate::store::db)?;
        if keep(k.value()) {
            out.insert(k.value().to_vec(), v.value().to_vec());
        }
    }
    Ok(out)
}

fn count_diff(a: &BTreeMap<Vec<u8>, Vec<u8>>, b: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
    let keys: BTreeSet<&Vec<u8>> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).count()
}

fn diff(a: &Store, b: &Store) -> Result<RebuildDiff, StoreError> {
    let ra = a.db.begin_read().map_err(crate::store::db)?;
    let rb = b.db.begin_read().map_err(crate::store::db)?;
    let journaled_record =
        |k: &[u8]| split_record_key(k).is_some_and(|(kind, _)| a.is_journaled(&kind));
    let rec_a = rows(&ra, RECORDS, journaled_record)?;
    let rec_b = rows(&rb, RECORDS, journaled_record)?;
    let owners: BTreeSet<Id> = rec_a
        .keys()
        .chain(rec_b.keys())
        .filter_map(|k| split_record_key(k).map(|(_, id)| id))
        .collect();
    // 関係の行の持ち主: 順向きの行 (from, rel, to) は from、逆向きの行 (to, ~rel, from) は from。
    let owned_link = |k: &[u8]| match key::decode(k).ok().as_deref() {
        Some([OwnedPart::Id(x), OwnedPart::Str(rel), OwnedPart::Id(y)]) => {
            let owner = if rel.starts_with(crate::relations::REVERSE) {
                y
            } else {
                x
            };
            owners.contains(owner)
        }
        _ => false,
    };
    let links_a = rows(&ra, LINKS, owned_link)?;
    let links_b = rows(&rb, LINKS, owned_link)?;
    let journaled_lookup = |k: &[u8]| match key::decode(k).ok().as_deref() {
        Some([OwnedPart::Str(kind), ..]) => a.is_journaled(kind),
        _ => false,
    };
    let lookups_a = rows(&ra, LOOKUP, journaled_lookup)?;
    let lookups_b = rows(&rb, LOOKUP, journaled_lookup)?;
    let wanted = |k: &[u8]| JOURNALED_META.iter().any(|m| m.as_bytes() == k);
    let meta_a = rows(&ra, META, wanted)?;
    let meta_b = rows(&rb, META, wanted)?;
    let meta = JOURNALED_META
        .iter()
        .filter(|m| meta_a.get(m.as_bytes()) != meta_b.get(m.as_bytes()))
        .map(|m| (*m).to_owned())
        .collect();
    Ok(RebuildDiff {
        records: count_diff(&rec_a, &rec_b),
        links: count_diff(&links_a, &links_b),
        lookups: count_diff(&lookups_a, &lookups_b),
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_import_dir_and_the_renamed_old_root_sit_beside_the_old_root() {
        let old = Path::new("/data/evorag");
        let dir = import_dir(old).expect("a named root");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some("evorag.v05-import")
        );
        assert_eq!(
            dir.parent(),
            std::path::absolute(old)
                .ok()
                .as_deref()
                .and_then(Path::parent)
        );
        let renamed = renamed_old_root(old, 1_794_700_800_000).expect("a named root");
        assert_eq!(
            renamed.file_name().and_then(|n| n.to_str()),
            Some("evorag-v0.4-20261115T000000000Z")
        );
    }

    /// (1) の判定: 形式の版 `0.3` で v0.4 が書き込みで開いた根だけを通す。
    #[test]
    fn only_a_format_0_3_root_opened_by_v0_4_passes_the_version_check() {
        let m = |format: &str, max: Option<&str>| {
            let mut m = BTreeMap::new();
            m.insert(meta::FORMAT_VERSION.to_owned(), format.to_owned());
            if let Some(w) = max {
                m.insert(meta::MAX_WRITER.to_owned(), w.to_owned());
            }
            m
        };
        assert_eq!(check_versions(&m("0.3", Some("0.4.0"))), Ok(()));
        assert_eq!(check_versions(&m("0.3", Some("0.4.2"))), Ok(()));
        assert!(matches!(
            check_versions(&m("0.3", Some("0.3.5"))),
            Err(Refusal::NotOpenedByV04 { .. })
        ));
        assert!(matches!(
            check_versions(&m("0.3", None)),
            Err(Refusal::NotOpenedByV04 { .. })
        ));
        assert!(matches!(
            check_versions(&m("0.2", Some("0.4.0"))),
            Err(Refusal::NotOpenedByV04 { .. })
        ));
        assert!(matches!(
            check_versions(&m("0.5", Some("0.5.0"))),
            Err(Refusal::NewerFormat { .. })
        ));
    }

    /// (4) の判定: DB とジャーナルの鍵が食い違えば使えない。ジャーナルにだけあれば v0.4 で一度開かせる。
    #[test]
    fn the_forget_key_check_fails_closed() {
        let k1 = "ab".repeat(32);
        let k2 = "cd".repeat(32);
        let line = |v: &str| (meta::FORGET_KEY.to_owned(), v.to_owned());
        assert_eq!(check_forget_key(Some(&k1), &[line(&k1)]), Ok(()));
        assert_eq!(check_forget_key(Some(&k1), &[]), Ok(()));
        assert_eq!(check_forget_key(None, &[]), Ok(()));
        assert_eq!(
            check_forget_key(Some(&k1), &[line(&k2)]),
            Err(Refusal::ForgetKeyUnusable)
        );
        assert_eq!(
            check_forget_key(None, &[line(&k1), line(&k2)]),
            Err(Refusal::ForgetKeyUnusable)
        );
        assert_eq!(
            check_forget_key(Some(&"AB".repeat(32)), &[]),
            Err(Refusal::ForgetKeyUnusable)
        );
        assert!(matches!(
            check_forget_key(None, &[line(&k1)]),
            Err(Refusal::Unfinished { .. })
        ));
    }

    #[test]
    fn the_snapshot_skips_the_lock_files_and_sees_changes() {
        let dir = crate::testutil::TempDir::new("snapshot");
        std::fs::write(dir.path().join("a"), b"1").expect("write");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub").join("b"), b"2").expect("write");
        let before = FileSnapshot::take(dir.path()).expect("snapshot");
        std::fs::write(dir.path().join(LOCK_FILE), b"").expect("write");
        std::fs::write(dir.path().join(PID_FILE), b"9").expect("write");
        let same = FileSnapshot::take(dir.path()).expect("snapshot");
        assert_eq!(before, same);
        assert_eq!(before.digest(), same.digest());
        assert_eq!(before.len(), 2);
        std::fs::write(dir.path().join("sub").join("b"), b"3").expect("write");
        let after = FileSnapshot::take(dir.path()).expect("snapshot");
        assert_eq!(before.changed(&after), vec!["sub/b".to_owned()]);
        assert_ne!(before.digest(), after.digest());
    }
}
