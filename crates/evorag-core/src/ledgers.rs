//! 区切りと、区切りごとの 1 回の呼び出し (B2) の sleep-time の手順 (04 §4 の手順 2・3、v0.5 の根)。
//!
//! 1. **区切り** (手順 2、LLM を使わない): チャットのセッションごとに、記録した区切りと台帳から戻せる区切り
//!    (04 §2 の「台帳から戻す細目」) を前提に区切りを決め ([`evorag_memory::plan_segments`])、変わった区切りを書く
//!    (閉じた区切りは `Immediate`、03 §3.4)。
//! 2. **台帳** (手順 3): 呼び直すかの判定 ([`evorag_memory::needs_call`]) に当たる区切りを、最初の往復の順に、
//!    `memory` の役割の呼ぶ先 ([`crate::MemoryCall`]) で呼び、台帳を `Immediate` で書く (04 §5.2.1)。区切りごとに、
//!    呼ぶ前に呼ぶ先のモデルを確かめる ([`crate::MemoryCall::verify`])。
//!
//! 進み具合は状態から求めるので job を書かない (04 §4)。利用者が操作したら、流れている要求を 1 秒以内に止め
//! (ストリームを落として接続を閉じる、06 §4)、型の上で完結した要素を `partial` の台帳に残す (04 §5.2.1 の
//! 「止められたとき」)。この手順は裏の処理の future を落とさずに自分で止める ([`crate::sleep`])。
//!
//! 忘れるの指示は、`[非表示]` の範囲を印に置き換えてから渡し、指示と合わないターンは渡さない。忘れるの指示の
//! `turns`・`redactions`・`hidden` に入る発話は訂正の候補にしない (04 §6・§7.5)。private の値は伏せずに渡す
//! (04 §5.2)。台帳は書く前に、単位を書く経路のロックの下で、効いているすべての忘れるの指示で掃除する (04 §4)。
//! 旧エンジンの根 ([`crate::RootFormat::V04Bench`]) では動かさない。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evorag_llm::{ChatEvent, ChatMessage, ChatRequest, Generation, LlmClient, LlmError, Role};
use evorag_memory::b2::{
    self, Call, CallerInputs, Candidate, Counting, Input, InvalidReason, Layout, LedgerMeta,
    Outcome, Pair, Parsed, Plan, Reading, Request, StreamEnd, Text,
};
use evorag_memory::corrections::{CORRECTION_V1, find_holes};
use evorag_memory::{
    CallDecision, Current, DirectiveBody, DirectiveSet, KIND_DIRECTIVE, KIND_PROPOSAL,
    KIND_SEGMENT, LedgerBody, LedgerRead, LedgerStatus, LedgerTokens, MODE_CHAT, RuleVersions,
    ScrubContext, SegmentBody, SegmentChange, SelectEnv, Selection, StoredLedger, Through, Turn,
    TurnRole, TurnTexts, ledger_valid, needs_call, plan_segments, read_ledger, restore_segment,
    scrub_ledger, turn_rev_checked,
};
use evorag_store::{Envelope, Read, ReadTxn, Store};
use evorag_types::{Id, PromptLanguage, Timestamp};
use tokio::sync::futures::Notified;

use crate::{Inner, MemoryCall, MemoryRoute, VerifyError};

/// 1 回の B2 の要求の締切 (04 §5.2.1 の「失敗の扱い」の締切切れ)。生成の上限 (2,048 トークン) を遅いモデル
/// (12B 級で約 6 tok/s) で出し切る時間 (約 340 秒) に、プロンプトの処理と余裕を足した値 (暫定)。
const DEADLINE: Duration = Duration::from_secs(600);
/// 締切切れと 5xx がこの回数続いた区切りは、`failed` の台帳を書く (04 §5.2.1 の「失敗の扱い」)。
const SERVER_ERROR_LIMIT: u32 = 3;
/// 続けてこの数の区切りが 4xx か 5xx で終わったら、区切りの呼び出しを止める (04 §5.2.1 の「続けて失敗したとき」。
/// 暫定)。
const CONSECUTIVE_LIMIT: u32 = 3;
/// 組み立てた利用者のメッセージを数え直して計画を直す回数の上限 (予算は直すたびに小さくなるので、ふつうは数回で
/// 終わる)。
const REFIT_ROUNDS: usize = 64;

