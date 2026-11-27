//! 台帳 (`proposal`、04 §5.2.1・§5.2、03 §3.3。v0.5 の根)。
//!
//! 区切りごとの LLM の 1 回の呼び出し (B2) の生の出力。派生だが、LLM を呼ばないと作り直せず同じものが戻らないので、
//! 区切りの呼び出しが終わるごとに `Immediate` のコミットで書き (03 §3.4)、退避と書き出しに含める。ジャーナルには
//! 書かない。
//!
//! - 本体と `output` の型は v0.5.0 から「項目を足すだけ」にする (03 §3.3 の「台帳の本体の互換」、OQ-53)。
//! - `output` は節ごとに、型の合った部分を wire の形のまま保存し ([`LedgerOutput`])、導出のときに読むときの型で読む
//!   ([`LedgerOutput::units`] など)。読むときは寛容にし、知らない項目を残し、知らない列挙の値を持つ要素はその要素
//!   だけを数えて使わない (台帳の全体を読めない記録にしない)。
//! - 書くときの型 (B2 の文法、すべての項目が必須) と読むときの型を分ける ([`LedgerUnitOut`] と [`LedgerUnit`])。

use std::collections::{BTreeMap, BTreeSet};

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, RawValue, WriteOp, WriteRequest};
use evorag_types::Id;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::derived::{Record, delete_ops, link, lookup};
use crate::kinds::SrcRev;
use crate::segment::{SegmentBody, SegmentError};
use crate::write::{Writes, expect_record, expect_turn};

/// 台帳 (04 §5.2.1、v0.5)。`Immediate` で書き、退避と書き出しに含める。ジャーナルには書かない。
pub const KIND_PROPOSAL: &str = "proposal";
/// `proposal` の本体の版。
pub(crate) const PROPOSAL_V: u32 = 1;
/// 出力の型の版 (`schema`、04 §5.2 の「出力の型」)。型の項目を足したら上げる。
pub const LEDGER_SCHEMA: u32 = 1;
/// 呼び出しの種類: 区切りの呼び出し (B2)。v0.7 の `topic_title`・v0.8 の `insight` を足す。
pub const CALL_SEGMENT: &str = "segment";
/// 関係: 台帳 → 区切り。
pub const REL_LEDGER_SEGMENT: &str = "ledger_segment";
/// 関係: 台帳 → `sentences` と `inputs` の `refs` のターン。
pub const REL_LEDGER_TURN: &str = "ledger_turn";
/// 副索引: 区切り (値は区切りの ID)。
pub const LOOKUP_LEDGER_SEGMENT: &str = "segment";
/// 副索引: 選択 (値は `model_key` ‖ U+0000 ‖ `variant`)。
pub const LOOKUP_LEDGER_SELECTION: &str = "mv";

/// [`derive_ledger`] が作りうる関係の名前。
pub(crate) const LEDGER_RELS: &[&str] = &[REL_LEDGER_SEGMENT, REL_LEDGER_TURN];
/// [`derive_ledger`] が作りうる副索引の項目。
pub(crate) const LEDGER_LOOKUPS: &[&str] = &[LOOKUP_LEDGER_SEGMENT, LOOKUP_LEDGER_SELECTION];

/// 副索引 `mv` の値 (`model_key` ‖ U+0000 ‖ `variant`)。
#[must_use]
pub fn selection_key(model_key: &str, variant: &str) -> String {
    format!("{model_key}\u{0}{variant}")
}

/// 台帳の本体と入れ子の型の、この版が知らない項目 (名前 → 保存した値)。書き戻すと元の値のまま残る (03 §4.2 の
/// 規則 7、03 §3.3 の「台帳の本体の互換」)。台帳の掃除は、この中の文字列にも当てる (04 §7.2)。
pub type UnknownFields = BTreeMap<String, RawValue>;

/// 知らない項目 (`extra`) を持つ型の `Serialize`。知っている項目を宣言の順に書き、知らない項目をその後に足す。
/// 長さの決まった map として書く (derive の `Serialize` と同じバイト列になる。`#[serde(flatten)]` の書き方は長さの
/// 決まらない map になり、見本 (`tests/fixtures/proposal/`) のバイト列が変わるため)。
macro_rules! serialize_with_extra {
    ($t:ty { $($field:ident),* $(,)? }) => {
        impl Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                const KNOWN: &[&str] = &[$(stringify!($field)),*];
                let extra: Vec<(&String, &RawValue)> = self
                    .extra
                    .iter()
                    .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
                    .collect();
                let mut m = s.serialize_map(Some(KNOWN.len() + extra.len()))?;
                $(m.serialize_entry(stringify!($field), &self.$field)?;)*
                for (k, v) in extra {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
        }
    };
}

