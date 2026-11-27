//! B2 の出力の読み取り (04 §5.2 の「出力の型」、§5.2.1 の「型が合わないとき」「止められたとき」)。I/O を持たない。
//!
//! - 節 (`keys`・`units`・`corrections`) ごとに型を確かめ、並びの節だけを台帳に入れる。合わない節は固定の符号
//!   ([`InvalidReason`]) だけを書き、LLM の出力の文字列を書かない。
//! - 要素は読むときの型 ([`LedgerUnit`]・[`LedgerCorrection`]・文字列) で確かめ、合わない要素と上限 (`maxItems`)
//!   を超えた要素を捨てて数える ([`Tally`])。残した要素は wire の形のまま (知らない項目を含めて) 入れる。`s` と
//!   `id` の範囲と文字列の長さは見ない (導出の関門で捨てて数える、04 §5.2・§5.5・§6)。
//! - 止められた出力 ([`StreamEnd::Stopped`]) は、型の上で完結した部分 (閉じた `keys` の節と、`units`・
//!   `corrections` の完結した要素) を採り、`partial` にする (OQ-53 の 10)。

use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use super::request::Request;
use super::{KEYS_MAX, UNITS_MAX};
use crate::ledger::{LedgerCorrection, LedgerOutput, LedgerOutputError, LedgerStatus, LedgerUnit};

/// 生成の終わり方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// 生成を終えた (`finish_reason` が `stop`)。
    Finished,
    /// 利用者の操作で止めた (04 §4 の「譲る」)。
    Stopped,
    /// 出力の上限で切れた (`finish_reason` が `length`)。
    Length,
}

/// 型の合わない節の理由 (台帳の `output` の `invalid` に書く固定の符号、04 §5.2.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvalidReason {
    /// 出力の全体が JSON として読めない。
    NotJson,
    /// 出力の全体が JSON のオブジェクトではない。
    NotObject,
    /// 節が無い。
    Missing,
    /// 節が並びではない。
    NotArray,
    /// 出力が上限で切れた (1 つの往復の呼び出しで、やり直しても切れた)。
    Truncated,
    /// 要求が受け付けられなかった (4xx)。
    RequestRejected,
    /// 締切切れか 5xx が、同じ区切りで 3 回続いた。
    ServerError,
}

impl InvalidReason {
    /// 台帳に書く符号。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotJson => "not_json",
            Self::NotObject => "not_object",
            Self::Missing => "missing",
            Self::NotArray => "not_array",
            Self::Truncated => "truncated",
            Self::RequestRejected => "request_rejected",
            Self::ServerError => "server_error",
        }
    }
}

impl fmt::Display for InvalidReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 1 つの節で残した要素と捨てた要素の数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SectionTally {
    /// 残した要素。
    pub kept: usize,
    /// 読むときの型に合わない (必須の項目を欠く・知らない列挙の値・型の違う項目・壊れた要素)。
    pub mismatched: usize,
    /// 上限 (`maxItems`。区切りを分けてまとめたときの `keys` の重複を含む) を超えた。
    pub over_limit: usize,
}

impl SectionTally {
    fn add(&mut self, other: Self) {
        self.kept += other.kept;
        self.mismatched += other.mismatched;
        self.over_limit += other.over_limit;
    }
}

/// 読み取りで数えたもの (ログと `doctor` に出す。台帳には書かない)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub keys: SectionTally,
    pub units: SectionTally,
    pub corrections: SectionTally,
    /// 知らない節 (読み飛ばした)。
    pub unknown_sections: usize,
}

/// 節の状態。
#[derive(Debug, Clone, PartialEq)]
enum Section {
    /// 型の合った節 (残した要素を wire の形のまま)。
    Items(Vec<Value>),
    Invalid(InvalidReason),
}

/// 節の名前 (この順に並べる)。
const NAMES: [&str; 3] = [
    LedgerOutput::KEYS,
    LedgerOutput::UNITS,
    LedgerOutput::CORRECTIONS,
];

/// 読み取った出力 (台帳の `output` と `status` の元)。区切りを分けて呼んだときは [`Read::merge`] でまとめる。
#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    /// [`NAMES`] の順。`None` は節が無い (止められて届かなかった)。
    sections: [Option<Section>; 3],
    partial: bool,
    tally: Tally,
}

