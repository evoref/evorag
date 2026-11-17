//! 書き出し (03 §11、FR-209)。何も書き換えずに DB を読み、記録を種類ごとの JSON Lines に写す。
//!
//! - `<出力先>/<kind>.jsonl`。先頭の行は形式名 `evorag-export` と形式の版、以降の行は封筒
//!   (`body` は JSON に写す。JSON にそのまま写せない値は `{"$cbor": …}`)。
//! - 含めないもの: `quarantine` の記録、呼ぶ側が挙げた作り直せる種類 (点検の結果など、03 §11)、
//!   [`ExportChecks::include`] が外した記録 (出典の無い派生の記録など)、DB の外にある索引。読めない記録は数えて
//!   飛ばす。
//! - 断るとき: 取り込んでいないジャーナルの行があるとき ([`ExportError::PendingJournal`])、当て直しの済んで
//!   いない忘れるがあるとき ([`ExportError::PendingForget`]、[`ExportChecks::pending_forget`])。evorag を一度
//!   起動すれば済む (伏せ字の前の本文を書き出さないため、03 §11)。断るときは出力先に何も作らない。
//! - 出力のフォルダとファイルは所有者だけの権限で作る。中身のあるフォルダには書かない。

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use evorag_types::Id;
use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTable};

use crate::envelope::Envelope;
use crate::fsutil;
use crate::journal;
use crate::key::{self, KeyBuf};
use crate::lock::DataRootLock;
use crate::meta;
use crate::store::{
    DB_FILE, KIND_QUARANTINE, OpenError, RECORDS, Read, classify, read_meta_db_any, record_key,
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
    /// [`ExportChecks::include`] が外した記録 (出典の無い派生の記録など)。
    pub skipped_by_check: u64,
    pub files: Vec<PathBuf>,
}

/// 書き出しの指定。
#[derive(Clone, Copy)]
pub struct ExportOptions<'a> {
    /// 書き出す evorag の版 (先頭の行に書く)。
    pub evorag_version: &'a str,
    /// 書き出した時刻 (UTC、ミリ秒。先頭の行に書く)。
    pub now_ms: i64,
    /// 書かない種類 (作り直せる派生の記録。種類の名前はストアの上の層が決める)。
    pub exclude: &'a [&'a str],
    /// ストアの層では決められない確かめ (メモリの層が持つ)。
    pub checks: &'a dyn ExportChecks,
}

/// 書き出しで、ストアの層では決められない確かめ (03 §11 の「private と忘れる」と「派生の記録の確かめ」)。
/// コアがメモリの層の判定を渡す。既定のメソッドは何も外さず、何も断らない ([`NoChecks`])。
pub trait ExportChecks {
    /// 当て直しの済んでいない忘れる (04 §7.2) があるか。真なら書き出しを断る
    /// ([`ExportError::PendingForget`])。確かめられないときは真を返す (伏せ字の前の本文を書き出さない側に倒す)。
    fn pending_forget(&self, _view: &ExportView<'_>) -> bool {
        false
    }

    /// その記録を書き出すか。出典が DB に無い派生の記録 (03 §4.2 の規則 12) と、出典の改訂の番号が今のターンと
    /// 合わない派生の記録 (04 §7.7 の 1) に偽を返す。偽の記録は書かずに数える。
    fn include(&self, _view: &ExportView<'_>, _env: &Envelope) -> bool {
        true
    }
}

/// 何も外さず、何も断らない確かめ (ストアの層だけで書き出すとき)。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChecks;

impl ExportChecks for NoChecks {}

/// 書き出す DB を読む口 ([`ExportChecks`] が使う)。書き出しと同じ読み取りのトランザクションを読む。
pub struct ExportView<'a> {
    tx: &'a redb::ReadTransaction,
}

