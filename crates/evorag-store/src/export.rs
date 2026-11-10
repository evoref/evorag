//! 書き出し (03 §11、FR-209)。何も書き換えずに DB を読み、記録を種類ごとの JSON Lines に写す。
//!
//! - `<出力先>/<kind>.jsonl`。先頭の行は形式名 `evorag-export` と形式の版、以降の行は封筒
//!   (`body` は JSON に写す。JSON にそのまま写せない値は `{"$cbor": …}`)。
//! - 含めないもの: `quarantine` の記録 (と、DB の外にある索引)。読めない記録は数えて飛ばす。
//! - 出力のフォルダとファイルは所有者だけの権限で作る。中身のあるフォルダには書かない。

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTable};

use crate::fsutil;
use crate::journal;
use crate::lock::DataRootLock;
use crate::meta;
use crate::store::{
    DB_FILE, KIND_QUARANTINE, OpenError, RECORDS, Read, classify, read_meta_db_any,
    split_record_key,
};

/// 書き出しの形式名と版。
pub const EXPORT_FORMAT: &str = "evorag-export";
pub const EXPORT_VERSION: u32 = 1;

/// 書き出しの結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// 種類ごとに書いた件数。
    pub kinds: BTreeMap<String, u64>,
    /// 新しい版の形で、この版では読めないので飛ばした記録。
    pub skipped_newer: u64,
    /// 壊れていて飛ばした記録。
    pub skipped_corrupt: u64,
    /// ジャーナルにあって DB にまだ無い行 (evorag を一度起動すると DB に入る)。
    pub pending_journal: usize,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("{0} is not empty")]
    NotEmpty(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("database: {0}")]
    Db(String),
}

fn db<E: std::fmt::Display>(e: E) -> ExportError {
    ExportError::Db(e.to_string())
}

/// 種類の名前を、ファイルの名前に使える形にする (英小文字・数字・`_`・`-` 以外は `%xx`)。
fn file_name(kind: &str) -> String {
    let mut s = String::new();
    for b in kind.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02x}"));
        }
    }
    format!("{s}.jsonl")
}

/// データ根 `root` を `out` に書き出す。使っているプロセスがあれば断る。
pub fn export(
    root: &Path,
    out: &Path,
    evorag_version: &str,
    now_ms: i64,
) -> Result<ExportReport, ExportError> {
    let _lock = DataRootLock::acquire_quiet(root).map_err(OpenError::from)?;
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return Err(ExportError::NotEmpty(out.to_path_buf()));
    }
    fsutil::create_private_dir(out)?;
    let mut report = ExportReport::default();
    let file = root.join(DB_FILE);
    let mut last_seq = 0;
    if file.is_file() {
        let database = ReadOnlyDatabase::open(&file).map_err(db)?;
        last_seq = read_meta_db_any(&database)
            .unwrap_or_default()
            .get(meta::JOURNAL_LAST_SEQ)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let rtx = database.begin_read().map_err(db)?;
        let table = rtx.open_table(RECORDS).map_err(db)?;
        let mut writers: BTreeMap<String, (PathBuf, io::BufWriter<std::fs::File>)> =
            BTreeMap::new();
        for item in table.iter().map_err(db)? {
            let (k, v) = item.map_err(db)?;
            let Some((kind, id)) = split_record_key(k.value()) else {
                report.skipped_corrupt += 1;
                continue;
            };
            if kind == KIND_QUARANTINE {
                continue;
            }
            let env = match classify(&kind, id, k.value(), v.value()) {
                Read::Ok(e) => e,
                Read::Newer { .. } => {
                    report.skipped_newer += 1;
                    continue;
                }
                Read::Corrupt { .. } => {
                    report.skipped_corrupt += 1;
                    continue;
                }
            };
            if !writers.contains_key(&kind) {
                let path = out.join(file_name(&kind));
                fsutil::prepare_private_file(&path)?;
                let mut w = io::BufWriter::new(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&path)?,
                );
                let header = serde_json::json!({
                    "format": EXPORT_FORMAT,
                    "version": EXPORT_VERSION,
                    "kind": kind,
                    "evorag": evorag_version,
                    "exported_at": now_ms,
                });
                writeln!(w, "{header}")?;
                writers.insert(kind.clone(), (path, w));
            }
            let (_, w) = writers.get_mut(&kind).expect("inserted above");
            writeln!(w, "{}", crate::json::envelope_to_json(&env))?;
            *report.kinds.entry(kind).or_default() += 1;
        }
        for (_, (path, w)) in writers {
            let f = w.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()?;
            report.files.push(path);
        }
        fsutil::sync_dir(out)?;
    }
    report.pending_journal = journal::summarize(root, last_seq)?.pending;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_become_safe_file_names() {
        assert_eq!(file_name("turn"), "turn.jsonl");
        assert_eq!(file_name("../x"), "%2e%2e%2fx.jsonl");
    }
}
