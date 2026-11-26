//! 忘れるの伏せ字と `[非表示]` の範囲 (04 §7.3・§7.5 の `redactions`・`hidden`)。
//!
//! 指示は書き換えないので、範囲は書いたときの改訂の番号の位置のまま持ち、読むときに後の伏せ字で写す (§7.3 の
//! 「位置の写し直し」)。写し直しと画面への印の範囲の核は `evorag_store::forget` にあり ([`Redaction::remap_range`]・
//! [`display_marks`])、ここは v0.5 の指示の本体 ([`DirectiveBody`]) をターンごとにまとめて、その核に渡す。
//!
//! ターンごとのまとめ方は、旧エンジンの `forget::TurnForgets` (v0.4 の `fact_directive` の本体を読むもの) の
//! 純粋な部分を、v0.5 の本体に写したもの。

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use evorag_store::forget::{
    HiddenRange, Mark, MarkKind, Redaction, TurnDirective, display_marks, merge_ranges,
    widen_to_graphemes,
};
use evorag_types::Id;

use crate::directive::{DirectiveBody, DirectiveRedaction};
use crate::value_key::{
    Edges, ForgetKey, Haystack, Needle, Placed, ValueKey, Widen, loose_key, value_key,
};

/// 1 つの忘れるの指示が 1 つのターンに持つもの。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    directive: Id,
    redaction: Option<Redaction>,
    hidden: Vec<HiddenRange>,
}

/// ターンごとの忘れるの伏せ字と `[非表示]` の範囲 (指示の ID の順)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnRedactions {
    marker: String,
    turns: BTreeMap<Id, Vec<Step>>,
    /// 記録した伏せ字が形として正しくないターン (合わないターンとして扱う、04 §7.2)。
    invalid_redaction: BTreeSet<Id>,
    /// 記録した `[非表示]` の範囲が形として正しくないターン。
    invalid_hidden: BTreeSet<Id>,
}

impl TurnRedactions {
    /// 忘れるの指示の (ID, 本体) から作る (順は問わない。指示の ID の順に並べ直す)。`marker` は伏せ字の印。
    #[must_use]
    pub fn new<'b>(
        forgets: impl IntoIterator<Item = (Id, &'b DirectiveBody)>,
        marker: &str,
    ) -> Self {
        let mut bodies: Vec<(Id, &DirectiveBody)> = forgets.into_iter().collect();
        bodies.sort_by_key(|(id, _)| *id);
        let mut out = Self {
            marker: marker.to_owned(),
            ..Self::default()
        };
        for (directive, b) in bodies {
            let mut per_turn: BTreeMap<Id, (Vec<&DirectiveRedaction>, Vec<HiddenRange>)> =
                BTreeMap::new();
            for r in &b.redactions {
                per_turn.entry(r.turn).or_default().0.push(r);
            }
            for h in &b.hidden {
                let entry = per_turn.entry(h.turn).or_default();
                match HiddenRange::from_stored(h.rev, u64::from(h.start), u64::from(h.end)) {
                    Some(x) => entry.1.push(x),
                    None => {
                        out.invalid_hidden.insert(h.turn);
                    }
                }
            }
            for (turn, (rs, hidden)) in per_turn {
                let redaction = match rs.first() {
                    None => None,
                    Some(first) => {
                        // 同じ指示の同じターンの要素は、同じ改訂の番号とハッシュを持つ (04 §7.3)。
                        let same = rs.iter().all(|r| {
                            (r.rev_before, r.rev_after, &r.hash)
                                == (first.rev_before, first.rev_after, &first.hash)
                        });
                        match Redaction::from_stored(
                            first.rev_before,
                            first.rev_after,
                            rs.iter().map(|r| (u64::from(r.start), u64::from(r.end))),
                            &first.hash,
                        ) {
                            Ok(r) if same => Some(r),
                            _ => {
                                out.invalid_redaction.insert(turn);
                                None
                            }
                        }
                    }
                };
                out.turns.entry(turn).or_default().push(Step {
                    directive,
                    redaction,
                    hidden,
                });
            }
        }
        out
    }

    /// 伏せ字の印。
    #[must_use]
    pub fn marker(&self) -> &str {
        &self.marker
    }

    /// 指示 `directive` がターン `turn` にした伏せ字。
    #[must_use]
    pub fn of(&self, directive: Id, turn: Id) -> Option<&Redaction> {
        self.turns
            .get(&turn)?
            .iter()
            .find(|s| s.directive == directive)?
            .redaction
            .as_ref()
    }

