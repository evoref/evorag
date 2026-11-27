//! ロックしたデータ根の改名 (03 §10.2 の「改名の隙」)。`evorag reset` の (1) の改名と、`import-v04` の入れ替えの
//! 2 つの改名 (03 §12.4) が、同じ [`rename_locked_root`] を使う。
//!
//! 改名の間に、ほかのプロセスが対象の根を開かないようにする。
//!
//! - Unix: 対象の根のロックを持ったまま改名する (ロックはファイルの取っ手に付くので、改名の後も効く)。
//! - Windows: ロックのファイルを開いたままではフォルダを改名できないので、ロックを放す → 改名する → 改名した先で
//!   ロックを取り直し、改名の前に取った要約 ([`RootSummary`]: `data_root.id`・`journal.last_seq`・ファイルの一覧の
//!   大きさと更新時刻) が変わっていないことを確かめる。取り直せない・合わないときは、改名を元に戻す。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use redb::ReadOnlyDatabase;

use crate::fsutil;
use crate::import_v04::FileSnapshot;
use crate::lock::{DataRootLock, LockError};
use crate::meta;
use crate::store::{DB_FILE, read_meta_db_any};

/// データ根の `meta` を、読むだけで開いて読む (redb の [`ReadOnlyDatabase`]。何も書かず、修復も compact もしない)。
/// 呼び手は、対象の根のロックを持っていること (ほかのプロセスが書いている DB を読まないため)。
///
/// # Errors
/// `evorag.redb` が無い・開けない (修復が要るときを含む)・`meta` を読めないとき。
pub fn read_meta(root: &Path) -> io::Result<BTreeMap<String, String>> {
    let db = ReadOnlyDatabase::open(root.join(DB_FILE)).map_err(io::Error::other)?;
    let m = read_meta_db_any(&db).map_err(io::Error::other);
    // 閉じるときも書かない (読むだけの取っ手)。
    let _ = db.close();
    m
}

/// 改名の前と後で比べる、データ根の要約 (03 §10.2 の「改名の隙」)。ロックのファイル (`lock`・`lock.pid`) は含めない
/// (取り直すと書き換わるため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSummary {
    /// `meta` の `data_root.id`。
    pub data_root_id: Option<String>,
    /// `meta` の `journal.last_seq` (無ければ 0)。
    pub journal_last_seq: u64,
    files: FileSnapshot,
}

impl RootSummary {
    /// 根 `root` の要約を取る。何も書かない。呼び手は `root` のロックを持っていること。
    ///
    /// # Errors
    /// DB の `meta` を読めない・フォルダを読めないとき。
    pub fn take(root: &Path) -> io::Result<Self> {
        let m = read_meta(root)?;
        Ok(Self {
            data_root_id: m.get(meta::DATA_ROOT_ID).cloned(),
            journal_last_seq: m
                .get(meta::JOURNAL_LAST_SEQ)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            files: FileSnapshot::take_listing(root)?,
        })
    }

    /// ファイルの数 (ロックのファイルを除く)。
    #[must_use]
    pub fn files(&self) -> usize {
        self.files.len()
    }
}

/// フォルダの改名を、ほかのプログラム (ウイルス対策・索引) が中のファイルを一時的に掴んでいる間、試し直す時間の
/// 合計 (Windows。書いた直後の DB をウイルス対策が数秒掴むことがあるため)。Unix の `EBUSY` も同じく試し直す。
pub const FOLDER_RENAME_PATIENCE: Duration = Duration::from_secs(12);

/// 改名を終えた結果。
#[derive(Debug)]
pub struct Renamed {
    /// 改名した先のロック。
    pub lock: DataRootLock,
    /// 改名の後に親のフォルダを書き切れなかった (改名はした。電源断で改名の前に戻ることがあるが、どちらの名前でも
    /// 次の手順が見分けられる)。
    pub sync_error: Option<io::Error>,
}

