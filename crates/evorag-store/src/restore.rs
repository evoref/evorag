//! 破損への対策と復元 (03 §7.3、§8、§10)。
//!
//! - 開けない DB のファイルは `broken/<日時>/` へ移す (消さない)。
//! - `restore`: 今の DB を退避してから、退避の写しを据える。次に開いたときに、退避の後の
//!   ジャーナルの行 (削除の行を含む) が取り込まれる。
//! - `purge_backups`: 削除した記録を、保持している退避から除いて写し直す。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use evorag_types::Id;
use redb::{Database, ReadableDatabase, ReadableTable};

use crate::backup::{self, BACKUPS_DIR, DAILY_DIR, PRE_OP_DIR, WEEKLY_DIR};
use crate::fsutil;
use crate::lock::DataRootLock;
use crate::store::{DB_FILE, META, OpenError, StoreError};

pub const BROKEN_DIR: &str = "broken";

/// 退避の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    Daily,
    Weekly,
    PreOp,
}

/// 1 つの退避。
#[derive(Debug, Clone)]
pub struct BackupInfo {
    /// 退避のフォルダ (中に `evorag.redb`)。
    pub dir: PathBuf,
    pub kind: BackupKind,
    pub modified: SystemTime,
    /// 退避に含まれる最後のジャーナルの連番。
    pub journal_seq: Option<u64>,
}

fn io(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

/// 開けない DB のファイルを `broken/<日時>/` へ移す。移した先を返す。
pub fn move_broken(root: &Path, now_ms: i64) -> std::io::Result<PathBuf> {
    let dir = root.join(BROKEN_DIR).join(backup::stamp(now_ms));
    fsutil::create_private_dir(&dir)?;
    let to = dir.join(DB_FILE);
    fsutil::rename_durable(&root.join(DB_FILE), &to)?;
    fsutil::sync_dir(root)?;
    Ok(to)
}

fn journal_seq_of(file: &Path) -> Option<u64> {
    let db = Database::open(file).ok()?;
    let rtx = db.begin_read().ok()?;
    let t = rtx.open_table(META).ok()?;
    let v = t.get(crate::meta::JOURNAL_LAST_SEQ.as_bytes()).ok()??;
    String::from_utf8_lossy(v.value()).parse().ok()
}

/// 退避を置くフォルダ (種類ごと)。日次と週次は `backups` (省くとデータ根の `backups/`) の下。移行と作り直しの
/// 前の退避は、`storage.backup_dir` に関わらずデータ根の `backups/pre-op/` に作る (03 §7.2) ので、そこも必ず
/// 含める (`backups` の下の `pre-op/` も、同じフォルダでなければ含める)。
fn backup_bases(root: &Path, backups: Option<&Path>) -> Vec<(PathBuf, BackupKind)> {
    let base = backups.map_or_else(|| root.join(BACKUPS_DIR), Path::to_path_buf);
    let mut out = vec![
        (base.join(DAILY_DIR), BackupKind::Daily),
        (base.join(WEEKLY_DIR), BackupKind::Weekly),
        (root.join(BACKUPS_DIR).join(PRE_OP_DIR), BackupKind::PreOp),
    ];
    let other = base.join(PRE_OP_DIR);
    let same = |a: &Path, b: &Path| {
        a == b
            || matches!(
                (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                (Ok(x), Ok(y)) if x == y
            )
    };
    if !same(&other, &out[2].0) {
        out.push((other, BackupKind::PreOp));
    }
    out
}

/// 保持している退避の一覧 (新しい順)。`backups` を省くとデータ根の `backups/`。移行と作り直しの前の退避は、
/// データ根の `backups/pre-op/` からも読む ([`backup_bases`])。
pub fn list_backups(root: &Path, backups: Option<&Path>) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    for (dir, kind) in backup_bases(root, backups) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let file = e.path().join(DB_FILE);
            let Ok(meta) = std::fs::metadata(&file) else {
                continue;
            };
            out.push(BackupInfo {
                dir: e.path(),
                kind,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                journal_seq: journal_seq_of(&file),
            });
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.dir.cmp(&a.dir)));
    out
}

