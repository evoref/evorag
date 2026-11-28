//! 記録を読んで、区切りを決め (04 §4 の手順 2)、呼ぶ区切りの B2 の入力を作る (手順 3 の前半)。I/O は読み取りだけ。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use evorag_memory::b2::{self, Text};
use evorag_memory::corrections::{CORRECTION_V1, find_holes};
use evorag_memory::{
    CallDecision, DirectiveBody, DirectiveSet, KIND_DIRECTIVE, KIND_PROPOSAL, KIND_SEGMENT,
    LedgerBody, LedgerRead, MODE_CHAT, RuleVersions, SegmentBody, SegmentChange, SelectEnv,
    Selection, StoredLedger, Through, Turn, TurnRole, needs_call, plan_segments, read_ledger,
    restore_segment, turn_rev_checked,
};
use evorag_store::{Envelope, Read, ReadTxn, Store};
use evorag_types::{Id, PromptLanguage, Timestamp};

use super::Target;

/// ターンの本文 (B2 に渡す形。`[非表示]` の範囲は今の本文の位置)。
#[derive(Debug, Clone)]
pub(super) struct OwnedText {
    pub(super) turn: Id,
    pub(super) rev: u32,
    pub(super) text: String,
    pub(super) hidden: Vec<Range<usize>>,
}

