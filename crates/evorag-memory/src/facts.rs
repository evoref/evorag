//! 事実と事実への指示の記録 (04 §5.1・§7.5、03 §3.3)。純粋な核 (`fold`) と保存の形をつなぎ、書き込みの
//! 入口を 1 つにする (04 §5.3 の「書き込みの入口は 1 つ」)。
//!
//! - 本体は最上位を平らにし、状態と `op` は文字列で持つ (知らない値の記録も丸ごと読めなくならないように、
//!   03 §4.2 の規則 7・11)。
//! - 書き込みの入口は、読む → 核で畳む → 前後の差を `WriteOp` にする、だけを行う。書き手のトランザクション
//!   の中で読み直すのは、呼ぶ側 (書き込みの依頼) の仕事にする。

use std::collections::{BTreeMap, BTreeSet};

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, Read, ReadTxn, WriteOp};
use evorag_types::{Id, IdGen, Timestamp};
use serde::{Deserialize, Serialize};

use crate::fold::{
    Directive, DirectiveKind, Directives, Event, Fact, FactSet, FactState, FoldEnv, Report, Scope,
    Source, Tense, fold, refresh_private,
};
use crate::read::MemoryError;
use crate::rules::Attributes;
use crate::value_key::{BlockMark, ForgetKey, NORM_V1, ValueKey};

/// 事実 (04 §5.1、v0.3)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_FACT: &str = "fact";
/// 事実への指示 (04 §7.5、v0.3)。利用者の明示の操作なのでジャーナルに書く。追記だけ。
pub const KIND_FACT_DIRECTIVE: &str = "fact_directive";
/// 関係: 事実 → 出典のターン (03 §3.3)。
pub const REL_FACT_SRC: &str = "fact_src";
/// 関係: 指示 → 解く相手の指示。
pub const REL_CANCELS: &str = "cancels";
/// 副索引: 主語・U+0000・属性 (03 §3.3)。
const LOOKUP_SA: &str = "sa";
/// 副索引: 値の鍵の 16 進。
const LOOKUP_VK: &str = "vk";
/// 副索引: 再び覚えない印の 16 進 (指示だけ)。
const LOOKUP_MARK: &str = "mark";

fn sa_key(subject: &str, attribute: &str) -> String {
    format!("{subject}\u{0}{attribute}")
}

/// 事実の出典 (保存の形)。`Retracted` では `turn` だけを残す。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBody {
    pub turn: Id,
    pub rev: u32,
    pub start: Option<u32>,
    pub end: Option<u32>,
}

/// `fact` の本体 (v1、04 §5.1 の「本体の形」)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactBody {
    pub subject: String,
    pub attribute: String,
    pub value: Option<String>,
    pub vk: Option<String>,
    pub norm: u32,
    pub tense: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub state: String,
    pub against: Option<Id>,
    pub by: Option<Id>,
    pub state_at: Option<i64>,
    pub recorded_at: i64,
    pub private: bool,
    pub sources: Vec<SourceBody>,
}

fn tense_name(t: Tense) -> &'static str {
    match t {
        Tense::Current => "current",
        Tense::Past => "past",
        Tense::Planned => "planned",
    }
}

fn tense_of(s: &str) -> Option<Tense> {
    match s {
        "current" => Some(Tense::Current),
        "past" => Some(Tense::Past),
        "planned" => Some(Tense::Planned),
        _ => None,
    }
}

impl FactBody {
    fn from_fact(f: &Fact, recorded_at: Timestamp) -> Self {
        let (state, against, by, state_at) = match f.state {
            FactState::Live => ("live", None, None, None),
            FactState::Contested { against } => ("contested", Some(against), None, None),
            FactState::Superseded { by, at } => ("superseded", None, by, Some(at.as_millis())),
            FactState::Retracted => ("retracted", None, None, None),
        };
        let retracted = f.state == FactState::Retracted;
        Self {
            subject: f.subject.clone(),
            attribute: f.attribute.clone(),
            value: if retracted { None } else { f.value.clone() },
            vk: if retracted {
                None
            } else {
                f.vk.map(|v| v.to_hex())
            },
            norm: NORM_V1,
            tense: tense_name(f.tense).into(),
            valid_from: None,
            valid_to: None,
            state: state.into(),
            against,
            by,
            state_at,
            recorded_at: recorded_at.as_millis(),
            private: f.private,
            sources: f
                .sources
                .iter()
                .map(|s| SourceBody {
                    turn: s.turn,
                    rev: s.rev,
                    start: (!retracted).then_some(s.start),
                    end: (!retracted).then_some(s.end),
                })
                .collect(),
        }
    }