impl ExportView<'_> {
    /// 1 件を読む。
    ///
    /// # Errors
    /// DB を読めないとき。
    pub fn get(&self, kind: &str, id: Id) -> Result<Option<Read>, ExportError> {
        let table = self.tx.open_table(RECORDS).map_err(db)?;
        let k = record_key(kind, id);
        Ok(table
            .get(k.as_slice())
            .map_err(db)?
            .map(|v| classify(kind, id, &k, v.value())))
    }

    /// 1 つの種類の記録を ID の順に読む。`f` が `false` を返したら止める。
    ///
    /// # Errors
    /// DB を読めないとき。
    pub fn for_each(&self, kind: &str, mut f: impl FnMut(Read) -> bool) -> Result<(), ExportError> {
        let table = self.tx.open_table(RECORDS).map_err(db)?;
        let mut start = KeyBuf::new();
        start.str(kind);
        let start = start.into_bytes();
        let end = key::prefix_end(&start);
        let end = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        let range = table
            .range::<&[u8]>((Bound::Included(start.as_slice()), end))
            .map_err(db)?;
        for item in range {
            let (k, v) = item.map_err(db)?;
            let Some((_, id)) = split_record_key(k.value()) else {
                continue;
            };
            if !f(classify(kind, id, k.value(), v.value())) {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("{0} is not empty")]
    NotEmpty(PathBuf),
    /// ジャーナルに、まだ DB に取り込んでいない行がある (evorag を一度起動すると取り込む、03 §11)。
    #[error("the journal has {lines} line(s) not yet in the database")]
    PendingJournal { lines: usize },
    /// 当て直しの済んでいない忘れるがある (evorag を一度起動すると当て直す、03 §11)。
    #[error("a forget has not been applied to the database yet")]
    PendingForget,
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
///
/// # Errors
/// 使っているプロセスがあるとき、`out` が空でないとき、取り込んでいないジャーナルの行か当て直しの済んでいない
/// 忘れるがあるとき (どれも出力先に何も作らない)、読み書きに失敗したとき。
pub fn export(
    root: &Path,
    out: &Path,
    opts: &ExportOptions<'_>,
) -> Result<ExportReport, ExportError> {
    let _lock = DataRootLock::acquire_quiet(root).map_err(OpenError::from)?;
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return Err(ExportError::NotEmpty(out.to_path_buf()));
    }
    let file = root.join(DB_FILE);
    let database = if file.is_file() {
        Some(ReadOnlyDatabase::open(&file).map_err(db)?)
    } else {
        None
    };
    let last_seq = database
        .as_ref()
        .and_then(|d| read_meta_db_any(d).ok())
        .and_then(|m| m.get(meta::JOURNAL_LAST_SEQ)?.parse().ok())
        .unwrap_or(0);
    let pending = journal::summarize(root, last_seq)?.pending;
    if pending > 0 {
        return Err(ExportError::PendingJournal { lines: pending });
    }
    let rtx = database
        .as_ref()
        .map(ReadableDatabase::begin_read)
        .transpose()
        .map_err(db)?;
    if let Some(tx) = &rtx
        && opts.checks.pending_forget(&ExportView { tx })
    {
        return Err(ExportError::PendingForget);
    }
    fsutil::create_private_dir(out)?;
    let mut report = ExportReport::default();
    if let Some(tx) = &rtx {
        write_records(tx, out, opts, &mut report)?;
        fsutil::sync_dir(out)?;
    }
    Ok(report)
}

/// 記録を種類ごとのファイルに書く。
fn write_records(
    tx: &redb::ReadTransaction,
    out: &Path,
    opts: &ExportOptions<'_>,
    report: &mut ExportReport,
) -> Result<(), ExportError> {
    let view = ExportView { tx };
    let table = tx.open_table(RECORDS).map_err(db)?;
    let mut writers: BTreeMap<String, (PathBuf, io::BufWriter<std::fs::File>)> = BTreeMap::new();
    for item in table.iter().map_err(db)? {
        let (k, v) = item.map_err(db)?;
        let Some((kind, id)) = split_record_key(k.value()) else {
            report.skipped_corrupt += 1;
            continue;
        };
        if kind == KIND_QUARANTINE || opts.exclude.contains(&kind.as_str()) {
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
        if !opts.checks.include(&view, &env) {
            report.skipped_by_check += 1;
            continue;
        }
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
                "evorag": opts.evorag_version,
                "exported_at": opts.now_ms,
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
    Ok(())
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
