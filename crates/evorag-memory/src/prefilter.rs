//! 前のふるい (04 §5.2): 利用者の発話を抽出の LLM に送るかを、決定論で決める。
//!
//! - コードブロック (``` か ~~~ の囲み) と、引用の記号 (`>`・`＞`) で始まる行を除く。送る本文は残りの行を
//!   つないだもので、送る本文の位置を保存した本文の位置に戻す対応表を持つ (関門 1 の span に使う)。
//! - 送らない理由は、モード → 空 → 挨拶・相槌 → 短い の順に当て、最初に当たったものにする。
//! - 挨拶・相槌の一覧は安全の規則として全言語を合わせて当て、短さのしきい値は、残りの本文の文字種から決めた
//!   言語のものを使う。

use std::fmt;
use std::ops::Range;

use unicode_normalization::UnicodeNormalization;

use crate::chars::is_separator;
use crate::lexical::evidence_language;
use crate::rules::{all_language_rules, fold, language_rules};

/// 抽出の対象にするセッションのモード (04 §5.2)。ほかのモード (知らないものを含む) は飛ばす。
pub const MODE_CHAT: &str = "chat";

/// 送らない理由 (04 §5.2)。並びは当てる順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkipReason {
    /// チャットのセッションではない。
    Mode,
    /// コードブロックと引用の行を除くと何も残らない。
    Empty,
    /// 挨拶・相槌の一覧に一致する。
    Greeting,
    /// 内容の単位 (日本語) か語 (英語) が、言語ごとのしきい値に足りない。
    Short,
}

impl SkipReason {
    /// 数えるときの名前 (`tests/memory-cases/README.md` の `reason`)。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Empty => "empty",
            Self::Greeting => "greeting",
            Self::Short => "short",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 送る本文の、保存した本文の続いた 1 片。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    /// 送る本文での始まり (バイト)。
    sent: usize,
    /// 保存した本文での始まり (バイト)。
    stored: usize,
    len: usize,
}

/// コードブロックと引用の行を除いた本文と、保存した本文への位置の対応 (04 §5.2)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefiltered {
    text: String,
    pieces: Vec<Piece>,
}

impl Prefiltered {
    /// 保存した本文から、コードブロックと引用の行を除く。前後の空白も除く。
    pub fn from_stored(stored: &str) -> Self {
        let mut kept: Vec<Range<usize>> = Vec::new();
        let mut fence: Option<(char, usize)> = None;
        let mut at = 0;
        for line in stored.split_inclusive('\n') {
            let range = at..at + line.len();
            at = range.end;
            let lead = line.trim_start();
            if let Some((ch, len)) = fence {
                if fence_of(lead).is_some_and(|(c, n)| c == ch && n >= len) {
                    fence = None;
                }
                continue;
            }
            if let Some(f) = fence_of(lead) {
                fence = Some(f);
                continue;
            }
            if lead.starts_with(['>', '＞']) {
                continue;
            }
            match kept.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => kept.push(range),
            }
        }
        // 前後の空白を除く (位置の対応は片の端を詰めて保つ)。
        while let Some(first) = kept.first_mut() {
            let s = &stored[first.clone()];
            first.start += s.len() - s.trim_start().len();
            if first.start == first.end {
                kept.remove(0);
            } else {
                break;
            }
        }
        while let Some(last) = kept.last_mut() {
            last.end = last.start + stored[last.clone()].trim_end().len();
            if last.start == last.end {
                kept.pop();
            } else {
                break;
            }
        }
        let mut text = String::new();
        let mut pieces = Vec::with_capacity(kept.len());
        for r in kept {
            pieces.push(Piece {
                sent: text.len(),
                stored: r.start,
                len: r.len(),
            });
            text.push_str(&stored[r]);
        }
        Self { text, pieces }
    }

    /// 抽出の LLM に送る本文。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 送る本文の範囲を、保存した本文の範囲に直す (除いた分を足し戻す)。範囲が本文の外なら `None`。
    ///
    /// 始まりと終わりを別々に直すので、除いた行をまたぐ範囲は、除いた行を含む範囲になる。
    pub fn to_stored(&self, sent: Range<usize>) -> Option<Range<usize>> {
        if sent.start > sent.end || sent.end > self.text.len() {
            return None;
        }
        let piece_at = |pos: usize| {
            self.pieces
                .iter()
                .find(|p| p.sent <= pos && pos < p.sent + p.len)
        };
        let start = piece_at(sent.start).map(|p| p.stored + (sent.start - p.sent));
        if sent.is_empty() {
            return start.map(|s| s..s);
        }
        let p = piece_at(sent.end - 1)?;
        Some(start?..p.stored + (sent.end - p.sent))
    }
}

/// 囲みの行なら、その文字と長さ (3 以上)。
fn fence_of(lead: &str) -> Option<(char, usize)> {
    let c = lead.chars().next()?;
    let ch = match c {
        '`' | '｀' => '`',
        '~' => '~',
        _ => return None,
    };
    let n = lead.chars().take_while(|&x| x == c).count();
    (n >= 3).then_some((ch, n))
}

/// 前のふるいの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screened {
    /// 抽出に送る。
    Send(Prefiltered),
    /// 送らない。
    Skip(SkipReason),
}

/// 前のふるい (04 §5.2)。`mode` はセッションのモード、`stored` は保存した発話の本文。
pub fn prefilter(mode: &str, stored: &str) -> Screened {
    if mode != MODE_CHAT {
        return Screened::Skip(SkipReason::Mode);
    }
    let p = Prefiltered::from_stored(stored);
    if p.text.trim().is_empty() {
        return Screened::Skip(SkipReason::Empty);
    }
    if is_greeting(&p.text) {
        return Screened::Skip(SkipReason::Greeting);
    }
    if is_short(&p.text) {
        return Screened::Skip(SkipReason::Short);
    }
    Screened::Send(p)
}

