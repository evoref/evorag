//! 入れ替えと、止まった取り込みの続き・片付け (03 §12.4 の「入れ替え」・「改名の前の比べ」・「止まった後の扱い」、
//! §10.2 の「改名の隙」)。
//!
//! 新しい根 `N` (`<P>.v05-import`) と古い根の元の名前 `P`・改名の後の名前 `R` (`P` の親の、`import.source` の
//! `path` の名前) の状態から、することを決める。どの手順も、何度当てても同じ所に落ち着き、どこで止まっても古い根を
//! 消さない。
//!
//! | 状態 | すること |
//! |---|---|
//! | `N` が無い | 何もしない (`import-v04` は取り込みから始める) |
//! | `N` に evorag と OS の作らないものがある・形式が 0.5 でない | ロックも取らずに触れない ([`ImportError::Leftover`]) |
//! | `N` のロックが取れない | 触れない (ほかの evorag が使っている。終了コード 2) |
//! | `N` の DB があるのに読めない・照合の結果があるのにジャーナルが無い | 触れない ([`ImportError::DamagedLeftover`]) |
//! | `N` の DB が無い・`import.source` があり `import.report` が無い (作りかけ) | `P` があれば `N` を消す (`import-v04` はやり直す)。`P` が無ければ触れない (2) |
//! | `N` に `import.report` があり、`P` が古い根 (1 つ目の改名の前) | 比べて合えば 2 つの改名、合わなければ `N` を消してやり直す |
//! | `N` に `import.report` があり、`P` が無く `R` がある (改名の間) | 比べて合えば 2 つ目の改名、合わなければ `R` を `P` に戻し、`N` を消してやり直す |
//! | `P` と `R` の両方がある・`P` の ID が違う・`P` を読めない・どちらも無い | 何も変えずに止める (それぞれの変種) |
//!
//! 「比べる」は、古い根のロックを持ったまま、`import.source` の `data_root_id`・`journal_last_seq`・ファイルの要約を、
//! 今の古い根と比べること。照合を終えた `N` を消す前には、照合の後に `N` へ書いていない (`N` の `journal.last_seq` が
//! `import.report` の値のまま) ことも確かめる。

use std::path::{Path, PathBuf};

use evorag_store::import_v04::{
    FileSnapshot, import_dir, is_os_file, remove_dir_retry, renamed_old_root,
};
use evorag_store::lock::{DataRootLock, LOCK_FILE};
use evorag_store::root_rename::{
    Gap, RenameHooks, RootSummary, read_meta, rename_locked_root_with,
};
use evorag_store::{DB_FILE, FORMAT_V05, JOURNAL_DIR, RESERVE_FILE, fsutil, meta};

use super::{
    Confirm, ImportError, ImportOptions, ImportReport, Imported, Plan, REBUILD_DIR, SwapHooks,
    check_locations, run_inner,
};

/// evorag が新しい根の直下に作りうるもの (これと OS の作るファイルだけから成るフォルダを、作りかけとして消して
/// よい)。`evorag.redb.` で始まる名前 (移行と compact の一時のファイル) も含める。
const MADE_BY_EVORAG: [&str; 11] = [
    DB_FILE,
    JOURNAL_DIR,
    "index",
    "backups",
    "broken",
    "cartridges",
    RESERVE_FILE,
    LOCK_FILE,
    "lock.pid",
    "lock.pid.tmp",
    REBUILD_DIR,
];

/// 取り込みと入れ替えを終えた結果。
#[derive(Debug)]
pub struct Finished {
    /// 新しい根 (古い根の元の名前)。
    pub root: PathBuf,
    /// 改名した古い根。
    pub renamed_old_root: PathBuf,
    /// 照合の結果。止まった入れ替えを続けただけのときは `None` (前の取り込みが照合を済ませている)。
    pub report: Option<ImportReport>,
    /// 前の取り込みの作りかけ (か、古い根を取り込みの後に使った取り込み) を消してから、やり直した。
    pub redone: bool,
    /// 危ない置き場所の警告 (警告だけにしたもの)。
    pub location_warnings: Vec<String>,
    /// 新しい根 (`root`) のロック。新しい根で続ける呼び手は、そのまま渡す。
    pub new_root_lock: DataRootLock,
    /// 改名した古い根のロック。
    pub old_root_lock: DataRootLock,
}