/// 文番号の根拠の文の位置 (04 §5.2.1 の `sentences`)。範囲は改訂の番号 `rev` のときの本文の UTF-8 のバイト位置。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SentenceSpan {
    /// 文番号 (区切りの中で 1 から通し)。
    pub s: u32,
    pub turn: Id,
    pub rev: u32,
    pub start: u32,
    pub end: u32,
    /// 知らない項目。
    #[serde(flatten)]
    pub extra: UnknownFields,
}

serialize_with_extra!(SentenceSpan {
    s,
    turn,
    rev,
    start,
    end
});

impl SentenceSpan {
    /// 知らない項目の無い文の位置。
    #[must_use]
    pub fn new(s: u32, turn: Id, rev: u32, start: u32, end: u32) -> Self {
        Self {
            s,
            turn,
            rev,
            start,
            end,
            extra: UnknownFields::new(),
        }
    }
}

/// 入れた応答 1 つ (04 §5.2.1 の `inputs` の `refs`。`[参照のみ]` の枠のターンと改訂の番号)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LedgerRef {
    pub turn: Id,
    pub rev: u32,
    /// 知らない項目。
    #[serde(flatten)]
    pub extra: UnknownFields,
}

serialize_with_extra!(LedgerRef { turn, rev });

impl LedgerRef {
    /// 知らない項目の無い応答の参照。
    #[must_use]
    pub fn new(turn: Id, rev: u32) -> Self {
        Self {
            turn,
            rev,
            extra: UnknownFields::new(),
        }
    }
}

/// 最後に入れた往復 (04 §5.2.1 の `inputs` の `through`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Through {
    /// 最後に入れた往復の発話のターン。
    pub turn: Id,
    /// 入れた往復の数。
    pub pairs: u32,
}

/// 本文の中の範囲 (UTF-8 のバイト位置)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u32,
    pub end: u32,
}

/// 送った訂正の候補 1 つ (04 §5.2.1 の `inputs` の `corr`、§6)。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CorrInput {
    /// 候補の番号 (出力の `corrections` の `id`)。
    pub id: u32,
    pub turn: Id,
    pub rev: u32,
    /// 当たった訂正の字句の型。
    pub pattern: String,
    /// 新しい値の範囲。
    pub new: ByteRange,
    /// 古い値の範囲 (無ければ null)。
    pub old: Option<ByteRange>,
    /// 知らない項目。
    #[serde(flatten)]
    pub extra: UnknownFields,
}

serialize_with_extra!(CorrInput {
    id,
    turn,
    rev,
    pattern,
    new,
    old
});

/// 呼び出しの入力の対応表と版 (04 §5.2.1 の `inputs`)。無い項目は既定値 (版は 0) で読み、規則の版の合わない台帳
/// (有効でない側) に倒す。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LedgerInputs {
    /// 前のふるいの規則の版。
    pub prefilter: u32,
    /// 文の分け方の規則の版 (04 §2 の「文の分け方 v1」)。
    pub splitter: u32,
    /// 訂正の字句の型の版。
    pub correction: u32,
    /// 入力を読んだときに見えていた最新の忘れるの指示の ID (無ければ null)。有効性には使わない。
    pub forgets_seen: Option<Id>,
    /// 送ったメッセージのバイト列の SHA-256 の 16 進。掃除で要素を変えたら null (04 §7.2)。
    pub input_hash: Option<String>,
    /// 縮退で chat のモデルを使ったか。
    pub role_fallback: bool,
    /// 意図した `memory` の役割の model_key。
    pub role_model_key: Option<String>,
    /// 最後に入れた往復。
    pub through: Option<Through>,
    /// 送った訂正の候補の表。
    #[serde(default)]
    pub corr: Vec<CorrInput>,
    /// 入れた応答 (`[参照のみ]` の枠) のターンと改訂の番号。
    #[serde(default)]
    pub refs: Vec<LedgerRef>,
    /// 知らない項目。
    #[serde(flatten)]
    pub extra: UnknownFields,
}

