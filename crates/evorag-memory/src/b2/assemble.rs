//! 呼び出しの結果から台帳の本体を組み立てる (04 §5.2.1 の「台帳の本体」「分けた呼び出しのまとめ方」)。

use evorag_types::Id;

use super::VARIANT;
use super::output::Parsed;
use super::request::{Call, CallerInputs};
use crate::ledger::{
    CALL_SEGMENT, LEDGER_SCHEMA, LedgerBody, LedgerOutputError, LedgerStatus, LedgerTokens,
};

/// 送った呼び出し 1 つの結果 ([`Call::requests`] の順)。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 型の合った節のある出力 ([`super::Reading::Complete`]・[`super::Reading::Partial`] の中身)。
    Parsed(Parsed),
    /// やり直しの後も型が合わない・切れた・4xx・5xx が続いた。この呼び出しは何も出さず、その文・候補・応答は台帳の
    /// 入力から外す。中身は合わなかった理由 ([`super::Reading::Invalid`] の中身か [`Parsed::failed`])。
    Failed(Parsed),
}

/// 台帳の本体の、呼び出しの結果に依らない項目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerMeta<'a> {
    /// 区切りの ID。
    pub segment: Id,
    /// 実際に呼んだモデル。
    pub model_key: &'a str,
    /// 呼んだ先の llama-server の `build_info` (台帳の `build_info`。サーバが返さなければ `None` で、null を書く)。
    pub build_info: Option<&'a str>,
    pub caller: &'a CallerInputs,
    /// 締切切れと 5xx で送り直した回数。
    pub attempts: u32,
    /// 入力と出力のトークン数と時間 (送った呼び出しの合計)。
    pub tokens: LedgerTokens,
}

/// 台帳を書けない理由。
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    /// 呼び出しがあるのに、結果が 1 つも無い (止められて完結した要素が無い。台帳を書かない)。
    #[error("no call has a result")]
    NoResult,
    /// 結果の数が呼び出しの数より多い。
    #[error("{results} results for {requests} requests")]
    TooManyResults { results: usize, requests: usize },
    #[error(transparent)]
    Output(#[from] LedgerOutputError),
}

/// 区切りの呼び出し `call` と、送った呼び出しの結果 `results` (前から順に。止めた後の呼び出しは含めない) から、
/// 台帳の本体を組み立てる (04 §5.2.1)。
///
/// - 呼び出しの無い区切り (番号を振った文が無い) は、3 つの節が空の並びの `complete` で、`input_hash` は null、
///   トークン数は 0 にする (`results` と `meta.tokens` は見ない)。
/// - 型の合った呼び出しの出力を順にまとめ ([`Parsed::merge`])、台帳の `sentences`・`corr`・`refs`・
///   `input_hash` はその呼び出しだけで作る。失敗した呼び出しの数を `failed_parts` に書く。
/// - `status`: どれかの呼び出しが止められたか、送らなかった呼び出しが残れば `partial`、ほかは `complete`。
///   すべての呼び出しが失敗したときだけ `failed` (出力は合わなかった理由だけ。入力は送った呼び出しのもの)。
///
/// # Errors
/// 呼び出しがあるのに結果が無いとき、結果が呼び出しより多いとき、出力を CBOR にできないとき。
pub fn ledger_body(
    call: &Call,
    results: &[Outcome],
    meta: &LedgerMeta<'_>,
) -> Result<LedgerBody, AssembleError> {
    let mut body = LedgerBody {
        call: CALL_SEGMENT.to_owned(),
        segment: Some(meta.segment),
        schema: LEDGER_SCHEMA,
        variant: VARIANT.to_owned(),
        model_key: meta.model_key.to_owned(),
        build_info: meta.build_info.map(str::to_owned),
        attempts: meta.attempts,
        ..LedgerBody::default()
    };
    if call.requests.is_empty() {
        body.inputs = call.inputs_for(&[], meta.caller);
        body.output = Parsed::empty().to_output()?;
        body.status = status_name(LedgerStatus::Complete);
        return Ok(body);
    }
    if results.is_empty() {
        return Err(AssembleError::NoResult);
    }
    if results.len() > call.requests.len() {
        return Err(AssembleError::TooManyResults {
            results: results.len(),
            requests: call.requests.len(),
        });
    }
    let mut used = Vec::new();
    let mut merged: Option<Parsed> = None;
    let mut failed: Option<Parsed> = None;
    let mut failed_parts = 0u32;
    for (i, r) in results.iter().enumerate() {
        match r {
            Outcome::Parsed(p) => {
                used.push(i);
                merged = Some(match merged {
                    Some(m) => m.merge(p.clone()),
                    None => p.clone(),
                });
            }
            Outcome::Failed(p) => {
                failed_parts += 1;
                failed = Some(match failed {
                    Some(f) => f.merge(p.clone()),
                    None => p.clone(),
                });
            }
        }
    }
    let (parsed, status) = match merged {
        Some(m) => {
            let partial =
                m.status() == LedgerStatus::Partial || results.len() < call.requests.len();
            let status = if m.status() == LedgerStatus::Failed {
                LedgerStatus::Failed
            } else if partial {
                LedgerStatus::Partial
            } else {
                LedgerStatus::Complete
            };
            (m, status)
        }
        None => {
            used = (0..results.len()).collect();
            (
                failed.unwrap_or_else(|| Parsed::failed(super::InvalidReason::NotJson)),
                LedgerStatus::Failed,
            )
        }
    };
    body.sentences = call.sentences_for(&used);
    body.inputs = call.inputs_for(&used, meta.caller);
    body.output = parsed.to_output()?;
    body.status = status_name(status);
    body.tokens = meta.tokens;
    body.failed_parts = failed_parts;
    Ok(body)
}

fn status_name(s: LedgerStatus) -> String {
    s.name().unwrap_or_default().to_owned()
}