impl OwnedText {
    pub(super) fn view(&self) -> Text<'_> {
        Text {
            turn: self.turn,
            rev: self.rev,
            text: &self.text,
            hidden: &self.hidden,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct OwnedPair {
    pub(super) utterance: OwnedText,
    pub(super) reply: Option<OwnedText>,
}

#[derive(Debug, Clone)]
pub(super) struct OwnedCandidate {
    pub(super) turn: Id,
    pub(super) pattern: &'static str,
    pub(super) new: Range<usize>,
    pub(super) old: Option<Range<usize>>,
    pub(super) previous: Option<OwnedText>,
}

/// 呼ぶ区切り 1 つ。
#[derive(Debug, Clone)]
pub(super) struct Work {
    pub(super) id: Id,
    pub(super) body: SegmentBody,
    /// 渡せる往復 (発話が読めて、忘れるの指示と合うもの)。
    pub(super) pairs: Vec<OwnedPair>,
    pub(super) candidates: Vec<OwnedCandidate>,
}

impl Work {
    /// 台帳の `through`: 区切りの最後の往復と往復の数 (読めない往復を含む)。渡した往復で数えると、読めない往復の
    /// ある区切りは `through` が区切りに合わず、呼び直し続けるため (04 §5.2.1 の「判定の細目」)。
    pub(super) fn through(&self) -> Option<Through> {
        Some(Through {
            turn: *self.body.pairs.last()?,
            pairs: u32::try_from(self.body.pairs.len()).ok()?,
        })
    }
}

/// 手順 2 で読んで決めたもの。
pub(super) struct Snapshot {
    /// 書く区切り (本体と、読んだ前の記録)。最初の往復の順。
    pub(super) writes: Vec<(SegmentBody, Option<Envelope>)>,
    /// 呼ぶ区切り (最初の往復の順、セッションをまたぐ 1 本の列)。
    pub(super) work: Vec<Work>,
    pub(super) due_at: Option<i64>,
    /// 入力を読んだときに見えていた最新の忘れるの指示。
    pub(super) forgets_seen: Option<Id>,
}

/// 効いている指示を読む。読めない指示があれば `None` (忘れるの範囲を確かめられないので呼ばない)。
pub(super) fn load_directives(r: &ReadTxn<'_>, lang: PromptLanguage) -> Option<DirectiveSet> {
    let mut bodies: Vec<(Id, DirectiveBody)> = Vec::new();
    let mut unreadable = 0usize;
    r.for_each(KIND_DIRECTIVE, |x| {
        match x {
            Read::Ok(e) => match e.body_as::<DirectiveBody>() {
                Ok(b) => bodies.push((e.id, b)),
                Err(_) => unreadable += 1,
            },
            _ => unreadable += 1,
        }
        true
    })
    .ok()?;
    if unreadable > 0 {
        tracing::warn!(
            unreadable,
            "directives could not be read; segment calls wait (forgotten ranges are not known)"
        );
        return None;
    }
    // 呼び名の解き方は、伏せる範囲と掃除には効かない (entity を見ない) ので、そのまま比べる。
    Some(DirectiveSet::new(
        bodies,
        evorag_memory::redaction_marker(lang.as_str()),
        str::to_owned,
    ))
}

/// 台帳を読む (区切りの呼び出しの台帳だけ。区切りの ID ごと)。型として読めない台帳は有効でない台帳なので
/// 選び方に渡さない (消さない)。
pub(super) fn load_ledgers(r: &ReadTxn<'_>) -> Option<HashMap<Id, Vec<(Id, LedgerBody)>>> {
    let mut out: HashMap<Id, Vec<(Id, LedgerBody)>> = HashMap::new();
    r.for_each(KIND_PROPOSAL, |x| {
        if let Read::Ok(e) = x
            && let Some(LedgerRead::Segment(body)) = read_ledger(&e)
            && let Some(seg) = body.segment
        {
            out.entry(seg).or_default().push((e.id, *body));
        }
        true
    })
    .ok()?;
    Some(out)
}

/// 記録した区切り (封筒と本体)。台帳から戻した区切りは、封筒が無い (まだ書いていない)。
type Known = (Option<Envelope>, SegmentBody);

/// 記録した区切り (セッションごと、封筒と本体)。
fn load_segments(r: &ReadTxn<'_>) -> Option<HashMap<Id, Vec<Known>>> {
    let mut out: HashMap<Id, Vec<Known>> = HashMap::new();
    r.for_each(KIND_SEGMENT, |x| {
        if let Read::Ok(e) = x
            && let Ok(b) = e.body_as::<SegmentBody>()
        {
            out.entry(b.session).or_default().push((Some(e), b));
        }
        true
    })
    .ok()?;
    Some(out)
}

/// ターンを B2 に渡す形にする。忘れるの指示と合わない (印の範囲を求められない) ターンと、改訂の番号を読めない
/// ターンは渡さない (`None`)。
pub(super) fn owned_text(directives: &DirectiveSet, t: &Turn) -> Option<OwnedText> {
    let rev = turn_rev_checked(&t.envelope)?;
    let hidden = directives
        .redactions()
        .hidden_ranges(t.envelope.id, &t.body.text, rev)?;
    Some(OwnedText {
        turn: t.envelope.id,
        rev,
        text: t.body.text.clone(),
        hidden,
    })
}

/// 呼ぶかの判定に要る値 (呼ぶ先が決まっているときだけ)。
pub(super) struct Judge<'a> {
    pub(super) target: &'a Target,
    pub(super) previous: Option<String>,
    pub(super) unsent: &'a HashMap<Id, usize>,
}

/// 記録を読んで、区切りを決め (手順 2)、呼ぶ区切りの入力を作る (手順 3 の前半)。
pub(super) fn read(
    store: &Store,
    lang: PromptLanguage,
    now_ms: i64,
    judge: Option<&Judge<'_>>,
) -> Option<Snapshot> {
    let r = store.read().ok()?;
    let directives = load_directives(&r, lang)?;
    let forgets_seen = directives.forgets().iter().map(|f| f.id()).max();
    let forgotten: BTreeSet<Id> = directives
        .forgets()
        .iter()
        .flat_map(|f| f.targets().iter().copied())
        .collect();
    let ledgers = load_ledgers(&r)?;
    let mut segments = load_segments(&r)?;
    let known_ids: BTreeSet<Id> = segments
        .values()
        .flatten()
        .filter_map(|(_, b)| b.id())
        .collect();
    let previous = judge.and_then(|j| j.previous.clone());

    let mut snap = Snapshot {
        writes: Vec::new(),
        work: Vec::new(),
        due_at: None,
        forgets_seen,
    };
    // 区切りと、そのセッションのターン (`session_turns` の位置)。
    let mut planned: Vec<(SegmentBody, usize)> = Vec::new();
    let mut session_turns: Vec<Vec<Turn>> = Vec::new();
    let mut sessions = evorag_memory::list_sessions(&r, usize::MAX).ok()?;
    sessions.sort_by_key(|s| s.envelope.id);
    for s in sessions {
        if s.body.mode != MODE_CHAT {
            continue;
        }
        let sid = s.envelope.id;
        let turns = evorag_memory::session_turns(&r, sid).ok()?;
        let mut known: Vec<Known> = segments.remove(&sid).unwrap_or_default();
        restore(sid, &turns, &ledgers, &known_ids, &mut known, &mut snap);
        let bodies: Vec<SegmentBody> = known.iter().map(|(_, b)| b.clone()).collect();
        let plan = match plan_segments(
            sid,
            &s.body.mode,
            &turns,
            &bodies,
            Timestamp::from_millis(now_ms),
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "the recorded segments of a session do not fit; it is skipped");
                continue;
            }
        };
        if plan.tail().is_some()
            && let Some(last) = turns
                .iter()
                .map(|t| t.envelope.created_at.as_millis())
                .max()
        {
            let at = last.saturating_add(evorag_memory::SEGMENT_CLOSE_AFTER_MS);
            snap.due_at = Some(snap.due_at.map_or(at, |d| d.min(at)));
        }
        for seg in plan.segments {
            if seg.change != SegmentChange::Kept {
                let previous = known
                    .iter()
                    .find_map(|(e, _)| e.as_ref().filter(|e| e.id == seg.id))
                    .cloned();
                snap.writes.push((seg.body.clone(), previous));
            }
            planned.push((seg.body, session_turns.len()));
        }
        session_turns.push(turns);
    }
    snap.writes.sort_by_key(|(b, _)| b.pairs.first().copied());
    let Some(judge) = judge else {
        return Some(snap);
    };

    // 呼ぶかの判定 (04 §5.2.1 の `needs_call`)。ターンの今の改訂の番号は、読んだすべてのセッションのターンから。
    let mut revs: HashMap<Id, u32> = HashMap::new();
    for turns in &session_turns {
        for t in turns {
            if let Some(rev) = turn_rev_checked(&t.envelope) {
                revs.insert(t.envelope.id, rev);
            }
        }
    }
    let env = SelectEnv {
        current: judge.target.current(),
        build_info: &judge.target.build_info,
        previous: previous.as_deref().map(|model_key| Selection {
            variant: b2::VARIANT,
            model_key,
        }),
        versions: RuleVersions::current(CORRECTION_V1),
        turn_rev: |id: Id| revs.get(&id).copied(),
    };
    planned.sort_by_key(|(b, _)| b.pairs.first().copied());
    for (body, at) in &planned {
        let turns = &session_turns[*at];
        let Some(id) = body.id() else { continue };
        if judge.unsent.get(&id) == Some(&body.pairs.len()) {
            continue;
        }
        let stored: Vec<StoredLedger<'_>> = ledgers
            .get(&id)
            .map(|v| {
                v.iter()
                    .map(|(lid, b)| StoredLedger { id: *lid, body: b })
                    .collect()
            })
            .unwrap_or_default();
        let decision = needs_call(body, &stored, &env);
        if let CallDecision::Call(reason) = decision {
            tracing::debug!(?reason, pairs = body.pairs.len(), "a segment is called");
            snap.work
                .push(work_of(id, body, turns, &directives, &forgotten));
        }
    }
    Some(snap)
}