    /// 核の形にする。知らない状態・時制・正規化の版の記録は `None` (畳みに入れず、書き換えない)。
    fn to_fact(&self, id: Id) -> Option<Fact> {
        if self.norm != NORM_V1 {
            return None;
        }
        let state = match self.state.as_str() {
            "live" => FactState::Live,
            "contested" => FactState::Contested {
                against: self.against?,
            },
            "superseded" => FactState::Superseded {
                by: self.by,
                at: Timestamp::from_millis(self.state_at?),
            },
            "retracted" => FactState::Retracted,
            _ => return None,
        };
        let vk = match &self.vk {
            Some(h) => Some(ValueKey::from_hex(h)?),
            None => None,
        };
        Some(Fact {
            id,
            subject: self.subject.clone(),
            attribute: self.attribute.clone(),
            value: self.value.clone(),
            vk,
            tense: tense_of(&self.tense)?,
            state,
            sources: self
                .sources
                .iter()
                .map(|s| Source {
                    turn: s.turn,
                    start: s.start.unwrap_or(0),
                    end: s.end.unwrap_or(0),
                    rev: s.rev,
                })
                .collect(),
            private: self.private,
        })
    }
}

/// 伏せ字 1 つ (04 §7.5 の忘れるの指示。M3.4 で使う)。範囲は伏せ字の前の本文の UTF-8 のバイト位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionBody {
    pub turn: Id,
    pub rev_before: u32,
    pub rev_after: u32,
    pub start: u32,
    pub end: u32,
    /// 伏せ字の後の本文の SHA-256 (16 進)。
    pub hash: String,
}

/// 伏せ字にしないで忘れたときに、想起で `[非表示]` にする範囲 (04 §7.1。M3.4 で使う)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenBody {
    pub turn: Id,
    pub rev: u32,
    pub start: u32,
    pub end: u32,
}

/// `fact_directive` の本体 (v1、04 §7.5 の「本体の形」)。使わない項目は null か空の並び。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactDirectiveBody {
    pub op: String,
    pub subject: String,
    pub attribute: String,
    pub vk: Option<String>,
    pub norm: Option<u32>,
    /// 操作したときの事実の ID (参考。当てるときは使わない)。
    pub fact: Option<Id>,
    /// 解く相手の指示 (`private_off`・`unblock`)。
    pub target: Option<Id>,
    /// `successors`・`value_only` (`private`)。
    pub scope: Option<String>,
    #[serde(default)]
    pub marks: Vec<String>,
    #[serde(default)]
    pub turns: Vec<Id>,
    #[serde(default)]
    pub redactions: Vec<RedactionBody>,
    #[serde(default)]
    pub hidden: Vec<HiddenBody>,
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub redact: bool,
}

const OP_FORGET: &str = "forget";
const OP_PRIVATE: &str = "private";
const OP_PRIVATE_OFF: &str = "private_off";
const OP_ACCEPT: &str = "accept";
const OP_REVERT: &str = "revert";
const OP_UNBLOCK: &str = "unblock";

impl FactDirectiveBody {
    /// 核の指示の形にする。知らない `op` は印だけを当てる `Unknown` にする (04 §7.5)。
    fn to_directive(&self, id: Id) -> Directive {
        let marks = || -> Vec<BlockMark> {
            self.marks
                .iter()
                .filter_map(|m| BlockMark::from_hex(m))
                .collect()
        };
        let kind = match (self.op.as_str(), self.target) {
            (OP_FORGET, _) => DirectiveKind::Forget { marks: marks() },
            (OP_PRIVATE, _) => DirectiveKind::Private {
                scope: match self.scope.as_deref() {
                    Some("value_only") => Scope::ValueOnly,
                    _ => Scope::Successors,
                },
            },
            (OP_PRIVATE_OFF, Some(target)) => DirectiveKind::PrivateOff { target },
            (OP_ACCEPT, _) => DirectiveKind::Accept,
            (OP_REVERT, _) => DirectiveKind::Revert,
            (OP_UNBLOCK, Some(target)) => DirectiveKind::Unblock { target },
            _ => DirectiveKind::Unknown { marks: marks() },
        };
        Directive {
            id,
            kind,
            subject: self.subject.clone(),
            attribute: self.attribute.clone(),
            vk: self.vk.as_deref().and_then(ValueKey::from_hex),
        }
    }