/// 手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話・指示・呼ぶ先が変わるたびに増える。突き合わせ終えた版と違えば、もう一度突き合わせる。
    version: u64,
    clean: Option<u64>,
    /// 開いている末尾の区切りが閉じる、いちばん早い時刻 (ミリ秒)。
    due_at: Option<i64>,
    /// 番号を振った文はあるのに 1 文も送れなかった区切り (04 §5.2.1 の「全部を送れなかった区切り」) と、そのときの
    /// 区切りの往復の数。台帳を書かないので、往復が変わるまで (か、次の起動まで) 飛ばす。
    unsent: HashMap<Id, usize>,
    /// 締切切れと 5xx が続いた区切りと、その回数 (台帳の `attempts`)。
    server_errors: HashMap<Id, u32>,
    /// 続けて 4xx か 5xx で終わった区切りの数。
    consecutive: u32,
    /// 続けて失敗したので呼び出しを止めた呼ぶ先 (model_key, `build_info`)。呼ぶ先が変われば試し直す。
    halted: Option<(String, String)>,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.ledgers.lock().unwrap_or_else(|e| e.into_inner())
}

/// 会話・指示・呼ぶ先が変わった (ターンを書いた、セッションを消した、忘れる、点検・役割の変化)。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 試験と `doctor` のための数 ([`crate::Core::ledger_stats`])。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerStats {
    /// 番号を振った文はあるのに、予算に入らず 1 文も送れなかった区切りの数 (このプロセスの間。台帳を書かない、
    /// 04 §5.2.1)。
    pub nothing_sent: usize,
    /// 続けて失敗したので、区切りの呼び出しを止めている。
    pub halted: bool,
}

pub(crate) fn stats(inner: &Inner) -> LedgerStats {
    let st = state(inner);
    LedgerStats {
        nothing_sent: st.unsent.len(),
        halted: st.halted.is_some(),
    }
}

/// 要るか (状態から求める。突き合わせていない変更があるか、末尾の区切りが閉じる時刻が来たか)。v0.5 の根で、
/// 起動時の突き合わせが済み、`memory.forget_key` が使えるとき (使えなければ呼び出しを止める、04 §5.2.1) だけ。
pub(crate) fn needed(inner: &Inner) -> bool {
    if crate::old_engine(inner)
        || !inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
        || inner.facts_key.key().is_none()
    {
        return false;
    }
    let st = state(inner);
    st.clean != Some(st.version)
        || st
            .due_at
            .is_some_and(|t| inner.clock.now().as_millis() >= t)
}

/// B2 の要求 (06 §6: 型縛り・temperature 0・固定の seed・固定のスロット・要求の付け足し)。
pub(crate) fn chat_request(
    request: &Request,
    extra: &evorag_llm::RequestExtra,
    slot: Option<u32>,
) -> ChatRequest {
    let mut extra = extra.clone();
    extra.insert("response_format".into(), request.response_format());
    ChatRequest {
        messages: vec![
            ChatMessage::new(Role::System, request.system),
            ChatMessage::new(Role::User, request.user.clone()),
        ],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(b2::MAX_TOKENS),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// ターンの本文 (B2 に渡す形。`[非表示]` の範囲は今の本文の位置)。
#[derive(Debug, Clone)]
struct OwnedText {
    turn: Id,
    rev: u32,
    text: String,
    hidden: Vec<Range<usize>>,
}

impl OwnedText {
    fn view(&self) -> Text<'_> {
        Text {
            turn: self.turn,
            rev: self.rev,
            text: &self.text,
            hidden: &self.hidden,
        }
    }
}

#[derive(Debug, Clone)]
struct OwnedPair {
    utterance: OwnedText,
    reply: Option<OwnedText>,
}

#[derive(Debug, Clone)]
struct OwnedCandidate {
    turn: Id,
    pattern: &'static str,
    new: Range<usize>,
    old: Option<Range<usize>>,
    previous: Option<OwnedText>,
}

/// 呼ぶ区切り 1 つ。
#[derive(Debug, Clone)]
struct Work {
    id: Id,
    body: SegmentBody,
    /// 渡せる往復 (発話が読めて、忘れるの指示と合うもの)。
    pairs: Vec<OwnedPair>,
    candidates: Vec<OwnedCandidate>,
}

impl Work {
    /// 台帳の `through`: 区切りの最後の往復と往復の数 (読めない往復を含む)。渡した往復で数えると、読めない往復の
    /// ある区切りは `through` が区切りに合わず、呼び直し続けるため (04 §5.2.1 の「判定の細目」)。
    fn through(&self) -> Option<Through> {
        Some(Through {
            turn: *self.body.pairs.last()?,
            pairs: u32::try_from(self.body.pairs.len()).ok()?,
        })
    }
}

/// 呼ぶ先の、選び方に要る値 (読む側に渡す)。
#[derive(Debug, Clone)]
struct Target {
    model_key: String,
    build_info: String,
    fallback: bool,
    role_model_key: Option<String>,
}

impl Target {
    fn of(c: &MemoryCall) -> Self {
        Self {
            model_key: c.model_key.clone(),
            build_info: c.build_info.clone(),
            fallback: c.is_fallback(),
            role_model_key: c.role_model_key.clone(),
        }
    }

    fn current(&self) -> Current<'_> {
        if self.fallback {
            Current::Fallback {
                variant: b2::VARIANT,
                intended: self.role_model_key.as_deref(),
                called: &self.model_key,
            }
        } else {
            Current::Ready(Selection {
                variant: b2::VARIANT,
                model_key: &self.model_key,
            })
        }
    }

    /// 台帳が今の選択のものか (状態は問わない。`evorag_memory::select_ledger` の「今」の段の台帳の作り)。
    fn is_current(&self, l: &LedgerBody) -> bool {
        if l.variant != b2::VARIANT {
            return false;
        }
        let own = |key: &str| !l.inputs.role_fallback && l.model_key == key;
        if self.fallback {
            self.role_model_key.as_deref().is_some_and(own)
                || (l.inputs.role_fallback
                    && l.model_key == self.model_key
                    && l.inputs.role_model_key == self.role_model_key)
        } else {
            own(&self.model_key)
        }
    }
}

