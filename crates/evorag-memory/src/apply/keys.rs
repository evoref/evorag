//! 語句・部分文字列の値の鍵 (04 §7.2 の台帳の掃除の 2・4、§5.5 の private の鍵のふるい)。

use std::collections::BTreeSet;

use sha2::Digest;

use crate::chars::graphemes;
use crate::value_key::{
    ForgetKey, KeyStream, ValueKey, dropped_spaces, fold_cluster, normalize_loose, normalize_v1,
    value_key,
};

/// 部分文字列の鍵を作る、正規化の後の長さ (文字数) の上限 (04 §7.2 の「掃除の補い」、§5.5)。
///
/// 忘れる値の長さの上限の最大 (辞書の属性の `max_chars` の最大 200 文字。自由記述の属性は 60 文字) に、NFKC で
/// 伸びる分の余裕 (56 文字) を足した値。これより長い部分文字列の鍵は作らない (それより長く伸びる値の一致は約束しない
/// 範囲、04 §7.8)。上限を上げるときは、掃除の費用 (書記素の数 × この値) を測り直す。
pub const SUBSTRING_MAX_CHARS: usize = 256;

/// 文字列の、書記素の境界で切った部分文字列 (全体を含む) の値の鍵と照合の鍵の集合 (04 §5.6・§7.2)。
///
/// 指示の `vks` は HMAC なので、当たる部分文字列の長さは分からない。そのため、正規化の後の長さが
/// [`SUBSTRING_MAX_CHARS`] 以下のすべての部分文字列を引く。区切りの鍵のふるい (§5.5) のように同じ鍵の文字列を
/// 何度も引くときは、作り置きして使い回す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubstringKeys(BTreeSet<ValueKey>);

impl SubstringKeys {
    /// 文字列 `s` の部分文字列の鍵。
    #[must_use]
    pub fn of(key: &ForgetKey, s: &str) -> Self {
        Self::of_all(key, [s])
    }

    /// `strings` のどれかの部分文字列の鍵の和。
    #[must_use]
    pub fn of_all<'s>(key: &ForgetKey, strings: impl IntoIterator<Item = &'s str>) -> Self {
        let ks = KeyStream::new(key);
        let mut out = BTreeSet::new();
        for s in strings {
            add_substrings(&ks, key, s, &mut out);
        }
        Self(out)
    }

    /// 鍵 `k` があるか。
    #[must_use]
    pub fn contains(&self, k: &ValueKey) -> bool {
        self.0.contains(k)
    }

    /// `keys` のどれかがあるか (指示の `vks` に当たるか)。
    #[must_use]
    pub fn hits(&self, keys: &BTreeSet<ValueKey>) -> bool {
        keys.iter().any(|k| self.0.contains(k))
    }

    /// 鍵の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 鍵が 1 つも無い (空白だけの文字列など)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 本文 `text` の中で、値の鍵か照合の鍵が `keys` のどれかに当たる部分文字列 (書記素の境界で切ったもの。正規化の