/// 出力の読み取りの結果 ([`read_output`])。
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// 型の合った節が 1 つ以上ある (台帳の `status` は `complete`。合わない節は理由だけを書く)。
    Complete(Read),
    /// 止められた。完結した要素を採った (`status` は `partial`)。
    Partial(Read),
    /// 全体が JSON として読めないか、どの節も型が合わない。1 回だけやり直し、それでも合わなければ、この `Read`
    /// (`status` は `failed`。節ごとの理由つき) で台帳を書く (04 §5.2.1 の「型が合わないとき」)。
    Invalid(Read),
    /// 出力が上限で切れた。区切りを往復の境で半分に分けて送り直す ([`super::Plan::halve`])。往復が 1 つなら
    /// [`Reading::Invalid`] と同じに扱い、やり直しても切れたら [`Read::failed`] (`truncated`) で書く。
    Truncated,
    /// 止められたが、完結した要素が 1 つも無い (台帳を書かない)。
    Nothing,
}

impl Read {
    /// 番号を振った文の無い区切りの、出力が空の台帳 (3 つの節が空の並び。`status` は `complete`、04 §5.2.1)。
    #[must_use]
    pub fn empty() -> Self {
        Self {
            sections: [
                Some(Section::Items(Vec::new())),
                Some(Section::Items(Vec::new())),
                Some(Section::Items(Vec::new())),
            ],
            partial: false,
            tally: Tally::default(),
        }
    }

    /// 出力の無い、すべての節が `reason` で合わない読み取り (`status` は `failed`)。4xx・5xx の続き・切れたままの
    /// 出力の台帳に使う。
    #[must_use]
    pub fn failed(reason: InvalidReason) -> Self {
        Self {
            sections: [
                Some(Section::Invalid(reason)),
                Some(Section::Invalid(reason)),
                Some(Section::Invalid(reason)),
            ],
            partial: false,
            tally: Tally::default(),
        }
    }

    /// 台帳の `status`: 型の合った節が無ければ `failed`、止められたものを含めば `partial`、ほかは `complete`。
    #[must_use]
    pub fn status(&self) -> LedgerStatus {
        if !self
            .sections
            .iter()
            .any(|s| matches!(s, Some(Section::Items(_))))
        {
            LedgerStatus::Failed
        } else if self.partial {
            LedgerStatus::Partial
        } else {
            LedgerStatus::Complete
        }
    }

    /// 数えたもの。
    #[must_use]
    pub fn tally(&self) -> &Tally {
        &self.tally
    }

    /// 区切りを分けた呼び出しの読み取りを、呼び出しの順にまとめる (04 §5.2.1 の「収まらない区切り」)。
    ///
    /// - どちらにも合った節は、要素を順につなぐ。`keys` は重複を除いて先の呼び出しのものから 8 個まで。
    /// - どちらかで合わなかった節は、先に合わなかった理由で合わない節にする (区切りの一部の要素だけを、節の全体の
    ///   ように見せないため)。
    /// - 片方に無い節 (止められて届かなかった) は、もう片方のものを使う。
    #[must_use]
    pub fn merge(self, next: Self) -> Self {
        let mut tally = self.tally;
        tally.keys.add(next.tally.keys);
        tally.units.add(next.tally.units);
        tally.corrections.add(next.tally.corrections);
        tally.unknown_sections += next.tally.unknown_sections;
        let mut sections: [Option<Section>; 3] = [None, None, None];
        for (k, (a, b)) in self.sections.into_iter().zip(next.sections).enumerate() {
            sections[k] = match (a, b) {
                (Some(Section::Invalid(r)), _) | (_, Some(Section::Invalid(r))) => {
                    Some(Section::Invalid(r))
                }
                (Some(Section::Items(mut x)), Some(Section::Items(y))) => {
                    x.extend(y);
                    Some(Section::Items(x))
                }
                (x, None) => x,
                (None, y) => y,
            };
        }
        if let Some(Section::Items(keys)) = &mut sections[0] {
            let before = keys.len();
            let mut seen: Vec<Value> = Vec::with_capacity(before);
            keys.retain(|k| {
                if seen.contains(k) || seen.len() >= KEYS_MAX {
                    false
                } else {
                    seen.push(k.clone());
                    true
                }
            });
            let dropped = before - keys.len();
            tally.keys.kept -= dropped.min(tally.keys.kept);
            tally.keys.over_limit += dropped;
        }
        Self {
            sections,
            partial: self.partial || next.partial,
            tally,
        }
    }

