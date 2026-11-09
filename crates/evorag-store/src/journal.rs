//! 会話のジャーナル (03 §6、ADR-0006)。
//!
//! - 置き場所は `journal/<年>-<月>.jsonl` (UTC)。1 行は `{"env": …, "op": …, "seq": N, "sum": "…"}`。
//!   `sum` は最後の項目で、行の `,"sum":"…"}` を `}` に置き換えた、書いたとおりのバイト列の xxh3-64。
//! - 操作は `put` (封筒)、`delete` (`{kind, id}`)、`meta` (`{key, value}`)。知らない操作の行は新しい版の
//!   行として数え、連番だけは数に入れる。
//! - 起動時に、ファイルの最後の行が改行で終わらなければ (書きかけ) その行だけを切り詰める。チェック
//!   サムが合わない行は読み飛ばして数える (読めない行は切り詰めない)。
//! - 削除の行がある ID の `put` の行が残っていれば (除く前に止まった)、起動時に除く。

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use evorag_types::{Id, Timestamp};
use serde_json::{Value as Json, json};

use crate::envelope::Envelope;
use crate::fsutil;
use crate::json::{envelope_from_json, envelope_to_json};

pub const JOURNAL_DIR: &str = "journal";
const SUM_MARK: &[u8] = b",\"sum\":\"";

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
}

