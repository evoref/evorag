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

use crate::extract::turn_rev;
use crate::fold::{
    Choice, Directive, DirectiveKind, Directives, Dropped, Event, Fact, FactSet, FactState,
    FoldEnv, Report, Scope, Source, Tense, drop_sources, fold,
};
use crate::gates::{Safety, check_subject, check_text, check_value};
use crate::kinds::KIND_TURN;
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
    /// 指示を足した経路 (04 §7.5): `user` (利用者の操作) か `delete` (セッションの削除・規則 12 の掃除で足した、
    /// その値だけの private)。無い記録と知らない値は `user` として読む ([`FactDirectiveBody::origin`])。
    #[serde(default = "origin_user")]
    pub origin: String,
}

/// 指示の経路: 利用者の操作 (既定)。
pub const ORIGIN_USER: &str = "user";
/// 指示の経路: セッションの削除・出典の無い事実の掃除で足した (04 §7.5 の「後継」)。
pub const ORIGIN_DELETE: &str = "delete";

fn origin_user() -> String {
    ORIGIN_USER.to_owned()
}

/// 指示を足した経路 (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// 利用者の操作。`origin` の無い記録と知らない値も、こちらとして読む。
    User,
    /// セッションの削除・出典の無い事実の掃除で足した。
    Delete,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::User => ORIGIN_USER,
            Origin::Delete => ORIGIN_DELETE,
        }
    }
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
            // 正規化の版は、値の鍵や印を持たない指示にも書く (04 §5.6)。
            norm: Some(NORM_V1),
            fact: None,
            target,
            scope: scope.map(Into::into),
            marks: marks.iter().map(BlockMark::to_hex).collect(),
            turns: Vec::new(),
            redactions: Vec::new(),
            hidden: Vec::new(),
            history: false,
            redact: false,
            origin: origin_user(),
        }
    }

    /// 指示を足した経路。知らない値は利用者の操作として読む。
    #[must_use]
    pub fn origin(&self) -> Origin {
        if self.origin == ORIGIN_DELETE {
            Origin::Delete
        } else {
            Origin::User
        }
    }
}

// 関係と副索引を作るのに要る項目は、1 つずつ別の型で読む (03 §4.2 の規則 11)。新しい版でほかの項目の
// 型が変わっても、読める項目の関係と副索引 (特に再び覚えない印の `mark`) を失わないようにする。

#[derive(Deserialize)]
struct SubjectAttribute {
    subject: String,
    attribute: String,
}

#[derive(Deserialize)]
struct VkField {
    #[serde(default)]
    vk: Option<String>,
}

#[derive(Deserialize)]
struct SourcesField {
    #[serde(default)]
    sources: Vec<SourceTurn>,
}

#[derive(Deserialize)]
struct SourceTurn {
    turn: Id,
}

#[derive(Deserialize)]
struct TargetField {
    #[serde(default)]
    target: Option<Id>,
}

#[derive(Deserialize)]
struct MarksField {
    #[serde(default)]
    marks: Vec<String>,
}

/// 記録から作る関係と副索引の 1 項目。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IndexEntry {
    Lookup { field: &'static str, value: String },
    Link { rel: &'static str, to: Id },
}

impl IndexEntry {
    fn put(&self, kind: &str, id: Id) -> WriteOp {
        match self {
            IndexEntry::Lookup { field, value } => WriteOp::PutLookup {
                kind: kind.into(),
                field: (*field).into(),
                value: OwnedPart::Str(value.clone()),
                id,
            },
            IndexEntry::Link { rel, to } => WriteOp::PutLink {
                from: id,
                rel: (*rel).into(),
                to: *to,
                attrs: Vec::new(),
            },
        }
    }

    fn delete(&self, kind: &str, id: Id) -> WriteOp {
        match self {
            IndexEntry::Lookup { field, value } => WriteOp::DeleteLookup {
                kind: kind.into(),
                field: (*field).into(),
                value: OwnedPart::Str(value.clone()),
                id,
            },
            IndexEntry::Link { rel, to } => WriteOp::DeleteLink {
                from: id,
                rel: (*rel).into(),
                to: *to,
            },
        }
    }
}

/// `fact` の関係と副索引の集合 (本体から決まる)。
pub(crate) fn fact_entries(e: &Envelope) -> BTreeSet<IndexEntry> {
    let mut out = BTreeSet::new();
    if let Ok(b) = e.body_as::<SubjectAttribute>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_SA,
            value: sa_key(&b.subject, &b.attribute),
        });
    }
    if let Ok(VkField { vk: Some(vk) }) = e.body_as::<VkField>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_VK,
            value: vk,
        });
    }
    if let Ok(b) = e.body_as::<SourcesField>() {
        out.extend(b.sources.into_iter().map(|s| IndexEntry::Link {
            rel: REL_FACT_SRC,
            to: s.turn,
        }));
    }
    out
}

/// `fact_directive` の関係と副索引の集合。
fn directive_entries(e: &Envelope) -> BTreeSet<IndexEntry> {
    let mut out = BTreeSet::new();
    if let Ok(b) = e.body_as::<SubjectAttribute>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_SA,
            value: sa_key(&b.subject, &b.attribute),
        });
    }
    if let Ok(VkField { vk: Some(vk) }) = e.body_as::<VkField>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_VK,
            value: vk,
        });
    }
    if let Ok(b) = e.body_as::<MarksField>() {
        out.extend(b.marks.into_iter().map(|m| IndexEntry::Lookup {
            field: LOOKUP_MARK,
            value: m,
        }));
    }
    if let Ok(TargetField { target: Some(t) }) = e.body_as::<TargetField>() {
        out.insert(IndexEntry::Link {
            rel: REL_CANCELS,
            to: t,
        });
    }
    out
}