    fn from_directive(d: &Directive) -> Self {
        let (op, target, scope, marks) = match &d.kind {
            DirectiveKind::Private { scope } => (
                OP_PRIVATE,
                None,
                Some(match scope {
                    Scope::Successors => "successors",
                    Scope::ValueOnly => "value_only",
                }),
                Vec::new(),
            ),
            DirectiveKind::PrivateOff { target } => {
                (OP_PRIVATE_OFF, Some(*target), None, Vec::new())
            }
            DirectiveKind::Accept => (OP_ACCEPT, None, None, Vec::new()),
            DirectiveKind::Revert => (OP_REVERT, None, None, Vec::new()),
            DirectiveKind::Forget { marks } => (OP_FORGET, None, None, marks.clone()),
            DirectiveKind::Unblock { target } => (OP_UNBLOCK, Some(*target), None, Vec::new()),
            DirectiveKind::Unknown { marks } => ("unknown", None, None, marks.clone()),
        };
        Self {
            op: op.into(),
            subject: d.subject.clone(),
            attribute: d.attribute.clone(),
            vk: d.vk.map(|v| v.to_hex()),
            norm: d.vk.map(|_| NORM_V1),
            fact: None,
            target,
            scope: scope.map(Into::into),
            marks: marks.iter().map(BlockMark::to_hex).collect(),
            turns: Vec::new(),
            redactions: Vec::new(),
            hidden: Vec::new(),
            history: false,
            redact: false,
        }
    }
}

/// `fact` の本体のうち、関係と副索引を作るのに要る項目だけ (03 §4.2 の規則 11)。
#[derive(Deserialize)]
struct FactLinks {
    subject: String,
    attribute: String,
    #[serde(default)]
    vk: Option<String>,
    #[serde(default)]
    sources: Vec<SourceTurn>,
}

#[derive(Deserialize)]
struct SourceTurn {
    turn: Id,
}

/// `fact_directive` の本体のうち、関係と副索引を作るのに要る項目だけ。
#[derive(Deserialize)]
struct DirectiveLinks {
    subject: String,
    attribute: String,
    #[serde(default)]
    vk: Option<String>,
    #[serde(default)]
    target: Option<Id>,
    #[serde(default)]
    marks: Vec<String>,
}

fn lookup(kind: &str, field: &str, value: String, id: Id) -> WriteOp {
    WriteOp::PutLookup {
        kind: kind.into(),
        field: field.into(),
        value: OwnedPart::Str(value),
        id,
    }
}

fn unlookup(kind: &str, field: &str, value: String, id: Id) -> WriteOp {
    WriteOp::DeleteLookup {
        kind: kind.into(),
        field: field.into(),
        value: OwnedPart::Str(value),
        id,
    }
}

fn link(from: Id, rel: &str, to: Id) -> WriteOp {
    WriteOp::PutLink {
        from,
        rel: rel.into(),
        to,
        attrs: Vec::new(),
    }
}

/// `fact` の関係と副索引 (ジャーナルの取り込みや repair で、本体から作り直すときにも使う)。
pub(crate) fn derive_fact(e: &Envelope) -> Vec<WriteOp> {
    let Ok(b) = e.body_as::<FactLinks>() else {
        return Vec::new();
    };
    let mut ops = vec![lookup(
        KIND_FACT,
        LOOKUP_SA,
        sa_key(&b.subject, &b.attribute),
        e.id,
    )];
    if let Some(vk) = b.vk {
        ops.push(lookup(KIND_FACT, LOOKUP_VK, vk, e.id));
    }
    let turns: BTreeSet<Id> = b.sources.iter().map(|s| s.turn).collect();
    ops.extend(turns.into_iter().map(|t| link(e.id, REL_FACT_SRC, t)));
    ops
}