/// 手順 2 で読んで決めたもの。
struct Snapshot {
    /// 書く区切り (本体と、読んだ前の記録)。最初の往復の順。
    writes: Vec<(SegmentBody, Option<Envelope>)>,
    /// 呼ぶ区切り (最初の往復の順、セッションをまたぐ 1 本の列)。
    work: Vec<Work>,
    due_at: Option<i64>,
    /// 入力を読んだときに見えていた最新の忘れるの指示。
    forgets_seen: Option<Id>,
}

/// 効いている指示を読む。読めない指示があれば `None` (忘れるの範囲を確かめられないので呼ばない)。
fn load_directives(r: &ReadTxn<'_>, lang: PromptLanguage) -> Option<DirectiveSet> {
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
fn load_ledgers(r: &ReadTxn<'_>) -> Option<HashMap<Id, Vec<(Id, LedgerBody)>>> {
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
fn owned_text(directives: &DirectiveSet, t: &Turn) -> Option<OwnedText> {
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
struct Judge<'a> {
    target: &'a Target,
    previous: Option<String>,
    unsent: &'a HashMap<Id, usize>,
}

/// 記録を読んで、区切りを決め (手順 2)、呼ぶ区切りの入力を作る (手順 3 の前半)。
fn read(
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

/// 利用者の操作を待つ口。一度鳴ったら、以後の待ちはすぐに終わる。
struct Yield<'a> {
    notified: Pin<Box<Notified<'a>>>,
    fired: bool,
}

impl<'a> Yield<'a> {
    fn new(inner: &'a Inner) -> Self {
        let mut notified = Box::pin(inner.activity.touched_notify().notified());
        notified.as_mut().enable();
        Self {
            notified,
            fired: false,
        }
    }

    /// `fut` を流し、利用者が操作したら落として `None` を返す。
    async fn run<T>(&mut self, fut: impl Future<Output = T>) -> Option<T> {
        if self.fired {
            return None;
        }
        tokio::select! {
            biased;
            () = self.notified.as_mut() => {
                self.fired = true;
                None
            }
            v = fut => Some(v),
        }
    }
}

/// 1 回の要求の終わり方。
enum Streamed {
    /// 生成を終えた (`length` なら上限で切れた)。
    Finished {
        text: String,
        length: bool,
        usage: Option<evorag_llm::Usage>,
    },
    /// 利用者の操作で止めた (ストリームを落として接続を閉じた)。受け取ったところまでの本文。
    Stopped { text: String },
    /// 失敗した。
    Failed(Failure),
}

/// 失敗の種類 (04 §5.2.1 の「失敗の扱い」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// 4xx (要求そのものが受け付けられない。再試行しない)。
    Rejected,
    /// 締切切れ・5xx・ストリームの途中の切断 (書かずに次の機会に送り直す)。
    Transient,
    /// 繋がらない (回数に入れず、書かずに待つ)。
    Unreachable,
}

impl Failure {
    fn of(e: &LlmError) -> Self {
        match e {
            LlmError::Connect(_) => Self::Unreachable,
            LlmError::Http { status, .. } if (400..500).contains(status) => Self::Rejected,
            LlmError::BadEndpoint(_)
            | LlmError::RemoteNotAllowed(_)
            | LlmError::RouterNeedsModel { .. }
            | LlmError::RouterModel { .. } => Self::Rejected,
            _ => Self::Transient,
        }
    }
}

/// 要求を逐次表示で送り、終わるまで受け取る。利用者が操作したら、ストリームを落として止める (06 §4)。
async fn stream(llm: &dyn LlmClient, req: ChatRequest, stop: &mut Yield<'_>) -> Streamed {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let opened = stop
        .run(tokio::time::timeout_at(deadline, llm.chat(req)))
        .await;
    let mut s = match opened {
        None => {
            return Streamed::Stopped {
                text: String::new(),
            };
        }
        Some(Err(_)) => return Streamed::Failed(Failure::Transient),
        Some(Ok(Err(e))) => {
            tracing::warn!(error = %e, "a segment call could not start");
            return Streamed::Failed(Failure::of(&e));
        }
        Some(Ok(Ok(s))) => s,
    };
    let mut text = String::new();
    loop {
        let ev = stop.run(tokio::time::timeout_at(deadline, s.next())).await;
        match ev {
            // 止める: ストリームを落として接続を閉じる (llama-server は生成を取り消す)。
            None => {
                drop(s);
                return Streamed::Stopped { text };
            }
            Some(Err(_)) => {
                tracing::warn!("a segment call timed out");
                return Streamed::Failed(Failure::Transient);
            }
            Some(Ok(None)) => return Streamed::Failed(Failure::Transient),
            Some(Ok(Some(Err(e)))) => {
                tracing::warn!(error = %e, "a segment call failed while streaming");
                return Streamed::Failed(Failure::of(&e));
            }
            Some(Ok(Some(Ok(ChatEvent::Delta(d))))) => text.push_str(&d),
            Some(Ok(Some(Ok(ChatEvent::Reasoning(_))))) => {}
            Some(Ok(Some(Ok(ChatEvent::Done {
                finish_reason,
                usage,
            })))) => {
                return Streamed::Finished {
                    text,
                    length: finish_reason.as_deref() == Some("length"),
                    usage,
                };
            }
        }
    }
}

/// 区切り 1 つを処理した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    /// 台帳を書いた。
    Written,
    /// 書かなかった (組み立てられない・書く前の確かめに合わない・結果の無い呼び出し)。次へ進む。
    Skipped,
    /// 1 文も送れなかった (数えて飛ばす)。
    Unsent,
    /// 利用者の操作で止めた (止めたところまでを書いた後)。手順を終える。
    Stopped,
    /// 締切切れ・5xx で、書かずに次の機会に送り直す。手順を終える。
    Retry,
    /// 繋がらない。手順を終える。
    Unreachable,
    /// 続けて失敗したので、呼び出しを止めた。
    Halted,
}

