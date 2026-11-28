//! 区切りの記録を失ったときに、台帳から区切りを戻す (04 §2 の「閉じた区切りは `Immediate` で書く」、v0.5)。
//! I/O と LLM を使わない純粋な関数。
//!
//! 台帳の `sentences` は番号を振った文のターンしか持たない (前のふるいで送らない発話は入らない) ので、往復の並びは
//! 台帳の `inputs` の `through` (最後に入れた往復の発話と往復の数) と、セッションの今の発話の並びから戻し、
//! 最初の往復から作った区切りの ID が台帳の `segment` と合うことで確かめる。

use std::collections::{BTreeMap, BTreeSet};

use evorag_types::Id;

use crate::kinds::TurnRole;
use crate::ledger::LedgerBody;
use crate::ledger_select::StoredLedger;
use crate::read::Turn;
use crate::segment::{SegmentBody, segment_id};
use crate::segment_plan::utterance_chars;

/// 台帳から区切りを戻せない理由 ([`restore_segment`])。戻せないときは、その往復を区切り直す
/// ([`crate::plan_segments`]。台帳は消さずに残る)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    /// この区切りの、`through` を持つ区切りの呼び出しの台帳が無い。
    #[error("no ledger of segment {segment:?} has `through`")]
    NoLedger { segment: Id },
    /// 台帳の `through` の発話が、セッションの今の発話に無い (読めない・消えた)。
    #[error("the last pair {turn:?} of ledger {ledger:?} is not an utterance of the session")]
    LastPairMissing { ledger: Id, turn: Id },
    /// 台帳の `through` の往復の数が 0 か、セッションの発話の数より多い。
    #[error("ledger {ledger:?} counts {pairs} pairs, which the session does not have")]
    PairCount { ledger: Id, pairs: u32 },
    /// 戻した最初の往復から作った区切りの ID が、台帳の `segment` と合わない (間の発話が読めない・増えた)。
    #[error("the pairs of ledger {ledger:?} do not start segment {segment:?}")]
    FirstPair { ledger: Id, segment: Id },
    /// 台帳の `sentences`・`corr` の発話か、`refs` の応答が答える発話が、戻した往復の外にある。
    #[error("turn {turn:?} of ledger {ledger:?} is outside the restored pairs")]
    Outside { ledger: Id, turn: Id },
    /// 戻した往復の範囲が、記録した区切りの往復の範囲と重なる。
    #[error("the restored pairs overlap recorded segment {segment:?}")]
    Overlap { segment: Id },
}

/// 1 つの台帳から戻した往復の並び。
fn pairs_of(
    ledger: Id,
    body: &LedgerBody,
    segment: Id,
    utterances: &[Id],
) -> Result<Option<Vec<Id>>, RestoreError> {
    let Some(through) = body.inputs.through else {
        return Ok(None);
    };
    let last = utterances.iter().position(|&u| u == through.turn).ok_or(
        RestoreError::LastPairMissing {
            ledger,
            turn: through.turn,
        },
    )?;
    let count = usize::try_from(through.pairs).unwrap_or(usize::MAX);
    if count == 0 || count > last + 1 {
        return Err(RestoreError::PairCount {
            ledger,
            pairs: through.pairs,
        });
    }
    let pairs = utterances[last + 1 - count..=last].to_vec();
    if segment_id(pairs[0]) != segment {
        return Err(RestoreError::FirstPair { ledger, segment });
    }
    Ok(Some(pairs))
}

/// 台帳の発話と応答が、戻した往復の中にあるか。
fn check_inside(
    ledger: Id,
    body: &LedgerBody,
    pairs: &BTreeSet<Id>,
    reply_to: &BTreeMap<Id, Option<Id>>,
) -> Result<(), RestoreError> {
    let utterances = body
        .sentences
        .iter()
        .map(|s| s.turn)
        .chain(body.inputs.corr.iter().map(|c| c.turn));
    for turn in utterances {
        if !pairs.contains(&turn) {
            return Err(RestoreError::Outside { ledger, turn });
        }
    }
    for r in &body.inputs.refs {
        // 今は読めない応答は確かめられないので飛ばす (有効性は台帳の選び方で別に見る)。
        if let Some(to) = reply_to.get(&r.turn)
            && !to.is_some_and(|u| pairs.contains(&u))
        {
            return Err(RestoreError::Outside {
                ledger,
                turn: r.turn,
            });
        }
    }
    Ok(())
}

