//! 関門 6a: 機微な情報 (04 §5.2、OQ-56)。
//!
//! - 語の一覧 (`rules/b3.toml` の `[<言語>.sensitive.words]`): 日本語の語は部分一致、英語の語は ASCII の英数字で
//!   決めた語の境界で当てる (末尾の `*` は語の頭だけの境界)。除外の語 (`exclude`) の一致 1 つにすっかり収まる一致は
//!   数えない。
//! - 番地・号室・郵便番号の形の型 (`address_shapes`)。
//! - 電話番号・カード番号 (Luhn の検査)・メールアドレスは、言語に依らないコードの型で見る。
//!
//! 安全の規則なので、同梱のすべての言語の規則を合わせて、NFKC と case folding の後の形と照合の形 (04 §5.6) に当てる。

use super::rules::{Rules, ShapePart, shape_parts};
use crate::lexical::{Exclusions, word_bounded};
use crate::rules::fold;
use crate::value_key::normalize_loose;

/// 文字列が機微な情報を含むか。
pub(super) fn is_sensitive(rules: &Rules<'_>, text: &str) -> bool {
    let forms = [fold(text), normalize_loose(text)];
    if forms.iter().any(|f| has_structured(f)) {
        return true;
    }
    rules.extras().iter().any(|(r, x)| {
        let bounded = word_bounded(&r.language);
        let s = &x.sensitive;
        forms.iter().any(|f| {
            let excluded = Exclusions::new(f, &s.exclude, bounded);
            s.all_words().any(|w| {
                word_hits(f, w, bounded)
                    .into_iter()
                    .any(|(a, b)| !excluded.covers(a, b))
            }) || s
                .address_shapes
                .iter()
                .any(|shape| shape_parts(shape).is_some_and(|parts| has_shape(f, &parts)))
        })
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// 語 `w` の `hay` の中の一致の範囲。`bounded` なら、語の端が英数字の側の外が英数字でない位置に限る。末尾の `*` は
/// 語幹 (語の頭だけに境界を求める)。
fn word_hits(hay: &str, w: &str, bounded: bool) -> Vec<(usize, usize)> {
    let (stem, prefix) = w.strip_suffix('*').map_or((w, false), |s| (s, true));
    let (Some(first), Some(last)) = (stem.chars().next(), stem.chars().next_back()) else {
        return Vec::new();
    };
    hay.match_indices(stem)
        .map(|(i, _)| (i, i + stem.len()))
        .filter(|&(i, j)| {
            if !bounded {
                return true;
            }
            let before = hay[..i].chars().next_back();
            let after = hay[j..].chars().next();
            !(is_word_char(first) && before.is_some_and(is_word_char))
                && (prefix || !(is_word_char(last) && after.is_some_and(is_word_char)))
        })
        .collect()
}

/// 番地の形の数字 (算用数字 (一般カテゴリ Nd) と漢数字)。
fn is_numeral(c: char) -> bool {
    is_digit(c)
        || matches!(
            c,
            '〇' | '一'
                | '二'
                | '三'
                | '四'
                | '五'
                | '六'
                | '七'
                | '八'
                | '九'
                | '十'
                | '百'
                | '千'
        )
}

fn is_digit(c: char) -> bool {
    unicode_properties::UnicodeGeneralCategory::general_category(c)
        == unicode_properties::GeneralCategory::DecimalNumber
}

/// 漢字かカタカナ (`{c}`)。
fn is_cjk(c: char) -> bool {
    use unicode_script::{Script, UnicodeScript};
    matches!(c.script(), Script::Han | Script::Katakana)
}

/// `text` のどこかに、型 `parts` に合う並びがあるか。型の頭が数字か英数字なら、その前に数字・英数字が続かない
/// 位置から始め、型の終わりが数字か英数字なら、その後ろに続かない位置で終わる。
fn has_shape(text: &str, parts: &[ShapePart]) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| {
        let before = i.checked_sub(1).map(|j| chars[j]);
        let blocked = match parts.first() {
            Some(ShapePart::Number | ShapePart::Digits(_)) => before.is_some_and(is_numeral),
            Some(ShapePart::Text(t)) => {
                t.starts_with(is_word_char) && before.is_some_and(is_word_char)
            }
            Some(ShapePart::Word | ShapePart::Words) => before.is_some_and(is_word_char),
            _ => false,
        };
        !blocked && shape_at(&chars[i..], parts)
    })
}

/// 型の終わりの後ろの文字 `after` が、型の最後の部分 `last` に続いてはならないものか。
fn ends_cleanly(last: Option<&ShapePart>, after: Option<char>) -> bool {
    match last {
        Some(ShapePart::Number | ShapePart::Digits(_)) => !after.is_some_and(is_numeral),
        Some(ShapePart::Text(t)) if t.ends_with(is_word_char) => !after.is_some_and(is_word_char),
        Some(ShapePart::Word | ShapePart::Words) => !after.is_some_and(is_word_char),
        _ => true,
    }
}

/// `chars` の頭が、型 `parts` に合うか (`{n}`・`{w}`・`{ws}` は長い方から試す)。
fn shape_at(chars: &[char], parts: &[ShapePart]) -> bool {
    shape_from(chars, parts, None)
}

fn shape_from(chars: &[char], parts: &[ShapePart], prev: Option<&ShapePart>) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return ends_cleanly(prev, chars.first().copied());
    };
    let next = |n: usize| shape_from(&chars[n..], rest, Some(first));
    match first {
        ShapePart::Text(t) => {
            let t: Vec<char> = t.chars().collect();
            chars.starts_with(&t) && next(t.len())
        }
        ShapePart::Number => {
            let run = chars.iter().take_while(|c| is_numeral(**c)).count();
            (1..=run).rev().any(next)
        }
        ShapePart::Digits(k) => {
            let run = chars.iter().take_while(|c| is_digit(**c)).count();
            run == *k && next(*k)
        }
        ShapePart::Cjk => chars.first().is_some_and(|c| is_cjk(*c)) && next(1),
        ShapePart::Word => {
            let run = chars.iter().take_while(|c| c.is_ascii_alphabetic()).count();
            run > 0 && next(run)
        }
        ShapePart::Words => {
            // 空白 1 つで区切った英字の語の 1〜3 個。
            let mut ends = Vec::new();
            let mut at = 0;
            for _ in 0..3 {
                let run = chars[at..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .count();
                if run == 0 {
                    break;
                }
                at += run;
                ends.push(at);
                if chars.get(at) != Some(&' ') {
                    break;
                }
                at += 1;
            }
            ends.into_iter().rev().any(next)
        }
    }
}