/// `fact` の関係と副索引 (ジャーナルの取り込みや repair で、本体から作り直すときにも使う)。
pub(crate) fn derive_fact(e: &Envelope) -> Vec<WriteOp> {
    fact_entries(e)
        .iter()
        .map(|x| x.put(KIND_FACT, e.id))
        .collect()
}

/// `fact_directive` の関係と副索引。
pub(crate) fn derive_directive(e: &Envelope) -> Vec<WriteOp> {
    directive_entries(e)
        .iter()
        .map(|x| x.put(KIND_FACT_DIRECTIVE, e.id))
        .collect()
}
/// `fact` の本体の版 (03 §3.3)。これより新しい `v` の記録は読むだけにし、書き換えない (03 §4.2 の規則 6)。
pub(crate) const FACT_V: u32 = 1;
/// `fact_directive` の本体の版。
pub(crate) const FACT_DIRECTIVE_V: u32 = 1;

/// 読んだ事実。`envelopes` は畳みの前の封筒 (差を作るのに使う)。
#[derive(Debug, Clone, Default)]
pub struct LoadedFacts {
    pub set: FactSet,
    envelopes: BTreeMap<Id, Envelope>,
    /// 新しい `v`・知らない状態・形の事実の数 (畳みに入れず、書き換えも削除もしない。`doctor` が報告する)。
    pub unknown: usize,
}

/// 封筒を核の事実にする。新しい `v`・知らない状態・正規化の版・形の事実は `None`。
fn fact_of(e: &Envelope) -> Option<Fact> {
    if e.v != FACT_V {
        return None;
    }
    e.body_as::<FactBody>().ok()?.to_fact(e.id)
}

