//! 話題の区切りの決め方 (04 §2 の「話題の区切り」、v0.5)。I/O と LLM を使わない純粋な関数。
//!
//! セッションの往復を、古い順に先頭から詰めて区切る。大きさはモデルに依らない見積もり (往復の数と、前のふるい
//! (04 §5.2) の後の利用者の発話の文字数) で決め、応答の本文は見ない。記録した閉じた区切りは変えず、どの区切りにも
//! 入っていない往復だけを詰める。書くのは呼び手 ([`crate::Writes::segment_request`]) で、ここは計画だけを返す。

use std::collections::{BTreeMap, BTreeSet};

use evorag_types::{Id, Timestamp};

use crate::kinds::{TurnRole, TurnState};
use crate::ledger::Through;
use crate::prefilter::{MODE_CHAT, Screened, prefilter};
use crate::read::Turn;
use crate::segment::{SegmentBody, segment_id};

/// 1 つの区切りの往復の上限 (04 §2。S-17 の 4 で決めた)。
pub const SEGMENT_MAX_PAIRS: u32 = 8;
/// 1 つの区切りの大きさの見積もりの上限 (前のふるいの後の利用者の発話の文字数、04 §2。S-17 の 4 で決めた)。
pub const SEGMENT_MAX_CHARS: u32 = 1_600;
/// セッションの最後のターンからこの長さ (ミリ秒) が過ぎたら、末尾の区切りを閉じる (04 §2)。
pub const SEGMENT_CLOSE_AFTER_MS: i64 = 10 * 60 * 1000;
/// 開いている末尾の区切りを呼び直す、前の呼び出しからの往復の増え (04 §2 の「末尾の区切りを呼ぶ時機」)。
pub const TAIL_CALL_PAIRS: u32 = 2;

/// 計画の中の区切りが、記録した区切りからどう変わるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentChange {
    /// 記録したまま (書かない)。
    Kept,
    /// 記録した開いている区切りに往復を足したか、閉じた (前の記録を渡して書く)。
    Updated,
    /// 新しい区切り (前の記録は無い)。
    New,
}

/// 計画の中の往復。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedPair {
    /// 利用者の発話のターン。
    pub utterance: Id,
    /// 組にする応答: その発話を `reply_to` に持つ、最新 (ID の最も大きい) の完了した応答。無ければ `None`
    /// (発話だけの往復)。呼び出しの時点のものなので、区切りの境界には使わない。
    pub reply: Option<Id>,
}

/// 計画の中の 1 つの区切り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSegment {
    /// 区切りの ID ([`segment_id`])。
    pub id: Id,
    pub body: SegmentBody,
    pub change: SegmentChange,
    /// `body.pairs` と同じ順の往復と、組にする応答。
    pub pairs: Vec<PlannedPair>,
    /// 記録した往復のうち、渡したターンに発話が無いもの (読めない・消えた)。区切りは変えない。
    pub missing: Vec<Id>,
}

/// セッションの区切りの計画 ([`plan_segments`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentPlan {
    /// 区切り。最初の往復の発話のターンの ID の順 (04 §2)。
    pub segments: Vec<PlannedSegment>,
}

impl SegmentPlan {
    /// 開いている末尾の区切り (無ければ `None`)。
    #[must_use]
    pub fn tail(&self) -> Option<&PlannedSegment> {
        self.segments.last().filter(|s| !s.body.closed)
    }

    /// 書く区切り ([`SegmentChange::Kept`] でないもの)。閉じた区切りは `Immediate` で書く (03 §3.4)。
    pub fn writes(&self) -> impl Iterator<Item = &PlannedSegment> {
        self.segments
            .iter()
            .filter(|s| s.change != SegmentChange::Kept)
    }
}

/// 記録した区切りが計画の前提に合わない理由。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// 往復の無い区切り。
    #[error("a recorded segment has no pairs")]
    NoPairs,
    /// ほかのセッションの区切り。
    #[error("segment {segment:?} belongs to another session")]
    OtherSession { segment: Id },
    /// 往復が古い順に並んでいない (同じ往復が 2 度あるものを含む)。
    #[error("the pairs of segment {segment:?} are not in ascending order")]
    Unordered { segment: Id },
    /// 2 つの区切りの往復の範囲が重なる。
    #[error("segment {segment:?} overlaps the previous segment")]
    Overlap { segment: Id },
}

/// 区切りに詰める発話。
#[derive(Debug, Clone, Copy)]
struct Utterance {
    id: Id,
    chars: u32,
}

/// 作っている区切り。
#[derive(Debug)]
struct Building {
    pairs: Vec<Id>,
    chars: u32,
    /// 記録した開いている区切りから始めたか (`Some(変えたか)`)。
    known: Option<bool>,
}

impl Building {
    fn new(u: Utterance) -> Self {
        Self {
            pairs: vec![u.id],
            chars: u.chars,
            known: None,
        }
    }

