//! データ根を丸ごと消す (`evorag reset [--old]`、03 §10.2、FR-218)。
//!
//! 今のデータ根を消すことと、`import-v04` の後の古い根の処分 (`--old`) を、同じ手順で行う (ADR-0021 の決定 4)。
//! CLI (`evorag reset`) と、TUI の処分の操作 (07 §3 の `/forget`・`/delete`) が、どちらも [`prepare`] と
//! [`Prepared::run`] を使う。
//!
//! 1. [`prepare`]: 確かめる (何も消さない)。対象がリンク・マウントポイントでない、今の根・設定・状態のフォルダと
//!    重ならない、evorag のデータ根である (`data_root.id` が読め、`--old` では `import.source` の ID と同じ)、
//!    ロックが取れる。`--old` では、取り込んだときから古い根が変わっていない (手順の (0))。前の `reset` が
//!    止まって残した `<名前>.deleting-<日時>` のフォルダも見つけ、続きとして消すものに入れる。
//! 2. [`Prepared::run`]: (1) 対象を `<名前>.deleting-<日時>` に改名する (改名の隙の規則、
//!    [`crate::root_rename::rename_locked_root`])。(2) `storage.backup_dir` の中の、対象の根の退避を消す。
//!    (3) 改名したフォルダの中身を消し (DB を最初に)、フォルダを消す。(4) `--old` では、呼び手の関数で今の根の
//!    `import.old_root` を `disposed:<時刻>` にする (呼び手が DB とジャーナルに書く)。
//!
//! どこで止まっても、次の [`prepare`] が残った `*.deleting-*` を見つけて続きを消す (中身を消すだけなので、何度
//! 当てても同じ)。シンボリックリンク・リパースポイントはたどらない (リンクそのものだけを消す)。

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTable};

use crate::backup::{self, BACKUPS_DIR, DAILY_DIR, PRE_OP_DIR, WEEKLY_DIR};
use crate::envelope::Envelope;
use crate::fsutil;
use crate::import_v04::{FileSnapshot, is_os_file};
use crate::journal::JOURNAL_DIR;
use crate::lock::{DataRootLock, LOCK_FILE, LockError, PID_FILE};
use crate::meta;
use crate::root_rename::{RenameRootError, read_meta, rename_locked_root};
use crate::store::{DB_FILE, RECORDS, RESERVE_FILE, split_record_key};

/// 消しかけのフォルダの名前の中ほど (`<名前>.deleting-<UTC の日時>`、03 §10.2 の手順の (1))。
pub const DELETING_INFIX: &str = ".deleting-";

/// evorag がデータ根の直下に作りうるもの。DB の無い `*.deleting-*` のフォルダは、これと OS の作るファイル
/// ([`is_os_file`]) と `evorag.redb.` で始まる名前 (移行と compact の一時のファイル) だけから成るときに限り、
/// 消しかけとして続きを消す (利用者のフォルダを、名前が似ているだけで消さないため)。
const MADE_BY_EVORAG: [&str; 11] = [
    DB_FILE,
    JOURNAL_DIR,
    "index",
    BACKUPS_DIR,
    "broken",
    "cartridges",
    RESERVE_FILE,
    LOCK_FILE,
    PID_FILE,
    "lock.pid.tmp",
    "import-check",
];

/// `import.source` (03 §3.1.1) のうち、古い根の処分に使うもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSource {
    /// 取り込み元の古い根の `data_root.id`。
    pub data_root_id: String,
    /// 記録した、改名の後の古い根のパス (表示と、[`ImportSource::old_root`] の手がかり)。
    pub path: PathBuf,
    /// 取り込んだときの古い根の `journal.last_seq`。
    pub journal_last_seq: u64,
    /// 取り込んだときの古い根のファイルの数 ([`FileSnapshot::len`])。
    pub files: usize,
    /// 取り込んだときの古い根のファイルの要約 ([`FileSnapshot::digest`])。
    pub files_sha256: String,
}

impl ImportSource {
    /// `import.source` の値 (JSON の文字列) を読む。形が合わなければ `None`。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        Some(Self {
            data_root_id: v.get("data_root_id")?.as_str()?.to_owned(),
            path: PathBuf::from(v.get("path")?.as_str()?),
            journal_last_seq: v.get("journal_last_seq")?.as_u64()?,
            files: usize::try_from(v.get("files")?.as_u64()?).ok()?,
            files_sha256: v.get("files_sha256")?.as_str()?.to_owned(),
        })
    }

    /// 古い根の今の場所。`import-v04` の続きの手順と同じく、今の根 `current_root` の親のフォルダと、記録した
    /// パスの名前から求める (ドライブの文字やマウントの先が変わっても見つかるように、03 §12.4 の「入れ替え」)。
    /// 名前が `<今の根の名前>-v0.4-` で始まらない、またはそこに無くて記録したパスにあれば、記録したパスを使う。
    #[must_use]
    pub fn old_root(&self, current_root: &Path) -> PathBuf {
        let derived = (|| {
            let current = std::path::absolute(current_root).ok()?;
            let own = current.file_name()?.to_str()?;
            let name = self.path.file_name()?.to_str()?;
            name.starts_with(&format!("{own}-v0.4-"))
                .then(|| current.parent().map(|p| p.join(name)))
                .flatten()
        })();
        match derived {
            Some(d) if exists(&d) || !exists(&self.path) => d,
            _ => self.path.clone(),
        }
    }
}