    /// 台帳の `output` にする (合った節は要素の並び、合わない節は `invalid` の下に符号)。
    ///
    /// # Errors
    /// 値を CBOR にできないとき。
    pub fn to_output(&self) -> Result<LedgerOutput, LedgerOutputError> {
        let mut out = LedgerOutput::default();
        for (name, s) in NAMES.iter().zip(&self.sections) {
            match s {
                Some(Section::Items(items)) => out.set_section(name, items)?,
                Some(Section::Invalid(r)) => out.set_invalid(name, r.as_str())?,
                None => {}
            }
        }
        Ok(out)
    }
}

/// 呼び出し `request` の出力 `text` を読む (04 §5.2・§5.2.1)。`end` は生成の終わり方。
///
/// - 全体が JSON のオブジェクトとして読めれば、終わり方に依らず節ごとに確かめる。型の合った節が 1 つも無ければ
///   [`Reading::Invalid`]。
/// - 読めないとき: 止められたなら完結した部分を採り ([`Reading::Partial`] か [`Reading::Nothing`])、上限で
///   切れたなら [`Reading::Truncated`]、ほかは [`Reading::Invalid`] (`not_json`)。
#[must_use]
pub fn read_output(request: &Request, text: &str, end: StreamEnd) -> Reading {
    let limits = Limits {
        corrections: request.candidate_ids().len(),
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(mut map)) => {
            let mut read = Read {
                sections: [None, None, None],
                partial: false,
                tally: Tally::default(),
            };
            for (k, name) in NAMES.iter().enumerate() {
                read.sections[k] = Some(match map.remove(*name) {
                    None => Section::Invalid(InvalidReason::Missing),
                    Some(Value::Array(items)) => limits.items(&mut read.tally, k, items),
                    Some(_) => Section::Invalid(InvalidReason::NotArray),
                });
            }
            read.tally.unknown_sections = map.len();
            if read.status() == LedgerStatus::Failed {
                Reading::Invalid(read)
            } else {
                Reading::Complete(read)
            }
        }
        Ok(_) => Reading::Invalid(Read::failed(InvalidReason::NotObject)),
        Err(_) => match end {
            StreamEnd::Stopped => salvage(text, limits),
            StreamEnd::Length => Reading::Truncated,
            StreamEnd::Finished => Reading::Invalid(Read::failed(InvalidReason::NotJson)),
        },
    }
}

/// 要素の数の上限 (`maxItems`)。
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// 送った訂正の候補の数。
    corrections: usize,
}

impl Limits {
    /// 並びの節 `k` の要素を確かめ、残す要素を返す。
    fn items(self, tally: &mut Tally, k: usize, items: Vec<Value>) -> Section {
        let (max, t) = match k {
            0 => (KEYS_MAX, &mut tally.keys),
            1 => (UNITS_MAX, &mut tally.units),
            _ => (self.corrections, &mut tally.corrections),
        };
        let mut kept = Vec::with_capacity(items.len().min(max));
        for v in items {
            let ok = match k {
                0 => v.is_string(),
                1 => LedgerUnit::deserialize(&v).is_ok(),
                _ => LedgerCorrection::deserialize(&v).is_ok(),
            };
            if !ok {
                t.mismatched += 1;
            } else if kept.len() >= max {
                t.over_limit += 1;
            } else {
                kept.push(v);
            }
        }
        t.kept += kept.len();
        Section::Items(kept)
    }
}

/// 止められた出力の、型の上で完結した部分 (OQ-53 の 10)。`keys` は閉じた節だけ、`units`・`corrections` は完結した
/// 要素ごとに採る。
fn salvage(text: &str, limits: Limits) -> Reading {
    let mut read = Read {
        sections: [None, None, None],
        partial: true,
        tally: Tally::default(),
    };
    let mut raw: [Option<Result<Vec<Value>, InvalidReason>>; 3] = [None, None, None];
    let mut unknown = 0usize;
    scan(text, &mut raw, &mut unknown);
    for (k, r) in raw.into_iter().enumerate() {
        read.sections[k] = r.map(|r| match r {
            Ok(items) => limits.items(&mut read.tally, k, items),
            Err(reason) => Section::Invalid(reason),
        });
    }
    read.tally.unknown_sections = unknown;
    let t = &read.tally;
    if t.keys.kept + t.units.kept + t.corrections.kept == 0 {
        Reading::Nothing
    } else {
        Reading::Partial(read)
    }
}