/// [`rename_locked_root`] が改名できなかった理由。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenameRootError {
    /// 改名の先が既にある。何も変えていない (ロックは放した)。
    #[error("{} already exists", .0.display())]
    Exists(PathBuf),
    /// 改名の前の要約を取れない。何も変えていない。
    #[error("the data root cannot be summarized before the rename: {0}")]
    Summary(#[source] io::Error),
    /// ロックを放した隙に、ほかのプロセスが根を開いたので改名できなかった (Windows)。根は元の名前のまま。
    #[error("another process opened the data root before it was renamed: {0}")]
    InUse(#[source] io::Error),
    /// 改名できなかった。根は元の名前のまま。
    #[error("the data root could not be renamed: {0}")]
    Rename(#[source] io::Error),
    /// 改名した先でロックを取り直せなかった (改名の隙にほかのプロセスが開いた、03 §10.2)。改名は元に戻した。
    #[error("the renamed data root could not be locked again: {0}")]
    Relock(#[source] LockError),
    /// 改名した先の要約が、改名の前と違う (改名の隙にほかのプロセスが書いた)。改名は元に戻した。
    #[error("the data root changed while it was being renamed")]
    Changed,
    /// `cause` で改名を元に戻そうとしたが、戻せなかった。根は `left` にある。
    #[error("{cause}; and the rename could not be undone, the data root is left at {}: {error}", left.display())]
    NotUndone {
        cause: Box<RenameRootError>,
        left: PathBuf,
        #[source]
        error: io::Error,
    },
}

impl RenameRootError {
    /// ロックの理由 (改名の隙にほかのプロセスが開いた・書いた) で断ったか。CLI は終了コード 2 で示す (03 §10.2)。
    #[must_use]
    pub fn is_lock(&self) -> bool {
        match self {
            Self::InUse(_) | Self::Relock(_) | Self::Changed => true,
            Self::NotUndone { cause, .. } => cause.is_lock(),
            Self::Exists(_) | Self::Summary(_) | Self::Rename(_) => false,
        }
    }
}

/// 改名の隙の閉じ方 (03 §10.2)。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// ロックを持ったまま改名する (Unix)。
    KeepLock,
    /// ロックを放して改名し、改名した先で取り直して要約を確かめる (Windows)。
    Relock,
}

impl Gap {
    /// この OS の閉じ方。
    pub const NATIVE: Gap = if cfg!(windows) {
        Gap::Relock
    } else {
        Gap::KeepLock
    };
}

/// 試験だけ: 改名の閉じ方と、隙にほかのプロセスの動きを作る関数。
#[doc(hidden)]
pub struct RenameHooks<'a> {
    pub gap: Gap,
    /// 改名の直前 (Windows の閉じ方ではロックを放した後) に、元の場所を渡して呼ぶ。
    pub before: Option<&'a mut dyn FnMut(&Path)>,
    /// 改名の直後 (Windows の閉じ方ではロックを取り直す前) に、改名した先を渡して呼ぶ。
    pub between: Option<&'a mut dyn FnMut(&Path)>,
    /// フォルダの改名を試し直す時間の合計 ([`FOLDER_RENAME_PATIENCE`])。
    pub patience: Duration,
}

impl Default for RenameHooks<'_> {
    fn default() -> Self {
        Self {
            gap: Gap::NATIVE,
            before: None,
            between: None,
            patience: FOLDER_RENAME_PATIENCE,
        }
    }
}

/// ロック `lock` を持っているデータ根 `from` を、同じ親のフォルダの `to` に改名し、改名した先のロックを返す
/// (03 §10.2 の「改名の隙」)。`to` があれば改名しない (置き換えない)。
///
/// # Errors
/// [`RenameRootError`] の通り。どの失敗でも、根は `from` のまま (元に戻せなければ [`RenameRootError::NotUndone`]
/// で、残った場所を示す)。失敗したときはロックを放している。
pub fn rename_locked_root(
    lock: DataRootLock,
    from: &Path,
    to: &Path,
) -> Result<Renamed, RenameRootError> {
    rename_locked_root_with(lock, from, to, RenameHooks::default())
}

fn call(hook: &mut Option<&mut dyn FnMut(&Path)>, p: &Path) {
    if let Some(f) = hook.as_mut() {
        f(p);
    }
}