/// 消す対象。
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    /// 今のデータ根 (`evorag reset`)。
    Current(&'a Path),
    /// `import-v04` の後の古い根 (`evorag reset --old`)。`current_root` は今の根 (取り込みで作った根)、`source`
    /// は今の根の `import.source`。呼び手は今の根のロックを持っていること。
    Old {
        current_root: &'a Path,
        source: &'a ImportSource,
    },
}

/// [`prepare`] に渡すもの。
#[derive(Debug, Clone, Copy)]
pub struct ResetOptions<'a> {
    pub target: Target<'a>,
    /// 設定 `storage.backup_dir` (無ければ `None`。退避はデータ根の中だけ)。
    pub backup_dir: Option<&'a Path>,
    /// 消してはいけない場所 (設定のファイル・状態のフォルダなど)。対象がこれらと同じか、その祖先なら断る。
    pub keep: &'a [&'a Path],
    /// 今の時刻 (UTC、ミリ秒)。改名の先の名前と `disposed:<時刻>` に使う。
    pub now_ms: i64,
}

/// [`prepare`] と [`Prepared::run`] が断った・止まった理由。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResetError {
    /// 対象が evorag のデータ根でない (`evorag.redb` が無い・`meta` の `data_root.id` を読めない)。何も変えていない。
    #[error("{} is not an evorag data root", path.display())]
    NotDataRoot { path: PathBuf },
    /// 古い根の `data_root.id` が `import.source` と違う。何も変えていない。
    #[error("{} is not the data root that was imported", path.display())]
    IdMismatch { path: PathBuf },
    /// 対象が `other` と同じか、その祖先か子孫 (`--old` の今の根、設定・状態のフォルダ)。何も変えていない。
    #[error("{} overlaps {}", path.display(), other.display())]
    Overlaps { path: PathBuf, other: PathBuf },
    /// 対象がリンク (シンボリックリンク・ジャンクション)。たどらずに断る。何も変えていない。
    #[error("{} is a link", path.display())]
    Link { path: PathBuf },
    /// 対象がマウントポイント (Unix で、親のフォルダとファイルシステムが違う。改名できない)。何も変えていない。
    #[error("{} is a mount point", path.display())]
    MountPoint { path: PathBuf },
    /// 取り込んだときから古い根が変わっている (取り込みの後に v0.4 で使った、手順の (0))。`lines` は増えた
    /// ジャーナルの行の数。何も変えていない。
    #[error("{} changed after the import ({lines} new journal line(s))", path.display())]
    OldRootChanged { path: PathBuf, lines: u64 },
    /// 今の根の取り込みが途中 (同じ親に `<名前>.v05-import` がある)。`import-v04` で続けるか片付けるまで消さない
    /// (03 §12.4 の (h))。何も変えていない。
    #[error("an import-v04 of {} is unfinished ({} exists)", path.display(), new_root.display())]
    ImportPending { path: PathBuf, new_root: PathBuf },
    /// 古い根の親のフォルダが無い (ドライブが外れているなど)。古い根が無いとはみなさない。何も変えていない。
    #[error("the folder of the old data root {} is not there", path.display())]
    OldRootParentMissing { path: PathBuf },
    /// 対象の中に `storage.backup_dir` があり、ほかの根の退避か ID の分からない退避 (`backup`) を含む。何も変えて
    /// いない。
    #[error("{} holds the backup folder {} with backups of other roots ({})", path.display(), dir.display(), backup.display())]
    BackupDirInside {
        path: PathBuf,
        dir: PathBuf,
        backup: PathBuf,
    },
    /// `*.deleting-*` のフォルダがあるが、消しかけの根に見えない (evorag の作らないものがある・別の根のもの・
    /// DB を読めない)。触れていない。
    #[error("{} does not look like a data root left by a reset", path.display())]
    UnknownLeftover { path: PathBuf },
    /// ほかの evorag がロックを持っている (分かれば PID)。何も変えていない。
    #[error("{} is in use by another evorag process", path.display())]
    Locked { path: PathBuf, pid: Option<u32> },
    /// ロックを取れない (ロックのファイルの I/O の失敗)。
    #[error("failed to lock {}: {error}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        error: LockError,
    },
    /// (1) の改名ができなかった。根は元の名前のまま ([`RenameRootError::NotUndone`] なら残った場所)。
    #[error(transparent)]
    Rename(#[from] RenameRootError),
    /// 古い根を消し終えたが、(4) の `import.old_root` を書けなかった。もう一度行えば、書くところから続く。
    #[error("the old data root was deleted, but it could not be recorded: {0}")]
    Mark(String),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

impl ResetError {
    /// CLI の終了コード (03 §10.2): ロックの理由 (改名の隙にほかのプロセスが開いたときを含む) は 2、ほかは 1。
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Locked { .. } | Self::Lock { .. } => 2,
            Self::Rename(r) if r.is_lock() => 2,
            _ => 1,
        }
    }
}

