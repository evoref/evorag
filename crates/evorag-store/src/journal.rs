//! 会話のジャーナル (03 §6、ADR-0006)。
//!
//! - 置き場所は `journal/<年>-<月>.jsonl` (UTC)。1 行は `{"env": …, "op": …, "seq": N, "sum": "…"}`。
//!   `sum` は最後の項目で、行の `,"sum":"…"}` を `}` に置き換えた、書いたとおりのバイト列の xxh3-64。
//! - 操作は `put` (封筒)、`delete` (`{kind, id}`)、`meta` (`{key, value}`)。知らない操作の行は新しい版の
//!   行として数え、連番だけは数に入れる。
//! - 起動時に、ファイルの最後の行が改行で終わらなければ (書きかけ) その行だけを切り詰める。チェック
//!   サムが合わない行は読み飛ばして数える (読めない行は切り詰めない)。
//! - 開くときに、ID ごとに `put` の行のあるファイルと、単調に増える項目 (ターンの改訂の番号) の値を集める
//!   (このプロセスで追記した行も足す)。削除と忘れるの書き直しは、この集めた結果でファイルを選ぶ (03 §6、
//!   04 §7.2 の「ジャーナルの層の見つけ方」)。
//! - 削除の行がある ID の `put` の行が残っていれば (除く前に止まった)、起動時に除く。入れ替えの前に止まった
//!   一時のファイル (`*.jsonl.tmp`) は、起動時に消す。
//! - 組の行 (v0.5、03 §6): 1 つの操作を成す複数の行 (忘れる・削除の 1 回の追記) は、同じ `grp` (`{"id": <組の
//!   最初の行の連番>, "n": <行数>, "ids": [<行ごとの put の記録の ID か null>]}`) を持つ。取り込み (起動時・
//!   `restore`・`repair`) は、組の行がすべてそろった組だけを当てる。欠けた行が、後に削除の行か同じ記録の `put` の
//!   行がある `put` の行 (削除と忘れるの書き直しで正しく除いた行) だけなら、そろった組とみなす。末尾で組がそろわない
//!   (書きかけの追記の途中で止まった) ときは、その組の行を当てずに捨てる (ファイルから切り詰める)。末尾でない場所の
//!   欠けた組は、当てずに数える (`doctor` が報告する)。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{self, Read as _, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use evorag_types::{Id, Timestamp};
use serde_json::{Value as Json, json};

use crate::envelope::Envelope;
use crate::fsutil;
use crate::json::{body_uint_json, envelope_from_json, envelope_to_json};

pub const JOURNAL_DIR: &str = "journal";
const SUM_MARK: &[u8] = b",\"sum\":\"";
/// 書き直しの一時のファイルの名前の終わり (`<年>-<月>.jsonl.tmp`、03 §6)。
const TMP_SUFFIX: &str = ".jsonl.tmp";

/// 種類の名前 → その種類の、単調に増える本体の整数の項目の名前 ([`crate::KindSpec::monotonic`])。
pub(crate) type Monotonic = BTreeMap<&'static str, &'static str>;

/// ジャーナルの 1 行の中身。
#[derive(Debug, Clone, PartialEq)]
pub enum LineOp {
    Put(Envelope),
    Delete { kind: String, id: Id },
    Meta { key: String, value: String },
}

/// 組の行の印 (03 §6 の「組の行」、v0.5)。同じ組の行は、同じ `id`・`n` を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// 組の ID。組の最初の行の連番 (組の行は 1 回の追記で、続いた連番で書く。行の組の中の位置は `連番 - id`)。
    pub id: u64,
    /// 組の行の数。
    pub n: u64,
    /// 組の最初の行 (位置 0) だけが持つ、位置の順の `put` の行の封筒の ID (`put` でない行は `None`。長さは `n`)。
    /// 欠けた行が何だったかを決めるのに使う (書き直しで正しく除いた `put` の行か、03 §6)。ほかの行は持たない
    /// (ジャーナルを組の行の数の 2 乗で太らせないため)。
    pub ids: Option<Vec<Option<Id>>>,
}

impl Group {
    /// `ops` を 1 つの組にするときの、`i` 番目の行の印。`first` は組の最初の行の連番。
    fn line(first: u64, ops: &[LineOp], i: usize) -> Self {
        Self {
            id: first,
            n: ops.len() as u64,
            ids: (i == 0).then(|| {
                ops.iter()
                    .map(|op| match op {
                        LineOp::Put(e) => Some(e.id),
                        _ => None,
                    })
                    .collect()
            }),
        }
    }
}

/// 読んだ行。
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub seq: u64,
    pub op: LineOp,
    /// 組の行なら、その組 (v0.5、03 §6)。
    pub grp: Option<Group>,
}

/// 起動時に読んだ結果。
#[derive(Debug, Default)]
pub struct Scan {
    pub lines: Vec<Line>,
    /// チェックサムが合わない行の数。
    pub bad: usize,
    /// チェックサムは合うが読めない行 (知らない操作・新しい形) の数。
    pub newer: usize,
    /// 書きかけで切り詰めた末尾のバイト数 (すべてのファイルの合計)。
    pub truncated: usize,
    /// すべての `meta` の行 (`last_seq` に関わらず、連番の順)。古い版が取り込まずに連番だけを
    /// 進めた `meta` の行を、新しい版が探すため (03 §3.1.1 の `memory.forget_key`)。
    pub metas: Vec<(String, String)>,
    /// すべての削除の行の ID と種類 (`last_seq` に関わらない)。書き手は、これらを `put` で作り直さない
    /// (04 §7.7 の 3)。取り込みは、これらの `put` の行を当てず、DB に残っていれば消す。
    pub deleted: BTreeMap<Id, String>,
    /// 削除の行のある ID の `put` の行を除く書き直しが、開くときに済まなかった理由 (次に開くとき、または
    /// 次の削除の書き直しで、もう一度試す)。
    pub rewrite_failed: Option<RewriteError>,
    /// 末尾で組の行がそろわず、当てずに捨てた組の数 (03 §6 の「組の行」)。切り詰めたバイト数は `truncated` に足す。
    pub torn_groups: usize,
    /// 行の欠けた組 (末尾の書きかけを除く) の数。読める行は当てる (`doctor` が報告する)。
    pub incomplete_groups: usize,
    /// 行の欠けた組のうち、`last_seq` より後の組の `put` の行の記録の ID (当て直しの印、03 §6)。
    pub partial: Vec<Id>,
}

/// 書き直しで、1 つの ID の `put` の行のうち何を除くか (03 §6 の「削除と忘れる」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalCut {
    /// 削除: 封筒の `id` がその ID の `put` の行をすべて除く (行の版に関わらない)。
    All,
    /// 忘れる: 封筒の `id` がその ID で、種類の単調に増える項目 (ターンの改訂の番号) が、この値より小さい
    /// `put` の行を除く。その ID の、この値以上の `put` の行がジャーナルに無ければ、何も除かない
    /// (伏せ字の後の行が無いのに前の行を除くと、戻せなくなるため、04 §7.2 の「ジャーナルの層」)。
    Below(u64),
}

/// 1 つの ID の `put` の行の、単調に増える項目の値の範囲 (開くときに集め、このプロセスの追記と書き直しで
/// 更新する)。忘れるの書き直しが済んでいないこと (`min < 伏せ字の後の番号 <= max`) を見つけるのに使う
/// (04 §7.2 の「ジャーナルの層の見つけ方」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevSpan {
    pub min: u64,
    pub max: u64,
}

/// 書き直しの結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewriteReport {
    /// 入れ替えたファイルの数。
    pub files: usize,
    /// 除いた行の数。
    pub removed: usize,
    /// 読んだファイルにあり、除く対象に当たらずに残した、新しい版の行 (チェックサムは合うが読めない行) の数
    /// (`doctor` が報告する)。
    pub kept_newer: usize,
    /// 忘れるの対象の ID の `put` の行のうち、単調に増える項目を読めずに残した行の数。
    pub unreadable_rev: usize,
    /// [`JournalCut::Below`] のうち、その値以上の `put` の行が無いので何も除かなかった ID。
    pub skipped: Vec<Id>,
}

/// 書き直しの失敗。どの場合も、失敗したファイルは元のまま残り (一時のファイルは消す)、同じ書き直しを
/// 試し直してよい (何度当てても同じ)。それより前に入れ替えたファイルは入れ替えたまま残る。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RewriteError {
    /// 入れ替え (rename) に失敗した (Windows で、ほかのプロセスがファイルを開いている)。後で試し直す
    /// (削除は job に残し、忘れるは sleep-time に試し直す、03 §6)。
    #[error("journal file {file} could not be replaced: {reason}")]
    Swap { file: String, reason: String },
    /// 書いた一時のファイルを読み直して確かめたら、合わなかった。入れ替えていない。
    #[error("the rewritten journal file {file} did not verify: {reason}")]
    Verify { file: String, reason: String },
    /// 読み書きに失敗した。
    #[error("journal file {file}: {reason}")]
    Io { file: String, reason: String },
}

impl RewriteError {
    /// 入れ替えの失敗 (後で試し直せば済みうる) か。
    pub fn is_swap(&self) -> bool {
        matches!(self, Self::Swap { .. })
    }
}

/// 試験のための注入: 次の書き直しを、決まった点で失敗させる (09 §3.5 の止める点)。1 度当たると消える。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteFault {
    /// 一時のファイルを書いた後、読み直しの前に、その中身を壊す (読み直しの検査で止まる)。
    CorruptTemp,
    /// 一時のファイルを書き切った後、入れ替えの前に止まる (強制終了をまねる。一時のファイルを残す)。
    StopAfterTemp,
    /// その書き直しで `nth` 番目 (0 から) に入れ替えるファイルの入れ替えを失敗させる。
    FailSwap { nth: usize },
    /// 入れ替えた後、集めた結果を更新する前に止まる (強制終了をまねる)。
    StopAfterSwap,
    /// 入れ替えた今の月のファイルを開き直すとき、`times` 回だけ共有違反 (一時の失敗) にする。
    ReopenBusy { times: usize },
    /// 入れ替えた今の月のファイルを開き直せないようにする (一時でない失敗)。
    ReopenFails,
}