/// すべての事実を読む (数百件の見込みなので全件を読む、04 §5.4)。
pub fn load_facts(tx: &ReadTxn<'_>) -> Result<LoadedFacts, MemoryError> {
    let mut out = LoadedFacts::default();
    tx.for_each(KIND_FACT, |r| {
        match r {
            Read::Ok(e) => match fact_of(&e) {
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

/// 事実を 1 件読む (想起で、事実の検索の鍵が当たったときに、今の状態を確かめる、05 §3.2)。無い・読めない・
/// 新しい `v`・知らない状態の事実は `None`。
///
/// # Errors
/// 記録を読めないとき。
pub fn load_fact(tx: &ReadTxn<'_>, id: Id) -> Result<Option<Fact>, MemoryError> {
    Ok(match tx.get(KIND_FACT, id)? {
        Some(Read::Ok(e)) => fact_of(&e),
        _ => None,
    })
}

/// 封筒を核の指示にする。読めない指示・新しい `v`・知らない正規化の版の指示は、印だけを当てる
/// `Unknown` にする (忘れた値を蘇らせないため、04 §7.5 の「知らない op の読み方」)。
fn directive_of(e: &Envelope) -> Directive {
    let readable = (e.v == FACT_DIRECTIVE_V)
        .then(|| e.body_as::<FactDirectiveBody>().ok())
        .flatten()
        .filter(|b| b.norm.is_none_or(|n| n == NORM_V1));
    if let Some(b) = readable {
        return b.to_directive(e.id);
    }
    let marks = e
        .body_as::<MarksField>()
        .map(|m| {
            m.marks
                .iter()
                .filter_map(|h| BlockMark::from_hex(h))
                .collect()
        })
        .unwrap_or_default();
    let (subject, attribute) = e
        .body_as::<SubjectAttribute>()
        .map(|b| (b.subject, b.attribute))
        .unwrap_or_default();
    Directive {
        id: e.id,
        kind: DirectiveKind::Unknown { marks },
        subject,
        attribute,
        vk: None,
    }
}

/// すべての事実への指示を読む。読めない指示も、印だけは当てる (`Directives::unknown_count` で数える)。
pub fn load_directives(tx: &ReadTxn<'_>) -> Result<Directives, MemoryError> {
    let mut list = Vec::new();
    tx.for_each(KIND_FACT_DIRECTIVE, |r| {
        // 封筒そのものが読めない指示は、印も読めない。`doctor` が quarantine として報告する。
        if let Read::Ok(e) = r {
            list.push(directive_of(&e));
        }
        true
    })?;
    Ok(Directives::new(list))
}

/// 畳みの前後の差を、書き込みの操作にする。関係と副索引は、前後の封筒から作る集合の差で付け替える
/// (repair でジャーナルや本体から作り直したものと、常に同じになるように)。
pub fn diff_ops(
    before: &LoadedFacts,
    after: &FactSet,
    now: Timestamp,
) -> Result<Vec<WriteOp>, BodyError> {
    let old_facts: BTreeMap<Id, &Fact> = before.set.facts.iter().map(|f| (f.id, f)).collect();
    let mut ops = Vec::new();
    let mut kept = BTreeSet::new();
    for f in &after.facts {
        kept.insert(f.id);
        if old_facts.get(&f.id) == Some(&f) {
            continue;
        }
        let old = before.envelopes.get(&f.id);
        // 核が決めない既知の項目 (現実の期間・記録した時刻) は、古い本体のものを残す。
        let old_body = old.and_then(|e| e.body_as::<FactBody>().ok());
        let mut body = FactBody::from_fact(f, now);
        if let Some(ob) = old_body {
            body.valid_from = ob.valid_from;
            body.valid_to = ob.valid_to;
            body.recorded_at = ob.recorded_at;
        }
        let mut env = match old {
            Some(e) => {
                let mut e = e.clone();
                e.updated_at = now;
                e
            }
            None => Envelope::new(f.id, KIND_FACT, FACT_V, now, ""),
        };
        env.set_body(&body)?;
        let before_entries = old.map(fact_entries).unwrap_or_default();
        let after_entries = fact_entries(&env);
        ops.extend(
            before_entries
                .difference(&after_entries)
                .map(|x| x.delete(KIND_FACT, f.id)),
        );
        ops.extend(
            after_entries
                .difference(&before_entries)
                .map(|x| x.put(KIND_FACT, f.id)),
        );
        ops.push(WriteOp::Put(env));
    }
    for f in &before.set.facts {
        if kept.contains(&f.id) {
            continue;
        }
        // 畳みで消えた事実 (同じ値の矛盾を今の値に合わせた) は、関係と副索引ごと消す。
        if let Some(e) = before.envelopes.get(&f.id) {
            ops.extend(fact_entries(e).iter().map(|x| x.delete(KIND_FACT, f.id)));
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
    /// 関門 6 の安全の規則 (書き込みの入口で、候補の値に当て直す)。
    pub safety: &'a Safety<'a>,
    pub ids: &'a dyn IdGen,
    pub now: Timestamp,
}

impl FactCtx<'_> {
    /// 書き込みの入口の検査 (04 §5.2.1 の「書き込みの入口での検査」): 出典の改訂の番号が今のターンの改訂の
    /// 番号と合わない (ターンが無い・読めないものを含む) 候補と訂正を捨て、候補の値に関門 6・7 を、主語に関門 5
    /// (根拠を見ない部分) と関門 6 を、検証済みの訂正の新しい値に関門 6 を当て直す。一覧に無い属性の候補は、畳みが数えて捨てるので、ここでは残す。
    fn screen(
        &self,
        tx: &ReadTxn<'_>,
        events: Vec<Event>,
        report: &mut Report,
    ) -> Result<Vec<Event>, MemoryError> {
        let mut revs: BTreeMap<Id, Option<u32>> = BTreeMap::new();
        let mut current = |turn: Id| -> Result<Option<u32>, MemoryError> {
            if let Some(r) = revs.get(&turn) {
                return Ok(*r);
            }
            let r = match tx.get(KIND_TURN, turn)? {
                Some(Read::Ok(e)) => Some(turn_rev(&e)),
                _ => None,
            };
            revs.insert(turn, r);
            Ok(r)
        };
        let mut out = Vec::with_capacity(events.len());
        for e in events {
            let source = match &e {
                Event::Candidate(c) => Some(c.source),
                Event::Correction(c) => Some(*c.source()),
                Event::Choose { .. } => None,
            };
            if let Some(src) = source
                && current(src.turn)? != Some(src.rev)
            {
                report.stale_source += 1;
                continue;
            }
            if let Event::Candidate(c) = &e
                && let Some(attribute) = self.attributes.get(&c.attribute)
            {
                if check_value(self.safety, attribute, &c.value).is_err() {
                    report.value_rejected += 1;
                    continue;
                }
                if check_subject(self.safety, attribute, &c.subject).is_err() {
                    report.subject_rejected += 1;
                    continue;
                }
            }
            // 検証済みの訂正の新しい値には関門 6 を当てる (関門 7 は当てる属性が決まってから畳みが当てる)。
            if let Event::Correction(c) = &e
                && check_text(self.safety, c.new_value()).is_err()
            {
                report.value_rejected += 1;
                continue;
            }
            out.push(e);
        }
        Ok(out)
    }

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

    /// 入口の検査を当ててから畳む。数えたものは 1 つの報告にまとめる。
    fn screen_and_fold(
        &self,
        tx: &ReadTxn<'_>,
        set: &mut FactSet,
        dirs: &Directives,
        events: Vec<Event>,
    ) -> Result<Report, MemoryError> {
        let mut screened = Report::default();
        let events = self.screen(tx, events, &mut screened)?;
        let mut report = self.fold(set, dirs, events);
        report.stale_source = screened.stale_source;
        report.value_rejected += screened.value_rejected;
        report.subject_rejected = screened.subject_rejected;
        Ok(report)
    }
}

/// 事実の表に入りうる状態 (`Live` で private でない) だった事実のうち、`after` で消えた・`Live` でなくなった・
/// private になった・値が変わったもの (事実の ID の順、`Report::withdrawn`)。
fn withdrawn(before: &FactSet, after: &FactSet) -> Vec<Id> {
    let shown = |f: &Fact| !f.private && matches!(f.state, FactState::Live);
    let now: BTreeMap<Id, &Fact> = after.facts.iter().map(|f| (f.id, f)).collect();
    let mut out: Vec<Id> = before
        .facts
        .iter()
        .filter(|f| shown(f))
        .filter(|f| {
            now.get(&f.id).is_none_or(|g| {
                !shown(g) || g.value != f.value || g.subject != f.subject || g.tense != f.tense
            })
        })
        .map(|f| f.id)
        .collect();
    out.sort_unstable();
    out
}

/// 書き込みの入口: 候補と検証済みの訂正を畳み、書き込みの操作を返す (04 §5.3)。
///
/// 畳む前に、出典の改訂の番号が今のターンと合わないものを捨て、候補の値に関門 6・7 を当て直す
/// (04 §5.2.1 の「書き込みの入口での検査」。数は `Report::stale_source`・`value_rejected`)。
///
/// 読んでから書くまでの間に別の書き込み (利用者の指示など) が入ると、古い状態からの差を書いてしまう。
/// 呼ぶ側は、事実を書く経路を 1 つのロックで直列にし、`tx` を開く前から書き終えるまで放さない
/// (04 §5.2.1 の「書き込みの直列化」)。
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
    let mut report = ctx.screen_and_fold(tx, &mut set, &dirs, events)?;
    report.withdrawn = withdrawn(&before.set, &set);
    Ok((diff_ops(&before, &set, ctx.now)?, report))
}

/// 事実を作り直す (`rebuild --facts`、03 §10): 事実を捨て、候補と訂正と、採る・戻すを当て直す。
/// 読めない事実は畳みに入らないので、消さずに残る。候補と訂正には `consolidate` と同じ入口の検査を当てる。
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
    let mut report = ctx.screen_and_fold(tx, &mut set, &dirs, events)?;
    report.withdrawn = withdrawn(&before.set, &set);
    Ok((diff_ops(&before, &set, ctx.now)?, report))
}

/// 出典のターンが消えたときの書き込み ([`drop_turns`])。
#[derive(Debug, Clone, Default)]
pub struct SourcesDropped {
    /// 書き込みの操作 (事実の記録と、その関係・副索引)。
    pub ops: Vec<WriteOp>,
    /// 消した事実 (ID の順。事実の検索の鍵の ID も同じ、04 §5.5)。読めない事実で、出典がすべて無くなった
    /// ものも含む。
    pub deleted: Vec<Id>,
    /// 核で変えたもの。
    pub dropped: Dropped,
}

/// 出典のターンが消えたときの書き込みの操作を返す (セッションの削除 04 §7.1、出典の無い事実の掃除
/// 04 §5.3・03 §4.2 の規則 12)。`gone` は、消えた (消す) ターンなら `true` を返す。
///
/// 畳みの核 ([`drop_sources`]) で出典を外し、出典の無くなった事実を消し (置き換えの連鎖は 04 §7.4)、前後の
/// 差を [`diff_ops`] で操作にする。新しい `v`・知らない状態の事実は書き換えず、出典 (本体の `sources` の
/// `turn`) がすべて消えたときだけ、本体を解釈せずに消す (03 §4.2 の規則 6 の「忘れる・削除」)。
/// 事実への指示は消さない (03 §3.3)。出典が外れて private でなくなる事実には、その値だけの private の指示
/// (ID は `ids` から) を同じ操作の列で足す (04 §7.5 の「後継」。指示はジャーナルにも書かれる)。
///
/// 呼ぶ側は、[`consolidate`] と同じく、事実を書く経路のロックを `tx` を開く前から書き終えるまで持つ。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn drop_turns(
    tx: &ReadTxn<'_>,
    gone: &dyn Fn(Id) -> bool,
    ids: &dyn IdGen,
    now: Timestamp,
) -> Result<SourcesDropped, MemoryError> {
    let before = load_facts(tx)?;
    let mut dirs = load_directives(tx)?;
    let mut set = before.set.clone();
    let mut new_id = || ids.next_id();
    let dropped = drop_sources(
        &mut set,
        &mut dirs,
        crate::rules::attributes(),
        gone,
        &mut new_id,
    );
    let mut ops = Vec::new();
    // 削除で足す指示は `origin = "delete"` で書く (`/memory` で「削除で足した指示」と見分ける、
    // [`kept_after_delete`])。
    for d in &dropped.directives {
        ops.extend(directive_ops(d, None, Origin::Delete, now)?);
    }
    ops.extend(diff_ops(&before, &set, now)?);
    let mut deleted = dropped.deleted.clone();
    if before.unknown > 0 {
        tx.for_each(KIND_FACT, |r| {
            if let Read::Ok(e) = r
                && !before.envelopes.contains_key(&e.id)
                && let Ok(SourcesField { sources }) = e.body_as::<SourcesField>()
                && !sources.is_empty()
                && sources.iter().all(|s| gone(s.turn))
            {
                ops.extend(fact_entries(&e).iter().map(|x| x.delete(KIND_FACT, e.id)));
                ops.push(WriteOp::DeleteLinksOf(e.id));
                ops.push(WriteOp::Delete {
                    kind: KIND_FACT.into(),
                    id: e.id,
                });
                deleted.push(e.id);
            }
            true
        })?;
        deleted.sort_unstable();
    }
    Ok(SourcesDropped {
        ops,
        deleted,
        dropped,
    })
}

/// 指示の記録を足す操作 (本体と、その関係・副索引)。種類 `fact_directive` はジャーナルにも書かれる。
/// `fact` は操作したときの事実の ID (参考。当てるときは使わない、04 §7.5)、`origin` は指示を足した経路。
fn directive_ops(
    d: &Directive,
    fact: Option<Id>,
    origin: Origin,
    now: Timestamp,
) -> Result<Vec<WriteOp>, BodyError> {
    let mut env = Envelope::new(d.id, KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V, now, "");
    let mut body = FactDirectiveBody::from_directive(d);
    body.fact = fact;
    body.origin = origin.as_str().to_owned();
    env.set_body(&body)?;
    let mut ops = derive_directive(&env);
    ops.insert(0, WriteOp::Put(env));
    Ok(ops)
}

/// 足す事実への指示 1 つ ([`add_directives`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDirective {
    pub kind: DirectiveKind,
    pub subject: String,
    pub attribute: String,
    pub vk: Option<ValueKey>,
    /// 操作したときの事実の ID (参考として本体の `fact` に書く。当てるときは使わない、04 §7.5)。
    pub fact: Option<Id>,
}

/// 事実への指示を足した書き込み ([`add_directives`])。
#[derive(Debug, Clone, Default)]
pub struct DirectivesAdded {
    /// 足した指示の ID (渡した順)。
    pub ids: Vec<Id>,
    /// 書き込みの操作 (指示の記録と、効き目を当てた事実の差)。
    pub ops: Vec<WriteOp>,
    /// この操作で private になった事実 (ID の順)。呼ぶ側は、コミットの直後に (利用者に知らせる前に)、その
    /// 検索の鍵を索引から除く (04 §5.5)。
    pub hidden: Vec<Id>,
}

/// 事実への指示を足し、その効き目を事実に当てた書き込みの操作を返す (04 §7.5)。
/// 採る・戻すは指示の時点で 1 回当て、private とその解除と印を解くは常に効く制約として計算し直す。
/// 指示は 1 つの読み取りから続けて畳むので、`/private off` で複数の指示を解くときも 1 回で渡す。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。足せない指示 (忘れる・知らない指示、値の鍵の無い採る・戻す、相手の
/// 無い解除) は `UnsupportedDirective`。
pub fn add_directives(
    tx: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    list: Vec<NewDirective>,
) -> Result<DirectivesAdded, MemoryError> {
    let before = load_facts(tx)?;
    let mut dirs = load_directives(tx)?;
    let mut out = DirectivesAdded::default();
    let mut events: Vec<Event> = Vec::new();
    for n in list {
        let choice = match n.kind {
            DirectiveKind::Private { .. }
            | DirectiveKind::PrivateOff { .. }
            | DirectiveKind::Unblock { .. } => None,
            DirectiveKind::Accept => Some(Choice::Accept),
            DirectiveKind::Revert => Some(Choice::Revert),
            DirectiveKind::Forget { .. } => {
                return Err(MemoryError::UnsupportedDirective(
                    "forget comes with redaction (apply_forget)",
                ));
            }
            DirectiveKind::Unknown { .. } => {
                return Err(MemoryError::UnsupportedDirective(
                    "unknown directives are read, never written",
                ));
            }
        };
        let id = ctx.ids.next_id();
        if let Some(choice) = choice {
            let Some(vk) = n.vk else {
                return Err(MemoryError::UnsupportedDirective(
                    "accept and revert need a value key",
                ));
            };
            events.push(Event::Choose {
                directive: id,
                subject: n.subject.clone(),
                attribute: n.attribute.clone(),
                vk,
                choice,
            });
        }
        let d = Directive {
            id,
            kind: n.kind,
            subject: n.subject,
            attribute: n.attribute,
            vk: n.vk,
        };
        out.ops
            .extend(directive_ops(&d, n.fact, Origin::User, ctx.now)?);
        out.ids.push(id);
        dirs.push(d);
    }
    let mut set = before.set.clone();
    // fold は最後に private を計算し直す (出来事が無くても)。
    ctx.fold(&mut set, &dirs, events);
    let was_private: BTreeSet<Id> = before
        .set
        .facts
        .iter()
        .filter(|f| f.private)
        .map(|f| f.id)
        .collect();
    out.hidden = set
        .facts
        .iter()
        .filter(|f| f.private && !was_private.contains(&f.id))
        .map(|f| f.id)
        .collect();
    out.hidden.sort_unstable();
    out.ops.extend(diff_ops(&before, &set, ctx.now)?);
    Ok(out)
}

/// 事実への指示を 1 つ足す ([`add_directives`])。足した指示の ID と書き込みの操作を返す。
///
/// # Errors
/// [`add_directives`] と同じ。
pub fn add_directive(
    tx: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    kind: DirectiveKind,
    subject: &str,
    attribute: &str,
    vk: Option<ValueKey>,
) -> Result<(Id, Vec<WriteOp>), MemoryError> {
    let added = add_directives(
        tx,
        ctx,
        vec![NewDirective {
            kind,
            subject: subject.to_owned(),
            attribute: attribute.to_owned(),
            vk,
            fact: None,
        }],
    )?;
    Ok((added.ids[0], added.ops))
}

/// 読める指示の本体を、ID とともに順に渡す (`v` が新しい・形の違う・正規化の版の違う指示は飛ばす)。
fn for_each_directive_body(
    tx: &ReadTxn<'_>,
    mut f: impl FnMut(Id, FactDirectiveBody),
) -> Result<(), MemoryError> {
    tx.for_each(KIND_FACT_DIRECTIVE, |r| {
        if let Read::Ok(e) = r
            && e.v == FACT_DIRECTIVE_V
            && let Ok(b) = e.body_as::<FactDirectiveBody>()
            && b.norm.is_none_or(|n| n == NORM_V1)
        {
            f(e.id, b);
        }
        true
    })?;
    Ok(())
}

/// セッションの削除・出典の無い事実の掃除で足した private の指示 (`origin = "delete"`、[`drop_turns`])。
/// `/memory` の private の理由の表示で、利用者が選んだ値と見分ける (04 §7.5 の「private の理由の表示」)。
///
/// # Errors
/// 記録を読めないとき。
pub fn kept_after_delete(tx: &ReadTxn<'_>) -> Result<BTreeSet<Id>, MemoryError> {
    let mut out = BTreeSet::new();
    for_each_directive_body(tx, |id, b| {
        if b.op == OP_PRIVATE && b.origin() == Origin::Delete {
            out.insert(id);
        }
    })?;
    Ok(out)
}

/// 伏せ字にしないで忘れたときに、想起で `[非表示]` にする範囲 (忘れるの指示の `hidden`、04 §7.1)。
/// 印を解いても範囲は効いたままにする (印を解くは関門 8 だけを変える)。
///
/// # Errors
/// 記録を読めないとき。
pub fn load_hidden(tx: &ReadTxn<'_>) -> Result<Vec<HiddenBody>, MemoryError> {
    let mut out = Vec::new();
    for_each_directive_body(tx, |_, b| {
        if b.op == OP_FORGET {
            out.extend(b.hidden);
        }
    })?;
    Ok(out)
}

/// 忘れるの指示 1 つ (`/memory forgotten` の材料。値は持たない、04 §7.6)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetRecord {
    pub id: Id,
    pub subject: String,
    pub attribute: String,
    /// 再び覚えない印の数 (忘れた値の数)。
    pub marks: usize,
    /// 忘れた事実の出典のターン。
    pub turns: Vec<Id>,
    /// 出典の発話を伏せ字にしたか。
    pub redact: bool,
    /// 印を解く指示で解かれた。
    pub unblocked: bool,
}

/// 忘れるの指示を、指示の ID の順に読む。
///
/// # Errors
/// 記録を読めないとき。
pub fn load_forgets(tx: &ReadTxn<'_>) -> Result<Vec<ForgetRecord>, MemoryError> {
    let dirs = load_directives(tx)?;
    let mut out = Vec::new();
    for_each_directive_body(tx, |id, b| {
        if b.op == OP_FORGET {
            out.push(ForgetRecord {
                id,
                subject: b.subject,
                attribute: b.attribute,
                marks: b.marks.len(),
                turns: b.turns,
                redact: b.redact,
                unblocked: dirs.is_unblocked(id),
            });
        }
    })?;
    out.sort_by_key(|r| r.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evorag_types::Timestamp;

    fn envelope(kind: &str, v: u32, body: &impl Serialize) -> Envelope {
        let mut e = Envelope::new(
            Id::from_bytes([1; 16]),
            kind,
            v,
            Timestamp::from_millis(1_762_650_000_000),
            "",
        );
        e.set_body(body).expect("body");
        e
    }

    fn fact_body(state: &str) -> FactBody {
        FactBody {
            subject: "self".into(),
            attribute: "residence".into(),
            value: Some("町".into()),
            vk: Some("ab".repeat(32)),
            norm: NORM_V1,
            tense: "current".into(),
            valid_from: None,
            valid_to: None,
            state: state.into(),
            against: None,
            by: None,
            state_at: None,
            recorded_at: 0,
            private: false,
            sources: Vec::new(),
        }
    }

    #[test]
    fn newer_unknown_or_other_norm_facts_stay_out_of_the_fold() {
        assert!(fact_of(&envelope(KIND_FACT, FACT_V, &fact_body("live"))).is_some());
        assert!(fact_of(&envelope(KIND_FACT, FACT_V + 1, &fact_body("live"))).is_none());
        assert!(fact_of(&envelope(KIND_FACT, FACT_V, &fact_body("someday"))).is_none());
        let mut b = fact_body("live");
        b.norm = 2;
        assert!(fact_of(&envelope(KIND_FACT, FACT_V, &b)).is_none());
    }

    #[test]
    fn unreadable_or_newer_directives_still_block_with_their_marks() {
        let mark = "cd".repeat(32);
        // 新しい版で、ほかの項目の型が変わった忘れるの指示 (redactions が文字列になった)。
        let odd = serde_json::json!({
            "op": "forget", "subject": "self", "attribute": "hobby",
            "marks": [mark], "redactions": "changed shape",
        });
        let e = envelope(KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V, &odd);
        let d = directive_of(&e);
        assert!(matches!(&d.kind, DirectiveKind::Unknown { marks } if marks.len() == 1));
        // 新しい v の指示も、印だけを当てる。
        let newer = envelope(KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V + 1, &odd);
        assert!(matches!(
            directive_of(&newer).kind,
            DirectiveKind::Unknown { .. }
        ));
        // 関係と副索引も、読める項目からは作る (印の副索引を失わない)。
        let entries = directive_entries(&e);
        assert!(entries.contains(&IndexEntry::Lookup {
            field: LOOKUP_MARK,
            value: "cd".repeat(32),
        }));
        // 別の正規化の版の private は、v1 の鍵で照合しない。
        let p = serde_json::json!({
            "op": "private", "subject": "self", "attribute": "dietary",
            "vk": "ab".repeat(32), "norm": 2, "scope": "successors",
        });
        let e = envelope(KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V, &p);
        assert!(matches!(
            directive_of(&e).kind,
            DirectiveKind::Unknown { .. }
        ));
    }

    /// 書き込みの入口: 出典の改訂の番号が今のターンと合わない候補 (ターンが無いものを含む) と、関門 6・7 に
    /// 外れる値の候補を捨てて数える (04 §5.2.1)。
    #[test]
    fn the_entry_drops_stale_sources_and_rechecks_values() {
        use std::sync::Arc;

        use evorag_store::testutil::TempDir;
        use evorag_store::{OpenOptions, Store, WriteRequest, Writer};
        use evorag_types::{ManualClock, UuidV7Gen};

        use crate::fold::{Candidate, Mood};
        use crate::kinds::KIND_TURN;
        use crate::lexical::Signals;
        use crate::rules::attributes;
        use crate::value_key::value_key;

        let dir = TempDir::new("facts-entry");
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
        let ids = UuidV7Gen::with_seed(clock.clone(), 3);
        let (store, _) = Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.3.0".into(),
            kinds: crate::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
            config_file: None,
        })
        .expect("open");
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).expect("writer");
        let thread = std::thread::spawn(move || writer.run());

        // 改訂の番号 1 のターン (伏せ字の後) と、番号の無いターン (0 として読む)。
        let now = Timestamp::from_millis(1_762_650_000_000);
        let turn = |rev: Option<u32>| {
            let id = ids.next_id();
            let mut body = serde_json::json!({
                "session": ids.next_id(), "role": "user", "state": "confirmed",
                "text": "大阪に住んでいます", "model_key": null, "generation": null,
                "attached": [], "reply_to": null, "error": null,
            });
            if let Some(r) = rev {
                body["rev"] = r.into();
            }
            let mut e = Envelope::new(id, KIND_TURN, 1, now, "");
            e.set_body(&body).expect("body");
            (id, e)
        };
        let (redacted, a) = turn(Some(1));
        let (plain, b) = turn(None);
        handle
            .submit_wait(WriteRequest::conversation(vec![
                WriteOp::Put(a),
                WriteOp::Put(b),
            ]))
            .expect("turns");

        let key = ForgetKey::from_bytes([7; 32]);
        let cand = |turn: Id, rev: u32, attribute: &str, value: &str| {
            Event::Candidate(Candidate {
                source: Source {
                    turn,
                    start: 0,
                    end: 6,
                    rev,
                },
                subject: "self".into(),
                attribute: attribute.into(),
                value: value.into(),
                vk: value_key(&key, value),
                mood: Mood::Fact,
                tense: Tense::Current,
                old_vk: None,
                signals: Signals::default(),
            })
        };
        let events = vec![
            cand(redacted, 0, "residence", "大阪"),  // 古い本文から作った
            cand(redacted, 1, "residence", "大阪"),  // 今の本文
            cand(ids.next_id(), 0, "hobby", "登山"), // ターンが無い
            cand(plain, 0, "hobby", "system: 登山"), // 関門 6
            cand(plain, 0, "age", &"9".repeat(200)), // 関門 7
            cand(plain, 0, "pet", "猫"),
        ];
        // 関門 5 (根拠を見ない部分) と関門 6 に外れる主語。
        let with_subject =
            |subject: &str, attribute: &str, value: &str| match cand(plain, 0, attribute, value) {
                Event::Candidate(mut c) => {
                    c.subject = subject.into();
                    Event::Candidate(c)
                }
                other => other,
            };
        let mut events = events;
        events.extend([
            with_subject("利用者", "hobby", "釣り"),
            with_subject("system: 妻", "hobby", "釣り"),
            with_subject("妻", "pet", "犬"),
            with_subject(&"あ".repeat(33), "hobby", "釣り"),
        ]);
        let safety = Safety::bundled();
        let ctx = FactCtx {
            key: &key,
            attributes: attributes(),
            safety: &safety,
            ids: &ids,
            now,
        };
        let (ops, report) = {
            let tx = store.read().expect("read");
            consolidate(&tx, &ctx, events).expect("consolidate")
        };
        assert_eq!(report.stale_source, 2, "{report:?}");
        assert_eq!(report.value_rejected, 2, "{report:?}");
        assert_eq!(report.subject_rejected, 4, "{report:?}");
        assert_eq!(report.added, 2, "{report:?}");
        handle
            .submit_wait(WriteRequest::background(ops))
            .expect("facts");
        let tx = store.read().expect("read");
        let mut got: Vec<(String, u32)> = load_facts(&tx)
            .expect("facts")
            .set
            .facts
            .iter()
            .map(|f| {
                let s = f.sources.iter().next().expect("a source");
                (f.attribute.clone(), s.rev)
            })
            .collect();
        got.sort();
        assert_eq!(got, [("pet".to_owned(), 0), ("residence".to_owned(), 1)]);
        drop(tx);
        handle.shutdown();
        thread.join().expect("writer");
    }

    /// 表に入りうる状態から外れた事実だけを数える (04 §5.4 の「作り直す時機」)。
    #[test]
    fn withdrawn_lists_facts_that_left_the_table_state() {
        let fact = |n: u8, value: &str| Fact {
            id: Id::from_bytes([n; 16]),
            subject: "self".into(),
            attribute: "hobby".into(),
            value: Some(value.into()),
            vk: None,
            tense: Tense::Current,
            state: FactState::Live,
            sources: BTreeSet::new(),
            private: false,
        };
        let mut hidden = fact(9, "読書");
        hidden.private = true;
        let before = FactSet {
            facts: vec![
                fact(1, "釣り"),
                fact(2, "登山"),
                fact(3, "将棋"),
                fact(4, "囲碁"),
                hidden.clone(),
            ],
        };
        let mut private = fact(2, "登山");
        private.private = true;
        let mut old = fact(3, "将棋");
        old.state = FactState::Superseded {
            by: None,
            at: Timestamp::from_millis(0),
        };
        let after = FactSet {
            facts: vec![fact(1, "釣り"), private, old, fact(5, "水泳"), hidden],
        };
        assert_eq!(
            withdrawn(&before, &after),
            [2u8, 3, 4].map(|n| Id::from_bytes([n; 16]))
        );
        assert!(withdrawn(&after, &after).is_empty());
    }

    /// 出典のターンが消えたとき: 読める事実は出典を外し、出典の無くなった事実を消す。知らない状態の事実は
    /// 書き換えず、出典がすべて消えたときだけ消す (04 §5.1 の「知らない状態の読み方」、03 §4.2 の規則 12)。
    #[test]
    fn dropping_turns_trims_or_deletes_facts_and_leaves_newer_ones_unless_orphaned() {
        use std::sync::Arc;

        use evorag_store::testutil::TempDir;
        use evorag_store::{OpenOptions, Store, WriteRequest, Writer};
        use evorag_types::{ManualClock, UuidV7Gen};

        let dir = TempDir::new("facts-drop");
        let now = Timestamp::from_millis(1_762_650_000_000);
        let clock = Arc::new(ManualClock::new(now));
        let (store, _) = Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.3.0".into(),
            kinds: crate::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 1)),
            config_file: None,
        })
        .expect("open");
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).expect("writer");
        let thread = std::thread::spawn(move || writer.run());

        let (kept, gone) = (Id::from_bytes([1; 16]), Id::from_bytes([2; 16]));
        let source = |turn| SourceBody {
            turn,
            rev: 0,
            start: Some(0),
            end: Some(3),
        };
        let put = |n: u8, state: &str, turns: &[Id]| {
            let mut b = fact_body(state);
            b.attribute = "hobby".into();
            b.vk = Some(format!("{n:02x}").repeat(32));
            b.sources = turns.iter().map(|t| source(*t)).collect();
            let mut e = Envelope::new(Id::from_bytes([n; 16]), KIND_FACT, FACT_V, now, "");
            e.set_body(&b).expect("body");
            let mut ops = derive_fact(&e);
            ops.push(WriteOp::Put(e));
            ops
        };
        let mut ops = Vec::new();
        ops.extend(put(10, "live", &[kept, gone])); // 出典を外す
        ops.extend(put(11, "live", &[gone])); // 消す
        ops.extend(put(12, "someday", &[kept, gone])); // 知らない状態: そのまま
        ops.extend(put(13, "someday", &[gone])); // 知らない状態: 消す
        handle
            .submit_wait(WriteRequest::background(ops))
            .expect("facts");

        let dropped = {
            let tx = store.read().expect("read");
            drop_turns(
                &tx,
                &|t| t == gone,
                &UuidV7Gen::with_seed(clock.clone(), 2),
                now,
            )
            .expect("drop")
        };
        assert_eq!(dropped.deleted, [11u8, 13].map(|n| Id::from_bytes([n; 16])));
        assert_eq!(dropped.dropped.trimmed, 1);
        handle
            .submit_wait(WriteRequest::background(dropped.ops))
            .expect("write");
        let tx = store.read().expect("read");
        let mut left = Vec::new();
        tx.for_each(KIND_FACT, |r| {
            if let Read::Ok(e) = r {
                let turns: Vec<Id> = e
                    .body_as::<SourcesField>()
                    .expect("sources")
                    .sources
                    .iter()
                    .map(|s| s.turn)
                    .collect();
                left.push((e.id, turns));
            }
            true
        })
        .expect("facts");
        assert_eq!(
            left,
            [
                (Id::from_bytes([10; 16]), vec![kept]),
                (Id::from_bytes([12; 16]), vec![kept, gone]),
            ]
        );
        // 外した出典の関係も消える。
        let src: Vec<Id> = tx
            .links_from(Id::from_bytes([10; 16]), REL_FACT_SRC)
            .expect("links")
            .into_iter()
            .map(|(to, _)| to)
            .collect();
        assert_eq!(src, [kept]);
        drop(tx);
        handle.shutdown();
        thread.join().expect("writer");
    }

    /// `origin` の無い記録 (古い版) と知らない値は利用者の操作として読み、`delete` だけを削除で足した指示とする
    /// (04 §7.5)。
    #[test]
    fn a_missing_or_unknown_origin_reads_as_user() {
        let body = |origin: Option<&str>| {
            let mut v = serde_json::json!({
                "op": "private", "subject": "self", "attribute": "dietary",
                "vk": "ab".repeat(32), "norm": 1, "scope": "value_only",
            });
            if let Some(o) = origin {
                v["origin"] = o.into();
            }
            envelope(KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V, &v)
                .body_as::<FactDirectiveBody>()
                .expect("a readable body")
        };
        assert_eq!(body(None).origin(), Origin::User);
        assert_eq!(body(None).origin, ORIGIN_USER);
        assert_eq!(body(Some("someday")).origin(), Origin::User);
        assert_eq!(body(Some("user")).origin(), Origin::User);
        assert_eq!(body(Some("delete")).origin(), Origin::Delete);
    }

    #[test]
    fn a_retracted_fact_has_no_value_key_index() {
        let mut b = fact_body("retracted");
        b.value = None;
        b.vk = None;
        let entries = fact_entries(&envelope(KIND_FACT, FACT_V, &b));
        assert!(
            entries
                .iter()
                .all(|x| !matches!(x, IndexEntry::Lookup { field, .. } if *field == LOOKUP_VK))
        );
    }
}