/// 通常の起動の前の、止まった取り込みの扱い ([`resume_on_startup`])。
#[derive(Debug)]
#[non_exhaustive]
pub enum Resumed {
    /// 止まった取り込みは無い。
    Nothing,
    /// 作りかけの新しい根を消した (古い根はそのまま)。
    RemovedUnfinished,
    /// 照合を終えた新しい根を消した (取り込みの後に古い根が変わった。古い根は元の名前に戻した)。
    RemovedStale,
    /// 止まった入れ替えを続けて終えた。
    Swapped {
        renamed_old_root: PathBuf,
        /// 新しい根 (元の名前) のロック。
        new_root_lock: DataRootLock,
    },
}

/// `import.source` (03 §3.1.1) のうち、入れ替えに使うもの。
#[derive(Debug, Clone)]
struct SourceRecord {
    data_root_id: Option<String>,
    /// 記録した、改名の後の古い根 (表示だけに使う。続きの手順は [`renamed_of`] で求める)。
    path: PathBuf,
    journal_last_seq: u64,
    files: usize,
    files_sha256: String,
}

impl SourceRecord {
    fn parse(s: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        Some(Self {
            data_root_id: v
                .get("data_root_id")
                .and_then(|x| x.as_str())
                .map(str::to_owned),
            path: PathBuf::from(v.get("path")?.as_str()?),
            journal_last_seq: v.get("journal_last_seq")?.as_u64()?,
            files: usize::try_from(v.get("files")?.as_u64()?).ok()?,
            files_sha256: v.get("files_sha256")?.as_str()?.to_owned(),
        })
    }
}

/// 改名の後の古い根: 元の名前 `target` と同じ親の、`import.source` の `path` の名前 (ドライブの文字やマウントの先が
/// 変わっても続けられるように、親は記録から取らない)。名前が `<元の名前>-v0.4-` で始まらなければ、この根のものでない。
fn renamed_of(
    target: &Path,
    new_root: &Path,
    source: &SourceRecord,
) -> Result<PathBuf, ImportError> {
    let foreign = || ImportError::ForeignSource(new_root.to_path_buf());
    let name = source
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(foreign)?;
    let own = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(foreign)?;
    let parent = target.parent().ok_or_else(foreign)?;
    if !name.starts_with(&format!("{own}-v0.4-")) {
        return Err(foreign());
    }
    Ok(parent.join(name))
}

/// 残っている新しい根のフォルダ。
enum Leftover {
    /// 消してよい作りかけ (照合を終えていない)。
    Unfinished(DataRootLock),
    /// 照合を終えている。
    Verified {
        lock: DataRootLock,
        source: SourceRecord,
        /// `import.report` の、照合を終えた時点の `journal.last_seq` (無ければ `None`)。
        verified_seq: Option<u64>,
    },
}

