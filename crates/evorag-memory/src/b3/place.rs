//! 値の位置の周り (文・節・括弧) を見る関門 (04 §5.2 の関門 3 の補い・3a・3b・4)。
//!
//! 旧エンジンの `gates.rs` (`sentences_at`・`Forms`・`has_cue`・`in_question_clause`・`QuoteRegions`・
//! `quoted_regions`) と `correction.rs` (`delimits`・`is_line_break`) の純粋な部分を写したもの。足したのは、問いの文の
//! 判定で略語の後ろの `.` で文を切らないこと ([`Cut::KeepAbbreviations`]) と、関門 3b の節と否定の印。

use std::ops::Range;

use unicode_normalization::UnicodeNormalization;

use super::rules::LanguageExtra;
use crate::chars::{graphemes, is_separator, is_unspaced_letter};
use crate::lexical::{Exclusions, any_outside, find_all, word_bounded};
use crate::prefilter::Prefiltered;
use crate::rules::{LanguageRules, Quote};
use crate::value_key::{Edges, Mapped, normalize_loose, normalize_v1};

/// 改行 (04 §6 の文の区切り)。旧エンジンの `correction::is_line_break` を写したもの。
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// 後ろに `.` が付いても文の終わりにしない語 (小文字で比べる)。文の分け方 v1 (`sentences.rs` の
/// `ABBREVIATIONS`) と同じ一覧を写したもの (試験 `abbreviations_match_splitter_v1` で v1 と同じことを確かめる)。
pub(super) const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "st", "jr", "sr", "prof", "vs", "etc", "e.g", "i.e", "inc", "ltd",
    "no", "fig", "approx", "u.s", "a.m", "p.m",
];

/// 文を切る規則。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cut {
    /// 04 §6 の「区切りの文字の判定」のとおり (関門 3a・3b)。
    Delimiters,
    /// 04 §6 の区切りから、略語 (文の分け方 v1 の一覧) の後ろの半角の `.` を除く (関門 3 の補いの問いの文の判定。
    /// 文が長くなるほど捨てる側に倒れる)。
    KeepAbbreviations,
}

/// 元の本文の書記素 `cluster` が文の区切りか (04 §6 の「区切りの文字の判定」)。全角の `。．！？` は常に、半角の
/// `.!?` は後ろが空白・改行・本文の終わりのときだけ区切り (小数・URL で文を切らない)。旧エンジンの
/// `correction::delimits` を写し、[`Cut::KeepAbbreviations`] を足したもの。
fn delimits(text: &str, cluster: &Range<usize>, cut: Cut) -> bool {
    match &text[cluster.clone()] {
        "。" | "．" | "！" | "？" | "｡" => true,
        s @ ("." | "!" | "?") => {
            let at_end = text[cluster.end..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace);
            at_end
                && !(s == "." && cut == Cut::KeepAbbreviations && after_abbreviation(text, cluster))
        }
        _ => false,
    }
}

/// `.` (`cluster`) の前の語 (空白から `.` まで。頭の `(`・`"`・`'`・`“`・`‘` を除く) が略語か。文の分け方 v1 の
/// `period_ends` と同じ判定。
fn after_abbreviation(text: &str, cluster: &Range<usize>) -> bool {
    let before = &text[..cluster.start];
    let word_start = before.rfind(char::is_whitespace).map_or(0, |p| {
        p + before[p..].chars().next().map_or(1, char::len_utf8)
    });
    let word = before[word_start..].trim_start_matches(['(', '"', '\'', '“', '‘']);
    ABBREVIATIONS.contains(&word.to_lowercase().as_str())
}