/// 試験の注入で使う、一時の共有違反 (Windows の `ERROR_SHARING_VIOLATION`)。
fn sharing_violation() -> io::Error {
    #[cfg(windows)]
    {
        io::Error::from_raw_os_error(32)
    }
    #[cfg(not(windows))]
    {
        io::Error::from(io::ErrorKind::WouldBlock)
    }
}

/// 集めた `put` の行の 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PutRef {
    /// ファイルの番号 ([`PutIndex::names`] の位置)。
    file: u32,
    /// 種類の単調に増える項目の値。宣言の無い種類と、読めない値は `None`。
    rev: Option<u64>,
}

/// ID ごとの `put` の行のあるファイルと、単調に増える項目の値 (03 §6、04 §7.2)。
#[derive(Debug, Default)]
struct PutIndex {
    /// ファイルの名前 (`<年>-<月>.jsonl`)。
    names: Vec<String>,
    puts: BTreeMap<Id, Vec<PutRef>>,
    /// チェックサムの合わない行のあるファイル (その行の ID は読めないので、書き直すたびに読む)。
    bad: BTreeSet<u32>,
}

impl PutIndex {
    fn file(&mut self, name: &str) -> u32 {
        let at = match self.names.iter().position(|n| n == name) {
            Some(at) => at,
            None => {
                self.names.push(name.to_owned());
                self.names.len() - 1
            }
        };
        u32::try_from(at).expect("fewer than 2^32 journal files")
    }

    fn add(&mut self, id: Id, r: PutRef) {
        self.puts.entry(id).or_default().push(r);
    }

    fn span(&self, id: Id) -> Option<RevSpan> {
        self.puts
            .get(&id)?
            .iter()
            .filter_map(|r| r.rev)
            .fold(None, |acc, r| {
                Some(match acc {
                    None => RevSpan { min: r, max: r },
                    Some(s) => RevSpan {
                        min: s.min.min(r),
                        max: s.max.max(r),
                    },
                })
            })
    }
}

pub struct Journal {
    dir: PathBuf,
    next_seq: u64,
    monotonic: Monotonic,
    index: PutIndex,
    /// 書き直しで入れ替えたファイルを、追記のために開き直せなかった。以後は追記しない (03 §6 の「失敗」)。
    stopped: Option<String>,
    /// このプロセスが最後に追記したファイルの名前 (今の月のファイル)。
    current: Option<String>,
    /// 試験のための注入: 次の追記を失敗させる。
    fail_next: AtomicBool,
    /// 試験のための注入: 次の書き直しを失敗させる。
    fault: Option<RewriteFault>,
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// 中身の JSON の文字列 (`}` で終わる) に `sum` を足して 1 行にする。
fn seal(body: String) -> Vec<u8> {
    let sum = checksum(body.as_bytes());
    let mut line = body.into_bytes();
    line.pop(); // `}`
    line.extend_from_slice(SUM_MARK);
    line.extend_from_slice(sum.as_bytes());
    line.extend_from_slice(b"\"}\n");
    line
}

fn encode(seq: u64, op: &LineOp, grp: Option<&Group>) -> Vec<u8> {
    let (name, env) = match op {
        LineOp::Put(e) => ("put", envelope_to_json(e)),
        LineOp::Delete { kind, id } => ("delete", json!({"kind": kind, "id": id.to_string()})),
        LineOp::Meta { key, value } => ("meta", json!({"key": key, "value": value})),
    };
    let mut line = json!({"seq": seq, "op": name, "env": env});
    if let Some(g) = grp {
        let mut grp = json!({"id": g.id, "n": g.n});
        if let Some(ids) = &g.ids {
            let ids: Vec<Json> = ids
                .iter()
                .map(|id| id.map_or(Json::Null, |id| Json::String(id.to_string())))
                .collect();
            grp["ids"] = Json::Array(ids);
        }
        line["grp"] = grp;
    }
    seal(line.to_string())
}

/// 行の `grp` を読む。無ければ `Some(None)`。あるのに形が違う (新しい版の形) なら `None`。
fn decode_group(v: &Json) -> Option<Option<Group>> {
    let Some(g) = v.get("grp") else {
        return Some(None);
    };
    let id = g.get("id").and_then(Json::as_u64)?;
    let n = g.get("n").and_then(Json::as_u64).filter(|n| *n >= 1)?;
    let ids = match g.get("ids") {
        None => None,
        Some(ids) => {
            let ids = ids
                .as_array()?
                .iter()
                .map(|x| match x {
                    Json::Null => Some(None),
                    Json::String(s) => s.parse().ok().map(Some),
                    _ => None,
                })
                .collect::<Option<Vec<Option<Id>>>>()?;
            if ids.len() as u64 != n {
                return None;
            }
            Some(ids)
        }
    };
    Some(Some(Group { id, n, ids }))
}

/// 追記する行を組にするか ([`Journal::append_group`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grouping {
    /// 行ごとに独り立ちさせる。
    Lines,
    /// 1 つの組にする。
    Group,
}

/// 末尾の書きかけの組を、まだコミットしていないものとして捨ててよいか (03 §6 の「組の行」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// DB は、書き手が使っていたもの: `journal.last_seq` より後の書きかけの組は、まだコミットしていない。
    Uncommitted,
    /// DB を restore で戻した・失って新しく作った・repair で作り直した (または読むだけで数える): 末尾の組を
    /// コミットしたか分からないので、切り詰めずに読める行を当てる。
    Unknown,
}

impl Tail {
    /// DB の `meta` (DB が無ければ `None`) から決める (03 §6)。
    pub(crate) fn of(meta: Option<&BTreeMap<String, String>>) -> Self {
        match meta {
            Some(m) if !m.contains_key(crate::meta::JOURNAL_TAIL_UNKNOWN) => Tail::Uncommitted,
            _ => Tail::Unknown,
        }
    }
}

/// 組の判定に使う 1 行の要約 (本文を持たない)。
#[derive(Debug)]
struct Row {
    /// ファイルの番号 (同じファイルの行を見分ける)。
    file: usize,
    /// 読める行 (新しい版の行を含む) の連番。
    seq: Option<u64>,
    /// この版が読める行か (新しい版の行でない)。
    known: bool,
    grp: Option<Group>,
    bad: bool,
    delete: Option<Id>,
    put: Option<Id>,
}

impl Row {
    fn of(file: usize, seen: &Seen) -> Self {
        let mut row = Self {
            file,
            seq: None,
            known: false,
            grp: None,
            bad: false,
            delete: None,
            put: seen.put.map(|(id, _)| id),
        };
        match &seen.decoded {
            Decoded::Line(line) => {
                row.seq = Some(line.seq);
                row.known = true;
                row.grp = line.grp.clone();
                if let LineOp::Delete { id, .. } = &line.op {
                    row.delete = Some(*id);
                }
            }
            Decoded::Newer(seq) => row.seq = *seq,
            Decoded::Bad => {
                row.bad = true;
                row.put = None;
            }
        }
        row
    }
}

/// 組の欠けた行を、書き直しで正しく除いた行と見分ける材料 (03 §6 の「組の行」)。当てる行の、ID ごとの削除の行と
/// `put` の行の連番の最大。
#[derive(Debug, Default)]
struct Later {
    deleted: BTreeMap<Id, u64>,
    put: BTreeMap<Id, u64>,
}

impl Later {
    fn note(map: &mut BTreeMap<Id, u64>, id: Id, seq: u64) {
        let e = map.entry(id).or_insert(seq);
        *e = (*e).max(seq);
    }

    /// 連番 `at` の `put` の行 (記録 `id`) が無いのは、書き直しで正しく除いたものか: その後に `id` の削除の行がある
    /// (削除の書き直し) か、その後に `id` の `put` の行がある (忘れるの書き直しで前の版を除いた)。
    fn explains(&self, id: Id, at: u64) -> bool {
        self.deleted.get(&id).is_some_and(|&s| s > at) || self.put.get(&id).is_some_and(|&s| s > at)
    }
}

/// 組の行のそろい方 (03 §6 の「組の行」)。
#[derive(Debug, Default)]
struct GroupCheck {
    /// 当てずに切り詰める、末尾の書きかけの組の ID と、その最初の行 (`rows` の添字)。
    torn: Option<(u64, usize)>,
    /// 行の欠けた組 (書きかけを除く)。読める行は当てる。
    partial: BTreeSet<u64>,
}

