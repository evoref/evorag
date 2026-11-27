//! 訂正の候補の試験。v0.4 の `correction.rs` の照合の試験を写し (写した元の試験の名前を注に書く)、v0.5 で足した
//! 範囲・`[非表示]`・B2 への渡し方の試験を足す。

use std::ops::Range;

use evorag_types::{Id, PromptLanguage};

use super::matcher::{compile, variants};
use super::*;
use crate::b2::{Input, Layout, Pair};
use crate::rules::{RULE_LANGUAGES, all_language_rules};

fn id(n: u8) -> Id {
    let mut b = [0u8; 16];
    b[15] = n;
    Id::from_bytes(b)
}

fn text(turn: u8, s: &str) -> Text<'_> {
    Text {
        turn: id(turn),
        rev: 0,
        text: s,
        hidden: &[],
    }
}

/// 型と、穴の範囲の本文 (古い側, 新しい側)。
fn dir(t: &str) -> Option<(String, Option<String>, String)> {
    find_holes(MODE_CHAT, t).map(|h| {
        (
            h.pattern.to_owned(),
            h.old.map(|o| t[o].to_owned()),
            t[h.new].to_owned(),
        )
    })
}

fn sides(t: &str) -> (Option<String>, String) {
    let (_, old, new) = dir(t).unwrap_or_else(|| panic!("a match in {t:?}"));
    (old, new)
}

fn some(s: &str) -> Option<String> {
    Some(s.to_owned())
}

// ---- v0.4 の試験の写し ----------------------------------------------------------------------------------------

/// 写した元: `patterns_compile_for_every_bundled_rule`。
#[test]
fn patterns_compile_for_every_bundled_rule() {
    for lang in RULE_LANGUAGES {
        let rules = language_rules(lang).expect("bundled");
        for p in &rules.correction.patterns {
            assert!(compile(p).is_some(), "{lang}: {p}");
        }
    }
    assert!(
        compile("{old}{new}").is_none(),
        "no separator between holes"
    );
    assert!(compile("{new} or {new}").is_none(), "two new holes");
    assert!(compile("{old}、{new}").is_none(), "no fixed text");
}

/// 写した元: `longer_fixed_text_wins_and_the_negated_side_is_old`。
#[test]
fn longer_fixed_text_wins_and_the_negated_side_is_old() {
    assert_eq!(
        dir("大阪ではなくて東京です"),
        Some(("{old}ではなくて{new}".into(), some("大阪"), "東京".into()))
    );
    // 英語は新しい値が先に来る。
    let (p, old, new) = dir("It's Osaka, not Tokyo.").expect("a match");
    assert_eq!(p, "it's {new}, not {old}");
    assert_eq!((old, new.as_str()), (some("Tokyo"), "Osaka"));
    assert_eq!(
        sides("東京は大阪の間違いでした"),
        (some("東京"), "大阪".into())
    );
    // 読点だけで分ける 2 つの穴。
    let (p, old, new) = dir("家は東京、大阪ではなく").expect("a match");
    assert_eq!(p, "{new}、{old}ではなく");
    assert_eq!((old, new.as_str()), (some("大阪"), "家は東京"));
}

/// 写した元: `holes_stay_in_their_sentence`。
#[test]
fn holes_stay_in_their_sentence() {
    assert_eq!(
        sides("前置きです。大阪じゃなくて東京です。妻は花子です"),
        (some("大阪"), "東京".into())
    );
    assert_eq!(
        sides("前置き\n大阪じゃなくて東京").0,
        some("大阪"),
        "a line break ends the hole"
    );
    // URL の `.` では文を切らない。
    assert_eq!(
        sides("https://example.com/a じゃなくて東京の話です").0,
        some("https://example.com/a")
    );
    // 固定の部分の直後が文の区切りなら、穴が空なので当たらない。
    assert_eq!(dir("大阪じゃなく。"), None);
}

