//! v0.4 の根から新しい根へ、何を写し、何を除き、何を変えて写すか (03 §12.4 の「写すもの」・「写して変えるもの」・
//! 「除くもの」・「指示の写し方」、04 §7.5)。
//!
//! - [`Selector`]: 封筒をそのまま写す種類 ([`COPY_KINDS`]) の記録を、写すか除くかに分ける。除く理由は
//!   [`Exclusion`]。
//! - [`imported_directive`]: `fact_directive` を `directive` に写す (同じ ID、`imported` を真に)。
//! - [`v04_ended`]: v0.4 で今の値でなかった値の表 (`meta` の `import.v04_ended`)。値の文字列は持たない。
//! - [`copy_ops`] は、写す封筒を、新しい根の種類の関係と副索引とともに書く操作。
//!
//! 古い根は読むだけ。DB を読めない・記録を読めないときは [`Refusal`] で断る (黙って写さないことはしない)。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use evorag_store::export::ExportView;
use evorag_store::forget::{Redaction, TurnOutcome, apply_to_turn};
use evorag_store::import_v04::Refusal;
use evorag_store::{DeriveFn, Envelope, Read, WriteOp};
use evorag_types::Id;
use serde::{Deserialize, Serialize};

use super::body::{
    FactBody, FactDirectiveBody, OP_ACCEPT, OP_FORGET, OP_PRIVATE, OP_PRIVATE_OFF, OP_REVERT,
    OP_UNBLOCK,
};
use super::kind_set::{KIND_FACT, KIND_FACT_DIRECTIVE};
use crate::directive::{DIRECTIVE_V, DirectiveBody, KIND_DIRECTIVE, TurnSpan};
use crate::kinds::{
    KIND_MODEL_BINDING, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_SETTING, KIND_SIGNAL,
    KIND_THRESHOLD, KIND_TURN,
};
use crate::value_key::{ForgetKey, ValueKey, loose_key};

/// 封筒をそのまま写す種類 (書く順、03 §12.4 の「書き方」のジャーナルの行の順。`threshold` はジャーナルに書かない)。
pub const COPY_KINDS: [&str; 7] = [
    KIND_SESSION,
    KIND_TURN,
    KIND_SIGNAL,
    KIND_RECALL_FEEDBACK,
    KIND_SETTING,
    KIND_MODEL_BINDING,
    KIND_THRESHOLD,
];

/// 写さない理由 (03 §12.4 の「除くもの」と「読み方」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Exclusion {
    /// (a) セッションの無いターン (古い版で戻したセッションに足した会話、03 §6)。
    TurnWithoutSession,
    /// (b) 忘れるの伏せ字の記録と本文が合わないターン (伏せ字の前の本文を写さない、03 §7.3)。
    RedactionMismatch,
    /// (c) 対象のターンを写さなかった `signal`・`recall_feedback`。
    TargetNotCopied,
    /// ジャーナルに削除の行のある ID の記録 (v0.4 が開くときに消すもの、03 §6)。
    Deleted,
}

impl Exclusion {
    /// 照合の結果 (`import.report`) に書く名前。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Exclusion::TurnWithoutSession => "turn_without_session",
            Exclusion::RedactionMismatch => "redaction_mismatch",
            Exclusion::TargetNotCopied => "target_not_copied",
            Exclusion::Deleted => "deleted",
        }
    }
}

/// 1 件の記録を写すか ([`Selector::judge`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Copy,
    Exclude(Exclusion),
}

fn read_error(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unreadable {
        what: e.to_string(),
    }
}

fn unreadable_record(kind: &str, id: Id) -> Refusal {
    Refusal::Unreadable {
        what: format!("record {kind}/{id}"),
    }
}

/// 読める封筒。チェックサムが合わない・新しい版の記録は読めない記録として断る。
fn readable(r: Read) -> Result<Envelope, Refusal> {
    match r {
        Read::Ok(e) => Ok(e),
        Read::Newer { kind, id, .. } | Read::Corrupt { kind, id, .. } => {
            Err(unreadable_record(&kind, id))
        }
    }
}