serialize_with_extra!(LedgerInputs {
    prefilter,
    splitter,
    correction,
    forgets_seen,
    input_hash,
    role_fallback,
    role_model_key,
    through,
    corr,
    refs,
});

/// 呼び出しのトークン数と時間 (04 §5.2.1 の `tokens`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerTokens {
    /// 入力 (プロンプト) のトークン数。
    pub input: u32,
    /// 出力 (生成) のトークン数。
    pub output: u32,
    /// 呼び出しにかかった時間 (ミリ秒)。
    pub ms: u64,
}

/// 台帳の `status` (04 §5.2.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LedgerStatus {
    Complete,
    /// 止められて、完結した要素だけを書いた (使える台帳。次の機会に呼び直して置き換える)。
    Partial,
    /// 型が合わない・4xx などで出力の無い台帳 (どの段でも使わない)。
    Failed,
    /// この版が知らない値 (新しい版の台帳)。台帳は読めるが、選び方で使わない。
    Unknown,
}

impl LedgerStatus {
    const NAMES: [(LedgerStatus, &'static str); 3] = [
        (LedgerStatus::Complete, "complete"),
        (LedgerStatus::Partial, "partial"),
        (LedgerStatus::Failed, "failed"),
    ];

    /// 保存の名前。[`LedgerStatus::Unknown`] には名前が無い。
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        Self::NAMES
            .iter()
            .find(|(s, _)| *s == self)
            .map(|(_, n)| *n)
    }

    /// 保存の名前から読む。知らない名前は [`LedgerStatus::Unknown`]。
    #[must_use]
    pub fn of(name: &str) -> Self {
        Self::NAMES
            .iter()
            .find(|(_, n)| *n == name)
            .map_or(LedgerStatus::Unknown, |(s, _)| *s)
    }
}

/// `proposal` の本体 (v1、04 §5.2.1 の「台帳の本体」)。項目は足すだけ (最上位か `output` の節に足す)。
///
/// 無い項目は既定値で読む (`status` は空で [`LedgerStatus::Unknown`]、`schema` と規則の版は 0、`sentences` は空)。
/// 項目を欠く台帳を、台帳の全体を読めない記録にせず、有効でない台帳の側に倒すため。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LedgerBody {
    /// 呼び出しの種類 ([`CALL_SEGMENT`])。`segment` 以外の台帳は、v0.5 の選び方と導出で読み飛ばす (残す)。
    pub call: String,
    /// 区切りの ID (`call` が `segment` のときだけ必須)。
    pub segment: Option<Id>,
    /// 出力の型の版 ([`LEDGER_SCHEMA`])。
    pub schema: u32,
    /// プロンプトの変種。
    pub variant: String,
    /// 実際に呼んだモデル。
    pub model_key: String,
    /// 文番号 → 根拠の文の位置 (`call` が `segment` のときだけ必須)。文番号の順。
    #[serde(default)]
    pub sentences: Vec<SentenceSpan>,
    pub inputs: LedgerInputs,
    /// 節ごとに型が合った部分 (wire の形のまま) と、合わなかった節の理由。
    #[serde(default)]
    pub output: LedgerOutput,
    /// `complete`・`partial`・`failed` ([`LedgerBody::status`])。
    pub status: String,
    /// 締切切れと 5xx で送り直した回数。
    pub attempts: u32,
    pub tokens: LedgerTokens,
    /// 忘れるの掃除で取り除いた要素の数 (04 §7.2)。
    pub scrubbed: u32,
    /// 区切りを分けた呼び出しのうち、やり直しの後も失敗して出力に入らなかった数 (無ければ 0)。0 より大きい
    /// `complete` の台帳は、`failed` と同じく model_key か `build_info` が変わったら呼び直す (04 §5.2.1 の
    /// `needs_call`)。
    pub failed_parts: u32,
    /// 知らない項目 (後の版が最上位に足した項目、04 §5.2.1 の「項目の足し方」)。
    #[serde(flatten)]
    pub extra: UnknownFields,
}

serialize_with_extra!(LedgerBody {
    call,
    segment,
    schema,
    variant,
    model_key,
    sentences,
    inputs,
    output,
    status,
    attempts,
    tokens,
    scrubbed,
    failed_parts,
});

impl LedgerBody {
    /// 台帳の `status`。知らない値は [`LedgerStatus::Unknown`]。
    #[must_use]
    pub fn status(&self) -> LedgerStatus {
        LedgerStatus::of(&self.status)
    }

