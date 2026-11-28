//! 区切りの鍵 (04 §5.5) を、使う台帳 (§5.2.1) の `keys` から作る純粋な関数 (08 §2.6 の M5.2)。
//!
//! 区切りの鍵は、問いと元の会話の語彙の違いを補う索引の項目で、文脈には入れない (鍵で当たったら、区切りの往復の
//! 原文を添える、05 §3)。作り方は 2 段に分ける。
//!
//! 1. [`check_keys`] (区切りの鍵の検査、B3): 台帳の `keys` の各々の前後の空白を除いて 40 文字で切り、空のものと
//!    関門 6 ([`crate::b3::check_text`]) に通らないものを捨て、重複を除いて 8 個までにする。結果
//!    ([`CheckedKeys`]) は台帳と規則の版だけで決まる。区切りの鍵の埋め込みは、この段の鍵を (台帳の ID,
//!    [`SegmentKey::vk`]) で指す (03 §3.3)。
//! 2. [`CheckedKeys::screen`] (導出の時点のふるい): 忘れるの指示の `vks` に当たる鍵 (台帳の掃除の 4 と同じ述語の
//!    多重の守り、§7.2) と、private の指示の `vks` か導出で private の印を付けた単位の値の鍵に当たる鍵 (§5.5) を
//!    除く。結果 ([`SegmentKeys`]) が索引に入れる区切りの鍵になる。
//!
//! I/O・LLM・書き込みを持たず、同じ入力から同じ結果を返す。鍵の並びは台帳の `keys` の順のまま。
//!
//! **依存の規則**: このモジュールは、旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・
//! `table`・`forget`・`mask`・`view`・`gates`) に依存しない (旧エンジンは v0.5 の判定の前に外す、08 §2.6)。v0.4 の
//! ノートの鍵の検査 (`note::sanitize_keys`) の形は、ここに写して v0.5 の関門 6 に替えた。
//!
//! 旧エンジンの型と取り違えないように、クレートの最上位に型を出さず、`segment_keys::…` で呼ぶ。

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;

use evorag_types::Id;

use crate::apply::{DirectiveSet, SubstringKeys, ValueKeys};
use crate::b3::{Reason, Rules, check_text};
use crate::chars::graphemes;
use crate::ledger::LedgerOutput;
use crate::unit::UnitBody;
use crate::value_key::{ForgetKey, ValueKey, value_key};

/// 区切りの鍵の作り方の版 (04 §5.5)。検査 (切り方・重複の除き方・上限) か導出の時点のふるいを変えたら上げる。
/// 導出の指紋の成分 (§5.3) にし、変われば区切りの鍵の項目をすべて作り直す。関門 6 の規則の版は
/// [`crate::b3::RuleVersions`] が別に持つ。
pub const SEGMENT_KEYS_VERSION: u32 = 1;

/// 1 つの区切りの鍵の数の上限 (04 §5.5。B2 の文法の `maxItems` と同じ、§5.2)。
pub const MAX_KEYS: usize = 8;

/// 1 つの区切りの鍵の長さの上限 (Unicode のスカラー値の数、04 §5.5。B2 の文法の `maxLength` と同じ、§5.2)。
pub const MAX_KEY_CHARS: usize = 40;

/// 検査とふるいを通った区切りの鍵 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    text: String,
    vk: ValueKey,
}

impl SegmentKey {
    /// 索引に入れる鍵の文字列 (前後の空白を除いて切ったもの)。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 鍵の文字列の値の鍵 (04 §5.6)。区切りの鍵の埋め込みを (台帳の ID, この値) で指し (03 §3.3)、重複もこれで
    /// 判定する。
    #[must_use]
    pub fn vk(&self) -> ValueKey {
        self.vk
    }
}

/// 鍵を捨てた理由 (04 §5.5)。並びは当てる順。名前 ([`KeyDrop::as_str`]) は数えるときの固定の符号 (値は出さない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyDrop {
    /// 前後の空白を除いて切ると空になる。
    Empty,
    /// 関門 6 に通らない (理由は関門 6 の中の最初に外れたもの)。
    Text(Reason),
    /// 前の鍵と値の鍵が同じ (正規化 v1 で同じ)。
    Duplicate,
    /// 9 個目からの鍵 ([`MAX_KEYS`])。
    OverLimit,
    /// 忘れるの指示の `vks` に、語句か部分文字列の値の鍵か照合の鍵が当たる (掃除の 4 の多重の守り、04 §7.2)。
    /// 当たった指示のうち ID の最も小さいもの。
    Forgotten(Id),
    /// private の指示の `vks` か、導出で private の印を付けた単位の値の鍵と照合の鍵に、語句か部分文字列の値の鍵か
    /// 照合の鍵が当たる (04 §5.5)。
    Private,
}

impl KeyDrop {
    /// 数えるときの名前 (関門 6 の理由は関門の名前、04 §5.2)。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Text(r) => r.as_str(),
            Self::Duplicate => "duplicate",
            Self::OverLimit => "over_limit",
            Self::Forgotten(_) => "forgotten",
            Self::Private => "private",
        }
    }
}