    fn reached(&self) -> bool {
        self.pairs.len() >= SEGMENT_MAX_PAIRS as usize || self.chars >= SEGMENT_MAX_CHARS
    }

    /// 往復を足しても上限を超えないか (上限に達した区切りには足さない)。
    fn fits(&self, u: Utterance) -> bool {
        !self.reached() && self.chars.saturating_add(u.chars) <= SEGMENT_MAX_CHARS
    }

    fn push(&mut self, u: Utterance) {
        self.pairs.push(u.id);
        self.chars = self.chars.saturating_add(u.chars);
        if let Some(changed) = &mut self.known {
            *changed = true;
        }
    }

    fn finish(self, session: Id, closed: bool) -> (SegmentBody, SegmentChange) {
        let change = match self.known {
            None => SegmentChange::New,
            Some(changed) if changed || closed => SegmentChange::Updated,
            Some(_) => SegmentChange::Kept,
        };
        let body = SegmentBody {
            session,
            pairs: self.pairs,
            closed,
            chars: self.chars,
        };
        (body, change)
    }
}

/// 区切りを順に組み立てる。
struct Packer {
    session: Id,
    done: Vec<(SegmentBody, SegmentChange)>,
}

impl Packer {
    /// `cur` に往復を足す。収まらなければ `cur` を閉じて新しい区切りを始める。上限に達したら閉じる。
    fn push(&mut self, cur: &mut Option<Building>, u: Utterance) {
        match cur {
            Some(b) if b.fits(u) => b.push(u),
            _ => {
                self.close(cur);
                *cur = Some(Building::new(u));
            }
        }
        if cur.as_ref().is_some_and(Building::reached) {
            self.close(cur);
        }
    }

    fn close(&mut self, cur: &mut Option<Building>) {
        if let Some(b) = cur.take() {
            self.done.push(b.finish(self.session, true));
        }
    }
}

/// 前のふるいの後の発話の文字数 (Unicode のスカラー値の数)。送らない発話は 0 (04 §2)。
pub(crate) fn utterance_chars(stored: &str) -> u32 {
    match prefilter(MODE_CHAT, stored) {
        Screened::Send(p) => u32::try_from(p.text().chars().count()).unwrap_or(u32::MAX),
        Screened::Skip(_) => 0,
    }
}

/// 記録した区切りを、最初の往復の順に並べて確かめる。
fn check_known(session: Id, known: &[SegmentBody]) -> Result<Vec<&SegmentBody>, PlanError> {
    let mut sorted: Vec<&SegmentBody> = Vec::with_capacity(known.len());
    for k in known {
        let id = k.id().ok_or(PlanError::NoPairs)?;
        if k.session != session {
            return Err(PlanError::OtherSession { segment: id });
        }
        if k.pairs.windows(2).any(|w| w[0] >= w[1]) {
            return Err(PlanError::Unordered { segment: id });
        }
        sorted.push(k);
    }
    sorted.sort_by_key(|k| k.pairs[0]);
    for w in sorted.windows(2) {
        let (prev, next) = (w[0], w[1]);
        if prev.pairs[prev.pairs.len() - 1] >= next.pairs[0] {
            return Err(PlanError::Overlap {
                segment: segment_id(next.pairs[0]),
            });
        }
    }
    Ok(sorted)
}