/// 穴の範囲の文の区切りは、文番号の文の分け方 v1 (04 §2) に寄せない (#269 で決めた、2026-11-27。04 §6 の「区切り
/// の文字の判定」)。2 つの分け方の食い違う事例で、穴がどう切れるかを固定する (v1 の文は、比べるために並べる)。
///
/// 写した元: `holes_use_the_correction_delimiters_not_splitter_v1`。
#[test]
fn holes_use_the_correction_delimiters_not_splitter_v1() {
    fn v1(t: &str) -> Vec<&str> {
        crate::sentences::split_v1(t)
            .into_iter()
            .map(|r| &t[r])
            .collect()
    }
    // 全角の `．`・`\r` だけの改行: 穴はそこで切れ、古い値が取れる (v1 は切らないので、穴が前の語まで伸びる)。
    for t in [
        "住まいは東京．大阪じゃなくて京都です",
        "東京\r大阪じゃなくて京都です",
    ] {
        assert_eq!(v1(t), [t], "{t:?}");
        assert_eq!(sides(t), (some("大阪"), "京都".into()), "{t:?}");
    }
    // 後ろが空白でない `!`: 穴は切れない (v1 はここで切るので、古い値の穴が空になる)。
    let t = "Yahoo!じゃなくてGoogleです";
    assert_eq!(v1(t), ["Yahoo!", "じゃなくてGoogleです"]);
    assert_eq!(sides(t), (some("Yahoo!"), "Google".into()));
    // 略語: 穴は略語の `.` で切れる (v1 は切らない)。値が穴の端に合わず、訂正は単位を変えない側に倒れる。
    let t = "I meant Dr. Smith, not Dr. Lee.";
    assert_eq!(v1(t), [t]);
    assert_eq!(
        dir(t),
        Some(("i meant {new}".to_owned(), None, "Dr".to_owned()))
    );
    let t = "It's St. Louis, not Chicago.";
    assert_eq!(v1(t), [t]);
    assert_eq!(sides(t), (some("Chicago"), "Louis".into()));
}

/// 写した元: `commas_spaces_and_sentence_final_yo_desu_are_not_distinguished`。
#[test]
fn commas_spaces_and_sentence_final_yo_desu_are_not_distinguished() {
    // 型の読点が無くても当たる。
    let (p, old, new) = dir("大阪じゃない東京だよ").expect("a match");
    assert_eq!(p, "{old}じゃないです、{new}");
    assert_eq!((old, new.as_str()), (some("大阪"), "東京だ"));
    // 型の文末の「です」が無くても当たる。
    let (p, _, _) = dir("大阪は間違い。東京").expect("a match");
    assert_eq!(p, "{old}は間違いです。{new}");
    // 全角の読点と余分な空白。
    assert_eq!(
        sides("大阪 じゃなくて ，東京"),
        (some("大阪"), "東京".into())
    );
    // 向きの違うアポストロフィ。
    let (p, _, _) = dir("It’s Osaka, not Tokyo").expect("a match");
    assert_eq!(p, "it's {new}, not {old}");
}

/// 写した元: `english_patterns_match_on_word_boundaries`。
#[test]
fn english_patterns_match_on_word_boundaries() {
    assert_eq!(dir("This is another cannot case"), None);
    assert!(dir("Osaka, not Tokyo").is_some());
    // 一次のふるいに当たらなければ型を当てない。
    assert_eq!(dir("Osaka and Tokyo"), None);
}

/// 写した元: `only_chat_sessions_after_the_prefilter_are_searched`。
#[test]
fn only_chat_sessions_after_the_prefilter_are_searched() {
    assert!(find_holes("chat", "大阪じゃなくて東京です").is_some());
    assert_eq!(find_holes("coding", "大阪じゃなくて東京です"), None);
    assert_eq!(find_holes("", "大阪じゃなくて東京です"), None);
    assert_eq!(
        find_holes("chat", "> 大阪じゃなくて東京です\nと言われた"),
        None
    );
    assert_eq!(find_holes("chat", "```\n大阪じゃなくて東京\n```"), None);
    assert_eq!(find_holes("chat", "  \n "), None);
    // 短くて抽出に送らない発話にも当てる。
    assert!(find_holes("chat", "大阪じゃなくて東京").is_some());
}

/// 長い発話には当てない。穴の範囲は長さで切らない (v0.4 は穴の本文を 200 文字で切っていた。古い値の 200 文字の
/// 上限は関門 4 で当てる)。
///
/// 写した元: `long_utterances_are_not_matched_and_hole_texts_are_capped`。
#[test]
fn long_utterances_are_not_matched_and_holes_are_not_capped() {
    let long = format!("{}大阪じゃなくて東京です", "あ".repeat(MATCH_MAX_CHARS));
    assert_eq!(dir(&long), None);
    let just = format!(
        "{}大阪じゃなくて東京です",
        "あ".repeat(MATCH_MAX_CHARS - 11)
    );
    let (old, _) = sides(&just);
    assert_eq!(old.expect("old").chars().count(), MATCH_MAX_CHARS - 11 + 2);
}

