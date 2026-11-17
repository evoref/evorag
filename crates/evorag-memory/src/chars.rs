//! 関門と前のふるいが使う、文字の性質の判定 (一般カテゴリ・書記素の境界・文字体系・区切り)。
//!
//! 一般カテゴリは unicode-properties、書記素の境界は unicode-segmentation (UAX #29 の拡張書記素クラスタ) で
//! 判定する。どちらも workspace の依存で、検索の正規化 (05 §2.1) と同じ表を使う。

use std::ops::Range;

use unicode_normalization::char::is_combining_mark;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

/// 一般カテゴリの 2 文字の名前 (`rules/common.toml` の `forbidden_categories` の書き方)。
fn category_name(gc: GeneralCategory) -> &'static str {
    use GeneralCategory::*;
    match gc {
        UppercaseLetter => "Lu",
        LowercaseLetter => "Ll",
        TitlecaseLetter => "Lt",
        ModifierLetter => "Lm",
        OtherLetter => "Lo",
        NonspacingMark => "Mn",
        SpacingMark => "Mc",
        EnclosingMark => "Me",
        DecimalNumber => "Nd",
        LetterNumber => "Nl",
        OtherNumber => "No",
        ConnectorPunctuation => "Pc",
        DashPunctuation => "Pd",
        OpenPunctuation => "Ps",
        ClosePunctuation => "Pe",
        InitialPunctuation => "Pi",
        FinalPunctuation => "Pf",
        OtherPunctuation => "Po",
        MathSymbol => "Sm",
        CurrencySymbol => "Sc",
        ModifierSymbol => "Sk",
        OtherSymbol => "So",
        SpaceSeparator => "Zs",
        LineSeparator => "Zl",
        ParagraphSeparator => "Zp",
        Control => "Cc",
        Format => "Cf",
        Surrogate => "Cs",
        PrivateUse => "Co",
        Unassigned => "Cn",
    }
}

/// 関門 6 で判定できる一般カテゴリ (すべて)。
pub(crate) const SUPPORTED_CATEGORIES: &[&str] = &[
    "Lu", "Ll", "Lt", "Lm", "Lo", "Mn", "Mc", "Me", "Nd", "Nl", "No", "Pc", "Pd", "Ps", "Pe", "Pi",
    "Pf", "Po", "Sm", "Sc", "Sk", "So", "Zs", "Zl", "Zp", "Cc", "Cf", "Cs", "Co", "Cn",
];

/// `c` が一般カテゴリ `name` に属するか。知らない名前は `None` (規則の検査で弾く)。
pub(crate) fn in_category(c: char, name: &str) -> Option<bool> {
    SUPPORTED_CATEGORIES
        .contains(&name)
        .then(|| category_name(c.general_category()) == name)
}

fn in_ranges(c: char, ranges: &[(u32, u32)]) -> bool {
    let c = u32::from(c);
    ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// 検索の正規化 (05 §2.1) の「区切り」: 空白と、記号 (一般カテゴリ P・S・Z)。
pub(crate) fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c.general_category_group(),
            GeneralCategoryGroup::Separator
                | GeneralCategoryGroup::Punctuation
                | GeneralCategoryGroup::Symbol
        )
}
/// 文字体系 (関門 6 の「文字体系の混在」に要る分だけ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Script {
    Latin,
    Cyrillic,
    Greek,
    Other,
}

pub(crate) fn script(c: char) -> Script {
    const LATIN: &[(u32, u32)] = &[
        (0x0041, 0x005A),
        (0x0061, 0x007A),
        (0x00C0, 0x00D6),
        (0x00D8, 0x00F6),
        (0x00F8, 0x024F),
        (0x1D00, 0x1D25),
        (0x1E00, 0x1EFF),
        (0x2C60, 0x2C7F),
        (0xA720, 0xA7FF),
        (0xAB30, 0xAB64),
        (0xFB00, 0xFB06),
        (0xFF21, 0xFF3A),
        (0xFF41, 0xFF5A),
    ];
    const CYRILLIC: &[(u32, u32)] = &[
        (0x0400, 0x052F),
        (0x1C80, 0x1C8F),
        (0x2DE0, 0x2DFF),
        (0xA640, 0xA69F),
    ];
    const GREEK: &[(u32, u32)] = &[(0x0370, 0x03FF), (0x1F00, 0x1FFF)];
    if in_ranges(c, LATIN) {
        Script::Latin
    } else if in_ranges(c, CYRILLIC) {
        Script::Cyrillic
    } else if in_ranges(c, GREEK) {
        Script::Greek
    } else {
        Script::Other
    }
}