impl GroupCheck {
    /// `rows` はすべての行の要約 (ファイルと行の順)、`max_seq` はすべての読める行 (新しい版の行を含む) の連番の最大、
    /// `last_seq` は DB が取り込んだ最後の連番。
    ///
    /// - 組の行がすべてあるか、欠けた行がどれも書き直しで正しく除いた `put` の行 ([`Later::explains`]) なら、そろった組。
    /// - 末尾の書きかけ (`tail` が [`Tail::Uncommitted`] で、組がファイルの末尾にあり、読める行が位置 0 から続き、
    ///   間にチェックサムの合わない行が無く、すべての行の中の最後で、`last_seq` より後) は、当てずに切り詰める組。
    /// - それ以外の欠けた組は、読める行を当てる組 ([`GroupCheck::is_partial`])。
    fn new(rows: &[Row], max_seq: u64, last_seq: u64, tail: Tail) -> Self {
        struct Seen<'r> {
            first: usize,
            n: u64,
            ids: Option<&'r [Option<Id>]>,
            at: BTreeSet<u64>,
            mixed: bool,
            min: u64,
            max: u64,
        }
        let mut seen: BTreeMap<u64, Seen<'_>> = BTreeMap::new();
        for (i, row) in rows.iter().enumerate() {
            let (Some(seq), Some(g)) = (row.seq, &row.grp) else {
                continue;
            };
            let e = seen.entry(g.id).or_insert(Seen {
                first: i,
                n: g.n,
                ids: None,
                at: BTreeSet::new(),
                mixed: false,
                min: seq,
                max: seq,
            });
            let pos = seq.checked_sub(g.id).filter(|p| *p < g.n);
            // 組の印の食い違い・組の外の連番・同じ位置の 2 行・位置 0 でない行の `ids` は、形の崩れた組とする。
            e.mixed |= e.n != g.n || pos.is_none_or(|p| !e.at.insert(p));
            if let Some(ids) = &g.ids {
                e.mixed |= e.ids.is_some() || pos != Some(0);
                e.ids = Some(ids);
            }
            e.min = e.min.min(seq);
            e.max = e.max.max(seq);
        }
        let mut out = Self::default();
        if tail == Tail::Uncommitted {
            out.torn = seen
                .iter()
                .find(|(id, e)| {
                    let prefix = e.at.iter().copied().eq(0..e.at.len() as u64);
                    !e.mixed
                        && (e.at.len() as u64) < e.n
                        && prefix
                        && e.max == max_seq
                        && e.min > last_seq
                        && Self::ends_its_file(rows, e.first, **id)
                })
                .map(|(id, e)| (*id, e.first));
        }
        let torn = out.torn.map(|(id, _)| id);
        let mut later = Later::default();
        for row in rows {
            if row.grp.as_ref().is_some_and(|g| Some(g.id) == torn) {
                continue;
            }
            if let Some(seq) = row.seq {
                if let Some(id) = row.delete {
                    Later::note(&mut later.deleted, id, seq);
                }
                if let Some(id) = row.put {
                    Later::note(&mut later.put, id, seq);
                }
            }
        }
        for (id, e) in seen {
            if Some(id) == torn {
                continue;
            }
            let explained = !e.mixed
                && (0..e.n).filter(|p| !e.at.contains(p)).all(|p| {
                    let rec = e
                        .ids
                        .and_then(|ids| ids.get(usize::try_from(p).ok()?).copied().flatten());
                    let at = id.checked_add(p);
                    matches!((rec, at), (Some(rec), Some(at)) if later.explains(rec, at))
                });
            if !explained {
                out.partial.insert(id);
            }
        }
        out
    }

    /// 組 `id` の最初の行 `first` から、そのファイルの終わりまでが、その組の行だけか。
    fn ends_its_file(rows: &[Row], first: usize, id: u64) -> bool {
        let file = rows[first].file;
        rows[first..]
            .iter()
            .take_while(|r| r.file == file)
            .all(|r| !r.bad && r.grp.as_ref().is_some_and(|g| g.id == id))
    }

    /// その行を当てるか (末尾の書きかけの組の行だけを当てない)。
    fn applies(&self, grp: Option<&Group>) -> bool {
        !grp.is_some_and(|g| self.torn.is_some_and(|(t, _)| t == g.id))
    }

    /// その行が、行の欠けた組 (読める行を当てる) の行か。
    fn is_partial(&self, grp: Option<&Group>) -> bool {
        grp.is_some_and(|g| self.partial.contains(&g.id))
    }

    /// 行の欠けた組の数 (書きかけを除く)。
    fn incomplete(&self) -> usize {
        self.partial.len()
    }
}

/// 行の読み方の結果。
enum Decoded {
    Line(Line),
    /// チェックサムは合うが読めない (新しい版の行)。分かれば連番。
    Newer(Option<u64>),
    Bad,
}

/// `sum` を確かめ、覆うバイト列 (`,"sum":"…"}` を `}` に置き換えたもの) を返す。
fn verified_body(line: &[u8]) -> Option<Vec<u8>> {
    let at = line.windows(SUM_MARK.len()).rposition(|w| w == SUM_MARK)?;
    let rest = &line[at + SUM_MARK.len()..];
    let hex = rest.strip_suffix(b"\"}")?;
    let mut body = line[..at].to_vec();
    body.push(b'}');
    (checksum(&body).as_bytes() == hex).then_some(body)
}

fn decode_value(v: &Json) -> Decoded {
    let Some(seq) = v.get("seq").and_then(Json::as_u64) else {
        return Decoded::Newer(None);
    };
    let text = |env: &Json, k: &str| env.get(k).and_then(Json::as_str).map(str::to_owned);
    let op = match (v.get("op").and_then(Json::as_str), v.get("env")) {
        (Some("put"), Some(env)) => envelope_from_json(env).ok().map(LineOp::Put),
        (Some("delete"), Some(env)) => match (text(env, "kind"), text(env, "id")) {
            (Some(kind), Some(id)) => id.parse().ok().map(|id| LineOp::Delete { kind, id }),
            _ => None,
        },
        (Some("meta"), Some(env)) => match (text(env, "key"), text(env, "value")) {
            (Some(key), Some(value)) => Some(LineOp::Meta { key, value }),
            _ => None,
        },
        _ => None,
    };
    match (op, decode_group(v)) {
        (Some(op), Some(grp)) => Decoded::Line(Line { seq, op, grp }),
        // 知らない操作と、読めない形の組の印の行は、新しい版の行として当てない。
        _ => Decoded::Newer(Some(seq)),
    }
}

/// 1 行を、集め方と書き直しのために読んだ結果。
struct Seen {
    decoded: Decoded,
    /// `put` の行なら、封筒の `id` と、その種類の単調に増える項目の値 (行の版に関わらず、この 2 つだけを読む)。
    put: Option<(Id, Option<u64>)>,
}

fn inspect(line: &[u8], monotonic: &Monotonic) -> Seen {
    let Some(body) = verified_body(line) else {
        return Seen {
            decoded: Decoded::Bad,
            put: None,
        };
    };
    let Ok(v) = serde_json::from_slice::<Json>(&body) else {
        return Seen {
            decoded: Decoded::Newer(None),
            put: None,
        };
    };
    Seen {
        put: raw_put(&v, monotonic),
        decoded: decode_value(&v),
    }
}

/// チェックサムの合う行が `put` なら、封筒の `id` と単調に増える項目の値を、封筒の全体を読まずに読む
/// (03 §6 の「除く行」の (b))。
fn raw_put(v: &Json, monotonic: &Monotonic) -> Option<(Id, Option<u64>)> {
    if v.get("op").and_then(Json::as_str) != Some("put") {
        return None;
    }
    let env = v.get("env")?;
    let id: Id = env.get("id")?.as_str()?.parse().ok()?;
    let rev = env
        .get("kind")
        .and_then(Json::as_str)
        .and_then(|kind| monotonic.get(kind))
        .and_then(|field| body_uint_json(env.get("body")?, field));
    Some((id, rev))
}

/// 書き直した一時のファイルを読み直し、書いたバイト列 (残した行) とちょうど同じかを確かめる (行ごとの `sum`・
/// 行の数・連番の順も、これで確かめたことになる)。
fn verify(tmp: &Path, kept: &[u8]) -> Result<(), String> {
    let got = std::fs::read(tmp).map_err(|e| e.to_string())?;
    if got == kept {
        return Ok(());
    }
    let at = got
        .iter()
        .zip(kept)
        .position(|(g, k)| g != k)
        .unwrap_or(got.len().min(kept.len()));
    Err(format!(
        "{} bytes written, {} read back, first difference at byte {at}",
        kept.len(),
        got.len()
    ))
}

fn month(ts: Timestamp) -> String {
    // 年は 4 桁に収める (範囲外の時計でもファイルの名前が崩れないように)。
    const Y0: i64 = -62_135_596_800_000; // 0001-01-01
    const Y9999: i64 = 253_402_300_799_000; // 9999-12-31
    let s = crate::backup::stamp(ts.as_millis().clamp(Y0, Y9999));
    format!("{}-{}", &s[0..4], &s[4..6])
}

fn files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    v.sort();
    Ok(v)
}

fn trim_newline(l: &[u8]) -> &[u8] {
    let l = l.strip_suffix(b"\n").unwrap_or(l);
    l.strip_suffix(b"\r").unwrap_or(l)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 読むだけの走査の結果 (点検のため。切り詰めも後始末もしない)。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub lines: usize,
    pub bad: usize,
    pub newer: usize,
    /// 書きかけの末尾のバイト数 (次の起動で切り詰める)。
    pub partial: usize,
    pub max_seq: u64,
    /// `last_seq` より後の行 (次の起動で取り込む)。組がそろわずに当てない行は数えない。
    pub pending: usize,
    /// 末尾で組の行がそろわない組 (次の起動で捨てる、03 §6 の「組の行」)。
    pub torn_groups: usize,
    /// 行の欠けた組 (末尾の書きかけを除く。読める行は当てる)。
    pub incomplete_groups: usize,
}

/// ジャーナルの全体を読んだ結果 (書き換えない)。`lines` には `keep` が残した行だけを持つ。
struct Pass {
    rows: Vec<Row>,
    lines: Vec<Line>,
    files: usize,
    bad: usize,
    newer: usize,
    known: usize,
    partial_bytes: usize,
    max_seq: u64,
}

/// すべてのファイルを、書き換えずに行の順に読む。書きかけの末尾 (改行で終わらない最後の行) は数えるだけにする。
fn read_pass(root: &Path, keep: &dyn Fn(&LineOp) -> bool) -> io::Result<Pass> {
    let mut p = Pass {
        rows: Vec::new(),
        lines: Vec::new(),
        files: 0,
        bad: 0,
        newer: 0,
        known: 0,
        partial_bytes: 0,
        max_seq: 0,
    };
    let none = Monotonic::new();
    for (file, path) in files(&root.join(JOURNAL_DIR))?.into_iter().enumerate() {
        p.files += 1;
        let bytes = std::fs::read(&path)?;
        let full = if bytes.is_empty() || bytes.last() == Some(&b'\n') {
            bytes.len()
        } else {
            bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
        };
        p.partial_bytes += bytes.len() - full;
        for raw in bytes[..full].split_inclusive(|&b| b == b'\n') {
            let l = trim_newline(raw);
            if l.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let seen = inspect(l, &none);
            p.rows.push(Row::of(file, &seen));
            match seen.decoded {
                Decoded::Line(line) => {
                    p.known += 1;
                    p.max_seq = p.max_seq.max(line.seq);
                    if keep(&line.op) {
                        p.lines.push(line);
                    }
                }
                Decoded::Newer(seq) => {
                    p.newer += 1;
                    p.max_seq = p.max_seq.max(seq.unwrap_or(0));
                }
                Decoded::Bad => p.bad += 1,
            }
        }
    }
    Ok(p)
}