/// 呼び出しの出力を数えて、台帳に書くまで。
struct Calling<'a> {
    inner: &'a Arc<Inner>,
    call: &'a MemoryCall,
    target: &'a Target,
    forgets_seen: Option<Id>,
}

impl Calling<'_> {
    async fn count(
        &self,
        text: &str,
        counting: &mut Counting,
        stop: &mut Yield<'_>,
    ) -> Option<usize> {
        let inner = self.inner;
        let llm = &*self.call.client;
        let key = &self.call.model_key;
        let n = stop.run(inner.tokens.count(llm, key, text)).await?;
        Some(match n {
            Some(n) => n,
            None => {
                *counting = Counting::Estimated;
                crate::estimate(text)
            }
        })
    }

    /// 計画を予算に収め (組み立てた利用者のメッセージを `/tokenize` で数えられるなら確かめて直し)、呼び出しを作る。
    /// 1 文も送れなければ `Ok(None)`。止められたら `Err(())`。
    async fn fit(
        &self,
        layout: &Layout,
        through: Option<Through>,
        stop: &mut Yield<'_>,
    ) -> Result<Option<(Plan, Call)>, ()> {
        if layout.is_empty() {
            let counts = vec![0; layout.pieces().len()];
            let plan = layout.plan(&counts, 0).map_err(|_| ())?;
            let call = render(layout, &plan, through);
            return Ok(Some((plan, call)));
        }
        let mut counting = Counting::Exact;
        let system = self
            .count(layout.system(), &mut counting, stop)
            .await
            .ok_or(())?;
        let pieces: Vec<String> = layout.pieces().map(str::to_owned).collect();
        let mut counts = Vec::with_capacity(pieces.len());
        for p in &pieces {
            counts.push(self.count(p, &mut counting, stop).await.ok_or(())?);
        }
        let budget = b2::user_budget(system, counting);
        let mut plan = layout.plan(&counts, budget).map_err(|_| ())?;
        let mut call = render(layout, &plan, through);
        if counting == Counting::Exact {
            let mut i = 0;
            let mut rounds = 0;
            while i < call.requests.len() && rounds < REFIT_ROUNDS {
                rounds += 1;
                let mut exact = Counting::Exact;
                let user = call.requests[i].user.clone();
                let measured = self.count(&user, &mut exact, stop).await.ok_or(())?;
                if exact != Counting::Exact {
                    break;
                }
                match layout
                    .refit(&plan, i, &counts, budget, measured)
                    .map_err(|_| ())?
                {
                    Some((p, _)) => {
                        plan = p;
                        call = render(layout, &plan, through);
                    }
                    None => i += 1,
                }
            }
        }
        if plan.nothing_sent() {
            return Ok(None);
        }
        Ok(Some((plan, call)))
    }

    /// 区切り 1 つを呼び、台帳を書く (04 §5.2.1 の「呼び出しの進め方」)。
    async fn segment(&self, work: &Work, stop: &mut Yield<'_>) -> Done {
        let inner = self.inner;
        let pairs: Vec<Pair<'_>> = work
            .pairs
            .iter()
            .map(|p| Pair {
                utterance: p.utterance.view(),
                reply: p.reply.as_ref().map(OwnedText::view),
            })
            .collect();
        if pairs.is_empty() {
            tracing::info!("no pair of a segment can be read; it is not called");
            return Done::Skipped;
        }
        let candidates: Vec<Candidate<'_>> = work
            .candidates
            .iter()
            .map(|c| Candidate {
                turn: c.turn,
                pattern: c.pattern,
                new: c.new.clone(),
                old: c.old.clone(),
                previous: c.previous.as_ref().map(OwnedText::view),
            })
            .collect();
        let input = Input {
            language: inner.language,
            pairs: &pairs,
            candidates: &candidates,
            correction_version: CORRECTION_V1,
        };
        let layout = match Layout::new(&input) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "a segment call could not be laid out; it is skipped");
                return Done::Skipped;
            }
        };
        let through = work.through();
        let (mut plan, mut call) = match self.fit(&layout, through, stop).await {
            Err(()) => return Done::Stopped,
            Ok(None) => {
                tracing::warn!(
                    sentences = layout.sentence_count(),
                    "no sentence of a segment fits the request budget; no ledger is written"
                );
                state(inner).unsent.insert(work.id, work.body.pairs.len());
                return Done::Unsent;
            }
            Ok(Some(x)) => x,
        };
        state(inner).unsent.remove(&work.id);
        if !call.requests.is_empty() && crate::notes::response_slot_busy(&*self.call.client).await {
            return Done::Stopped;
        }

        let started = Instant::now();
        let mut tokens = LedgerTokens::default();
        let mut results: Vec<Outcome> = Vec::new();
        let mut retried = false;
        let mut stopped = false;
        let mut errors = false;
        // 締切切れか 5xx が続いたので、送り直さずに諦めた呼び出しがある。
        let mut gave_up = false;
        let mut i = 0;
        while i < call.requests.len() {
            let request = &call.requests[i];
            let req = chat_request(request, &self.call.extra, self.call.slot);
            match stream(&*self.call.client, req, stop).await {
                Streamed::Stopped { text } => {
                    if let Reading::Complete(p) | Reading::Partial(p) =
                        b2::read_output(request, &text, StreamEnd::Stopped)
                    {
                        results.push(Outcome::Parsed(p));
                    }
                    stopped = true;
                    break;
                }
                Streamed::Finished {
                    text,
                    length,
                    usage,
                } => {
                    if let Some(u) = usage {
                        tokens.input = tokens
                            .input
                            .saturating_add(u32::try_from(u.prompt_tokens).unwrap_or(u32::MAX));
                        tokens.output = tokens
                            .output
                            .saturating_add(u32::try_from(u.completion_tokens).unwrap_or(u32::MAX));
                    }
                    let end = if length {
                        StreamEnd::Length
                    } else {
                        StreamEnd::Finished
                    };
                    match b2::read_output(request, &text, end) {
                        Reading::Complete(p) | Reading::Partial(p) => {
                            results.push(Outcome::Parsed(p));
                            i += 1;
                            retried = false;
                        }
                        Reading::Truncated => {
                            // 往復の境で半分に分けて送り直す (1 つの往復なら型が合わないときと同じ、04 §5.2.1)。
                            if let Some((p, _)) = plan.halve(i, &layout) {
                                tracing::info!(
                                    "a segment call was cut at the output limit; it is sent in halves"
                                );
                                plan = p;
                                call = render(&layout, &plan, through);
                                retried = false;
                            } else if !retried {
                                retried = true;
                            } else {
                                results.push(Outcome::Failed(Parsed::failed(
                                    InvalidReason::Truncated,
                                )));
                                i += 1;
                                retried = false;
                            }
                        }
                        Reading::Invalid(_) | Reading::Nothing if !retried => retried = true,
                        Reading::Invalid(p) => {
                            tracing::warn!("a segment call did not match the schema twice");
                            results.push(Outcome::Failed(p));
                            i += 1;
                            retried = false;
                        }
                        Reading::Nothing => {
                            results.push(Outcome::Failed(Parsed::failed(InvalidReason::NotJson)));
                            i += 1;
                            retried = false;
                        }
                    }
                }
                Streamed::Failed(Failure::Rejected) => {
                    errors = true;
                    results.push(Outcome::Failed(Parsed::failed(
                        InvalidReason::RequestRejected,
                    )));
                    i += 1;
                    retried = false;
                }
                Streamed::Failed(Failure::Transient) => {
                    let n = {
                        let mut st = state(inner);
                        let n = st.server_errors.entry(work.id).or_default();
                        *n += 1;
                        *n
                    };
                    if n < SERVER_ERROR_LIMIT {
                        return Done::Retry;
                    }
                    errors = true;
                    gave_up = true;
                    results.push(Outcome::Failed(Parsed::failed(InvalidReason::ServerError)));
                    i += 1;
                    retried = false;
                }
                Streamed::Failed(Failure::Unreachable) => return Done::Unreachable,
            }
        }
        tokens.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !call.requests.is_empty() && results.is_empty() {
            // 止められて完結した要素が 1 つも無い (台帳を書かない)。
            return if stopped {
                Done::Stopped
            } else {
                Done::Skipped
            };
        }
        let all_failed = results.iter().all(|r| matches!(r, Outcome::Failed(_)));
        if all_failed && errors {
            let mut st = state(inner);
            st.consecutive += 1;
            if st.consecutive >= CONSECUTIVE_LIMIT {
                st.halted = Some((
                    self.target.model_key.clone(),
                    self.target.build_info.clone(),
                ));
                tracing::warn!(
                    failures = st.consecutive,
                    "segment calls kept failing; they are stopped until the model or the settings change"
                );
                return Done::Halted;
            }
        } else if !all_failed {
            state(inner).consecutive = 0;
        }
        // 締切切れと 5xx で送り直した回数 (諦めた呼び出しの最後の失敗は、送り直していない)。
        let failures = state(inner)
            .server_errors
            .get(&work.id)
            .copied()
            .unwrap_or(0);
        let attempts = failures.saturating_sub(u32::from(gave_up));
        let caller = CallerInputs {
            forgets_seen: self.forgets_seen,
            role_fallback: self.call.is_fallback(),
            role_model_key: self.call.role_model_key.clone(),
        };
        let build_info = Some(self.target.build_info.as_str()).filter(|b| !b.is_empty());
        let meta = LedgerMeta {
            segment: work.id,
            model_key: &self.target.model_key,
            build_info,
            caller: &caller,
            attempts,
            tokens,
        };
        let body = match b2::ledger_body(&call, &results, &meta) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "a ledger could not be assembled");
                return Done::Skipped;
            }
        };
        let first = work.body.pairs.first().copied();
        let written = save(inner, body, first, self.target.clone()).await;
        state(inner).server_errors.remove(&work.id);
        match (written, stopped) {
            (_, true) => Done::Stopped,
            (true, false) => Done::Written,
            (false, false) => Done::Skipped,
        }
    }
}