/// 写した元: `variants_drop_sentence_final_yo_and_desu`。
#[test]
fn variants_drop_sentence_final_yo_and_desu() {
    let v = variants("だよ、");
    assert!(v.contains(&vec!['だ', 'よ']) && v.contains(&vec!['だ']));
    // 「よ」「です」が多い固定の部分でも、変種は 2^8 までに抑える。
    assert!(variants(&"ですよ。".repeat(20)).len() <= 256);
}

/// 写した元: `adversarial_utterances_are_matched_quickly` (照合の部分)。
#[test]
fn adversarial_utterances_are_matched_quickly() {
    let inputs = [
        format!("{}違", "は".repeat(MATCH_MAX_CHARS - 10)),
        "not ".repeat(MATCH_MAX_CHARS / 4 - 1),
        format!("{}間違", "は".repeat(MATCH_MAX_CHARS - 10)),
        "じゃなくて".repeat(MATCH_MAX_CHARS / 5 - 1),
        format!("{}大阪ではなく", "東京、".repeat(MATCH_MAX_CHARS / 3 - 5)),
    ];
    for t in &inputs {
        let started = std::time::Instant::now();
        let _ = find_holes(MODE_CHAT, t);
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(200),
            "{} chars took {took:?}",
            t.chars().count()
        );
    }
    // 正規化で長くなる本文は、正規化の後の長さで切る (U+FDFA は NFKC で 18 文字になる)。
    let wide = format!("{}大阪じゃなくて東京", "\u{FDFA}".repeat(300));
    assert_eq!(dir(&wide), None);
}

/// `{old}` の穴は読点でも切る。文末でない「よ」は値に残し、文末の「です」は 1 回だけ除く。
///
/// 写した元: `the_old_hole_is_cut_at_commas_and_must_match_whole` (照合の部分。関門は B3 と導出で当てる)。
#[test]
fn the_old_hole_is_cut_at_commas() {
    assert_eq!(
        sides("いや、大阪じゃなくて京都です"),
        (some("大阪"), "京都".into())
    );
    assert_eq!(sides("東大阪じゃなくて京都です").0, some("東大阪"));
    assert_eq!(
        sides("かよじゃなくてさよです"),
        (some("かよ"), "さよ".into())
    );
    assert_eq!(sides("大阪じゃなくてですです").1, "です");
}

/// 根拠の範囲に依らず、穴は文で決まる (古い値の穴を値と同じ長さに縮められない)。
///
/// 写した元: `a_truncated_evidence_does_not_shrink_the_old_hole` (穴の部分)。
#[test]
fn the_old_hole_runs_to_the_sentence_start() {
    for (t, old) in [
        ("東大阪じゃなくて京都です", "東大阪"),
        ("いや大阪じゃなくて京都です", "いや大阪"),
        ("妻の実家の大阪じゃなくて京都です", "妻の実家の大阪"),
        ("東東京は大阪の間違いでした", "東東京"),
        ("1.2ではなくて1.3です", "1.2"),
        ("I work at Google, not Apple Music.", "Apple Music"),
    ] {
        assert_eq!(sides(t).0, some(old), "{t}");
    }
}

/// 写した元: `a_leading_interjection_does_not_pick_the_comma_pattern`。
#[test]
fn a_leading_interjection_does_not_pick_the_comma_pattern() {
    for t in ["いや、大阪ではなく京都です", "そう、大阪ではなく京都です"]
    {
        assert_eq!(sides(t), (some("大阪"), "京都".into()), "{t}");
    }
    assert_eq!(sides("東京、大阪ではなく"), (some("大阪"), "東京".into()));
}

/// 文の区切りは元の文字で決める。
///
/// 写した元: `word_boundaries_and_delimiters_use_the_whole_original_text` (照合の部分)。
#[test]
fn delimiters_use_the_original_characters() {
    // 半角の `.` は後ろが空白か本文の終わりのときだけ文を切る。全角の `．` は常に切る。
    assert_eq!(
        sides("Version 1.2, not 1.3 is right").0,
        some("1.3 is right")
    );
    assert_eq!(sides("前置き．大阪じゃなくて東京").0, some("大阪"));
    // 半角の `.` の後が読点なら、文を切らない。
    assert_eq!(
        sides("前置き.、大阪じゃなくて東京").0,
        some("大阪"),
        "the comma cuts the old hole"
    );
}