/// `fact_directive` の関係と副索引。
pub(crate) fn derive_directive(e: &Envelope) -> Vec<WriteOp> {
    let Ok(b) = e.body_as::<DirectiveLinks>() else {
        return Vec::new();
    };
    let mut ops = vec![lookup(
        KIND_FACT_DIRECTIVE,
        LOOKUP_SA,
        sa_key(&b.subject, &b.attribute),
        e.id,
    )];
    if let Some(vk) = b.vk {
        ops.push(lookup(KIND_FACT_DIRECTIVE, LOOKUP_VK, vk, e.id));
    }
    ops.extend(
        b.marks
            .into_iter()
            .map(|m| lookup(KIND_FACT_DIRECTIVE, LOOKUP_MARK, m, e.id)),
    );
    if let Some(t) = b.target {
        ops.push(link(e.id, REL_CANCELS, t));
    }
    ops
}

/// 読んだ事実。`envelopes` は畳みの前の封筒 (差を作るのに使う)。
#[derive(Debug, Clone, Default)]
pub struct LoadedFacts {
    pub set: FactSet,
    envelopes: BTreeMap<Id, Envelope>,
    /// 知らない状態・形の事実の数 (畳みに入れず、書き換えない。`doctor` が報告する)。
    pub unknown: usize,
}

/// すべての事実を読む (数百件の見込みなので全件を読む、04 §5.4)。
pub fn load_facts(tx: &ReadTxn<'_>) -> Result<LoadedFacts, MemoryError> {
    let mut out = LoadedFacts::default();
    tx.for_each(KIND_FACT, |r| {
        match r {
            Read::Ok(e) => match e.body_as::<FactBody>().ok().and_then(|b| b.to_fact(e.id)) {
                Some(f) => {
                    out.set.facts.push(f);
                    out.envelopes.insert(e.id, e);
                }
                None => out.unknown += 1,
            },
            _ => out.unknown += 1,
        }
        true
    })?;
    Ok(out)
}

/// すべての事実への指示を読む。
pub fn load_directives(tx: &ReadTxn<'_>) -> Result<Directives, MemoryError> {
    let mut list = Vec::new();
    tx.for_each(KIND_FACT_DIRECTIVE, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<FactDirectiveBody>()
        {
            list.push(b.to_directive(e.id));
        }
        true
    })?;
    Ok(Directives::new(list))
}

/// 畳みの前後の差を、書き込みの操作にする。
pub fn diff_ops(
    before: &LoadedFacts,
    after: &FactSet,
    now: Timestamp,
) -> Result<Vec<WriteOp>, BodyError> {
    let mut ops = Vec::new();
    let mut kept = BTreeSet::new();
    for f in &after.facts {
        kept.insert(f.id);
        let old = before.envelopes.get(&f.id);
        let old_fact = before.set.facts.iter().find(|o| o.id == f.id);
        if old_fact == Some(f) {
            continue;
        }
        let recorded_at = old.map_or(now, |e| e.created_at);
        let body = FactBody::from_fact(f, recorded_at);
        let mut env = match old {
            Some(e) => {
                let mut e = e.clone();
                e.updated_at = now;
                e
            }
            None => Envelope::new(f.id, KIND_FACT, 1, now, ""),
        };
        env.set_body(&body)?;
        // 副索引: 値の鍵が変わった (忘れるで消えた) ときだけ付け替える。主語と属性は変わらない。
        let old_vk = old_fact.and_then(|o| o.vk);
        if old_fact.is_none() {
            ops.push(lookup(
                KIND_FACT,
                LOOKUP_SA,
                sa_key(&f.subject, &f.attribute),
                f.id,
            ));
        }
        if old_vk != f.vk {
            if let Some(v) = old_vk {
                ops.push(unlookup(KIND_FACT, LOOKUP_VK, v.to_hex(), f.id));
            }
            if let Some(v) = f.vk.filter(|_| f.state != FactState::Retracted) {
                ops.push(lookup(KIND_FACT, LOOKUP_VK, v.to_hex(), f.id));
            }
        }
        // 関係: 出典のターンの増減。
        let old_turns: BTreeSet<Id> = old_fact
            .map(|o| o.sources.iter().map(|s| s.turn).collect())
            .unwrap_or_default();
        let new_turns: BTreeSet<Id> = f.sources.iter().map(|s| s.turn).collect();
        for t in new_turns.difference(&old_turns) {
            ops.push(link(f.id, REL_FACT_SRC, *t));
        }
        for t in old_turns.difference(&new_turns) {
            ops.push(WriteOp::DeleteLink {
                from: f.id,
                rel: REL_FACT_SRC.into(),
                to: *t,
            });
        }
        ops.push(WriteOp::Put(env));
    }
    for f in &before.set.facts {
        if kept.contains(&f.id) {
            continue;
        }
        // 畳みで消えた事実 (同じ値の矛盾を今の値に合わせた) は、関係と副索引ごと消す。
        ops.push(unlookup(
            KIND_FACT,
            LOOKUP_SA,
            sa_key(&f.subject, &f.attribute),
            f.id,
        ));
        if let Some(v) = f.vk {
            ops.push(unlookup(KIND_FACT, LOOKUP_VK, v.to_hex(), f.id));
        }
        ops.push(WriteOp::DeleteLinksOf(f.id));
        ops.push(WriteOp::Delete {
            kind: KIND_FACT.into(),
            id: f.id,
        });
    }
    Ok(ops)
}