/// 新しい根のフォルダ `dir` を調べる。無ければ `None`。evorag と OS の作らないものがある・取り込みの作った根でない
/// ときは、ロックも取らずに触れない。続ける・消すかもしれないフォルダだけ、ロックを取ってから読み直して決める。
/// `target_exists` は元の名前の根があるか (無ければ、作りかけも消さない)。
fn leftover(dir: &Path, target: &Path) -> Result<Option<Leftover>, ImportError> {
    let Ok(m) = std::fs::symlink_metadata(dir) else {
        return Ok(None);
    };
    if !m.is_dir() || fsutil::is_link_or_reparse_point(&m) {
        return Err(ImportError::Leftover(dir.to_path_buf()));
    }
    let mut checking = false;
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name();
        let name = name.to_string_lossy();
        if name == REBUILD_DIR {
            checking = true;
        }
        if !MADE_BY_EVORAG.contains(&name.as_ref())
            && !name.starts_with("evorag.redb.")
            && !is_os_file(&name)
        {
            return Err(ImportError::Leftover(dir.to_path_buf()));
        }
    }
    let has_db = dir.join(DB_FILE).is_file();
    // ロックを取る前に、読むだけで見分ける (取り込みの作った根でなければ、ロックのファイルも作らない)。読めない
    // (ほかが開いている・壊れている) ときは、ロックを取ってから決める。
    if has_db
        && let Ok(m) = read_meta(dir)
        && m.get(meta::FORMAT_VERSION)
            .is_some_and(|v| v.as_str() != FORMAT_V05)
    {
        return Err(ImportError::Leftover(dir.to_path_buf()));
    }
    let lock = DataRootLock::acquire(dir)?;
    let unfinished = |lock| {
        if std::fs::symlink_metadata(target).is_ok() {
            Ok(Some(Leftover::Unfinished(lock)))
        } else {
            Err(ImportError::OriginalMissing {
                root: target.to_path_buf(),
                new_root: dir.to_path_buf(),
            })
        }
    };
    if !has_db {
        // 書き始める前に止まった (DB の作られる前)。
        return unfinished(lock);
    }
    let m = read_meta(dir).map_err(|_| ImportError::DamagedLeftover(dir.to_path_buf()))?;
    if m.get(meta::FORMAT_VERSION).map(String::as_str) != Some(FORMAT_V05) {
        return Err(ImportError::Leftover(dir.to_path_buf()));
    }
    let Some(source) = m
        .get(meta::IMPORT_SOURCE)
        .and_then(|s| SourceRecord::parse(s))
    else {
        return Err(ImportError::Leftover(dir.to_path_buf()));
    };
    let Some(report) = m.get(meta::IMPORT_REPORT) else {
        // 照合を終えていない作りかけ。
        return unfinished(lock);
    };
    if checking || !dir.join(JOURNAL_DIR).is_dir() {
        return Err(ImportError::DamagedLeftover(dir.to_path_buf()));
    }
    let verified_seq = serde_json::from_str::<serde_json::Value>(report)
        .ok()
        .and_then(|v| v.get("journal_last_seq")?.as_u64());
    Ok(Some(Leftover::Verified {
        lock,
        source,
        verified_seq,
    }))
}

/// ロック `lock` を持っているフォルダ `dir` を消す。DB を最初に消し (途中で止まっても、照合を終えた根に見える
/// 残りを作らないため)、ロックのファイルのほかを消してからロックを放し、残りを消す (消している間に、ほかのプロセスが
/// 開かないように)。
pub(super) fn remove_locked(lock: DataRootLock, dir: &Path) -> Result<(), ImportError> {
    let cannot = |error| ImportError::CannotRemove {
        left: dir.to_path_buf(),
        error,
    };
    let remove = |path: &Path, is_dir: bool| {
        fsutil::retry_transient(|| {
            let r = if is_dir {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            match r {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                r => r,
            }
        })
    };
    remove(&dir.join(DB_FILE), false).map_err(cannot)?;
    let entries = std::fs::read_dir(dir).map_err(cannot)?;
    for e in entries {
        let e = e.map_err(cannot)?;
        if e.file_name() == LOCK_FILE {
            continue;
        }
        let is_dir = e
            .file_type()
            .map(|t| t.is_dir() && !t.is_symlink())
            .map_err(cannot)?;
        remove(&e.path(), is_dir).map_err(cannot)?;
    }
    drop(lock);
    remove_dir_retry(dir).map_err(cannot)
}

/// 古い根の今の場所。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OldAt {
    /// 元の名前 (1 つ目の改名の前)。
    Original,
    /// 改名の後の名前 (2 つの改名の間)。
    Renamed,
}

/// 照合を終えた新しい根の組になる古い根を探して、ロックを取る。
fn locate_old(
    target: &Path,
    renamed: &Path,
    new_root: &Path,
    source: &SourceRecord,
) -> Result<(OldAt, DataRootLock), ImportError> {
    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    match (exists(target), exists(renamed)) {
        (true, false) => {
            let lock = DataRootLock::acquire(target)?;
            let id = read_meta(target)
                .map_err(|_| ImportError::OriginalUnreadable {
                    root: target.to_path_buf(),
                })?
                .get(meta::DATA_ROOT_ID)
                .cloned();
            // 元の名前の根が、取り込んだ古い根でなければ (ID が違う) 組にならない。
            if id.is_none() || id != source.data_root_id {
                return Err(ImportError::OtherRootAtOriginal {
                    root: target.to_path_buf(),
                    new_root: new_root.to_path_buf(),
                });
            }
            Ok((OldAt::Original, lock))
        }
        (false, true) => Ok((OldAt::Renamed, DataRootLock::acquire(renamed)?)),
        (false, false) => Err(ImportError::OldRootMissing(renamed.to_path_buf())),
        (true, true) => Err(ImportError::BothRoots {
            root: target.to_path_buf(),
            renamed: renamed.to_path_buf(),
        }),
    }
}

