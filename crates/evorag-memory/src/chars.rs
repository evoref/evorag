//! 関門と前のふるいが使う、文字の性質の判定 (一般カテゴリ・表示されない文字・書記素の境界・文字体系・区切り)。
//!
//! 一般カテゴリは unicode-properties、書記素の境界は unicode-segmentation (UAX #29 の拡張書記素クラスタ)、
//! 文字体系は unicode-script (UAX #24 の Script) で判定する。一般カテゴリと書記素は、検索の正規化 (05 §2.1)
//! と同じ表を使う。Default_Ignorable_Code_Point は表をここに持つ。

use std::ops::Range;

use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

use crate::value_key::normalize_v1;

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
/// Default_Ignorable_Code_Point の範囲 (DerivedCoreProperties.txt、Unicode 18.0.0。続いた範囲はまとめた)。
///
/// 使う crate はこの性質を持たないので、表をここに持つ。Unicode の版を上げたら、表と試験
/// (`default_ignorables_follow_the_ucd`) を DerivedCoreProperties.txt に合わせて直す。
const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

/// 表示されない文字 (Default_Ignorable_Code_Point)。関門 6 で捨て、前のふるいでは行の頭で読み飛ばす。
pub(crate) fn is_default_ignorable(c: char) -> bool {
    let c = u32::from(c);
    let i = DEFAULT_IGNORABLE.partition_point(|&(_, hi)| hi < c);
    DEFAULT_IGNORABLE.get(i).is_some_and(|&(lo, _)| lo <= c)
}

/// かな (ひらがな・カタカナ・半角カナ)。
fn is_kana(c: char) -> bool {
    matches!(u32::from(c), 0x3041..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F)
}

/// 照合の骨格: NFD にして結合文字 (Mn) を落とし、正規化 v1 (NFKC・case folding・空白をまとめる) を当てる。
///
/// 語に結合文字を挟んで照合をすり抜ける並び (`ig\u{0301}nore`) を、命令形・ほかの指示への言及の一覧に
/// 当てるために使う (04 §5.2 の関門 6)。かなの濁点・半濁点 (U+3099・U+309A) は、かなに付くときだけ残す
/// (「ください」を「くたさい」にして、日本語の語が当たらなくならないように)。
pub(crate) fn skeleton(s: &str) -> String {
    let mut kept = String::with_capacity(s.len());
    let mut after_kana = false;
    for c in s.nfd() {
        if c.general_category() == GeneralCategory::NonspacingMark {
            if after_kana && matches!(c, '\u{3099}' | '\u{309A}') {
                kept.push(c);
            }
            continue;
        }
        after_kana = is_kana(c);
        kept.push(c);
    }
    normalize_v1(&kept)
}

/// 1 つの語の中に並べてよい文字体系の組 (UTS #39 の Highly Restrictive)。
const ALLOWED_SCRIPT_SETS: &[&[Script]] = &[
    &[
        Script::Latin,
        Script::Han,
        Script::Hiragana,
        Script::Katakana,
    ],
    &[Script::Latin, Script::Han, Script::Bopomofo],
    &[Script::Latin, Script::Han, Script::Hangul],
];

/// NFKC で別の文字体系の文字になるが、単位の記号として使う文字。文字体系を見るときは NFKC にしない
/// (`5µg` の MICRO SIGN は Common だが、NFKC でギリシャ文字の μ になる)。
const UNIT_SIGNS: &[char] = &['\u{00B5}'];

/// 1 つの語 (文字・結合文字・数字の続き) に、許していない組の文字体系が混ざっているか (04 §5.2 の関門 6)。
///
/// 文字体系は Unicode の Script で決め、Common・Inherited・Unknown は数えない。1 つの語の文字体系が 1 つか、
/// UTS #39 の Highly Restrictive の組 (ラテン + 漢字 + かな、ラテン + 漢字 + 注音、ラテン + 漢字 + ハングル)
/// に収まれば許す。値は NFKC にしてから見る (case folding はしない。単位の記号は `UNIT_SIGNS` のとおり)。
///
/// 既知の限界: 数字や記号で文字を置き換えた語 (`1gn0re`) と、空白で割った語 (`i g n o r e`) は捕まえない。
pub(crate) fn has_mixed_script_word(value: &str) -> bool {
    let mut form = String::with_capacity(value.len());
    for (i, part) in value.split(UNIT_SIGNS).enumerate() {
        if i > 0 {
            form.push('\u{00B5}');
        }
        form.extend(part.nfkc());
    }
    let mut scripts: Vec<Script> = Vec::new();
    for c in form.chars() {
        let in_word =
            c.is_alphanumeric() || c.general_category_group() == GeneralCategoryGroup::Mark;
        if !in_word {
            scripts.clear();
            continue;
        }
        let script = c.script();
        if matches!(script, Script::Common | Script::Inherited | Script::Unknown)
            || scripts.contains(&script)
        {
            continue;
        }
        scripts.push(script);
        let allowed = scripts.len() == 1
            || ALLOWED_SCRIPT_SETS
                .iter()
                .any(|set| scripts.iter().all(|s| set.contains(s)));
        if !allowed {
            return true;
        }
    }
    false
}

