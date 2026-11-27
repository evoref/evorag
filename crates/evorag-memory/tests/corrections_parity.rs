//! v0.5 の訂正の候補 (`corrections::find_holes`) が、v0.4 の旧エンジンの候補 (`find_correction`) と同じ型・同じ穴を
//! 見つけることを、同じ本文の並びで比べる (04 §6、08 §2.6 の「旧エンジンとの比較」)。
//!
//! 旧エンジンを外すとき (M5.5) に、この試験も外す。
//!
//! 違ってよいのは次だけ (どれもこの並びには現れない): 旧エンジンは穴の本文を 200 文字 (`DIR_MAX_CHARS`) で切り、
//! v0.5 は範囲を切らない (ここでは v0.5 の側を同じに切って比べる)。穴を元の本文に戻せないとき、旧エンジンは
//! 正規化の後の本文を使い、v0.5 は候補にしない。

use std::path::PathBuf;

use evorag_memory::corrections::find_holes;
use evorag_memory::{DIR_MAX_CHARS, find_correction};

fn cap(s: &str) -> String {
    s.chars().take(DIR_MAX_CHARS).collect()
}

/// 同じ結果か確かめ、候補になったかを返す。
fn same(mode: &str, text: &str) -> bool {
    let old = find_correction(mode, text);
    let new = find_holes(mode, text);
    let new = new.map(|h| {
        (
            h.pattern.to_owned(),
            h.old.map(|r| cap(&text[r])),
            cap(&text[h.new]),
        )
    });
    let old = old.map(|m| (m.pattern, m.dir_old, m.dir_new));
    assert_eq!(new, old, "{mode}: {text:?}");
    new.is_some()
}

/// v0.3 の訂正の事例 (`tests/memory-cases/corrections.toml`) と、v0.5 の事例の訂正の発話。
#[test]
fn the_bundled_cases_find_the_same_holes() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/memory-cases");
    let read = |p: &str| -> toml::Value {
        let t = std::fs::read_to_string(dir.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        toml::from_str(&t).unwrap_or_else(|e| panic!("{p}: {e}"))
    };
    let doc = read("corrections.toml");
    let mut n = 0;
    for case in doc["case"].as_array().expect("[[case]]") {
        let s = |k: &str| case.get(k).and_then(toml::Value::as_str);
        let _ = same(s("mode").unwrap_or("chat"), s("text").expect("text"));
        n += 1;
    }
    // v0.5 の事例の発話 (`turn` の段の `text`)。
    for file in [
        "v0.5/forget.toml",
        "v0.5/governance.toml",
        "v0.5/derive.toml",
        "v0.5/attacks.toml",
        "v0.5/faults.toml",
    ] {
        let doc = read(file);
        for case in doc["case"].as_array().expect("[[case]]") {
            for step in case
                .get("step")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(t) = step
                    .get("turn")
                    .and_then(|t| t.get("text"))
                    .and_then(toml::Value::as_str)
                {
                    let _ = same("chat", t);
                    n += 1;
                }
            }
        }
    }
    assert!(n > 100, "{n} texts");
}

/// 型の固定の部分・値・区切り・読点・空白・引用の行・囲みを、決まった順の擬似乱数でつないだ本文。
#[test]
fn generated_texts_find_the_same_holes() {
    const PARTS: &[&str] = &[
        "大阪",
        "東京",
        "京都",
        "東大阪",
        "いや",
        "そう",
        "出身は",
        "かよ",
        "さよ",
        "です",
        "よ",
        "だよ",
        "じゃなくて",
        "ではなくて",
        "ではなく",
        "じゃなく",
        "じゃない",
        "じゃないです",
        "ではありません",
        "は間違いで",
        "は間違いです",
        "の間違い",
        "違う",
        "正しくは",
        "間違い",
        "訂正",
        "、",
        "，",
        "。",
        "．",
        "！",
        "？",
        " ",
        "\n",
        "\r",
        "\r\n",
        "\u{2028}",
        "> ",
        "```\n",
        "「",
        "」",
        "(",
        ")",
        "Osaka",
        "Tokyo",
        "Kyoto",
        "Dr.",
        "St. Louis",
        "1.2",
        "1.3",
        "Yahoo!",
        "Google",
        ", not ",
        "not ",
        " but ",
        "it's ",
        "It’s ",
        "it isn't ",
        "i meant ",
        "I meant ",
        " rather than ",
        " instead of ",
        "actually ",
        " is wrong, ",
        " was wrong, ",
        "no, ",
        ".",
        "!",
        "?",
        ", ",
        "ＯＳＡＫＡ",
        "ｷｮｳﾄ",
    ];
    let mut seed: u64 = 0x5eed_2026_1128;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(seed >> 33).expect("fits") % n
    };
    let mut found = 0;
    for _ in 0..6_000 {
        let len = 2 + next(10);
        let text: String = (0..len).map(|_| PARTS[next(PARTS.len())]).collect();
        found += usize::from(same("chat", &text));
    }
    // 候補になる本文も十分に含む。
    assert!(found > 300, "{found} candidates");
}