/// 計画から呼び出しを作り、台帳の `through` を区切りのもの ([`Work::through`]) にする。
fn render(layout: &Layout, plan: &Plan, through: Option<Through>) -> Call {
    let mut call = layout.render(plan);
    if let Some(t) = through {
        call.through = t;
    }
    call
}

/// 置き換えて消す台帳 (04 §5.2.1 の「台帳を消すとき」)。`complete` を書くときは、同じ区切りのほかの台帳のうち、
/// 直前の選択の最新の有効な台帳を除くものを消す (`partial` と末尾の区切りの前の台帳を含む)。`partial`・`failed`
/// を書くときは、今の選択の `partial`・`failed` の台帳だけを消す (使える `complete` の台帳は、次の `complete` で
/// 置き換えるまで残す)。
fn replaced(
    r: &ReadTxn<'_>,
    new: &LedgerBody,
    target: &Target,
    previous: Option<&str>,
) -> Option<Vec<Envelope>> {
    let seg = new.segment?;
    let mut found: Vec<(Envelope, LedgerBody)> = Vec::new();
    for (id, _) in r.links_to(seg, evorag_memory::REL_LEDGER_SEGMENT).ok()? {
        if let Some(Read::Ok(e)) = r.get(KIND_PROPOSAL, id).ok()?
            && let Some(LedgerRead::Segment(b)) = read_ledger(&e)
            && b.segment == Some(seg)
        {
            found.push((e, *b));
        }
    }
    let rev = |id: Id| -> Option<u32> {
        match r.get(evorag_memory::KIND_TURN, id).ok()? {
            Some(Read::Ok(e)) => turn_rev_checked(&e),
            _ => None,
        }
    };
    let versions = RuleVersions::current(CORRECTION_V1);
    let keep_previous = previous.and_then(|p| {
        found
            .iter()
            .filter(|(_, b)| {
                !b.inputs.role_fallback
                    && b.variant == b2::VARIANT
                    && b.model_key == p
                    && matches!(b.status(), LedgerStatus::Complete | LedgerStatus::Partial)
                    && ledger_valid(b, rev, versions).is_valid()
            })
            .map(|(e, _)| e.id)
            .max()
    });
    let out = found
        .into_iter()
        .filter(|(e, b)| match new.status() {
            LedgerStatus::Complete => Some(e.id) != keep_previous,
            _ => {
                target.is_current(b)
                    && matches!(b.status(), LedgerStatus::Partial | LedgerStatus::Failed)
            }
        })
        .map(|(e, _)| e)
        .collect();
    Some(out)
}