    /// ターン `turn` の伏せ字 (指示の ID の順)。記録が形として正しくなければ `None`。
    #[must_use]
    pub fn redactions(&self, turn: Id) -> Option<Vec<&Redaction>> {
        if self.invalid_redaction.contains(&turn) {
            return None;
        }
        Some(
            self.turns
                .get(&turn)
                .map(|steps| steps.iter().filter_map(|s| s.redaction.as_ref()).collect())
                .unwrap_or_default(),
        )
    }

    /// ターン `turn` の改訂の番号 `rev` の本文の範囲 `range` を、改訂の番号 `to_rev` の本文の位置に写す (04 §7.3 の
    /// 「位置の写し直し」。伏せ字の前の番号が今の番号と同じ伏せ字を、指示の ID の順に 1 つずつ当てる)。`to_rev` に
    /// 届かない (伏せ字が欠けている・追い越す) か、記録が形として正しくなければ `None`。
    #[must_use]
    pub fn remap(
        &self,
        turn: Id,
        rev: u32,
        range: Range<usize>,
        to_rev: u32,
    ) -> Option<Range<usize>> {
        if self.invalid_redaction.contains(&turn) && rev != to_rev {
            return None;
        }
        let steps = self.turns.get(&turn).map(Vec::as_slice).unwrap_or_default();
        let (mut rev, mut range) = (rev, range);
        for r in steps.iter().filter_map(|s| s.redaction.as_ref()) {
            if rev == to_rev {
                break;
            }
            if r.rev_before() == rev {
                range = r.remap_range(range, &self.marker);
                rev = r.rev_after();
            }
        }
        (rev == to_rev).then_some(range)
    }

    /// ターン `turn` (今の本文 `text`、改訂の番号 `rev`) の印の範囲 (04 §7.3 の「画面への印の範囲」)。指示の無い
    /// ターンは空。指示の伏せ字と今の本文が合わない (当て直しが済んでいない・指示が欠けている・記録が壊れている)
    /// ときは `None` (呼び手は、そのターンを「合わないターン」として扱う、04 §7.2)。
    #[must_use]
    pub fn marks(&self, turn: Id, text: &str, rev: u32) -> Option<Vec<Mark>> {
        if self.invalid_redaction.contains(&turn) || self.invalid_hidden.contains(&turn) {
            return None;
        }
        let Some(steps) = self.turns.get(&turn) else {
            return Some(Vec::new());
        };
        let dirs: Vec<TurnDirective<'_>> = steps
            .iter()
            .map(|s| TurnDirective {
                redaction: s.redaction.as_ref(),
                hidden: &s.hidden,
            })
            .collect();
        display_marks(text, rev, &dirs, &self.marker)
    }

    /// ターンの `[非表示]` の範囲 (今の本文の位置。伏せ字にしない忘れるの想起・候補の帯・今のセッションの履歴で
    /// 伏せる範囲、04 §7.1)。印の範囲を求められない (合わないターン) ときは `None`: 呼び手はそのターンを除く。
    #[must_use]
    pub fn hidden_ranges(&self, turn: Id, text: &str, rev: u32) -> Option<Vec<Range<usize>>> {
        self.marks(turn, text, rev).map(|marks| {
            marks
                .into_iter()
                .filter(|m| m.kind == MarkKind::Hidden)
                .map(|m| m.range)
                .collect()
        })
    }
}

/// 本文の中の、忘れる値の範囲を探した結果 ([`locate_forgotten`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// 一致する範囲 (書記素の境界に広げ、まとめたもの。始まりの順。空なら一致なし)。
    Ranges(Vec<Range<usize>>),
    /// 一致の文字列の値の鍵と照合の鍵のどちらも、忘れる値の鍵に合わない (本文が記録と食い違う。伏せ字なら中止、
    /// 04 §7.3)。
    Mismatch,
    /// 正規化した本文に値はあるが、位置を戻せない (呼び手は伏せる側に倒す: 範囲の全体を伏せるか中止する)。
    Unplaceable,
}