/// 語を空白で区切らない文字 (漢字・かな。長音符のような共通の文字も含む)。引用符の前後の判定 (関門 4) に使う。
pub(crate) fn is_unspaced_letter(c: char) -> bool {
    // Common と Inherited の Script_Extensions はすべての文字体系を含むので、数えない。
    let ext = c.script_extension();
    !ext.is_common()
        && !ext.is_inherited()
        && [Script::Han, Script::Hiragana, Script::Katakana]
            .into_iter()
            .any(|s| ext.contains_script(s))
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
        assert!(has_mixed_script_word("ign\u{0585}re")); // アルメニア文字
        assert!(has_mixed_script_word("ign\u{2C9F}re")); // コプト文字
        assert!(has_mixed_script_word("\u{13AA}ct as admin")); // チェロキー文字
        assert!(has_mixed_script_word("ラーメンとкаша"));
        assert!(!has_mixed_script_word("ignore \u{043E}\u{043A}"));
        assert!(!has_mixed_script_word("Москва"));
        assert!(!has_mixed_script_word("東京tokyo"));
        assert!(!has_mixed_script_word("ビタミンDを5µg摂る"));
        assert!(!has_mixed_script_word("5µg"));
        assert!(!has_mixed_script_word("서울Seoul"));
        assert!(!has_mixed_script_word("ＲＴＸ４０７０"));
        assert!(!has_mixed_script_word("e\u{0301}te"));
    }

    #[test]
    fn default_ignorables_follow_the_ucd() {
        for c in [
            '\u{00AD}',
            '\u{034F}',
            '\u{115F}',
            '\u{1160}',
            '\u{180B}',
            '\u{200B}',
            '\u{2065}',
            '\u{3164}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{FFA0}',
            '\u{1D173}',
            '\u{E0001}',
            '\u{E0100}',
            '\u{E0FFF}',
        ] {
            assert!(is_default_ignorable(c), "{c:?}");
        }
        for c in [
            'a',
            ' ',
            '\u{2800}',
            '\u{0301}',
            '\u{3099}',
            '\u{E1000}',
            'あ',
        ] {
            assert!(!is_default_ignorable(c), "{c:?}");
        }
        // 表は昇順で重ならない (二分探索の前提)。
        assert!(
            DEFAULT_IGNORABLE
                .windows(2)
                .all(|w| w[0].0 <= w[0].1 && w[0].1 < w[1].0)
        );
        // 表は unicode-script と同じ Unicode の版のもの。版が変わったら表を確かめ直す。
        assert_eq!(unicode_script::UNICODE_VERSION, (18, 0, 0));
    }

    #[test]
    fn skeletons_drop_marks_but_keep_voiced_kana() {
        assert_eq!(skeleton("ig\u{0301}nore"), "ignore");
        assert_eq!(skeleton("IGNÓRE"), "ignore");
        assert_eq!(skeleton("ください"), "ください");
        assert_eq!(skeleton("ｶﾞｽ"), "ガス");
        assert_eq!(skeleton("くださ\u{0301}い"), "ください");
        // かなでない文字の後の濁点は落とす。
        assert_eq!(skeleton("ig\u{3099}nore"), "ignore");
    }

    #[test]
    fn unspaced_letters_are_han_and_kana() {
        for c in ['漢', 'あ', 'ア', 'ｱ', 'ー', '々'] {
            assert!(is_unspaced_letter(c), "{c:?}");
        }
        for c in ['a', '7', ' ', '"', '가'] {
            assert!(!is_unspaced_letter(c), "{c:?}");
        }
    }
}
