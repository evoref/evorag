//! ベクトルの索引 (05 §2.2・§2.3)。埋め込みを int8 に量子化して全件を走査する (ADR-0009、S-05)。
//!
//! ```text
//! index/memory/vec-<embed の model_key>/
//!   CURRENT              ← 使っているマニフェストの名前
//!   manifest-<n>.json    ← セグメントの一覧
//!   seg-<n>/
//!     header.json        ← 形式名・版・次元・件数・作成時刻・各ファイルのハッシュ
//!     ids.bin            ← 行番号 → 記録の ID・セッション・単位の種類
//!     vec.i8             ← N × 次元の int8 と、行ごとの scale (f32)
//! ```
//!
//! - 量子化 (S-05): 埋め込みを長さ 1 にしてから、行ごとに対称の int8 にする (scale = 行の絶対値の
//!   最大 / 127)。問いも同じ形で int8 にし、整数の内積に 2 つの scale を掛けて値にする。
//! - セグメントは一度書いたら変えない。足す・除く・まとめるときは新しいセグメントとマニフェストを
//!   書き切ってから、`CURRENT` を一時名に書いて rename で入れ替える (語彙の索引と同じ)。
//! - 読み手は開いたときの一覧 ([`VecSnapshot`]) を `Arc` で持ち、応答の間は使い続ける。
//! - 開くときにハッシュ・形式・次元のどれかが合わなければ、空の索引として開き、作り直しが要ることを
//!   返す (NFR-306)。索引は DB の埋め込みから作り直せる派生のデータ。

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use evorag_store::fsutil;
use evorag_types::Id;
use xxhash_rust::xxh3::xxh3_64;

use crate::lex::Opened;

const SEGMENT_FORMAT: &str = "evorag-vec-segment";
const MANIFEST_FORMAT: &str = "evorag-vec-manifest";
const FORMAT_VERSION: u32 = 1;
const IDS_MAGIC: &[u8; 4] = b"EVVI";
const VEC_MAGIC: &[u8; 4] = b"EVVQ";
const CURRENT: &str = "CURRENT";
/// セグメントがこの数を超えたら 1 つにまとめる (05 §2.3)。
pub const MERGE_AT: usize = 16;

/// 索引の単位の種類 (05 §2.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    /// チャンク。
    Chunk,
    /// ノートの検索の鍵 (04 §2)。
    Key,
}

impl Unit {
    fn byte(self) -> u8 {
        match self {
            Unit::Chunk => 0,
            Unit::Key => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Unit::Chunk),
            1 => Some(Unit::Key),
            _ => None,
        }
    }
}

/// 索引に入れる 1 件。
#[derive(Debug, Clone, PartialEq)]
pub struct VecInput {
    pub id: Id,
    pub session: Id,
    pub unit: Unit,
    pub vector: Vec<f32>,
}

/// 索引の 1 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VecRow {
    pub id: Id,
    pub session: Id,
    pub unit: Unit,
}

/// 長さ 1 にしてから、行ごとに対称の int8 にする (S-05)。
pub fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let inv = if n > 0.0 { 1.0 / n } else { 0.0 };
    let m = v.iter().fold(0f32, |a, x| a.max((x * inv).abs()));
    let scale = if m > 0.0 { m / 127.0 } else { 1.0 };
    (
        v.iter()
            .map(|x| (x * inv / scale).round().clamp(-127.0, 127.0) as i8)
            .collect(),
        scale,
    )
}

#[inline]
fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    // 16 個ずつ i16 にして掛け、i32 に足す (自動のベクトル化に向く形、S-05)。
    let mut acc = [0i32; 16];
    let n = a.len().min(b.len()) / 16 * 16;
    for (x, y) in a[..n]
        .as_chunks::<16>()
        .0
        .iter()
        .zip(b[..n].as_chunks::<16>().0)
    {
        for i in 0..16 {
            acc[i] += i32::from(i16::from(x[i]) * i16::from(y[i]));
        }
    }
    let mut s: i32 = acc.iter().sum();
    for (x, y) in a[n..].iter().zip(&b[n..]) {
        s += i32::from(*x) * i32::from(*y);
    }
    s
}

/// 一度書いたら変えない、索引の一部。
#[derive(Debug)]
pub struct VecSegment {
    name: String,
    dim: usize,
    rows: Vec<VecRow>,
    data: Vec<i8>,
    scales: Vec<f32>,
}