/// 古い値の穴の無い型。
#[test]
fn patterns_without_an_old_hole_have_no_old_range() {
    let (p, old, new) = dir("正しくは東京です").expect("a match");
    assert_eq!(
        (p.as_str(), old, new.as_str()),
        ("正しくは{new}", None, "東京")
    );
    let (p, old, new) = dir("Sorry, I meant Kyoto.").expect("a match");
    assert_eq!(
        (p.as_str(), old, new.as_str()),
        ("i meant {new}", None, "Kyoto")
    );
}

/// v0.3 の訂正の事例 (`tests/memory-cases/corrections.toml`) の、候補になるかと型から決まる向き。
#[test]
fn the_v03_correction_cases_find_the_same_candidates() {
    let doc: toml::Value = toml::from_str(include_str!(
        "../../../../tests/memory-cases/corrections.toml"
    ))
    .expect("corrections.toml parses");
    let cases = doc["case"].as_array().expect("[[case]]");
    assert!(cases.len() >= 20, "{} cases", cases.len());
    for case in cases {
        let s = |k: &str| case.get(k).and_then(toml::Value::as_str);
        let cid = s("id").expect("id");
        let t = s("text").expect("text");
        let mode = s("mode").unwrap_or(MODE_CHAT);
        let want = case
            .get("expect_candidate")
            .and_then(toml::Value::as_bool)
            .expect("expect_candidate");
        let found = find_holes(mode, t);
        assert_eq!(found.is_some(), want, "{cid}: {t:?}");
        let Some(h) = found else { continue };
        let d = case.get("direction").expect("direction");
        let new = d.get("new").and_then(toml::Value::as_str).expect("new");
        assert!(t[h.new.clone()].contains(new), "{cid}: new {new:?}");
        match (d.get("old").and_then(toml::Value::as_str), h.old) {
            (Some(old), Some(r)) => assert!(t[r].contains(old), "{cid}: old {old:?}"),
            (None, None) => {}
            (want, got) => panic!("{cid}: old {want:?} vs {got:?}"),
        }
    }
}

// ---- v0.5 で足した試験 ----------------------------------------------------------------------------------------

/// 同梱の規則の `[correction]` (全言語) を固定する。落ちたら [`CORRECTION_V1`] の代わりに版を上げ、ここの値を
/// 直す (版の合わない台帳の `corrections` を使わないため、04 §5.2.1)。
#[test]
fn correction_rules_are_pinned() {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for r in all_language_rules() {
        h.update(format!("{}:{:?}\n", r.language, r.correction).as_bytes());
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        (CORRECTION_V1, got.as_str()),
        (
            1,
            "0d88d733098cc92bd842363e0afdc1a50163e3e82cd4eea6666be3c1a9b8996a"
        ),
        "the [correction] rules changed: raise the correction version"
    );
}

/// 範囲は保存した本文の位置 (前のふるいで除いた行・前後の空白の分を足し戻す)。
#[test]
fn ranges_are_positions_in_the_stored_text() {
    let stored = "```\ncode じゃなくて\n```\n> 引用じゃなくて\n  いや、大阪じゃなくて京都です  ";
    let h = find_holes(MODE_CHAT, stored).expect("a match");
    assert_eq!(&stored[h.new.clone()], "京都");
    assert_eq!(&stored[h.old.clone().expect("old")], "大阪");
    assert!(stored.is_char_boundary(h.new.start) && stored.is_char_boundary(h.new.end));
    // 2 行目の訂正 (除いた行の後)。
    let stored = "> quoted, not this\nIt's Osaka, not Tokyo.";
    let h = find_holes(MODE_CHAT, stored).expect("a match");
    assert_eq!(
        (&stored[h.new], &stored[h.old.expect("old")]),
        ("Osaka", "Tokyo")
    );
}

/// 範囲は元の本文の書記素の範囲 (NFKC で形の変わる文字は、元の文字のまま指す)。
#[test]
fn ranges_cover_the_original_characters() {
    let t = "ＯＳＡＫＡじゃなくてＫＹＯＴＯです";
    assert_eq!(sides(t), (some("ＯＳＡＫＡ"), "ＫＹＯＴＯ".into()));
    let t = "ｵｵｻｶじゃなくてｷｮｳﾄです";
    assert_eq!(sides(t), (some("ｵｵｻｶ"), "ｷｮｳﾄ".into()));
}