    /// 区切りの呼び出し (B2) の台帳か (v0.5 の選び方と導出が読む台帳)。
    #[must_use]
    pub fn is_segment_call(&self) -> bool {
        self.call == CALL_SEGMENT
    }

    /// 出典のターンと改訂の番号 (`sentences` と `inputs` の `refs`。有効性と書く前の確かめと掃除の対象、§5.2.1)。
    #[must_use]
    pub fn sources(&self) -> BTreeSet<SrcRev> {
        self.sentences
            .iter()
            .map(|s| SrcRev {
                turn: s.turn,
                rev: s.rev,
            })
            .chain(self.inputs.refs.iter().map(|r| SrcRev {
                turn: r.turn,
                rev: r.rev,
            }))
            .collect()
    }
}

/// 台帳の頭 (呼び出しの種類と、選び方に要る項目)。台帳を読むときは、先にこれだけを読み、`call` が `segment` で
/// なければ本体を読まない (v0.5 は `segment` 以外の台帳を読み飛ばして残す、03 §3.3)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LedgerHead {
    pub call: String,
    pub segment: Option<Id>,
    pub schema: u32,
    pub variant: String,
    pub model_key: String,
    pub status: String,
}

impl LedgerHead {
    /// 区切りの呼び出し (B2) の台帳か。
    #[must_use]
    pub fn is_segment_call(&self) -> bool {
        self.call == CALL_SEGMENT
    }

    /// 台帳の `status`。知らない値は [`LedgerStatus::Unknown`]。
    #[must_use]
    pub fn status(&self) -> LedgerStatus {
        LedgerStatus::of(&self.status)
    }
}

/// 台帳の記録の読み方 ([`read_ledger`])。
#[derive(Debug, Clone, PartialEq)]
pub enum LedgerRead {
    /// 区切りの呼び出しの台帳。
    Segment(Box<LedgerBody>),
    /// `segment` 以外の呼び出しの台帳 (読み飛ばして残す)。
    OtherCall(LedgerHead),
    /// 区切りの呼び出しの台帳だが、本体を型として読めない (有効でない台帳として扱う。消さない)。
    Unreadable(LedgerHead),
}

/// 台帳の記録を読む。先に頭 ([`LedgerHead`]) を読み、`call` が `segment` のときだけ本体を読む。頭も読めなければ
/// `None` (本体が map でない)。
#[must_use]
pub fn read_ledger(e: &Envelope) -> Option<LedgerRead> {
    let head: LedgerHead = e.body_as().ok()?;
    if !head.is_segment_call() {
        return Some(LedgerRead::OtherCall(head));
    }
    Some(match e.body_as::<LedgerBody>() {
        Ok(body) => LedgerRead::Segment(Box::new(body)),
        Err(_) => LedgerRead::Unreadable(head),
    })
}

/// 台帳の `output` を書き換えられない理由。
#[derive(Debug, thiserror::Error)]
pub enum LedgerOutputError {
    /// 節の名前が予約の名前 ([`LedgerOutput::INVALID`]) である。
    #[error("`{0}` is reserved and is not a section name")]
    Reserved(String),
    /// 合わなかった節の理由 ([`LedgerOutput::INVALID`]) が読めない形で入っている (上書きしない)。
    #[error("the reasons of the invalid sections cannot be read")]
    UnreadableInvalid,
    #[error(transparent)]
    Body(#[from] BodyError),
}

/// 台帳の `output` (04 §5.2 の「出力の型」の wire の節の名前 → その節の値)。節ごとに型を確かめ、合った節だけを
/// 入れる。合わなかった節は、名前 [`LedgerOutput::INVALID`] の下に理由だけを書く。知らない節も残す。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LedgerOutput(BTreeMap<String, RawValue>);

/// 寛容に読んだ 1 つの節 ([`LedgerOutput::units`] など)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section<T> {
    /// 型の合った要素 (元の順)。
    pub items: Vec<T>,
    /// 型が合わずに捨てた要素の数 (必須の項目を欠く・知らない列挙の値・型の違う項目)。
    pub mismatched: usize,
}

impl<T> Default for Section<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            mismatched: 0,
        }
    }
}

impl LedgerOutput {
    /// 節: 検索の鍵 (区切りの鍵、§5.5)。
    pub const KEYS: &'static str = "keys";
    /// 節: 単位の候補。
    pub const UNITS: &'static str = "units";
    /// 節: 訂正の候補ごとの判定。
    pub const CORRECTIONS: &'static str = "corrections";
    /// 型の合わなかった節の名前 → 理由。
    pub const INVALID: &'static str = "invalid";

