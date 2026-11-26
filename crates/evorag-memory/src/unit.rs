//! 単位 (`unit`、04 §5.1、03 §3.3。v0.5 の根)。
//!
//! 導出 (04 §5.3) で台帳と正本から何度でも同じものを作る派生の記録。ID は中身から作る (導出と一緒に M5.3 で入れる)。
//! 失っても導出の指紋が合わなくなって導き直すので、裏の処理の結果として `None` で書き、ジャーナル・退避・書き出し
//! には写さない (03 §3.4・§7.1・§11)。
//!
//! 単位は導出の結果なので、知らない状態・層の単位を読んで使うことは無い (本体を型として読めなければ、指紋が合わない
//! ものとして導き直す、04 §5.1 の「知らない値」)。列挙は型で持つ。

use std::collections::BTreeSet;

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, WriteOp, WriteRequest};
use evorag_types::{Id, Timestamp};
use serde::{Deserialize, Serialize};

use crate::derived::{Record, link, lookup};

/// 単位 (04 §5.1、v0.5)。導出で作り直す派生の記録。ジャーナル・退避・書き出しに写さない。
pub const KIND_UNIT: &str = "unit";
/// `unit` の本体の版。
pub(crate) const UNIT_V: u32 = 1;
/// 関係: 単位 → 根拠のターン。
pub const REL_UNIT_ANCHOR: &str = "unit_anchor";
/// 副索引: (entity, aspect) を U+0000 でつないだ文字列。
pub const LOOKUP_UNIT_EA: &str = "ea";
/// 副索引: 値の鍵の 16 進 (private の単位は持たない)。
pub const LOOKUP_UNIT_VK: &str = "vk";
/// 副索引: 日付 (`when` の `from`。private の単位は持たない)。
pub const LOOKUP_UNIT_DATE: &str = "date";
/// 副索引: 根拠のターン。
pub const LOOKUP_UNIT_SRC: &str = "src";

/// [`derive_unit`] が作りうる関係の名前。
pub(crate) const UNIT_RELS: &[&str] = &[REL_UNIT_ANCHOR];
/// [`derive_unit`] が作りうる副索引の項目。
pub(crate) const UNIT_LOOKUPS: &[&str] = &[
    LOOKUP_UNIT_EA,
    LOOKUP_UNIT_VK,
    LOOKUP_UNIT_DATE,
    LOOKUP_UNIT_SRC,
];

/// 副索引 `ea` の値。
#[must_use]
pub fn entity_aspect_key(entity: &str, aspect: &str) -> String {
    format!("{entity}\u{0}{aspect}")
}

/// 層 (`layer`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitLayer {
    /// 置き換えのある事実 (今の値と履歴を持つ)。
    World,
    /// 出来事 (置き換えない)。
    Episode,
}

/// 時制 (`tense`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitTense {
    Current,
    Past,
    Planned,
}

/// 状態 (`state`。`world` だけ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Live,
    Contested,
    Superseded,
}

/// 根拠の種類 (`anchors` の `kind`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    /// 値を言った。
    Said,
    /// 過去形・否定で終えた発言。
    Ended,
    /// 訂正の古い値の穴。
    Corrected,
}

/// 日付の解き方 (`when` の `basis`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenBasis {
    /// 絶対の日付の表現。
    Stated,
    /// 相対の表現を、発話のターンの UTC からのずれで解いた。
    Relative,
    /// 表現が無いか読めない出来事の、発話の日。
    Utterance,
    /// ずれの記録の無いターンの相対の表現 (解かなかった)。
    NoOffset,
}

/// 日付 (`when`)。`from`・`to` は `YYYY`・`YYYY-MM`・`YYYY-MM-DD` の文字列か null。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitWhen {
    pub from: Option<String>,
    pub to: Option<String>,
    pub basis: WhenBasis,
}

/// 根拠 1 つ (`anchors` の要素)。範囲は保存した本文 (改訂の番号 `rev`) の UTF-8 のバイト位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub turn: Id,
    pub rev: u32,
    /// 値の範囲。
    pub start: u32,
    pub end: u32,
    /// 根拠の文の範囲。
    pub s_start: u32,
    pub s_end: u32,
    pub kind: AnchorKind,
    pub when: Option<UnitWhen>,
}