/// 書き込みの入口の環境。
pub struct FactCtx<'a> {
    pub key: &'a ForgetKey,
    pub attributes: &'a Attributes,
    pub ids: &'a dyn IdGen,
    pub now: Timestamp,
}

impl FactCtx<'_> {
    fn fold(&self, set: &mut FactSet, dirs: &Directives, events: Vec<Event>) -> Report {
        let ids = self.ids;
        let mut new_id = || ids.next_id();
        let mut env = FoldEnv {
            key: self.key,
            attributes: self.attributes,
            new_id: &mut new_id,
        };
        fold(set, dirs, events, &mut env)
    }
}

/// 書き込みの入口: 候補と検証済みの訂正を畳み、書き込みの操作を返す (04 §5.3)。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn consolidate(
    tx: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    events: Vec<Event>,
) -> Result<(Vec<WriteOp>, Report), MemoryError> {
    let before = load_facts(tx)?;
    let dirs = load_directives(tx)?;
    let mut set = before.set.clone();
    let report = ctx.fold(&mut set, &dirs, events);
    Ok((diff_ops(&before, &set, ctx.now)?, report))
}

/// 事実を作り直す (`rebuild --facts`、03 §10): 事実を捨て、候補と訂正と、採る・戻すを当て直す。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn rebuild(
    tx: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    mut events: Vec<Event>,
) -> Result<(Vec<WriteOp>, Report), MemoryError> {
    let before = load_facts(tx)?;
    let dirs = load_directives(tx)?;
    events.extend(dirs.choices());
    let mut set = FactSet::default();
    let report = ctx.fold(&mut set, &dirs, events);
    Ok((diff_ops(&before, &set, ctx.now)?, report))
}

/// 事実への指示を足し、その効き目を事実に当てた書き込みの操作を返す (04 §7.5)。
/// 採る・戻すは指示の時点で 1 回当て、private とその解除は事実の写しを計算し直す。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn add_directive(
    tx: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    kind: DirectiveKind,
    subject: &str,
    attribute: &str,
    vk: Option<ValueKey>,
) -> Result<(Id, Vec<WriteOp>), MemoryError> {
    let id = ctx.ids.next_id();
    let d = Directive {
        id,
        kind,
        subject: subject.to_owned(),
        attribute: attribute.to_owned(),
        vk,
    };
    let mut env = Envelope::new(id, KIND_FACT_DIRECTIVE, 1, ctx.now, "");
    env.set_body(&FactDirectiveBody::from_directive(&d))?;
    let mut ops = derive_directive(&env);
    ops.insert(0, WriteOp::Put(env));

    let before = load_facts(tx)?;
    let mut dirs = load_directives(tx)?;
    dirs.push(d);
    let mut set = before.set.clone();
    let events: Vec<Event> = dirs
        .choices()
        .into_iter()
        .filter(|e| is_choice_of(e, id))
        .collect();
    ctx.fold(&mut set, &dirs, events);
    refresh_private(&mut set, &dirs);
    ops.extend(diff_ops(&before, &set, ctx.now)?);
    Ok((id, ops))
}

fn is_choice_of(e: &Event, id: Id) -> bool {
    matches!(e, Event::Choose { directive, .. } if *directive == id)
}