/// [`Prepared::run`] の (4) で、呼び手が今の根の `import.old_root` を書けなかった理由。
pub type MarkError = Box<dyn std::error::Error + Send + Sync>;

/// [`Prepared::run`] の (4) を行う呼び手の関数 (`disposed:<時刻>` を受け取り、今の根に書く)。
pub type MarkFn<'a> = dyn FnMut(&str) -> Result<(), MarkError> + 'a;

/// 消しかけのフォルダ (前の `reset` が止まって残した `*.deleting-*`)。
#[derive(Debug)]
struct Leftover {
    path: PathBuf,
    /// ロックのファイルがあれば、そのロック。
    lock: Option<DataRootLock>,
    /// DB が残っていれば、その `data_root.id` (その根の外の退避を消すため)。
    id: Option<String>,
    /// 確かめたときのフォルダ ([`Ident`])。
    ident: Ident,
}

/// 確かめたときのフォルダの見分け (Unix はデバイスと i-node)。消す直前に、リンクに差し替えられていないことを
/// 確かめ直す (03 §10.2 の細目)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ident(Option<(u64, u64)>);

impl Ident {
    fn of(m: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self(Some((m.dev(), m.ino())))
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            Self(None)
        }
    }

    /// `path` が、リンクでないフォルダで、確かめたときと同じもの (Unix) か。
    fn still(self, path: &Path) -> io::Result<()> {
        let m = std::fs::symlink_metadata(path)?;
        if fsutil::is_link_or_reparse_point(&m)
            || !m.is_dir()
            || (self.0.is_some() && Ident::of(&m) != self)
        {
            return Err(io::Error::other(
                "the folder was replaced after it was checked; it was not deleted",
            ));
        }
        Ok(())
    }
}

/// 消しかけのフォルダがどの根のものか ([`deleting_leftovers`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeftoverOf {
    /// 今の根の消しかけ (`<名前>.deleting-<日時>`)。続きは `evorag reset` (今の根も一緒に消える)。
    Current,
    /// 古い根の消しかけ (`<名前>-v0.4-<日時>.deleting-<日時>`)。続きは `evorag reset --old`。
    Old,
}

/// 試験だけ: (2) で外の退避を消す代わりの関数 ([`RunHooks::remove_backup`])。
#[doc(hidden)]
pub type RemoveFn<'a> = dyn Fn(&Path) -> io::Result<()> + 'a;

/// 試験だけ: [`Prepared::run_with`] の途中の動きを作る関数。
#[doc(hidden)]
#[derive(Default)]
pub struct RunHooks<'a> {
    /// (2) で外の退避を消す代わりに呼ぶ。
    pub remove_backup: Option<&'a RemoveFn<'a>>,
    /// (3) でフォルダを確かめ直す直前に、そのフォルダを渡して呼ぶ。
    pub before_remove: Option<&'a mut dyn FnMut(&Path)>,
}

/// 確かめ終えた、消すもの ([`prepare`])。対象と消しかけのフォルダのロックを持つ。落とすと何も消さずに放す。
#[derive(Debug)]
pub struct Prepared {
    old: Option<String>,
    target: PathBuf,
    /// 対象があれば、そのロックと `data_root.id`。
    found: Option<Found>,
    leftovers: Vec<Leftover>,
    backup_dir: Option<PathBuf>,
    /// 外の退避を消さない場所 (今の根・設定・状態のフォルダ)。
    protect: Vec<PathBuf>,
    outside: usize,
    inside: usize,
    now_ms: i64,
}

impl Prepared {
    /// 消す対象 (今の根か、古い根の今の場所)。[`Prepared::target_exists`] が偽なら、もう無い。
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// 対象のフォルダがある (偽なら、消しかけのフォルダの続きと外の退避だけを消す)。
    #[must_use]
    pub fn target_exists(&self) -> bool {
        self.found.is_some()
    }