/// 本文 `text` (`within` があればその範囲) の中で、忘れる値 `values` のどれかに一致する範囲を探す (04 §7.3 の
/// 伏せる範囲、§7.1 の `[非表示]` の範囲)。値ごとに、正規化 v1 の一致を先に採り、照合の形ではそれと重ならない
/// 一致を足す (§5.6 の「照合の順」)。一致の文字列 (広げる前) の値の鍵か照合の鍵が `keys` (指示の `vks`) にある
/// ことを確かめ、両端を書記素の境界に広げる (§7.3)。英数字は本文の全体の語の境界で当てる。
///
/// 旧エンジンの `forget::whole_matches` の純粋な部分を写したもの (範囲 `within` を足した)。
#[must_use]
pub fn locate_forgotten(
    key: &ForgetKey,
    keys: &BTreeSet<ValueKey>,
    text: &str,
    within: Option<Range<usize>>,
    values: &[&str],
) -> Located {
    let hay = Haystack::new(text);
    let inside = |r: &Range<usize>| {
        within
            .as_ref()
            .is_none_or(|w| w.start <= r.start && r.end <= w.end)
    };
    let checked = |r: &Range<usize>| {
        text.get(r.clone())
            .is_some_and(|s| keys.contains(&value_key(key, s)) || keys.contains(&loose_key(key, s)))
    };
    let mut out = Vec::new();
    let mut unplaceable = false;
    for v in values {
        match hay.find_all(&Needle::new(v), Edges::Word, Widen::No) {
            Placed::Ranges(hits) => {
                for h in hits.into_iter().filter(|r| inside(r)) {
                    if !checked(&h) {
                        return Located::Mismatch;
                    }
                    out.push(widen_to_graphemes(text, h));
                }
            }
            Placed::Partial(hits) => {
                if hits.iter().filter(|r| inside(r)).any(|h| !checked(h)) {
                    return Located::Mismatch;
                }
                unplaceable = true;
            }
            Placed::Unplaceable => unplaceable = true,
        }
    }
    if unplaceable {
        return Located::Unplaceable;
    }
    Located::Ranges(merge_ranges(out))
}

