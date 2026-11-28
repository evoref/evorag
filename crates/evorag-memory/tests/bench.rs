//! ベンチマーク (09 §5.1): BM-109 (B3 と B4 を合わせた全件の導出、1 万件の合成データ。S-17 の 6、08 §2.6 の
//! M5.3)。`cargo xtask bench` で流し、`EVORAG_BENCH_OUT` があれば `<そこ>/BM-109.json` に書く (docs/perf/<版>/)。
//!
//! 合成データは 04 §5.3 の「関門の結果の覚え」の注の形: 625 の区切り × 8 往復、1 往復に候補 2 件 (住まいと趣味)
//! で 1 万件。発話は前後に雑談の文を持つ区切りに近い長さにし、往復ごとに応答を置き、16 往復に 1 回は住まいの訂正に
//! する。指示は住まいの値の `end` 300 件、趣味の値の忘れると private 50 件ずつ。値は作った語だけ (11 §9.7)。
//!
//! 測るもの: 覚えの無い導出 (起動の後の最初の導出。目安は無く記録する)、覚えの効いた導出 (同じ入力)、覚えの効いた
//! 導出 (区切りを 1 つ足した後。会話が進んだときの導き直し)。目安の 1 秒は、覚えの効いた導出に当てる (04 §5.3)。

mod derive_support;

use std::path::Path;
use std::time::Instant;

use derive_support::{Cand, World, unit};
use evorag_memory::DirectiveOp;
use evorag_memory::b3::Rules;
use evorag_memory::derive::{CorrectionReason, Derived, FoldNote, GateCache};
use evorag_memory::{DirectiveBody, ValueKeys};

/// 区切りの数と、区切りの往復の数 (1 往復に候補 2 件で 1 万件)。
const SEGMENTS: u32 = 625;
const PAIRS: u32 = 8;
/// `end` の件数と、忘れる・private のそれぞれの件数 (04 §5.3 の注)。
const ENDS: u32 = 300;
const OTHERS: u32 = 50;
/// 覚えの効いた導出の目安 (04 §5.3)。
const TARGET_MS: f64 = 1_000.0;
/// 覚えの無い導出と、覚えの効いた導出を流す回数 (中央値を採る)。
const COLD_RUNS: usize = 3;
const WARM_RUNS: usize = 5;

const CHAT: &str =
    "今日は天気が良かったので、近所の公園まで散歩に出かけて、帰りにパン屋に寄りました。";
const MORE: &str = "夕方からは少し雨が降りそうなので、洗濯物は早めに取り込んでおこうと思います。";

fn dict_directive(op: DirectiveOp, aspect: &str, value: &str) -> DirectiveBody {
    let mut b = DirectiveBody::new(op);
    b.entity = Some("self".to_owned());
    b.aspect = Some(aspect.to_owned());
    b.vks = ValueKeys::of(&derive_support::key(), value).to_hex();
    b.norm = Some(1);
    b
}

/// 区切り `seg` (往復 `PAIRS`) を足す。`positions` に発話の ID を足す。
fn add_segment(w: &mut World, seg: u32, positions: &mut Vec<evorag_types::Id>) {
    let session = u16::try_from(seg % 200).expect("fits");
    let mut turns = Vec::new();
    let mut cands = Vec::new();
    for k in 0..PAIRS {
        let i = seg * PAIRS + k;
        let correction = i % 16 == 15;
        let residence = if correction {
            format!("いや、町{}番じゃなくて町{i}番に住んでいます。", i - 1)
        } else {
            format!("私は町{i}番に住んでいます。")
        };
        let hobby = format!("趣味は将棋{i}です。");
        let text = format!("{CHAT}{residence}{hobby}{MORE}");
        let t = w.say(session, &text, Some(540), false);
        turns.push(t);
        positions.push(t);
        let turn = usize::try_from(k).expect("fits");
        cands.push(Cand {
            turn,
            at: CHAT.len(),
            unit: unit("residence", &format!("町{i}番")),
        });
        cands.push(Cand {
            turn,
            at: CHAT.len() + residence.len(),
            unit: unit("hobby", &format!("将棋{i}")),
        });
        w.answer(session, &format!("{MORE}{CHAT}"));
    }
    w.ledger(&turns, &cands, |_| Some("accept"));
}