/// 記録を失った区切りを、その区切りの台帳から戻す (04 §2 の「台帳から戻す細目」)。戻した区切りは閉じた区切りと
/// して書く。戻せない区切りの往復は、区切り直す (台帳は消さない)。
fn restore(
    session: Id,
    turns: &[Turn],
    ledgers: &HashMap<Id, Vec<(Id, LedgerBody)>>,
    known_ids: &BTreeSet<Id>,
    known: &mut Vec<Known>,
    snap: &mut Snapshot,
) {
    let mine: BTreeSet<Id> = turns
        .iter()
        .filter(|t| t.body.role == TurnRole::User)
        .map(|t| t.envelope.id)
        .collect();
    // 記録の無い区切りの台帳のうち、`through` の発話がこのセッションにあるもの (最初の往復の順は分からないので、
    // `through` の発話の順に戻す)。
    let mut lost: Vec<(Id, Id)> = ledgers
        .iter()
        .filter(|(seg, _)| !known_ids.contains(seg))
        .filter_map(|(seg, v)| {
            let last = v
                .iter()
                .filter_map(|(_, b)| b.inputs.through.map(|t| t.turn))
                .find(|t| mine.contains(t))?;
            Some((last, *seg))
        })
        .collect();
    lost.sort_unstable();
    let mut restored: Vec<SegmentBody> = Vec::new();
    for (_, seg) in lost {
        let stored: Vec<StoredLedger<'_>> = ledgers[&seg]
            .iter()
            .map(|(id, body)| StoredLedger { id: *id, body })
            .collect();
        let bodies: Vec<SegmentBody> = known
            .iter()
            .map(|(_, b)| b.clone())
            .chain(restored.iter().cloned())
            .collect();
        match restore_segment(seg, session, turns, &stored, &bodies) {
            Ok(body) => {
                tracing::info!(
                    pairs = body.pairs.len(),
                    "a lost segment was restored from its ledger"
                );
                restored.push(body);
            }
            Err(e) => tracing::info!(error = %e, "a lost segment is segmented again"),
        }
    }
    for body in restored {
        // 戻した区切りは、記録した区切りとして区切りの前提にする (前の記録は無いので、閉じた新しい区切りとして書く)。
        snap.writes.push((body.clone(), None));
        known.push((None, body));
    }
}