/// 改名の前の比べ (03 §12.4): ロックを持っている古い根 `old` が、取り込んだときから変わっていないか。
fn unchanged(source: &SourceRecord, old: &Path) -> Result<bool, ImportError> {
    let m = read_meta(old)?;
    let last_seq: u64 = m
        .get(meta::JOURNAL_LAST_SEQ)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if m.get(meta::DATA_ROOT_ID) != source.data_root_id.as_ref()
        || last_seq != source.journal_last_seq
    {
        return Ok(false);
    }
    let files = FileSnapshot::take(old)?;
    Ok(files.len() == source.files && files.digest() == source.files_sha256)
}

/// 照合を終えた新しい根 `new_root` を消してよいか: 照合の後に書いていない (`journal.last_seq` が照合の時点のまま)。
fn ensure_unused(new_root: &Path, verified_seq: Option<u64>) -> Result<(), ImportError> {
    let now: Option<u64> = read_meta(new_root)?
        .get(meta::JOURNAL_LAST_SEQ)
        .and_then(|s| s.parse().ok());
    match (verified_seq, now) {
        (Some(v), Some(n)) if n <= v => Ok(()),
        (Some(_), None) => Ok(()),
        _ => Err(ImportError::NewRootUsed {
            new_root: new_root.to_path_buf(),
        }),
    }
}

/// 改名の隙の規則で改名する ([`rename_locked_root_with`]。試験の関数を渡す)。
fn rename(
    lock: DataRootLock,
    from: &Path,
    to: &Path,
    hooks: &SwapHooks,
) -> Result<DataRootLock, ImportError> {
    let mut before = |p: &Path| {
        if let Some(f) = hooks.before_rename {
            f(p);
        }
    };
    let mut after = |p: &Path| {
        if let Some(f) = hooks.after_rename {
            f(p);
        }
    };
    let mut h = RenameHooks {
        before: Some(&mut before),
        between: Some(&mut after),
        ..RenameHooks::default()
    };
    if let Some(p) = hooks.patience {
        h.patience = p;
    }
    let done = rename_locked_root_with(lock, from, to, h)?;
    if done.sync_error.is_some() {
        // パスは利用者の名前を含みうるので、ログには出さない (NFR-502)。
        tracing::warn!("a data root was renamed, but its parent folder could not be flushed");
    }
    Ok(done.lock)
}

/// 入れ替えの残りを行う (`at` から)。`compared` は、呼び手がこのロックの下で比べを済ませたか。
///
/// 1 つ目の改名の前には全部を比べる (済ませていなければ)。2 つ目の改名の前には、Unix (ロックを持ったまま改名する)
/// では 1 つ目の改名の前に取った要約 (`meta` とファイルの一覧) と比べるだけにし、Windows (ロックを放して改名する)
/// では全部を比べ直す (元の名前のフォルダが無い時間を短くするため。03 §12.4 の「改名の前の比べ」)。合わなければ
/// [`ImportError::Mismatch`] で止める (次の `import-v04` か次の起動がやり直す)。返すのは (新しい根のロック、古い根の
/// ロック)。
#[expect(
    clippy::too_many_arguments,
    reason = "the swap needs both roots, both locks and the comparison state"
)]
fn complete(
    at: OldAt,
    target: &Path,
    renamed: &Path,
    new_root: &Path,
    source: &SourceRecord,
    compared: bool,
    old_lock: DataRootLock,
    new_lock: DataRootLock,
    hooks: &SwapHooks,
) -> Result<(DataRootLock, DataRootLock), ImportError> {
    let changed =
        || ImportError::Mismatch(vec!["the old data root changed before the swap".into()]);
    let old_lock = match at {
        OldAt::Original => {
            if !compared && !unchanged(source, target)? {
                return Err(changed());
            }
            let light = (Gap::NATIVE == Gap::KeepLock)
                .then(|| RootSummary::take(target))
                .transpose()?;
            let old_lock = rename(old_lock, target, renamed, hooks)?;
            if let Some(f) = hooks.between_renames {
                f(renamed, new_root);
            }
            let same = match &light {
                Some(before) => RootSummary::take(renamed)? == *before,
                None => unchanged(source, renamed)?,
            };
            if !same {
                return Err(changed());
            }
            old_lock
        }
        OldAt::Renamed => {
            if !compared && !unchanged(source, renamed)? {
                return Err(changed());
            }
            old_lock
        }
    };
    let new_lock = rename(new_lock, new_root, target, hooks)?;
    Ok((new_lock, old_lock))
}

