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

/// 読んだ行。
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub seq: u64,
    pub op: LineOp,
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

fn encode(seq: u64, op: &LineOp) -> Vec<u8> {
    let (name, env) = match op {
        LineOp::Put(e) => ("put", envelope_to_json(e)),
        LineOp::Delete { kind, id } => ("delete", json!({"kind": kind, "id": id.to_string()})),
        LineOp::Meta { key, value } => ("meta", json!({"key": key, "value": value})),
    };
    seal(json!({"seq": seq, "op": name, "env": env}).to_string())
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
    match op {
        Some(op) => Decoded::Line(Line { seq, op }),
        None => Decoded::Newer(Some(seq)),
    }
}

fn decode(line: &[u8]) -> Decoded {
    let Some(body) = verified_body(line) else {
        return Decoded::Bad;
    };
    match serde_json::from_slice::<Json>(&body) {
        Ok(v) => decode_value(&v),
        Err(_) => Decoded::Newer(None),
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

/// 読み直しの検査で比べる、行の形。
#[derive(Debug, PartialEq, Eq)]
enum Shape {
    Blank,
    /// チェックサムが合わない。
    Bad,
    /// チェックサムが合う (分かれば連番)。
    Valid(Option<u64>),
}

fn shape(raw: &[u8]) -> Shape {
    let l = trim_newline(raw);
    if l.iter().all(u8::is_ascii_whitespace) {
        return Shape::Blank;
    }
    match verified_body(l) {
        None => Shape::Bad,
        Some(body) => Shape::Valid(
            serde_json::from_slice::<Json>(&body)
                .ok()
                .and_then(|v| v.get("seq").and_then(Json::as_u64)),
        ),
    }
}

/// 書き直した一時のファイルを読み直し、長さ・行の数・行ごとの `sum`・連番の順が、残した行と合うかを確かめる。
fn verify(tmp: &Path, len: usize, want: &[Shape]) -> Result<(), String> {
    let got = std::fs::read(tmp).map_err(|e| e.to_string())?;
    if got.len() != len {
        return Err(format!("{len} bytes written, {} read back", got.len()));
    }
    let shapes: Vec<Shape> = got.split_inclusive(|&b| b == b'\n').map(shape).collect();
    if shapes.len() != want.len() {
        return Err(format!(
            "{} lines kept, {} read back",
            want.len(),
            shapes.len()
        ));
    }
    match shapes.iter().zip(want).position(|(g, w)| g != w) {
        Some(at) => Err(format!(
            "line {}: expected {:?}, read {:?}",
            at + 1,
            want[at],
            shapes[at]
        )),
        None => Ok(()),
    }
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
    /// `last_seq` より後の行 (次の起動で取り込む)。
    pub pending: usize,
}

/// ジャーナルを、何も書かずに数える (`doctor`、AC-FR-207-1)。
pub fn summarize(root: &Path, last_seq: u64) -> io::Result<Summary> {
    let mut s = Summary::default();
    for path in files(&root.join(JOURNAL_DIR))? {
        s.files += 1;
        let bytes = std::fs::read(&path)?;
        let keep = if bytes.is_empty() || bytes.last() == Some(&b'\n') {
            bytes.len()
        } else {
            bytes.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1)
        };
        s.partial += bytes.len() - keep;
        for raw in bytes[..keep].split_inclusive(|&b| b == b'\n') {
            let l = trim_newline(raw);
            if l.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match decode(l) {
                Decoded::Line(line) => {
                    s.lines += 1;
                    s.max_seq = s.max_seq.max(line.seq);
                    if line.seq > last_seq {
                        s.pending += 1;
                    }
                }
                Decoded::Newer(seq) => {
                    s.newer += 1;
                    s.max_seq = s.max_seq.max(seq.unwrap_or(0));
                }
                Decoded::Bad => s.bad += 1,
            }
        }
    }
    Ok(s)
}

/// 書き換えずに、すべての `meta` の行を連番の順に集める (読むだけで開いたとき、03 §3.1.1)。
pub fn scan_metas(root: &Path) -> io::Result<Vec<(String, String)>> {
    let dir = root.join(JOURNAL_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut metas: Vec<(u64, String, String)> = Vec::new();
    for path in files(&dir)? {
        let bytes = std::fs::read(&path)?;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            if raw.last() != Some(&b'\n') {
                continue; // 書きかけの末尾
            }
            if let Decoded::Line(line) = decode(trim_newline(raw))
                && let LineOp::Meta { key, value } = line.op
            {
                metas.push((line.seq, key, value));
            }
        }
    }
    metas.sort_by_key(|m| m.0);
    Ok(metas.into_iter().map(|(_, k, v)| (k, v)).collect())
}

impl Journal {
    /// 開いて読む。`last_seq` より大きい行を返す。書きかけの末尾を切り詰め、残っている一時のファイルを消し、
    /// 削除の後始末をする。`monotonic` は、種類ごとの単調に増える項目 (集める値)。
    pub fn open(root: &Path, last_seq: u64, monotonic: Monotonic) -> io::Result<(Self, Scan)> {
        let dir = root.join(JOURNAL_DIR);
        fsutil::create_private_dir(&dir)?;
        // 入れ替えの前に止まった一時のファイルは要らない (元のファイルが残っている)。
        for e in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
            if e.file_name().to_string_lossy().ends_with(TMP_SUFFIX) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let mut scan = Scan::default();
        let mut max_seq = last_seq;
        let mut deleted: BTreeMap<Id, String> = BTreeMap::new();
        let mut index = PutIndex::default();
        let mut metas: Vec<(u64, String, String)> = Vec::new();
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
            for raw in bytes.split_inclusive(|&b| b == b'\n') {
                let l = trim_newline(raw);
                if l.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let seen = inspect(l, &monotonic);
                if let Some((id, rev)) = seen.put {
                    index.add(id, PutRef { file, rev });
                }
                match seen.decoded {
                    Decoded::Line(line) => {
                        max_seq = max_seq.max(line.seq);
                        match &line.op {
                            LineOp::Delete { id, kind } => {
                                deleted.insert(*id, kind.clone());
                            }
                            LineOp::Put(_) => {}
                            LineOp::Meta { key, value } => {
                                metas.push((line.seq, key.clone(), value.clone()));
                            }
                        }
                        if line.seq > last_seq {
                            scan.lines.push(line);
                        }
                    }
                    Decoded::Newer(seq) => {
                        scan.newer += 1;
                        if let Some(s) = seq {
                            max_seq = max_seq.max(s);
                        }
                    }
                    Decoded::Bad => {
                        scan.bad += 1;
                        index.bad.insert(file);
                    }
                }
            }
        }
        scan.lines.sort_by_key(|l| l.seq);
        metas.sort_by_key(|m| m.0);
        scan.metas = metas.into_iter().map(|(_, k, v)| (k, v)).collect();
        let mut journal = Self {
            dir,
            next_seq: max_seq + 1,
            monotonic,
            index,
            stopped: None,
            fail_next: AtomicBool::new(false),
            fault: None,
        };
        // 削除の行があるのに `put` の行が残っていれば、除く (除く前に止まった、03 §6)。入れ替えられなくても
        // 開くのは止めない (次に開くときに、もう一度試す)。
        let leftover: BTreeMap<Id, JournalCut> = deleted
            .keys()
            .filter(|id| journal.index.puts.contains_key(*id))
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
        for op in ops {
            data.extend_from_slice(&encode(seq, op));
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
            // 追記の前の長さに戻す (書きかけの断片を残さない)。
            let _ = f.set_len(len).and_then(|()| f.sync_all());
            return Err(e);
        }
        drop(f);
        if new {
            fsutil::sync_dir(&self.dir)?;
        }
        self.next_seq = seq;
        // 追記した `put` の行も集める (このプロセスで追記したファイルを、書き直すファイルに足す、03 §6)。
        let file = self.index.file(&name);
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
    /// - 一時のファイル (`<年>-<月>.jsonl.tmp`、所有者だけの権限) を書き切ってから読み直し、行ごとの `sum`・
    ///   行の数・連番の順を確かめてから入れ替える。
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
        let mut shapes = Vec::new();
        let mut refs = Vec::new();
        let mut removed = 0;
        let mut any_bad = false;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            let l = trim_newline(raw);
            if l.iter().all(u8::is_ascii_whitespace) {
                kept.extend_from_slice(raw);
                shapes.push(Shape::Blank);
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
            shapes.push(match seen.decoded {
                Decoded::Bad => {
                    any_bad = true;
                    Shape::Bad
                }
                Decoded::Newer(seq) => {
                    report.kept_newer += 1;
                    Shape::Valid(seq)
                }
                Decoded::Line(line) => Shape::Valid(Some(line.seq)),
            });
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
        if let Err(reason) = verify(&tmp, kept.len(), &shapes) {
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
            _ => fsutil::rename_durable(&tmp, &path),
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
        // 追記のために開き直せることを確かめる。開き直せなければ、以後は追記しない (03 §6)。
        if let Err(e) = OpenOptions::new().read(true).write(true).open(&path) {
            self.stopped = Some(format!("{name} could not be reopened: {e}"));
            return Err(io_err(e));
        }
        self.refresh(file, cuts, refs, any_bad);
        report.files += 1;
        report.removed += removed;
        Ok(true)
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
        let (mut j, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (_, scan) = Journal::open(dir.path(), 1, mono()).unwrap();
        assert_eq!(scan.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(scan.lines[0].op, LineOp::Put(env(2, "二")));
        assert_eq!((scan.bad, scan.newer), (0, 0));
        let (j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        assert_eq!(j.next_seq, 4);
    }

    #[test]
    fn a_partial_tail_is_truncated_and_a_changed_line_is_bad() {
        let dir = TempDir::new("journal-partial");
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a")), LineOp::Put(env(2, "b"))])
            .unwrap();
        drop(j);
        let mut text = std::fs::read_to_string(path(&dir)).unwrap();
        text = text.replacen("\"a\"", "\"X\"", 1);
        text.push_str("{\"seq\":10,\"op\":\"pu");
        std::fs::write(path(&dir), &text).unwrap();
        let (_, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
        assert_eq!((scan.bad, scan.newer), (0, 1));
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).unwrap(), 42);
    }

    #[test]
    fn appending_after_a_partial_line_does_not_break_the_new_line() {
        let dir = TempDir::new("journal-append-after-partial");
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a"))]).unwrap();
        let mut f = OpenOptions::new().append(true).open(path(&dir)).unwrap();
        f.write_all(b"{\"seq\":2,\"op\"").unwrap();
        drop(f);
        j.append(NOW, &[LineOp::Put(env(2, "b"))]).unwrap();
        drop(j);
        let (_, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
        assert_eq!(scan.lines.len(), 2, "the new line must survive");
        assert_eq!(
            scan.bad, 1,
            "the fragment is a bad line, not a lost new line"
        );
    }

    #[test]
    fn removing_ids_drops_their_lines_keeps_the_delete_line_and_survives_bad_bytes() {
        let dir = TempDir::new("journal-remove");
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (_, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
        assert!(matches!(
            scan.lines.last().unwrap().op,
            LineOp::Delete { .. }
        ));
    }

    #[test]
    fn a_delete_whose_lines_were_not_removed_is_cleaned_up_at_startup() {
        let dir = TempDir::new("journal-cleanup");
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        Journal::open(dir.path(), 0, mono()).unwrap();
        let text = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!text.contains("残ってはいけない"));
    }

    #[test]
    fn an_injected_failure_does_not_advance_the_sequence() {
        let dir = TempDir::new("journal-fail");
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        j.fail_next_append();
        assert!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).is_err());
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).unwrap(), 1);
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (_, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "伏せる前", 0))])
            .unwrap();
        j.append(NOW, &[LineOp::Put(turn(1, "[伏せ字]", 1))])
            .unwrap();
        j.inject_rewrite_fault(RewriteFault::StopAfterSwap);
        assert!(j.rewrite(&cut(id, JournalCut::Below(1))).is_err());
        drop(j);
        let (mut j, scan) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
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
        let (mut j, _) = Journal::open(dir.path(), 0, mono()).unwrap();
        let span = j.rev_span(id).unwrap();
        assert!(span.min < 1 && 1 <= span.max, "unfinished: {span:?}");
        let report = j.rewrite(&cut(id, JournalCut::Below(1))).unwrap();
        assert_eq!((report.files, report.removed), (1, 1));
        assert!(!raw_lines(&december).iter().any(|l| has(l, "伏せる前")));
        assert_eq!(j.rev_span(id), Some(RevSpan { min: 1, max: 1 }));
    }
}