/// [`rename_locked_root`] の本体。試験が [`RenameHooks`] で閉じ方を選び、隙にほかのプロセスの動きを作る。
#[doc(hidden)]
pub fn rename_locked_root_with(
    lock: DataRootLock,
    from: &Path,
    to: &Path,
    mut hooks: RenameHooks<'_>,
) -> Result<Renamed, RenameRootError> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(RenameRootError::Exists(to.to_path_buf()));
    }
    let patience = hooks.patience;
    let lock = match hooks.gap {
        Gap::KeepLock => {
            call(&mut hooks.before, from);
            rename_patiently(from, to, patience).map_err(|e| rename_error(e, to))?;
            call(&mut hooks.between, to);
            lock.relocated(to)
        }
        Gap::Relock => {
            let before = RootSummary::take(from).map_err(RenameRootError::Summary)?;
            drop(lock);
            call(&mut hooks.before, from);
            if let Err(e) = rename_patiently(from, to, patience) {
                return Err(match rename_error(e, to) {
                    RenameRootError::Rename(e) if in_use(from) => RenameRootError::InUse(e),
                    other => other,
                });
            }
            call(&mut hooks.between, to);
            let lock = match acquire_patiently(to) {
                Ok(l) => l,
                Err(e) => return Err(undo(RenameRootError::Relock(e), from, to, patience)),
            };
            match RootSummary::take(to) {
                Ok(after) if after == before => lock,
                Ok(_) => {
                    drop(lock);
                    return Err(undo(RenameRootError::Changed, from, to, patience));
                }
                Err(e) => {
                    drop(lock);
                    return Err(undo(RenameRootError::Summary(e), from, to, patience));
                }
            }
        }
    };
    Ok(Renamed {
        lock,
        sync_error: sync_parents(from, to).err(),
    })
}