/// 後の長さが [`SUBSTRING_MAX_CHARS`] 以下) の範囲 (本文のバイト位置。重なりうる)。
///
/// 値の文字列を持たない指示 (`vks` だけを持つ v0.5 の `private`) の値を、本文の中で探すのに使う (想起での
/// `[非表示]`、04 §7.5・§7.9)。[`SubstringKeys`] と同じ部分文字列を引くので、費用も同じ (書記素の数 × 上限)。
#[must_use]
pub fn locate_keys(
    key: &ForgetKey,
    keys: &BTreeSet<ValueKey>,
    text: &str,
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    if keys.is_empty() || text.is_empty() {
        return out;
    }
    let clusters = graphemes(text);
    let forms: Vec<String> = clusters
        .iter()
        .map(|r| fold_cluster(&text[r.clone()]))
        .collect();
    let dropped = dropped_spaces(text, &clusters);
    let any_dropped = dropped.iter().any(|d| *d);
    let fast = joined(&forms, None) == normalize_v1(text)
        && (!any_dropped || joined(&forms, Some(&dropped)) == normalize_loose(text));
    if !fast {
        for i in 0..clusters.len() {
            for j in i..clusters.len() {
                let sub = &text[clusters[i].start..clusters[j].end];
                let (v1, loose) = (normalize_v1(sub), normalize_loose(sub));
                let (n1, nl) = (v1.chars().count(), loose.chars().count());
                if n1 > SUBSTRING_MAX_CHARS && nl > SUBSTRING_MAX_CHARS {
                    break;
                }
                let hit = [(v1, n1), (loose, nl)].into_iter().any(|(form, n)| {
                    !form.is_empty()
                        && n <= SUBSTRING_MAX_CHARS
                        && keys.contains(&value_key(key, &form))
                });
                if hit {
                    out.push(clusters[i].start..clusters[j].end);
                }
            }
        }
        return out;
    }
    let ks = KeyStream::new(key);
    for start in 0..forms.len() {
        locate_from(&ks, keys, &forms, &clusters, None, start, &mut out);
        if any_dropped {
            locate_from(
                &ks,
                keys,
                &forms,
                &clusters,
                Some(&dropped),
                start,
                &mut out,
            );
        }
    }
    out
}

/// 書記素 `start` から始まる部分文字列のうち、鍵が `keys` に当たるものの範囲を足す ([`grow`] と同じ伸ばし方)。
fn locate_from(
    ks: &KeyStream,
    keys: &BTreeSet<ValueKey>,
    forms: &[String],
    clusters: &[std::ops::Range<usize>],
    dropped: Option<&[bool]>,
    start: usize,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    let mut h = ks.start();
    let mut chars = 0usize;
    let mut pending = false;
    let mut buf = [0u8; 4];
    for (j, f) in forms.iter().enumerate().skip(start) {
        if dropped.is_some_and(|d| d[j]) {
            continue;
        }
        let mut grew = false;
        for c in f.chars() {
            if c.is_whitespace() {
                pending = chars > 0;
            } else {
                if pending {
                    h.update(b" ");
                    chars += 1;
                    pending = false;
                }
                h.update(c.encode_utf8(&mut buf).as_bytes());
                chars += 1;
                grew = true;
            }
        }
        if chars > SUBSTRING_MAX_CHARS {
            return;
        }
        if grew && keys.contains(&ks.finish(&h)) {
            out.push(clusters[start].start..clusters[j].end);
        }
    }
}

/// `s` の部分文字列の鍵を `out` に足す。
///
/// 書記素ごとの正規化 ([`fold_cluster`]) を 1 回だけ作り、始まりの書記素ごとに、後ろへ 1 つずつ書記素をつないで
/// 正規化の後の文字列 (空白の連なりを 1 つにまとめ、両端を除いたもの) を伸ばしながら、HMAC の状態にも同じバイト列を
/// 足していく。正規化の後の長さが上限を超えたら、その始まりからは伸ばさない。費用は 書記素の数 × 上限 に比例する。
/// 書記素ごとの正規化をつないだものが全体の正規化と食い違う文字列 (書記素をまたぐ合成など) は、部分文字列ごとに
/// 正規化し直す形 ([`add_substrings_slow`]) で作る。
fn add_substrings(ks: &KeyStream, key: &ForgetKey, s: &str, out: &mut BTreeSet<ValueKey>) {
    let clusters = graphemes(s);
    let forms: Vec<String> = clusters
        .iter()
        .map(|r| fold_cluster(&s[r.clone()]))
        .collect();
    let dropped = dropped_spaces(s, &clusters);
    let any_dropped = dropped.iter().any(|d| *d);
    if joined(&forms, None) != normalize_v1(s)
        || (any_dropped && joined(&forms, Some(&dropped)) != normalize_loose(s))
    {
        add_substrings_slow(key, s, &clusters, out);
        return;
    }
    for start in 0..forms.len() {
        grow(ks, &forms, None, start, out);
        // 落とす空白が無ければ、照合の形は正規化 v1 と同じ。
        if any_dropped {
            grow(ks, &forms, Some(&dropped), start, out);
        }
    }
}