/// 区切り 1 つの B2 の入力 (往復の本文と訂正の候補)。
fn work_of(
    id: Id,
    body: &SegmentBody,
    turns: &[Turn],
    directives: &DirectiveSet,
    forgotten: &BTreeSet<Id>,
) -> Work {
    let by_id: BTreeMap<Id, &Turn> = turns.iter().map(|t| (t.envelope.id, t)).collect();
    // 発話ごとの最新の完了した応答 (04 §2 の往復)。
    let mut replies: BTreeMap<Id, &Turn> = BTreeMap::new();
    for t in turns {
        if t.body.role == TurnRole::Assistant
            && t.body.state == evorag_memory::TurnState::Completed
            && let Some(u) = t.body.reply_to
        {
            let e = replies.entry(u).or_insert(t);
            if t.envelope.id > e.envelope.id {
                *e = t;
            }
        }
    }
    let mut answers: Vec<&Turn> = turns
        .iter()
        .filter(|t| t.body.role == TurnRole::Assistant)
        .collect();
    answers.sort_by_key(|t| t.envelope.id);
    let mut pairs = Vec::new();
    let mut candidates = Vec::new();
    for u in &body.pairs {
        let Some(t) = by_id.get(u).filter(|t| t.body.role == TurnRole::User) else {
            continue;
        };
        let Some(utterance) = owned_text(directives, t) else {
            continue;
        };
        let reply = replies.get(u).and_then(|r| owned_text(directives, r));
        // 訂正の候補 (04 §6): 忘れるの指示の `turns`・`redactions`・`hidden` の発話は候補にしない。
        if utterance.hidden.is_empty()
            && !forgotten.contains(u)
            && let Some(h) = find_holes(MODE_CHAT, &utterance.text)
        {
            // 直前の応答: 同じセッションの、候補の発話より前の最後の応答のターン (状態は問わない)。
            let previous = answers
                .iter()
                .rev()
                .find(|a| a.envelope.id < *u)
                .and_then(|a| owned_text(directives, a));
            candidates.push(OwnedCandidate {
                turn: *u,
                pattern: h.pattern,
                new: h.new,
                old: h.old,
                previous,
            });
        }
        pairs.push(OwnedPair { utterance, reply });
    }
    Work {
        id,
        body: body.clone(),
        pairs,
        candidates,
    }
}
