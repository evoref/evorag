//! 語彙の索引 (05 §2.1、§2.3)。単位 (正規化と分け方は `evorag-retrieval`) の転置索引と BM25。
//!
//! ```text
//! index/memory/lex/
//!   CURRENT              ← 使っているマニフェストの名前
//!   manifest-<n>.json    ← セグメントの一覧
//!   seg-<n>/
//!     header.json        ← 形式名・版・正規化の規則の版・件数・作成時刻・各ファイルのハッシュ
//!     ids.bin            ← 行番号 → 記録の ID (とセッションと単位の数)
//!     lex.bin            ← 単位の表と posting (impact 順の CSR)
//! ```
//!
//! - セグメントは一度書いたら変えない。足す・除く・まとめるときは新しいセグメントとマニフェストを
//!   書き切ってから、`CURRENT` を一時名に書いて rename で入れ替える。
//! - 読み手は開いたときの一覧 ([`Snapshot`]) を `Arc` で持ち、応答の間は使い続ける。
//! - 開くときにハッシュ・形式・規則の版のどれかが合わなければ、空の索引として開き、作り直しが
//!   要ることを返す (NFR-306)。索引は DB から作り直せる派生のデータ。
//! - 読むときは、ファイルを読んでメモリに置く (v0.1 の件数では mmap を使わない)。

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use evorag_store::fsutil;
use evorag_types::Id;
use xxhash_rust::xxh3::xxh3_64;

/// セグメントの形式名と版。
const SEGMENT_FORMAT: &str = "evorag-lex-segment";
const MANIFEST_FORMAT: &str = "evorag-lex-manifest";
const FORMAT_VERSION: u32 = 1;
const IDS_MAGIC: &[u8; 4] = b"EVID";
const LEX_MAGIC: &[u8; 4] = b"EVLX";
const CURRENT: &str = "CURRENT";
/// セグメントがこの数を超えたら 1 つにまとめる (05 §2.3)。
pub const MERGE_AT: usize = 16;

/// BM25 の係数 (05 §2.1)。
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// 索引に入れる 1 件 (v0.1 はターン)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocInput {
    pub id: Id,
    pub session: Id,
    /// 単位の列 (重複を含む。数えた数が単位の頻度と文書の長さになる)。
    pub terms: Vec<String>,
}

/// 索引の 1 件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Doc {
    pub id: Id,
    pub session: Id,
    /// 単位の数 (BM25 の文書の長さ)。
    pub len: u32,
}

/// 一度書いたら変えない、索引の一部。
#[derive(Debug)]
pub struct Segment {
    name: String,
    docs: Vec<Doc>,
    /// 単位 → (行番号, 頻度) の列 (頻度の大きい順)。
    postings: HashMap<String, Vec<(u32, u32)>>,
}

impl Segment {
    fn build(name: String, docs: &[DocInput]) -> Self {
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut rows = Vec::with_capacity(docs.len());
        for (row, d) in docs.iter().enumerate() {
            let mut tf: HashMap<&str, u32> = HashMap::new();
            for t in &d.terms {
                *tf.entry(t).or_default() += 1;
            }
            for (t, n) in tf {
                postings
                    .entry(t.to_owned())
                    .or_default()
                    .push((row as u32, n));
            }
            rows.push(Doc {
                id: d.id,
                session: d.session,
                len: u32::try_from(d.terms.len()).unwrap_or(u32::MAX),
            });
        }
        for p in postings.values_mut() {
            sort_impact(p);
        }
        Self {
            name,
            docs: rows,
            postings,
        }
    }

    /// いくつかのセグメントから、`keep` を満たす行だけを集めて 1 つにする (テキストを読み直さない)。
    fn merge(name: String, parts: &[Arc<Segment>], keep: &dyn Fn(Id) -> bool) -> Self {
        let mut docs = Vec::new();
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for seg in parts {
            let mut remap = vec![None; seg.docs.len()];
            for (row, d) in seg.docs.iter().enumerate() {
                if keep(d.id) {
                    remap[row] = Some(docs.len() as u32);
                    docs.push(*d);
                }
            }
            for (t, list) in &seg.postings {
                for &(row, n) in list {
                    if let Some(new) = remap[row as usize] {
                        postings.entry(t.clone()).or_default().push((new, n));
                    }
                }
            }
        }
        postings.retain(|_, p| !p.is_empty());
        for p in postings.values_mut() {
            sort_impact(p);
        }
        Self {
            name,
            docs,
            postings,
        }
    }