/// 書記素ごとの正規化をつなぎ、空白の連なりを 1 つにまとめて両端を除く (正規化 v1 の最後の手順)。
fn joined(forms: &[String], dropped: Option<&[bool]>) -> String {
    let mut out = String::new();
    let mut pending = false;
    for (i, f) in forms.iter().enumerate() {
        if dropped.is_some_and(|d| d[i]) {
            continue;
        }
        for c in f.chars() {
            if c.is_whitespace() {
                pending = !out.is_empty();
            } else {
                if pending {
                    out.push(' ');
                    pending = false;
                }
                out.push(c);
            }
        }
    }
    out
}

/// 書記素 `start` から始まる部分文字列の鍵を、長さの上限まで足す。
fn grow(
    ks: &KeyStream,
    forms: &[String],
    dropped: Option<&[bool]>,
    start: usize,
    out: &mut BTreeSet<ValueKey>,
) {
    let mut h = ks.start();
    let mut chars = 0usize;
    let mut pending = false;
    let mut buf = [0u8; 4];
    for (j, f) in forms.iter().enumerate().skip(start) {
        if dropped.is_some_and(|d| d[j]) {
            continue;
        }
        let mut grew = false;
        for c in f.chars() {
            if c.is_whitespace() {
                pending = chars > 0;
            } else {
                if pending {
                    h.update(b" ");
                    chars += 1;
                    pending = false;
                }
                h.update(c.encode_utf8(&mut buf).as_bytes());
                chars += 1;
                grew = true;
            }
        }
        if chars > SUBSTRING_MAX_CHARS {
            return;
        }
        if grew {
            out.insert(ks.finish(&h));
        }
    }
}

