//! 退避 (03 §7)。DB の一貫した写しを、論理的な写しで作る。
//!
//! 読み取りのトランザクションを 1 つ開き、すべての表を新しい redb のファイルへ写す。一時名に
//! 書いて書き切り、rename する。ファイルのバイト列をそのまま写さない ([OQ-15])。
//!
//! この段階では、移行・作り直し・復元の前の退避 (§7.2) だけを持つ。日次の退避と世代の管理
//! (§7.1) はこの上に載せる。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use evorag_types::Id;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata};

use crate::fsutil;
use crate::key::{self, OwnedPart};
use crate::meta;
use crate::store::{StoreError, TABLES, db};

/// 退避と書き出しに写さない種類 (03 §7.1・§11)。v0.5 の単位 (`unit`) は台帳と指示から導き直せる派生で、忘れた値の
/// 単位を退避に残さないため。restore の後は導出の指紋が無いので、全件を導き直して作る。
pub const UNCOPIED_KINDS: &[&str] = &["unit"];

/// 退避に写さない `meta` のキー (03 §7.1。単位と組の導出の指紋)。
pub const UNCOPIED_META: &[&str] = &[meta::DERIVE_FINGERPRINT];

/// 写さない種類の記録と、その記録の関係・副索引・`blobs`・隔離の記録を見分ける ([`UNCOPIED_KINDS`])。表は
/// [`TABLES`] の順 (`records` が `links`・`lookup`・`blobs` より前) に写すので、`records` で見つけた ID を後の表で
/// 使う。
#[derive(Debug)]
struct Uncopied {
    ids: BTreeSet<Id>,
    /// 写さない種類の、キーの接頭辞 (`records` と `lookup` のキーは種類の名前で始まる)。
    prefixes: Vec<Vec<u8>>,
    /// 隔離の記録のキーの接頭辞。
    quarantine: Vec<u8>,
}

fn kind_prefix(kind: &str) -> Vec<u8> {
    let mut k = key::KeyBuf::new();
    k.str(kind);
    k.into_bytes()
}

/// 隔離の記録 (`quarantine`) の、元の記録の種類 (本体の `kind`)。
fn quarantined_kind(k: &[u8], v: &[u8]) -> Option<String> {
    let env = crate::envelope::Envelope::decode(k, v).ok()?;
    let ciborium::Value::Map(map) = &env.body else {
        return None;
    };
    map.iter()
        .find(|(k, _)| k.as_text() == Some("kind"))
        .and_then(|(_, v)| v.as_text())
        .map(str::to_owned)
}

impl Uncopied {
    fn new() -> Self {
        Self {
            ids: BTreeSet::new(),
            prefixes: UNCOPIED_KINDS.iter().map(|k| kind_prefix(k)).collect(),
            quarantine: kind_prefix(crate::store::KIND_QUARANTINE),
        }
    }

    fn uncopied_kind(&self, k: &[u8]) -> bool {
        self.prefixes.iter().any(|p| k.starts_with(p))
    }

    fn drops(&mut self, table: Table, k: &[u8], v: &[u8]) -> bool {
        let parts = || key::decode(k).ok();
        match table {
            Table::Meta => UNCOPIED_META.iter().any(|m| m.as_bytes() == k),
            Table::Records => {
                let gone = self.uncopied_kind(k)
                    || (k.starts_with(&self.quarantine)
                        && quarantined_kind(k, v)
                            .is_some_and(|kind| UNCOPIED_KINDS.contains(&kind.as_str())));
                if gone && let Some((_, id)) = crate::store::split_record_key(k) {
                    self.ids.insert(id);
                }
                gone
            }
            Table::Links => {
                !self.ids.is_empty()
                    && parts().is_some_and(|p| {
                        p.iter()
                            .any(|x| matches!(x, OwnedPart::Id(id) if self.ids.contains(id)))
                    })
            }
            // 副索引のキーは、持ち主の記録の種類で始まる。
            Table::Lookup => self.uncopied_kind(k),
            Table::Blobs => {
                !self.ids.is_empty()
                    && parts().is_some_and(
                        |p| matches!(p.first(), Some(OwnedPart::Id(id)) if self.ids.contains(id)),
                    )
            }
        }
    }
}

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

/// 写す行の扱い ([`logical_copy_rows`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// そのまま写す。
    Keep,
    /// 写さない。
    Drop,
    /// 値をこのバイト列にして写す (記録を書き換えて写すとき。封筒のチェックサムは呼び手が計算し直す)。
    Replace(Vec<u8>),
}

/// 写す行の表 ([`TABLES`] と同じ順)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Table {
    Meta,
    Records,
    Links,
    Lookup,
    Blobs,
}

/// 行ごとの扱いを決める関数 ([`logical_copy_rows`])。引数は表、キー、値。
pub(crate) type RowFn<'a> = dyn FnMut(Table, &[u8], &[u8]) -> Result<Row, StoreError> + 'a;