    /// 型の合った節を入れる (同じ名前の節は置き換える。合わなかった理由があれば消す)。
    ///
    /// # Errors
    /// 名前が予約の名前のとき、合わなかった理由が読めない形のとき、値を CBOR にできないとき。
    pub fn set_section<T: Serialize>(
        &mut self,
        name: &str,
        value: &T,
    ) -> Result<(), LedgerOutputError> {
        Self::check_name(name)?;
        let mut invalid = self.read_invalid()?;
        self.0
            .insert(name.to_owned(), RawValue::from_serialize(value)?);
        if invalid.remove(name).is_some() {
            self.put_invalid(&invalid)?;
        }
        Ok(())
    }

    /// 型の合わなかった節の理由を書く (その節の値は入れない)。
    ///
    /// # Errors
    /// 名前が予約の名前のとき、合わなかった理由が読めない形のとき、値を CBOR にできないとき。
    pub fn set_invalid(&mut self, name: &str, reason: &str) -> Result<(), LedgerOutputError> {
        Self::check_name(name)?;
        let mut invalid = self.read_invalid()?;
        self.0.remove(name);
        invalid.insert(name.to_owned(), reason.to_owned());
        Ok(self.put_invalid(&invalid)?)
    }

    fn check_name(name: &str) -> Result<(), LedgerOutputError> {
        if name == Self::INVALID {
            return Err(LedgerOutputError::Reserved(name.to_owned()));
        }
        Ok(())
    }

    /// 合わなかった節の理由 (無ければ空)。読めない形なら `Err`。
    fn read_invalid(&self) -> Result<BTreeMap<String, String>, LedgerOutputError> {
        match self.0.get(Self::INVALID) {
            None => Ok(BTreeMap::new()),
            Some(v) => v.to().map_err(|_| LedgerOutputError::UnreadableInvalid),
        }
    }

    fn put_invalid(&mut self, invalid: &BTreeMap<String, String>) -> Result<(), BodyError> {
        if invalid.is_empty() {
            self.0.remove(Self::INVALID);
        } else {
            self.0
                .insert(Self::INVALID.to_owned(), RawValue::from_serialize(invalid)?);
        }
        Ok(())
    }

    /// 型の合わなかった節の名前 → 理由。読めなければ空。
    #[must_use]
    pub fn invalid(&self) -> BTreeMap<String, String> {
        self.0
            .get(Self::INVALID)
            .and_then(|v| v.to().ok())
            .unwrap_or_default()
    }

    /// 節があるか (型の合った節として入っているか)。
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        name != Self::INVALID && self.0.contains_key(name)
    }

    /// 節の名前 (知らない節を含む。[`LedgerOutput::INVALID`] は除く)。
    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.0
            .keys()
            .map(String::as_str)
            .filter(|n| *n != Self::INVALID)
    }

    /// 節の保存した値 (wire の形のまま。台帳の掃除が要素ごとに除くために読む、04 §7.2)。
    pub(crate) fn raw_section(&self, name: &str) -> Option<&RawValue> {
        (name != Self::INVALID).then(|| self.0.get(name)).flatten()
    }

    /// 節の値を、保存した値のまま置き換える (台帳の掃除が残した要素で書き戻す)。予約の名前には書かない。
    pub(crate) fn put_raw_section(&mut self, name: &str, value: RawValue) {
        if name != Self::INVALID {
            self.0.insert(name.to_owned(), value);
        }
    }

    /// 合わなかった節の理由 (読めない形なら `None`。台帳の掃除が理由の文字列にも当てるため、04 §7.2)。
    pub(crate) fn invalid_reasons(&self) -> Option<BTreeMap<String, String>> {
        self.read_invalid().ok()
    }

    /// 合わなかった節の理由を置き換える (空なら理由の項目を除く)。
    pub(crate) fn replace_invalid_reasons(
        &mut self,
        reasons: &BTreeMap<String, String>,
    ) -> Result<(), BodyError> {
        self.put_invalid(reasons)
    }

    /// 節を除く (台帳の掃除で、並びでない節の全体を除くとき)。
    pub(crate) fn remove_raw_section(&mut self, name: &str) {
        if name != Self::INVALID {
            self.0.remove(name);
        }
    }

    /// 節を要素ごとに型 `T` で読む。節が無ければ空。並びでない節は、要素が 1 つの型の合わない節として数える。
    fn section<T: DeserializeOwned>(&self, name: &str) -> Section<T> {
        let Some(raw) = self.0.get(name) else {
            return Section::default();
        };
        let Some(elements) = raw.elements() else {
            return Section {
                items: Vec::new(),
                mismatched: 1,
            };
        };
        let mut out = Section::default();
        for e in elements {
            match e.to::<T>() {
                Ok(item) => out.items.push(item),
                Err(_) => out.mismatched += 1,
            }
        }
        out
    }

    /// 検索の鍵 (文字列でない要素は型が合わないとして数える)。
    #[must_use]
    pub fn keys(&self) -> Section<String> {
        self.section(Self::KEYS)
    }

    /// 単位の候補 (読むときの型 [`LedgerUnit`])。
    #[must_use]
    pub fn units(&self) -> Section<LedgerUnit> {
        self.section(Self::UNITS)
    }

    /// 訂正の判定 (読むときの型 [`LedgerCorrection`])。
    #[must_use]
    pub fn corrections(&self) -> Section<LedgerCorrection> {
        self.section(Self::CORRECTIONS)
    }
}