/// 退避のフォルダに残った作りかけのファイル (`*.partial`、整理の `*.purging`) か。
fn is_leftover(name: &str) -> bool {
    name != DB_FILE
        && Path::new(name)
            .extension()
            .is_some_and(|e| e == "partial" || e == "purging")
}

/// 強制終了で残った作りかけの退避を消す (03 §7.3、04 §7.7 の 5)。作りかけのファイル (`*.partial`・`*.purging`)
/// と、作りかけで止まった退避のフォルダ (DB の写しが無く、作りかけのファイルだけを持つもの) は、世代の数え方にも
/// 整理にも入らないまま、消した記録の本文を持ち続けうるため。利用者が置いたほかのファイルとフォルダには触れない
/// (中へ再帰しない)。移行と作り直しの前の退避は、データ根の `backups/pre-op/` だけを見る (そこにしか作らない)。
/// 退避を作る処理と同じ順序で通すこと (`Store` の退避の鍵の下で呼ぶ)。消せなかったものを返す。
pub fn sweep_leftovers(root: &Path, backups: Option<&Path>) -> Vec<String> {
    let base = backups.map_or_else(|| root.join(BACKUPS_DIR), Path::to_path_buf);
    let mut failed = Vec::new();
    let mut remove = |path: &Path, r: std::io::Result<()>| {
        if let Err(e) = r
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("{}: {e}", path.display()));
        }
    };
    for parent in [
        base.join(DAILY_DIR),
        base.join(WEEKLY_DIR),
        root.join(BACKUPS_DIR).join(PRE_OP_DIR),
    ] {
        let Ok(rd) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let dir = e.path();
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let has_db = match std::fs::metadata(dir.join(DB_FILE)) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                // 調べられないフォルダには触れない。
                Err(_) => continue,
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut leftovers = Vec::new();
            let mut other = false;
            for f in entries.filter_map(Result::ok) {
                let is_file = f.file_type().is_ok_and(|t| t.is_file());
                if is_file && is_leftover(&f.file_name().to_string_lossy()) {
                    leftovers.push(f.path());
                } else if !(has_db && f.file_name() == DB_FILE) {
                    other = true;
                }
            }
            for f in &leftovers {
                remove(f, std::fs::remove_file(f));
            }
            // 作りかけのファイルだけを持っていたフォルダ (DB の写しが無い) は、空にしてから消す。
            if !has_db && !other {
                remove(&dir, std::fs::remove_dir(&dir));
            }
        }
    }
    failed
}

/// 退避 `backup` (フォルダか、`evorag.redb` のファイル) から戻す (03 §10)。
/// 今の DB が開ければ、先に退避する (03 §7.2)。戻した後に `Store::open` すると、退避の後の
/// ジャーナルの行が取り込まれる。データ根を使っているプロセスがあれば断る。
pub fn restore(
    root: &Path,
    backup: &Path,
    now_ms: i64,
    config_file: Option<&Path>,
) -> Result<Option<PathBuf>, OpenError> {
    let src = if backup.is_dir() {
        backup.join(DB_FILE)
    } else {
        backup.to_path_buf()
    };
    // 退避の写しが開けることを先に確かめる。
    Database::open(&src).map_err(|e| OpenError::Unreadable(format!("{}: {e}", src.display())))?;
    let _lock = DataRootLock::acquire(root)?;
    let main = root.join(DB_FILE);
    let mut pre_op = None;
    if main.exists() {
        match Database::open(&main) {
            Ok(db) => {
                let extra: Vec<&Path> = config_file.into_iter().collect();
                let fmt = String::from("current");
                pre_op = Some(
                    backup::pre_op(&db, root, now_ms, "restore", &fmt, &extra)
                        .map_err(|e| OpenError::Unreadable(e.to_string()))?,
                );
            }
            Err(_) => {
                move_broken(root, now_ms)?;
            }
        }
    }
    let tmp = root.join(format!("{DB_FILE}.restoring"));
    fsutil::prepare_private_file(&tmp)?;
    std::fs::copy(&src, &tmp)?;
    fsutil::sync_file(&tmp)?;
    fsutil::rename_durable(&tmp, &main)?;
    Ok(pre_op)
}

