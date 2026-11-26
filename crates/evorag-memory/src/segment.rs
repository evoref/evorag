//! 話題の区切り (`segment`、04 §2、03 §3.3。v0.5 の根)。
//!
//! セッションの往復を古い順に先頭から詰めて区切った、往復の並び。区切りの決め方 (大きさの見積もり・閉じる時機) は
//! M5.2 で入れる。ここでは本体の形・ID・関係と副索引と、書くときの耐久性 (閉じた区切りは `Immediate`、03 §3.4)
//! を持つ。

use std::collections::BTreeSet;

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, WriteOp, WriteRequest};
use evorag_types::Id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::derived::{Record, link, lookup};
use crate::write::Writes;

/// 話題の区切り (04 §2、v0.5)。派生だが、閉じた区切りは決め直さないので `Immediate` で書く。ジャーナルには書かない。
pub const KIND_SEGMENT: &str = "segment";
/// `segment` の本体の版。
pub(crate) const SEGMENT_V: u32 = 1;
/// 関係: 区切り → セッション。
pub const REL_SEGMENT_SESSION: &str = "segment_session";
/// 関係: 区切り → 往復の利用者の発話のターン。
pub const REL_SEGMENT_TURN: &str = "segment_turn";
/// 副索引: セッション (値はセッションの ID)。
pub const LOOKUP_SEGMENT_SESSION: &str = "session";
/// 副索引: 区切りの順 (値は最初の往復の発話のターンの ID。区切りを並べる順、04 §2・§5.2.1)。
pub const LOOKUP_SEGMENT_FIRST: &str = "first";

/// [`derive_segment`] が作りうる関係の名前。
pub(crate) const SEGMENT_RELS: &[&str] = &[REL_SEGMENT_SESSION, REL_SEGMENT_TURN];
/// [`derive_segment`] が作りうる副索引の項目。
pub(crate) const SEGMENT_LOOKUPS: &[&str] = &[LOOKUP_SEGMENT_SESSION, LOOKUP_SEGMENT_FIRST];

/// 区切りの ID の作り方の印 (04 §2 の「区切りの ID」)。
const SEGMENT_ID_TAG: &[u8] = b"evorag.seg.1";

/// 区切りの ID (04 §2): SHA-256 (`"evorag.seg.1"` ‖ 0x00 ‖ 最初の往復の発話のターンの ID の 16 バイト) の頭の
/// 16 バイト。決め直しても同じ ID になり、台帳の `segment` と結び付く。ターンの ID と同じ値にはしない。
#[must_use]
pub fn segment_id(first_turn: Id) -> Id {
    let mut h = Sha256::new();
    h.update(SEGMENT_ID_TAG);
    h.update([0u8]);
    h.update(first_turn.as_bytes());
    let digest = h.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    Id::from_bytes(id)
}

/// `segment` の本体 (v1、04 §2 の「区切りの本体」)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentBody {
    pub session: Id,
    /// 区切りの中の往復 (各往復の利用者の発話のターンの ID、古い順)。1 つ以上。往復の数はこの長さ。
    pub pairs: Vec<Id>,
    /// 閉じたか (閉じた区切りは変えない)。
    pub closed: bool,
    /// 大きさの見積もり: 前のふるいの後の利用者の発話の文字数 (送らない発話は数えない)。
    pub chars: u32,
}

impl SegmentBody {
    /// この区切りの ID ([`segment_id`])。往復が無ければ `None`。
    #[must_use]
    pub fn id(&self) -> Option<Id> {
        self.pairs.first().copied().map(segment_id)
    }
}

/// 区切りを書けない理由。
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    /// 往復が 1 つも無い (区切りには 1 往復が必ず入る、04 §2)。
    #[error("a segment needs at least one pair")]
    NoPairs,
    /// 前の封筒が、この区切りの記録ではない。
    #[error("the previous record is not this segment")]
    WrongPrevious,
    /// 前の区切りが閉じている (閉じた区切りは変えない、04 §2)。後から足された往復は、新しい区切りに入れる。
    #[error("the segment is closed and is not changed")]
    Closed,
    #[error(transparent)]
    Body(#[from] BodyError),
}

#[derive(Deserialize)]
struct ClosedField {
    #[serde(default)]
    closed: bool,
}

#[derive(Deserialize)]
struct SessionField {
    session: Id,
}

#[derive(Deserialize)]
struct PairsField {
    #[serde(default)]
    pairs: Vec<Id>,
}

/// `segment` の関係と副索引 (repair で本体から作り直すときにも使う)。
pub(crate) fn derive_segment(e: &Envelope) -> Vec<WriteOp> {
    let mut ops = Vec::new();
    if let Ok(b) = e.body_as::<SessionField>() {
        ops.push(link(e.id, REL_SEGMENT_SESSION, b.session));
        ops.push(lookup(
            KIND_SEGMENT,
            LOOKUP_SEGMENT_SESSION,
            OwnedPart::Id(b.session),
            e.id,
        ));
    }
    if let Ok(b) = e.body_as::<PairsField>() {
        if let Some(first) = b.pairs.first() {
            ops.push(lookup(
                KIND_SEGMENT,
                LOOKUP_SEGMENT_FIRST,
                OwnedPart::Id(*first),
                e.id,
            ));
        }
        let turns: BTreeSet<Id> = b.pairs.into_iter().collect();
        ops.extend(turns.into_iter().map(|t| link(e.id, REL_SEGMENT_TURN, t)));
    }
    ops
}

impl Writes<'_> {
    /// 区切りを書く操作 (封筒と、その関係・副索引)。`previous` は読み直した前の区切りの記録 (開いている末尾の区切り
    /// に往復を足すとき)。知らない項目と作成の時刻を残す。台帳と同じコミットに入れるときは、この操作を台帳の依頼に
    /// 足す ([`Writes::ledger`])。
    ///
    /// # Errors
    /// 往復が無いとき、`previous` が同じ区切りでないか閉じているとき、本体を CBOR にできないとき。
    pub fn segment(
        &self,
        body: &SegmentBody,
        previous: Option<&Envelope>,
    ) -> Result<(Id, Vec<WriteOp>), SegmentError> {
        let id = body.id().ok_or(SegmentError::NoPairs)?;
        if let Some(p) = previous {
            if p.id != id || p.kind != KIND_SEGMENT {
                return Err(SegmentError::WrongPrevious);
            }
            if p.body_as::<ClosedField>().is_ok_and(|b| b.closed) {
                return Err(SegmentError::Closed);
            }
        }
        let ops = Record {
            kind: KIND_SEGMENT,
            v: SEGMENT_V,
            id,
            now: self.clock.now(),
            body,
            derive: derive_segment,
        }
        .ops(previous)?;
        Ok((id, ops))
    }

    /// 区切りだけを書く依頼 (03 §3.4)。閉じた区切りは `Immediate` (閉じた区切りの境界は決め直さないため、04 §2)、
    /// 開いている末尾の区切りは裏の処理の結果として `None` で書く (失っても往復から決め直せる)。どちらも会話の追記
    /// より後に回す。
    ///
    /// # Errors
    /// [`Writes::segment`] と同じ。
    pub fn segment_request(
        &self,
        body: &SegmentBody,
        previous: Option<&Envelope>,
    ) -> Result<(Id, WriteRequest), SegmentError> {
        let (id, ops) = self.segment(body, previous)?;
        let req = if body.closed {
            WriteRequest::background_durable(ops)
        } else {
            WriteRequest::background(ops)
        };
        Ok((id, req))
    }
}
