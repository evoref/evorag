//! 会話のジャーナル (03 §6、ADR-0006)。
//!
//! - 置き場所は `journal/<年>-<月>.jsonl`。1 行は `{"env": …, "op": "put"|"delete"|"meta", "seq": N, "sum": "…"}`。
//!   `sum` は、`sum` を除いた行の JSON (キーの順) の xxh3-64 を 16 進で書いたもの。
//! - 書き手が、ジャーナルに書いて書き切ってから DB をコミットする (先書き)。連番は単調に増える。
//! - 削除の行は、対象の種類と ID だけを持ち、本文を含めない。削除のときは、対象の ID の行を除いた
//!   ファイルを書いて入れ替える (一時名 → rename)。
//! - 起動時に、最後の行が改行で終わらなければ (書きかけ) その行だけを切り詰める。チェックサムが
//!   合わない行は読み飛ばして数える (新しい形式の行を消さないため、読めない行は切り詰めない)。

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use evorag_types::{Id, Timestamp};
use serde_json::{Map, Value as Json, json};

use crate::envelope::Envelope;
use crate::fsutil;
use crate::json::{envelope_from_json, envelope_to_json};

pub const JOURNAL_DIR: &str = "journal";

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
    /// チェックサムが合わない・読めない行の数。
    pub bad: usize,
    /// 書きかけで切り詰めた末尾のバイト数。
    pub truncated: usize,
}

pub struct Journal {
    dir: PathBuf,
    next_seq: u64,
    /// 試験のための注入: 次の追記を失敗させる。
    fail_next: AtomicBool,
}

fn sum_of(body: &Json) -> String {
    format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(body.to_string().as_bytes())
    )
}

fn encode(seq: u64, op: &LineOp) -> String {
    let (name, env) = match op {
        LineOp::Put(e) => ("put", envelope_to_json(e)),
        LineOp::Delete { kind, id } => ("delete", json!({"kind": kind, "id": id.to_string()})),
        LineOp::Meta { key, value } => ("meta", json!({"key": key, "value": value})),
    };
    let mut body = json!({"seq": seq, "op": name, "env": env});
    let sum = sum_of(&body);
    body.as_object_mut()
        .expect("object")
        .insert("sum".into(), Json::String(sum));
    let mut s = body.to_string();
    s.push('\n');
    s
}

fn decode(text: &str) -> Option<Line> {
    let mut v: Json = serde_json::from_str(text).ok()?;
    let obj: &mut Map<String, Json> = v.as_object_mut()?;
    let sum = obj.remove("sum")?;
    if sum.as_str()? != sum_of(&v) {
        return None;
    }
    let seq = v.get("seq")?.as_u64()?;
    let env = v.get("env")?;
    let op = match v.get("op")?.as_str()? {
        "put" => LineOp::Put(envelope_from_json(env).ok()?),
        "delete" => LineOp::Delete {
            kind: env.get("kind")?.as_str()?.to_owned(),
            id: env.get("id")?.as_str()?.parse().ok()?,
        },
        "meta" => LineOp::Meta {
            key: env.get("key")?.as_str()?.to_owned(),
            value: env.get("value")?.as_str()?.to_owned(),
        },
        _ => return None,
    };
    Some(Line { seq, op })
}

/// 行の対象の ID (put と delete)。
fn line_id(text: &str) -> Option<Id> {
    let v: Json = serde_json::from_str(text).ok()?;
    v.get("env")?.get("id")?.as_str()?.parse().ok()
}