/// セッションの区切りを決める (04 §2 の「話題の区切り」)。決定論で、モデル・変種・応答の本文に依らない。
///
/// - `mode` が `chat` でないセッションは区切らない (空の計画を返し、記録した区切りにも触れない)。
/// - `turns` はセッションのターン (順は問わない。ほかのセッションのターンは無視する)。往復は利用者の発話ごとに
///   1 つで、発話のターンの ID の順に並べる。
/// - `known` は記録した区切り (閉じた区切りと、あれば開いている末尾の区切り)。閉じた区切りは変えない
///   ([`SegmentChange::Kept`])。開いている区切りは、往復を足すか閉じるときだけ [`SegmentChange::Updated`] にする。
///   記録した往復と大きさの見積もり (`chars`) は決め直さない。
/// - どの区切りにも入っていない往復を、古い順に先頭から詰める。往復の数が [`SEGMENT_MAX_PAIRS`] に達するか、
///   見積もりが [`SEGMENT_MAX_CHARS`] に達したら閉じる。足すと上限を超える往復は、前の区切りを閉じて新しい区切りを
///   始める。1 往復は必ず入る。
/// - 後ろに区切りがある区切り (記録した区切りの前や間に詰めたもの) は閉じる。末尾の区切りは、上限に達したときと、
///   `now` がセッションの最後のターンの時刻 (ターンの作成の時刻の最大) から [`SEGMENT_CLOSE_AFTER_MS`] 以上
///   後のときに閉じる。
///
/// # Errors
/// 記録した区切りに往復が無い・ほかのセッションのもの・往復が古い順でない・2 つの区切りの範囲が重なるとき。
pub fn plan_segments(
    session: Id,
    mode: &str,
    turns: &[Turn],
    known: &[SegmentBody],
    now: Timestamp,
) -> Result<SegmentPlan, PlanError> {
    if mode != MODE_CHAT {
        return Ok(SegmentPlan::default());
    }
    let known = check_known(session, known)?;
    let turns: Vec<&Turn> = turns.iter().filter(|t| t.body.session == session).collect();

    let mut utterances: BTreeMap<Id, Utterance> = BTreeMap::new();
    for t in &turns {
        if t.body.role == TurnRole::User {
            let id = t.envelope.id;
            utterances.insert(
                id,
                Utterance {
                    id,
                    chars: utterance_chars(&t.body.text),
                },
            );
        }
    }
    let mut replies: BTreeMap<Id, Id> = BTreeMap::new();
    for t in &turns {
        if t.body.role == TurnRole::Assistant
            && t.body.state == TurnState::Completed
            && let Some(u) = t.body.reply_to
            && utterances.contains_key(&u)
        {
            let r = replies.entry(u).or_insert(t.envelope.id);
            *r = (*r).max(t.envelope.id);
        }
    }
    let last_at = turns
        .iter()
        .map(|t| t.envelope.created_at.as_millis())
        .max();

    let assigned: BTreeSet<Id> = known.iter().flat_map(|k| k.pairs.iter().copied()).collect();
    let free: Vec<Utterance> = utterances
        .values()
        .filter(|u| !assigned.contains(&u.id))
        .copied()
        .collect();

    let mut packer = Packer {
        session,
        done: Vec::new(),
    };
    let mut cur: Option<Building> = None;
    let mut free = free.into_iter().peekable();
    for k in known {
        let (first, last) = (k.pairs[0], k.pairs[k.pairs.len() - 1]);
        while let Some(u) = free.next_if(|u| u.id < first) {
            packer.push(&mut cur, u);
        }
        // 後ろに区切りがあるので閉じる。
        packer.close(&mut cur);
        // 区切りの往復の間にある、どの区切りにも入っていない往復は、別の閉じた区切りに詰める。
        let mut inside: Option<Building> = None;
        while let Some(u) = free.next_if(|u| u.id < last) {
            packer.push(&mut inside, u);
        }
        packer.close(&mut inside);
        if k.closed {
            packer.done.push((k.clone(), SegmentChange::Kept));
        } else {
            cur = Some(Building {
                pairs: k.pairs.clone(),
                chars: k.chars,
                known: Some(false),
            });
        }
    }
    for u in free {
        packer.push(&mut cur, u);
    }
    if let Some(b) = cur.take() {
        let quiet =
            last_at.is_some_and(|at| now.as_millis() >= at.saturating_add(SEGMENT_CLOSE_AFTER_MS));
        let closed = b.reached() || quiet;
        packer.done.push(b.finish(session, closed));
    }

    let mut done = packer.done;
    done.sort_by_key(|(b, _)| b.pairs[0]);
    let segments = done
        .into_iter()
        .map(|(body, change)| {
            let pairs = body
                .pairs
                .iter()
                .map(|&u| PlannedPair {
                    utterance: u,
                    reply: replies.get(&u).copied(),
                })
                .collect();
            let missing = body
                .pairs
                .iter()
                .filter(|u| !utterances.contains_key(u))
                .copied()
                .collect();
            PlannedSegment {
                id: segment_id(body.pairs[0]),
                body,
                change,
                pairs,
                missing,
            }
        })
        .collect();
    Ok(SegmentPlan { segments })
}