    /// 続きを消す、前の `reset` の消しかけのフォルダ。
    pub fn leftovers(&self) -> impl Iterator<Item = &Path> {
        self.leftovers.iter().map(|l| l.path.as_path())
    }

    /// 対象のフォルダの中の退避 (日次・週次・移行と作り直しの前) の数。
    #[must_use]
    pub fn inside_backups(&self) -> usize {
        self.inside
    }

    /// `storage.backup_dir` の中の、対象の根の退避の数 (退避の `meta` の `data_root.id` が合うもの)。
    #[must_use]
    pub fn outside_backups(&self) -> usize {
        self.outside
    }

    /// 設定 `storage.backup_dir`。
    #[must_use]
    pub fn backup_dir(&self) -> Option<&Path> {
        self.backup_dir.as_deref()
    }

    /// 消す (03 §10.2 の手順の (1)〜(4))。`--old` では、消し残しが無いときだけ、最後に `mark` に
    /// `disposed:<時刻>` を渡して呼ぶ (呼び手が今の根の `import.old_root` に、DB とジャーナルの両方で書く)。
    ///
    /// # Errors
    /// (1) の改名ができない ([`ResetError::Rename`]。ロックの理由なら終了コード 2)、`mark` が失敗した
    /// ([`ResetError::Mark`])。消せないファイルが残ったときは失敗にせず、[`Done::left`] に示す。(2) で外の退避が
    /// 残ったときは (3) を行わず、改名したフォルダを DB ごと残す (次の `reset` が根の ID を読んで続ける)。
    pub fn run(self, mark: &mut MarkFn<'_>) -> Result<Done, ResetError> {
        self.run_with(mark, RunHooks::default())
    }

    /// [`Prepared::run`] の本体。試験が [`RunHooks`] で途中の動きを作る。
    #[doc(hidden)]
    pub fn run_with(
        self,
        mark: &mut MarkFn<'_>,
        mut hooks: RunHooks<'_>,
    ) -> Result<Done, ResetError> {
        let Self {
            old,
            target,
            found,
            leftovers,
            backup_dir,
            protect,
            now_ms,
            ..
        } = self;
        let mut ids = BTreeSet::new();
        let mut folders: Vec<(PathBuf, Option<DataRootLock>, Ident)> = Vec::new();
        if let Some(Found { lock, id, ident }) = found {
            ids.insert(id);
            let to = deleting_path(&target, now_ms)?;
            let done = rename_locked_root(lock, &target, &to)?;
            if done.sync_error.is_some() {
                // パスは利用者の名前を含みうるので、ログには出さない (NFR-502)。
                tracing::warn!(
                    "a data root was renamed, but its parent folder could not be flushed"
                );
            }
            folders.push((to, Some(done.lock), ident));
        }
        if let Some(id) = &old {
            // 古い根がもう無くても (消し終えた後に (4) を書けなかったときなど)、外の退避は消す。
            ids.insert(id.clone());
        }
        for l in leftovers {
            ids.extend(l.id);
            folders.push((l.path, l.lock, l.ident));
        }
        let mut out = Done::default();
        if let Some(dir) = &backup_dir {
            for b in own_backups(dir, &ids, &protect) {
                let removed = match hooks.remove_backup {
                    Some(f) => f(&b),
                    None => remove_dir_retry(&b),
                };
                match removed {
                    Ok(()) => out.backups_removed += 1,
                    Err(e) => out.left.push(format!("{}: {e}", b.display())),
                }
            }
        }
        if !out.left.is_empty() {
            // 外の退避が残った: フォルダを DB ごと残す (次の `reset` が根の ID を読んで、(2) からやり直す)。
            return Ok(out);
        }
        for (dir, lock, ident) in folders {
            if let Some(f) = hooks.before_remove.as_mut() {
                f(&dir);
            }
            match ident.still(&dir).and_then(|()| remove_folder(lock, &dir)) {
                Ok(()) => out.removed.push(dir),
                Err(e) => out.left.push(format!("{}: {e}", dir.display())),
            }
        }
        if old.is_some() && out.left.is_empty() {
            let value = format!("{}{now_ms}", meta::IMPORT_OLD_ROOT_DISPOSED);
            mark(&value).map_err(|e| ResetError::Mark(e.to_string()))?;
            out.disposed = Some(value);
        }
        Ok(out)
    }
}

/// [`Prepared::run`] の結果。
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Done {
    /// 消したフォルダ (改名の後の名前)。
    pub removed: Vec<PathBuf>,
    /// 消した、`storage.backup_dir` の中の退避の数。
    pub backups_removed: usize,
    /// 消せなかったもの (パスと理由)。空でなければ、次の `reset` が続きを消す (終了コード 1)。
    pub left: Vec<String>,
    /// `--old` で、今の根の `import.old_root` に書いた値 (`disposed:<時刻>`)。
    pub disposed: Option<String>,
}