/// 記録を失った区切り `segment` の本体を、その区切りの台帳から戻す (04 §2)。
///
/// - `turns` はセッション `session` のターン (順は問わない。ほかのセッションのターンは無視する)。`ledgers` は
///   `segment` が `segment` の区切りの呼び出しの台帳 (状態と有効性は問わない。ほかの台帳は無視する)。`known` は
///   そのセッションの、記録の残っている区切り。
/// - 往復の並び: `through` を持つ台帳ごとに、セッションの今の発話 (ID の順) のうち `through` の発話で終わる
///   `through` の数の発話を往復とし、最初の往復から作った区切りの ID が `segment` と合うことを確かめる。すべての
///   台帳が合えば、往復の最も多い台帳の並びを使う (同じ最初の往復から始まるので、ほかの台帳の並びはその前の部分)。
///   台帳の `sentences`・`corr` の発話と、`refs` の今読める応答が答える発話は、その並びの中にあることを確かめる。
/// - 戻した区切りは閉じたものとする。大きさの見積もり (`chars`) は、今の前のふるいで数え直す。
///
/// # Errors
/// `through` を持つ台帳が無いとき、どれかの台帳の `through` の発話がセッションに無いか、往復の数が合わないか、
/// 最初の往復が区切りの ID に合わないとき (間の発話が読めない・増えた)、台帳の発話や応答が並びの外にあるとき、
/// 戻した往復が記録した区切りと重なるとき。
pub fn restore_segment(
    segment: Id,
    session: Id,
    turns: &[Turn],
    ledgers: &[StoredLedger<'_>],
    known: &[SegmentBody],
) -> Result<SegmentBody, RestoreError> {
    let turns: Vec<&Turn> = turns.iter().filter(|t| t.body.session == session).collect();
    let mut utterances: Vec<Id> = turns
        .iter()
        .filter(|t| t.body.role == TurnRole::User)
        .map(|t| t.envelope.id)
        .collect();
    utterances.sort_unstable();
    utterances.dedup();
    let reply_to: BTreeMap<Id, Option<Id>> = turns
        .iter()
        .filter(|t| t.body.role == TurnRole::Assistant)
        .map(|t| (t.envelope.id, t.body.reply_to))
        .collect();

    let ledgers: Vec<(Id, &LedgerBody)> = ledgers
        .iter()
        .filter(|l| l.body.is_segment_call() && l.body.segment == Some(segment))
        .map(|l| (l.id, l.body))
        .collect();
    let mut restored: Option<Vec<Id>> = None;
    for &(id, body) in &ledgers {
        if let Some(pairs) = pairs_of(id, body, segment, &utterances)?
            && restored.as_ref().is_none_or(|r| pairs.len() > r.len())
        {
            restored = Some(pairs);
        }
    }
    let pairs = restored.ok_or(RestoreError::NoLedger { segment })?;
    let inside: BTreeSet<Id> = pairs.iter().copied().collect();
    for &(id, body) in &ledgers {
        check_inside(id, body, &inside, &reply_to)?;
    }
    let (first, last) = (pairs[0], pairs[pairs.len() - 1]);
    for k in known.iter().filter(|k| k.session == session) {
        let (Some(&kf), Some(&kl)) = (k.pairs.first(), k.pairs.last()) else {
            continue;
        };
        if kf <= last && first <= kl {
            return Err(RestoreError::Overlap {
                segment: k.id().unwrap_or(segment),
            });
        }
    }
    let text: BTreeMap<Id, &str> = turns
        .iter()
        .map(|t| (t.envelope.id, t.body.text.as_str()))
        .collect();
    let chars = pairs
        .iter()
        .filter_map(|p| text.get(p))
        .fold(0u32, |n, t| n.saturating_add(utterance_chars(t)));
    Ok(SegmentBody {
        session,
        pairs,
        closed: true,
        chars,
    })
}

#[cfg(test)]
mod tests;