/// ジャーナルを、何も書かずに数える (`doctor`、AC-FR-207-1)。`tail` は、DB が末尾の書きかけの組をコミットして
/// いないと分かるか (03 §6)。
pub(crate) fn summarize(root: &Path, last_seq: u64, tail: Tail) -> io::Result<Summary> {
    let p = read_pass(root, &|_| false)?;
    let groups = GroupCheck::new(&p.rows, p.max_seq, last_seq, tail);
    Ok(Summary {
        files: p.files,
        lines: p.known,
        bad: p.bad,
        newer: p.newer,
        partial: p.partial_bytes,
        max_seq: p.max_seq,
        pending: p
            .rows
            .iter()
            .filter(|r| r.known && r.seq.is_some_and(|s| s > last_seq))
            .filter(|r| groups.applies(r.grp.as_ref()))
            .count(),
        torn_groups: usize::from(groups.torn.is_some()),
        incomplete_groups: groups.incomplete(),
    })
}

/// 書き換えずに、すべての `meta` の行を連番の順に集める (読むだけで開いたとき、03 §3.1.1)。ほかの行の本文は持たない。
pub fn scan_metas(root: &Path) -> io::Result<Vec<(String, String)>> {
    let p = read_pass(root, &|op| matches!(op, LineOp::Meta { .. }))?;
    let groups = GroupCheck::new(&p.rows, p.max_seq, 0, Tail::Unknown);
    let mut lines: Vec<Line> = p
        .lines
        .into_iter()
        .filter(|l| groups.applies(l.grp.as_ref()))
        .collect();
    lines.sort_by_key(|l| l.seq);
    Ok(lines
        .into_iter()
        .filter_map(|line| match line.op {
            LineOp::Meta { key, value } => Some((key, value)),
            _ => None,
        })
        .collect())
}

/// 書き換えずに、すべての削除の行の ID と種類を集める (`import-v04` が、v0.4 が開くときに消すものを取り込まない
/// ため、03 §12.4)。チェックサムの合わない行と書きかけの末尾は飛ばす。
pub(crate) fn scan_deleted(root: &Path) -> io::Result<BTreeMap<Id, String>> {
    let p = read_pass(root, &|op| matches!(op, LineOp::Delete { .. }))?;
    Ok(p.lines
        .into_iter()
        .filter_map(|line| match line.op {
            LineOp::Delete { kind, id } => Some((id, kind)),
            _ => None,
        })
        .collect())
}