/// 1 つの語 (文字と結合文字の続き) に、ラテン文字と、キリル文字かギリシャ文字が混ざっているか。
pub(crate) fn has_mixed_script_word(text: &str) -> bool {
    let mut latin = false;
    let mut other = false;
    for c in text.chars() {
        if c.is_alphabetic() || is_combining_mark(c) {
            match script(c) {
                Script::Latin => latin = true,
                Script::Cyrillic | Script::Greek => other = true,
                Script::Other => {}
            }
            if latin && other {
                return true;
            }
        } else {
            latin = false;
            other = false;
        }
    }
    false
}

/// 書記素の範囲 (UTF-8 のバイト位置) の並び (拡張書記素クラスタ)。空の本文では空。
pub(crate) fn graphemes(text: &str) -> Vec<Range<usize>> {
    text.grapheme_indices(true)
        .map(|(i, g)| i..i + g.len())
        .collect()
}

/// 書記素の境界の位置 (0 と本文の長さを含む、昇順)。
pub(crate) fn grapheme_boundaries(text: &str) -> Vec<usize> {
    let mut b: Vec<usize> = graphemes(text).into_iter().map(|r| r.start).collect();
    b.push(text.len());
    b
}
#[cfg(test)]
mod tests {
    use super::*;

    fn clusters(text: &str) -> Vec<&str> {
        graphemes(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn format_characters_are_cf() {
        for c in [
            '\u{200B}',
            '\u{200D}',
            '\u{202E}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{E0001}',
        ] {
            assert_eq!(in_category(c, "Cf"), Some(true), "{c:?}");
        }
        for c in ['a', '\n', '\u{2028}', '\u{3000}', 'あ'] {
            assert_eq!(in_category(c, "Cf"), Some(false), "{c:?}");
        }
        assert_eq!(in_category('\n', "Cc"), Some(true));
        assert_eq!(in_category('\u{0085}', "Cc"), Some(true));
        assert_eq!(in_category('a', "Ll"), Some(true));
        assert_eq!(in_category('a', "Xx"), None);
    }

    #[test]
    fn grapheme_clusters_keep_marks_emoji_and_hangul_together() {
        assert_eq!(clusters("ｶﾞス"), ["ｶﾞ", "ス"]);
        assert_eq!(clusters("e\u{0301}x"), ["e\u{0301}", "x"]);
        assert_eq!(clusters("a\r\nb"), ["a", "\r\n", "b"]);
        assert_eq!(clusters("👨‍👩‍👧!"), ["👨‍👩‍👧", "!"]);
        assert_eq!(clusters("👍🏽"), ["👍🏽"]);
        assert_eq!(clusters("🇯🇵🇺🇸"), ["🇯🇵", "🇺🇸"]);
        assert_eq!(
            clusters("\u{1100}\u{1161}\u{11A8}가"),
            ["\u{1100}\u{1161}\u{11A8}", "가"]
        );
        assert_eq!(
            clusters("ig\u{200B}nore"),
            ["i", "g", "\u{200B}", "n", "o", "r", "e"]
        );
        assert_eq!(clusters(""), Vec::<&str>::new());
        assert_eq!(grapheme_boundaries("ab"), [0, 1, 2]);
    }

    #[test]
    fn separators_are_spaces_punctuation_and_symbols() {
        for c in [' ', '、', '。', '！', '!', '・', '+', '😀', '\u{3000}'] {
            assert!(is_separator(c), "{c:?}");
        }
        for c in ['a', '1', 'あ', 'ー', '々', '漢', '\u{0301}', '\u{200D}'] {
            assert!(!is_separator(c), "{c:?}");
        }
    }

    #[test]
    fn mixed_scripts_are_found_inside_one_word() {
        assert!(has_mixed_script_word("ign\u{043E}re"));
        assert!(has_mixed_script_word("\u{03B1}lpha"));
        assert!(!has_mixed_script_word("ignore \u{043E}\u{043A}"));
        assert!(!has_mixed_script_word("Москва"));
        assert!(!has_mixed_script_word("東京tokyo"));
    }
}