/// 新しい根の `import.source` を読む (ロックを持っていること)。
fn source_of(new_root: &Path) -> Result<SourceRecord, ImportError> {
    read_meta(new_root)?
        .get(meta::IMPORT_SOURCE)
        .and_then(|s| SourceRecord::parse(s))
        .ok_or_else(|| ImportError::Leftover(new_root.to_path_buf()))
}

/// 取り込んで照合した根 `imported` を入れ替える。
fn swap_imported(
    target: &Path,
    imported: Imported,
    redone: bool,
    hooks: &SwapHooks,
) -> Result<Finished, ImportError> {
    let Imported {
        new_root,
        renamed_old_root,
        report,
        location_warnings,
        old_root_lock,
        new_root_lock,
    } = imported;
    let source = source_of(&new_root)?;
    let renamed = renamed_of(target, &new_root, &source)?;
    debug_assert_eq!(renamed, renamed_old_root);
    let (new_root_lock, old_root_lock) = complete(
        OldAt::Original,
        target,
        &renamed,
        &new_root,
        &source,
        false,
        old_root_lock,
        new_root_lock,
        hooks,
    )?;
    Ok(Finished {
        root: target.to_path_buf(),
        renamed_old_root: renamed,
        report: Some(report),
        redone,
        location_warnings,
        new_root_lock,
        old_root_lock,
    })
}

/// 取り込み元の元の名前と、その `.v05-import`。どちらも、触れる前に置き場所を確かめる。
fn places(opts: &ImportOptions) -> Result<(PathBuf, PathBuf), ImportError> {
    let target = std::path::absolute(&opts.old_root)?;
    let new_root = import_dir(&target).ok_or_else(|| {
        ImportError::from(evorag_store::import_v04::Refusal::NotDataRoot {
            path: target.clone(),
        })
    })?;
    Ok((target, new_root))
}

/// v0.4 の根 `opts.old_root` を新しい根へ取り込み、照合が通ったら入れ替える (03 §12.4)。前の取り込みが止まって
/// いれば、続けるか、片付けてからやり直す (このモジュールの表)。
///
/// `confirm` は、何かを変える前に 1 度だけ呼ぶ ([`Plan`])。取り込みから始めるときは、取り込める根と確かめた後に呼ぶ。
///
/// # Errors
/// [`ImportError`] の通り ([`ImportError::exit_code`] が終了コード)。どの失敗でも古い根は消さず、古い根の中身も
/// 変えない。改名の途中で止まったときは、次の `import-v04` と次の起動が続ける。
pub fn import_and_swap(
    opts: &ImportOptions,
    confirm: &mut Confirm<'_>,
) -> Result<Finished, ImportError> {
    let (target, new_root) = places(opts)?;
    let hooks = opts.swap_hooks;
    let location_warnings = check_locations(opts, [&target, &new_root])?;
    let started = opts.clock.now().as_millis();
    let fresh = |confirm: &mut Confirm<'_>, lock: Option<DataRootLock>| {
        let redone = lock.is_some();
        let imported = run_inner(opts, confirm, lock, started)?;
        swap_imported(&target, imported, redone, &hooks)
    };
    let (lock, source, verified_seq) = match leftover(&new_root, &target)? {
        None => return fresh(confirm, None),
        Some(Leftover::Unfinished(lock)) => return fresh(confirm, Some(lock)),
        Some(Leftover::Verified {
            lock,
            source,
            verified_seq,
        }) => (lock, source, verified_seq),
    };
    let renamed = renamed_of(&target, &new_root, &source)?;
    let (at, old_lock) = locate_old(&target, &renamed, &new_root, &source)?;
    let old_path = match at {
        OldAt::Original => target.clone(),
        OldAt::Renamed => renamed.clone(),
    };
    if unchanged(&source, &old_path)? {
        let plan = Plan::Continue {
            old_root: &old_path,
            root: &target,
            renamed_old_root: &renamed,
            new_root: &new_root,
        };
        if !confirm(&plan) {
            return Err(ImportError::Declined);
        }
        let (new_root_lock, old_root_lock) = complete(
            at, &target, &renamed, &new_root, &source, true, old_lock, lock, &hooks,
        )?;
        return Ok(Finished {
            root: target,
            renamed_old_root: renamed,
            report: None,
            redone: false,
            location_warnings,
            new_root_lock,
            old_root_lock,
        });
    }
    // 取り込みの後に古い根を v0.4 で使った: 新しい根を消して作り直す (照合の後に新しい根へ書いていれば止める)。
    ensure_unused(&new_root, verified_seq)?;
    match at {
        OldAt::Original => {
            drop(old_lock);
            fresh(confirm, Some(lock))
        }
        OldAt::Renamed => {
            let again = renamed_old_root(&target, started).ok_or_else(|| {
                ImportError::from(evorag_store::import_v04::Refusal::NotDataRoot {
                    path: target.clone(),
                })
            })?;
            let plan = Plan::Import {
                old_root: &old_path,
                root: &target,
                renamed_old_root: &again,
                new_root: &new_root,
                redo: true,
            };
            if !confirm(&plan) {
                return Err(ImportError::Declined);
            }
            // 古い根を元の名前に戻してから (1 つ目の改名を元に戻す)、取り込みからやり直す。
            drop(rename(old_lock, &renamed, &target, &hooks)?);
            remove_locked(lock, &new_root)?;
            fresh(&mut |_: &Plan<'_>| true, None)
        }
    }
}