/// 前後の空白と記号を除いた本文の全体が、挨拶・相槌の一覧 (全言語) のどれかと一致するか。
fn is_greeting(text: &str) -> bool {
    let folded = fold(text);
    let core = folded.trim_matches(is_separator);
    let core = core.split_whitespace().collect::<Vec<_>>().join(" ");
    all_language_rules()
        .iter()
        .any(|r| r.prefilter.greetings.contains(&core))
}

/// 言語ごとのしきい値に足りないか。言語は本文の文字種から決める。
fn is_short(text: &str) -> bool {
    let lang = evidence_language(text);
    let Some(rules) = language_rules(lang) else {
        return false;
    };
    let p = &rules.prefilter;
    if let Some(min) = p.min_content_units {
        return content_units(text) < usize::try_from(min).unwrap_or(usize::MAX);
    }
    if let Some(min) = p.min_words {
        return words(text) < usize::try_from(min).unwrap_or(usize::MAX);
    }
    false
}

/// 語の数 (空白で区切った並びのうち、文字か数字を含むもの)。
fn words(text: &str) -> usize {
    text.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
}

fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{309F}').contains(&c)
}

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

/// 内容の単位 (05 §2.1) の数 (重複を含む)。
///
/// 単位の作り方は検索の正規化 (`evorag-retrieval` の `normalize::units`) と同じにする。この crate は
/// `evorag-retrieval` より下の層にある (02 §4.2) ので、数えるのに要る分だけをここに持つ。
/// 単位: 区切りの中の文字の bi-gram (区切りが 1 文字ならその字)、漢字 1 文字、英数字の並び。
/// 内容の単位は、元の表記でひらがなだけではない単位。
fn content_units(text: &str) -> usize {
    let raw = folded_for_units(text);
    let mut count = 0;
    for seg in raw.split(|&c| c == ' ').filter(|s| !s.is_empty()) {
        if let [c] = seg {
            count += usize::from(!is_hiragana(*c));
        } else {
            count += seg
                .windows(2)
                .filter(|w| !(is_hiragana(w[0]) && is_hiragana(w[1])))
                .count();
        }
        count += seg.iter().filter(|c| is_kanji(**c)).count();
        // 英数字の並び (かなの統一は英数字を変えない)。
        let mut in_word = false;
        for &c in seg {
            let w = c.is_ascii_lowercase() || c.is_ascii_digit();
            if w && !in_word {
                count += 1;
            }
            in_word = w;
        }
    }
    count
}

/// 検索の正規化の 1・2・4・5 を当てた文字の列 (区切りは空白 1 つ)。
fn folded_for_units(text: &str) -> Vec<char> {
    let nfkc: Vec<char> = text.nfkc().flat_map(char::to_lowercase).collect();
    let mut out: Vec<char> = Vec::with_capacity(nfkc.len());
    for (i, &c) in nfkc.iter().enumerate() {
        // 桁区切りの「,」(数字の後で、続く 3 文字が数字) は詰める。
        if c == ','
            && i > 0
            && nfkc[i - 1].is_ascii_digit()
            && nfkc
                .get(i + 1..i + 4)
                .is_some_and(|d| d.iter().all(char::is_ascii_digit))
        {
            continue;
        }
        if is_separator(c) {
            if out.last().is_some_and(|&l| l != ' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    while out.last() == Some(&' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_units_follow_the_retrieval_units() {
        // 「大阪」: 大阪・u:大・u:阪。
        assert_eq!(content_units("大阪"), 3);
        // ひらがなだけの単位は数えない。
        assert_eq!(content_units("ですか"), 0);
        assert_eq!(content_units("ニンニク"), 3);
        // 「ssd 512gb」: ss・sd・w:ssd・51・12・2g・gb・w:512gb。
        assert_eq!(content_units("SSD 512GB"), 8);
        assert_eq!(content_units("x"), 2);
    }

    #[test]
    fn words_need_a_letter_or_digit() {
        assert_eq!(words("Osaka."), 1);
        assert_eq!(words("I live in Osaka ."), 4);
        assert_eq!(words("  "), 0);
    }

    #[test]
    fn positions_map_back_over_removed_lines() {
        let stored = "> quoted\nkept one\n```\ncode\n```\nkept two";
        let p = Prefiltered::from_stored(stored);
        assert_eq!(p.text(), "kept one\nkept two");
        let two = p.text().find("two").expect("present");
        let r = p.to_stored(two..two + 3).expect("inside");
        assert_eq!(&stored[r], "two");
        let r = p.to_stored(0..4).expect("inside");
        assert_eq!(&stored[r], "kept");
        // 除いた行をまたぐ範囲は、除いた行を含む。
        let whole = p.to_stored(0..p.text().len()).expect("inside");
        assert_eq!(&stored[whole], "kept one\n```\ncode\n```\nkept two");
        assert_eq!(p.to_stored(0..p.text().len() + 1), None);
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end_and_spaces_are_trimmed() {
        let p = Prefiltered::from_stored("  before\n```\nsecret\n");
        assert_eq!(p.text(), "before");
        assert_eq!(p.to_stored(0..6), Some(2..8));
        let p = Prefiltered::from_stored("＞ 引用\n  本文  ");
        assert_eq!(p.text(), "本文");
    }

    #[test]
    fn greetings_ignore_surrounding_symbols_and_case() {
        assert!(is_greeting("ありがとう！"));
        assert!(is_greeting("Thank   you!!"));
        assert!(!is_greeting("ありがとう、助かった"));
    }
}
