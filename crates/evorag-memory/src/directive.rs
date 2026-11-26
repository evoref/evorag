//! 指示 (`directive`、04 §7.5、03 §3.3。v0.5 の根)。
//!
//! 利用者の明示の操作と、その結果として足す出来事 (`end`) の正本。DB とジャーナルの 2 か所に持ち、追記だけにする
//! (書き換えも削除もしない)。値そのものは持たず、値の鍵と照合の鍵の並び (`vks`)・entity・属性・根拠の位置を持つ。
//!
//! - 本体は最上位を平らにし、使わない項目は null か空の並びで書く。`op`・`origin`・`scope` は文字列で持つ (知らない
//!   値の指示も丸ごと読めなくならないように。知らない `op` は当てずに数えて残す、04 §7.5)。
//! - 関係と副索引は、要る項目だけを 1 つずつ別の型で読んで作る (03 §4.2 の規則 11)。

use std::collections::BTreeSet;

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, WriteOp};
use evorag_types::Id;
use serde::{Deserialize, Serialize};

use crate::derived::{Record, link, lookup};
use crate::value_key::NORM_V1;
use crate::write::Writes;

/// 指示 (04 §7.5、v0.5)。利用者の明示の操作なのでジャーナルに書く。追記だけ。
pub const KIND_DIRECTIVE: &str = "directive";
/// `directive` の本体の版。
pub(crate) const DIRECTIVE_V: u32 = 1;
/// 関係: 指示 → 解く相手の指示 (`private_off`・`unblock`、03 §3.3)。
pub const REL_DIRECTIVE_CANCELS: &str = "cancels";
/// 副索引: 値の鍵と照合の鍵 (`vks` の要素ごと)。
pub const LOOKUP_DIRECTIVE_VK: &str = "vk";
/// 副索引: 正規化した entity。
pub const LOOKUP_DIRECTIVE_ENT: &str = "ent";
/// 副索引: 根拠と伏せ字のターン (`at`・`turns`・`redactions`・`hidden`・`end` の位置のターンごと)。
pub const LOOKUP_DIRECTIVE_TURN: &str = "turn";

/// [`derive_directive`] が作りうる関係の名前。
pub(crate) const DIRECTIVE_RELS: &[&str] = &[REL_DIRECTIVE_CANCELS];
/// [`derive_directive`] が作りうる副索引の項目。
pub(crate) const DIRECTIVE_LOOKUPS: &[&str] = &[
    LOOKUP_DIRECTIVE_VK,
    LOOKUP_DIRECTIVE_ENT,
    LOOKUP_DIRECTIVE_TURN,
];

/// 指示の `op` (04 §7.5)。v0.6 から `pin`、v0.8 から `note_confirmed` を足す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirectiveOp {
    Forget,
    End,
    Private,
    PrivateOff,
    Accept,
    Revert,
    Unblock,
    /// この版が知らない (新しい版の) `op`。当てずに数えて残す (04 §7.5 の「知らない `op` の読み方」)。
    Unknown,
}

impl DirectiveOp {
    const NAMES: [(DirectiveOp, &'static str); 7] = [
        (DirectiveOp::Forget, "forget"),
        (DirectiveOp::End, "end"),
        (DirectiveOp::Private, "private"),
        (DirectiveOp::PrivateOff, "private_off"),
        (DirectiveOp::Accept, "accept"),
        (DirectiveOp::Revert, "revert"),
        (DirectiveOp::Unblock, "unblock"),
    ];

    /// 保存の名前。[`DirectiveOp::Unknown`] には名前が無い。
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        Self::NAMES
            .iter()
            .find(|(op, _)| *op == self)
            .map(|(_, n)| *n)
    }

    /// 保存の名前から読む。知らない名前は [`DirectiveOp::Unknown`]。
    #[must_use]
    pub fn of(name: &str) -> Self {
        Self::NAMES
            .iter()
            .find(|(_, n)| *n == name)
            .map_or(DirectiveOp::Unknown, |(op, _)| *op)
    }
}