/// 今の選択の台帳がある区切りを、往復の増えと閉じたことで呼び直すか (04 §2 の「末尾の区切りを呼ぶ時機」。
/// 04 §5.2.1 の `needs_call` の 2 つ目)。`through` はその台帳の `inputs` の `through`。
///
/// 開いている区切りは、台帳が入れた往復から [`TAIL_CALL_PAIRS`] 以上増えたときに呼ぶ。閉じた区切りは、台帳が
/// すべての往復を入れていなければ呼ぶ (閉じたときの呼び出し)。`through` の往復が区切りに無いか、数が合わないとき
/// は、台帳が今の往復に合わないので呼ぶ。台帳の無い区切りと、`partial`・`failed` の台帳の判定は `needs_call` の
/// ほかの条件で行う。
#[must_use]
pub fn tail_call_due(segment: &SegmentBody, through: &Through) -> bool {
    let covered = segment
        .pairs
        .iter()
        .position(|&p| p == through.turn)
        .map(|i| i + 1)
        .filter(|&n| u32::try_from(n).is_ok_and(|n| n == through.pairs));
    let Some(covered) = covered else {
        return true;
    };
    let added = segment.pairs.len() - covered;
    if segment.closed {
        added > 0
    } else {
        added >= TAIL_CALL_PAIRS as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::TurnBody;
    use evorag_store::Envelope;

    const S: Id = Id::from_bytes([0xEE; 16]);
    const T0: i64 = 1_000_000;

    fn id(n: u16) -> Id {
        let mut b = [0u8; 16];
        b[14..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(b)
    }

    fn turn(
        n: u16,
        role: TurnRole,
        state: TurnState,
        reply_to: Option<u16>,
        text: &str,
        at: i64,
    ) -> Turn {
        let body = TurnBody {
            session: S,
            role,
            text: text.into(),
            state,
            model_key: None,
            generation: None,
            reply_to: reply_to.map(id),
            attached: Vec::new(),
            error: None,
            recall: None,
            obs: None,
        };
        let mut envelope = Envelope::new(id(n), "turn", 1, Timestamp::from_millis(at), "test");
        envelope.set_body(&body).expect("turn body encodes");
        Turn { envelope, body }
    }

    fn user(n: u16, text: &str, at: i64) -> Turn {
        turn(n, TurnRole::User, TurnState::Confirmed, None, text, at)
    }

    fn reply(n: u16, to: u16, state: TurnState, text: &str, at: i64) -> Turn {
        turn(n, TurnRole::Assistant, state, Some(to), text, at)
    }

    /// `n` 個の往復 (発話は 2i+1、完了した応答は 2i+2)。発話はどれも `text`。
    fn exchanges(n: u16, text: &str, at: i64) -> Vec<Turn> {
        (0..n)
            .flat_map(|i| {
                [
                    user(2 * i + 1, text, at),
                    reply(2 * i + 2, 2 * i + 1, TurnState::Completed, "応答です", at),
                ]
            })
            .collect()
    }

    /// 前のふるいで送る、`n` 文字の発話 (漢字だけ)。
    fn kanji(n: usize) -> String {
        "東".repeat(n)
    }

    fn plan(turns: &[Turn], known: &[SegmentBody], now: i64) -> SegmentPlan {
        plan_segments(S, MODE_CHAT, turns, known, Timestamp::from_millis(now)).expect("plans")
    }

    fn pairs_of(p: &SegmentPlan) -> Vec<Vec<Id>> {
        p.segments.iter().map(|s| s.body.pairs.clone()).collect()
    }

    fn bodies(p: &SegmentPlan) -> Vec<SegmentBody> {
        p.segments.iter().map(|s| s.body.clone()).collect()
    }

    #[test]
    fn utterance_chars_follow_the_prefilter() {
        assert_eq!(utterance_chars(&kanji(10)), 10);
        // 挨拶・相槌・短い発話は数えない。
        assert_eq!(utterance_chars("ありがとう！"), 0);
        assert_eq!(utterance_chars("はい"), 0);
        // コードブロックと引用の行は除いて数える。
        assert_eq!(
            utterance_chars("```\nlet x = 1;\n```\n東京に住んでいます"),
            9
        );
        assert_eq!(utterance_chars("> 引用です\n東京に住んでいます"), 9);
    }

    #[test]
    fn pairs_are_packed_by_count_and_the_tail_stays_open() {
        let turns = exchanges(10, "東京に住んでいます", T0);
        let p = plan(&turns, &[], T0);
        assert_eq!(p.segments.len(), 2);
        let first = &p.segments[0];
        assert_eq!(first.body.pairs.len(), 8);
        assert!(first.body.closed);
        assert_eq!(first.body.chars, 8 * 9);
        assert_eq!(first.id, segment_id(id(1)));
        assert_eq!(first.change, SegmentChange::New);
        let tail = p.tail().expect("open tail");
        assert_eq!(tail.body.pairs, [id(17), id(19)]);
        assert_eq!(tail.id, segment_id(id(17)));
        assert_eq!(tail.pairs[0].reply, Some(id(18)));
        assert!(tail.missing.is_empty());
        assert_eq!(p.writes().count(), 2);
    }

    #[test]
    fn exactly_eight_pairs_close_the_segment() {
        let turns = exchanges(8, "東京に住んでいます", T0);
        let p = plan(&turns, &[], T0);
        assert_eq!(p.segments.len(), 1);
        assert!(p.segments[0].body.closed);
        assert!(p.tail().is_none());
    }

    #[test]
    fn chars_at_the_limit_close_and_over_the_limit_start_a_new_segment() {
        // 800 + 800 = 1,600 文字ちょうどで閉じる。後の挨拶 (0 文字) は新しい区切りに入る。
        let turns = vec![
            user(1, &kanji(800), T0),
            user(3, &kanji(800), T0),
            user(5, "ありがとう", T0),
        ];
        let p = plan(&turns, &[], T0);
        assert_eq!(pairs_of(&p), [vec![id(1), id(3)], vec![id(5)]]);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].body.chars, 1_600);
        assert_eq!(p.segments[1].body.chars, 0);

        // 1,599 文字に 0 文字の往復は入る。2 文字の往復は入らない。
        let turns = vec![
            user(1, &kanji(1_599), T0),
            user(3, "はい", T0),
            user(5, &kanji(5), T0),
        ];
        let p = plan(&turns, &[], T0);
        assert_eq!(pairs_of(&p), [vec![id(1), id(3)], vec![id(5)]]);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].body.chars, 1_599);
        assert!(!p.segments[1].body.closed);
    }

    #[test]
    fn a_single_huge_utterance_always_fits() {
        let turns = vec![
            user(1, &kanji(10), T0),
            user(3, &kanji(5_000), T0),
            user(5, &kanji(10), T0),
        ];
        let p = plan(&turns, &[], T0);
        assert_eq!(pairs_of(&p), [vec![id(1)], vec![id(3)], vec![id(5)]]);
        assert_eq!(p.segments[1].body.chars, 5_000);
        assert!(p.segments[0].body.closed && p.segments[1].body.closed);
        assert!(!p.segments[2].body.closed);
    }

    #[test]
    fn skipped_utterances_count_as_pairs_but_not_chars() {
        let mut turns: Vec<Turn> = (0..7).map(|i| user(2 * i + 1, "ありがとう", T0)).collect();
        turns.push(user(15, &kanji(1_600), T0));
        let p = plan(&turns, &[], T0);
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.segments[0].body.pairs.len(), 8);
        assert_eq!(p.segments[0].body.chars, 1_600);
        assert!(p.segments[0].body.closed);
    }

    #[test]
    fn the_tail_closes_ten_minutes_after_the_last_turn() {
        let mut turns = exchanges(2, "東京に住んでいます", T0);
        // 最後のターン (応答) の時刻から数える。
        turns.push(reply(5, 3, TurnState::Completed, "再生成", T0 + 1_000));
        let last = T0 + 1_000;
        let open = plan(&turns, &[], last + SEGMENT_CLOSE_AFTER_MS - 1);
        assert!(!open.segments[0].body.closed);
        let closed = plan(&turns, &[], last + SEGMENT_CLOSE_AFTER_MS);
        assert!(closed.segments[0].body.closed);
        assert_eq!(bodies(&open)[0].pairs, bodies(&closed)[0].pairs);
    }

    #[test]
    fn a_clock_going_backwards_does_not_close_early() {
        // 後のターンの時刻が前のターンより古い (時計が戻った)。最後のターンの時刻は作成の時刻の最大で見る。
        let turns = vec![
            user(1, "東京に住んでいます", T0 + SEGMENT_CLOSE_AFTER_MS),
            user(3, "大阪に住んでいます", T0),
        ];
        let p = plan(&turns, &[], T0 + SEGMENT_CLOSE_AFTER_MS + 1);
        assert!(!p.segments[0].body.closed);
        // 今が最後のターンより前でも閉じない。
        let p = plan(&turns, &[], 0);
        assert!(!p.segments[0].body.closed);
        assert_eq!(p.segments[0].body.pairs, [id(1), id(3)]);
    }

    #[test]
    fn the_latest_completed_reply_is_the_pair() {
        let turns = vec![
            user(1, "東京に住んでいます", T0),
            reply(2, 1, TurnState::Completed, "一つ目", T0),
            reply(3, 1, TurnState::Completed, "再生成", T0),
            reply(4, 1, TurnState::Failed, "失敗", T0),
            user(5, "大阪に住んでいます", T0),
            reply(6, 5, TurnState::Interrupted, "中断", T0),
            user(7, "京都に住んでいます", T0),
            reply(8, 7, TurnState::Failed, "失敗", T0),
            user(9, "名古屋に住んでいます", T0),
        ];
        let p = plan(&turns, &[], T0);
        let got: Vec<Option<Id>> = p.segments[0].pairs.iter().map(|x| x.reply).collect();
        assert_eq!(got, [Some(id(3)), None, None, None]);
        assert_eq!(p.segments[0].body.pairs.len(), 4);
    }

    #[test]
    fn non_chat_sessions_are_not_segmented() {
        let turns = exchanges(3, "東京に住んでいます", T0);
        let p = plan_segments(S, "coding", &turns, &[], Timestamp::from_millis(T0)).expect("plans");
        assert_eq!(p, SegmentPlan::default());
    }

    #[test]
    fn turns_of_other_sessions_and_system_turns_are_ignored() {
        let mut turns = exchanges(1, "東京に住んでいます", T0);
        let mut other = user(9, "大阪に住んでいます", T0 + SEGMENT_CLOSE_AFTER_MS);
        other.body.session = Id::from_bytes([1; 16]);
        turns.push(other);
        turns.push(turn(
            11,
            TurnRole::System,
            TurnState::Completed,
            None,
            "sys",
            T0,
        ));
        let p = plan(&turns, &[], T0 + SEGMENT_CLOSE_AFTER_MS);
        assert_eq!(pairs_of(&p), [vec![id(1)]]);
        assert!(p.segments[0].body.closed);
    }

    #[test]
    fn a_pair_after_a_closed_segment_starts_a_new_one() {
        let turns = exchanges(2, "東京に住んでいます", T0);
        let first = plan(&turns, &[], T0 + SEGMENT_CLOSE_AFTER_MS);
        assert!(first.segments[0].body.closed);
        let known = bodies(&first);
        let mut turns = turns;
        turns.push(user(
            5,
            "大阪に住んでいます",
            T0 + SEGMENT_CLOSE_AFTER_MS + 1,
        ));
        let p = plan(&turns, &known, T0 + SEGMENT_CLOSE_AFTER_MS + 2);
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].body, known[0]);
        assert_eq!(p.segments[0].change, SegmentChange::Kept);
        assert_eq!(p.segments[1].body.pairs, [id(5)]);
        assert_eq!(p.segments[1].id, segment_id(id(5)));
        assert_eq!(p.segments[1].change, SegmentChange::New);
        assert_eq!(p.writes().count(), 1);
    }

    #[test]
    fn the_recorded_open_tail_is_extended_and_closed() {
        let turns = exchanges(2, "東京に住んでいます", T0);
        let known = bodies(&plan(&turns, &[], T0));
        assert!(!known[0].closed);
        // 何も変わらなければ書かない。
        let same = plan(&turns, &known, T0);
        assert_eq!(same.segments[0].change, SegmentChange::Kept);
        assert_eq!(same.writes().count(), 0);
        // 往復が足されたら、同じ区切り (同じ ID) に足す。
        let mut more = turns.clone();
        more.push(user(5, "大阪に住んでいます", T0));
        let p = plan(&more, &known, T0);
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.segments[0].id, segment_id(id(1)));
        assert_eq!(p.segments[0].change, SegmentChange::Updated);
        assert_eq!(p.segments[0].body.pairs, [id(1), id(3), id(5)]);
        // 時間で閉じるだけでも書く。
        let p = plan(&turns, &known, T0 + SEGMENT_CLOSE_AFTER_MS);
        assert_eq!(p.segments[0].change, SegmentChange::Updated);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].body.pairs, known[0].pairs);
    }

    #[test]
    fn a_recorded_open_segment_at_the_limit_closes() {
        let turns = exchanges(9, "東京に住んでいます", T0);
        let known = SegmentBody {
            session: S,
            pairs: (0..8).map(|i| id(2 * i + 1)).collect(),
            closed: false,
            chars: 72,
        };
        let p = plan(&turns, std::slice::from_ref(&known), T0);
        assert_eq!(p.segments.len(), 2);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].change, SegmentChange::Updated);
        assert_eq!(p.segments[1].body.pairs, [id(17)]);
    }

    #[test]
    fn redaction_does_not_move_recorded_boundaries() {
        // 末尾の区切りの記録の大きさの見積もりは決め直さない (伏せ字で本文が短くなっても)。
        let turns = vec![user(1, &kanji(1_000), T0), user(3, &kanji(500), T0)];
        let known = bodies(&plan(&turns, &[], T0));
        assert_eq!(known[0].chars, 1_500);
        let mut redacted = turns.clone();
        redacted[0].body.text = format!("{}[伏せ字]", kanji(10));
        redacted.push(user(5, &kanji(200), T0));
        let p = plan(&redacted, &known, T0);
        assert_eq!(pairs_of(&p), [vec![id(1), id(3)], vec![id(5)]]);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].body.chars, 1_500);
    }

    #[test]
    fn missing_turns_are_reported_and_the_segment_is_kept() {
        let turns = exchanges(3, "東京に住んでいます", T0);
        let known = bodies(&plan(&turns, &[], T0 + SEGMENT_CLOSE_AFTER_MS));
        // 2 つ目の往復の発話が読めない (消えた)。
        let fewer: Vec<Turn> = turns
            .iter()
            .filter(|t| t.envelope.id != id(3))
            .cloned()
            .collect();
        let p = plan(&fewer, &known, T0 + SEGMENT_CLOSE_AFTER_MS);
        assert_eq!(p.segments[0].body, known[0]);
        assert_eq!(p.segments[0].change, SegmentChange::Kept);
        assert_eq!(p.segments[0].missing, [id(3)]);
        // 発話の無い往復には応答を組にしない。
        assert_eq!(p.segments[0].pairs[1].reply, None);
        assert_eq!(p.segments[0].pairs[2].reply, Some(id(6)));
    }

    #[test]
    fn free_pairs_between_recorded_segments_are_closed_in_place() {
        let turns = exchanges(6, "東京に住んでいます", T0);
        // 1・3 と 9・11 を記録した。5・7 (間) はどの区切りにも無い。
        let a = SegmentBody {
            session: S,
            pairs: vec![id(1), id(3)],
            closed: true,
            chars: 18,
        };
        let b = SegmentBody {
            session: S,
            pairs: vec![id(9), id(11)],
            closed: false,
            chars: 18,
        };
        let p = plan(&turns, &[b.clone(), a.clone()], T0);
        assert_eq!(
            pairs_of(&p),
            [vec![id(1), id(3)], vec![id(5), id(7)], vec![id(9), id(11)]]
        );
        assert!(p.segments[1].body.closed);
        assert_eq!(p.segments[1].change, SegmentChange::New);
        assert_eq!(p.segments[2].change, SegmentChange::Kept);
        assert!(!p.segments[2].body.closed);

        // 記録した区切りの往復の間に現れた往復 (読めなかったターンが読めるようになった) は、別の閉じた区切りにする。
        let c = SegmentBody {
            session: S,
            pairs: vec![id(1), id(7)],
            closed: false,
            chars: 18,
        };
        let p = plan(&turns[..8], std::slice::from_ref(&c), T0);
        assert_eq!(pairs_of(&p), [vec![id(1), id(7)], vec![id(3), id(5)]]);
        assert!(!p.segments[0].body.closed);
        assert!(p.segments[1].body.closed);
    }

    #[test]
    fn an_open_segment_followed_by_another_closes() {
        let turns = exchanges(3, "東京に住んでいます", T0);
        let a = SegmentBody {
            session: S,
            pairs: vec![id(1)],
            closed: false,
            chars: 9,
        };
        let b = SegmentBody {
            session: S,
            pairs: vec![id(5)],
            closed: false,
            chars: 9,
        };
        let p = plan(&turns, &[a, b], T0);
        // 3 は 1 の区切りに足してから閉じる。
        assert_eq!(pairs_of(&p), [vec![id(1), id(3)], vec![id(5)]]);
        assert!(p.segments[0].body.closed);
        assert_eq!(p.segments[0].change, SegmentChange::Updated);
        assert!(!p.segments[1].body.closed);
    }

    #[test]
    fn inconsistent_records_are_errors() {
        let turns = exchanges(3, "東京に住んでいます", T0);
        let seg = |pairs: Vec<Id>| SegmentBody {
            session: S,
            pairs,
            closed: true,
            chars: 0,
        };
        let run = |known: &[SegmentBody]| {
            plan_segments(S, MODE_CHAT, &turns, known, Timestamp::from_millis(T0))
        };
        assert_eq!(run(&[seg(vec![])]), Err(PlanError::NoPairs));
        let mut other = seg(vec![id(1)]);
        other.session = Id::from_bytes([1; 16]);
        assert_eq!(
            run(&[other]),
            Err(PlanError::OtherSession {
                segment: segment_id(id(1))
            })
        );
        assert_eq!(
            run(&[seg(vec![id(3), id(1)])]),
            Err(PlanError::Unordered {
                segment: segment_id(id(3))
            })
        );
        assert_eq!(
            run(&[seg(vec![id(1), id(1)])]),
            Err(PlanError::Unordered {
                segment: segment_id(id(1))
            })
        );
        assert_eq!(
            run(&[seg(vec![id(1), id(5)]), seg(vec![id(3)])]),
            Err(PlanError::Overlap {
                segment: segment_id(id(3))
            })
        );
    }

    #[test]
    fn the_tail_is_called_every_two_pairs_and_on_close() {
        let seg = |n: u16, closed: bool| SegmentBody {
            session: S,
            pairs: (0..n).map(|i| id(2 * i + 1)).collect(),
            closed,
            chars: 0,
        };
        let through = |n: u16| Through {
            turn: id(2 * n - 1),
            pairs: u32::from(n),
        };
        assert!(!tail_call_due(&seg(3, false), &through(3)));
        assert!(!tail_call_due(&seg(4, false), &through(3)));
        assert!(tail_call_due(&seg(5, false), &through(3)));
        // 閉じたら、入れていない往復があれば呼ぶ。
        assert!(tail_call_due(&seg(4, true), &through(3)));
        assert!(!tail_call_due(&seg(4, true), &through(4)));
        // `through` が区切りに合わなければ呼ぶ。
        assert!(tail_call_due(
            &seg(4, false),
            &Through {
                turn: id(99),
                pairs: 2
            }
        ));
        assert!(tail_call_due(
            &seg(4, false),
            &Through {
                turn: id(3),
                pairs: 3
            }
        ));
    }

    /// 種を決めた擬似乱数 (xorshift64*)。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// 乱数で作るセッション: 発話の長さ・挨拶・応答の有無と状態・再生成・時刻の間隔 (戻ることもある)。
    fn random_session(rng: &mut Rng, n: u16) -> Vec<Turn> {
        let mut turns = Vec::new();
        let mut at = T0;
        let mut next = 1u16;
        for _ in 0..n {
            let step = match rng.below(10) {
                0 => SEGMENT_CLOSE_AFTER_MS + 1,
                1 => -60_000,
                _ => i64::try_from(rng.below(120_000)).expect("small"),
            };
            at += step;
            let text = match rng.below(6) {
                0 => "ありがとう".to_string(),
                1 => "はい".to_string(),
                2 => kanji(usize::try_from(rng.below(2_000)).expect("small")),
                _ => kanji(usize::try_from(rng.below(400) + 4).expect("small")),
            };
            let u = next;
            turns.push(user(u, &text, at));
            next += 1;
            for _ in 0..rng.below(3) {
                let state = match rng.below(4) {
                    0 => TurnState::Failed,
                    1 => TurnState::Interrupted,
                    _ => TurnState::Completed,
                };
                turns.push(reply(next, u, state, "応答", at + 10));
                next += 1;
            }
        }
        turns
    }

    /// 計画を記録したものとして、次の計画に渡す区切り。
    fn recorded(p: &SegmentPlan) -> Vec<SegmentBody> {
        bodies(p)
    }

    #[test]
    fn plans_are_deterministic_and_independent_of_input_order() {
        for seed in 1..=60u64 {
            let mut rng = Rng(seed);
            let turns = random_session(&mut rng, 30);
            let now = T0 + i64::try_from(rng.below(10_000_000)).expect("small");
            let a = plan(&turns, &[], now);
            let b = plan(&turns, &[], now);
            assert_eq!(a, b, "seed {seed}");
            let mut shuffled = turns.clone();
            for i in (1..shuffled.len()).rev() {
                let j = usize::try_from(rng.below(i as u64 + 1)).expect("small");
                shuffled.swap(i, j);
            }
            assert_eq!(plan(&shuffled, &[], now), a, "seed {seed}");
            // 記録した計画から決め直しても、何も書かない (同じ入力で同じ区切り)。
            let again = plan(&turns, &recorded(&a), now);
            assert_eq!(bodies(&again), bodies(&a), "seed {seed}");
            assert_eq!(again.writes().count(), 0, "seed {seed}");
            for s in &a.segments {
                assert!(!s.body.pairs.is_empty());
                assert!(
                    s.body.pairs.len() <= SEGMENT_MAX_PAIRS as usize,
                    "seed {seed}"
                );
                assert!(
                    s.body.pairs.len() == 1 || s.body.chars <= SEGMENT_MAX_CHARS,
                    "seed {seed}"
                );
            }
        }
    }

    #[test]
    fn appending_turns_never_changes_closed_segments() {
        for seed in 1..=60u64 {
            let mut rng = Rng(seed);
            let turns = random_session(&mut rng, 40);
            let mut known: Vec<SegmentBody> = Vec::new();
            let mut cut = 0usize;
            let mut now = T0;
            while cut < turns.len() {
                cut = (cut + 1 + usize::try_from(rng.below(6)).expect("small")).min(turns.len());
                let prefix = &turns[..cut];
                let last = prefix
                    .iter()
                    .map(|t| t.envelope.created_at.as_millis())
                    .max()
                    .expect("turns");
                now = now.max(last) + i64::try_from(rng.below(900_000)).expect("small");
                let p = plan(prefix, &known, now);
                for k in &known {
                    let s = p
                        .segments
                        .iter()
                        .find(|s| s.id == k.id().expect("pairs"))
                        .expect("recorded segment stays");
                    if k.closed {
                        assert_eq!(&s.body, k, "seed {seed}");
                        assert_eq!(s.change, SegmentChange::Kept, "seed {seed}");
                    } else {
                        assert!(s.body.pairs.starts_with(&k.pairs), "seed {seed}");
                    }
                }
                // 往復はどれか 1 つの区切りに入る。
                let all: Vec<Id> = p
                    .segments
                    .iter()
                    .flat_map(|s| s.body.pairs.iter().copied())
                    .collect();
                let users: Vec<Id> = prefix
                    .iter()
                    .filter(|t| t.body.role == TurnRole::User)
                    .map(|t| t.envelope.id)
                    .collect();
                assert_eq!(all, users, "seed {seed}");
                known = recorded(&p);
            }
        }
    }

    #[test]
    fn boundaries_do_not_depend_on_reply_text_or_state() {
        for seed in 1..=60u64 {
            let mut rng = Rng(seed);
            let turns = random_session(&mut rng, 30);
            let now = T0 + 5_000_000;
            let base = plan(&turns, &[], now);
            let mut changed = turns.clone();
            for t in &mut changed {
                if t.body.role == TurnRole::Assistant {
                    t.body.text = kanji(usize::try_from(rng.below(3_000)).expect("small"));
                    t.body.state = match rng.below(3) {
                        0 => TurnState::Failed,
                        1 => TurnState::Interrupted,
                        _ => TurnState::Completed,
                    };
                }
            }
            assert_eq!(
                bodies(&plan(&changed, &[], now)),
                bodies(&base),
                "seed {seed}"
            );
        }
    }
}
