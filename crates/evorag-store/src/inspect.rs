//! 読むだけの点検 (03 §10 の `doctor`、FR-207)。何も書かない (AC-FR-207-1)。

use std::path::{Path, PathBuf};

use redb::ReadOnlyDatabase;

use crate::envelope::Envelope;
use crate::journal;
use crate::lock::DataRootLock;
use crate::meta;
use crate::restore::{self, BROKEN_DIR, BackupInfo};
use crate::store::{
    Census, DB_FILE, KindSpec, OpenError, census_of, classify, kind_map, read_meta_db_any,
    split_record_key,
};

/// DB のファイルの状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbState {
    /// まだ無い (新しいデータ根)。
    Missing,
    Opened,
    /// 前回の終わり方のせいで、読むだけでは開けない (次の起動で修復される)。
    NeedsRepair(String),
    Unreadable(String),
}

/// 点検の結果。
#[derive(Debug, Clone)]
pub struct Inspection {
    pub db: DbState,
    pub meta: std::collections::BTreeMap<String, String>,
    pub census: Census,
    pub journal: journal::Summary,
    pub backups: Vec<BackupInfo>,
    /// `broken/` に残っている壊れた DB のファイル。
    pub broken: Vec<PathBuf>,
    /// 読むよう頼まれた種類の、読めた記録 (種類ごと)。
    pub collected: std::collections::BTreeMap<String, Vec<Envelope>>,
}

impl Inspection {
    /// quarantine に移した記録の数。
    pub fn quarantined(&self) -> u64 {
        self.census
            .known
            .get(crate::store::KIND_QUARANTINE)
            .map_or(0, |c| c.ok)
    }
}

/// データ根を、何も書かずに点検する。使っているプロセスがあれば断る (03 §9)。
pub fn inspect(
    root: &Path,
    kinds: Vec<KindSpec>,
    backups: Option<&Path>,
) -> Result<Inspection, OpenError> {
    inspect_collect(root, kinds, backups, &[])
}

/// [`inspect`] に加えて、`collect` の種類の読める記録を集める (`doctor` が中身を見せるため)。
pub fn inspect_collect(
    root: &Path,
    kinds: Vec<KindSpec>,
    backups: Option<&Path>,
    collect: &[&str],
) -> Result<Inspection, OpenError> {
    let _lock = DataRootLock::acquire_quiet(root)?;
    let kinds = kind_map(kinds);
    let file = root.join(DB_FILE);
    let mut out = Inspection {
        db: DbState::Missing,
        meta: Default::default(),
        census: Census::default(),
        journal: journal::Summary::default(),
        backups: restore::list_backups(root, backups),
        broken: Vec::new(),
        collected: Default::default(),
    };
    if file.is_file() {
        match ReadOnlyDatabase::open(&file) {
            Ok(db) => {
                out.db = DbState::Opened;
                out.meta = read_meta_db_any(&db).unwrap_or_default();
                if let Ok(rtx) = redb::ReadableDatabase::begin_read(&db) {
                    out.census = census_of(&rtx, &kinds).unwrap_or_default();
                    out.collected = collect_of(&rtx, collect);
                }
            }
            Err(redb::DatabaseError::RepairAborted) => {
                out.db = DbState::NeedsRepair("repair aborted".into());
            }
            Err(e) => {
                let s = e.to_string();
                out.db = if s.contains("repair") {
                    DbState::NeedsRepair(s)
                } else {
                    DbState::Unreadable(s)
                };
            }
        }
    }
    let last: u64 = out
        .meta
        .get(meta::JOURNAL_LAST_SEQ)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    out.journal = journal::summarize(root, last)?;
    if let Ok(rd) = std::fs::read_dir(root.join(BROKEN_DIR)) {
        for e in rd.filter_map(Result::ok) {
            let f = e.path().join(DB_FILE);
            if f.is_file() {
                out.broken.push(f);
            }
        }
    }
    Ok(out)
}

fn collect_of(
    rtx: &redb::ReadTransaction,
    collect: &[&str],
) -> std::collections::BTreeMap<String, Vec<Envelope>> {
    let mut out = std::collections::BTreeMap::new();
    if collect.is_empty() {
        return out;
    }
    let Ok(table) = rtx.open_table(crate::store::RECORDS) else {
        return out;
    };
    let Ok(iter) = redb::ReadableTable::iter(&table) else {
        return out;
    };
    for (k, v) in iter.filter_map(Result::ok) {
        let Some((kind, id)) = split_record_key(k.value()) else {
            continue;
        };
        if !collect.contains(&kind.as_str()) {
            continue;
        }
        if let crate::store::Read::Ok(env) = classify(&kind, id, k.value(), v.value()) {
            out.entry(kind).or_insert_with(Vec::new).push(env);
        }
    }
    out
}