/// 指示を足した経路 (04 §7.5 の `origin`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirectiveOrigin {
    /// 利用者の操作。無い・知らない値も、こちらとして読む。
    User,
    /// セッションの削除で足した指示。
    Delete,
    /// `import-v04` で足した指示。
    Import,
}

impl DirectiveOrigin {
    /// 保存の名前。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            DirectiveOrigin::User => "user",
            DirectiveOrigin::Delete => "delete",
            DirectiveOrigin::Import => "import",
        }
    }

    /// 保存の名前から読む。知らない名前は [`DirectiveOrigin::User`]。
    #[must_use]
    pub fn of(name: &str) -> Self {
        match name {
            "delete" => DirectiveOrigin::Delete,
            "import" => DirectiveOrigin::Import,
            _ => DirectiveOrigin::User,
        }
    }
}

/// private の効く範囲 (04 §7.5 の `scope`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivateScope {
    /// 指定した値と、同じ (entity, 辞書の属性) にあとで入る値。知らない値も、こちら (広く効く側) として読む。
    Successors,
    /// 指定した値だけ。
    ValueOnly,
}

impl PrivateScope {
    /// 保存の名前。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PrivateScope::Successors => "successors",
            PrivateScope::ValueOnly => "value_only",
        }
    }

    /// 保存の名前から読む。知らない名前は [`PrivateScope::Successors`]。
    #[must_use]
    pub fn of(name: &str) -> Self {
        if name == "value_only" {
            PrivateScope::ValueOnly
        } else {
            PrivateScope::Successors
        }
    }
}

/// ターンの本文の中の範囲 (UTF-8 のバイト位置。改訂の番号 `rev` のときの本文)。指示の根拠の位置 (`at`)、伏せ字に
/// しないときの `[非表示]` の範囲 (`hidden`) に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpan {
    pub turn: Id,
    pub rev: u32,
    pub start: u32,
    pub end: u32,
}

/// 伏せ字 1 つ (04 §7.3)。範囲は伏せ字の前の本文 (`rev_before`) の UTF-8 のバイト位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectiveRedaction {
    pub turn: Id,
    pub rev_before: u32,
    pub rev_after: u32,
    pub start: u32,
    pub end: u32,
    /// 伏せ字の後の本文の SHA-256 (64 桁の小文字の 16 進)。
    pub hash: String,
}