impl VecSegment {
    fn build(name: String, dim: usize, inputs: &[VecInput]) -> Self {
        let mut rows = Vec::with_capacity(inputs.len());
        let mut data = Vec::with_capacity(inputs.len() * dim);
        let mut scales = Vec::with_capacity(inputs.len());
        for i in inputs {
            let (q, s) = quantize(&i.vector);
            rows.push(VecRow {
                id: i.id,
                session: i.session,
                unit: i.unit,
            });
            data.extend_from_slice(&q);
            scales.push(s);
        }
        Self {
            name,
            dim,
            rows,
            data,
            scales,
        }
    }

    /// `keep` が真の行だけを残して、いくつかのセグメントを 1 つにする。
    fn merge(
        name: String,
        dim: usize,
        segs: &[Arc<VecSegment>],
        keep: &dyn Fn(Id) -> bool,
    ) -> Self {
        let mut out = Self {
            name,
            dim,
            rows: Vec::new(),
            data: Vec::new(),
            scales: Vec::new(),
        };
        let mut seen = HashSet::new();
        // 新しいセグメントの行を先に採る (同じ ID は新しいものが勝つ)。
        for s in segs.iter().rev() {
            for (r, row) in s.rows.iter().enumerate() {
                if keep(row.id) && seen.insert(row.id) {
                    out.rows.push(*row);
                    out.data.extend_from_slice(&s.data[r * dim..(r + 1) * dim]);
                    out.scales.push(s.scales[r]);
                }
            }
        }
        out
    }

    fn ids_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + self.rows.len() * 33);
        b.extend_from_slice(IDS_MAGIC);
        b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        b.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        for r in &self.rows {
            b.extend_from_slice(r.id.as_bytes());
            b.extend_from_slice(r.session.as_bytes());
            b.push(r.unit.byte());
        }
        b
    }

    fn vec_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + self.data.len() + self.scales.len() * 4);
        b.extend_from_slice(VEC_MAGIC);
        b.extend_from_slice(&(self.dim as u32).to_le_bytes());
        b.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        b.extend(self.data.iter().map(|x| *x as u8));
        for s in &self.scales {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    fn parse(name: String, dim: usize, ids: &[u8], vecs: &[u8]) -> Option<Self> {
        let u32_at = |b: &[u8], at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
        };
        if ids.get(..4)? != IDS_MAGIC || u32_at(ids, 4)? != FORMAT_VERSION {
            return None;
        }
        let n = u32_at(ids, 8)? as usize;
        if ids.len() != 12 + n * 33 {
            return None;
        }
        let mut rows = Vec::with_capacity(n);
        for r in 0..n {
            let at = 12 + r * 33;
            rows.push(VecRow {
                id: Id::from_bytes(ids[at..at + 16].try_into().ok()?),
                session: Id::from_bytes(ids[at + 16..at + 32].try_into().ok()?),
                unit: Unit::from_byte(ids[at + 32])?,
            });
        }
        if vecs.get(..4)? != VEC_MAGIC
            || u32_at(vecs, 4)? as usize != dim
            || u32_at(vecs, 8)? as usize != n
            || vecs.len() != 12 + n * dim + n * 4
        {
            return None;
        }
        let data: Vec<i8> = vecs[12..12 + n * dim].iter().map(|x| *x as i8).collect();
        let scales = vecs[12 + n * dim..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        Some(Self {
            name,
            dim,
            rows,
            data,
            scales,
        })
    }
}

/// 読み手が持つ、ある時点のセグメントの一覧。
#[derive(Debug, Default)]
pub struct VecSnapshot {
    segments: Vec<Arc<VecSegment>>,
}

impl VecSnapshot {
    /// 行の数。
    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.rows.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 入っている記録の ID。
    pub fn ids(&self) -> HashSet<Id> {
        self.segments
            .iter()
            .flat_map(|s| s.rows.iter().map(|r| r.id))
            .collect()
    }