fn month(ts: Timestamp) -> String {
    // 03 §6 の `<年>-<月>`。backup::stamp の先頭 6 文字 (YYYYMM) から作る。
    let s = crate::backup::stamp(ts.as_millis());
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

impl Journal {
    /// 開いて読む。`last_seq` より大きい行を返す。書きかけの末尾を切り詰める。
    pub fn open(root: &Path, last_seq: u64) -> io::Result<(Self, Scan)> {
        let dir = root.join(JOURNAL_DIR);
        fsutil::create_private_dir(&dir)?;
        let mut scan = Scan::default();
        let mut max_seq = last_seq;
        let all = files(&dir)?;
        for (i, path) in all.iter().enumerate() {
            let mut bytes = std::fs::read(path)?;
            if i + 1 == all.len() && !bytes.is_empty() && bytes.last() != Some(&b'\n') {
                let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                scan.truncated = bytes.len() - keep;
                bytes.truncate(keep);
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(keep as u64)?;
                f.sync_all()?;
            }
            for text in String::from_utf8_lossy(&bytes).lines() {
                if text.trim().is_empty() {
                    continue;
                }
                match decode(text) {
                    Some(line) => {
                        max_seq = max_seq.max(line.seq);
                        if line.seq > last_seq {
                            scan.lines.push(line);
                        }
                    }
                    None => scan.bad += 1,
                }
            }
        }
        scan.lines.sort_by_key(|l| l.seq);
        Ok((
            Self {
                dir,
                next_seq: max_seq + 1,
                fail_next: AtomicBool::new(false),
            },
            scan,
        ))
    }

    /// 行を追記して書き切る。最後の行の連番を返す。失敗したら連番を進めない。
    pub fn append(&mut self, now: Timestamp, ops: &[LineOp]) -> io::Result<u64> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected journal failure"));
        }
        let mut text = String::new();
        let mut seq = self.next_seq;
        for op in ops {
            text.push_str(&encode(seq, op));
            seq += 1;
        }
        let path = self.dir.join(format!("{}.jsonl", month(now)));
        let new = !path.exists();
        if new {
            fsutil::prepare_private_file(&path)?;
        }
        let mut f = OpenOptions::new().append(true).open(&path)?;
        f.write_all(text.as_bytes())?;
        // 書き切る (Windows は FlushFileBuffers、Linux は fdatasync)。
        f.sync_data()?;
        if new {
            fsutil::sync_dir(&self.dir)?;
        }
        self.next_seq = seq;
        Ok(seq - 1)
    }

    /// 対象の ID の行を除いたファイルを書いて入れ替える (削除、03 §6)。削除の行そのものは残す。
    pub fn remove_ids(&mut self, ids: &BTreeSet<Id>) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        for path in files(&self.dir)? {
            let text = std::fs::read_to_string(&path)?;
            let mut kept = String::with_capacity(text.len());
            let mut removed = false;
            for l in text.split_inclusive('\n') {
                let is_put_of_target = line_id(l).is_some_and(|id| ids.contains(&id))
                    && serde_json::from_str::<Json>(l)
                        .ok()
                        .and_then(|v| v.get("op").and_then(Json::as_str).map(|o| o == "put"))
                        .unwrap_or(false);
                if is_put_of_target {
                    removed = true;
                } else {
                    kept.push_str(l);
                }
            }
            if removed {
                let tmp = path.with_extension("jsonl.tmp");
                fsutil::prepare_private_file(&tmp)?;
                let mut f = File::create(&tmp)?;
                f.write_all(kept.as_bytes())?;
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
        e.body = ciborium::Value::Map(vec![("text".into(), text.into())]);
        e
    }

    #[test]
    fn appends_and_reads_back_in_order() {
        let dir = TempDir::new("journal");
        let (mut j, scan) = Journal::open(dir.path(), 0).unwrap();
        assert!(scan.lines.is_empty());
        let now = Timestamp::from_millis(1_762_650_000_000);
        assert_eq!(j.append(now, &[LineOp::Put(env(1, "一"))]).unwrap(), 1);
        assert_eq!(
            j.append(
                now,
                &[
                    LineOp::Put(env(2, "二")),
                    LineOp::Meta {
                        key: "prompt.language".into(),
                        value: "ja".into()
                    }
                ]
            )
            .unwrap(),
            3
        );
        drop(j);
        assert!(dir.path().join("journal/2025-11.jsonl").exists());
        let (_, scan) = Journal::open(dir.path(), 1).unwrap();
        assert_eq!(scan.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(scan.lines[0].op, LineOp::Put(env(2, "二")));
        let (j, _) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!(j.next_seq, 4);
    }

    #[test]
    fn a_partial_last_line_is_truncated_and_bad_lines_are_counted() {
        let dir = TempDir::new("journal-partial");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
        let now = Timestamp::from_millis(1_762_650_000_000);
        j.append(now, &[LineOp::Put(env(1, "a")), LineOp::Put(env(2, "b"))])
            .unwrap();
        drop(j);
        let path = dir.path().join("journal/2025-11.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        // 1 行目を壊し、書きかけの行を足す。
        text = text.replacen("\"a\"", "\"X\"", 1);
        text.push_str("{\"seq\":3,\"op\":\"pu");
        std::fs::write(&path, &text).unwrap();
        let (_, scan) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!(scan.bad, 1);
        assert_eq!(scan.lines.len(), 1);
        assert_eq!(scan.truncated, "{\"seq\":3,\"op\":\"pu".len());
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
    }

    #[test]
    fn removing_ids_drops_their_lines_but_keeps_the_delete_line() {
        let dir = TempDir::new("journal-remove");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
        let now = Timestamp::from_millis(1_762_650_000_000);
        j.append(
            now,
            &[LineOp::Put(env(1, "消す")), LineOp::Put(env(2, "残す"))],
        )
        .unwrap();
        let gone = Id::from_bytes([1; 16]);
        j.remove_ids(&[gone].into_iter().collect()).unwrap();
        j.append(
            now,
            &[LineOp::Delete {
                kind: "turn".into(),
                id: gone,
            }],
        )
        .unwrap();
        drop(j);
        let text = std::fs::read_to_string(dir.path().join("journal/2025-11.jsonl")).unwrap();
        assert!(!text.contains("消す"));
        assert!(text.contains("残す"));
        let (_, scan) = Journal::open(dir.path(), 0).unwrap();
        assert_eq!(scan.bad, 0);
        assert!(matches!(
            scan.lines.last().unwrap().op,
            LineOp::Delete { .. }
        ));
    }

    #[test]
    fn an_injected_failure_does_not_advance_the_sequence() {
        let dir = TempDir::new("journal-fail");
        let (mut j, _) = Journal::open(dir.path(), 0).unwrap();
        let now = Timestamp::from_millis(0);
        j.fail_next_append();
        assert!(j.append(now, &[LineOp::Put(env(1, "x"))]).is_err());
        assert_eq!(j.append(now, &[LineOp::Put(env(1, "x"))]).unwrap(), 1);
    }
}