impl fmt::Display for KeyDrop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 捨てた鍵 1 つ。鍵の文字列は持たない (数えるだけで、値を画面にも `doctor` にも出さないため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dropped {
    /// 台帳の `keys` の中の位置 (型の合った要素だけを数えた 0 からの番号、[`crate::Section::items`])。
    pub index: usize,
    pub reason: KeyDrop,
}

/// 区切りの鍵の検査 (B3) の結果 ([`check_keys`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckedKeys {
    /// 通った鍵と、その台帳の `keys` の中の位置 (台帳の順)。
    keys: Vec<(usize, SegmentKey)>,
    dropped: Vec<Dropped>,
    mismatched: usize,
}

impl CheckedKeys {
    /// 検査を通った鍵 (台帳の順。[`MAX_KEYS`] 個まで)。区切りの鍵の埋め込みは、この鍵を指す。
    pub fn keys(&self) -> impl Iterator<Item = &SegmentKey> {
        self.keys.iter().map(|(_, k)| k)
    }

    /// 検査を通った鍵の値の鍵 (埋め込みを指す値の集合)。
    #[must_use]
    pub fn vks(&self) -> BTreeSet<ValueKey> {
        self.keys().map(SegmentKey::vk).collect()
    }

    /// 検査で捨てた鍵 (台帳の順)。
    #[must_use]
    pub fn dropped(&self) -> &[Dropped] {
        &self.dropped
    }

    /// 台帳の `keys` の要素のうち、文字列でなく読めなかったものの数 ([`crate::Section::mismatched`])。
    #[must_use]
    pub fn mismatched(&self) -> usize {
        self.mismatched
    }

    /// 導出の時点のふるい (04 §5.5): 検査を通った鍵から、忘れるの指示の `vks` に当たるものと、private の値に
    /// 当たるものを除く。当たりは、鍵の語句か部分文字列 (書記素の境界で切ったもの。正規化の後の長さが
    /// [`crate::apply::SUBSTRING_MAX_CHARS`] 以下) の値の鍵か照合の鍵で判定する ([`SubstringKeys`])。
    ///
    /// - 忘れる: [`DirectiveSet::forgets`] のすべて (印を解いたものを含む)。印を解いた指示は、区切りの最初の往復の
    ///   発話のターン `first_turn` が `unblock` の指示の ID より前のときだけ当てる (`None` なら当て続ける。台帳の
    ///   掃除と同じ、04 §7.2)。掃除した台帳では、ここで除く鍵は無い。
    /// - private: [`DirectiveSet::screens_key`] (効いている private の指示の `vks` と、`private_units`)。
    ///
    /// 上限 ([`MAX_KEYS`]) は検査で当て終えているので、ふるいで除いても 9 個目以降の鍵は戻らない (private を
    /// 解いたり付けたりしても、ほかの鍵が入れ替わらない)。
    #[must_use]
    pub fn screen(&self, cx: &Screen<'_>) -> SegmentKeys {
        let mut out = SegmentKeys {
            keys: Vec::with_capacity(self.keys.len()),
            dropped: self.dropped.clone(),
            mismatched: self.mismatched,
        };
        let forgets = cx.directives.forgets();
        let screening = !forgets.is_empty()
            || !cx.directives.privates().is_empty()
            || !cx.private_units.is_empty();
        for (index, k) in &self.keys {
            let reason = screening.then(|| {
                let subs = SubstringKeys::of(cx.key, &k.text);
                let forgotten = forgets
                    .iter()
                    .filter(|f| cx.first_turn.is_none_or(|t| f.applies_at(t)))
                    .find(|f| subs.hits(f.vks()))
                    .map(|f| KeyDrop::Forgotten(f.id()));
                forgotten.or_else(|| {
                    cx.directives
                        .screens_key(&subs, cx.private_units)
                        .then_some(KeyDrop::Private)
                })
            });
            match reason.flatten() {
                Some(reason) => out.dropped.push(Dropped {
                    index: *index,
                    reason,
                }),
                None => out.keys.push(k.clone()),
            }
        }
        out.dropped.sort_by_key(|d| d.index);
        out
    }
}

/// 導出の時点のふるいの入力 ([`CheckedKeys::screen`])。
#[derive(Debug, Clone, Copy)]
pub struct Screen<'a> {
    /// 値の鍵の HMAC の鍵 (`memory.forget_key`)。
    pub key: &'a ForgetKey,
    /// 指示の集合 (忘れるのすべてと、効いている private)。
    pub directives: &'a DirectiveSet,
    /// 導出で private の印を付けた単位 (後継・根拠の位置で private になったものを含む) の値の鍵と照合の鍵
    /// ([`private_unit_keys`])。
    pub private_units: &'a BTreeSet<ValueKey>,
    /// 区切りの最初の往復の発話のターン (印を解いた忘れるの指示と比べる、04 §7.2)。分からなければ `None`。
    pub first_turn: Option<Id>,
}

