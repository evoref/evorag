//! 読み取りの規則 (04 §5.3) を根拠の文字列に当てる: 主語を明示する語・変化を示す語・否定で今の値を
//! 終える語。規則は根拠の文字種から決めた言語のものを選ぶ (04 §5.2 の「読み取りの規則」)。
//!
//! 照合は NFKC と完全な case folding の後に行う (規則の語もこの形で書く、rules.rs の `fold`)。日本語は部分一致、
//! 英語は ASCII の英数字で決めた語の境界で当てる。除外の語の一致にすっかり収まる一致は数えない (`Exclusions`)。

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
        let excluded = Exclusions::new(&text, &r.subject_self_exclude, word_bounded);
        any_outside(&text, &r.subject_self, &excluded, word_bounded)
    } else {
        // ほかの主語は、呼び名そのものが根拠にあれば明示とみなす (英語は語の境界で、04 §5.2)。
        let name = normalize_v1(subject);
        !name.is_empty() && !find_all(&normalize_v1(evidence), &name, word_bounded).is_empty()
    };
    let unchanged = Exclusions::new(&text, &r.change_exclude, word_bounded);
    Signals {
        explicit_subject,
        change: any_outside(&text, &r.change, &unchanged, word_bounded),
        negation_change: any_outside(
            &text,
            &r.negation_change,
            &Exclusions::default(),
            word_bounded,
        ),
    }
}

/// 規則の語と同じ形にそろえる (NFKC → 完全な case folding、04 §5.2)。
fn fold(s: &str) -> String {
    crate::rules::fold(s)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// `needle` が `hay` に現れる位置 (バイト)。`bounded` なら、前後が英数字でない位置だけ。
pub(crate) fn find_all(hay: &str, needle: &str, bounded: bool) -> Vec<usize> {
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

/// 除外の語が当たった範囲 (04 §5.2)。規則の語の一致は、除外の語の一致 1 つの中にすっかり収まるときだけ
/// 数えない (はみ出す一致は数える。「無視してくれる」の「無視して」は「してくれる」に収まらない)。
///
/// 範囲を始まりの順に並べ、終わりの累積の最大を持つので、作るのは O(n log n)、1 回の問い合わせは O(log n)。
#[derive(Debug, Clone, Default)]
pub(crate) struct Exclusions {
    /// 範囲の始まり (昇順)。
    starts: Vec<usize>,
    /// `starts[..=i]` の範囲の終わりの最大。
    max_end: Vec<usize>,
}

impl Exclusions {
    /// `words` が `hay` に当たる範囲を集める。`bounded` は `find_all` と同じ。
    pub(crate) fn new<'w>(
        hay: &str,
        words: impl IntoIterator<Item = &'w String>,
        bounded: bool,
    ) -> Self {
        let mut ranges: Vec<(usize, usize)> = words
            .into_iter()
            .flat_map(|w| {
                find_all(hay, w, bounded)
                    .into_iter()
                    .map(move |i| (i, i + w.len()))
            })
            .collect();
        ranges.sort_unstable();
        let starts = ranges.iter().map(|r| r.0).collect();
        let max_end = ranges
            .iter()
            .scan(0, |m, r| {
                *m = (*m).max(r.1);
                Some(*m)
            })
            .collect();
        Self { starts, max_end }
    }

    /// `[start, end)` が、除外の語の一致のどれか 1 つの中に収まるか。
    pub(crate) fn covers(&self, start: usize, end: usize) -> bool {
        // 始まりが `start` 以下の範囲のうち、終わりの最大が `end` 以上なら、その範囲が収める。
        let n = self.starts.partition_point(|&s| s <= start);
        n > 0 && self.max_end[n - 1] >= end
    }
}

/// `words` のどれかが `hay` に、除外の語の一致に収まらずに当たるか。
pub(crate) fn any_outside(
    hay: &str,
    words: &[String],
    exclusions: &Exclusions,
    bounded: bool,
) -> bool {
    words.iter().any(|w| {
        find_all(hay, w, bounded)
            .into_iter()
            .any(|i| !exclusions.covers(i, i + w.len()))
    })
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
        assert!(signals("Ken lives in Kyoto", "ken").explicit_subject);
        // 英語の呼び名は語の境界で当てる ("ken" は "Kenji" に当たらない)。
        assert!(!signals("Kenji lives in Kyoto", "ken").explicit_subject);
    }

    #[test]
    fn exclusions_hide_only_matches_they_contain() {
        let ex = Exclusions::new("無視してくれる", &["してくれる".to_owned()], false);
        // 「してくれ」は「してくれる」に収まり、「無視して」ははみ出す。
        assert!(ex.covers(6, 18));
        assert!(!ex.covers(0, 12));
        assert!(any_outside(
            "無視してくれる",
            &["無視して".to_owned()],
            &ex,
            false
        ));
        assert!(!any_outside(
            "無視してくれる",
            &["してくれ".to_owned()],
            &ex,
            false
        ));
        // 重なる範囲でも、どれか 1 つに収まればよい。
        let ex = Exclusions::new("abcdef", &["abcd".to_owned(), "b".to_owned()], false);
        assert!(ex.covers(1, 4));
        assert!(!ex.covers(3, 5));
        assert!(!Exclusions::default().covers(0, 0));
    }

    #[test]
    fn subject_words_next_to_an_exclusion_still_count() {
        // 「私の妻」に収まる「私」は数えず、ほかの「私」は数える。
        assert!(signals("私の妻と私は京都に住んでいます", "self").explicit_subject);
        assert!(!signals("私の妻は京都に住んでいます", "self").explicit_subject);
    }

    #[test]
    fn change_words_have_exclusions() {
        assert!(signals("住所が変わりました", "self").change);
        assert!(!signals("相変わらず東京に住んでる", "self").change);
        assert!(!signals("着替えて東京に行った", "self").change);
    }

    #[test]
    fn language_is_chosen_from_the_script() {
        assert_eq!(evidence_language("ラーメンが好き"), "ja");
        assert_eq!(evidence_language("I like ramen"), "en");
        assert_eq!(evidence_language("ｶﾞｽ"), "ja");
    }
}