/// 層 (`l`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerLayer {
    /// 今も続く事実・状態 (読むときの既定)。
    #[default]
    World,
    /// 一度の出来事。
    Episode,
}

/// 叙法 (`m`)。v0.4 と同じ 4 つ。値を足すのは「足すだけ」の変更として許す (知らない値の要素は捨てて数える)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerMood {
    /// 言い切り (読むときの既定)。
    #[default]
    Fact,
    Negated,
    Question,
    Hypothetical,
}

/// 時制 (`tn`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerTense {
    /// 読むときの既定。
    #[default]
    Current,
    Past,
    Planned,
}

/// 訂正の判定 (`verdict`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerVerdict {
    Accept,
    Reject,
    Unclear,
}

fn self_entity() -> String {
    "self".to_owned()
}

/// 単位の候補の、読むときの型 (04 §5.2 の「読むときの型」)。`s`・`a`・`v` を欠く要素は型が合わないとして捨てる。
/// ほかの項目は既定値で読む。知らない項目は読み飛ばす (保存した節には残る)。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LedgerUnit {
    #[serde(rename = "s")]
    pub sentence: u32,
    #[serde(rename = "l", default)]
    pub layer: LedgerLayer,
    #[serde(rename = "e", default = "self_entity")]
    pub entity: String,
    #[serde(rename = "a")]
    pub aspect: String,
    #[serde(rename = "v")]
    pub value: String,
    #[serde(rename = "m", default)]
    pub mood: LedgerMood,
    #[serde(rename = "tn", default)]
    pub tense: LedgerTense,
    #[serde(default)]
    pub old: String,
    #[serde(default)]
    pub when: String,
}

/// 訂正の判定の、読むときの型。`id`・`verdict` を欠く要素は型が合わないとして捨てる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct LedgerCorrection {
    pub id: u32,
    pub verdict: LedgerVerdict,
}

/// 単位の候補の、書くときの型 (B2 の文法。すべての項目が必須で、無いものは "")。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerUnitOut {
    #[serde(rename = "s")]
    pub sentence: u32,
    #[serde(rename = "l")]
    pub layer: LedgerLayer,
    #[serde(rename = "e")]
    pub entity: String,
    #[serde(rename = "a")]
    pub aspect: String,
    #[serde(rename = "v")]
    pub value: String,
    #[serde(rename = "m")]
    pub mood: LedgerMood,
    #[serde(rename = "tn")]
    pub tense: LedgerTense,
    pub old: String,
    pub when: String,
}

/// 訂正の判定の、書くときの型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerCorrectionOut {
    pub id: u32,
    pub verdict: LedgerVerdict,
}

#[derive(Deserialize)]
struct SegmentField {
    #[serde(default)]
    segment: Option<Id>,
}

#[derive(Deserialize)]
struct SelectionFields {
    model_key: String,
    variant: String,
}

#[derive(Deserialize)]
struct TurnOnly {
    turn: Id,
}

#[derive(Deserialize)]
struct SentencesField {
    #[serde(default)]
    sentences: Vec<TurnOnly>,
}