/// 台帳を、単位を書く経路のロックの下で、効いているすべての忘れるの指示で掃除してから書く (04 §4・§5.2.1)。
/// 書き手は、`sentences` と `refs` のターンの改訂の番号が読んだときと合うことを確かめ、合わなければ全体を断る
/// (捨てる)。落とされない task で行う。
async fn save(inner: &Arc<Inner>, mut body: LedgerBody, first: Option<Id>, target: Target) -> bool {
    let lock = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let w = worker.clone();
        let built = tokio::task::spawn_blocking(move || -> Option<evorag_store::WriteRequest> {
            let key = w.facts_key.key()?;
            let r = w.store.read().ok()?;
            let directives = load_directives(&r, w.language)?;
            let mut turns: BTreeSet<Id> = body.sources().into_iter().map(|s| s.turn).collect();
            turns.extend(body.inputs.corr.iter().map(|c| c.turn));
            let mut owned: Vec<(Id, u32, String)> = Vec::new();
            let mut missing: Vec<Id> = Vec::new();
            for t in turns {
                match r.get(evorag_memory::KIND_TURN, t).ok()? {
                    Some(Read::Ok(e)) => {
                        match (turn_rev_checked(&e), e.body_as::<evorag_memory::TurnBody>()) {
                            (Some(rev), Ok(b)) => owned.push((t, rev, b.text)),
                            _ => missing.push(t),
                        }
                    }
                    _ => missing.push(t),
                }
            }
            let mut texts = TurnTexts::new();
            for (t, rev, text) in &owned {
                texts.insert(*t, *rev, text);
            }
            for t in &missing {
                texts.unreadable(*t);
            }
            let scrubbed = scrub_ledger(
                &mut body,
                &ScrubContext {
                    key,
                    directives: &directives,
                    first_turn: first,
                    texts: &texts,
                },
            )
            .map_err(|e| tracing::warn!(error = %e, "a ledger could not be scrubbed"))
            .ok()?;
            if scrubbed.changed() {
                tracing::info!(
                    removed = scrubbed.removed(),
                    "a ledger was scrubbed by a forget added while it was made"
                );
            }
            let previous = previous_key(&r, &w);
            let replaces = replaced(&r, &body, &target, previous.as_deref())?;
            let (_, req) = crate::writes(&w)
                .ledger(&body, None, &replaces)
                .map_err(|e| tracing::warn!(error = %e, "a ledger could not be written"))
                .ok()?;
            Some(req)
        })
        .await
        .ok()
        .flatten();
        let Some(req) = built else {
            return false;
        };
        match crate::write_outcome(&worker, req).await {
            Ok(_) => true,
            Err(e) => {
                // 読んでから書くまでに、ターンが変わった・消えた (伏せ字・削除)。古い本文から作った台帳を捨てる。
                tracing::info!(error = %e, "a ledger was discarded (its turns changed while it was made)");
                false
            }
        }
    });
    task.await.unwrap_or(false)
}