/// 本文 `text` の、範囲 `pos` が掛かる文の範囲 (前から順)。文は、区切りの文字の後と、改行で切る (区切りの文字は
/// 前の文に含める)。旧エンジンの `gates::sentences_at` を写し、`cut` を足したもの。
pub(super) fn sentences_at(text: &str, pos: &Range<usize>, cut: Cut) -> Vec<Range<usize>> {
    let mut sentences: Vec<Range<usize>> = Vec::new();
    let mut start = 0;
    for cluster in graphemes(text) {
        if text[cluster.clone()].chars().any(is_line_break) {
            if start < cluster.start {
                sentences.push(start..cluster.start);
            }
            start = cluster.end;
        } else if delimits(text, &cluster, cut) {
            sentences.push(start..cluster.end);
            start = cluster.end;
        }
    }
    if start < text.len() {
        sentences.push(start..text.len());
    }
    sentences.retain(|s| s.start < pos.end && pos.start < s.end);
    sentences
}

/// 読み取りの規則の語を照合する本文の 3 つの形 (生の本文・正規化 v1・照合の形、04 §5.6)。旧エンジンの
/// `gates::Forms` を写したもの。
pub(super) struct Forms<'t> {
    raw: &'t str,
    v1: String,
    loose: String,
}

impl<'t> Forms<'t> {
    pub(super) fn new(raw: &'t str) -> Self {
        Self {
            raw,
            v1: normalize_v1(raw),
            loose: normalize_loose(raw),
        }
    }

    /// 規則の語 (NFKC と case folding の後の形) のどれかが当たるか。生の本文 → 正規化 v1 → 照合の形の順に当てる。
    /// `bounded` なら ASCII の英数字で決めた語の境界で (英語)、でなければ部分一致で (日本語) 当てる。
    pub(super) fn mention(&self, words: &[String], bounded: bool) -> bool {
        words.iter().any(|w| {
            !find_all(self.raw, w, bounded).is_empty()
                || !find_all(&self.v1, w, bounded).is_empty()
                || !find_all(&self.loose, &normalize_loose(w), bounded).is_empty()
        })
    }

    /// [`Self::mention`] と同じ照合で、除外の語 `excludes` の一致 1 つにすっかり収まる一致を数えずに当たるか
    /// (関門 3b の否定の印)。
    fn mention_outside(&self, words: &[String], excludes: &[String], bounded: bool) -> bool {
        let loose_words: Vec<String> = words.iter().map(|w| normalize_loose(w)).collect();
        let loose_excludes: Vec<String> = excludes.iter().map(|w| normalize_loose(w)).collect();
        [
            (self.raw, words, excludes),
            (self.v1.as_str(), words, excludes),
            (self.loose.as_str(), &loose_words[..], &loose_excludes[..]),
        ]
        .into_iter()
        .any(|(form, w, x)| any_outside(form, w, &Exclusions::new(form, x, bounded), bounded))
    }

    /// 正規化 v1 か照合の形の末尾 (後ろの空白と、`?` 以外の文の区切りの文字を除く) が、規則の語のどれかで
    /// 終わるか。`bounded` なら、英数字で始まる語の前が英数字でないことも求める。
    fn ends_with_any(&self, words: &[String], bounded: bool) -> bool {
        let word = |c: char| c.is_ascii_alphanumeric();
        [(&self.v1, false), (&self.loose, true)]
            .into_iter()
            .any(|(form, loose)| {
                let body = form
                    .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '。' | '.' | '!'));
                words.iter().any(|w| {
                    let w = if loose { normalize_loose(w) } else { w.clone() };
                    body.strip_suffix(w.as_str()).is_some_and(|before| {
                        !(bounded && w.starts_with(word) && before.ends_with(word))
                    })
                })
            })
    }
}

/// 規則の言語の語を、語の境界で当てるか。
fn bounded(rules: &LanguageRules) -> bool {
    word_bounded(&rules.language)
}

/// 関門 3a: 文に、属性 `attribute` の手がかりの語 (どれかの言語の `<属性>_cue`) があるか。
pub(super) fn has_cue(sentence: &Forms<'_>, attribute: &str, languages: &[&LanguageRules]) -> bool {
    languages.iter().any(|r| {
        r.reading
            .cue(attribute)
            .is_some_and(|words| sentence.mention(words, bounded(r)))
    })
}

