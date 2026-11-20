//! 文の窓 (05 §3.4、v0.4): 添える往復の中で、問いに当たった文を中心に前後の文を足して引用にする。
//!
//! - 文の区切り: 。！？!? と改行。半角の `.` は、後ろが空白か終わりのときだけ区切る (「3.5」、URL・ドメインの
//!   中では区切らない)。既知の略語 (「Mr.」「Dr.」「e.g.」など) の後ろでも区切らない。閉じの括弧・引用符は前の文に
//!   含める。チャンクの規則 (`chunk.rs`) とは別で、索引を作り直さない。
//! - 文の点数: 問いの内容の単位 (05 §2.1) のうち、その文にあるものの重みの和。重みは、渡した文の集まりの中での
//!   出現の少なさ (ln(1 + 文の数 / 出現した文の数))。
//! - 問いの分析 (決定論): 件数・合計・一覧の意図、日付の表現、記憶を明示する言い方。

use std::collections::{HashMap, HashSet};

use crate::normalize::units;

/// 文の範囲 (UTF-8 のバイト位置。先頭と末尾の空白を除く)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sentence {
    pub start: usize,
    pub end: usize,
}

/// 後ろに `.` が付いても文の終わりにしない語 (小文字)。
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "st", "jr", "sr", "prof", "vs", "etc", "e.g", "i.e", "inc", "ltd",
    "no", "fig", "approx", "u.s", "a.m", "p.m",
];

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

/// 文に分ける。空の文は返さない。
#[must_use]
pub fn sentences(text: &str) -> Vec<Sentence> {
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
        push_trimmed(text, start, end, &mut out);
        start = end;
    }
    push_trimmed(text, start, text.len(), &mut out);
    out
}

fn push_trimmed(text: &str, start: usize, end: usize, out: &mut Vec<Sentence>) {
    let s = &text[start..end];
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    if lead + trail < s.len() {
        out.push(Sentence {
            start: start + lead,
            end: end - trail,
        });
    }
}

/// 文ごとの点数 (問いの内容の単位のうち、その文にあるものの重みの和)。重みは `sentences` の集まりで求める。
#[must_use]
pub fn sentence_scores(query: &str, sentences: &[&str]) -> Vec<f64> {
    let q: HashSet<String> = units(query)
        .into_iter()
        .filter(|u| u.content)
        .map(|u| u.term)
        .collect();
    if q.is_empty() {
        return vec![0.0; sentences.len()];
    }
    let per: Vec<HashSet<String>> = sentences
        .iter()
        .map(|s| {
            units(s)
                .into_iter()
                .filter(|u| u.content && q.contains(&u.term))
                .map(|u| u.term)
                .collect()
        })
        .collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for set in &per {
        for t in set {
            *df.entry(t.as_str()).or_default() += 1;
        }
    }
    let n = sentences.len() as f64;
    per.iter()
        .map(|set| {
            set.iter()
                .map(|t| (1.0 + n / df[t.as_str()] as f64).ln())
                .sum()
        })
        .collect()
}

/// 問いの分析の結果 (05 §3.1、v0.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryIntent {
    /// 件数・合計・一覧を問う。
    pub count: bool,
    /// 日付・時期の表現がある。
    pub date: bool,
    /// 以前の会話を明示して問う。
    pub explicit: bool,
}

const COUNT_EN: &[&str] = &[
    "how many",
    "how much",
    "in total",
    "total ",
    "altogether",
    "combined",
    "number of",
    "list ",
    "all the ",
    "all of the ",
    "each of",
    "every ",
];
const COUNT_JA: &[&str] = &[
    "何回",
    "何度",
    "いくつ",
    "何個",
    "何件",
    "何人",
    "何冊",
    "何本",
    "何枚",
    "何軒",
    "何か所",
    "何カ所",
    "何時間",
    "何日間",
    "合計",
    "全部で",
    "全部",
    "すべて",
    "全て",
    "一覧",
    "いくら",
    "どれだけ",
    "何種類",
];
const DATE_EN: &[&str] = &[
    "when ",
    "yesterday",
    "last week",
    "last month",
    "last year",
    " ago",
    "how long",
    "which day",
    "what date",
    "january",
    "february",
    "march",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "first ",
    "before ",
    "after ",
];
const DATE_JA: &[&str] = &[
    "いつ",
    "昨日",
    "先週",
    "先月",
    "去年",
    "昨年",
    "何日前",
    "何年",
    "何月",
    "前に",
    "後に",
    "最初",
    "最後",
];
const EXPLICIT_EN: &[&str] = &[
    "remember",
    "did i mention",
    "i mentioned",
    "i told you",
    "i said",
    "we talked",
    "we discussed",
    "previously",
    "last time",
    "earlier",
    "you told me",
    "you said",
    "you suggested",
    "you recommended",
];
const EXPLICIT_JA: &[&str] = &[
    "覚えて",
    "前に話",
    "以前",
    "話した",
    "言った",
    "伝えた",
    "教えた",
    "この前",
    "前回",
];

/// 問いを分析する (決定論。ほぼ 0 ms)。
#[must_use]
pub fn analyze(query: &str) -> QueryIntent {
    let q = format!(" {} ", query.to_lowercase());
    let any = |list: &[&str]| list.iter().any(|w| q.contains(w));
    QueryIntent {
        count: any(COUNT_EN) || any(COUNT_JA),
        date: any(DATE_EN) || any(DATE_JA) || q.chars().any(|c| c.is_ascii_digit()),
        explicit: any(EXPLICIT_EN) || any(EXPLICIT_JA),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<&str> {
        sentences(text)
            .into_iter()
            .map(|s| &text[s.start..s.end])
            .collect()
    }

    #[test]
    fn japanese_sentences_end_at_marks_and_keep_closers() {
        assert_eq!(
            split("今日は晴れ。「明日は？」と聞いた！\nはい"),
            ["今日は晴れ。", "「明日は？」", "と聞いた！", "はい"]
        );
    }

    #[test]
    fn periods_split_only_between_sentences() {
        assert_eq!(
            split("It costs 3.5 dollars. See example.com for more. Mr. Smith agreed."),
            [
                "It costs 3.5 dollars.",
                "See example.com for more.",
                "Mr. Smith agreed."
            ]
        );
        assert_eq!(split("Use e.g. salt. Done"), ["Use e.g. salt.", "Done"]);
        assert_eq!(split("Really?! Yes."), ["Really?!", "Yes."]);
    }

    #[test]
    fn empty_and_blank_lines_are_skipped() {
        assert!(sentences("").is_empty());
        assert_eq!(split("a\n\n\nb"), ["a", "b"]);
    }

    #[test]
    fn rare_query_words_score_higher() {
        let s = [
            "I like coffee in the morning.",
            "My dog is a golden retriever.",
            "The morning walk is nice.",
        ];
        let v = sentence_scores("what breed is my dog?", &s);
        assert!(v[1] > v[0] && v[1] > v[2], "{v:?}");
        assert_eq!(sentence_scores("", &s), vec![0.0; 3]);
    }

    #[test]
    fn queries_are_analyzed() {
        let a = analyze("How many weddings did I attend this year?");
        assert!(a.count);
        assert!(analyze("先月、何回ジムに行った?").count);
        assert!(analyze("When did I buy the bike?").date);
        assert!(analyze("Do you remember my dog's name?").explicit);
        assert_eq!(analyze("hello"), QueryIntent::default());
    }
}
