//! 正規化と単位 (05 §2.1)。索引と問いで同じ規則を使う。規則はここ 1 か所に置き、版を持つ。
//!
//! 1. NFKC。
//! 2. 大文字・小文字の統一 (小文字にする)。
//! 3. カタカナをひらがなに統一する (長音の「ー」は残す)。
//! 4. 空白を 1 つにまとめる。
//! 5. 記号 (Unicode の P・S 類) は区切りとして扱う。ただし、数字に挟まれた桁区切りの「,」は詰める。
//!
//! 単位: 区切りの中の文字の bi-gram (区切りが 1 文字ならその字)、漢字 1 文字、英数字の並び。
//! 各単位に「内容の単位」(元の表記でひらがなだけではない) かを添える (しきい値の比に使う)。

use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

/// 規則の版。変えたら上げる (索引は作り直す)。
pub const RULES_VERSION: u32 = 1;

/// 1 つの単位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub term: String,
    /// 元の表記 (かなの統一の前) でひらがなだけではない単位。
    pub content: bool,
}

fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{309F}').contains(&c)
}

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c.general_category_group(),
            GeneralCategoryGroup::Separator
                | GeneralCategoryGroup::Punctuation
                | GeneralCategoryGroup::Symbol
        )
}

/// 1〜2 と 4〜5 を当てた文字の列 (区切りは空白 1 つ)。かなの統一の前。
fn folded(text: &str) -> Vec<char> {
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

fn unify_kana(c: char) -> char {
    if ('\u{30A1}'..='\u{30F6}').contains(&c) {
        char::from_u32(c as u32 - 0x60).unwrap_or(c)
    } else {
        c
    }
}

/// 正規化した文字列 (表示と試験のため)。
pub fn normalize(text: &str) -> String {
    folded(text).into_iter().map(unify_kana).collect()
}

/// 単位の列 (重複を含む)。
pub fn units(text: &str) -> Vec<Unit> {
    let raw = folded(text);
    let mut out = Vec::new();
    for seg in raw.split(|&c| c == ' ').filter(|s| !s.is_empty()) {
        let norm: Vec<char> = seg.iter().copied().map(unify_kana).collect();
        if norm.len() == 1 {
            out.push(Unit {
                term: format!("u:{}", norm[0]),
                content: !is_hiragana(seg[0]),
            });
        } else {
            for i in 0..norm.len() - 1 {
                out.push(Unit {
                    term: norm[i..i + 2].iter().collect(),
                    content: !(is_hiragana(seg[i]) && is_hiragana(seg[i + 1])),
                });
            }
        }
        for &c in norm.iter().filter(|c| is_kanji(**c)) {
            out.push(Unit {
                term: format!("u:{c}"),
                content: true,
            });
        }
        // 英数字の並びは語の単位も加える。
        let mut word = String::new();
        for &c in norm.iter().chain(std::iter::once(&' ')) {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                word.push(c);
            } else if !word.is_empty() {
                out.push(Unit {
                    term: format!("w:{}", std::mem::take(&mut word)),
                    content: true,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        units(text).into_iter().map(|u| u.term).collect()
    }

    /// AC-FR-401-1: 全角と半角・大文字と小文字・ひらがなとカタカナだけが違う語は、同じ単位になる。
    #[test]
    fn width_case_and_kana_differences_vanish() {
        assert_eq!(terms("ＴＯＥＩＣ"), terms("toeic"));
        assert_eq!(terms("ニンニク"), terms("にんにく"));
        assert_eq!(terms("ｶﾚｰ"), terms("かれー"));
        assert_eq!(normalize("Ｐｙｔｈｏｎ　と  ＶＥＮＶ"), "python と venv");
    }

    #[test]
    fn symbols_split_and_digit_commas_join() {
        assert_eq!(normalize("予算３，０００円！"), "予算3000円");
        assert_eq!(normalize("a,b「c」"), "a b c");
        assert_eq!(normalize("1,23"), "1 23");
    }

    #[test]
    fn bigrams_kanji_and_words() {
        assert_eq!(
            terms("京都へ"),
            ["京都", "都へ", "u:京", "u:都"].map(String::from)
        );
        assert_eq!(terms("x"), ["u:x", "w:x"].map(String::from));
        assert_eq!(
            terms("ssd 512gb"),
            ["ss", "sd", "w:ssd", "51", "12", "2g", "gb", "w:512gb"].map(String::from)
        );
    }

    #[test]
    fn hiragana_only_units_are_not_content() {
        let u = units("ですか");
        assert!(u.iter().all(|u| !u.content));
        // カタカナで書いた語は、ひらがなに統一しても内容の単位。
        let u = units("ニンニク");
        assert!(u.iter().all(|u| u.content));
        assert_eq!(u[0].term, "にん");
        let u = units("長音ー");
        assert!(u.iter().any(|u| u.term == "音ー"));
    }
}
