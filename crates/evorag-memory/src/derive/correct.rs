//! 訂正の関門のうち、畳みの前に当てる 1・2・3・5 と `forgotten` (04 §6 の「関門」「判定の入力」、§5.3 の「訂正を
//! 当てる位置」)。関門 4 と単位への効き方は、畳みの中で訂正の位置に来たときに当てる (`fold`)。
//!
//! 新しい値と古い値は、LLM の写しではなく、台帳の `corr` の穴の範囲と、同じ根拠の文の単位の候補 (関門を通った
//! もの) から決定論に決める (04 §6 の「値の決め方」「訂正の型の使い方」)。

use std::fmt;
use std::ops::Range;

use evorag_types::Id;

use super::{Cand, Inputs, Speaker};
use crate::apply::ValueKeys;
use crate::b3::{Passed, Recorded, quoted};
use crate::chars::is_separator;
use crate::ledger::{ByteRange, CorrInput, LedgerVerdict};

/// 訂正の関門と単位への効き方で数える理由 (04 §6 の括弧の中の名前と、効き方の結果)。並びは当てる順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CorrectionReason {
    /// 判定の `id` が、台帳の `corr` の候補の番号に無い (04 §5.2 の「`id` が候補の番号の外の判定」)。
    UnknownId,
    /// 関門 1: `verdict` が `accept` でない。
    NotCorrection,
    /// 古い値の穴の無い型 (`I meant {new}` など)。単位を変えない訂正 (学習 (v0.7) にだけ使う)。
    Unanchored,
    /// 関門 2: 新しい値の単位の候補が、`{new}` の穴の固定の部分に接する端に無い。
    NewNotInUtterance,
    /// 関門 3: 新しい値の位置か型の固定の部分が、括弧・引用符の内側にある。
    Quoted,
    /// 関門 5: 古い値と新しい値の値の鍵が同じか、照合の形で同じ。
    Direction,
    /// 古い値か新しい値が、忘れるの指示の `vks` にある。
    Forgotten,
    /// 関門 4: 古い値が直前の応答・それより前の発話・今の値のどれにも無い、または関門 6 か 200 文字の上限に外れる。
    OldNotFound,
    /// 検証済みの訂正だが、古い値が今の値 (時制 `current` の `Live`) のどれとも一致しないか、新しい値の候補が同じ
    /// (entity, 属性) に無い。単位を変えない。
    NoEffect,
    /// 検証済みの訂正だが、古い値が 2 つ以上の (entity, 属性) の今の値と一致する。単位を変えない。
    Ambiguous,
    /// 検証済みの訂正が、単位の置き換えの条件 (04 §5.3 の「単値の辞書の属性」の 2 つ目) になった。
    Applied,
    /// 関門 4 の材料 (候補の発話の前の、同じセッションのターン) が入力に欠けている。関門 4 を落とす側に倒す。
    ContextMissing,
    /// 版の不一致の段で選んだ台帳の判定 (使わない、04 §5.2.1)。
    StaleLedger,
}

impl CorrectionReason {
    /// 数えるときの名前。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownId => "unknown_id",
            Self::NotCorrection => "not_correction",
            Self::Unanchored => "unanchored",
            Self::NewNotInUtterance => "new_not_in_utterance",
            Self::Quoted => "quoted",
            Self::Direction => "direction",
            Self::Forgotten => "forgotten",
            Self::OldNotFound => "old_not_found",
            Self::NoEffect => "no_effect",
            Self::Ambiguous => "ambiguous",
            Self::Applied => "applied",
            Self::ContextMissing => "context_missing",
            Self::StaleLedger => "stale_ledger",
        }
    }
}

impl fmt::Display for CorrectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 古い値の上限 (文字数、04 §6 の関門 4)。
pub(super) const OLD_MAX_CHARS: usize = 200;

/// 畳みの前の関門を通った訂正 (畳みの中で関門 4 と効き方を当てる)。
#[derive(Debug, Clone)]
pub(super) struct Prepared {
    /// 候補の発話のターン。
    pub turn: Id,
    /// 候補の発話の改訂の番号。
    pub rev: u32,
    /// 新しい値の範囲 (入力の順の位置。新しい値の候補の値の範囲)。
    pub new_range: ByteRange,
    /// 古い値 (`{old}` の穴の中で一致した保存した本文の部分文字列)。
    pub old: Recorded,
    /// 新しい値の単位の候補 (畳みの候補の番号。入力の順)。
    pub cands: Vec<usize>,
    /// 関門 4 で古い値を探す本文のターン (直前の応答と、同じセッションのそれより前の利用者の発話)。
    pub context: Vec<Id>,
}