/// 消す前に確かめる (03 §10.2 の「確かめること」と手順の (0))。何も消さない。書くのは、対象のロックのファイル
/// (`lock`・`lock.pid`) だけ (DB の無いフォルダには、ロックのファイルも作らない)。
///
/// 今のデータ根 ([`Target::Current`]) が無くても、前の `reset` の消しかけのフォルダがあれば、その続きを消す
/// ものとして返す。古い根 ([`Target::Old`]) も消しかけのフォルダも無ければ、外の退避を消して処分を記録する
/// だけのものとして返す (消し終えた後に (4) を書けなかったときの続き)。
///
/// # Errors
/// [`ResetError`] の通り (どれも何も変えていない)。
pub fn prepare(opts: &ResetOptions<'_>) -> Result<Prepared, ResetError> {
    let (target, old, current) = match opts.target {
        Target::Current(root) => (std::path::absolute(root)?, None, None),
        Target::Old {
            current_root,
            source,
        } => (
            std::path::absolute(source.old_root(current_root))?,
            Some(source.data_root_id.clone()),
            Some(real(current_root)),
        ),
    };
    // 今の根の取り込みが途中なら消さない (今の根を消すと起動が止まり、会話の写しが `.v05-import` に残る)。
    if old.is_none()
        && let Some(new_root) = crate::import_v04::import_dir(&target)
        && presence(&new_root)?
    {
        return Err(ResetError::ImportPending {
            path: target,
            new_root,
        });
    }
    let keep: Vec<PathBuf> = opts.keep.iter().map(|p| real(p)).collect();
    let mut protect = keep.clone();
    protect.extend(current.clone());
    let found = if presence(&target)? {
        let found = check_target(&target, old.as_deref(), current.as_deref(), &keep)?;
        if let Some(dir) = opts.backup_dir {
            refuse_foreign_backups_inside(&target, dir, &found.id)?;
        }
        Some(found)
    } else {
        // 古い根が無いとみなすのは、親のフォルダがあって、その中に無いと分かったときだけ。
        if old.is_some()
            && let Some(parent) = target.parent()
            && !presence(parent)?
        {
            return Err(ResetError::OldRootParentMissing { path: target });
        }
        None
    };
    // 古い根: 取り込んだときから変わっていないか (手順の (0))。ロックを持ったまま比べる。
    if let (Some(_), Target::Old { source, .. }) = (&found, opts.target) {
        unchanged(&target, source)?;
    }
    let mut leftovers = Vec::new();
    for path in deleting_leftovers_of(&target)? {
        leftovers.push(check_leftover(path, old.as_deref(), &protect)?);
    }
    if found.is_none() && leftovers.is_empty() && old.is_none() {
        return Err(ResetError::NotDataRoot { path: target });
    }
    let mut ids: BTreeSet<String> = leftovers.iter().filter_map(|l| l.id.clone()).collect();
    ids.extend(found.as_ref().map(|f| f.id.clone()));
    ids.extend(old.clone());
    let real_target = real(&target);
    let outside = opts.backup_dir.map_or(0, |dir| {
        own_backups(dir, &ids, &protect)
            .iter()
            .filter(|b| !real(b).starts_with(&real_target))
            .count()
    });
    let inside = if found.is_some() {
        count_backups_in(&target.join(BACKUPS_DIR))
    } else {
        0
    };
    Ok(Prepared {
        old,
        target,
        found,
        leftovers,
        backup_dir: opts.backup_dir.map(Path::to_path_buf),
        protect,
        outside,
        inside,
        now_ms: opts.now_ms,
    })
}

/// データ根 `root` の隣に残っている、`reset` の消しかけのフォルダ (今の根の `<名前>.deleting-<日時>` と、古い根の
/// `<名前>-v0.4-<日時>.deleting-<日時>`)。`evorag doctor` が、どちらの根のものかを分けて示す (続きを消すコマンドが
/// 違うため、03 §10.2)。読むだけ。
#[must_use]
pub fn deleting_leftovers(root: &Path) -> Vec<(PathBuf, LeftoverOf)> {
    let Some((parent, name)) = split(root) else {
        return Vec::new();
    };
    let own = format!("{name}{DELETING_INFIX}");
    let old = format!("{name}-v0.4-");
    let Ok(rd) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, LeftoverOf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_owned();
            let of = if n.strip_prefix(&own).is_some_and(is_stamp_suffix) {
                LeftoverOf::Current
            } else if n.starts_with(&old)
                && n.rsplit_once(DELETING_INFIX)
                    .is_some_and(|(_, s)| is_stamp_suffix(s))
            {
                LeftoverOf::Old
            } else {
                return None;
            };
            Some((e.path(), of))
        })
        .collect();
    out.sort();
    out
}

