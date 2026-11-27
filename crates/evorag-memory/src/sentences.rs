//! 文の分け方 v1 (04 §2 の「文の分け方 v1」)。区切りの文番号 (04 §5.2) と、文の窓 (05 §3.8 の 1) が使う。
//!
//! - 区切り: `。`・`！`・`？`・`!`・`?` と改行 (`\n`)。半角の `.` は、後ろが空白 (`char::is_whitespace`) か本文の
//!   終わりのときだけ区切る (「3.5」、URL・ドメインの中の `.` では区切らない)。`.` の前の語 (空白から `.` まで。
//!   頭の `(`・`"`・`'`・`“`・`‘` を除く) が既知の略語 ([`ABBREVIATIONS`]) なら区切らない。
//! - 区切りの文字の直後に続く閉じの括弧・引用符と `。！？!?` は前の文に含める (改行の直後は、閉じの括弧・
//!   引用符だけを含める)。
//! - 文の範囲は UTF-8 のバイト位置で、頭と末尾の空白を除く。空白だけの文は返さない。判定は Unicode のスカラー
//!   値ごとで、書記素は見ない (区切りの文字の直後の結合文字は次の文の頭になる)。
//!
//! **v1 は凍結する**: 台帳の `sentences` の範囲と `inputs` の `splitter` (04 §5.2.1) がこの分け方に結び付き、
//! `splitter` の合わない台帳は有効でない (04 §5.2.1 の「有効な台帳」)。出力を 1 か所でも変えるときは、この関数を
//! 直さずに v2 (`split_v2` と `SPLITTER_V2`) を足し、台帳に書く版を上げる。試験 (`v1_is_frozen`) が落ちたら、
//! v1 の出力を元に戻す。旧エンジンのモジュール (`facts`・`fold`・`correction`・`gates` など) に依存しない。

use std::ops::Range;

/// 文の分け方 v1 の版。台帳の `inputs` の `splitter` に書く (04 §5.2.1)。
pub const SPLITTER_V1: u32 = 1;

/// 後ろに `.` が付いても文の終わりにしない語 (小文字で比べる)。v1 の一部なので、足し引きは v2 にする。
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "st", "jr", "sr", "prof", "vs", "etc", "e.g", "i.e", "inc", "ltd",
    "no", "fig", "approx", "u.s", "a.m", "p.m",
];

/// 文の分け方 v1 で `text` を文に分け、各文のバイト範囲を前から返す (頭と末尾の空白を除き、空の文は返さない)。
#[must_use]
pub fn split_v1(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let end_here = match c {
            '。' | '！' | '？' | '!' | '?' | '\n' => true,
            '.' => period_ends(text, i),
            _ => false,
        };
        if !end_here {
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, d)) = iter.peek() {
            if is_closer(d) || (c != '\n' && matches!(d, '。' | '！' | '？' | '!' | '?')) {
                end = j + d.len_utf8();
                iter.next();
            } else {
                break;
            }
        }
        push_trimmed(text, start..end, &mut out);
        start = end;
    }
    push_trimmed(text, start..text.len(), &mut out);
    out
}

fn is_closer(c: char) -> bool {
    matches!(
        c,
        '」' | '』' | '）' | ')' | '"' | '\'' | '”' | '’' | '】' | ']'
    )
}

/// `text` の `i` (バイト位置、`.` の位置) の `.` が文の終わりか。
fn period_ends(text: &str, i: usize) -> bool {
    let next = text[i + 1..].chars().next();
    if next.is_some_and(|c| !c.is_whitespace()) {
        return false;
    }
    // `.` の前の語 (空白まで)。
    let word_start = text[..i].rfind(char::is_whitespace).map_or(0, |p| {
        p + text[p..].chars().next().map_or(1, char::len_utf8)
    });
    let word = text[word_start..i].trim_start_matches(['(', '"', '\'', '“', '‘']);
    let lower = word.to_lowercase();
    !ABBREVIATIONS.contains(&lower.as_str())
}