/// 直前の選択の model_key (06 §3.1 の履歴の前の model_key。`llm.memory` を設定したときは memory の、しないときは
/// chat の記録)。
fn previous_key(r: &ReadTxn<'_>, inner: &Inner) -> Option<String> {
    let role = if inner.memory_configured() {
        evorag_memory::ROLE_MEMORY
    } else {
        evorag_memory::ROLE_CHAT
    };
    let bindings = evorag_memory::load_bindings(r).ok()?;
    let b = bindings.by_role.get(role)?;
    b.body.history.last().map(|o| o.model_key.clone())
}

/// 区切りを書く (閉じた区切りは `Immediate`、開いている末尾は裏の処理の結果として、03 §3.4)。書けなければ偽。
async fn write_segments(inner: &Arc<Inner>, writes: Vec<(SegmentBody, Option<Envelope>)>) -> bool {
    for (body, previous) in writes {
        let req = match crate::writes(inner).segment_request(&body, previous.as_ref()) {
            Ok((_, req)) => req,
            Err(e) => {
                tracing::warn!(error = %e, "a segment could not be written");
                return false;
            }
        };
        if let Err(e) = crate::write_outcome(inner, req).await {
            // その間に区切りが変わった (前の本体の確かめに合わない)。次の回で決め直す。
            tracing::info!(error = %e, "a segment was not written (it changed meanwhile)");
            return false;
        }
    }
    true
}

