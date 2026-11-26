//! 語句・部分文字列の値の鍵 (04 §7.2 の台帳の掃除の 2・4、§5.5 の private の鍵のふるい)。

use std::collections::{BTreeSet, HashSet};

use crate::chars::grapheme_boundaries;
use crate::value_key::{ForgetKey, ValueKey, normalize_loose, normalize_v1, value_key};

/// 文字列 `s` の、書記素の境界で切ったすべての部分文字列 (全体を含む) の値の鍵と照合の鍵 (04 §5.6)。
///
/// 指示の `vks` は HMAC なので、当たる部分文字列の長さは分からない。そのためすべての部分文字列を引く (書記素が
/// n 個なら n(n+1)/2 個)。同じ形に正規化される部分文字列は 1 回だけ HMAC にする。正規化すると空になる部分文字列
/// (空白だけ) は引かない。
///
/// 照合の鍵は、部分文字列の照合の形 ([`normalize_loose`]) に値の鍵と同じ HMAC を当てたもの
/// ([`crate::value_key::loose_key`] と同じ値。照合の形は正規化 v1 の結果なので、正規化 v1 を当て直しても変わらない)。
pub(crate) fn substring_keys(key: &ForgetKey, s: &str) -> BTreeSet<ValueKey> {
    let bounds = grapheme_boundaries(s);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = BTreeSet::new();
    for (i, &start) in bounds.iter().enumerate() {
        for &end in &bounds[i + 1..] {
            let sub = &s[start..end];
            for form in [normalize_v1(sub), normalize_loose(sub)] {
                if !form.is_empty() && seen.insert(form.clone()) {
                    out.insert(value_key(key, &form));
                }
            }
        }
    }
    out
}

/// `strings` のどれかの部分文字列の鍵 ([`substring_keys`]) の和。
pub(crate) fn substring_keys_of<'s>(
    key: &ForgetKey,
    strings: impl IntoIterator<Item = &'s str>,
) -> BTreeSet<ValueKey> {
    strings
        .into_iter()
        .flat_map(|s| substring_keys(key, s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_key::loose_key;

    const K: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

    #[test]
    fn every_grapheme_bounded_substring_is_keyed() {
        let keys = substring_keys(&K, "霧ヶ丘の朝市");
        for sub in ["霧ヶ丘", "朝市", "霧ヶ丘の朝市", "ヶ", "の朝"] {
            assert!(keys.contains(&value_key(&K, sub)), "{sub}");
        }
        assert!(!keys.contains(&value_key(&K, "霧ヶ丘の夜市")));
    }

    #[test]
    fn substrings_match_through_normalization_and_the_loose_form() {
        // 正規化 v1 (全角・大文字) と照合の形 (漢字に接する空白) の両方で当たる。
        let keys = substring_keys(&K, "ＭＡＰＬＥ 7 月 22 日の予定");
        assert!(keys.contains(&value_key(&K, "maple")));
        assert!(keys.contains(&loose_key(&K, "7月22日")));
        assert!(keys.contains(&value_key(&K, "7 月 22 日")));
    }

    #[test]
    fn combining_sequences_are_not_split() {
        // 「が」を「か」+ 結合の濁点で書いた並びは、濁点だけを切り出さない。
        let keys = substring_keys(&K, "か\u{3099}き");
        assert!(keys.contains(&value_key(&K, "がき")));
        assert!(!keys.contains(&value_key(&K, "か")));
    }

    #[test]
    fn blank_substrings_are_not_keyed() {
        assert!(substring_keys(&K, "   ").is_empty());
        assert!(substring_keys(&K, "").is_empty());
    }
}