const TABLE_KINDS: [Table; TABLES.len()] = [
    Table::Meta,
    Table::Records,
    Table::Links,
    Table::Lookup,
    Table::Blobs,
];

/// `src` の一貫した写しを `dst` に作る (`dst` は無いこと)。一時名に書いて書き切ってから入れ替える。
///
/// どの写しも、[`UNCOPIED_KINDS`] の記録 (とその関係・副索引・`blobs`) と [`UNCOPIED_META`] の `meta` の値を
/// 写さない (03 §7.1 の「v0.5 の根の退避」。忘れるの写し直しも同じ、§7.3)。
pub fn logical_copy(src: &Database, dst: &Path) -> Result<CopyStats, StoreError> {
    logical_copy_excluding(src, dst, &|_| false)
}

/// `exclude` が真を返すキーの行を除いて写す (削除した記録を退避から除く、03 §7.3)。
pub fn logical_copy_excluding(
    src: &Database,
    dst: &Path,
    exclude: &dyn Fn(&[u8]) -> bool,
) -> Result<CopyStats, StoreError> {
    logical_copy_rows(src, dst, &mut |_, k, _| {
        Ok(if exclude(k) { Row::Drop } else { Row::Keep })
    })
}

/// 行ごとに `row` で扱いを決めて写す (忘れるの写し直し、03 §7.3)。
pub(crate) fn logical_copy_rows(
    src: &Database,
    dst: &Path,
    row: &mut RowFn<'_>,
) -> Result<CopyStats, StoreError> {
    let tmp = dst.with_extension("partial");
    let _ = std::fs::remove_file(&tmp);
    fsutil::prepare_private_file(&tmp).map_err(|e| StoreError::Io(e.to_string()))?;
    let stats = match copy_into(src, &tmp, row) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    fsutil::rename_durable(&tmp, dst).map_err(|e| StoreError::Io(e.to_string()))?;
    Ok(stats)
}

fn copy_into(src: &Database, path: &Path, row: &mut RowFn<'_>) -> Result<CopyStats, StoreError> {
    let out = Database::create(path).map_err(db)?;
    let rtx = src.begin_read().map_err(db)?;
    let mut stats = CopyStats::default();
    let mut uncopied = Uncopied::new();
    let mut tx = out.begin_write().map_err(db)?;
    tx.set_durability(redb::Durability::None).map_err(db)?;
    for t in TABLES {
        tx.open_table(t).map_err(db)?;
    }
    let mut in_batch = 0;
    for (i, t) in TABLES.iter().enumerate() {
        let kind = TABLE_KINDS[i];
        let table = rtx.open_table(*t).map_err(db)?;
        for item in table.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let (k, v) = (k.value(), v.value());
            if uncopied.drops(kind, k, v) {
                continue;
            }
            let replaced = match row(kind, k, v)? {
                Row::Drop => continue,
                Row::Keep => None,
                Row::Replace(bytes) => Some(bytes),
            };
            tx.open_table(*t)
                .map_err(db)?
                .insert(k, replaced.as_deref().unwrap_or(v))
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
    prune(&base, PRE_OP_KEEP, &dir, root_id_of_db(src).as_deref());
    Ok(dir)
}

/// DB の `meta` の `data_root.id`。読めなければ `None`。
fn root_id_of_db(db: &Database) -> Option<String> {
    crate::store::read_meta_db_any(db)
        .ok()?
        .get(crate::meta::DATA_ROOT_ID)
        .cloned()
}

/// 退避 `dir` (中に `evorag.redb`) の `meta` の `data_root.id`。読むだけで開く。開けない・無いときは `None`
/// (ID の分からない退避、03 §7.1 の「退避と根の ID」)。
#[must_use]
pub fn backup_root_id(dir: &Path) -> Option<String> {
    let db = redb::ReadOnlyDatabase::open(dir.join(crate::store::DB_FILE)).ok()?;
    let id = crate::store::read_meta_db_any(&db)
        .ok()?
        .get(crate::meta::DATA_ROOT_ID)
        .cloned();
    let _ = db.close();
    id
}

/// 退避 `dir` が根 `id` のものか。`id` が `None` (根の ID が分からない) なら、どの退避も数える (前の振る舞い)。
fn owned_by(dir: &Path, id: Option<&str>) -> bool {
    id.is_none_or(|id| backup_root_id(dir).as_deref() == Some(id))
}

/// `base` の下に作る退避のフォルダの名前。`name` が空いていればそれ、ほかの根の退避が使っていれば、根の ID の
/// 先頭 8 文字を足した名前 (それも使われていれば番号を足す、03 §7.1 の「退避と根の ID」)。
fn free_name(base: &Path, name: &str, id: Option<&str>) -> PathBuf {
    let plain = base.join(name);
    // 無いか、DB の写しを持たない (作りかけの) フォルダなら、その名前を使う (前の振る舞い)。
    if std::fs::symlink_metadata(&plain).is_err() || !plain.join(crate::store::DB_FILE).is_file() {
        return plain;
    }
    let short: String = id
        .unwrap_or("unknown")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let tagged = format!("{name}-{short}");
    let mut dir = base.join(&tagged);
    let mut n = 1;
    while std::fs::symlink_metadata(&dir).is_ok() {
        dir = base.join(format!("{tagged}-{n}"));
        n += 1;
    }
    dir
}

/// `base` の下で、名前が `name` か `name-` で始まる、根 `id` の退避 (DB の写しを持つもの)。
fn own_backup(base: &Path, name: &str, id: Option<&str>) -> Option<PathBuf> {
    let tagged = format!("{name}-");
    std::fs::read_dir(base)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == name || n.starts_with(&tagged))
        })
        .find(|p| p.join(crate::store::DB_FILE).is_file() && owned_by(p, id))
}