/// 手順を動かす。利用者が操作したら、流れている要求を止め、止めたところまでを書いて `None` を返す。終えたら、
/// 進んだか (区切りか台帳を 1 つでも書いたか)。
pub(crate) async fn run(inner: &Arc<Inner>, generation: u64) -> Option<bool> {
    let mut stop = Yield::new(inner);
    if inner.activity.generation() != generation {
        return None;
    }
    let version = state(inner).version;
    let route = crate::memory_role::resolve(inner).1;
    let call = match route {
        MemoryRoute::Call(c) => {
            let halted = state(inner).halted.clone();
            match halted {
                Some((k, b)) if k == c.model_key && b == c.build_info => None,
                Some(_) => {
                    let mut st = state(inner);
                    st.halted = None;
                    st.consecutive = 0;
                    Some(c)
                }
                None => Some(c),
            }
        }
        MemoryRoute::Pending | MemoryRoute::Stopped => None,
    };
    let target = call.as_ref().map(Target::of);
    let unsent = state(inner).unsent.clone();
    let store = inner.store.clone();
    let lang = inner.language;
    let now = inner.clock.now().as_millis();
    let previous = inner
        .store
        .read()
        .ok()
        .and_then(|r| previous_key(&r, inner));
    let target_for_read = target.clone();
    // 読む間は単位を書く経路のロックを持つ (落とされても、終了の処理がこのロックを取れたら、読み終えてストアを
    // 手放している)。
    let lock = inner.facts_lock.clone().lock_owned().await;
    let snap = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let judge = target_for_read.as_ref().map(|t| Judge {
            target: t,
            previous,
            unsent: &unsent,
        });
        read(&store, lang, now, judge.as_ref())
    })
    .await
    .ok()
    .flatten();
    let Some(snap) = snap else {
        return Some(false);
    };
    let mut progressed = !snap.writes.is_empty();
    if !write_segments(inner, snap.writes).await {
        return Some(false);
    }
    state(inner).due_at = snap.due_at;
    let (Some(call), Some(target)) = (call, target) else {
        state(inner).clean = Some(version);
        return Some(progressed);
    };
    let calling = Calling {
        inner,
        call: &call,
        target: &target,
        forgets_seen: snap.forgets_seen,
    };
    for work in &snap.work {
        // 区切りを呼ぶ前ごとに、呼ぶ先のモデルが選んだ時と同じかを確かめる (06 §2.1 の細目)。
        match stop.run(call.verify()).await? {
            Ok(info) if b2::fits(info.n_ctx as usize) => {}
            Ok(info) => {
                tracing::warn!(
                    n_ctx = info.n_ctx,
                    "the slot context is too small for segment calls; they wait"
                );
                return Some(progressed);
            }
            Err(e) => {
                tracing::warn!(error = %e, "the segment call target changed or is not reachable");
                if !call.is_fallback() && inner.memory_configured() {
                    // 記録と照合し直す (繋がるまで chat で代行する、06 §3 の表)。
                    crate::memory_role::mark_unreachable(inner);
                } else if matches!(e, VerifyError::Changed { .. }) {
                    // chat のモデルが差し替わった。応答の経路の照合 (06 §3) に任せ、ここでは呼ばない。
                    touch(inner);
                }
                return Some(progressed);
            }
        }
        match calling.segment(work, &mut stop).await {
            Done::Written => progressed = true,
            Done::Skipped | Done::Unsent => {}
            Done::Stopped => return None,
            // 次の機会に送り直す (すぐには送り直さず、裏の処理の「繰り返しを防ぐ」で待たせる、04 §4)。止めた
            // ときは、次の回の区切りの突き合わせで、呼ぶ先が変わるまで呼ばない。
            Done::Retry | Done::Halted => return Some(false),
            Done::Unreachable => {
                if !call.is_fallback() && inner.memory_configured() {
                    // 繋がるまで chat で代行する (06 §3 の表)。呼ぶ先が変わったので、すぐに続ける。
                    crate::memory_role::mark_unreachable(inner);
                    return Some(true);
                }
                return Some(false);
            }
        }
    }
    state(inner).clean = Some(version);
    Some(progressed)
}