/// 関門 3 の補い: 値の位置 `pos` が、文 `sentence` (本文 `text` の範囲) の最後の節に掛かり、その文が問いの印
/// (どれかの言語の `question_end`) で終わるか。値の後ろから文の終わりまでに節の区切り (どれかの言語の
/// `clause_break`) があれば、値は最後の節に掛からない。
pub(super) fn in_question_clause(
    text: &str,
    sentence: &Range<usize>,
    pos: &Range<usize>,
    languages: &[&LanguageRules],
) -> bool {
    let whole = Forms::new(&text[sentence.clone()]);
    if !languages
        .iter()
        .any(|r| whole.ends_with_any(&r.reading.question_end, bounded(r)))
    {
        return false;
    }
    let tail = Forms::new(
        text.get(pos.end.min(sentence.end)..sentence.end)
            .unwrap_or(""),
    );
    !languages
        .iter()
        .any(|r| tail.mention(&r.reading.clause_break, bounded(r)))
}

/// 関門 3b: 値の位置 `pos` が掛かる、文 `sentence` の中の節 (本文 `text` の範囲)。節は、値の前の最後の節の区切り
/// の後から、値の後の最初の節の区切りの前まで。区切りは関門 3b の節の区切り (`polarity_clause_break`。接続の区切り
/// で、読点だけでは切らない。同梱のすべての言語)。区切りの一致は、正規化 v1 と照合の形の対応表 (04 §5.6) で元の
/// 位置に戻す。対応表を作れないときは文の全体。
pub(super) fn clause_at(
    text: &str,
    sentence: &Range<usize>,
    pos: &Range<usize>,
    extras: &[(&LanguageRules, &LanguageExtra)],
) -> Range<usize> {
    let s = &text[sentence.clone()];
    let maps = [Mapped::new(s), Mapped::loose(s)];
    if maps.iter().any(Option::is_none) {
        return sentence.clone();
    }
    let (mut start, mut end) = (sentence.start, sentence.end);
    let value = pos.start.max(sentence.start)..pos.end.min(sentence.end);
    for (r, x) in extras {
        let edges = if bounded(r) { Edges::Word } else { Edges::Any };
        for w in &x.polarity_clause_break {
            for (m, needle) in maps
                .iter()
                .flatten()
                .zip([normalize_v1(w), normalize_loose(w)])
            {
                for hit in m.find(&needle, edges) {
                    let h = hit.source.start + sentence.start..hit.source.end + sentence.start;
                    if h.end <= value.start {
                        start = start.max(h.end);
                    } else if h.start >= value.end {
                        end = end.min(h.start);
                    }
                }
            }
        }
    }
    start..end
}

/// 関門 3b: 節 `clause` (本文 `text` の範囲) の中で、値の位置 `pos` の外に否定の印 (どれかの言語の
/// `negation_mark`。除外の語 `negation_mark_exclude` の一致にすっかり収まるものは数えない) があるか。値の中の印は
/// 数えない (値の意味の一部なので)。日本語の印は、述語が節の最後に来るので値の後ろだけで見る (「猫ではなく犬が
/// 好き」の「犬」に前の「なく」を当てない)。英語の印は値の前と後ろの両方で見る。
pub(super) fn has_negation(
    text: &str,
    clause: &Range<usize>,
    pos: &Range<usize>,
    extras: &[(&LanguageRules, &LanguageExtra)],
) -> bool {
    let head = Forms::new(
        text.get(clause.start..pos.start.max(clause.start))
            .unwrap_or(""),
    );
    let tail = Forms::new(text.get(pos.end.min(clause.end)..clause.end).unwrap_or(""));
    extras.iter().any(|(r, x)| {
        let b = bounded(r);
        let hit = |f: &Forms<'_>| f.mention_outside(&x.negation_mark, &x.negation_mark_exclude, b);
        hit(&tail) || (b && hit(&head))
    })
}

/// 関門 3b: 文に、値の意味が否定を含む属性の否定を終える語 (どれかの言語の `negative_end`) があるか。
pub(super) fn has_negative_end(
    sentence: &Forms<'_>,
    extras: &[(&LanguageRules, &LanguageExtra)],
) -> bool {
    extras
        .iter()
        .any(|(r, x)| sentence.mention(&x.negative_end, bounded(r)))
}