/// 改名の後に、`from` と `to` の親のフォルダを書き切る。
fn sync_parents(from: &Path, to: &Path) -> io::Result<()> {
    let parent = |p: &Path| {
        p.parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    let (a, b) = (parent(to), parent(from));
    if let Some(a) = &a {
        fsutil::sync_dir(a)?;
    }
    if let Some(b) = b.filter(|b| Some(b) != a.as_ref()) {
        fsutil::sync_dir(&b)?;
    }
    Ok(())
}

/// フォルダの改名を、一時の掴み ([`fsutil::is_lock_error`]) の間は `patience` まで試し直す。
fn rename_patiently(from: &Path, to: &Path, patience: Duration) -> io::Result<()> {
    let started = Instant::now();
    let mut wait = Duration::from_millis(50);
    loop {
        match fsutil::rename_dir_no_replace(from, to) {
            Err(e) if fsutil::is_lock_error(&e) && started.elapsed() < patience => {
                std::thread::sleep(wait.min(patience.saturating_sub(started.elapsed())));
                wait = (wait * 2).min(Duration::from_secs(1));
            }
            r => return r,
        }
    }
}

/// ロックを取る。ほかのプログラムが一時的にロックのファイルを掴んでいるための失敗だけを、
/// [`fsutil::LOCK_RETRY_WAITS`] の間隔で試し直す (ほかの evorag が持つロックは試し直さない)。
fn acquire_patiently(root: &Path) -> Result<DataRootLock, LockError> {
    for wait in fsutil::LOCK_RETRY_WAITS {
        match DataRootLock::acquire(root) {
            Err(LockError::Io(e)) if fsutil::is_lock_error(&e) => std::thread::sleep(wait),
            r => return r,
        }
    }
    DataRootLock::acquire(root)
}

/// ほかのプロセスが根 `root` のロックを持っているか (何も書かずに確かめる)。
fn in_use(root: &Path) -> bool {
    matches!(
        DataRootLock::acquire_quiet(root),
        Err(LockError::InUse { .. })
    )
}

fn rename_error(e: io::Error, to: &Path) -> RenameRootError {
    if e.kind() == io::ErrorKind::AlreadyExists {
        RenameRootError::Exists(to.to_path_buf())
    } else {
        RenameRootError::Rename(e)
    }
}

/// 改名を元に戻す (`to` → `from`)。戻せなければ、`cause` に残った場所を添える。
fn undo(cause: RenameRootError, from: &Path, to: &Path, patience: Duration) -> RenameRootError {
    match rename_patiently(to, from, patience) {
        Ok(()) => cause,
        Err(error) => RenameRootError::NotUndone {
            cause: Box::new(cause),
            left: to.to_path_buf(),
            error,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testutil::TempDir;
    use crate::{OpenOptions, Store};
    use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

    /// 閉じたデータ根を `<一時>/<name>` に作る。
    fn make_root(tmp: &TempDir, name: &str) -> PathBuf {
        let root = tmp.path().join(name);
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_795_000_000_000)));
        let (store, _) = Store::open(OpenOptions {
            data_root: root.clone(),
            evorag_version: "0.5.0".into(),
            kinds: Vec::new(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
            config_file: None,
        })
        .expect("create a data root");
        drop(store);
        root
    }

    /// 試験の閉じ方。改名の試し直しは短くする。
    fn hooks<'a>(
        gap: Gap,
        before: Option<&'a mut dyn FnMut(&Path)>,
        between: Option<&'a mut dyn FnMut(&Path)>,
    ) -> RenameHooks<'a> {
        RenameHooks {
            gap,
            before,
            between,
            patience: Duration::from_millis(300),
        }
    }

    #[test]
    fn the_summary_reads_the_meta_and_skips_the_lock_files() {
        let tmp = TempDir::new("rename-summary");
        let root = make_root(&tmp, "a");
        let lock = DataRootLock::acquire(&root).expect("lock");
        let s = RootSummary::take(&root).expect("summary");
        assert!(s.data_root_id.is_some());
        drop(lock);
        let again = DataRootLock::acquire(&root).expect("lock again");
        assert_eq!(RootSummary::take(&root).expect("summary"), s);
        drop(again);
        // OS の作るファイルは比べに含めない (03 §12.4 の (c))。
        std::fs::write(root.join(".DS_Store"), b"x").expect("write");
        std::fs::write(root.join("journal").join("._x.jsonl"), b"x").expect("write");
        assert_eq!(RootSummary::take(&root).expect("summary"), s);
        std::fs::write(root.join("extra"), b"x").expect("write");
        assert_ne!(RootSummary::take(&root).expect("summary"), s);
    }

    /// 取り直す閉じ方 (Windows の規則) は、どの OS でも動く。改名した先のロックを返す。
    #[test]
    fn a_relocking_rename_moves_the_root_and_returns_the_lock_at_the_new_place() {
        let tmp = TempDir::new("rename-relock");
        let from = make_root(&tmp, "a");
        let to = tmp.path().join("b");
        let lock = DataRootLock::acquire(&from).expect("lock");
        let done = rename_locked_root_with(lock, &from, &to, hooks(Gap::Relock, None, None))
            .expect("rename");
        assert!(done.sync_error.is_none());
        assert_eq!(done.lock.path(), to.join(crate::lock::LOCK_FILE));
        assert!(!from.exists());
        assert!(matches!(
            DataRootLock::acquire(&to),
            Err(LockError::InUse { .. })
        ));
    }

    /// 改名の隙にほかのプロセスがロックを取った・書いたときは、改名を元に戻して断る (03 §10.2)。
    #[test]
    fn a_relocking_rename_is_undone_when_the_gap_was_used() {
        let tmp = TempDir::new("rename-gap");
        let from = make_root(&tmp, "a");
        let to = tmp.path().join("b");

        let lock = DataRootLock::acquire(&from).expect("lock");
        let mut write = |at: &Path| {
            std::fs::write(at.join("journal").join("x.jsonl"), b"{}\n").expect("write");
        };
        let err =
            rename_locked_root_with(lock, &from, &to, hooks(Gap::Relock, None, Some(&mut write)))
                .expect_err("changed in the gap");
        assert!(matches!(err, RenameRootError::Changed), "{err}");
        assert!(err.is_lock());
        assert!(from.join(DB_FILE).is_file(), "the rename is undone");
        assert!(!to.exists());
        std::fs::remove_file(from.join("journal").join("x.jsonl")).expect("clean up");

        // Unix のロックは開いた取っ手ごとなので、同じプロセスの中でもう 1 つ取ると断られる (Windows も同じ)。
        let mut held = None;
        let lock = DataRootLock::acquire(&from).expect("lock");
        let mut take = |at: &Path| {
            held = Some(DataRootLock::acquire(at).expect("another process locks it"));
        };
        let err =
            rename_locked_root_with(lock, &from, &to, hooks(Gap::Relock, None, Some(&mut take)))
                .expect_err("locked in the gap");
        if cfg!(windows) {
            // ロックのファイルを開いた取っ手があるので、元に戻す改名もできない。
            assert!(
                matches!(&err, RenameRootError::NotUndone { cause, .. } if matches!(**cause, RenameRootError::Relock(_))),
                "{err}"
            );
            assert!(err.is_lock());
            drop(held.take());
            fsutil::rename_dir_no_replace(&to, &from).expect("undo by hand");
        } else {
            assert!(matches!(err, RenameRootError::Relock(_)), "{err}");
            assert!(from.join(DB_FILE).is_file());
        }
    }

    /// ロックを放してから改名するまでの隙に、ほかのプロセスが元の側を開いたら、ロックの理由 (2) で断る。
    #[test]
    fn a_root_opened_before_the_rename_is_reported_as_in_use() {
        let tmp = TempDir::new("rename-before");
        let from = make_root(&tmp, "a");
        let to = tmp.path().join("b");
        let mut held = None;
        let lock = DataRootLock::acquire(&from).expect("lock");
        let mut take = |at: &Path| {
            held = Some(DataRootLock::acquire(at).expect("another process locks it"));
        };
        let r =
            rename_locked_root_with(lock, &from, &to, hooks(Gap::Relock, Some(&mut take), None));
        if cfg!(windows) {
            // 開いたロックのファイルがあるのでフォルダを改名できない。
            let err = r.expect_err("in use");
            assert!(matches!(err, RenameRootError::InUse(_)), "{err}");
            assert!(err.is_lock());
            assert!(from.join(DB_FILE).is_file());
            assert!(!to.exists());
        } else {
            // Unix では改名はできるが、改名した先のロックを取り直せないので元に戻す。
            let err = r.expect_err("relock fails");
            assert!(matches!(err, RenameRootError::Relock(_)), "{err}");
            assert!(from.join(DB_FILE).is_file());
        }
    }

    #[test]
    fn a_rename_never_replaces_an_existing_folder() {
        let tmp = TempDir::new("rename-exists");
        let from = make_root(&tmp, "a");
        let to = tmp.path().join("b");
        std::fs::create_dir(&to).expect("an empty folder");
        for gap in [Gap::Relock, Gap::KeepLock] {
            let lock = DataRootLock::acquire(&from).expect("lock");
            let err = rename_locked_root_with(lock, &from, &to, hooks(gap, None, None))
                .expect_err("the target exists");
            assert!(matches!(err, RenameRootError::Exists(_)), "{err}");
            assert!(from.join(DB_FILE).is_file());
        }
        // 確かめた後に作られた先も置き換えない (OS の置き換えない改名)。
        std::fs::remove_dir(&to).expect("remove");
        let lock = DataRootLock::acquire(&from).expect("lock");
        let mut make = |_: &Path| std::fs::create_dir(&to).expect("made in the gap");
        let err =
            rename_locked_root_with(lock, &from, &to, hooks(Gap::Relock, Some(&mut make), None))
                .expect_err("the target appeared");
        assert!(matches!(err, RenameRootError::Exists(_)), "{err}");
        assert!(from.join(DB_FILE).is_file());
    }

    /// ロックを持ったままの改名 (Unix の規則)。Windows ではロックのファイルを開いたままフォルダを改名できない。
    #[cfg(unix)]
    #[test]
    fn a_rename_keeping_the_lock_keeps_the_root_locked() {
        let tmp = TempDir::new("rename-keep");
        let from = make_root(&tmp, "a");
        let to = tmp.path().join("b");
        let lock = DataRootLock::acquire(&from).expect("lock");
        let mut check = |at: &Path| {
            assert!(matches!(
                DataRootLock::acquire(at),
                Err(LockError::InUse { .. })
            ));
        };
        let done = rename_locked_root_with(
            lock,
            &from,
            &to,
            hooks(Gap::KeepLock, None, Some(&mut check)),
        )
        .expect("rename");
        assert_eq!(done.lock.path(), to.join(crate::lock::LOCK_FILE));
    }
}
