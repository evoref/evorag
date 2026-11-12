//! ベンチマーク (09 §5.1): BM-106 (ベクトルの全件走査)。`cargo xtask bench` で流し、`EVORAG_BENCH_OUT`
//! があれば `<そこ>/BM-106.json` に書く (docs/perf/<版>/)。
//!
//! verifies: AC-NFR-106-1

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use evorag_index::vec::{Unit, VecIndex, VecInput};
use evorag_store::testutil::TempDir;
use evorag_types::Id;

/// 10 万件、1024 次元 (bge-m3 と同じ)。
const ROWS: usize = 100_000;
const DIM: usize = 1024;
/// 応答の経路で集める候補の数 (05 §3.2)。
const TOP: usize = 50;

/// 決定的な擬似乱数 (xorshift)。
fn rng(seed: &mut u64) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    ((*seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn id(i: usize) -> Id {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
    Id::from_bytes(b)
}

fn record(id: &str, value: serde_json::Value) {
    println!("{id}: {value}");
    if let Some(dir) = std::env::var_os("EVORAG_BENCH_OUT") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut v = value;
        v["id"] = id.into();
        v["evorag"] = env!("CARGO_PKG_VERSION").into();
        v["target"] = std::env::consts::ARCH.into();
        v["os"] = std::env::consts::OS.into();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&v).unwrap() + "\n",
        )
        .unwrap();
    }
}

/// BM-106 / NFR-106: 10 万件のベクトルの全件走査 (int8、上位 50 件)。50 ms 以内。
#[ignore = "benchmark: run with cargo xtask bench"]
#[test]
fn bm_106_vector_scan() {
    let dir = TempDir::new("bm106");
    let (ix, _) = VecIndex::open(dir.path(), DIM).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let session = id(usize::MAX - 1);
    let mut batch = Vec::new();
    for i in 0..ROWS {
        batch.push(VecInput {
            id: id(i),
            session,
            unit: Unit::Chunk,
            vector: (0..DIM).map(|_| rng(&mut seed)).collect(),
        });
        if batch.len() == 10_000 {
            ix.add(std::mem::take(&mut batch)).unwrap();
        }
    }
    let snap = ix.snapshot();
    assert_eq!(snap.ids().len(), ROWS);
    let skip: HashSet<Id> = HashSet::new();
    let mut times = Vec::new();
    for _ in 0..21 {
        let q: Vec<f32> = (0..DIM).map(|_| rng(&mut seed)).collect();
        let t = Instant::now();
        let hits = snap.search(&q, TOP, &|row| skip.contains(&row.id));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(hits.len(), TOP);
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    record(
        "BM-106",
        serde_json::json!({
            "rows": ROWS, "dim": DIM, "top": TOP,
            "median_ms": median, "p95_ms": p95, "runs_ms": times, "target_ms": 50
        }),
    );
    assert!(median <= 50.0, "vector scan {median:.1} ms > 50 ms");
}