/// 保存した本文 `text` の範囲 `r` から、前後の区切りの文字 (空白・記号) を除いた範囲。空になれば `None`。
fn trim_separators(text: &str, r: &Range<usize>) -> Option<Range<usize>> {
    let s = text.get(r.clone())?;
    let lead = s.len() - s.trim_start_matches(is_separator).len();
    let trail = s.len() - s.trim_end_matches(is_separator).len();
    (lead + trail < s.len()).then(|| r.start + lead..r.end - trail)
}

/// 古い値の穴の前後の読点・空白を除いた範囲 (04 §6 の「古い値は穴の全体」)。
fn trim_old(text: &str, r: &Range<usize>) -> Option<Range<usize>> {
    let s = text.get(r.clone())?;
    let edge = |c: char| c.is_whitespace() || matches!(c, '、' | '，' | ',');
    let lead = s.len() - s.trim_start_matches(edge).len();
    let trail = s.len() - s.trim_end_matches(edge).len();
    (lead + trail < s.len()).then(|| r.start + lead..r.end - trail)
}

fn to_range(r: ByteRange) -> Range<usize> {
    usize::try_from(r.start).unwrap_or(usize::MAX)..usize::try_from(r.end).unwrap_or(usize::MAX)
}

/// 型の固定の部分の文字数 (読点と空白を数えない。型の文字列の `{new}`・`{old}` の外)。
fn fixed_chars(s: &str) -> usize {
    s.chars()
        .filter(|c| !(c.is_whitespace() || matches!(c, '、' | '，' | ',')))
        .count()
}