/// 送る本文 (前のふるいの後) の、引用の括弧の内側の範囲と、その括弧の属性の例外 (関門 4)。旧エンジンの
/// `gates::QuoteRegions` を写したもの。
#[derive(Debug, Clone)]
pub(super) struct QuoteRegions<'s> {
    regions: Vec<(Range<usize>, &'s [String])>,
}

impl<'s> QuoteRegions<'s> {
    /// すべての言語の括弧の組 `quotes` を `text` に当てる。括弧の文字は 1 文字ずつ NFKC にして比べる (全角の `＂`
    /// も `"` として数える)。
    pub(super) fn new(quotes: impl IntoIterator<Item = &'s Quote>, text: &str) -> Self {
        let view: Vec<(usize, char)> = text
            .char_indices()
            .map(|(i, c)| {
                let mut n = c.nfkc();
                match (n.next(), n.next()) {
                    (Some(x), None) => (i, x),
                    _ => (i, c),
                }
            })
            .collect();
        let regions = quotes
            .into_iter()
            .flat_map(|q| {
                quoted_regions(text, &view, &q.open, &q.close)
                    .into_iter()
                    .map(|r| (r, q.allow_attributes.as_slice()))
            })
            .collect();
        Self { regions }
    }

    /// `pos` が、`attribute` (辞書の属性の名前。自由記述の属性は `None` で、どの括弧の例外にも当たらない) を
    /// 許していない括弧の内側と 1 文字でも重なるか。
    pub(super) fn overlaps(&self, attribute: Option<&str>, pos: &Range<usize>) -> bool {
        self.regions.iter().any(|(inner, allow)| {
            !attribute.is_some_and(|a| allow.iter().any(|x| x == a))
                && inner.start < pos.end
                && pos.start < inner.end
        })
    }
}

/// 括弧の内側の範囲 (括弧を含まない)。閉じていない括弧は本文の終わりまでとみなす (安全の側に倒す)。旧エンジンの
/// `gates::quoted_regions` を写したもの。
///
/// `view` は `text` の文字ごとの (バイト位置, 比べる文字)。開きと閉じが同じ記号 (`"`) は、前が本文の頭・
/// 空白・記号・漢字かなのときだけ開き、後ろが本文の終わり・空白・記号・漢字かなのときだけ閉じる
/// (`27"` のインチの記号で組がずれないように)。
pub(super) fn quoted_regions(
    text: &str,
    view: &[(usize, char)],
    open: &str,
    close: &str,
) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let open: Vec<char> = open.chars().collect();
    let close: Vec<char> = close.chars().collect();
    if open.is_empty() || close.is_empty() {
        return out;
    }
    let at = |k: usize, pat: &[char]| {
        view.get(k..k + pat.len())
            .is_some_and(|w| w.iter().map(|x| x.1).eq(pat.iter().copied()))
    };
    let byte = |k: usize| view.get(k).map_or(text.len(), |x| x.0);
    let edge = |c: Option<&(usize, char)>| {
        c.is_none_or(|&(_, c)| is_separator(c) || is_unspaced_letter(c))
    };
    let mut k = 0;
    if open == close {
        let mut start: Option<usize> = None;
        while k < view.len() {
            if at(k, &open) {
                let after = k + open.len();
                let before = k.checked_sub(1).and_then(|j| view.get(j));
                match start {
                    Some(s) if edge(view.get(after)) => {
                        out.push(s..byte(k));
                        start = None;
                        k = after;
                        continue;
                    }
                    None if edge(before) => {
                        start = Some(byte(after));
                        k = after;
                        continue;
                    }
                    _ => {}
                }
            }
            k += 1;
        }
        if let Some(s) = start {
            out.push(s..text.len());
        }
        return out;
    }
    let mut stack: Vec<usize> = Vec::new();
    while k < view.len() {
        if at(k, &open) {
            k += open.len();
            stack.push(byte(k));
        } else if at(k, &close) {
            if let Some(s) = stack.pop() {
                out.push(s..byte(k));
            }
            k += close.len();
        } else {
            k += 1;
        }
    }
    out.extend(stack.into_iter().map(|s| s..text.len()));
    out
}