    pub fn docs(&self) -> &[Doc] {
        &self.docs
    }

    fn ids_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + self.docs.len() * 36);
        b.extend_from_slice(IDS_MAGIC);
        b.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        for d in &self.docs {
            b.extend_from_slice(d.id.as_bytes());
            b.extend_from_slice(d.session.as_bytes());
            b.extend_from_slice(&d.len.to_le_bytes());
        }
        b
    }

    /// 単位の表 (単位の順) と、CSR の posting (各単位の中は頻度の大きい順)。
    fn lex_bytes(&self) -> Vec<u8> {
        let mut terms: Vec<&String> = self.postings.keys().collect();
        terms.sort();
        let mut b = Vec::new();
        b.extend_from_slice(LEX_MAGIC);
        b.extend_from_slice(&(terms.len() as u32).to_le_bytes());
        for t in terms {
            let list = &self.postings[t];
            b.extend_from_slice(&(t.len() as u32).to_le_bytes());
            b.extend_from_slice(t.as_bytes());
            b.extend_from_slice(&(list.len() as u32).to_le_bytes());
            for (row, n) in list {
                b.extend_from_slice(&row.to_le_bytes());
                b.extend_from_slice(&n.to_le_bytes());
            }
        }
        b
    }

    fn parse(name: String, ids: &[u8], lex: &[u8]) -> Option<Self> {
        let mut r = Reader(ids);
        if r.take(4)? != IDS_MAGIC {
            return None;
        }
        let n = r.u32()? as usize;
        let mut docs = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let id = Id::from_bytes(r.take(16)?.try_into().ok()?);
            let session = Id::from_bytes(r.take(16)?.try_into().ok()?);
            docs.push(Doc {
                id,
                session,
                len: r.u32()?,
            });
        }
        if !r.0.is_empty() {
            return None;
        }
        let mut r = Reader(lex);
        if r.take(4)? != LEX_MAGIC {
            return None;
        }
        let terms = r.u32()? as usize;
        let mut postings = HashMap::with_capacity(terms.min(1 << 20));
        for _ in 0..terms {
            let len = r.u32()? as usize;
            let t = std::str::from_utf8(r.take(len)?).ok()?.to_owned();
            let count = r.u32()? as usize;
            let mut list = Vec::with_capacity(count.min(1 << 20));
            for _ in 0..count {
                let row = r.u32()?;
                if row as usize >= docs.len() {
                    return None;
                }
                list.push((row, r.u32()?));
            }
            postings.insert(t, list);
        }
        if !r.0.is_empty() {
            return None;
        }
        Some(Self {
            name,
            docs,
            postings,
        })
    }
}

fn sort_impact(p: &mut [(u32, u32)]) {
    p.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// 読み手が持つ、ある時点のセグメントの一覧。
#[derive(Debug, Default)]
pub struct Snapshot {
    segments: Vec<Arc<Segment>>,
    docs: u64,
    total_len: u64,
}

/// 文書の位置 (セグメントと行)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocRef {
    seg: u32,
    row: u32,
}

impl Snapshot {
    fn new(segments: Vec<Arc<Segment>>) -> Self {
        let docs = segments.iter().map(|s| s.docs.len() as u64).sum();
        let total_len = segments
            .iter()
            .flat_map(|s| s.docs.iter())
            .map(|d| u64::from(d.len))
            .sum();
        Self {
            segments,
            docs,
            total_len,
        }
    }

    /// 文書の数。
    pub fn len(&self) -> u64 {
        self.docs
    }

    pub fn is_empty(&self) -> bool {
        self.docs == 0
    }

    pub fn doc(&self, r: DocRef) -> Doc {
        self.segments[r.seg as usize].docs[r.row as usize]
    }

    fn avgdl(&self) -> f64 {
        if self.docs == 0 {
            1.0
        } else {
            (self.total_len as f64 / self.docs as f64).max(1.0)
        }
    }