/// 1 つの種類の読める記録をすべて、ID の順に読む。
///
/// # Errors
/// DB を読めない・読めない記録があるとき。
pub fn read_kind(view: &ExportView<'_>, kind: &str) -> Result<Vec<Envelope>, Refusal> {
    let mut out = Vec::new();
    let mut bad = None;
    view.for_each(kind, |r| match readable(r) {
        Ok(e) => {
            out.push(e);
            true
        }
        Err(e) => {
            bad = Some(e);
            false
        }
    })
    .map_err(read_error)?;
    match bad {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// v0.4 の `fact_directive` を ID の順に読む (本体も読む)。
///
/// # Errors
/// DB を読めない・読めない記録があるとき。
pub fn read_fact_directives(
    view: &ExportView<'_>,
) -> Result<Vec<(Envelope, FactDirectiveBody)>, Refusal> {
    read_kind(view, KIND_FACT_DIRECTIVE)?
        .into_iter()
        .map(|e| {
            let b = e
                .body_as::<FactDirectiveBody>()
                .map_err(|_| unreadable_record(&e.kind, e.id))?;
            Ok((e, b))
        })
        .collect()
}

/// v0.4 の事実 (ID → 本体)。指示の根拠の位置と `import.v04_ended` に使う (事実そのものは写さない)。
///
/// # Errors
/// DB を読めない・読めない記録があるとき。
pub fn read_facts(view: &ExportView<'_>) -> Result<BTreeMap<Id, FactBody>, Refusal> {
    read_kind(view, KIND_FACT)?
        .into_iter()
        .map(|e| {
            let b = e
                .body_as::<FactBody>()
                .map_err(|_| unreadable_record(&e.kind, e.id))?;
            Ok((e.id, b))
        })
        .collect()
}

#[derive(Deserialize)]
struct SessionOf {
    session: Id,
}

#[derive(Deserialize)]
struct TargetOf {
    target: Id,
}

#[derive(Deserialize)]
struct FeedbackOf {
    answer: Id,
    target: Id,
}

/// 封筒をそのまま写す種類の記録を、写すか除くかに分ける。[`COPY_KINDS`] の順に、種類ごとに ID の順で渡す (写した
/// セッションとターンを覚えて、後の種類の判定に使うため)。
#[derive(Debug, Clone)]
pub struct Selector {
    deleted: BTreeSet<Id>,
    /// 忘れるの伏せ字 (ターン → 指示の ID の順の伏せ字。記録の形が正しくなければ `None`)。
    redactions: BTreeMap<Id, Option<Vec<Redaction>>>,
    markers: Vec<&'static str>,
    sessions: BTreeSet<Id>,
    turns: BTreeSet<Id>,
}

impl Selector {
    /// `directives` (v0.4 の `fact_directive`) の忘れるの伏せ字と、ジャーナルに削除の行のある ID `deleted` から作る。
    /// `markers` は伏せ字の印 (プロンプトの言語の印。言語の記録が無い・知らない値なら、すべての印、04 §7.3)。
    #[must_use]
    pub fn new(
        directives: &[(Envelope, FactDirectiveBody)],
        deleted: impl IntoIterator<Item = Id>,
        markers: &[&'static str],
    ) -> Self {
        let mut redactions: BTreeMap<Id, Option<Vec<Redaction>>> = BTreeMap::new();
        let mut forgets: Vec<&(Envelope, FactDirectiveBody)> = directives
            .iter()
            .filter(|(_, b)| b.op == OP_FORGET)
            .collect();
        forgets.sort_by_key(|(e, _)| e.id);
        for (_, b) in forgets {
            let mut per_turn: BTreeMap<Id, Vec<&super::body::RedactionBody>> = BTreeMap::new();
            for r in &b.redactions {
                per_turn.entry(r.turn).or_default().push(r);
            }
            for (turn, rs) in per_turn {
                let first = rs[0];
                let same = rs.iter().all(|r| {
                    (r.rev_before, r.rev_after, &r.hash)
                        == (first.rev_before, first.rev_after, &first.hash)
                });
                let step = Redaction::from_stored(
                    first.rev_before,
                    first.rev_after,
                    rs.iter().map(|r| (u64::from(r.start), u64::from(r.end))),
                    &first.hash,
                )
                .ok()
                .filter(|_| same);
                let slot = redactions.entry(turn).or_insert_with(|| Some(Vec::new()));
                match (slot.as_mut(), step) {
                    (Some(steps), Some(step)) => steps.push(step),
                    _ => *slot = None,
                }
            }
        }
        Self {
            deleted: deleted.into_iter().collect(),
            redactions,
            markers: markers.to_vec(),
            sessions: BTreeSet::new(),
            turns: BTreeSet::new(),
        }
    }

    /// 忘れるの伏せ字の掛かるターンを、書き始める前に確かめる (DB の本文が当て直しの前なら断る。合わないターンは
    /// 写すときに除くので、ここでは断らない)。
    ///
    /// # Errors
    /// DB を読めない・ターンを読めないとき、本文が忘れるの当て直しの前のとき ([`Refusal::ForgetPending`])。
    pub fn check_redacted_turns(&self, view: &ExportView<'_>) -> Result<(), Refusal> {
        for &turn in self.redactions.keys() {
            if self.deleted.contains(&turn) {
                continue;
            }
            if let Some(r) = view.get(KIND_TURN, turn).map_err(read_error)? {
                self.redactions_match(&readable(r)?)?;
            }
        }
        Ok(())
    }

    /// ジャーナルに削除の行のある ID か。
    #[must_use]
    pub fn is_deleted(&self, id: Id) -> bool {
        self.deleted.contains(&id)
    }

    /// 記録 `env` を写すか。写すなら、写したセッションとターンを覚える。
    ///
    /// # Errors
    /// 判定に要る項目を読めないとき、DB の本文が忘れるの当て直しの前のとき ([`Refusal::ForgetPending`])。
    pub fn judge(&mut self, env: &Envelope) -> Result<Disposition, Refusal> {
        if self.deleted.contains(&env.id) {
            return Ok(Disposition::Exclude(Exclusion::Deleted));
        }
        let field = || unreadable_record(&env.kind, env.id);
        let verdict = match env.kind.as_str() {
            KIND_SESSION => {
                self.sessions.insert(env.id);
                Disposition::Copy
            }
            KIND_TURN => {
                let SessionOf { session } = env.body_as().map_err(|_| field())?;
                if !self.sessions.contains(&session) {
                    Disposition::Exclude(Exclusion::TurnWithoutSession)
                } else if !self.redactions_match(env)? {
                    Disposition::Exclude(Exclusion::RedactionMismatch)
                } else {
                    self.turns.insert(env.id);
                    Disposition::Copy
                }
            }
            KIND_SIGNAL => {
                let TargetOf { target } = env.body_as().map_err(|_| field())?;
                if self.turns.contains(&target) {
                    Disposition::Copy
                } else {
                    Disposition::Exclude(Exclusion::TargetNotCopied)
                }
            }
            KIND_RECALL_FEEDBACK => {
                let FeedbackOf { answer, target } = env.body_as().map_err(|_| field())?;
                if self.turns.contains(&answer) && self.turns.contains(&target) {
                    Disposition::Copy
                } else {
                    Disposition::Exclude(Exclusion::TargetNotCopied)
                }
            }
            _ => Disposition::Copy,
        };
        Ok(verdict)
    }

    /// ターンの本文が、そのターンのすべての忘れるの伏せ字と合うか (03 §7.3 の退避の写しと同じ判定)。どれかの印で
    /// 当て済みなら合う。当てれば伏せ字にできる (当て直しの前) なら断る。
    fn redactions_match(&self, turn: &Envelope) -> Result<bool, Refusal> {
        let Some(steps) = self.redactions.get(&turn.id) else {
            return Ok(true);
        };
        let Some(steps) = steps else {
            return Ok(false);
        };
        let outcomes: Vec<TurnOutcome> = self
            .markers
            .iter()
            .map(|m| apply_to_turn(turn, steps, m))
            .collect();
        if outcomes.contains(&TurnOutcome::Unchanged) {
            return Ok(true);
        }
        if outcomes
            .iter()
            .any(|o| matches!(o, TurnOutcome::Changed { .. }))
        {
            return Err(Refusal::ForgetPending);
        }
        Ok(false)
    }
}

/// 新しい根の種類の、関係と副索引を作る関数 (種類の名前 → `derive`)。
fn derives() -> &'static BTreeMap<&'static str, Option<DeriveFn>> {
    static DERIVES: OnceLock<BTreeMap<&'static str, Option<DeriveFn>>> = OnceLock::new();
    DERIVES.get_or_init(|| {
        crate::kinds::kinds_v05()
            .into_iter()
            .map(|k| (k.name, k.derive))
            .collect()
    })
}

/// 写す封筒 `env` を書く操作 (封筒と、新しい根の種類の定めが本体から作る関係と副索引。ジャーナルからの取り込みと
/// 同じものになる、03 §6)。
#[must_use]
pub fn copy_ops(env: Envelope) -> Vec<WriteOp> {
    let derived = derives()
        .get(env.kind.as_str())
        .copied()
        .flatten()
        .map(|d| d(&env))
        .unwrap_or_default();
    let mut ops = Vec::with_capacity(1 + derived.len());
    ops.push(WriteOp::Put(env));
    ops.extend(derived);
    ops
}

/// 取り込んだ指示の根拠の位置: v0.4 の事実の出典の範囲 (04 §7.5 の「取り込んだ指示の位置」)。出典が 2 つ以上あれば、
/// 範囲を持つ出典のうちターンの ID の最も新しいものにする (範囲が要るので、範囲の無い出典は使わない。
/// `import.v04_ended` の位置は範囲の有無に関わらず最も新しい出典のターンにするので、決め方が違う)。事実が無い・
/// 範囲を持つ出典が無ければ `None`。
fn source_span(fact: Option<&FactBody>) -> Option<TurnSpan> {
    fact?
        .sources
        .iter()
        .filter_map(|s| {
            Some(TurnSpan {
                turn: s.turn,
                rev: s.rev,
                start: s.start?,
                end: s.end?,
            })
        })
        .max_by_key(|s| s.turn)
}

/// v0.4 の `fact_directive` (`env` と本体 `b`) を、同じ ID の `directive` の封筒に写す (03 §12.4 の「指示の写し方」)。
/// 作成と更新の時刻は元の封筒のまま (書いた版は書き手がこの版にする)。`facts` は v0.4 の事実 (根拠の位置に使う)。
///
/// - `op`・`target`・`redactions`・`hidden`・`turns`・`history`・`redact`・`norm`・`origin`・`marks` は値を変えずに写し、
///   `imported` を真にする。
/// - 主語を entity、属性を辞書の属性 (`aspect`) にする。解く指示 (`private_off`・`unblock`) は v0.5 の形に合わせて
///   どちらも null (相手の指示が持つ)。
/// - `forget` は `vks` をそのまま、`private`・`accept`・`revert` は `vk` を `vks` の 1 要素にし、根拠の位置 (`at`) に
///   v0.4 の事実の出典の範囲を入れる。
///
/// # Errors
/// 本体を CBOR にできないとき (起きない)。
pub fn imported_directive(
    env: &Envelope,
    b: &FactDirectiveBody,
    facts: &BTreeMap<Id, FactBody>,
) -> Result<Envelope, Refusal> {
    let op = b.op.as_str();
    let cancels = op == OP_PRIVATE_OFF || op == OP_UNBLOCK;
    let positioned = op == OP_PRIVATE || op == OP_ACCEPT || op == OP_REVERT;
    let body = DirectiveBody {
        op: b.op.clone(),
        entity: (!cancels).then(|| b.subject.clone()),
        aliases: Vec::new(),
        aspect: (!cancels).then(|| b.attribute.clone()),
        aspect_mac: None,
        vks: if op == OP_FORGET {
            b.vks.clone()
        } else if positioned {
            b.vk.iter().cloned().collect()
        } else {
            Vec::new()
        },
        norm: b.norm,
        at: if positioned {
            source_span(b.fact.and_then(|f| facts.get(&f)))
        } else {
            None
        },
        unit: None,
        target: b.target,
        origin: b.origin.clone(),
        turns: b.turns.clone(),
        redactions: b
            .redactions
            .iter()
            .map(|r| crate::directive::DirectiveRedaction {
                turn: r.turn,
                rev_before: r.rev_before,
                rev_after: r.rev_after,
                start: r.start,
                end: r.end,
                hash: r.hash.clone(),
            })
            .collect(),
        hidden: b
            .hidden
            .iter()
            .map(|h| TurnSpan {
                turn: h.turn,
                rev: h.rev,
                start: h.start,
                end: h.end,
            })
            .collect(),
        history: b.history,
        redact: b.redact,
        scope: if op == OP_PRIVATE {
            b.scope.clone()
        } else {
            None
        },
        turn: None,
        cause: None,
        entity_raw: None,
        imported: true,
        marks: b.marks.clone(),
    };
    let mut out = Envelope::new(env.id, KIND_DIRECTIVE, DIRECTIVE_V, env.created_at, "");
    out.updated_at = env.updated_at;
    out.set_body(&body).map_err(read_error)?;
    Ok(out)
}

/// `import.v04_ended` の形の版。
pub const V04_ENDED_V: u32 = 1;

/// v0.4 で今の値でなかった値の表 (`meta` の `import.v04_ended`、03 §3.1.1・§12.4)。値の文字列は持たない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V04Ended {
    pub v: u32,
    /// 取り込んだターンの区切りの台帳がそろって、表を照らし終えたか (書き換えてよい唯一の項目)。
    pub done: bool,
    pub values: Vec<EndedValue>,
}

/// [`V04Ended`] の 1 つの値。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EndedValue {
    /// v0.4 の事実の主語 (v0.5 の entity)。
    pub entity: String,
    /// v0.4 の事実の属性 (v0.5 の辞書の属性)。
    pub aspect: String,
    /// 値の鍵と照合の鍵の 16 進 (同じ鍵は 1 つ)。
    pub vks: Vec<String>,
    /// 出典の最も新しいターンの ID (`end` の位置)。出典が無ければ `None`。
    pub turn: Option<Id>,
}

/// v0.4 の事実 `facts` のうち `Superseded` か `Retracted` だった値の表を作る (03 §12.4 の「`end` の指示を足す」)。
/// 照合の鍵は値の文字列から `key` で作る (値の文字列の無い `Retracted` の事実は値の鍵だけ)。値の鍵を持たない事実は
/// 表に入れない (`end` を当てられないため)。同じ (entity, 属性, 鍵) は 1 つにまとめ、最も新しいターンを採る。
#[must_use]
pub fn v04_ended(facts: &BTreeMap<Id, FactBody>, key: Option<&ForgetKey>) -> V04Ended {
    let mut merged: BTreeMap<(String, String, Vec<String>), Option<Id>> = BTreeMap::new();
    for f in facts.values() {
        if f.state != "superseded" && f.state != "retracted" {
            continue;
        }
        let Some(vk) = f.vk.as_deref().and_then(ValueKey::from_hex) else {
            continue;
        };
        let mut vks: BTreeSet<String> = BTreeSet::from([vk.to_hex()]);
        if let (Some(k), Some(value)) = (key, f.value.as_deref()) {
            vks.insert(loose_key(k, value).to_hex());
        }
        let turn = f.sources.iter().map(|s| s.turn).max();
        let slot = merged
            .entry((
                f.subject.clone(),
                f.attribute.clone(),
                vks.into_iter().collect(),
            ))
            .or_insert(None);
        *slot = (*slot).max(turn);
    }
    V04Ended {
        v: V04_ENDED_V,
        done: false,
        values: merged
            .into_iter()
            .map(|((entity, aspect, vks), turn)| EndedValue {
                entity,
                aspect,
                vks,
                turn,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use evorag_types::Timestamp;

    use super::*;
    use crate::import_v04::body::SourceBody;
    use crate::value_key::value_key;

    const T: i64 = 1_762_650_000_000;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    fn env(kind: &str, n: u8, body: serde_json::Value) -> Envelope {
        let mut e = Envelope::new(id(n), kind, 1, Timestamp::from_millis(T), "0.4.0");
        e.set_body(&body).expect("a body");
        e
    }

    /// (a) セッションの無いターン・(c) 対象のターンを写さなかった信号と想起への操作を除き、削除の行のある ID も除く
    /// (03 §12.4 の「除くもの」)。
    #[test]
    fn the_selector_excludes_orphans_and_their_signals() {
        let mut s = Selector::new(&[], [id(9)], &["[伏せ字]"]);
        let session = env(KIND_SESSION, 1, serde_json::json!({ "title": "t" }));
        let turn = |n, session| {
            env(
                KIND_TURN,
                n,
                serde_json::json!({ "session": id(session), "text": "x" }),
            )
        };
        assert_eq!(s.judge(&session), Ok(Disposition::Copy));
        assert_eq!(s.judge(&turn(2, 1)), Ok(Disposition::Copy));
        assert_eq!(
            s.judge(&turn(3, 8)),
            Ok(Disposition::Exclude(Exclusion::TurnWithoutSession))
        );
        assert_eq!(
            s.judge(&turn(9, 1)),
            Ok(Disposition::Exclude(Exclusion::Deleted))
        );
        let signal = |n, target| {
            env(
                KIND_SIGNAL,
                n,
                serde_json::json!({ "kind": "good", "target": id(target) }),
            )
        };
        assert_eq!(s.judge(&signal(4, 2)), Ok(Disposition::Copy));
        assert_eq!(
            s.judge(&signal(5, 3)),
            Ok(Disposition::Exclude(Exclusion::TargetNotCopied))
        );
        let feedback = |n, answer, target| {
            env(
                KIND_RECALL_FEEDBACK,
                n,
                serde_json::json!({ "op": "attach", "answer": id(answer), "target": id(target) }),
            )
        };
        assert_eq!(s.judge(&feedback(6, 2, 2)), Ok(Disposition::Copy));
        assert_eq!(
            s.judge(&feedback(7, 2, 3)),
            Ok(Disposition::Exclude(Exclusion::TargetNotCopied))
        );
        // 判定に要る項目を読めない記録は、黙って除かずに断る。
        let bad = env(KIND_TURN, 10, serde_json::json!({ "text": "no session" }));
        assert!(matches!(s.judge(&bad), Err(Refusal::Unreadable { .. })));
    }

    fn fact(state: &str, value: Option<&str>, vk: Option<String>, turns: &[u8]) -> FactBody {
        FactBody {
            subject: "self".into(),
            attribute: "residence".into(),
            value: value.map(str::to_owned),
            vk,
            norm: 1,
            tense: "present".into(),
            valid_from: None,
            valid_to: None,
            state: state.into(),
            against: None,
            by: None,
            state_at: None,
            recorded_at: T,
            private: false,
            sources: turns
                .iter()
                .map(|&t| SourceBody {
                    turn: id(t),
                    rev: 0,
                    start: Some(0),
                    end: Some(3),
                })
                .collect(),
        }
    }

    /// `import.v04_ended` は、`Superseded`・`Retracted` の値の鍵と照合の鍵と、出典の最も新しいターンだけを持つ
    /// (`Live` の値と、値の鍵の無い事実は入れない。同じ鍵はまとめる)。
    #[test]
    fn the_ended_table_keeps_keys_and_the_newest_turn_only() {
        let key = ForgetKey::from_bytes([3; 32]);
        let vk = |v: &str| Some(value_key(&key, v).to_hex());
        let facts = BTreeMap::from([
            (id(1), fact("live", Some("大阪"), vk("大阪"), &[1])),
            (id(2), fact("superseded", Some("東京"), vk("東京"), &[2, 5])),
            (id(3), fact("superseded", Some("東京"), vk("東京"), &[7])),
            (id(4), fact("retracted", None, vk("京都"), &[4])),
            (id(5), fact("retracted", None, None, &[6])),
        ]);
        let t = v04_ended(&facts, Some(&key));
        assert!(!t.done);
        assert_eq!(t.values.len(), 2, "{t:?}");
        let tokyo = t
            .values
            .iter()
            .find(|v| v.vks.contains(&value_key(&key, "東京").to_hex()))
            .expect("東京");
        assert_eq!(tokyo.turn, Some(id(7)));
        assert!(tokyo.vks.contains(&loose_key(&key, "東京").to_hex()));
        let json = serde_json::to_string(&t).expect("json");
        assert!(!json.contains("東京") && !json.contains("大阪") && !json.contains("京都"));
    }

    /// 取り込んだ指示の根拠の位置は、範囲を持つ出典のうちターンの ID の最も新しいもの (04 §7.5)。
    #[test]
    fn an_imported_position_is_the_newest_ranged_source() {
        let mut f = fact("live", Some("猫"), None, &[2, 4]);
        f.sources.push(SourceBody {
            turn: id(6),
            rev: 1,
            start: None,
            end: None,
        });
        let at = source_span(Some(&f)).expect("a span");
        assert_eq!((at.turn, at.rev, at.start, at.end), (id(4), 0, 0, 3));
        assert_eq!(source_span(None), None);
    }
}