/// 保存した本文の範囲 `stored` を、前のふるいの後の本文 (送る本文) の範囲に直す ([`Prefiltered::to_stored`] の
/// 逆)。範囲が除いた行に掛かるか、2 つの片にまたがるなら `None`。
///
/// 送る本文の位置 `s` から保存した本文の位置への写しは、送る本文の中で狭義に増えるので、二分探索で探す。
pub(super) fn to_sent(filtered: &Prefiltered, stored: &Range<usize>) -> Option<Range<usize>> {
    let len = stored.end.checked_sub(stored.start)?;
    let text_len = filtered.text().len();
    let (mut lo, mut hi) = (0usize, text_len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match filtered.to_stored(mid..mid) {
            Some(r) if r.start < stored.start => lo = mid + 1,
            _ => hi = mid,
        }
    }
    let sent = lo..lo.checked_add(len)?;
    (filtered.to_stored(sent.clone()).as_ref() == Some(stored)).then_some(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions<'t>(text: &'t str, open: &str, close: &str) -> Vec<&'t str> {
        let view: Vec<(usize, char)> = text.char_indices().collect();
        quoted_regions(text, &view, open, close)
            .into_iter()
            .map(|r| &text[r])
            .collect()
    }

    #[test]
    fn quoted_regions_pair_and_nest() {
        assert_eq!(regions("a「b「c」d」e", "「", "」"), ["c", "b「c」d"]);
        assert_eq!(regions("x \"y\" z \"w", "\"", "\""), ["y", "w"]);
        assert_eq!(regions("a」b", "「", "」"), Vec::<&str>::new());
    }

    #[test]
    fn symmetric_quotes_need_an_edge_on_the_outside() {
        assert_eq!(
            regions(
                "My 27\" monitor; the doc says \"pirate speak\" lol",
                "\"",
                "\""
            ),
            ["pirate speak"]
        );
        assert_eq!(regions("妻は\"東京\"と言った", "\"", "\""), ["東京"]);
        assert_eq!(regions("\"a\"b c\"", "\"", "\""), ["a\"b c"]);
    }

    #[test]
    fn to_sent_inverts_to_stored() {
        let stored = "前の行\n> 引用\n後ろの行です\n```\nコード\n```\n終わり";
        let f = Prefiltered::from_stored(stored);
        for needle in ["前の行", "後ろの行です", "終わり", "の行で"] {
            let at = stored.find(needle).expect("present");
            let r = at..at + needle.len();
            let sent = to_sent(&f, &r).expect("kept");
            assert_eq!(&f.text()[sent.clone()], needle, "{needle:?}");
            assert_eq!(f.to_stored(sent), Some(r));
        }
        for needle in ["引用", "コード", "行\n> 引"] {
            let at = stored.find(needle).expect("present");
            assert_eq!(to_sent(&f, &(at..at + needle.len())), None, "{needle:?}");
        }
    }

    /// 写した略語の一覧は、文の分け方 v1 で文を切らない語と同じ。
    #[test]
    fn abbreviations_match_splitter_v1() {
        for a in ABBREVIATIONS {
            let text = format!("x {a}. y");
            assert_eq!(crate::sentences::split_v1(&text).len(), 1, "{a:?}");
        }
        assert_eq!(crate::sentences::split_v1("x dog. y").len(), 2);
    }

    #[test]
    fn question_sentences_keep_abbreviations_only_when_asked() {
        let text = "Is Osaka near Dr. Kim's place?";
        let p = text.find("Osaka").expect("present");
        let pos = p..p + "Osaka".len();
        let cut = |c| {
            sentences_at(text, &pos, c)
                .into_iter()
                .map(|r| &text[r])
                .collect::<Vec<_>>()
        };
        assert_eq!(cut(Cut::Delimiters), ["Is Osaka near Dr."]);
        assert_eq!(cut(Cut::KeepAbbreviations), [text]);
    }
}
