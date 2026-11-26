//! v0.4 の `fact`・`fact_directive` の本体の型 (04 §5.1・§7.5、03 §3.3)。保存の形なので、v0.3〜v0.4 の形のまま
//! 変えない (golden fixture `tests/fixtures/fact`・`fact_directive`)。
//!
//! 本体は最上位を平らにし、状態と `op` は文字列で持つ (知らない値の記録も丸ごと読めなくならないように、03 §4.2 の
//! 規則 7・11)。核の形 (`fold`) との行き来は旧エンジン (`facts`) が持つ。

use evorag_types::Id;
use serde::{Deserialize, Serialize};

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
    /// 忘れた値の鍵の 16 進の並び (忘れる、04 §7.5 の「忘れると訂正」)。ほかの指示では空。
    #[serde(default)]
    pub vks: Vec<String>,
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

pub(crate) fn origin_user() -> String {
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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Origin::User => ORIGIN_USER,
            Origin::Delete => ORIGIN_DELETE,
        }
    }
}

/// 指示の `op`: 忘れる (04 §7.5)。
pub const OP_FORGET: &str = "forget";
pub(crate) const OP_PRIVATE: &str = "private";
pub(crate) const OP_PRIVATE_OFF: &str = "private_off";
pub(crate) const OP_ACCEPT: &str = "accept";
pub(crate) const OP_REVERT: &str = "revert";
/// 指示の `op`: 印を解く (04 §7.5)。
pub const OP_UNBLOCK: &str = "unblock";

impl FactDirectiveBody {
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

#[cfg(test)]
mod tests {
    use evorag_store::Envelope;
    use evorag_types::Timestamp;

    use super::*;

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
            let mut e = Envelope::new(
                Id::from_bytes([1; 16]),
                crate::import_v04::KIND_FACT_DIRECTIVE,
                crate::import_v04::FACT_DIRECTIVE_V,
                Timestamp::from_millis(1_762_650_000_000),
                "",
            );
            e.set_body(&v).expect("body");
            e.body_as::<FactDirectiveBody>().expect("a readable body")
        };
        assert_eq!(body(None).origin(), Origin::User);
        assert_eq!(body(None).origin, ORIGIN_USER);
        assert_eq!(body(Some("someday")).origin(), Origin::User);
        assert_eq!(body(Some("user")).origin(), Origin::User);
        assert_eq!(body(Some("delete")).origin(), Origin::Delete);
    }
}
