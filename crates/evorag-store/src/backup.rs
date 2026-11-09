//! 退避 (03 §7)。DB の一貫した写しを、論理的な写しで作る。
//!
//! 読み取りのトランザクションを 1 つ開き、すべての表を新しい redb のファイルへ写す。一時名に
//! 書いて書き切り、rename する。ファイルのバイト列をそのまま写さない ([OQ-15])。
//!
//! この段階では、移行・作り直し・復元の前の退避 (§7.2) だけを持つ。日次の退避と世代の管理
//! (§7.1) はこの上に載せる。

use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata};

use crate::fsutil;
use crate::store::{StoreError, TABLES, db};

/// 退避のフォルダ (データ根から)。
pub const BACKUPS_DIR: &str = "backups";
pub const PRE_OP_DIR: &str = "pre-op";
/// 移行・作り直し・復元の前の退避を残す世代 (03 §7.2)。
pub const PRE_OP_KEEP: usize = 3;

/// 写した件数 (表ごと)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyStats {
    pub rows: [u64; TABLES.len()],
}

/// 1 回のコミットで写す行の数。最後に `Immediate` で書き切る。
const BATCH: usize = 10_000;

/// `src` の一貫した写しを `dst` に作る (`dst` は無いこと)。一時名に書いて書き切ってから入れ替える。
pub fn logical_copy(src: &Database, dst: &Path) -> Result<CopyStats, StoreError> {
    let tmp = dst.with_extension("partial");
    let _ = std::fs::remove_file(&tmp);
    fsutil::prepare_private_file(&tmp).map_err(|e| StoreError::Io(e.to_string()))?;
    let stats = match copy_into(src, &tmp) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    fsutil::rename_durable(&tmp, dst).map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(stats)
}

fn copy_into(src: &Database, path: &Path) -> Result<CopyStats, StoreError> {
    let out = Database::create(path).map_err(db)?;
    let rtx = src.begin_read().map_err(db)?;
    let mut stats = CopyStats::default();
    let mut tx = out.begin_write().map_err(db)?;
    tx.set_durability(redb::Durability::None).map_err(db)?;
    for t in TABLES {
        tx.open_table(t).map_err(db)?;
    }
    let mut in_batch = 0;
    for (i, t) in TABLES.iter().enumerate() {
        let table = rtx.open_table(*t).map_err(db)?;
        for item in table.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            tx.open_table(*t)
                .map_err(db)?
                .insert(k.value(), v.value())
                .map_err(db)?;
            stats.rows[i] += 1;
            in_batch += 1;
            if in_batch == BATCH {
                tx.commit().map_err(db)?;
                tx = out.begin_write().map_err(db)?;
                tx.set_durability(redb::Durability::None).map_err(db)?;
                in_batch = 0;
            }
        }
    }
    tx.set_durability(redb::Durability::Immediate).map_err(db)?;
    tx.commit().map_err(db)?;
    // 写しの件数を確かめる。
    let check = out.begin_read().map_err(db)?;
    for (i, t) in TABLES.iter().enumerate() {
        let n = check.open_table(*t).map_err(db)?.len().map_err(db)?;
        if n != stats.rows[i] {
            return Err(StoreError::Io(format!(
                "backup copy has {n} rows in table {i}, expected {}",
                stats.rows[i]
            )));
        }
    }
    Ok(stats)
}

/// UTC のミリ秒を `YYYYMMDDTHHMMSSmmmZ` にする (フォルダの名前が時刻の順に並ぶように)。
pub fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant の civil_from_days。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}{milli:03}Z",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    )
}

/// 移行・作り直し・復元の前の退避を作る (03 §7.2)。中身は DB の写しと、渡されたファイル
/// (`config.toml` など)。新しいものから `PRE_OP_KEEP` 世代を残す。作ったフォルダを返す。
pub fn pre_op(
    src: &Database,
    data_root: &Path,
    now_ms: i64,
    operation: &str,
    format_version: &str,
    extra_files: &[&Path],
) -> Result<PathBuf, StoreError> {
    let backups = data_root.join(BACKUPS_DIR);
    let base = backups.join(PRE_OP_DIR);
    let io = |e: std::io::Error| StoreError::Io(e.to_string());
    let new_backups = !backups.exists();
    fsutil::create_private_dir(&base).map_err(io)?;
    // 同じ名前 (同じミリ秒) の退避を上書きしないように、番号を足す。
    let name = format!("{}-{operation}-v{format_version}", stamp(now_ms));
    let mut dir = base.join(&name);
    let mut n = 1;
    while dir.exists() {
        dir = base.join(format!("{name}-{n}"));
        n += 1;
    }
    fsutil::create_private_dir(&dir).map_err(io)?;
    let made = (|| {
        logical_copy(src, &dir.join(crate::store::DB_FILE))?;
        for f in extra_files {
            if let Some(name) = f.file_name()
                && f.exists()
            {
                let to = dir.join(name);
                fsutil::prepare_private_file(&to).map_err(io)?;
                std::fs::copy(f, &to).map_err(io)?;
                fsutil::sync_file(&to).map_err(io)?;
            }
        }
        // 退避のフォルダの作成を書き切る (電源断で退避のフォルダごと消えないように)。
        fsutil::sync_dir(&base).map_err(io)?;
        if new_backups {
            fsutil::sync_dir(&backups).map_err(io)?;
            fsutil::sync_dir(data_root).map_err(io)?;
        }
        Ok(())
    })();
    if let Err(e) = made {
        // 作りかけの退避を世代に数えないように消す。
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    prune(&base, PRE_OP_KEEP, &dir);
    Ok(dir)
}

/// `base` の下の退避 (DB の写しを持つフォルダ) を、名前の新しい順に `keep` 個だけ残す。
/// 今作った `keep_always` は、時計が戻されて名前が古く並んでも消さない。消せないものは残す
/// (次の機会に消す)。DB の写しを持たないフォルダは、世代に数えず、消さない。
fn prune(base: &Path, keep: usize, keep_always: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(crate::store::DB_FILE).is_file() && p != keep_always)
        .collect();
    dirs.sort();
    let excess = dirs.len().saturating_sub(keep.saturating_sub(1));
    for d in dirs.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_sort_by_time() {
        assert_eq!(stamp(0), "19700101T000000000Z");
        assert_eq!(stamp(1_762_650_000_123), "20251109T010000123Z");
        assert_eq!(stamp(951_782_400_000), "20000229T000000000Z");
        assert!(stamp(1_000) < stamp(10_000));
    }

    #[test]
    fn prune_keeps_the_newest() {
        let dir = crate::testutil::TempDir::new("prune");
        for n in ["a", "b", "c", "d", "e", "f"] {
            std::fs::create_dir(dir.path().join(n)).unwrap();
            std::fs::write(dir.path().join(n).join(crate::store::DB_FILE), b"").unwrap();
        }
        // 写しを持たないフォルダは数えない。
        std::fs::create_dir(dir.path().join("z-empty")).unwrap();
        // 今作った退避 (時計が戻って名前が最も古い) は残す。
        prune(dir.path(), 3, &dir.path().join("a"));
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["a", "e", "f", "z-empty"]);
    }
}