/// 自動の退避のフォルダ (03 §7.1)。
pub const DAILY_DIR: &str = "daily";
pub const WEEKLY_DIR: &str = "weekly";
/// 日次 7 世代と週次 4 世代を残す (ADR-0007)。
pub const DAILY_KEEP: usize = 7;
pub const WEEKLY_KEEP: usize = 4;

/// 1 日 1 回までの自動の退避 (03 §7.1)。今日の分が既にあれば何もしない (`None`)。
///
/// `backups/daily/<YYYYMMDD>/` に一貫した写しを作り、その週の最初の退避なら `backups/weekly/` にも
/// 残す。新しいものから日次 7 世代・週次 4 世代を残す。日付は UTC。
/// その日の、根 `root_id` の日次の退避がまだ無いか (裏の処理が作るかを決める、03 §7.1)。`storage.backup_dir` を
/// ほかの根 (`import-v04` の古い根など) と共有していても、ほかの根の退避は数えない。
pub fn daily_due(backups: &Path, now_ms: i64, root_id: Option<&str>) -> bool {
    let day = stamp(now_ms)[..8].to_owned();
    own_backup(&backups.join(DAILY_DIR), &day, root_id).is_none()
}

pub fn daily(src: &Database, backups: &Path, now_ms: i64) -> Result<Option<PathBuf>, StoreError> {
    let io = |e: std::io::Error| StoreError::Io(e.to_string());
    let id = root_id_of_db(src);
    let id = id.as_deref();
    let day = stamp(now_ms)[..8].to_owned();
    let daily_base = backups.join(DAILY_DIR);
    if own_backup(&daily_base, &day, id).is_some() {
        return Ok(None);
    }
    let dir = free_name(&daily_base, &day, id);
    fsutil::create_private_dir(&dir).map_err(io)?;
    if let Err(e) = logical_copy(src, &dir.join(crate::store::DB_FILE)) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    fsutil::sync_dir(&daily_base).map_err(io)?;
    prune(&daily_base, DAILY_KEEP, &dir, id);

    // 週は月曜から (1970-01-01 は木曜)。名前は週の番号と、その週の最初の退避の日付。
    let week = (now_ms.div_euclid(86_400_000) + 3).div_euclid(7);
    let prefix = format!("w{week:06}-");
    let weekly_base = backups.join(WEEKLY_DIR);
    fsutil::create_private_dir(&weekly_base).map_err(io)?;
    let has_week = std::fs::read_dir(&weekly_base)
        .map_err(io)?
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name().to_string_lossy().starts_with(&prefix)
                && e.path().join(crate::store::DB_FILE).is_file()
                && owned_by(&e.path(), id)
        });
    if !has_week {
        let wdir = free_name(&weekly_base, &format!("{prefix}{day}"), id);
        fsutil::create_private_dir(&wdir).map_err(io)?;
        let to = wdir.join(crate::store::DB_FILE);
        let tmp = to.with_extension("partial");
        let copied = std::fs::copy(dir.join(crate::store::DB_FILE), &tmp)
            .and_then(|_| fsutil::sync_file(&tmp))
            .and_then(|()| fsutil::rename_durable(&tmp, &to));
        if let Err(e) = copied {
            let _ = std::fs::remove_dir_all(&wdir);
            return Err(io(e));
        }
        prune(&weekly_base, WEEKLY_KEEP, &wdir, id);
    }
    Ok(Some(dir))
}

/// `base` の下の、根 `owner` の退避 (DB の写しを持つフォルダ) を、名前の新しい順に `keep` 個だけ残す。
/// 今作った `keep_always` は、時計が戻されて名前が古く並んでも消さない。消せないものは残す
/// (次の機会に消す)。DB の写しを持たないフォルダと、ほかの根の退避・ID の分からない退避は、世代に数えず、消さない
/// (03 §7.1 の「退避と根の ID」)。`owner` が `None` なら、根を見分けずにすべてを数える。
fn prune(base: &Path, keep: usize, keep_always: &Path, owner: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(crate::store::DB_FILE).is_file() && p != keep_always)
        .filter(|p| owned_by(p, owner))
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
        prune(dir.path(), 3, &dir.path().join("a"), None);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["a", "e", "f", "z-empty"]);
    }
}