fn push_trimmed(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let s = &text[range.clone()];
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    if lead + trail < s.len() {
        out.push(range.start + lead..range.end - trail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1 の golden: (入力, 期待する文)。範囲は、期待する文を前の文の終わりから探した位置と比べる。行を変えずに
    /// 足すのはよい。既存の行の期待を変える必要が出たら、v1 を直さずに v2 を足す (モジュールの注)。
    const GOLDEN: &[(&str, &[&str])] = &[
        // 空と空白だけ。
        ("", &[]),
        ("  \n \u{3000}\n", &[]),
        // 頭と末尾の空白を除く。全角の空白・NBSP も空白。
        ("  Hello.  World  ", &["Hello.", "World"]),
        ("Done.\u{3000}次へ", &["Done.", "次へ"]),
        ("Done.\u{a0}Next", &["Done.", "Next"]),
        // 小数・版の番号の `.` では切らない。
        (
            "It costs 3.5 dollars. v1.2 is out.",
            &["It costs 3.5 dollars.", "v1.2 is out."],
        ),
        ("体温は36.5度。平熱", &["体温は36.5度。", "平熱"]),
        // 略語の後ろでは切らない (大文字も、頭の括弧・引用符も除いて比べる)。
        (
            "Mr. Smith met Dr. Lee. MR. X agreed.",
            &["Mr. Smith met Dr. Lee.", "MR. X agreed."],
        ),
        (
            "Use e.g. salt, i.e. NaCl. Done",
            &["Use e.g. salt, i.e. NaCl.", "Done"],
        ),
        (
            "Meet at 5 p.m. tomorrow in the U.S. ok? Fine.",
            &["Meet at 5 p.m. tomorrow in the U.S. ok?", "Fine."],
        ),
        (
            "(Fig. 3) and (etc. more). End",
            &["(Fig. 3) and (etc. more).", "End"],
        ),
        // 略語と同じ綴りの語でも切らない (v1 の癖)。
        ("I said no. Then I left.", &["I said no. Then I left."]),
        // 末尾の略語は、最後の文として残る。
        ("I met Dr.", &["I met Dr."]),
        // 省略の `...` は、最後の `.` の後ろが空白なら切る。
        ("Wait... what?", &["Wait...", "what?"]),
        // URL・ドメインの `.` では切らない。`?`・`!` は URL の中でも切る (v1 の癖)。
        (
            "See example.com for more. Then https://a.example/x.html now.",
            &[
                "See example.com for more.",
                "Then https://a.example/x.html now.",
            ],
        ),
        (
            "Open https://example.com/s?q=1 now.",
            &["Open https://example.com/s?", "q=1 now."],
        ),
        ("Yahoo!ニュースを見た", &["Yahoo!", "ニュースを見た"]),
        // 続く `！？!?` と閉じの括弧・引用符は前の文に含める。
        ("Really?! Yes.", &["Really?!", "Yes."]),
        ("本当！？うん。", &["本当！？", "うん。"]),
        (
            "今日は晴れ。「明日は？」と聞いた！\nはい",
            &["今日は晴れ。", "「明日は？」", "と聞いた！", "はい"],
        ),
        (
            "彼は「行く。」と言った。",
            &["彼は「行く。」", "と言った。"],
        ),
        ("（本当？）そう", &["（本当？）", "そう"]),
        ("Why?) ok", &["Why?)", "ok"]),
        ("『終わり！』】次", &["『終わり！』】", "次"]),
        // `.` の直後が閉じの引用符なら、空白ではないので切らない (v1 の癖)。
        (
            "He said \"Stop.\" Then he left.",
            &["He said \"Stop.\" Then he left."],
        ),
        ("(See Fig. 3.) Next", &["(See Fig. 3.) Next"]),
        // 全角の `．`・半角の `｡` では切らない (v1 の癖)。
        ("全角の．では切らない．次", &["全角の．では切らない．次"]),
        ("半角の｡でも切らない", &["半角の｡でも切らない"]),
        // 改行は `\n` だけ。`\r\n` の `\r` は空白として除く。`\r` だけ・U+2028 では切らない (v1 の癖)。
        (
            "一行目\n二行目\r\n三行目\n\n  \n四",
            &["一行目", "二行目", "三行目", "四"],
        ),
        ("a\rb", &["a\rb"]),
        ("a\u{2028}b", &["a\u{2028}b"]),
        // 改行の直後の区切りの文字・閉じの括弧は、前の文に含めない (閉じの括弧は改行の後でも含める)。
        ("はい\n。", &["はい", "。"]),
        ("はい\n」次", &["はい\n」", "次"]),
        // 絵文字・ZWJ の並び・肌の色は分けない。区切りの直後の異体字セレクタは次の文の頭になる (v1 の癖)。
        (
            "家族👨\u{200d}👩\u{200d}👧と旅行！楽しかった👍🏽。次は？",
            &[
                "家族👨\u{200d}👩\u{200d}👧と旅行！",
                "楽しかった👍🏽。",
                "次は？",
            ],
        ),
        ("OK?\u{fe0f}次", &["OK?", "\u{fe0f}次"]),
        // 日本語と英語の混在。`.` の後ろが空白でなければ切らない。
        (
            "東京に住んでいます.I live in Tokyo. 好きなのはsushi!本当",
            &[
                "東京に住んでいます.I live in Tokyo.",
                "好きなのはsushi!",
                "本当",
            ],
        ),
        (
            "I moved to 大阪. 今は京都です。Next year, Kobe?",
            &["I moved to 大阪.", "今は京都です。", "Next year, Kobe?"],
        ),
    ];

    /// 期待する文を、前の文の終わりから順に探した範囲。
    fn expected_ranges(text: &str, parts: &[&str]) -> Vec<Range<usize>> {
        let mut at = 0;
        parts
            .iter()
            .map(|p| {
                let start = at
                    + text[at..]
                        .find(p)
                        .unwrap_or_else(|| panic!("{p:?} is not in {text:?} after byte {at}"));
                at = start + p.len();
                start..at
            })
            .collect()
    }

    #[test]
    fn v1_is_frozen() {
        for &(text, parts) in GOLDEN {
            let got = split_v1(text);
            let as_text: Vec<&str> = got.iter().map(|r| &text[r.clone()]).collect();
            assert_eq!(as_text, parts, "input {text:?}");
            assert_eq!(got, expected_ranges(text, parts), "input {text:?}");
        }
        assert_eq!(SPLITTER_V1, 1);
    }

    #[test]
    fn sentences_are_ordered_trimmed_and_non_empty() {
        for &(text, _) in GOLDEN {
            let got = split_v1(text);
            for w in got.windows(2) {
                assert!(w[0].end <= w[1].start, "input {text:?}: {got:?}");
            }
            for r in &got {
                let s = &text[r.clone()];
                assert!(!s.is_empty(), "input {text:?}: {got:?}");
                assert_eq!(s, s.trim(), "input {text:?}: {got:?}");
            }
        }
    }
}