/// キーが、消した記録の ID を含むか (ID の要素は `0x04` + 16 バイト、03 §3.3)。
fn mentions(key: &[u8], ids: &BTreeSet<[u8; 16]>) -> bool {
    key.windows(17).any(|w| {
        w[0] == 0x04
            && w[1..]
                .try_into()
                .is_ok_and(|id: [u8; 16]| ids.contains(&id))
    })
}

/// 削除した記録を、保持している退避から除いて写し直す (03 §7.3)。写し直した退避の数を返す。
///
/// 1 つの退避で失敗しても (開けない・読めない・写せない・入れ替えられない)、ほかの退避は写し直してから失敗を
/// 返す (呼び手は job を残して再試行する、04 §7.7 の 5)。データ根を開いているプロセスからは
/// [`crate::Store::purge_backups`] を使う (退避を作る処理と同じ順序で通し、作りかけの退避も片付けるため)。
///
/// # Errors
/// 写し直せなかった退避があるとき。
pub fn purge_backups(root: &Path, backups: Option<&Path>, ids: &[Id]) -> Result<usize, StoreError> {
    purge_backups_until(root, backups, ids, &|| false)
}

/// [`purge_backups`] を、`stop` が真を返したら次の退避に移る前に止めて行う (止めたら失敗を返す)。
///
/// # Errors
/// 写し直せなかった退避があるとき、または止めたとき。
pub fn purge_backups_until(
    root: &Path,
    backups: Option<&Path>,
    ids: &[Id],
    stop: &dyn Fn() -> bool,
) -> Result<usize, StoreError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let set: BTreeSet<[u8; 16]> = ids.iter().map(|i| *i.as_bytes()).collect();
    let mut n = 0;
    let mut failed = Vec::new();
    for b in list_backups(root, backups) {
        if stop() {
            return Err(StoreError::Io(
                "the purge was stopped before it finished".into(),
            ));
        }
        let file = b.dir.join(DB_FILE);
        match purge_one(&file, &set) {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(e) => failed.push(format!("{}: {e}", file.display())),
        }
    }
    if !failed.is_empty() {
        return Err(StoreError::Io(format!(
            "backups could not be purged: {}",
            failed.join("; ")
        )));
    }
    Ok(n)
}

/// 1 つの退避から `set` の記録を除いて写し直す。写し直したら `true` (含んでいなければ `false`)。
fn purge_one(file: &Path, set: &BTreeSet<[u8; 16]>) -> Result<bool, StoreError> {
    let db = Database::open(file).map_err(crate::store::db)?;
    // 対象を含むかを、記録の表のキーで調べる。
    let hit = {
        let rtx = db.begin_read().map_err(crate::store::db)?;
        let t = rtx
            .open_table(crate::store::RECORDS)
            .map_err(crate::store::db)?;
        let mut hit = false;
        for item in t.iter().map_err(crate::store::db)? {
            let (k, _) = item.map_err(crate::store::db)?;
            if mentions(k.value(), set) {
                hit = true;
                break;
            }
        }
        hit
    };
    if !hit {
        return Ok(false);
    }
    let tmp = file.with_file_name(format!("{DB_FILE}.purging"));
    backup::logical_copy_excluding(&db, &tmp, &|k| mentions(k, set))?;
    drop(db);
    fsutil::rename_durable(&tmp, file).map_err(io)?;
    Ok(true)
}