/// 索引に入れる区切りの鍵 ([`CheckedKeys::screen`] の結果)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentKeys {
    keys: Vec<SegmentKey>,
    dropped: Vec<Dropped>,
    mismatched: usize,
}

impl SegmentKeys {
    /// 索引に入れる鍵 (台帳の順)。区切りの ID を項目の ID にし、鍵の数だけ項目を作る (04 §5.5)。
    #[must_use]
    pub fn keys(&self) -> &[SegmentKey] {
        &self.keys
    }

    /// 索引に入れる鍵の文字列 (台帳の順)。
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(SegmentKey::text)
    }

    /// 検査とふるいで捨てた鍵 (台帳の順)。
    #[must_use]
    pub fn dropped(&self) -> &[Dropped] {
        &self.dropped
    }

    /// 台帳の `keys` の要素のうち、文字列でなく読めなかったものの数。
    #[must_use]
    pub fn mismatched(&self) -> usize {
        self.mismatched
    }
}

/// 区切りの鍵の検査 (04 §5.5、B3) を、使う台帳の出力 `output` の `keys` に当てる。
///
/// 鍵ごとに、前後の空白 (Unicode の White_Space) を除き、[`MAX_KEY_CHARS`] 文字を超える部分を切り捨て (書記素の
/// 途中では切らず、その書記素の前で切る)、切った後の後ろの空白も除く。空になったものと、関門 6 に通らないものを
/// 捨てる。残りから、前の鍵と値の鍵 (正規化 v1、§5.6) が同じものを除き、[`MAX_KEYS`] 個を超えた分を捨てる。鍵が
/// 区切りの本文に含まれる語だけからなる必要はない (言い換えを補うため)。
#[must_use]
pub fn check_keys(rules: &Rules<'_>, key: &ForgetKey, output: &LedgerOutput) -> CheckedKeys {
    let section = output.keys();
    let mut out = check_strings(rules, key, section.items.iter().map(String::as_str));
    out.mismatched = section.mismatched;
    out
}

/// [`check_keys`] を文字列の並びに当てる (台帳の `keys` の型の合った要素の順)。
#[must_use]
pub fn check_strings<'s>(
    rules: &Rules<'_>,
    key: &ForgetKey,
    raw: impl IntoIterator<Item = &'s str>,
) -> CheckedKeys {
    let mut out = CheckedKeys::default();
    let mut seen: BTreeSet<ValueKey> = BTreeSet::new();
    for (index, raw) in raw.into_iter().enumerate() {
        let text = cut(raw.trim(), MAX_KEY_CHARS).trim_end();
        let reason = if text.is_empty() {
            Some(KeyDrop::Empty)
        } else {
            check_text(rules, text).err().map(KeyDrop::Text)
        };
        if let Some(reason) = reason {
            out.dropped.push(Dropped { index, reason });
            continue;
        }
        let vk = value_key(key, text);
        let reason = if seen.contains(&vk) {
            KeyDrop::Duplicate
        } else if out.keys.len() == MAX_KEYS {
            KeyDrop::OverLimit
        } else {
            seen.insert(vk);
            out.keys.push((
                index,
                SegmentKey {
                    text: text.to_owned(),
                    vk,
                },
            ));
            continue;
        };
        out.dropped.push(Dropped { index, reason });
    }
    out
}

/// 導出で private の印を付けた単位の、値の鍵と照合の鍵の集合 ([`Screen::private_units`]。04 §5.5)。
#[must_use]
pub fn private_unit_keys<'u>(
    key: &ForgetKey,
    units: impl IntoIterator<Item = &'u UnitBody>,
) -> BTreeSet<ValueKey> {
    units
        .into_iter()
        .filter(|u| u.private)
        .flat_map(|u| {
            let k = ValueKeys::of(key, &u.value);
            [k.vk, k.loose]
        })
        .collect()
}

/// 台帳を変えた (掃除した) ときに消す区切りの鍵の埋め込みの値の鍵: 前の台帳の検査の結果 `before` にあって、後の
/// 台帳の検査の結果 `after` に無い鍵 (値の鍵の順)。埋め込みは (台帳の ID, 値の鍵) で指すので、残った鍵の埋め込みは
/// そのまま使える (04 §7.2)。導出の時点のふるいで除いた鍵は、索引の項目だけを除き、埋め込みは消さない (`/private
/// off` で同じ鍵が戻るため、§5.5)。
#[must_use]
pub fn stale_embeddings(before: &CheckedKeys, after: &CheckedKeys) -> Vec<ValueKey> {
    let after = after.vks();
    before.vks().difference(&after).copied().collect()
}

/// `text` の頭の、`max` 個以下のスカラー値からなる書記素の並び (書記素の途中では切らない)。
fn cut(text: &str, max: usize) -> &str {
    let mut chars = 0;
    let mut end = 0;
    for g in graphemes(text) {
        chars += text[g.clone()].chars().count();
        if chars > max {
            break;
        }
        end = g.end;
    }
    &text[..end]
}