/// 伏せ字にしない忘れるの、根拠の文 `sentence` の中の `[非表示]` の範囲 (04 §7.1 の確認の画面)。根拠の文に値の
/// 一致が見つからないときは、書記素の境界に広げた根拠の文の全体にする (private と同じ、§7.5)。位置を戻せない
/// ときも、伏せる側に倒して根拠の文の全体にする。食い違い ([`Located::Mismatch`]) はそのまま返す。
#[must_use]
pub fn hidden_in_evidence(
    key: &ForgetKey,
    keys: &BTreeSet<ValueKey>,
    text: &str,
    sentence: Range<usize>,
    values: &[&str],
) -> Located {
    match locate_forgotten(key, keys, text, Some(sentence.clone()), values) {
        Located::Ranges(r) if !r.is_empty() => Located::Ranges(r),
        Located::Mismatch => Located::Mismatch,
        Located::Ranges(_) | Located::Unplaceable => {
            Located::Ranges(vec![widen_to_graphemes(text, sentence)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::ValueKeys;
    use crate::directive::{DirectiveOp, TurnSpan};

    const K: ForgetKey = ForgetKey::from_bytes_const([5; 32]);
    const M: &str = "[伏せ字]";

    fn id(n: u64) -> Id {
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(b)
    }

    fn keys(values: &[&str]) -> BTreeSet<ValueKey> {
        values
            .iter()
            .flat_map(|v| {
                let k = ValueKeys::of(&K, v);
                [k.vk, k.loose]
            })
            .collect()
    }

    fn span(text: &str, needle: &str) -> Range<usize> {
        let s = text.find(needle).expect("needle in text");
        s..s + needle.len()
    }

    fn redacting(
        turn: Id,
        text: &str,
        rev: u32,
        ranges: &[Range<usize>],
    ) -> (DirectiveBody, String) {
        let (r, after) = Redaction::plan(text, rev, ranges.iter().cloned(), M).expect("plan");
        let mut b = DirectiveBody::new(DirectiveOp::Forget);
        b.redact = true;
        b.redactions = r
            .ranges()
            .iter()
            .map(|x| DirectiveRedaction {
                turn,
                rev_before: rev,
                rev_after: rev + 1,
                start: u32::try_from(x.start).expect("fits"),
                end: u32::try_from(x.end).expect("fits"),
                hash: r.hash().to_string(),
            })
            .collect();
        (b, after)
    }

    /// verifies: 04 §7.1 (伏せ字にしないときの範囲: 正規化 v1 の一致を先に、照合の形で重ならない一致を足す。値の鍵で確かめる)、§7.3
    #[test]
    fn forgotten_values_are_located_and_checked() {
        let text = "誕生日は7月22日で、7 月 22 日に祝う。";
        let k = keys(&["7月22日"]);
        let Located::Ranges(r) = locate_forgotten(&K, &k, text, None, &["7月22日"]) else {
            panic!("expected ranges");
        };
        assert_eq!(r, vec![span(text, "7月22日"), span(text, "7 月 22 日")]);
        // 範囲を根拠の文に限る。
        let first = 0..text.find('、').expect("comma");
        assert_eq!(
            locate_forgotten(&K, &k, text, Some(first), &["7月22日"]),
            Located::Ranges(vec![span(text, "7月22日")])
        );
        // 鍵の合わない値は食い違い。
        assert_eq!(
            locate_forgotten(&K, &keys(&["8月1日"]), text, None, &["7月22日"]),
            Located::Mismatch
        );
    }

    /// verifies: 04 §7.1 (根拠の文に値の一致が見つからないときは、根拠の文の全体を [非表示] にする)
    #[test]
    fn evidence_without_the_value_is_hidden_whole() {
        let text = "前の文。値の無い文です。";
        let sentence = span(text, "値の無い文です。");
        assert_eq!(
            hidden_in_evidence(&K, &keys(&["港南台"]), text, sentence.clone(), &["港南台"]),
            Located::Ranges(vec![sentence])
        );
    }

    /// verifies: 04 §7.3 (位置の写し直し: 指示の ID の順に 1 つずつ当て、届かなければ写さない)
    #[test]
    fn positions_are_remapped_through_later_redactions_in_order() {
        let turn = id(1);
        let text = "aa secret bb other cc";
        let (f1, after1) = redacting(turn, text, 0, &[span(text, "secret")]);
        let (f2, after2) = redacting(turn, &after1, 1, &[span(&after1, "other")]);
        let red = TurnRedactions::new([(id(20), &f2), (id(10), &f1)], M);
        let cc = span(text, "cc");
        let mapped = red.remap(turn, 0, cc.clone(), 2).expect("reaches rev 2");
        assert_eq!(&after2[mapped], "cc");
        // 1 段だけ写す。
        let one = red.remap(turn, 0, cc.clone(), 1).expect("reaches rev 1");
        assert_eq!(&after1[one], "cc");
        // 届かない改訂の番号には写さない。
        assert_eq!(red.remap(turn, 0, cc.clone(), 3), None);
        assert_eq!(red.remap(id(2), 0, 0..2, 1), None);
        assert_eq!(red.remap(id(2), 4, 0..2, 4), Some(0..2));
    }

    /// verifies: 04 §7.3 (画面への印の範囲: [非表示] は後の伏せ字で写す)、§7.5 の hidden
    #[test]
    fn hidden_ranges_follow_later_redactions() {
        let turn = id(1);
        let text = "愛犬はモクレンで、港南台に住む。";
        let mut keep = DirectiveBody::new(DirectiveOp::Forget);
        let h = span(text, "モクレン");
        keep.hidden = vec![TurnSpan {
            turn,
            rev: 0,
            start: u32::try_from(h.start).expect("fits"),
            end: u32::try_from(h.end).expect("fits"),
        }];
        let (redact, after) = redacting(turn, text, 0, &[span(text, "港南台")]);
        let red = TurnRedactions::new([(id(10), &keep), (id(20), &redact)], M);
        let hidden = red.hidden_ranges(turn, &after, 1).expect("consistent");
        assert_eq!(hidden.len(), 1);
        assert_eq!(&after[hidden[0].clone()], "モクレン");
        // 伏せ字の前の本文のままでは合わない (当て直しが済んでいない)。
        assert_eq!(red.hidden_ranges(turn, text, 0), None);
        // 指示の無いターンは空。
        assert_eq!(red.marks(id(9), "x", 0), Some(Vec::new()));
    }

    #[test]
    fn malformed_redactions_make_the_turn_unmatched() {
        let turn = id(1);
        let mut b = DirectiveBody::new(DirectiveOp::Forget);
        b.redactions = vec![DirectiveRedaction {
            turn,
            rev_before: 1,
            rev_after: 1,
            start: 0,
            end: 1,
            hash: "00".repeat(32),
        }];
        let red = TurnRedactions::new([(id(10), &b)], M);
        assert_eq!(red.redactions(turn), None);
        assert_eq!(red.marks(turn, "x", 1), None);
        assert_eq!(red.remap(turn, 1, 0..1, 2), None);
    }
}
