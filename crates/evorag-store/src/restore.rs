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

/// 保持している退避の一覧 (新しい順)。`backups` を省くとデータ根の `backups/`。
pub fn list_backups(root: &Path, backups: Option<&Path>) -> Vec<BackupInfo> {
    let base = backups.map_or_else(|| root.join(BACKUPS_DIR), Path::to_path_buf);
    let mut out = Vec::new();
    for (sub, kind) in [
        (DAILY_DIR, BackupKind::Daily),
        (WEEKLY_DIR, BackupKind::Weekly),
        (PRE_OP_DIR, BackupKind::PreOp),
    ] {
        let Ok(rd) = std::fs::read_dir(base.join(sub)) else {
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
pub fn purge_backups(root: &Path, backups: Option<&Path>, ids: &[Id]) -> Result<usize, StoreError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let set: BTreeSet<[u8; 16]> = ids.iter().map(|i| *i.as_bytes()).collect();
    let mut n = 0;
    for b in list_backups(root, backups) {
        let file = b.dir.join(DB_FILE);
        let Ok(db) = Database::open(&file) else {
            continue;
        };
        // 対象を含むかを、記録の表のキーで調べる。
        let hit = {
            let rtx = db.begin_read().map_err(crate::store::db)?;
            let t = rtx
                .open_table(crate::store::RECORDS)
                .map_err(crate::store::db)?;
            let mut hit = false;
            for item in t.iter().map_err(crate::store::db)? {
                let (k, _) = item.map_err(crate::store::db)?;
                if mentions(k.value(), &set) {
                    hit = true;
                    break;
                }
            }
            hit
        };
        if !hit {
            continue;
        }
        let tmp = b.dir.join(format!("{DB_FILE}.purging"));
        backup::logical_copy_excluding(&db, &tmp, &|k| mentions(k, &set))?;
        drop(db);
        fsutil::rename_durable(&tmp, &file).map_err(io)?;
        n += 1;
    }
    Ok(n)
}