/// 部分文字列ごとに正規化し直して鍵を作る (書記素ごとの正規化をつなげない文字列だけに使う)。長さの上限は同じ。
fn add_substrings_slow(
    key: &ForgetKey,
    s: &str,
    clusters: &[std::ops::Range<usize>],
    out: &mut BTreeSet<ValueKey>,
) {
    for i in 0..clusters.len() {
        for j in i..clusters.len() {
            let sub = &s[clusters[i].start..clusters[j].end];
            let (v1, loose) = (normalize_v1(sub), normalize_loose(sub));
            let (n1, nl) = (v1.chars().count(), loose.chars().count());
            if n1 > SUBSTRING_MAX_CHARS && nl > SUBSTRING_MAX_CHARS {
                break;
            }
            for (form, n) in [(v1, n1), (loose, nl)] {
                if !form.is_empty() && n <= SUBSTRING_MAX_CHARS {
                    out.insert(value_key(key, &form));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_key::loose_key;

    const K: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

    fn keys(s: &str) -> SubstringKeys {
        SubstringKeys::of(&K, s)
    }

    /// 定義どおりの作り方 (部分文字列ごとに正規化 v1 と照合の形を当てる)。
    fn by_definition(s: &str) -> BTreeSet<ValueKey> {
        let clusters = graphemes(s);
        let mut out = BTreeSet::new();
        for i in 0..clusters.len() {
            for j in i..clusters.len() {
                let sub = &s[clusters[i].start..clusters[j].end];
                for form in [normalize_v1(sub), normalize_loose(sub)] {
                    if !form.is_empty() && form.chars().count() <= SUBSTRING_MAX_CHARS {
                        out.insert(value_key(&K, &form));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_grapheme_bounded_substring_is_keyed() {
        let k = keys("霧ヶ丘の朝市");
        for sub in ["霧ヶ丘", "朝市", "霧ヶ丘の朝市", "ヶ", "の朝"] {
            assert!(k.contains(&value_key(&K, sub)), "{sub}");
        }
        assert!(!k.contains(&value_key(&K, "霧ヶ丘の夜市")));
    }

    #[test]
    fn substrings_match_through_normalization_and_the_loose_form() {
        // 正規化 v1 (全角・大文字) と照合の形 (漢字に接する空白) の両方で当たる。
        let k = keys("ＭＡＰＬＥ 7 月 22 日の予定");
        assert!(k.contains(&value_key(&K, "maple")));
        assert!(k.contains(&loose_key(&K, "7月22日")));
        assert!(k.contains(&value_key(&K, "7 月 22 日")));
    }

    /// verifies: 04 §7.2 (部分文字列の鍵は、部分文字列ごとに正規化 v1 と照合の形を当てた鍵と同じ)
    #[test]
    fn the_fast_path_equals_the_definition() {
        let samples = [
            "霧ヶ丘に住んで 7 月 22 日に、ＡＢＣ  def\t港町 。",
            "か\u{3099}き ｶﾞｲﾄﾞ ㈱ ﬁne Straße",
            "  前後の空白  と\u{3000}全角の空白  ",
            "e\u{301}cole 👨‍👩‍👧 ok",
        ];
        for s in samples {
            assert_eq!(keys(s).0, by_definition(s), "{s:?}");
        }
    }

    #[test]
    fn combining_sequences_are_not_split() {
        // 「が」を「か」+ 結合の濁点で書いた並びは、濁点だけを切り出さない。
        let k = keys("か\u{3099}き");
        assert!(k.contains(&value_key(&K, "がき")));
        assert!(!k.contains(&value_key(&K, "か")));
    }

    #[test]
    fn blank_substrings_are_not_keyed() {
        assert!(keys("   ").is_empty());
        assert!(keys("").is_empty());
    }

    /// verifies: 04 §7.2 (部分文字列の長さの上限。上限を超える一致は約束しない)
    #[test]
    fn substrings_longer_than_the_limit_are_not_keyed() {
        let long: String = "あ".repeat(SUBSTRING_MAX_CHARS) + "い";
        let k = keys(&long);
        assert!(k.contains(&value_key(&K, &"あ".repeat(SUBSTRING_MAX_CHARS))));
        assert!(!k.contains(&value_key(&K, &long)));
    }

    /// verifies: 04 §7.2 (部分文字列の鍵の費用は 書記素の数 × 上限 に比例する)
    #[test]
    fn the_cost_is_bounded_for_long_values() {
        let text: String = (0..2_000)
            .map(|i| char::from_u32(0x4e00 + (i * 7919) % 20_000).expect("han"))
            .collect();
        let started = std::time::Instant::now();
        let k = keys(&text);
        let elapsed = started.elapsed();
        assert!(k.len() <= 2_000 * SUBSTRING_MAX_CHARS, "{} keys", k.len());
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "{elapsed:?} for 2,000 characters"
        );
    }
}

#[cfg(test)]
mod locate_tests {
    use super::*;
    use crate::value_key::loose_key;

    const K: ForgetKey = ForgetKey::from_bytes_const([9; 32]);

    /// 値の鍵か照合の鍵に当たる部分文字列の範囲を、本文の位置で返す (値の文字列を持たない指示のため)。
    #[test]
    fn substrings_matching_the_keys_are_located() {
        let text = "私はえびが苦手で、えびせんも食べません。";
        let keys = BTreeSet::from([value_key(&K, "えび")]);
        let found: Vec<&str> = locate_keys(&K, &keys, text)
            .into_iter()
            .map(|r| &text[r])
            .collect();
        assert_eq!(found, ["えび", "えび"]);

        let text = "My pet is a Cat.";
        let keys = BTreeSet::from([loose_key(&K, "cat")]);
        let found: Vec<&str> = locate_keys(&K, &keys, text)
            .into_iter()
            .map(|r| &text[r])
            .collect();
        assert!(found.contains(&"Cat"), "{found:?}");
        assert!(locate_keys(&K, &BTreeSet::new(), text).is_empty());
    }
}