/// `unit` の本体 (v1、04 §5.1 の「本体の形」。最上位を平らにする)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitBody {
    pub layer: UnitLayer,
    /// `self` か、呼び名に正規化 v1 を当てた文字列。
    pub entity: String,
    /// 辞書の属性の名前か、正規化した自由記述の属性。
    pub aspect: String,
    /// 自由記述の属性なら true。
    pub free: bool,
    /// 根拠の文の部分文字列。
    pub value: String,
    /// 値の鍵の 16 進。
    pub vk: String,
    /// 値の鍵の正規化の版。
    pub norm: u32,
    /// 根拠の順で最初の、解けた日付。
    pub when: Option<UnitWhen>,
    pub tense: UnitTense,
    /// `world` だけ。`episode` では null。
    pub state: Option<UnitState>,
    pub against: Option<Id>,
    pub by: Option<Id>,
    /// `Superseded` になったときの、置き換えた候補か `end` の位置のターンの時刻 (UTC、ミリ秒)。
    pub state_at: Option<i64>,
    /// 応答に添えない印 (指示から導出で決まる)。
    pub private: bool,
    /// 根拠の並び ((ターン, 値の始まり) の順)。
    pub anchors: Vec<Anchor>,
}

#[derive(Deserialize)]
struct EaFields {
    entity: String,
    aspect: String,
}

#[derive(Deserialize)]
struct PrivateField {
    #[serde(default)]
    private: bool,
}

#[derive(Deserialize)]
struct VkField {
    vk: String,
}

#[derive(Deserialize)]
struct FromOnly {
    #[serde(default)]
    from: Option<String>,
}

#[derive(Deserialize)]
struct WhenField {
    #[serde(default)]
    when: Option<FromOnly>,
}

#[derive(Deserialize)]
struct TurnOnly {
    turn: Id,
}

#[derive(Deserialize)]
struct AnchorsField {
    #[serde(default)]
    anchors: Vec<TurnOnly>,
}

/// `unit` の関係と副索引 (03 §3.3)。private の単位は `vk` と `date` の副索引を持たない (列挙で件数に出さないため)。
/// private を読めない単位も、持たない側に倒す。
pub(crate) fn derive_unit(e: &Envelope) -> Vec<WriteOp> {
    let mut ops = Vec::new();
    if let Ok(b) = e.body_as::<EaFields>() {
        ops.push(lookup(
            KIND_UNIT,
            LOOKUP_UNIT_EA,
            OwnedPart::Str(entity_aspect_key(&b.entity, &b.aspect)),
            e.id,
        ));
    }
    let open = e.body_as::<PrivateField>().is_ok_and(|b| !b.private);
    if open {
        if let Ok(b) = e.body_as::<VkField>() {
            ops.push(lookup(
                KIND_UNIT,
                LOOKUP_UNIT_VK,
                OwnedPart::Str(b.vk),
                e.id,
            ));
        }
        if let Ok(WhenField {
            when: Some(FromOnly { from: Some(date) }),
        }) = e.body_as::<WhenField>()
        {
            ops.push(lookup(
                KIND_UNIT,
                LOOKUP_UNIT_DATE,
                OwnedPart::Str(date),
                e.id,
            ));
        }
    }
    if let Ok(b) = e.body_as::<AnchorsField>() {
        let turns: BTreeSet<Id> = b.anchors.into_iter().map(|a| a.turn).collect();
        for t in turns {
            ops.push(link(e.id, REL_UNIT_ANCHOR, t));
            ops.push(lookup(KIND_UNIT, LOOKUP_UNIT_SRC, OwnedPart::Id(t), e.id));
        }
    }
    ops
}

/// 単位を書く操作 (封筒と、その関係・副索引)。`id` は中身から作った単位の ID (04 §5.1)。`previous` は読み直した
/// 前の同じ単位の記録 (あれば、作成の時刻と知らない項目を残し、前の関係と副索引を作り直す)。
///
/// # Errors
/// 本体を CBOR にできないとき。
pub fn unit_ops(
    id: Id,
    now: Timestamp,
    body: &UnitBody,
    previous: Option<&Envelope>,
) -> Result<Vec<WriteOp>, BodyError> {
    Record {
        kind: KIND_UNIT,
        v: UNIT_V,
        id,
        now,
        body,
        derive: derive_unit,
    }
    .ops(previous)
}

/// 単位 (と導出の指紋) を書く依頼。裏の処理の結果として `None` で書く (失っても指紋で気付いて導き直す、03 §3.4)。
#[must_use]
pub fn units_request(ops: Vec<WriteOp>) -> WriteRequest {
    WriteRequest::background(ops)
}

/// 単位を消す操作 (封筒と、その関係・副索引)。導出で無くなった単位と、出典の無い単位を消すとき。
#[must_use]
pub fn unit_delete_ops(env: &Envelope) -> Vec<WriteOp> {
    crate::derived::delete_ops(env, derive_unit)
}