/// 通常の起動 (`evorag` の TUI と `evorag sleep`) の前に、データ根 `data_root` の止まった取り込みを扱う (03 §12.4 の
/// 「止まった後の扱い」)。照合を終えた新しい根があれば入れ替えを続け、作りかけは消す。取り込みの後に古い根が変わって
/// いれば (v0.4 で使った)、新しい根を消し、古い根を元の名前に戻す (取り込み直すかは、次に尋ねる)。尋ねない。
///
/// 新しい根のフォルダが取り込みの作ったものでないときは、元の名前の根があれば何もしない ([`Resumed::Nothing`])。
///
/// # Errors
/// ロックが取れない ([`ImportError::Locked`])、元の名前の根が無い・ほかの根がある、改名・削除ができない、など。
pub fn resume_on_startup(data_root: &Path) -> Result<Resumed, ImportError> {
    let target = std::path::absolute(data_root)?;
    let Some(new_root) = import_dir(&target) else {
        return Ok(Resumed::Nothing);
    };
    let hooks = SwapHooks::default();
    let found = match leftover(&new_root, &target) {
        Err(ImportError::Leftover(_)) if target.exists() => return Ok(Resumed::Nothing),
        r => r?,
    };
    match found {
        None => Ok(Resumed::Nothing),
        Some(Leftover::Unfinished(lock)) => {
            remove_locked(lock, &new_root)?;
            Ok(Resumed::RemovedUnfinished)
        }
        Some(Leftover::Verified {
            lock,
            source,
            verified_seq,
        }) => {
            let renamed = renamed_of(&target, &new_root, &source)?;
            let (at, old_lock) = locate_old(&target, &renamed, &new_root, &source)?;
            let old_path = match at {
                OldAt::Original => target.clone(),
                OldAt::Renamed => renamed.clone(),
            };
            if unchanged(&source, &old_path)? {
                let (new_root_lock, _) = complete(
                    at, &target, &renamed, &new_root, &source, true, old_lock, lock, &hooks,
                )?;
                return Ok(Resumed::Swapped {
                    renamed_old_root: renamed,
                    new_root_lock,
                });
            }
            ensure_unused(&new_root, verified_seq)?;
            if at == OldAt::Renamed {
                drop(rename(old_lock, &renamed, &target, &hooks)?);
            } else {
                drop(old_lock);
            }
            remove_locked(lock, &new_root)?;
            Ok(Resumed::RemovedStale)
        }
    }
}