pub struct Journal {
    dir: PathBuf,
    next_seq: u64,
    /// 試験のための注入: 次の追記を失敗させる。
    fail_next: AtomicBool,
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

fn decode(line: &[u8]) -> Decoded {
    let Some(body) = verified_body(line) else {
        return Decoded::Bad;
    };
    let Ok(v) = serde_json::from_slice::<Json>(&body) else {
        return Decoded::Newer(None);
    };
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

impl Journal {
    /// 開いて読む。`last_seq` より大きい行を返す。書きかけの末尾を切り詰め、削除の後始末をする。
    pub fn open(root: &Path, last_seq: u64) -> io::Result<(Self, Scan)> {
        let dir = root.join(JOURNAL_DIR);
        fsutil::create_private_dir(&dir)?;
        // 入れ替えの前に止まった一時のファイルは要らない (元のファイルが残っている)。
        for e in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
            if e.path().to_string_lossy().ends_with(".jsonl.tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let mut scan = Scan::default();
        let mut max_seq = last_seq;
        let mut deleted = BTreeSet::new();
        let mut put_ids = BTreeSet::new();
        for path in files(&dir)? {
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
                match decode(l) {
                    Decoded::Line(line) => {
                        max_seq = max_seq.max(line.seq);
                        match &line.op {
                            LineOp::Delete { id, .. } => {
                                deleted.insert(*id);
                            }
                            LineOp::Put(e) => {
                                put_ids.insert(e.id);
                            }
                            LineOp::Meta { .. } => {}
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
                    Decoded::Bad => scan.bad += 1,
                }
            }
        }
        scan.lines.sort_by_key(|l| l.seq);
        let mut journal = Self {
            dir,
            next_seq: max_seq + 1,
            fail_next: AtomicBool::new(false),
        };
        // 削除の行があるのに本文の行が残っていれば、除く (除く前に止まった、03 §6)。
        let leftover: BTreeSet<Id> = deleted.intersection(&put_ids).copied().collect();
        journal.remove_ids(&leftover)?;
        Ok((journal, scan))
    }

    /// 行を追記して書き切る。最後の行の連番を返す。失敗したらファイルを追記の前の長さに戻し、
    /// 連番を進めない。
    pub fn append(&mut self, now: Timestamp, ops: &[LineOp]) -> io::Result<u64> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected journal failure"));
        }
        let mut data = Vec::new();
        let mut seq = self.next_seq;
        for op in ops {
            data.extend_from_slice(&encode(seq, op));
            seq += 1;
        }
        let path = self.dir.join(format!("{}.jsonl", month(now)));
        let new = !path.exists();
        if new {
            fsutil::prepare_private_file(&path)?;
        }
        let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
        let len = f.seek(SeekFrom::End(0))?;
        if len > 0 {
            // 前の行が書きかけで終わっていれば、改行を補ってから書く (新しい行を壊さない)。
            f.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                data.insert(0, b'\n');
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
        Ok(seq - 1)
    }

    /// 対象の ID の `put` の行 (と、対象の ID を含む読めない行) を除いたファイルを書いて入れ替える。
    /// 削除の行は残す。バイト単位で扱う (不正な UTF-8 があっても止まらない)。
    pub fn remove_ids(&mut self, ids: &BTreeSet<Id>) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let needles: Vec<Vec<u8>> = ids.iter().map(|id| id.to_string().into_bytes()).collect();
        for path in files(&self.dir)? {
            let bytes = std::fs::read(&path)?;
            let mut kept = Vec::with_capacity(bytes.len());
            let mut removed = false;
            for raw in bytes.split_inclusive(|&b| b == b'\n') {
                let l = trim_newline(raw);
                let drop_it = needles.iter().any(|n| contains(l, n))
                    && match decode(l) {
                        Decoded::Line(Line {
                            op: LineOp::Put(e), ..
                        }) => ids.contains(&e.id),
                        Decoded::Line(_) => false,
                        // 読めない行に対象の ID があれば、本文が残らないように除く。
                        Decoded::Newer(_) | Decoded::Bad => true,
                    };
                if drop_it {
                    removed = true;
                } else {
                    kept.extend_from_slice(raw);
                }
            }
            if removed {
                let tmp = path.with_extension("jsonl.tmp");
                fsutil::prepare_private_file(&tmp)?;
                let mut f = File::create(&tmp)?;
                f.write_all(&kept)?;
                f.sync_all()?;
                drop(f);
                fsutil::rename_durable(&tmp, &path)?;
            }
        }
        Ok(())
    }

    /// 試験のための注入: 次の追記を失敗させる (AC-FR-202-1)。
    pub fn fail_next_append(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
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

    const NOW: Timestamp = Timestamp::from_millis(1_762_650_000_000);

    fn path(dir: &TempDir) -> PathBuf {
        dir.path().join("journal/2025-11.jsonl")
    }

    #[test]
    fn appends_and_reads_back_in_order_with_exact_floats() {
        let dir = TempDir::new("journal");
        let (mut j, scan) = Journal::open(dir.path(), 0).unwrap();
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
        let (_, scan) = Journal::open(dir.path(), 1).unwrap();
        assert_eq!(scan.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(scan.lines[0].op, LineOp::Put(env(2, "二")));
        assert_eq!((scan.bad, scan.newer), (0, 0));
        let (j, _) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!(j.next_seq, 4);
    }

    #[test]
    fn a_partial_tail_is_truncated_and_a_changed_line_is_bad() {
        let dir = TempDir::new("journal-partial");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a")), LineOp::Put(env(2, "b"))])
            .unwrap();
        drop(j);
        let mut text = std::fs::read_to_string(path(&dir)).unwrap();
        text = text.replacen("\"a\"", "\"X\"", 1);
        text.push_str("{\"seq\":10,\"op\":\"pu");
        std::fs::write(path(&dir), &text).unwrap();
        let (_, scan) = Journal::open(dir.path(), 0).unwrap();
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
        let (mut j, scan) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!((scan.bad, scan.newer), (0, 1));
        assert_eq!(j.append(NOW, &[LineOp::Put(env(1, "x"))]).unwrap(), 42);
    }

    #[test]
    fn appending_after_a_partial_line_does_not_break_the_new_line() {
        let dir = TempDir::new("journal-append-after-partial");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
        j.append(NOW, &[LineOp::Put(env(1, "a"))]).unwrap();
        let mut f = OpenOptions::new().append(true).open(path(&dir)).unwrap();
        f.write_all(b"{\"seq\":2,\"op\"").unwrap();
        drop(f);
        j.append(NOW, &[LineOp::Put(env(2, "b"))]).unwrap();
        drop(j);
        let (_, scan) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!(scan.lines.len(), 2, "the new line must survive");
        assert_eq!(
            scan.bad, 1,
            "the fragment is a bad line, not a lost new line"
        );
    }

    #[test]
    fn removing_ids_drops_their_lines_keeps_the_delete_line_and_survives_bad_bytes() {
        let dir = TempDir::new("journal-remove");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
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
        j.remove_ids(&[gone].into_iter().collect()).unwrap();
        drop(j);
        let text = String::from_utf8_lossy(&std::fs::read(path(&dir)).unwrap()).into_owned();
        assert!(!text.contains("消す"));
        assert!(text.contains("残す"));
        let (_, scan) = Journal::open(dir.path(), 0).unwrap();
        assert!(matches!(
            scan.lines.last().unwrap().op,
            LineOp::Delete { .. }
        ));
    }

    #[test]
    fn a_delete_whose_lines_were_not_removed_is_cleaned_up_at_startup() {
        let dir = TempDir::new("journal-cleanup");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
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
        drop(j); // remove_ids の前に止まった
        Journal::open(dir.path(), 0).unwrap();
        let text = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(!text.contains("残ってはいけない"));
    }

    #[test]
    fn an_injected_failure_does_not_advance_the_sequence() {
        let dir = TempDir::new("journal-fail");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
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
}