#[derive(Deserialize)]
struct RefsOnly {
    #[serde(default)]
    refs: Vec<TurnOnly>,
}

#[derive(Deserialize)]
struct InputsField {
    inputs: RefsOnly,
}

/// `proposal` の関係と副索引 (repair で本体から作り直すときにも使う)。
pub(crate) fn derive_ledger(e: &Envelope) -> Vec<WriteOp> {
    let mut ops = Vec::new();
    if let Ok(SegmentField { segment: Some(s) }) = e.body_as::<SegmentField>() {
        ops.push(link(e.id, REL_LEDGER_SEGMENT, s));
        ops.push(lookup(
            KIND_PROPOSAL,
            LOOKUP_LEDGER_SEGMENT,
            OwnedPart::Id(s),
            e.id,
        ));
    }
    if let Ok(b) = e.body_as::<SelectionFields>() {
        ops.push(lookup(
            KIND_PROPOSAL,
            LOOKUP_LEDGER_SELECTION,
            OwnedPart::Str(selection_key(&b.model_key, &b.variant)),
            e.id,
        ));
    }
    let mut turns = BTreeSet::new();
    if let Ok(b) = e.body_as::<SentencesField>() {
        turns.extend(b.sentences.into_iter().map(|s| s.turn));
    }
    if let Ok(b) = e.body_as::<InputsField>() {
        turns.extend(b.inputs.refs.into_iter().map(|r| r.turn));
    }
    ops.extend(turns.into_iter().map(|t| link(e.id, REL_LEDGER_TURN, t)));
    ops
}

/// 台帳を書けない理由。
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// 置き換える前の記録が台帳ではない。
    #[error("record {0} is not a ledger")]
    NotALedger(Id),
    #[error(transparent)]
    Segment(#[from] SegmentError),
    #[error(transparent)]
    Body(#[from] BodyError),
}

/// 台帳と同じコミットで書く区切り (04 §5.2.1、03 §3.4)。
#[derive(Debug, Clone, Copy)]
pub struct SegmentUpdate<'a> {
    pub body: &'a SegmentBody,
    /// 読み直した前の区切りの記録 (無ければ新しく作る)。
    pub previous: Option<&'a Envelope>,
}

impl Writes<'_> {
    /// 台帳を書く依頼 (03 §3.4 の「台帳」)。1 つの `Immediate` のコミットで、次をまとめて書く: 同じ区切りの `segment`
    /// の更新 (`segment`)、置き換える前の台帳の削除 (`replaces`。末尾の区切り・`partial` の置き換え、04 §5.2.1)、
    /// 新しい台帳とその関係・副索引。会話の追記より後に回す (裏の処理の待ち行列)。
    ///
    /// 書く前の確かめとして、`sentences` と `refs` のターンが残っていて、改訂の番号が台帳のものと合うことを、書き手の
    /// トランザクションの中で確かめる (合わなければ依頼の全体を断る。呼び出しの間に伏せ字にしたターンの台帳を書かない、
    /// 04 §7.5 の「読んだ後に足された忘れる」)。
    ///
    /// # Errors
    /// `replaces` に台帳でない記録があるとき、区切りを書けないとき、本体を CBOR にできないとき。
    pub fn ledger(
        &self,
        ledger: &LedgerBody,
        segment: Option<SegmentUpdate<'_>>,
        replaces: &[Envelope],
    ) -> Result<(Id, WriteRequest), LedgerError> {
        let mut ops: Vec<WriteOp> = ledger
            .sources()
            .into_iter()
            .map(|s| expect_turn(s.turn, s.rev))
            .collect();
        if let Some(s) = segment {
            ops.extend(self.segment(s.body, s.previous)?.1);
        }
        for old in replaces {
            if old.kind != KIND_PROPOSAL {
                return Err(LedgerError::NotALedger(old.id));
            }
            // 置き換える前の台帳が残っていること (同じ依頼を送り直して、台帳が 2 つ残らないため)。
            ops.push(expect_record(KIND_PROPOSAL, old.id));
            ops.extend(delete_ops(old, derive_ledger));
        }
        let id = self.ids.next_id();
        ops.extend(
            Record {
                kind: KIND_PROPOSAL,
                v: PROPOSAL_V,
                id,
                now: self.clock.now(),
                body: ledger,
                derive: derive_ledger,
            }
            .new_ops()?,
        );
        Ok((id, WriteRequest::background_durable(ops)))
    }
}