    fn idf(&self, term: &str) -> f64 {
        let df: usize = self
            .segments
            .iter()
            .map(|s| s.postings.get(term).map_or(0, Vec::len))
            .sum();
        let (n, df) = (self.docs as f64, df as f64);
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    fn weight(&self, tf: u32, dl: u32) -> f64 {
        let tf = f64::from(tf);
        tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * f64::from(dl) / self.avgdl()))
    }

    /// BM25 のスコア。`terms` は問いの単位 (重複は 1 回として数える)。`skip` が真の文書は除く。
    pub fn score(&self, terms: &[&str], skip: &dyn Fn(&Doc) -> bool) -> HashMap<DocRef, f64> {
        let unique: HashSet<&str> = terms.iter().copied().collect();
        let mut out: HashMap<DocRef, f64> = HashMap::new();
        for t in unique {
            let idf = self.idf(t);
            for (si, seg) in self.segments.iter().enumerate() {
                let Some(list) = seg.postings.get(t) else {
                    continue;
                };
                for &(row, n) in list {
                    let d = &seg.docs[row as usize];
                    if skip(d) {
                        continue;
                    }
                    *out.entry(DocRef {
                        seg: si as u32,
                        row,
                    })
                    .or_default() += idf * self.weight(n, d.len);
                }
            }
        }
        out
    }

    /// 問いそのものを文書と見たときのスコア (しきい値の比の分母、05 §2.1)。
    pub fn self_score(&self, terms: &[&str]) -> f64 {
        let mut tf: HashMap<&str, u32> = HashMap::new();
        for t in terms {
            *tf.entry(t).or_default() += 1;
        }
        let dl = u32::try_from(terms.len()).unwrap_or(u32::MAX);
        tf.into_iter()
            .map(|(t, n)| self.idf(t) * self.weight(n, dl))
            .sum()
    }

    /// 入っている文書の ID。
    pub fn ids(&self) -> HashSet<Id> {
        self.segments
            .iter()
            .flat_map(|s| s.docs.iter().map(|d| d.id))
            .collect()
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }
}

/// 開いたときの状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// 前の索引をそのまま開いた (空のことを含む)。
    Ok,
    /// 読めない・合わないものがあったので、空の索引で開いた。DB から作り直す。
    NeedsRebuild(String),
}

/// 語彙の索引。読むのは [`LexIndex::snapshot`]、変えるのは 1 つずつ (内部で順に並べる)。
pub struct LexIndex {
    dir: PathBuf,
    rules: u32,
    current: RwLock<Arc<Snapshot>>,
    /// 変える操作を順に並べ、次の名前の番号を持つ。
    writer: Mutex<u64>,
}