/// 書き直しの一時のファイル (`*.jsonl.tmp`) が残っているか (次に書き込みで開いたときに片付ける、03 §6)。
pub(crate) fn has_rewrite_leftover(root: &Path) -> io::Result<bool> {
    match std::fs::read_dir(root.join(JOURNAL_DIR)) {
        Ok(rd) => Ok(rd
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(TMP_SUFFIX))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// [`read_all`] の結果。
#[derive(Debug, Default)]
pub(crate) struct ReadAll {
    /// 当てる行 (連番の順)。
    pub lines: Vec<Line>,
    /// チェックサムの合わない行の数。
    pub bad: usize,
    /// 行の欠けた組の数 (読める行は `lines` に入れた)。
    pub incomplete_groups: usize,
    /// 行の欠けた組の `put` の行の記録の ID (`journal.partial_groups`、03 §6)。
    pub partial: Vec<Id>,
}

/// 書き換えずに、読めるすべての行を連番の順に読む (`repair`、03 §10)。書きかけの末尾、チェックサムの合わない行、
/// 新しい版の行は飛ばす。DB が末尾の組をコミットしたか分からないので、行の欠けた組も読める行を当てる (03 §6)。
pub(crate) fn read_all(root: &Path) -> io::Result<ReadAll> {
    let p = read_pass(root, &|_| true)?;
    let groups = GroupCheck::new(&p.rows, p.max_seq, 0, Tail::Unknown);
    let mut out = ReadAll {
        bad: p.bad,
        incomplete_groups: groups.incomplete(),
        ..ReadAll::default()
    };
    for line in p.lines {
        if groups.is_partial(line.grp.as_ref())
            && let LineOp::Put(e) = &line.op
        {
            out.partial.push(e.id);
        }
        out.lines.push(line);
    }
    out.lines.sort_by_key(|l| l.seq);
    Ok(out)
}

/// 書き換えずに、種類 `kind` の `put` の行の単調に増える項目 `field` の値を、ID ごとに行の数だけ集める (`doctor`、
/// 04 §7.2 の「ジャーナルの層の見つけ方」)。書きかけの末尾とチェックサムの合わない行と、値を読めない行は飛ばす。
pub(crate) fn put_revs(
    root: &Path,
    kind: &'static str,
    field: &'static str,
) -> io::Result<BTreeMap<Id, Vec<u64>>> {
    let monotonic: Monotonic = [(kind, field)].into_iter().collect();
    let mut out: BTreeMap<Id, Vec<u64>> = BTreeMap::new();
    for path in files(&root.join(JOURNAL_DIR))? {
        let bytes = std::fs::read(&path)?;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            if raw.last() != Some(&b'\n') {
                continue; // 書きかけの末尾
            }
            if let Some((id, Some(rev))) = inspect(trim_newline(raw), &monotonic).put {
                out.entry(id).or_default().push(rev);
            }
        }
    }
    Ok(out)
}

impl Journal {
    /// 開いて読む。`last_seq` より大きい行を返す。書きかけの末尾を切り詰め、残っている一時のファイルを消し、
    /// 削除の後始末をする。`monotonic` は、種類ごとの単調に増える項目 (集める値)。
    pub fn open(
        root: &Path,
        last_seq: u64,
        monotonic: Monotonic,
        tail: Tail,
    ) -> io::Result<(Self, Scan)> {
        let dir = root.join(JOURNAL_DIR);
        fsutil::create_private_dir(&dir)?;
        // 残っている一時のファイル (03 §6)。元のファイルがあれば、入れ替えの前に止まったので要らない。元のファイルが
        // 無ければ、読み直しの検査を通った後の入れ替えの途中で止まった完全な写しなので、元の名前に戻す。
        let leftovers: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect();
        for name in leftovers {
            let tmp = dir.join(&name);
            let original = dir.join(name.trim_end_matches(".tmp"));
            if original.exists() {
                let _ = std::fs::remove_file(&tmp);
            } else {
                fsutil::rename_durable(&tmp, &original)?;
            }
        }
        /// 1 つの行の、ファイルの中の位置と、要る中身 (削除・`meta` と、`last_seq` より後の行)。
        struct Entry {
            path: usize,
            file: u32,
            start: usize,
            op: Option<LineOp>,
            /// `put` の行なら、封筒の `id` と単調に増える項目の値 (書き直しのために集める)。
            put: Option<(Id, Option<u64>)>,
        }
        let mut scan = Scan::default();
        let mut max_seq = last_seq;
        let mut index = PutIndex::default();
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        for path in files(&dir)? {
            let file = index.file(&path.file_name().unwrap_or_default().to_string_lossy());
            let mut bytes = std::fs::read(&path)?;
            if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
                let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                scan.truncated += bytes.len() - keep;
                bytes.truncate(keep);
                let f = OpenOptions::new().write(true).open(&path)?;
                f.set_len(keep as u64)?;
                f.sync_all()?;
            }
            let at = paths.len();
            paths.push(path);
            let mut start = 0;
            for raw in bytes.split_inclusive(|&b| b == b'\n') {
                let here = start;
                start += raw.len();
                let l = trim_newline(raw);
                if l.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let seen = inspect(l, &monotonic);
                rows.push(Row::of(at, &seen));
                let op = match seen.decoded {
                    Decoded::Line(line) => {
                        max_seq = max_seq.max(line.seq);
                        let keep = line.seq > last_seq || !matches!(line.op, LineOp::Put(_));
                        keep.then_some(line.op)
                    }
                    Decoded::Newer(seq) => {
                        scan.newer += 1;
                        if let Some(s) = seq {
                            max_seq = max_seq.max(s);
                        }
                        None
                    }
                    Decoded::Bad => {
                        scan.bad += 1;
                        index.bad.insert(file);
                        None
                    }
                };
                entries.push(Entry {
                    path: at,
                    file,
                    start: here,
                    op,
                    put: seen.put,
                });
            }
        }
        // 組の行のそろい方 (03 §6)。末尾の書きかけの組は、DB にコミットしていないので、ファイルから切り詰めて捨てる。
        let groups = GroupCheck::new(&rows, max_seq, last_seq, tail);
        let mut cut_from: Option<(usize, usize)> = None;
        if let Some((_, first)) = groups.torn {
            let (path, cut) = (entries[first].path, entries[first].start);
            let f = OpenOptions::new().write(true).open(&paths[path])?;
            let len = f.metadata()?.len();
            f.set_len(cut as u64)?;
            f.sync_all()?;
            scan.truncated += usize::try_from(len).unwrap_or(usize::MAX) - cut;
            cut_from = Some((path, cut));
            scan.torn_groups = 1;
        }
        scan.incomplete_groups = groups.incomplete();
        let mut deleted: BTreeMap<Id, String> = BTreeMap::new();
        let mut metas: Vec<(u64, String, String)> = Vec::new();
        for (e, row) in entries.into_iter().zip(rows) {
            // 切り詰めた行は、書き直しのために集めない (無い行の改訂の番号で、前の行を除かないため)。
            let gone = cut_from.is_some_and(|(path, cut)| e.path == path && e.start >= cut);
            if let (Some((id, rev)), false) = (e.put, gone) {
                index.add(id, PutRef { file: e.file, rev });
            }
            let (Some(op), Some(seq)) = (e.op, row.seq) else {
                continue;
            };
            if !groups.applies(row.grp.as_ref()) {
                continue;
            }
            match &op {
                // 組の中の削除の行も、いつも削除として扱う (行の欠けた組でも、消したものを蘇らせない)。
                LineOp::Delete { id, kind } => {
                    deleted.insert(*id, kind.clone());
                }
                LineOp::Put(_) => {}
                LineOp::Meta { key, value } => {
                    metas.push((seq, key.clone(), value.clone()));
                }
            }
            if seq > last_seq {
                if groups.is_partial(row.grp.as_ref())
                    && let LineOp::Put(env) = &op
                {
                    scan.partial.push(env.id);
                }
                scan.lines.push(Line {
                    seq,
                    op,
                    grp: row.grp,
                });
            }
        }
        scan.lines.sort_by_key(|l| l.seq);
        metas.sort_by_key(|m| m.0);
        scan.metas = metas.into_iter().map(|(_, k, v)| (k, v)).collect();
        let mut journal = Self {
            dir,
            next_seq: max_seq.saturating_add(1),
            monotonic,
            index,
            stopped: None,
            current: None,
            fail_next: AtomicBool::new(false),
            fault: None,
        };
        // 削除の行があるのに `put` の行が残っていれば、除く (除く前に止まった、03 §6)。チェックサムの合わない行が
        // あれば、その行の ID は集められないので、削除の行のある ID をすべて当てる (正しい `put` の行がもう無くても、
        // 消した ID を含む壊れた行を除くため)。入れ替えられなくても開くのは止めない (次に開くときに、もう一度試す)。
        let all = !journal.index.bad.is_empty();
        let leftover: BTreeMap<Id, JournalCut> = deleted
            .keys()
            .filter(|id| all || journal.index.puts.contains_key(*id))
            .map(|id| (*id, JournalCut::All))
            .collect();
        if let Err(e) = journal.rewrite(&leftover) {
            scan.rewrite_failed = Some(e);
        }
        scan.deleted = deleted;
        Ok((journal, scan))
    }

    /// 行を追記して書き切る。最後の行の連番を返す。失敗したらファイルを追記の前の長さに戻し、
    /// 連番を進めない。
    pub fn append(&mut self, now: Timestamp, ops: &[LineOp]) -> io::Result<u64> {
        self.append_lines(now, ops, Grouping::Lines)
    }

    /// 1 つの操作を成す行 (忘れる・削除の 1 回の追記、03 §6 の「組の行」) を、組の行として追記する。行が 2 つ以上
    /// なら、どの行にも同じ `grp` (組の最初の行の連番・行の数・行ごとの `put` の記録の ID) を付ける。1 行なら組にしない (1 行の追記は、行の
    /// 単位で書き切るか切り詰めるかなので、そろわないことが無い)。
    pub fn append_group(&mut self, now: Timestamp, ops: &[LineOp]) -> io::Result<u64> {
        self.append_lines(now, ops, Grouping::Group)
    }

    fn append_lines(
        &mut self,
        now: Timestamp,
        ops: &[LineOp],
        grouping: Grouping,
    ) -> io::Result<u64> {
        if let Some(reason) = &self.stopped {
            return Err(io::Error::other(format!(
                "journal appends are stopped: {reason}"
            )));
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected journal failure"));
        }
        let mut data = Vec::new();
        let mut seq = self.next_seq;
        let first = seq;
        let grouped = grouping == Grouping::Group && ops.len() >= 2;
        for (i, op) in ops.iter().enumerate() {
            let grp = grouped.then(|| Group::line(first, ops, i));
            data.extend_from_slice(&encode(seq, op, grp.as_ref()));
            seq += 1;
        }
        let name = format!("{}.jsonl", month(now));
        let path = self.dir.join(&name);
        let new = !path.exists();
        if new {
            fsutil::prepare_private_file(&path)?;
        }
        let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
        let len = f.seek(SeekFrom::End(0))?;
        let mut mended = false;
        if len > 0 {
            // 前の行が書きかけで終わっていれば、改行を補ってから書く (新しい行を壊さない)。
            f.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                data.insert(0, b'\n');
                mended = true;
            }
            f.seek(SeekFrom::End(0))?;
        }
        let written = f.write_all(&data).and_then(|()| f.sync_data());
        if let Err(e) = written {
            // 追記の前の長さに戻す (書きかけの断片を残さない)。戻せなければ、以後は追記しない (断片の後ろに同じ連番と
            // 組の ID の行を書かないため)。
            if let Err(undo) = f.set_len(len).and_then(|()| f.sync_all()) {
                self.stopped = Some(format!(
                    "a failed append could not be undone ({undo}); the next start cleans up"
                ));
            }
            return Err(e);
        }
        drop(f);
        if new {
            fsutil::sync_dir(&self.dir)?;
        }
        self.next_seq = seq;
        // 追記した `put` の行も集める (このプロセスで追記したファイルを、書き直すファイルに足す、03 §6)。
        let file = self.index.file(&name);
        self.current = Some(name);
        if mended {
            self.index.bad.insert(file);
        }
        for op in ops {
            if let LineOp::Put(e) = op {
                let rev = self
                    .monotonic
                    .get(e.kind.as_str())
                    .and_then(|field| e.body_uint(field));
                self.index.add(e.id, PutRef { file, rev });
            }
        }
        Ok(seq - 1)
    }

    /// 集めた `put` の行の、単調に増える項目の値の範囲。その ID の行が無い・宣言の無い種類なら `None`。
    pub fn rev_span(&self, id: Id) -> Option<RevSpan> {
        self.index.span(id)
    }

    /// 削除と忘れるの書き直し (03 §6 の「削除と忘れる」)。`cuts` の ID ごとに、除く `put` の行を決め、それと、
    /// チェックサムが合わずに対象の ID の文字列を含む行を除いたファイルを書いて入れ替える。
    ///
    /// - 書き直すファイルは、集めた `put` の行のあるファイル (と、チェックサムの合わない行のあるファイル) から
    ///   選ぶ。ID の時刻では選ばない。
    /// - 残す行は、読んだバイト列のまま写す。ほかの行の中に ID が文字列として現れても除かない。
    /// - 一時のファイル (`<年>-<月>.jsonl.tmp`、所有者だけの権限) を書き切ってから読み直し、残した行のバイト列と
    ///   ちょうど同じであること (行ごとの `sum`・行の数・連番の順を含む) を確かめてから入れ替える。
    /// - このプロセスが追記している今の月のファイルを入れ替えたら、追記のために開き直せることを確かめる (一時の
    ///   共有違反は少し待って試し直す)。開き直せなければ、以後の追記を止める。
    /// - 追記と直列にするため、書き手スレッドがジャーナルのロックを持ったまま呼ぶ (`&mut self`)。
    ///
    /// # Errors
    /// 1 つのファイルで失敗したら、そこで止めて返す ([`RewriteError`])。試し直してよい。
    pub fn rewrite(
        &mut self,
        cuts: &BTreeMap<Id, JournalCut>,
    ) -> Result<RewriteReport, RewriteError> {
        let mut report = RewriteReport::default();
        let mut effective: BTreeMap<Id, JournalCut> = BTreeMap::new();
        for (&id, &cut) in cuts {
            match cut {
                JournalCut::All => {
                    effective.insert(id, cut);
                }
                JournalCut::Below(rev) => {
                    if self.index.span(id).is_some_and(|s| s.max >= rev) {
                        effective.insert(id, cut);
                    } else {
                        report.skipped.push(id);
                    }
                }
            }
        }
        if effective.is_empty() {
            return Ok(report);
        }
        let mut chosen: BTreeSet<u32> = self.index.bad.clone();
        for (id, cut) in &effective {
            for r in self.index.puts.get(id).into_iter().flatten() {
                let hit = match cut {
                    JournalCut::All => true,
                    JournalCut::Below(n) => r.rev.is_none_or(|v| v < *n),
                };
                if hit {
                    chosen.insert(r.file);
                }
            }
        }
        let mut chosen: Vec<(String, u32)> = chosen
            .into_iter()
            .map(|f| (self.index.names[f as usize].clone(), f))
            .collect();
        chosen.sort();
        let needles: Vec<Vec<u8>> = effective
            .keys()
            .map(|id| id.to_string().into_bytes())
            .collect();
        let mut swapped = 0;
        for (name, file) in chosen {
            if self.rewrite_file(&name, file, &effective, &needles, swapped, &mut report)? {
                swapped += 1;
            }
        }
        Ok(report)
    }

    /// 1 つのファイルを書き直す。入れ替えたら `true`。
    fn rewrite_file(
        &mut self,
        name: &str,
        file: u32,
        cuts: &BTreeMap<Id, JournalCut>,
        needles: &[Vec<u8>],
        nth: usize,
        report: &mut RewriteReport,
    ) -> Result<bool, RewriteError> {
        let io_err = |e: io::Error| RewriteError::Io {
            file: name.to_owned(),
            reason: e.to_string(),
        };
        let path = self.dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.refresh(file, cuts, Vec::new(), false);
                return Ok(false);
            }
            Err(e) => return Err(io_err(e)),
        };
        let mut kept = Vec::with_capacity(bytes.len());
        let mut refs = Vec::new();
        let mut removed = 0;
        let mut any_bad = false;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            let l = trim_newline(raw);
            if l.iter().all(u8::is_ascii_whitespace) {
                kept.extend_from_slice(raw);
                continue;
            }
            let seen = inspect(l, &self.monotonic);
            let drop_it = match (&seen.decoded, seen.put) {
                (Decoded::Bad, _) => needles.iter().any(|n| contains(l, n)),
                (_, Some((id, rev))) => match cuts.get(&id) {
                    Some(JournalCut::All) => true,
                    Some(JournalCut::Below(n)) => match rev {
                        Some(r) => r < *n,
                        None => {
                            report.unreadable_rev += 1;
                            false
                        }
                    },
                    None => false,
                },
                _ => false,
            };
            if drop_it {
                removed += 1;
                continue;
            }
            kept.extend_from_slice(raw);
            if let Some((id, rev)) = seen.put
                && cuts.contains_key(&id)
            {
                refs.push((id, PutRef { file, rev }));
            }
            match seen.decoded {
                Decoded::Bad => any_bad = true,
                Decoded::Newer(_) => report.kept_newer += 1,
                Decoded::Line(_) => {}
            }
        }
        if removed == 0 {
            // 前の試みで入れ替えた後に止まったときも、集めた結果をファイルに合わせる。
            self.refresh(file, cuts, refs, any_bad);
            return Ok(false);
        }

        let tmp = self.dir.join(format!("{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let written = (|| -> io::Result<()> {
            fsutil::prepare_private_file(&tmp)?;
            let mut f = OpenOptions::new().write(true).truncate(true).open(&tmp)?;
            f.write_all(&kept)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        match self.fault {
            Some(RewriteFault::CorruptTemp) => {
                self.fault = None;
                let mut bad = kept.clone();
                if let Some(at) = bad.iter().position(|&b| b == b'"') {
                    bad[at] = b'\'';
                }
                std::fs::write(&tmp, &bad).map_err(io_err)?;
            }
            Some(RewriteFault::StopAfterTemp) => {
                self.fault = None;
                return Err(io_err(io::Error::other(
                    "injected stop after writing the temporary file",
                )));
            }
            _ => {}
        }
        if let Err(reason) = verify(&tmp, &kept) {
            let _ = std::fs::remove_file(&tmp);
            return Err(RewriteError::Verify {
                file: name.to_owned(),
                reason,
            });
        }
        // 追記の取っ手は追記のたびに開いて閉じるので、ここで開いたままのものは無い (入れ替えの前に閉じる)。
        let swap = match self.fault {
            Some(RewriteFault::FailSwap { nth: at }) if at == nth => {
                self.fault = None;
                Err(io::Error::other("injected failure to replace the file"))
            }
            _ => fsutil::rename_replace(&tmp, &path),
        };
        if let Err(e) = swap {
            let _ = std::fs::remove_file(&tmp);
            return Err(RewriteError::Swap {
                file: name.to_owned(),
                reason: e.to_string(),
            });
        }
        if self.fault == Some(RewriteFault::StopAfterSwap) {
            self.fault = None;
            return Err(io_err(io::Error::other(
                "injected stop after replacing the file",
            )));
        }
        // 入れ替えは済んだ。フォルダの書き切りの失敗は、入れ替えの失敗とは分けて返す (集めた結果は更新する)。
        let synced = fsutil::sync_dir(&self.dir);
        let reopened = if self.current.as_deref() == Some(name) {
            self.reopen(&path)
        } else {
            Ok(())
        };
        self.refresh(file, cuts, refs, any_bad);
        report.files += 1;
        report.removed += removed;
        if let Err(e) = reopened {
            // 追記のために開き直せない。以後は追記しない (03 §6 の「失敗」)。
            self.stopped = Some(format!("{name} could not be reopened: {e}"));
            return Err(io_err(e));
        }
        synced.map_err(io_err)?;
        Ok(true)
    }

    /// 入れ替えた今の月のファイルを、追記のために開き直せるかを確かめる。ほかのプログラムが一時的に掴んでいる
    /// ときは、rename と同じ間隔で試し直す。
    fn reopen(&mut self, path: &Path) -> io::Result<()> {
        let (mut busy, fails) = match self.fault {
            Some(RewriteFault::ReopenBusy { times }) => {
                self.fault = None;
                (times, false)
            }
            Some(RewriteFault::ReopenFails) => {
                self.fault = None;
                (0, true)
            }
            _ => (0, false),
        };
        fsutil::retry_transient(|| {
            if fails {
                return Err(io::Error::other("injected failure to reopen the file"));
            }
            if busy > 0 {
                busy -= 1;
                return Err(sharing_violation());
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map(drop)
        })
    }

    /// 書き直したファイルについて、対象の ID の集めた行を、残した行で置き換える。
    fn refresh(
        &mut self,
        file: u32,
        cuts: &BTreeMap<Id, JournalCut>,
        refs: Vec<(Id, PutRef)>,
        any_bad: bool,
    ) {
        for id in cuts.keys() {
            if let Some(v) = self.index.puts.get_mut(id) {
                v.retain(|r| r.file != file);
            }
        }
        for (id, r) in refs {
            self.index.add(id, r);
        }
        for id in cuts.keys() {
            if self.index.puts.get(id).is_some_and(Vec::is_empty) {
                self.index.puts.remove(id);
            }
        }
        if !any_bad {
            self.index.bad.remove(&file);
        }
    }

    /// 試験のための注入: 次の追記を失敗させる (AC-FR-202-1)。
    pub fn fail_next_append(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }

    /// 試験のための注入: 次の書き直しを、決まった点で失敗させる。
    pub fn inject_rewrite_fault(&mut self, fault: RewriteFault) {
        self.fault = Some(fault);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn env(n: u8, text: &str) -> Envelope {
        let mut e = Envelope::new(
            Id::from_bytes([n; 16]),
            "turn",
            1,
            Timestamp::from_millis(1_762_650_000_000),
            "0.0.4",
        );
        e.body = ciborium::Value::Map(vec![
            ("text".into(), text.into()),
            // 表記を短くすると 1 ULP ずれる値 (計算で出る値の例)。
            (
                "score".into(),
                ciborium::Value::Float(0.961_175_748_098_983_5),
            ),
        ]);
        e
    }

    fn mono() -> Monotonic {
        [("turn", "rev")].into_iter().collect()
    }

    const NOW: Timestamp = Timestamp::from_millis(1_762_650_000_000);

    fn path(dir: &TempDir) -> PathBuf {
        dir.path().join("journal/2025-11.jsonl")
    }

    #[test]
    fn appends_and_reads_back_in_order_with_exact_floats() {
        let dir = TempDir::new("journal");
        let (mut j, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert!(scan.lines.is_empty());
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "一"))]).unwrap(), 1);
        let meta = LineOp::Meta {
            key: "prompt.language".into(),
            value: "ja".into(),
        };
        assert_eq!(
            j.append(NOW, &[LineOp::Put(env(2, "二")), meta]).unwrap(),
            3
        );
        drop(j);
        let (_, scan) = Journal::open(dir.path(), 1, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(scan.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(scan.lines[0].op, LineOp::Put(env(2, "二")));
        assert_eq!((scan.bad, scan.newer), (0, 0));
        let (j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(j.next_seq, 4);
    }

    #[test]
    fn a_partial_tail_is_truncated_and_a_changed_line_is_bad() {
        let dir = TempDir::new("journal-partial");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a")), LineOp::Put(env(2, "b"))])
            .unwrap();
        drop(j);
        let mut text = std::fs::read_to_string(path(&dir)).unwrap();
        text = text.replacen("\"a\"", "\"X\"", 1);
        text.push_str("{\"seq\":10,\"op\":\"pu");
        std::fs::write(path(&dir), &text).unwrap();
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(scan.lines.len(), 1);
        assert_eq!(scan.bad, 1);
        assert_eq!(scan.truncated, "{\"seq\":10,\"op\":\"pu".len());
        assert!(std::fs::read_to_string(path(&dir)).unwrap().ends_with('\n'));
    }

    #[test]
    fn a_line_from_a_newer_version_is_counted_as_newer_and_its_sequence_is_kept() {
        let dir = TempDir::new("journal-newer");
        std::fs::create_dir_all(dir.path().join(JOURNAL_DIR)).unwrap();
        let line = seal(json!({"seq": 41, "op": "future", "env": {}}).to_string());
        std::fs::write(path(&dir), &line).unwrap();
        let (mut j, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!((scan.bad, scan.newer), (0, 1));
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).unwrap(), 42);
    }

    #[test]
    fn appending_after_a_partial_line_does_not_break_the_new_line() {
        let dir = TempDir::new("journal-append-after-partial");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a"))]).unwrap();
        let mut f = OpenOptions::new().append(true).open(path(&dir)).unwrap();
        f.write_all(b"{\"seq\":2,\"op\"").unwrap();
        drop(f);
        j.append(NOW, &[LineOp::Put(env(2, "b"))]).unwrap();
        drop(j);
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(scan.lines.len(), 2, "the new line must survive");
        assert_eq!(
            scan.bad, 1,
            "the fragment is a bad line, not a lost new line"
        );
    }

    #[test]
    fn removing_ids_drops_their_lines_keeps_the_delete_line_and_survives_bad_bytes() {
        let dir = TempDir::new("journal-remove");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(
            NOW,
            &[LineOp::Put(env(1, "消す")), LineOp::Put(env(2, "残す"))],
        )
        .unwrap();
        let mut f = OpenOptions::new().append(true).open(path(&dir)).unwrap();
        f.write_all(b"\xff\xfe broken\n").unwrap();
        drop(f);
        let gone = Id::from_bytes([1; 16]);
        j.append(
            NOW,
            &[LineOp::Delete {
                kind: "turn".into(),
                id: gone,
            }],
        )
        .unwrap();
        j.rewrite(&[(gone, JournalCut::All)].into_iter().collect())
            .unwrap();
        drop(j);
        let text = String::from_utf8_lossy(&std::fs::read(path(&dir)).unwrap()).into_owned();
        assert!(!text.contains("消す"));
        assert!(text.contains("残す"));
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert!(matches!(
            scan.lines.last().unwrap().op,
            LineOp::Delete { .. }
        ));
    }

    #[test]
    fn a_delete_whose_lines_were_not_removed_is_cleaned_up_at_startup() {
        let dir = TempDir::new("journal-cleanup");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        let gone = Id::from_bytes([1; 16]);
        j.append(NOW, &[LineOp::Put(env(1, "残ってはいけない"))])
            .unwrap();
        j.append(
            NOW,
            &[LineOp::Delete {
                kind: "turn".into(),
                id: gone,
            }],
        )
        .unwrap();
        drop(j); // 書き直しの前に止まった
        Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        let text = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!text.contains("残ってはいけない"));
    }

    #[test]
    fn an_injected_failure_does_not_advance_the_sequence() {
        let dir = TempDir::new("journal-fail");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.fail_next_append();
        assert!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).is_err());
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).unwrap(), 1);
    }

    /// verifies: 03 §6 (組の行がそろわない組は、`repair` の読み方でも当てない。1 行の組は作らない)
    #[test]
    fn groups_missing_a_line_are_left_out_of_every_reading() {
        let dir = TempDir::new("journal-groups");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append_group(NOW, &[LineOp::Put(env(1, "a")), LineOp::Put(env(2, "b"))])
            .unwrap();
        j.append_group(NOW, &[LineOp::Put(env(3, "1 行"))]).unwrap();
        j.append_group(NOW, &[LineOp::Put(env(4, "c")), LineOp::Put(env(5, "d"))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(env(6, "e"))]).unwrap();
        drop(j);
        let all = std::fs::read_to_string(path(&dir)).unwrap();
        let lines: Vec<&str> = all.lines().collect();
        // `ids` は組の最初の行だけが持つ。1 行の追記は組にしない。
        assert!(lines[0].contains("\"ids\":[") && !lines[1].contains("\"ids\""));
        assert!(!lines[2].contains("\"grp\""));
        // 最初の組の 2 行目を、理由なく失う。
        let kept: String = all
            .split_inclusive('\n')
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, l)| l)
            .collect();
        std::fs::write(path(&dir), kept).unwrap();
        // 行の欠けた組も、読める行を当てる (どの読み方でも)。
        let read = read_all(dir.path()).unwrap();
        assert_eq!(read.bad, 0);
        assert_eq!(
            read.lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
            [1, 3, 4, 5, 6]
        );
        assert_eq!(
            (read.incomplete_groups, read.partial.as_slice()),
            (1, &[Id::from_bytes([1; 16])][..])
        );
        let s = summarize(dir.path(), 0, Tail::Uncommitted).unwrap();
        assert_eq!((s.incomplete_groups, s.torn_groups, s.pending), (1, 0, 5));
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!((scan.incomplete_groups, scan.torn_groups), (1, 0));
        assert_eq!(scan.lines.len(), 5);
        assert_eq!(scan.partial, [Id::from_bytes([1; 16])]);
    }

    #[test]
    fn months_are_named_in_utc_and_clamped() {
        assert_eq!(month(NOW), "2025-11");
        assert_eq!(month(Timestamp::from_millis(i64::MIN)), "0001-01");
        assert_eq!(month(Timestamp::from_millis(i64::MAX)), "9999-12");
    }

    // --- 削除と忘れるの書き直し (03 §6 の「削除と忘れる」) ---

    const LATER: Timestamp = Timestamp::from_millis(1_765_000_000_000); // 2025-12

    fn turn(n: u8, text: &str, rev: u64) -> Envelope {
        let mut e = env(n, text);
        if let ciborium::Value::Map(m) = &mut e.body {
            m.push(("rev".into(), rev.into()));
        }
        e
    }

    fn with_field(mut e: Envelope, name: &str, value: &str) -> Envelope {
        if let ciborium::Value::Map(m) = &mut e.body {
            m.push((name.into(), value.into()));
        }
        e
    }

    fn cut(id: Id, c: JournalCut) -> BTreeMap<Id, JournalCut> {
        [(id, c)].into_iter().collect()
    }

    /// ファイルの行 (改行を含む、書いたとおりのバイト列)。
    fn raw_lines(p: &Path) -> Vec<Vec<u8>> {
        std::fs::read(p)
            .unwrap()
            .split_inclusive(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    }

    fn push_raw(p: &Path, line: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(line).unwrap();
    }

    fn has(line: &[u8], text: &str) -> bool {
        contains(line, text.as_bytes())
    }

    fn tmp_files(dir: &TempDir) -> Vec<String> {
        std::fs::read_dir(dir.path().join(JOURNAL_DIR))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(TMP_SUFFIX))
            .collect()
    }

    /// 削除の書き直しは、封筒の `id` が対象の `put` の行 (新しい版の行も) と、チェックサムが合わずに ID を
    /// 含む行だけを除く。`reply_to`・`target` に ID を持つ行、削除の行、新しい版の別の操作の行は、読んだ
    /// バイト列のまま残る。
    /// verifies: 03 §6 (削除と忘れるの除く行)
    #[test]
    fn a_delete_rewrite_removes_exactly_the_put_lines_of_the_id() {
        let dir = TempDir::new("journal-rewrite-delete");
        let gone = Id::from_bytes([1; 16]);
        let g = gone.to_string();
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "消す本文"))]).unwrap();
        j.append(
            NOW,
            &[LineOp::Put(with_field(env(2, "返事"), "reply_to", &g))],
        )
        .unwrap();
        drop(j);
        let p = path(&dir);
        // 新しい版の `put` の行 (封筒は読めないが `env.id` は読める)、新しい版の別の操作の行、壊れた行。
        let newer_put = json!({"seq": 10, "op": "put", "env": {
            "id": g, "kind": "turn", "created_at": "x", "body": {"text": "新しい版の本文"}
        }});
        push_raw(&p, &seal(newer_put.to_string()));
        let newer_op = json!({"seq": 11, "op": "future", "env": {"target": g}});
        push_raw(&p, &seal(newer_op.to_string()));
        let broken = format!(
            "{{\"seq\":12,\"op\":\"put\",\"env\":{{\"id\":\"{g}\",\"body\":{{\"text\":\"壊れた本文\"}}}},\"sum\":\"0\"}}\n"
        );
        push_raw(&p, broken.as_bytes());
        push_raw(&p, b"{\"seq\":13,\"broken\":true,\"sum\":\"0\"}\n");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(
            NOW,
            &[LineOp::Delete {
                kind: "turn".into(),
                id: gone,
            }],
        )
        .unwrap();
        let before = raw_lines(&p);
        let report = j.rewrite(&cut(gone, JournalCut::All)).unwrap();
        assert_eq!((report.files, report.removed, report.kept_newer), (1, 3, 1));
        let after = raw_lines(&p);
        let expected: Vec<Vec<u8>> = before
            .iter()
            .filter(|l| !has(l, "消す本文") && !has(l, "新しい版の本文") && !has(l, "壊れた本文"))
            .cloned()
            .collect();
        assert_eq!(after, expected, "kept lines are copied byte for byte");
        assert!(after.iter().any(|l| has(l, "返事")));
        assert!(after.iter().any(|l| has(l, "\"future\"")));
        assert!(after.iter().any(|l| has(l, "\"broken\"")));
        assert!(after.iter().any(|l| has(l, "\"delete\"")));
        assert!(tmp_files(&dir).is_empty());
        // 開き直しても、もう除く行は無い。
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(scan.rewrite_failed, None);
        assert_eq!(raw_lines(&p), expected);
    }

    /// 忘れるの書き直しは、対象のターンの、改訂の番号が伏せ字の後の番号より小さい `put` の行 (番号の無い行は
    /// 0) を、行の版に関わらず除き、伏せ字の後の行とその後の状態の行を残す。番号を読めない行は残して数える。
    /// verifies: 03 §6 (削除と忘れるの除く行)、04 §7.2 (ジャーナルの層)
    #[test]
    fn a_forget_rewrite_removes_only_lines_below_the_revision() {
        let dir = TempDir::new("journal-rewrite-forget");
        let id = Id::from_bytes([1; 16]);
        let i = id.to_string();
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "伏せる前・番号なし"))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前・確定", 0))])
            .unwrap();
        j.append(
            NOW,
            &[LineOp::Put(with_field(
                turn(2, "別のターン", 0),
                "reply_to",
                &i,
            ))],
        )
        .unwrap();
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 0, max: 0 }));
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]・後", 1))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]・状態", 1))])
            .unwrap();
        drop(j);
        let p = path(&dir);
        let newer_before = json!({"seq": 20, "op": "put", "env": {
            "id": i, "kind": "turn", "v": "new", "body": {"rev": 0, "text": "伏せる前・新しい版"}
        }});
        push_raw(&p, &seal(newer_before.to_string()));
        let odd_rev = json!({"seq": 21, "op": "put", "env": {
            "id": i, "kind": "turn", "v": "new", "body": {"rev": "x", "text": "番号を読めない"}
        }});
        push_raw(&p, &seal(odd_rev.to_string()));
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 0, max: 1 }));
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!(report.removed, 3);
        assert_eq!(report.unreadable_rev, 1);
        assert!(report.skipped.is_empty());
        let after = raw_lines(&p);
        assert!(!after.iter().any(|l| has(l, "伏せる前")));
        for kept in [
            "別のターン",
            "[伏せ字]・後",
            "[伏せ字]・状態",
            "番号を読めない",
        ] {
            assert!(after.iter().any(|l| has(l, kept)), "{kept} must be kept");
        }
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 1, max: 1 }));
        // 何度当てても同じ。
        let again = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!((again.files, again.removed), (0, 0));
        assert_eq!(raw_lines(&p), after);
    }

    /// 伏せ字の後の行がジャーナルに無ければ、前の行を除かない (04 §7.2 の「ジャーナルの層」)。
    /// verifies: 04 §7.2 (ジャーナルの層)
    #[test]
    fn a_forget_rewrite_keeps_everything_without_a_line_at_the_revision() {
        let dir = TempDir::new("journal-rewrite-no-after");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        let before = raw_lines(&path(&dir));
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!(report.skipped, vec![id]);
        assert_eq!(report.removed, 0);
        assert_eq!(raw_lines(&path(&dir)), before);
    }

    /// 書いた一時のファイルが読み直しの検査に合わなければ、入れ替えずに元のファイルを残し、一時のファイルを
    /// 消す。
    /// verifies: 03 §6 (書き直し方)
    #[test]
    fn a_rewrite_that_does_not_verify_leaves_the_file_as_it_was() {
        let dir = TempDir::new("journal-rewrite-verify");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        let before = raw_lines(&path(&dir));
        j.inject_rewrite_fault(RewriteFault::CorruptTemp);
        let err = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap_err();
        assert!(matches!(err, RewriteError::Verify { .. }), "{err:?}");
        assert!(!err.is_swap());
        assert_eq!(raw_lines(&path(&dir)), before);
        assert!(tmp_files(&dir).is_empty());
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 0, max: 1 }));
        // 試し直せば済む。
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!(report.removed, 1);
    }

    /// 入れ替えに失敗したら (Windows でほかのプロセスが開いている)、試し直せる失敗を返し、元のファイルを残す。
    /// 試し直すと済む。
    /// verifies: 03 §6 (失敗)
    #[test]
    fn a_failed_swap_is_retryable_and_the_retry_finishes() {
        let dir = TempDir::new("journal-rewrite-swap");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        let before = raw_lines(&path(&dir));
        j.inject_rewrite_fault(RewriteFault::FailSwap { nth: 0 });
        let err = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap_err();
        assert!(err.is_swap(), "{err:?}");
        assert_eq!(raw_lines(&path(&dir)), before);
        assert!(tmp_files(&dir).is_empty());
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!((report.files, report.removed), (1, 1));
        // 追記は書き直したファイルに続く。
        let state = turn(1, "[伏せ字]・状態", 1);
        assert_eq!(j.append(NOW, &[LineOp::Put(state)]).unwrap(), 3);
        assert_eq!(raw_lines(&path(&dir)).len(), 2);
    }

    /// 一時のファイルを書いた後、入れ替えの前に止まったとき: 一時のファイルは所有者だけの権限で、次に開くときに
    /// 消え、元のファイルはそのまま残る。開き直した後の書き直しで済む。
    /// verifies: 03 §6 (書き直し方)、09 §3.5 (ジャーナルの書き直しの途中)
    #[test]
    fn a_stop_after_the_temporary_file_leaves_the_original_and_the_tmp_is_removed_at_open() {
        let dir = TempDir::new("journal-rewrite-stop-tmp");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        let before = raw_lines(&path(&dir));
        j.inject_rewrite_fault(RewriteFault::StopAfterTemp);
        assert!(j.rewrite(&cut(id, JournalCut::Below(1))).is_err());
        drop(j);
        assert_eq!(tmp_files(&dir), vec!["2025-11.jsonl.tmp".to_owned()]);
        let tmp = dir.path().join(JOURNAL_DIR).join("2025-11.jsonl.tmp");
        if fsutil::acl_applicable(&tmp).unwrap() {
            assert!(
                fsutil::is_private(&tmp).unwrap(),
                "the temporary file is owner-only"
            );
        }
        assert_eq!(raw_lines(&path(&dir)), before);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert!(
            tmp_files(&dir).is_empty(),
            "a leftover tmp is removed at open"
        );
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 0, max: 1 }));
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!(report.removed, 1);
    }

    /// 入れ替えた後、集めた結果を更新する前に止まったとき: 次に開くと、入れ替えたファイルから集め直し、
    /// 書き直しは済んでいて、追記も続けられる。
    /// verifies: 09 §3.5 (入れ替えの後、追記の取っ手を開き直す前)
    #[test]
    fn a_stop_after_the_swap_opens_fine_next_time() {
        let dir = TempDir::new("journal-rewrite-stop-swap");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        j.inject_rewrite_fault(RewriteFault::StopAfterSwap);
        assert!(j.rewrite(&cut(id, JournalCut::Below(1))).is_err());
        drop(j);
        let (mut j, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!((scan.bad, scan.lines.len()), (0, 1));
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 1, max: 1 }));
        assert_eq!(j.append(NOW, &[LineOp::Put(env(2, "次"))]).unwrap(), 3);
        assert!(tmp_files(&dir).is_empty());
    }

    /// 対象の行が 2 つの月のファイルにあり、2 つ目の入れ替えの前に止まったとき: 1 つ目は書き直したまま残り、
    /// 次に開いたときに集めた値の範囲で済んでいないことが分かり、書き直しを当て直すと済む。書き直すファイルは、
    /// ID の時刻ではなく集めた行で選ぶ (後の月に足した行も入る)。
    /// verifies: 03 §6 (書き直すファイル)、09 §3.5 (ジャーナルの書き直しの途中)
    #[test]
    fn a_partly_rewritten_set_of_months_is_finished_after_reopening() {
        let dir = TempDir::new("journal-rewrite-months");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前・11月", 0))])
            .unwrap();
        // 古い版が後の月に足した、同じターンの伏せ字の前の行。
        j.append(LATER, &[LineOp::Put(turn(1, "伏せる前・12月", 0))])
            .unwrap();
        j.append(LATER, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        let december = dir.path().join("journal/2025-12.jsonl");
        j.inject_rewrite_fault(RewriteFault::FailSwap { nth: 1 });
        let err = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap_err();
        assert!(err.is_swap(), "{err:?}");
        assert!(!raw_lines(&path(&dir)).iter().any(|l| has(l, "伏せる前")));
        assert!(
            raw_lines(&december)
                .iter()
                .any(|l| has(l, "伏せる前・12月"))
        );
        drop(j);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        let span = j.rev_span(id).unwrap();
        assert!(span.min < 1 && 1 <= span.max, "unfinished: {span:?}");
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!((report.files, report.removed), (1, 1));
        assert!(!raw_lines(&december).iter().any(|l| has(l, "伏せる前")));
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 1, max: 1 }));
    }

    /// 元のファイルが無いのに一時のファイルが残っていれば (検査を通った後の入れ替えの途中で止まった)、元の名前に
    /// 戻して読む。元のファイルがあれば、一時のファイルを消す。
    /// verifies: 03 §6 (書き直し方)、09 §3.5 (ジャーナルの書き直しの途中)
    #[test]
    fn a_leftover_tmp_is_renamed_back_only_when_the_original_is_missing() {
        let dir = TempDir::new("journal-tmp-recover");
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "一"))]).unwrap();
        j.append(LATER, &[LineOp::Put(env(2, "二"))]).unwrap();
        drop(j);
        let jdir = dir.path().join(JOURNAL_DIR);
        // 11 月: 元のファイルが無く、一時のファイルだけがある。
        std::fs::rename(jdir.join("2025-11.jsonl"), jdir.join("2025-11.jsonl.tmp")).unwrap();
        // 12 月: 元のファイルも一時のファイルもある。
        std::fs::write(jdir.join("2025-12.jsonl.tmp"), b"stale").unwrap();
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert!(tmp_files(&dir).is_empty());
        assert_eq!(scan.lines.len(), 2, "the November line is read back");
        assert!(raw_lines(&path(&dir)).iter().any(|l| has(l, "一")));
        assert!(
            !std::fs::read(jdir.join("2025-12.jsonl"))
                .unwrap()
                .starts_with(b"stale")
        );
    }

    /// 削除の書き直しが 2 つ目の月の入れ替えの前に止まり、1 つ目の月の `put` の行は除いたが、2 つ目の月に
    /// 削除した ID を含む壊れた行が残ったとき: 次に開くと、正しい `put` の行がもう無くても、その壊れた行を除く。
    /// verifies: 03 §6 (除く行の (a)・除く前に止まったとき)
    #[test]
    fn a_bad_line_naming_a_deleted_id_is_removed_at_the_next_open() {
        let dir = TempDir::new("journal-bad-after-partial");
        let gone = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "消す本文"))]).unwrap();
        j.append(LATER, &[LineOp::Put(env(2, "残す"))]).unwrap();
        drop(j);
        let december = dir.path().join("journal/2025-12.jsonl");
        let broken = format!(
            "{{\"seq\":9,\"op\":\"put\",\"env\":{{\"id\":\"{gone}\",\"body\":{{\"text\":\"壊れた本文\"}}}},\"sum\":\"0\"}}\n"
        );
        push_raw(&december, broken.as_bytes());
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(
            LATER,
            &[LineOp::Delete {
                kind: "turn".into(),
                id: gone,
            }],
        )
        .unwrap();
        j.inject_rewrite_fault(RewriteFault::FailSwap { nth: 1 });
        let err = j.rewrite(&cut(gone, JournalCut::All)).unwrap_err();
        assert!(err.is_swap(), "{err:?}");
        assert!(!raw_lines(&path(&dir)).iter().any(|l| has(l, "消す本文")));
        assert!(raw_lines(&december).iter().any(|l| has(l, "壊れた本文")));
        drop(j);
        let (_, scan) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        assert_eq!(scan.rewrite_failed, None);
        let after = raw_lines(&december);
        assert!(!after.iter().any(|l| has(l, "壊れた本文")));
        assert!(after.iter().any(|l| has(l, "残す")));
        assert!(after.iter().any(|l| has(l, "\"delete\"")));
    }

    /// 今の月のファイルを入れ替えた後に開き直せなければ、以後の追記を止める。今の月でないファイルは開き直さない。
    /// verifies: 03 §6 (追記との直列・失敗)
    #[test]
    fn appends_stop_when_the_current_month_cannot_be_reopened() {
        let dir = TempDir::new("journal-reopen-fails");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        let other = Id::from_bytes([3; 16]);
        j.append(NOW, &[LineOp::Put(turn(3, "11月だけ", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(3, "[伏せ字]", 1))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前・11月", 0))])
            .unwrap();
        j.append(LATER, &[LineOp::Put(turn(1, "伏せる前・12月", 0))])
            .unwrap();
        j.append(LATER, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        // 今の月 (最後に追記したファイル) は 12 月。11 月だけを書き直すときは開き直さない (注入は使われない)。
        j.inject_rewrite_fault(RewriteFault::ReopenFails);
        assert_eq!(
            j.rewrite(&cut(other, JournalCut::Below(1))).unwrap().files,
            1
        );
        // 12 月を入れ替えると開き直す。開き直せなければ止まる。
        let err = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap_err();
        assert!(matches!(err, RewriteError::Io { .. }), "{err:?}");
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 1, max: 1 }));
        assert!(j.append(LATER, &[LineOp::Put(env(6, "x"))]).is_err());
    }

    /// 開き直しの一時の共有違反は、少し待って試し直す (Windows)。
    /// verifies: 03 §6 (失敗)
    #[cfg(windows)]
    #[test]
    fn a_transient_reopen_failure_is_retried() {
        let dir = TempDir::new("journal-reopen-busy");
        let id = Id::from_bytes([1; 16]);
        let (mut j, _) = Journal::open(dir.path(), 0, mono(), Tail::Uncommitted).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        j.inject_rewrite_fault(RewriteFault::ReopenBusy { times: 2 });
        assert_eq!(
            j.rewrite(&cut(id, JournalCut::Below(1))).unwrap().removed,
            1
        );
        assert_eq!(j.append(NOW, &[LineOp::Put(env(2, "次"))]).unwrap(), 3);
    }
}