/// JSON の途中までを読む字句の走査。
struct Scanner<'a> {
    text: &'a str,
    at: usize,
}

impl Scanner<'_> {
    fn ws(&mut self) {
        let rest = &self.text[self.at..];
        self.at += rest.len() - rest.trim_start_matches([' ', '\t', '\n', '\r']).len();
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    /// `start` の文字列 (`"` から) の終わり (閉じの `"` の後)。閉じていなければ `None`。
    fn string_end(&self, start: usize) -> Option<usize> {
        let b = self.text.as_bytes();
        let mut i = start + 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    }

    /// `start` から始まる値の終わり。値が閉じていない (途中で切れた) なら `None`。
    fn value_end(&self, start: usize) -> Option<usize> {
        let b = self.text.as_bytes();
        match *b.get(start)? {
            b'"' => self.string_end(start),
            b'{' | b'[' => {
                let mut depth = 0usize;
                let mut i = start;
                while i < b.len() {
                    match b[i] {
                        b'"' => {
                            i = self.string_end(i)?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                None
            }
            _ => {
                // 数・true・false・null。後ろに区切りが来るまでは、続きがありうるので閉じていない。
                let len = b[start..]
                    .iter()
                    .position(|c| matches!(c, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r'))?;
                (len > 0).then_some(start + len)
            }
        }
    }

    /// 値を読んで進む。閉じていないか壊れていれば `None` (進まない)。
    fn value(&mut self) -> Option<Result<Value, ()>> {
        let end = self.value_end(self.at)?;
        let v = serde_json::from_str::<Value>(&self.text[self.at..end]).map_err(|_| ());
        self.at = end;
        Some(v)
    }
}

/// 最上位のオブジェクトを途中まで読み、節ごとの要素 (完結したもの) を `raw` に入れる。
fn scan(text: &str, raw: &mut [Option<Result<Vec<Value>, InvalidReason>>; 3], unknown: &mut usize) {
    let mut s = Scanner { text, at: 0 };
    s.ws();
    if s.peek() != Some(b'{') {
        return;
    }
    s.at += 1;
    'members: loop {
        s.ws();
        match s.peek() {
            Some(b',') => {
                s.at += 1;
                continue;
            }
            Some(b'"') => {}
            _ => return,
        }
        let Some(key_end) = s.string_end(s.at) else {
            return;
        };
        let Ok(key) = serde_json::from_str::<String>(&text[s.at..key_end]) else {
            return;
        };
        s.at = key_end;
        s.ws();
        if s.peek() != Some(b':') {
            return;
        }
        s.at += 1;
        s.ws();
        let slot = NAMES.iter().position(|n| *n == key);
        match (slot, s.peek()) {
            (_, None) => return,
            // units・corrections の並びは、要素ごとに採る。
            (Some(k @ (1 | 2)), Some(b'[')) => {
                s.at += 1;
                let mut items = Vec::new();
                loop {
                    s.ws();
                    match s.peek() {
                        Some(b']') => {
                            s.at += 1;
                            raw[k] = Some(Ok(items));
                            continue 'members;
                        }
                        Some(b',') => s.at += 1,
                        None => {
                            raw[k] = Some(Ok(items));
                            return;
                        }
                        Some(_) => match s.value() {
                            Some(Ok(v)) => items.push(v),
                            // 壊れた要素は、読むときの型に合わない要素として数える。
                            Some(Err(())) => items.push(Value::Null),
                            None => {
                                raw[k] = Some(Ok(items));
                                return;
                            }
                        },
                    }
                }
            }
            (slot, Some(_)) => {
                let Some(Ok(v)) = s.value() else {
                    return;
                };
                match (slot, v) {
                    (Some(k), Value::Array(items)) => raw[k] = Some(Ok(items)),
                    (Some(k), _) => raw[k] = Some(Err(InvalidReason::NotArray)),
                    (None, _) => *unknown += 1,
                }
            }
        }
    }
}