/// 1 万件の世界と、発話の ID の並び。
fn mixed_world() -> (World, Vec<evorag_types::Id>) {
    let mut w = World::default();
    let mut positions = Vec::new();
    for seg in 0..SEGMENTS {
        add_segment(&mut w, seg, &mut positions);
    }
    let n = SEGMENTS * PAIRS;
    for k in 0..ENDS {
        let i = k * n / ENDS;
        let mut e = dict_directive(DirectiveOp::End, "residence", &format!("町{i}番"));
        e.entity_raw = Some("self".to_owned());
        e.turn = Some(positions[usize::try_from(i).expect("fits")]);
        w.directive(e);
    }
    for k in 0..OTHERS {
        let i = k * n / OTHERS;
        w.directive(dict_directive(
            DirectiveOp::Forget,
            "hobby",
            &format!("将棋{i}"),
        ));
        w.directive(dict_directive(
            DirectiveOp::Private,
            "hobby",
            &format!("将棋{}", i + 1),
        ));
    }
    (w, positions)
}

fn timed(f: impl FnOnce() -> Derived) -> (f64, Derived) {
    let t = Instant::now();
    let d = f();
    (t.elapsed().as_secs_f64() * 1_000.0, d)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// 結果を書く (`EVORAG_BENCH_OUT` があるとき)。
fn record(id: &str, value: serde_json::Value) {
    println!("{id}: {value}");
    if let Some(dir) = std::env::var_os("EVORAG_BENCH_OUT") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).expect("creates the output folder");
        let mut v = value;
        v["id"] = id.into();
        v["evorag"] = env!("CARGO_PKG_VERSION").into();
        v["target"] = std::env::consts::ARCH.into();
        v["os"] = std::env::consts::OS.into();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&v).expect("serializes") + "\n",
        )
        .expect("writes the result");
    }
}

/// BM-109: B3 と B4 を合わせた全件の導出 (1 万件)。覚えの効いた導出が 1 秒以内 (04 §5.3 の目安)。
#[ignore = "benchmark: run with cargo xtask bench"]
#[test]
fn bm_109_derive_10k() {
    let rules = Rules::bundled();
    let (mut w, mut positions) = mixed_world();
    let candidates: usize = w.ledgers.iter().map(|l| l.units.len()).sum();

    let mut cold_ms = Vec::new();
    let mut last = None;
    for _ in 0..COLD_RUNS {
        let mut cache = GateCache::new();
        let (ms, d) = timed(|| w.derive_with(&rules, &mut cache));
        cold_ms.push(ms);
        last = Some((d, cache));
    }
    let (cold, mut cache) = last.expect("ran");
    assert_eq!(cold.cache.hits, 0);

    let mut warm_ms = Vec::new();
    for _ in 0..WARM_RUNS {
        let (ms, d) = timed(|| w.derive_with(&rules, &mut cache));
        assert_eq!(d.cache.misses, 0);
        assert_eq!(d.units, cold.units);
        warm_ms.push(ms);
    }

    // 会話が進んだ: 区切りを 1 つ足して、覚えを使って導き直す (足した区切りの候補だけが覚えに無い)。
    add_segment(&mut w, SEGMENTS, &mut positions);
    let mut added_ms = Vec::new();
    let mut added = None;
    for _ in 0..WARM_RUNS {
        let mut c = cache.clone();
        let (ms, d) = timed(|| w.derive_with(&rules, &mut c));
        assert_eq!(d.cache.misses, 2 * usize::try_from(PAIRS).expect("fits"));
        added_ms.push(ms);
        added = Some(d);
    }
    let added = added.expect("ran");
    assert_eq!(
        added.units,
        w.derive_with(&rules, &mut GateCache::new()).units
    );

    assert!(cold.units.len() >= 9_900, "{} units", cold.units.len());
    assert!(cold.counts.correction(CorrectionReason::Applied) > 0);
    assert!(cold.counts.folded(FoldNote::Ended) > 0);
    assert!(cold.units.iter().any(|u| u.body.private));

    let cold_median = median(&mut cold_ms);
    let warm_median = median(&mut warm_ms);
    let added_median = median(&mut added_ms);
    record(
        "BM-109",
        serde_json::json!({
            "candidates": candidates,
            "units": cold.units.len(),
            "segments": SEGMENTS,
            "ends": ENDS,
            "forgets": OTHERS,
            "privates": OTHERS,
            "corrections_applied": cold.counts.correction(CorrectionReason::Applied),
            "cold_median_ms": cold_median,
            "cold_runs_ms": cold_ms,
            "warm_median_ms": warm_median,
            "warm_runs_ms": warm_ms,
            "warm_after_segment_median_ms": added_median,
            "warm_after_segment_runs_ms": added_ms,
            "target_warm_ms": TARGET_MS,
        }),
    );
    assert!(
        warm_median <= TARGET_MS && added_median <= TARGET_MS,
        "warm derive {warm_median:.1} ms / after a segment {added_median:.1} ms > {TARGET_MS} ms"
    );
}