/// 電話番号・カード番号・メールアドレスの形を含むか (言語に依らない型)。`text` は NFKC と case folding の後。
fn has_structured(text: &str) -> bool {
    has_email(text) || digit_groups(text).iter().any(|g| is_phone(g) || is_card(g))
}

/// メールアドレス: `@` の前に英数字か `._%+-`、後ろに英数字の並びと `.` と 2 文字以上の英字の終わりがある。
fn has_email(text: &str) -> bool {
    text.match_indices('@').any(|(i, _)| {
        let local_ok = text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
        let domain: String = text[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            .collect();
        let domain = domain.trim_end_matches('.');
        let tld_ok = domain.rsplit_once('.').is_some_and(|(host, tld)| {
            !host.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
        });
        local_ok && tld_ok
    })
}

/// 数字の塊: 数字と、その間の区切り (`-`・`.`・空白 1 つ・`(`・`)`) の続き。頭の `+` は持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DigitGroup {
    plus: bool,
    /// 区切りで分けた数字の並び。
    parts: Vec<String>,
}

impl DigitGroup {
    fn digits(&self) -> String {
        self.parts.concat()
    }
}

fn digit_groups(text: &str) -> Vec<DigitGroup> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let plus = chars[i] == '+' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
        let opens = chars[i] == '(' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
        if !(chars[i].is_ascii_digit() || plus || opens) {
            i += 1;
            continue;
        }
        if plus || opens {
            i += 1;
        }
        let is_sep = |c: char| matches!(c, '-' | '.' | ' ' | '(' | ')');
        let mut parts = vec![String::new()];
        while i < chars.len() {
            let c = chars[i];
            if c.is_ascii_digit() {
                parts.last_mut().expect("one part").push(c);
                i += 1;
                continue;
            }
            // 区切りは 2 文字まで (`) `・`- ` など) 続いてよく、その後ろに数字が来るときだけ塊を続ける。
            let seps = chars[i..]
                .iter()
                .take(2)
                .take_while(|c| is_sep(**c))
                .count();
            if seps > 0 && chars.get(i + seps).is_some_and(char::is_ascii_digit) {
                parts.push(String::new());
                i += seps;
            } else {
                break;
            }
        }
        parts.retain(|p| !p.is_empty());
        out.push(DigitGroup { plus, parts });
    }
    out
}