/// `directive` の本体 (v1、04 §7.5 の「本体の形」)。使わない項目は null か空の並び。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectiveBody {
    /// `forget`・`end`・`private`・`private_off`・`accept`・`revert`・`unblock` ([`DirectiveBody::op`])。
    pub op: String,
    /// 正規化した entity (`self` か呼び名)。解く指示 (`private_off`・`unblock`) では null。
    pub entity: Option<String>,
    /// entity のその時点の別名の並び (v0.5 は空。v0.7 から entity の別名の辞書)。
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 辞書の属性の名前 (自由記述の属性なら null)。
    pub aspect: Option<String>,
    /// 自由記述の属性の鍵の 16 進 (辞書の属性なら null)。
    pub aspect_mac: Option<String>,
    /// 値の鍵と照合の鍵の 16 進の並び (区別せずに 1 つの並びに入れる。同じ鍵は 1 つ)。`end` では終える値の鍵。
    #[serde(default)]
    pub vks: Vec<String>,
    /// 値の鍵の正規化の版。
    pub norm: Option<u32>,
    /// 根拠の位置 (`private`・`accept`・`revert`)。取り込んだ指示では v0.4 の事実の出典の範囲。
    pub at: Option<TurnSpan>,
    /// 操作したときの単位の ID (参考の情報。当てるときは使わない、04 §2)。
    pub unit: Option<Id>,
    /// 解く相手の指示 (`private_off`・`unblock`)。
    pub target: Option<Id>,
    /// `user`・`delete`・`import` ([`DirectiveBody::origin`])。
    #[serde(default = "origin_user")]
    pub origin: String,
    /// `forget`: 忘れた単位の根拠のターンの並び。
    #[serde(default)]
    pub turns: Vec<Id>,
    /// `forget`: 伏せ字の一覧。
    #[serde(default)]
    pub redactions: Vec<DirectiveRedaction>,
    /// `forget`: 伏せ字にしないときに想起で `[非表示]` にする範囲。
    #[serde(default)]
    pub hidden: Vec<TurnSpan>,
    /// `forget`: 履歴もまとめたか。
    #[serde(default)]
    pub history: bool,
    /// `forget`: 伏せ字にしたか。
    #[serde(default)]
    pub redact: bool,
    /// `private`: `successors`・`value_only` ([`DirectiveBody::scope`])。
    pub scope: Option<String>,
    /// `end`: 終えた位置 (ターンの ID。決まらなければ指示の ID)。
    pub turn: Option<Id>,
    /// `end`: きっかけの忘れるの指示の ID (削除と取り込みでは null)。
    pub cause: Option<Id>,
    /// `end`: entity の生の文字列 (今の規則で解き直して比べる、04 §7.4)。
    pub entity_raw: Option<String>,
    /// `import-v04` で写した指示か。
    #[serde(default)]
    pub imported: bool,
    /// v0.4 の再び覚えない印 (`import-v04` で写した指示だけ。当てるときは使わない、04 §7.6)。
    #[serde(default)]
    pub marks: Vec<String>,
}

fn origin_user() -> String {
    DirectiveOrigin::User.name().to_owned()
}

impl DirectiveBody {
    /// `op` の、どの項目も空の指示 (利用者の操作。値の鍵の正規化は v1)。
    ///
    /// # Panics
    /// `op` が [`DirectiveOp::Unknown`] のとき (書く指示の `op` は、この版が知るものだけ)。
    #[must_use]
    pub fn new(op: DirectiveOp) -> Self {
        let op = op
            .name()
            .unwrap_or_else(|| panic!("a new directive needs a known op, not {op:?}"));
        Self {
            op: op.to_owned(),
            entity: None,
            aliases: Vec::new(),
            aspect: None,
            aspect_mac: None,
            vks: Vec::new(),
            norm: Some(NORM_V1),
            at: None,
            unit: None,
            target: None,
            origin: origin_user(),
            turns: Vec::new(),
            redactions: Vec::new(),
            hidden: Vec::new(),
            history: false,
            redact: false,
            scope: None,
            turn: None,
            cause: None,
            entity_raw: None,
            imported: false,
            marks: Vec::new(),
        }
    }

    /// 指示の `op`。知らない値は [`DirectiveOp::Unknown`]。
    #[must_use]
    pub fn op(&self) -> DirectiveOp {
        DirectiveOp::of(&self.op)
    }

    /// 指示を足した経路。知らない値は利用者の操作として読む。
    #[must_use]
    pub fn origin(&self) -> DirectiveOrigin {
        DirectiveOrigin::of(&self.origin)
    }

    /// private の効く範囲。`private` でなければ `None`。項目の無い・知らない値は「後継まで」。
    #[must_use]
    pub fn scope(&self) -> Option<PrivateScope> {
        (self.op() == DirectiveOp::Private)
            .then(|| PrivateScope::of(self.scope.as_deref().unwrap_or_default()))
    }
}

// 関係と副索引に要る項目は、1 つずつ別の型で読む (03 §4.2 の規則 11)。新しい版でほかの項目の型が変わっても、
// 読める項目の関係と副索引 (特に忘れるの `vk`) を失わない。

#[derive(Deserialize)]
struct VksField {
    #[serde(default)]
    vks: Vec<String>,
}