    /// 問いのベクトルに近い上位 `k` 行 (値の高い順)。`skip` が真の行は除く。
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        skip: &dyn Fn(&VecRow) -> bool,
    ) -> Vec<(VecRow, f32)> {
        let (q, qs) = quantize(query);
        let mut best: Vec<(VecRow, f32)> = Vec::with_capacity(k + 1);
        for s in &self.segments {
            if s.dim != q.len() {
                continue;
            }
            for (r, row) in s.rows.iter().enumerate() {
                if skip(row) {
                    continue;
                }
                let v = dot_i8(&q, &s.data[r * s.dim..(r + 1) * s.dim]) as f32 * s.scales[r] * qs;
                if best.len() < k || v > best[best.len() - 1].1 {
                    let at = best.partition_point(|x| x.1 >= v);
                    best.insert(at, (*row, v));
                    best.truncate(k);
                }
            }
        }
        best
    }
}

/// ベクトルの索引。読むのは [`VecIndex::snapshot`]、変えるのは 1 つずつ (内部で順に並べる)。
pub struct VecIndex {
    dir: PathBuf,
    dim: usize,
    current: RwLock<Arc<VecSnapshot>>,
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

fn check(v: &serde_json::Value, format: &str, dim: usize) -> io::Result<()> {
    if v["format"].as_str() != Some(format) {
        return Err(invalid(format!("not a {format}")));
    }
    if v["version"].as_u64() != Some(u64::from(FORMAT_VERSION)) {
        return Err(invalid(format!("{format}: another version")));
    }
    if v["dim"].as_u64() != Some(dim as u64) {
        return Err(invalid(format!("{format}: another dimension")));
    }
    Ok(())
}

impl VecIndex {
    /// `dir` (例: `<データ根>/index/memory/vec-<model_key>`) を、次元 `dim` で開く。
    pub fn open(dir: &Path, dim: usize) -> io::Result<(Self, Opened)> {
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
                .and_then(|s| u64::from_str_radix(s, 16).ok());
            if let Some(n) = n {
                next = next.max(n + 1);
            }
        }
        let (segments, opened) = match Self::load(dir, dim) {
            Ok(Some(s)) => (s, Opened::Ok),
            Ok(None) => (Vec::new(), Opened::Ok),
            Err(e) => (Vec::new(), Opened::NeedsRebuild(e.to_string())),
        };
        let index = Self {
            dir: dir.to_path_buf(),
            dim,
            current: RwLock::new(Arc::new(VecSnapshot { segments })),
            writer: Mutex::new(next),
        };
        if opened == Opened::Ok {
            index.remove_unreferenced();
        }
        Ok((index, opened))
    }

    fn load(dir: &Path, dim: usize) -> io::Result<Option<Vec<Arc<VecSegment>>>> {
        let current = match std::fs::read_to_string(dir.join(CURRENT)) {
            Ok(s) => s.trim().to_owned(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if current.contains(['/', '\\']) || !current.starts_with("manifest-") {
            return Err(invalid("CURRENT names an unexpected file"));
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(&current))?)
                .map_err(|e| invalid(e.to_string()))?;
        check(&manifest, MANIFEST_FORMAT, dim)?;
        let names = manifest["segments"]
            .as_array()
            .ok_or_else(|| invalid("the manifest has no segments"))?;
        let mut segments = Vec::with_capacity(names.len());
        for n in names {
            let name = n
                .as_str()
                .filter(|s| s.starts_with("seg-") && !s.contains(['/', '\\', '.']))
                .ok_or_else(|| invalid("a bad segment name"))?;
            segments.push(Arc::new(Self::load_segment(dir, name, dim)?));
        }
        Ok(Some(segments))
    }

    fn load_segment(dir: &Path, name: &str, dim: usize) -> io::Result<VecSegment> {
        let seg = dir.join(name);
        let header: serde_json::Value =
            serde_json::from_slice(&std::fs::read(seg.join("header.json"))?)
                .map_err(|e| invalid(e.to_string()))?;
        check(&header, SEGMENT_FORMAT, dim)?;
        let read = |file: &str| -> io::Result<Vec<u8>> {
            let b = std::fs::read(seg.join(file))?;
            if hex(xxh3_64(&b)) != header["files"][file].as_str().unwrap_or_default() {
                return Err(invalid(format!("{name}/{file}: the hash does not match")));
            }
            Ok(b)
        };
        let (ids, vecs) = (read("ids.bin")?, read("vec.i8")?);
        let s = VecSegment::parse(name.to_owned(), dim, &ids, &vecs)
            .ok_or_else(|| invalid(format!("{name}: unreadable")))?;
        if header["rows"].as_u64() != Some(s.rows.len() as u64) {
            return Err(invalid(format!("{name}: the count does not match")));
        }
        Ok(s)
    }

    /// 次元。
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 今の一覧。
    pub fn snapshot(&self) -> Arc<VecSnapshot> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 行を足す (既に入っている ID は入れ直す)。次元の合わないものは断る。
    pub fn add(&self, inputs: Vec<VecInput>) -> io::Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        if let Some(bad) = inputs.iter().find(|i| i.vector.len() != self.dim) {
            return Err(invalid(format!(
                "a vector of {} dimensions for an index of {}",
                bad.vector.len(),
                self.dim
            )));
        }
        let mut next = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.snapshot();
        let replaced: HashSet<Id> = inputs.iter().map(|i| i.id).collect();
        let mut segments: Vec<Arc<VecSegment>> = if snap
            .segments
            .iter()
            .any(|s| s.rows.iter().any(|r| replaced.contains(&r.id)))
        {
            let name = self.name(&mut next);
            let kept = VecSegment::merge(name, self.dim, &snap.segments, &|id| {
                !replaced.contains(&id)
            });
            self.write_segment(&kept)?;
            vec![Arc::new(kept)]
        } else {
            snap.segments.clone()
        };
        let name = self.name(&mut next);
        let seg = VecSegment::build(name, self.dim, &inputs);
        self.write_segment(&seg)?;
        segments.push(Arc::new(seg));
        if segments.len() > MERGE_AT {
            let name = self.name(&mut next);
            let merged = VecSegment::merge(name, self.dim, &segments, &|_| true);
            self.write_segment(&merged)?;
            segments = vec![Arc::new(merged)];
        }
        self.publish(&mut next, segments)
    }