fn hex(v: u64) -> String {
    format!("{v:016x}")
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fsutil::prepare_private_file(path)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

impl LexIndex {
    /// `dir` (例: `<データ根>/index/memory/lex`) を開く。`rules` は正規化の規則の版。
    pub fn open(dir: &Path, rules: u32) -> io::Result<(Self, Opened)> {
        fsutil::create_private_dir(dir)?;
        let mut next = 1;
        for e in std::fs::read_dir(dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name
                .strip_prefix("seg-")
                .or_else(|| {
                    name.strip_prefix("manifest-")
                        .and_then(|s| s.strip_suffix(".json"))
                })
                .and_then(|s| u64::from_str_radix(s.split('.').next().unwrap_or(s), 16).ok());
            if let Some(n) = n {
                next = next.max(n + 1);
            }
        }
        let (segments, opened) = match Self::load(dir, rules) {
            Ok(Some(s)) => (s, Opened::Ok),
            Ok(None) => (Vec::new(), Opened::Ok),
            Err(e) => (Vec::new(), Opened::NeedsRebuild(e.to_string())),
        };
        let index = Self {
            dir: dir.to_path_buf(),
            rules,
            current: RwLock::new(Arc::new(Snapshot::new(segments))),
            writer: Mutex::new(next),
        };
        if opened == Opened::Ok {
            index.remove_unreferenced();
        }
        Ok((index, opened))
    }

    /// 読むだけで調べる (doctor)。まだ作っていなければ `None`、あれば (セグメントの数, 文書の数)。
    pub fn inspect(dir: &Path, rules: u32) -> io::Result<Option<(usize, u64)>> {
        Ok(Self::load(dir, rules)?.map(|s| (s.len(), s.iter().map(|x| x.docs.len() as u64).sum())))
    }

    /// `CURRENT` が無ければ `None` (まだ作っていない)。
    fn load(dir: &Path, rules: u32) -> io::Result<Option<Vec<Arc<Segment>>>> {
        let current = match std::fs::read_to_string(dir.join(CURRENT)) {
            Ok(s) => s.trim().to_owned(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if current.contains(['/', '\\']) || !current.starts_with("manifest-") {
            return Err(invalid("CURRENT names an unexpected file"));
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(&current))?).map_err(invalid_json)?;
        check_format(&manifest, MANIFEST_FORMAT, rules)?;
        let names = manifest["segments"]
            .as_array()
            .ok_or_else(|| invalid("the manifest has no segments"))?;
        let mut segments = Vec::with_capacity(names.len());
        for n in names {
            let name = n
                .as_str()
                .filter(|s| s.starts_with("seg-") && !s.contains(['/', '\\', '.']))
                .ok_or_else(|| invalid("a bad segment name"))?;
            segments.push(Arc::new(Self::load_segment(dir, name, rules)?));
        }
        Ok(Some(segments))
    }

    fn load_segment(dir: &Path, name: &str, rules: u32) -> io::Result<Segment> {
        let seg = dir.join(name);
        let header: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seg.join("header.json"))?)
                .map_err(invalid_json)?;
        check_format(&header, SEGMENT_FORMAT, rules)?;
        let read = |file: &str| -> io::Result<Vec<u8>> {
            let b = std::fs::read(seg.join(file))?;
            let want = header["files"][file].as_str().unwrap_or_default();
            if hex(xxh3_64(&b)) != want {
                return Err(invalid(format!("{name}/{file}: the hash does not match")));
            }
            Ok(b)
        };
        let (ids, lex) = (read("ids.bin")?, read("lex.bin")?);
        let s = Segment::parse(name.to_owned(), &ids, &lex)
            .ok_or_else(|| invalid(format!("{name}: unreadable")))?;
        if header["docs"].as_u64() != Some(s.docs.len() as u64) {
            return Err(invalid(format!("{name}: the count does not match")));
        }
        Ok(s)
    }

    /// 今の一覧。
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 文書を足す (小さなセグメントを 1 つ足し、数が [`MERGE_AT`] を超えたらまとめる)。
    /// 既に入っている ID は入れ直す。
    pub fn add(&self, docs: Vec<DocInput>) -> io::Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let mut next = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.snapshot();
        let replaced: HashSet<Id> = docs.iter().map(|d| d.id).collect();
        let mut segments: Vec<Arc<Segment>> = if snap
            .segments
            .iter()
            .any(|s| s.docs.iter().any(|d| replaced.contains(&d.id)))
        {
            let name = self.name(&mut next);
            let kept = Segment::merge(name, &snap.segments, &|id| !replaced.contains(&id));
            self.write_segment(&kept)?;
            vec![Arc::new(kept)]
        } else {
            snap.segments.clone()
        };
        let name = self.name(&mut next);
        let seg = Segment::build(name, &docs);
        self.write_segment(&seg)?;
        segments.push(Arc::new(seg));
        if segments.len() > MERGE_AT {
            let name = self.name(&mut next);
            let merged = Segment::merge(name, &segments, &|_| true);
            self.write_segment(&merged)?;
            segments = vec![Arc::new(merged)];
        }
        self.publish(&mut next, segments)
    }

    /// 文書を除く (除く ID を含むセグメントだけを書き直す)。
    pub fn remove(&self, ids: &HashSet<Id>) -> io::Result<()> {
        let mut next = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.snapshot();
        let mut segments = Vec::with_capacity(snap.segments.len());
        let mut changed = false;
        for s in &snap.segments {
            if s.docs.iter().any(|d| ids.contains(&d.id)) {
                changed = true;
                let name = self.name(&mut next);
                let kept = Segment::merge(name, std::slice::from_ref(s), &|id| !ids.contains(&id));
                if !kept.docs.is_empty() {
                    self.write_segment(&kept)?;
                    segments.push(Arc::new(kept));
                }
            } else {
                segments.push(s.clone());
            }
        }
        if !changed {
            return Ok(());
        }
        self.publish(&mut next, segments)
    }

    /// すべてを `docs` で置き換える (作り直し)。
    pub fn replace_all(&self, docs: Vec<DocInput>) -> io::Result<()> {
        let mut next = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut segments = Vec::new();
        if !docs.is_empty() {
            let name = self.name(&mut next);
            let seg = Segment::build(name, &docs);
            self.write_segment(&seg)?;
            segments.push(Arc::new(seg));
        }
        self.publish(&mut next, segments)
    }

    fn name(&self, next: &mut u64) -> String {
        let n = *next;
        *next += 1;
        format!("seg-{}", hex(n))
    }

    fn write_segment(&self, seg: &Segment) -> io::Result<()> {
        let dir = self.dir.join(&seg.name);
        fsutil::create_private_dir(&dir)?;
        let ids = seg.ids_bytes();
        let lex = seg.lex_bytes();
        write_private(&dir.join("ids.bin"), &ids)?;
        write_private(&dir.join("lex.bin"), &lex)?;
        let header = serde_json::json!({
            "format": SEGMENT_FORMAT,
            "version": FORMAT_VERSION,
            "rules": self.rules,
            "docs": seg.docs.len(),
            "created_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            "files": { "ids.bin": hex(xxh3_64(&ids)), "lex.bin": hex(xxh3_64(&lex)) },
        });
        write_private(
            &dir.join("header.json"),
            serde_json::to_string_pretty(&header)
                .map_err(io::Error::other)?
                .as_bytes(),
        )?;
        fsutil::sync_dir(&dir)
    }

    /// マニフェストを書き、`CURRENT` を入れ替え、読み手の一覧を替え、古いものを消す。
    fn publish(&self, next: &mut u64, segments: Vec<Arc<Segment>>) -> io::Result<()> {
        let n = *next;
        *next += 1;
        let manifest_name = format!("manifest-{}.json", hex(n));
        let manifest = serde_json::json!({
            "format": MANIFEST_FORMAT,
            "version": FORMAT_VERSION,
            "rules": self.rules,
            "segments": segments.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
        });
        write_private(
            &self.dir.join(&manifest_name),
            serde_json::to_string_pretty(&manifest)
                .map_err(io::Error::other)?
                .as_bytes(),
        )?;
        let tmp = self.dir.join("CURRENT.tmp");
        write_private(&tmp, format!("{manifest_name}\n").as_bytes())?;
        fsutil::rename_durable(&tmp, &self.dir.join(CURRENT))?;
        *self.current.write().unwrap_or_else(|e| e.into_inner()) =
            Arc::new(Snapshot::new(segments));
        self.remove_unreferenced();
        Ok(())
    }

    /// 今の一覧が指さないセグメントとマニフェストを消す。消せなければ次に開いたときに消す。
    fn remove_unreferenced(&self) {
        let snap = self.snapshot();
        let live: HashSet<&str> = snap.segments.iter().map(|s| s.name.as_str()).collect();
        let current = std::fs::read_to_string(self.dir.join(CURRENT)).unwrap_or_default();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("seg-") && !live.contains(name.as_str()) {
                let _ = std::fs::remove_dir_all(e.path());
            } else if name.starts_with("manifest-") && name != current.trim() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn invalid_json(e: serde_json::Error) -> io::Error {
    invalid(e.to_string())
}

fn check_format(v: &serde_json::Value, format: &str, rules: u32) -> io::Result<()> {
    if v["format"].as_str() != Some(format) {
        return Err(invalid(format!("not a {format}")));
    }
    if v["version"].as_u64() != Some(u64::from(FORMAT_VERSION)) {
        return Err(invalid(format!("{format}: another version")));
    }
    if v["rules"].as_u64() != Some(u64::from(rules)) {
        return Err(invalid(format!("{format}: made with other rules")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    fn doc(n: u8, session: u8, terms: &[&str]) -> DocInput {
        DocInput {
            id: id(n),
            session: id(session),
            terms: terms.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn temp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "evorag-lex-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn best(s: &Snapshot, q: &[&str]) -> Vec<Id> {
        let mut v: Vec<_> = s.score(q, &|_| false).into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().map(|(r, _)| s.doc(r).id).collect()
    }

    #[test]
    fn adds_reopens_removes_and_merges() {
        let dir = temp();
        let (ix, opened) = LexIndex::open(&dir, 1).unwrap();
        assert_eq!(opened, Opened::Ok);
        assert!(ix.snapshot().is_empty());
        ix.add(vec![
            doc(1, 9, &["京都", "旅行"]),
            doc(2, 9, &["カレー", "玉葱"]),
        ])
        .unwrap();
        ix.add(vec![doc(3, 8, &["京都", "歴史", "京都"])]).unwrap();
        assert_eq!(best(&ix.snapshot(), &["京都"]), [id(3), id(1)]);
        drop(ix);

        let (ix, opened) = LexIndex::open(&dir, 1).unwrap();
        assert_eq!(opened, Opened::Ok);
        assert_eq!(ix.snapshot().len(), 3);
        assert_eq!(ix.snapshot().segment_count(), 2);
        // 読み手が持っている一覧は、変えた後も同じものを返す。
        let held = ix.snapshot();
        ix.remove(&HashSet::from([id(3)])).unwrap();
        assert_eq!(best(&held, &["京都"]).len(), 2);
        assert_eq!(best(&ix.snapshot(), &["京都"]), [id(1)]);
        assert!(!ix.snapshot().ids().contains(&id(3)));

        for n in 10..(10 + MERGE_AT as u8) {
            ix.add(vec![doc(n, 7, &["電池"])]).unwrap();
        }
        assert_eq!(ix.snapshot().segment_count(), 1, "merged");
        assert_eq!(ix.snapshot().len(), 2 + MERGE_AT as u64);
        // 消したものはファイルにも残らない。
        let segs = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("seg-"))
            .count();
        assert_eq!(segs, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// verifies: AC-NFR-306-1
    #[test]
    fn a_bad_hash_or_other_rules_ask_for_a_rebuild() {
        let dir = temp();
        let (ix, _) = LexIndex::open(&dir, 1).unwrap();
        ix.add(vec![doc(1, 9, &["京都"])]).unwrap();
        drop(ix);
        let (_, opened) = LexIndex::open(&dir, 2).unwrap();
        assert!(matches!(opened, Opened::NeedsRebuild(_)));
        let seg = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("seg-"))
            .unwrap()
            .path();
        let mut b = std::fs::read(seg.join("lex.bin")).unwrap();
        let last = b.len() - 1;
        b[last] ^= 1;
        std::fs::write(seg.join("lex.bin"), b).unwrap();
        let (ix, opened) = LexIndex::open(&dir, 1).unwrap();
        assert!(matches!(opened, Opened::NeedsRebuild(_)), "{opened:?}");
        assert!(ix.snapshot().is_empty());
        ix.replace_all(vec![doc(1, 9, &["京都"])]).unwrap();
        let (ix, opened) = LexIndex::open(&dir, 1).unwrap();
        assert_eq!(opened, Opened::Ok);
        assert_eq!(ix.snapshot().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn adding_an_existing_id_replaces_it() {
        let dir = temp();
        let (ix, _) = LexIndex::open(&dir, 1).unwrap();
        ix.add(vec![doc(1, 9, &["古い"])]).unwrap();
        ix.add(vec![doc(1, 9, &["新しい"])]).unwrap();
        let s = ix.snapshot();
        assert_eq!(s.len(), 1);
        assert!(best(&s, &["古い"]).is_empty());
        assert_eq!(best(&s, &["新しい"]), [id(1)]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn self_score_counts_repeats() {
        let dir = temp();
        let (ix, _) = LexIndex::open(&dir, 1).unwrap();
        ix.add(vec![doc(1, 9, &["a", "b"]), doc(2, 9, &["c"])])
            .unwrap();
        let s = ix.snapshot();
        assert!(s.self_score(&["a", "a"]) > s.self_score(&["a"]));
        assert_eq!(s.self_score(&[]), 0.0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
