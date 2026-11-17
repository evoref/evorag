//! 読み取りの規則 (04 §5.3) を根拠の文字列に当てる: 主語を明示する語・変化を示す語・否定で今の値を
//! 終える語。規則は根拠の文字種から決めた言語のものを選ぶ (04 §5.2 の「読み取りの規則」)。
//!
//! 照合は NFKC と小文字化の後に行う (規則の語もこの形で書く、rules.rs の `fold`)。日本語は部分一致、
//! 英語は ASCII の英数字で決めた語の境界で当てる。除外の語に当たった箇所は、照合の前に伏せる。

use unicode_normalization::UnicodeNormalization;

use crate::rules::{ReadingRules, language_rules};
use crate::value_key::normalize_v1;

/// 根拠から読み取った印 (04 §5.3 の置き換えの条件と否定)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals {
    /// 主語を明示する語がある (`self` なら「私」など、ほかの主語なら呼び名)。
    pub explicit_subject: bool,
    /// 変化を示す語がある (「引っ越した」など)。
    pub change: bool,
    /// 否定の候補で、今の値を終えてよい語がある (「もう」など)。
    pub negation_change: bool,
}

/// 根拠の文字種から言語を決める (かな・漢字があれば `ja`、無ければ `en`)。
pub fn evidence_language(evidence: &str) -> &'static str {
    let ja = evidence.chars().any(|c| {
        matches!(c as u32,
            0x3040..=0x30FF   // ひらがな・カタカナ
            | 0x31F0..=0x31FF // カタカナの拡張
            | 0x3400..=0x4DBF // CJK 拡張 A
            | 0x4E00..=0x9FFF // CJK 統合漢字
            | 0xF900..=0xFAFF // CJK 互換漢字
            | 0xFF66..=0xFF9F // 半角カナ
        )
    });
    if ja { "ja" } else { "en" }
}

/// 根拠に読み取りの規則を当てる。`subject` は候補の主語 (`self` か、正規化した呼び名)。
pub fn signals(evidence: &str, subject: &str) -> Signals {
    let lang = evidence_language(evidence);
    let Some(rules) = language_rules(lang) else {
        return Signals::default();
    };
    let text = fold(evidence);
    let r: &ReadingRules = &rules.reading;
    let word_bounded = lang == "en";
    let explicit_subject = if subject == "self" {
        let masked = mask(&text, &r.subject_self_exclude, word_bounded);
        any_match(&masked, &r.subject_self, word_bounded)
    } else {
        // ほかの主語は、呼び名そのものが根拠にあれば明示とみなす。
        let name = normalize_v1(subject);
        !name.is_empty() && normalize_v1(evidence).contains(&name)
    };
    Signals {
        explicit_subject,
        change: any_match(&text, &r.change, word_bounded),
        negation_change: any_match(&text, &r.negation_change, word_bounded),
    }
}

/// 規則の語と同じ形にそろえる (NFKC → 小文字)。
fn fold(s: &str) -> String {
    s.nfkc().flat_map(char::to_lowercase).collect()
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// `needle` が `hay` に現れる位置 (バイト)。`bounded` なら、前後が英数字でない位置だけ。
fn find_all(hay: &str, needle: &str, bounded: bool) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    hay.match_indices(needle)
        .map(|(i, _)| i)
        .filter(|&i| {
            if !bounded {
                return true;
            }
            let before = hay[..i].chars().next_back();
            let after = hay[i + needle.len()..].chars().next();
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
        .collect()
}

fn any_match(hay: &str, words: &[String], bounded: bool) -> bool {
    words.iter().any(|w| !find_all(hay, w, bounded).is_empty())
}

/// 除外の語に当たった箇所を伏せる (同じ長さの、どの語にも当たらない文字で埋める)。
fn mask(hay: &str, excludes: &[String], bounded: bool) -> String {
    // 範囲は元の文字列で集め、その範囲の文字をまとめて置き換える (位置がずれないように)。
    let ranges: Vec<(usize, usize)> = excludes
        .iter()
        .flat_map(|w| {
            find_all(hay, w, bounded)
                .into_iter()
                .map(move |i| (i, i + w.len()))
        })
        .collect();
    hay.char_indices()
        .map(|(i, c)| {
            if ranges.iter().any(|&(s, e)| s <= i && i < e) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_subject_and_change_words() {
        assert!(signals("私は東京に住んでいます", "self").explicit_subject);
        assert!(!signals("東京に住んでるよ", "self").explicit_subject);
        assert!(signals("先月、東京に引っ越しました", "self").change);
        assert!(!signals("東京に住んでるよ", "self").change);
        // 「私の妻」は主語の語として数えない。
        assert!(!signals("私の妻は京都に住んでいます", "self").explicit_subject);
        // 「私立」も主語の語ではない。
        assert!(!signals("私立の高校に通っていた", "self").explicit_subject);
        assert!(signals("もう大阪には住んでいない", "self").negation_change);
        assert!(!signals("大阪には住んでいない", "self").negation_change);
    }

    #[test]
    fn english_words_need_word_boundaries() {
        assert!(signals("I live in Tokyo.", "self").explicit_subject);
        assert!(signals("Moved to Osaka last month.", "self").change);
        // "my wife" は主語の語として数えない。
        assert!(!signals("My wife lives in Kyoto.", "self").explicit_subject);
        // "now" は "know" の中には当たらない。
        assert!(!signals("you know Tokyo", "self").change);
        assert!(signals("I no longer live in Tokyo", "self").negation_change);
    }

    #[test]
    fn another_subject_is_explicit_when_named() {
        assert!(signals("妻は京都に住んでいます", "妻").explicit_subject);
        assert!(!signals("京都に住んでいます", "妻").explicit_subject);
    }

    #[test]
    fn language_is_chosen_from_the_script() {
        assert_eq!(evidence_language("ラーメンが好き"), "ja");
        assert_eq!(evidence_language("I like ramen"), "en");
        assert_eq!(evidence_language("ｶﾞｽ"), "ja");
    }
}