    /// 行を除く (除く ID を含むセグメントだけを書き直す)。
    pub fn remove(&self, ids: &HashSet<Id>) -> io::Result<()> {
        let mut next = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.snapshot();
        let mut segments = Vec::with_capacity(snap.segments.len());
        let mut changed = false;
        for s in &snap.segments {
            if s.rows.iter().any(|r| ids.contains(&r.id)) {
                changed = true;
                let name = self.name(&mut next);
                let kept = VecSegment::merge(name, self.dim, std::slice::from_ref(s), &|id| {
                    !ids.contains(&id)
                });
                if !kept.rows.is_empty() {
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

    fn name(&self, next: &mut u64) -> String {
        let n = *next;
        *next += 1;
        format!("seg-{}", hex(n))
    }

    fn write_segment(&self, seg: &VecSegment) -> io::Result<()> {
        let dir = self.dir.join(&seg.name);
        fsutil::create_private_dir(&dir)?;
        let ids = seg.ids_bytes();
        let vecs = seg.vec_bytes();
        write_private(&dir.join("ids.bin"), &ids)?;
        write_private(&dir.join("vec.i8"), &vecs)?;
        let header = serde_json::json!({
            "format": SEGMENT_FORMAT,
            "version": FORMAT_VERSION,
            "dim": self.dim,
            "rows": seg.rows.len(),
            "created_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            "files": { "ids.bin": hex(xxh3_64(&ids)), "vec.i8": hex(xxh3_64(&vecs)) },
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
    fn publish(&self, next: &mut u64, segments: Vec<Arc<VecSegment>>) -> io::Result<()> {
        let n = *next;
        *next += 1;
        let manifest_name = format!("manifest-{}.json", hex(n));
        let manifest = serde_json::json!({
            "format": MANIFEST_FORMAT,
            "version": FORMAT_VERSION,
            "dim": self.dim,
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
            Arc::new(VecSnapshot { segments });
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

/// `vec-<model_key>` の索引のうち、`keep` (今と前の世代) 以外を消す (05 §2.2 の「1 世代だけ残す」)。
/// 消した数を返す。
pub fn prune_generations(memory_dir: &Path, keep: &[&str]) -> usize {
    let Ok(entries) = std::fs::read_dir(memory_dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(key) = name.strip_prefix("vec-")
            && !keep.contains(&key)
            && std::fs::remove_dir_all(e.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    fn temp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "evorag-vec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 次元 8 の、`hot` 番目だけが大きいベクトル。
    fn v(hot: usize) -> Vec<f32> {
        let mut x = vec![0.05f32; 8];
        x[hot] = 1.0;
        x
    }

    fn input(n: u8, session: u8, hot: usize) -> VecInput {
        VecInput {
            id: id(n),
            session: id(session),
            unit: Unit::Chunk,
            vector: v(hot),
        }
    }

    #[test]
    fn the_nearest_rows_come_first_and_survive_a_reopen() {
        let dir = temp();
        let (ix, opened) = VecIndex::open(&dir, 8).unwrap();
        assert_eq!(opened, Opened::Ok);
        ix.add(vec![input(1, 9, 0), input(2, 9, 3), input(3, 8, 3)])
            .unwrap();
        let hits = ix.snapshot().search(&v(3), 2, &|_| false);
        let ids: Vec<Id> = hits.iter().map(|(r, _)| r.id).collect();
        assert!(ids.contains(&id(2)) && ids.contains(&id(3)), "{ids:?}");
        // 今のセッションの行は除ける。
        let hits = ix.snapshot().search(&v(3), 1, &|r| r.session == id(8));
        assert_eq!(hits[0].0.id, id(2));
        drop(ix);
        let (ix, opened) = VecIndex::open(&dir, 8).unwrap();
        assert_eq!(opened, Opened::Ok);
        assert_eq!(ix.snapshot().len(), 3);
        // 入れ直しと除く。
        ix.add(vec![VecInput {
            vector: v(5),
            ..input(1, 9, 0)
        }])
        .unwrap();
        assert_eq!(ix.snapshot().len(), 3);
        assert_eq!(ix.snapshot().search(&v(5), 1, &|_| false)[0].0.id, id(1));
        ix.remove(&[id(2)].into()).unwrap();
        assert_eq!(ix.snapshot().ids(), [id(1), id(3)].into());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_segment_or_another_dimension_needs_a_rebuild() {
        let dir = temp();
        let (ix, _) = VecIndex::open(&dir, 8).unwrap();
        ix.add(vec![input(1, 9, 0)]).unwrap();
        assert!(
            ix.add(vec![VecInput {
                vector: vec![1.0; 4],
                ..input(2, 9, 0)
            }])
            .is_err()
        );
        drop(ix);
        // 次元が違えば作り直す。
        let (ix, opened) = VecIndex::open(&dir, 16).unwrap();
        assert!(matches!(opened, Opened::NeedsRebuild(_)));
        assert!(ix.snapshot().is_empty());
        drop(ix);
        // ハッシュが合わなければ作り直す。
        let (ix, _) = VecIndex::open(&dir, 8).unwrap();
        ix.add(vec![input(2, 9, 1)]).unwrap();
        drop(ix);
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            if e.file_name().to_string_lossy().starts_with("seg-") {
                std::fs::write(e.path().join("vec.i8"), b"broken").unwrap();
            }
        }
        let (_, opened) = VecIndex::open(&dir, 8).unwrap();
        assert!(matches!(opened, Opened::NeedsRebuild(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn many_small_additions_are_merged() {
        let dir = temp();
        let (ix, _) = VecIndex::open(&dir, 8).unwrap();
        for n in 0..(MERGE_AT as u8 + 3) {
            ix.add(vec![input(n + 1, 9, usize::from(n) % 8)]).unwrap();
        }
        assert!(ix.snapshot().segments.len() <= MERGE_AT);
        assert_eq!(ix.snapshot().len(), MERGE_AT + 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_current_and_previous_generations_are_kept() {
        let dir = temp();
        for key in ["a", "b", "c"] {
            std::fs::create_dir_all(dir.join(format!("vec-{key}"))).unwrap();
        }
        std::fs::create_dir_all(dir.join("chunk-lex")).unwrap();
        assert_eq!(prune_generations(&dir, &["c", "b"]), 1);
        assert!(!dir.join("vec-a").exists());
        assert!(dir.join("vec-b").exists() && dir.join("chunk-lex").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quantized_scores_match_float_order() {
        let a = [0.3f32, -0.2, 0.9, 0.1, 0.0, 0.4, -0.5, 0.2];
        let b = [0.25f32, -0.1, 0.8, 0.2, 0.1, 0.3, -0.4, 0.1];
        let c = [-0.3f32, 0.2, -0.9, 0.1, 0.0, -0.4, 0.5, -0.2];
        let (qa, sa) = quantize(&a);
        let (qb, sb) = quantize(&b);
        let (qc, sc) = quantize(&c);
        let ab = dot_i8(&qa, &qb) as f32 * sa * sb;
        let ac = dot_i8(&qa, &qc) as f32 * sa * sc;
        assert!(ab > 0.9 && ac < -0.9, "{ab} {ac}");
    }
}