/// 1 つの発話に型が 2 つ以上当たっても、最初に当たった 1 つ (固定の部分の長い型から。同じ型なら本文の前から)
/// だけを候補にする (04 §6)。
#[test]
fn one_candidate_per_utterance() {
    // 同じ型が 2 回: 前の一致。
    let (p, old, new) = dir("大阪じゃなくて東京です。名古屋じゃなくて横浜です。").expect("a match");
    assert_eq!(
        (p.as_str(), old, new.as_str()),
        ("{old}じゃなくて{new}", some("大阪"), "東京")
    );
    // 違う型: 本文の後ろにあっても、固定の部分の長い型 (「ではなくて」) が先。
    let (p, old, new) = dir("大阪じゃなく東京。名古屋ではなくて横浜です").expect("a match");
    assert_eq!(
        (p.as_str(), old, new.as_str()),
        ("{old}ではなくて{new}", some("名古屋"), "横浜")
    );
    // 英語。
    let (p, old, new) = dir("Not Tokyo but Osaka. I meant Kyoto.").expect("a match");
    assert_eq!(
        (p.as_str(), old, new.as_str()),
        ("not {old} but {new}", some("Tokyo"), "Osaka")
    );
}

/// 忘れるの `[非表示]` の範囲を持つ発話は候補にしない (04 §6 の「候補を見つける時機」)。範囲が穴に掛からなくても
/// 候補にしない。
#[test]
fn utterances_with_hidden_ranges_are_not_candidates() {
    let t = "前置きの話です。大阪じゃなくて京都です";
    assert!(find_candidate(MODE_CHAT, text(1, t), None).is_some());
    for hidden in [0..9, t.find("京都").map(|s| s..s + 6).expect("京都")] {
        let hidden: [Range<usize>; 1] = [hidden];
        let u = Text {
            hidden: &hidden,
            ..text(1, t)
        };
        assert!(find_candidate(MODE_CHAT, u, None).is_none(), "{hidden:?}");
    }
}

/// 候補は B2 の要求の組み立てにそのまま渡せる (台帳の `corr` に範囲と型が入る)。直前の応答は添えたまま渡す。
#[test]
fn candidates_feed_the_b2_layout() {
    let u1 = "出身は霧ヶ丘じゃなくて白樺台です。";
    let r1 = "霧ヶ丘のご出身なんですね。";
    let u2 = "It's Osaka, not Tokyo.";
    let pairs = [
        Pair {
            utterance: text(1, u1),
            reply: Some(text(2, r1)),
        },
        Pair {
            utterance: text(3, u2),
            reply: None,
        },
    ];
    let candidates: Vec<Candidate<'_>> = [
        find_candidate(MODE_CHAT, text(1, u1), None),
        find_candidate(MODE_CHAT, text(3, u2), Some(text(2, r1))),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[1].previous.map(|p| p.turn), Some(id(2)));
    let layout = Layout::new(&Input {
        language: PromptLanguage::Ja,
        pairs: &pairs,
        candidates: &candidates,
        correction_version: CORRECTION_V1,
    })
    .expect("layout");
    let counts: Vec<usize> = layout.pieces().map(|p| p.chars().count()).collect();
    let plan = layout.plan(&counts, usize::MAX / 4).expect("plan");
    let call = layout.render(&plan);
    let corr = call.requests[0].corr();
    assert_eq!(corr.len(), 2);
    let slice = |t: &'static str, r: crate::ledger::ByteRange| &t[r.start as usize..r.end as usize];
    assert_eq!(
        (
            corr[0].turn,
            corr[0].pattern.as_str(),
            slice(u1, corr[0].new)
        ),
        (id(1), "{old}じゃなくて{new}", "白樺台")
    );
    // `{old}` の穴は文の頭から (04 §6 の「古い値は穴の全体」。v0.4 と同じ)。
    assert_eq!(slice(u1, corr[0].old.expect("old")), "出身は霧ヶ丘");
    assert_eq!(
        (
            corr[1].turn,
            slice(u2, corr[1].new),
            slice(u2, corr[1].old.expect("old"))
        ),
        (id(3), "Osaka", "Tokyo")
    );
}