#[derive(Deserialize)]
struct EntityField {
    #[serde(default)]
    entity: Option<String>,
}

#[derive(Deserialize)]
struct TargetField {
    #[serde(default)]
    target: Option<Id>,
}

#[derive(Deserialize)]
struct TurnOnly {
    turn: Id,
}

#[derive(Deserialize)]
struct AtField {
    #[serde(default)]
    at: Option<TurnOnly>,
}

#[derive(Deserialize)]
struct TurnsField {
    #[serde(default)]
    turns: Vec<Id>,
}

#[derive(Deserialize)]
struct RedactionsField {
    #[serde(default)]
    redactions: Vec<TurnOnly>,
}

#[derive(Deserialize)]
struct HiddenField {
    #[serde(default)]
    hidden: Vec<TurnOnly>,
}

#[derive(Deserialize)]
struct EndTurnField {
    #[serde(default)]
    turn: Option<Id>,
}

/// 指示の根拠と伏せ字のターン (副索引 `turn` の値)。
fn directive_turns(e: &Envelope) -> BTreeSet<Id> {
    let mut out = BTreeSet::new();
    if let Ok(AtField { at: Some(at) }) = e.body_as::<AtField>() {
        out.insert(at.turn);
    }
    if let Ok(b) = e.body_as::<TurnsField>() {
        out.extend(b.turns);
    }
    if let Ok(b) = e.body_as::<RedactionsField>() {
        out.extend(b.redactions.into_iter().map(|r| r.turn));
    }
    if let Ok(b) = e.body_as::<HiddenField>() {
        out.extend(b.hidden.into_iter().map(|h| h.turn));
    }
    if let Ok(EndTurnField { turn: Some(t) }) = e.body_as::<EndTurnField>() {
        out.insert(t);
    }
    out
}

/// `directive` の関係と副索引 (ジャーナルの取り込みと repair で、本体から作り直すときにも使う)。
pub(crate) fn derive_directive(e: &Envelope) -> Vec<WriteOp> {
    let mut ops = Vec::new();
    if let Ok(b) = e.body_as::<VksField>() {
        let vks: BTreeSet<String> = b.vks.into_iter().collect();
        ops.extend(vks.into_iter().map(|vk| {
            lookup(
                KIND_DIRECTIVE,
                LOOKUP_DIRECTIVE_VK,
                OwnedPart::Str(vk),
                e.id,
            )
        }));
    }
    if let Ok(EntityField {
        entity: Some(entity),
    }) = e.body_as::<EntityField>()
        && !entity.is_empty()
    {
        ops.push(lookup(
            KIND_DIRECTIVE,
            LOOKUP_DIRECTIVE_ENT,
            OwnedPart::Str(entity),
            e.id,
        ));
    }
    ops.extend(directive_turns(e).into_iter().map(|t| {
        lookup(
            KIND_DIRECTIVE,
            LOOKUP_DIRECTIVE_TURN,
            OwnedPart::Id(t),
            e.id,
        )
    }));
    if let Ok(TargetField { target: Some(t) }) = e.body_as::<TargetField>() {
        ops.push(link(e.id, REL_DIRECTIVE_CANCELS, t));
    }
    ops
}

impl Writes<'_> {
    /// 指示を足す操作 (封筒と、その関係・副索引)。指示は追記だけなので、いつも新しい ID で作る。忘れる・削除で
    /// 足す指示は、同じ依頼に [`WriteOp::GroupJournalLines`] を入れて、ジャーナルの組の行にする (03 §6)。
    ///
    /// # Errors
    /// 本体を CBOR にできないとき。
    pub fn directive(&self, body: &DirectiveBody) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let ops = Record {
            kind: KIND_DIRECTIVE,
            v: DIRECTIVE_V,
            id,
            now: self.clock.now(),
            body,
            derive: derive_directive,
        }
        .ops(None)?;
        Ok((id, ops))
    }
}