/// 本文 `text` の位置 `from` から前 (`back`) か後ろへ `n` 文字の範囲。
fn chars_from(text: &str, from: usize, n: usize, back: bool) -> Range<usize> {
    if back {
        let start = text
            .get(..from)
            .and_then(|s| {
                s.char_indices()
                    .rev()
                    .nth(n.saturating_sub(1))
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        start..from
    } else {
        let tail = text.get(from..).unwrap_or("");
        let end = tail
            .char_indices()
            .nth(n)
            .map_or(text.len(), |(i, _)| from + i);
        from..end
    }
}

/// 型の固定の部分がある範囲 (2 つの穴の間と、型の頭・末尾の固定の部分の長さの分)。括弧の内側にあってはならない
/// (04 §6 の関門 3)。
fn fixed_regions(
    text: &str,
    pattern: &str,
    new: &Range<usize>,
    old: &Range<usize>,
) -> Vec<Range<usize>> {
    let (first, second) = if new.start <= old.start {
        (new, old)
    } else {
        (old, new)
    };
    let mut out = Vec::new();
    if first.end <= second.start {
        out.push(first.end..second.start);
    }
    let head = pattern.split('{').next().unwrap_or("");
    let tail = pattern.rsplit('}').next().unwrap_or("");
    let (h, t) = (fixed_chars(head), fixed_chars(tail));
    if h > 0 {
        out.push(chars_from(text, first.start, h, true));
    }
    if t > 0 {
        out.push(chars_from(text, second.end, t, false));
    }
    out
}

/// 畳みの前の関門 (1・2・3・5 と `forgotten`) を当てる (04 §6)。`corr` は判定の `id` で引いた候補、`cands` は
/// 同じ台帳の、関門を通った単位の候補 (畳みの候補の番号と組。入力の順)、`dropped` は同じ台帳の、関門 1〜7 を通った
/// が関門 8 で捨てた候補。通れば `Ok(Some)`、古い値の穴の無い型は `Ok(None)` (単位を変えない訂正)、外れれば最初に
/// 外れた理由。新しい値の候補が関門 8 で捨てた候補にしか無ければ `forgotten`。
pub(super) fn prepare(
    inputs: &Inputs<'_>,
    corr: &CorrInput,
    verdict: LedgerVerdict,
    cands: &[(usize, &Cand)],
    dropped: &[&Passed],
) -> Result<Option<Prepared>, CorrectionReason> {
    if verdict != LedgerVerdict::Accept {
        return Err(CorrectionReason::NotCorrection);
    }
    let Some(old_hole) = corr.old else {
        return Ok(None);
    };
    let turn = inputs
        .turns
        .get(&corr.turn)
        .filter(|t| t.speaker == Speaker::User && t.rev == corr.rev)
        .ok_or(CorrectionReason::NewNotInUtterance)?;
    let text = turn.text;
    // 関門 2: 新しい値の候補は、同じターンの、値の位置が `{new}` の穴の固定の部分に接する端にあるもの。後ろに固定の
    // 部分がある穴では値の終わりを穴の終わりに、末尾の穴では値の始まりを穴の始まりに合わせる (間の記号・空白は
    // 飛ばす)。
    let hole =
        trim_separators(text, &to_range(corr.new)).ok_or(CorrectionReason::NewNotInUtterance)?;
    let trailing = corr.pattern.trim_end().ends_with("{new}");
    let at_edge = |p: &Passed| {
        let v = to_range(p.value.range);
        p.turn == corr.turn
            && p.rev == corr.rev
            && hole.start <= v.start
            && v.end <= hole.end
            && if trailing {
                v.start == hole.start
            } else {
                v.end == hole.end
            }
    };
    let aligned: Vec<(usize, &Cand)> = cands
        .iter()
        .copied()
        .filter(|(_, c)| at_edge(&c.passed))
        .collect();
    if aligned.is_empty() {
        // 新しい値の候補が忘れた値だった (関門 8 で捨てた) ときは、忘れた値として数える (04 §6)。
        return Err(if dropped.iter().any(|p| at_edge(p)) {
            CorrectionReason::Forgotten
        } else {
            CorrectionReason::NewNotInUtterance
        });
    }
    // 関門 3: 新しい値の一致と型の固定の部分が、括弧・引用符の内側に無い。
    let old_range = to_range(old_hole);
    let rules = inputs.env.rules;
    if quoted(
        rules,
        text,
        &fixed_regions(text, &corr.pattern, &hole, &old_range),
    ) {
        return Err(CorrectionReason::Quoted);
    }
    let unquoted: Vec<(usize, &Cand)> = aligned
        .into_iter()
        .filter(|(_, c)| !quoted(rules, text, &[to_range(c.passed.value.range)]))
        .collect();
    if unquoted.is_empty() {
        return Err(CorrectionReason::Quoted);
    }
    // 古い値: `{old}` の穴の全体 (前後の読点・空白を除く) の保存した本文。
    let old_at = trim_old(text, &old_range).ok_or(CorrectionReason::OldNotFound)?;
    let old_text = text
        .get(old_at.clone())
        .ok_or(CorrectionReason::OldNotFound)?;
    let old = Recorded {
        text: old_text.to_owned(),
        range: ByteRange {
            start: u32::try_from(old_at.start).map_err(|_| CorrectionReason::OldNotFound)?,
            end: u32::try_from(old_at.end).map_err(|_| CorrectionReason::OldNotFound)?,
        },
        keys: ValueKeys::of(inputs.env.key, old_text),
    };
    // 関門 5: 向き。古い値と新しい値が値の鍵か照合の形で同じなら落とす。
    let directed: Vec<(usize, &Cand)> = unquoted
        .into_iter()
        .filter(|(_, c)| {
            let n = c.passed.value.keys;
            n.vk != old.keys.vk && n.loose != old.keys.loose
        })
        .collect();
    // 訂正の位置は、新しい値の候補のうち最も前の値の範囲 (台帳の中の候補の順に依らない)。
    let Some(new_range) = directed
        .iter()
        .map(|(_, c)| c.passed.value.range)
        .min_by_key(|r| (r.start, r.end))
    else {
        return Err(CorrectionReason::Direction);
    };
    // 忘れた値を訂正で戻さない (04 §6 の「判定の入力」。entity と属性を問わない広い側)。
    let mut keys: Vec<ValueKeys> = vec![old.keys];
    keys.extend(directed.iter().map(|(_, c)| c.passed.value.keys));
    if inputs
        .directives
        .forgotten_correction(&keys, corr.turn)
        .is_some()
    {
        return Err(CorrectionReason::Forgotten);
    }
    Ok(Some(Prepared {
        turn: corr.turn,
        rev: corr.rev,
        new_range,
        old,
        cands: directed.iter().map(|(i, _)| *i).collect(),
        context: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators_are_trimmed_from_holes() {
        let t = "いや、大阪じゃなくて 東京。";
        let r = 0..t.len();
        let got = trim_separators(t, &r).map(|r| &t[r]);
        assert_eq!(got, Some("いや、大阪じゃなくて 東京"));
        assert_eq!(trim_separators("、。 ", &(0.."、。 ".len())), None);
        let o = " 大阪、";
        assert_eq!(trim_old(o, &(0..o.len())).map(|r| &o[r]), Some("大阪"));
    }

    #[test]
    fn fixed_regions_cover_the_gap_and_the_tail() {
        let t = "東京、大阪ではなく";
        let new = 0.."東京".len();
        let old = "東京、".len().."東京、大阪".len();
        let got: Vec<&str> = fixed_regions(t, "{new}、{old}ではなく", &new, &old)
            .into_iter()
            .map(|r| &t[r])
            .collect();
        assert_eq!(got, vec!["、", "ではなく"]);
        let t = "I meant Kyoto";
        let new = "I meant ".len()..t.len();
        let got: Vec<&str> = fixed_regions(t, "I meant {new}", &new, &new)
            .into_iter()
            .map(|r| &t[r])
            .collect();
        assert_eq!(got, vec!["meant "]);
    }
}