/// 電話番号: `+` で始まり 8〜15 桁、0 で始まり区切りを持つ 10〜11 桁か区切り無しの 10〜11 桁、区切りで 3・3・4 に
/// 分かれた 10 桁 (北米の形)。
fn is_phone(g: &DigitGroup) -> bool {
    let d = g.digits();
    let n = d.len();
    let lens: Vec<usize> = g.parts.iter().map(String::len).collect();
    (g.plus && (8..=15).contains(&n))
        || (d.starts_with('0') && (10..=11).contains(&n))
        || lens == [3, 3, 4]
}

/// カード番号: 13〜19 桁で Luhn の検査に合う。
fn is_card(g: &DigitGroup) -> bool {
    let d = g.digits();
    (13..=19).contains(&d.len()) && luhn(&d)
}

fn luhn(digits: &str) -> bool {
    let sum: u32 = digits
        .chars()
        .rev()
        .filter_map(|c| c.to_digit(10))
        .enumerate()
        .map(|(i, x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 { y - 9 } else { y }
            } else {
                x
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_categories_shapes_and_structured_values() {
        let rules = Rules::bundled();
        for s in [
            "持病",
            "高血圧",
            "胃がん",
            "白血病",
            "心臓病",
            "教会のミサ",
            "支持政党",
            "みどり党",
            "同性愛者",
            "レズ",
            "ノンバイナリー",
            "住所",
            "北町3丁目5番8号",
            "北町三丁目",
            "５番８号",
            "203号室",
            "北町3-5-8",
            "本町1-2",
            "〒123-4567",
            "123-4567",
            "前科がある",
            "障がい",
            "人種",
            "diabetes",
            "Diagnosed with ADHD",
            "Bisexual",
            "pregnancy",
            "depressed",
            "physical therapy",
            "Islamic art",
            "HIV",
            "742 Evergreen Street",
            "100 North Main Street",
            "12 Oak Terrace",
            "Apt 4",
            "convicted of fraud",
            "victim of a scam",
            "wheelchair user",
            "ken@example.com",
            "090-1234-5678",
            "+81 90 1234 5678",
            "(555) 123-4567",
            "4111 1111 1111 1111",
        ] {
            assert!(is_sensitive(&rules, s), "{s:?}");
        }
        for s in [
            "ラーメン",
            "看護師",
            "ミサキ",
            "応用情報の勉強",
            "大阪",
            "病院の受付",
            "臆病な猫",
            "甘党",
            "左翼手",
            "ゲイン",
            "Churchill",
            "Islamabad",
            "archive",
            "beehive",
            "Gaylord",
            "Hindustani",
            "political science",
            "my sign is Cancer",
            "3丁",
            "2026-11-28",
            "1990.05.12",
            "Main Street",
            "第十二号",
            "がんばること",
            "5 km all the way",
            "@ken",
            "1234 5678 9012 3456",
            "12345",
        ] {
            assert!(!is_sensitive(&rules, s), "{s:?}");
        }
    }

    #[test]
    fn luhn_checks_card_numbers() {
        assert!(luhn("4111111111111111"));
        assert!(!luhn("4111111111111112"));
    }
}