/// 消しかけのフォルダの名前の後ろ (`<日時>` か `<日時>-<番号>`、日時は [`backup::stamp`] の形) か。
fn is_stamp_suffix(s: &str) -> bool {
    let (stamp, n) = s.split_once('-').map_or((s, None), |(a, b)| (a, Some(b)));
    let b = stamp.as_bytes();
    b.len() == 19
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..18].iter().all(u8::is_ascii_digit)
        && b[18] == b'Z'
        && n.is_none_or(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

/// 根 `root` の種類 `kind` の、読める記録 (読むだけで開く。読めない記録は飛ばす)。呼び手は `root` のロックを持って
/// いること。`evorag reset --old` の確認で、取り込みの後の忘れるの数を数えるために使う。
///
/// # Errors
/// DB を開けない・読めないとき。
pub fn read_records(root: &Path, kind: &str) -> io::Result<Vec<Envelope>> {
    let db = ReadOnlyDatabase::open(root.join(DB_FILE)).map_err(io::Error::other)?;
    let out = (|| {
        let rtx = db.begin_read().map_err(io::Error::other)?;
        let table = rtx.open_table(RECORDS).map_err(io::Error::other)?;
        let mut out = Vec::new();
        for item in table.iter().map_err(io::Error::other)? {
            let (k, v) = item.map_err(io::Error::other)?;
            if split_record_key(k.value()).is_some_and(|(kd, _)| kd == kind)
                && let Ok(env) = Envelope::decode(k.value(), v.value())
            {
                out.push(env);
            }
        }
        Ok(out)
    })();
    let _ = db.close();
    out
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// `p` があるか。無いとみなすのは、調べて「無い」(NotFound) と分かったときだけ (調べられないときは誤り)。
fn presence(p: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// (親のフォルダ, 名前)。名前を持たないパス (ドライブの根など) なら `None`。
fn split(root: &Path) -> Option<(PathBuf, String)> {
    let abs = std::path::absolute(root).ok()?;
    let name = abs.file_name()?.to_str()?.to_owned();
    Some((abs.parent()?.to_path_buf(), name))
}

/// 実際のパス (リンクを解いた形)。無ければ、ある所までを解いて残りを足す (比べるため)。
fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut rest = Vec::new();
    let mut at = abs.as_path();
    while let Some(parent) = at.parent() {
        if let Some(name) = at.file_name() {
            rest.push(name.to_owned());
        }
        if let Ok(r) = std::fs::canonicalize(parent) {
            return rest.iter().rev().fold(r, |acc, n| acc.join(n));
        }
        at = parent;
    }
    abs
}

/// `a` と `b` の一方が他方と同じか、その祖先か。
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// 確かめ終えた対象のフォルダ。
#[derive(Debug)]
struct Found {
    lock: DataRootLock,
    id: String,
    ident: Ident,
}

/// 対象のフォルダの置き場所・重なり・データ根であること・ロックを確かめる。今の根 (`expected_id` が `None`) では
/// `journal/` があることも求める (退避のフォルダを `--data-root` で指したときに、データ根として消さないため)。
fn check_target(
    target: &Path,
    expected_id: Option<&str>,
    current: Option<&Path>,
    keep: &[PathBuf],
) -> Result<Found, ResetError> {
    let m = std::fs::symlink_metadata(target)?;
    if fsutil::is_link_or_reparse_point(&m) {
        return Err(ResetError::Link {
            path: target.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(parent) = target.parent()
            && let Ok(p) = std::fs::metadata(parent)
            && p.dev() != m.dev()
        {
            return Err(ResetError::MountPoint {
                path: target.to_path_buf(),
            });
        }
    }
    let real_target = real(target);
    // 古い根は今の根と、どちらの向きにも重ならない。設定・状態のフォルダは、対象の中に無い。
    let hit = current.filter(|c| overlap(&real_target, c)).or_else(|| {
        keep.iter()
            .map(PathBuf::as_path)
            .find(|p| p.starts_with(&real_target))
    });
    if let Some(other) = hit {
        return Err(ResetError::Overlaps {
            path: target.to_path_buf(),
            other: other.to_path_buf(),
        });
    }
    let not_root = || ResetError::NotDataRoot {
        path: target.to_path_buf(),
    };
    // DB の無いフォルダには、ロックのファイルも作らない。
    if !m.is_dir()
        || !std::fs::symlink_metadata(target.join(DB_FILE)).is_ok_and(|d| d.is_file())
        || (expected_id.is_none()
            && !std::fs::symlink_metadata(target.join(JOURNAL_DIR)).is_ok_and(|d| d.is_dir()))
    {
        return Err(not_root());
    }
    let lock = lock(target)?;
    let id = read_meta(target)
        .map_err(|_| not_root())?
        .get(meta::DATA_ROOT_ID)
        .cloned()
        .ok_or_else(not_root)?;
    if expected_id.is_some_and(|e| e != id) {
        return Err(ResetError::IdMismatch {
            path: target.to_path_buf(),
        });
    }
    Ok(Found {
        lock,
        id,
        ident: Ident::of(&m),
    })
}

/// 対象 `target` の中に退避の置き場所 `dir` があり、根 `id` のほかの退避 (ほかの根のもの・ID の分からないもの) を
/// 含むなら断る (対象を消すと、ほかの根の退避まで消えるため)。
fn refuse_foreign_backups_inside(target: &Path, dir: &Path, id: &str) -> Result<(), ResetError> {
    if !real(dir).starts_with(real(target)) {
        return Ok(());
    }
    for base in [DAILY_DIR, WEEKLY_DIR, PRE_OP_DIR] {
        let Ok(rd) = std::fs::read_dir(dir.join(base)) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let path = e.path();
            if std::fs::symlink_metadata(path.join(DB_FILE)).is_ok_and(|m| m.is_file())
                && backup::backup_root_id(&path).as_deref() != Some(id)
            {
                return Err(ResetError::BackupDirInside {
                    path: target.to_path_buf(),
                    dir: dir.to_path_buf(),
                    backup: path,
                });
            }
        }
    }
    Ok(())
}

fn lock(root: &Path) -> Result<DataRootLock, ResetError> {
    DataRootLock::acquire(root).map_err(|e| lock_error(root, e))
}

fn lock_error(root: &Path, e: LockError) -> ResetError {
    match e {
        LockError::InUse { pid } => ResetError::Locked {
            path: root.to_path_buf(),
            pid,
        },
        error => ResetError::Lock {
            path: root.to_path_buf(),
            error,
        },
    }
}

/// 古い根 `old` (ロックを持っている) が、取り込んだときから変わっていないか (手順の (0)。`import-v04` の改名の
/// 前の比べと同じ取り方、03 §12.4)。
fn unchanged(old: &Path, source: &ImportSource) -> Result<(), ResetError> {
    let m = read_meta(old).map_err(|_| ResetError::NotDataRoot {
        path: old.to_path_buf(),
    })?;
    let seq: u64 = m
        .get(meta::JOURNAL_LAST_SEQ)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let files = FileSnapshot::take(old)?;
    if seq == source.journal_last_seq
        && files.len() == source.files
        && files.digest() == source.files_sha256
    {
        return Ok(());
    }
    Err(ResetError::OldRootChanged {
        path: old.to_path_buf(),
        lines: seq.saturating_sub(source.journal_last_seq),
    })
}

/// 対象 `target` の隣の、`<名前>.deleting-<日時>` (名前の順。日時の形でない名前は、消しかけとして扱わない)。
fn deleting_leftovers_of(target: &Path) -> io::Result<Vec<PathBuf>> {
    let Some((parent, name)) = split(target) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{name}{DELETING_INFIX}");
    let rd = match std::fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e?;
        if e.file_name()
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .is_some_and(is_stamp_suffix)
        {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

/// 消しかけのフォルダ `path` を、続きを消してよいか確かめる。リンクでなく、ほかの evorag が使っておらず、
/// DB があれば (古い根では) 取り込み元の ID と同じ根で、DB が無ければ evorag の作るものだけを持つこと。
fn check_leftover(
    path: PathBuf,
    expected_id: Option<&str>,
    protect: &[PathBuf],
) -> Result<Leftover, ResetError> {
    let unknown = |path: &Path| ResetError::UnknownLeftover {
        path: path.to_path_buf(),
    };
    let m = std::fs::symlink_metadata(&path)?;
    if fsutil::is_link_or_reparse_point(&m) || !m.is_dir() {
        return Err(unknown(&path));
    }
    let real_path = real(&path);
    if let Some(p) = protect.iter().find(|p| p.starts_with(&real_path)) {
        return Err(ResetError::Overlaps {
            path,
            other: p.clone(),
        });
    }
    let lock = DataRootLock::acquire_quiet(&path).map_err(|e| lock_error(&path, e))?;
    let id = if std::fs::symlink_metadata(path.join(DB_FILE)).is_ok() {
        let id = read_meta(&path)
            .ok()
            .and_then(|m| m.get(meta::DATA_ROOT_ID).cloned())
            .ok_or_else(|| unknown(&path))?;
        if expected_id.is_some_and(|e| e != id) {
            return Err(unknown(&path));
        }
        Some(id)
    } else {
        for e in std::fs::read_dir(&path)? {
            let name = e?.file_name();
            let name = name.to_string_lossy();
            if !MADE_BY_EVORAG.contains(&name.as_ref())
                && !name.starts_with("evorag.redb.")
                && !is_os_file(&name)
            {
                return Err(unknown(&path));
            }
        }
        None
    };
    Ok(Leftover {
        path,
        lock,
        id,
        ident: Ident::of(&m),
    })
}

/// 改名の先 `<名前>.deleting-<UTC の日時>` (同じ名前があれば番号を足す)。
fn deleting_path(target: &Path, now_ms: i64) -> io::Result<PathBuf> {
    let (parent, name) = split(target).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no folder name", target.display()),
        )
    })?;
    let base = format!("{name}{DELETING_INFIX}{}", backup::stamp(now_ms));
    let mut to = parent.join(&base);
    let mut n = 1;
    while exists(&to) {
        to = parent.join(format!("{base}-{n}"));
        n += 1;
    }
    Ok(to)
}

/// 退避の置き場所 `dir` の中の、根 `ids` のどれかの退避 (日次・週次・移行と作り直しの前)。リンクはたどらず、
/// データ根に見えるフォルダ (ジャーナルのフォルダかロックのファイルを持つ) と、`protect` そのものかその祖先の
/// フォルダは入れない。ID の分からない退避も入れない (03 §7.1 の「退避と根の ID」。ほかの根の退避と利用者の置いたものに
/// 触れないため)。
fn own_backups(dir: &Path, ids: &BTreeSet<String>, protect: &[PathBuf]) -> Vec<PathBuf> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for base in [DAILY_DIR, WEEKLY_DIR, PRE_OP_DIR] {
        let Ok(rd) = std::fs::read_dir(dir.join(base)) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let path = e.path();
            let Ok(m) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if fsutil::is_link_or_reparse_point(&m)
                || !m.is_dir()
                || !std::fs::symlink_metadata(path.join(DB_FILE)).is_ok_and(|d| d.is_file())
                || exists(&path.join(JOURNAL_DIR))
                || exists(&path.join(LOCK_FILE))
            {
                continue;
            }
            // 今の根・設定・状態のフォルダそのものか、その祖先は消さない (中にある退避は消してよい)。
            let real_path = real(&path);
            if protect.iter().any(|p| p.starts_with(&real_path)) {
                continue;
            }
            if backup::backup_root_id(&path).is_some_and(|id| ids.contains(&id)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// データ根の中の退避の数 (`backups/` の下の、DB の写しを持つフォルダ)。
fn count_backups_in(backups: &Path) -> usize {
    [DAILY_DIR, WEEKLY_DIR, PRE_OP_DIR]
        .iter()
        .filter_map(|b| std::fs::read_dir(backups.join(b)).ok())
        .flat_map(|rd| rd.filter_map(Result::ok))
        .filter(|e| std::fs::symlink_metadata(e.path().join(DB_FILE)).is_ok_and(|m| m.is_file()))
        .count()
}

/// フォルダ `dir` を中身ごと消す (無ければ何もしない)。一時の掴みの間は短く試し直す。リンクはたどらない
/// (std の `remove_dir_all` は、シンボリックリンクとジャンクションを、その先をたどらずにリンクだけ消す)。
fn remove_dir_retry(dir: &Path) -> io::Result<()> {
    fsutil::retry_transient(|| match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// 1 つの項目を消す。リンクは、その先をたどらずにリンクだけを消す (Windows のフォルダへのリンクはフォルダの
/// 削除で消す)。
fn remove_entry(path: &Path) -> io::Result<()> {
    let m = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    fsutil::retry_transient(|| {
        let r = if fsutil::is_link_or_reparse_point(&m) {
            std::fs::remove_file(path).or_else(|e| std::fs::remove_dir(path).map_err(|_| e))
        } else if m.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match r {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    })
}

/// 改名したフォルダ `dir` を消す (手順の (3))。DB を最初に消し (途中で止まっても、開ける根に見える残りを作らない)、
/// ロックのファイルのほかを消してからロックを放し、残りを消す (消している間に、ほかのプロセスが開かないように)。
fn remove_folder(lock: Option<DataRootLock>, dir: &Path) -> io::Result<()> {
    remove_entry(&dir.join(DB_FILE))?;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_name() == LOCK_FILE {
            continue;
        }
        remove_entry(&e.path())?;
    }
    drop(lock);
    remove_dir_retry(dir)?;
    if let Some(parent) = dir.parent() {
        // 書き切れなくても、消したことは変わらない (次の `reset` が見分ける)。
        let _ = fsutil::sync_dir(parent);
    }
    Ok(())
}
