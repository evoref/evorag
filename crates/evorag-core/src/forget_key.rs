//! 値の鍵と再び覚えない印の HMAC の鍵 (`meta` の `memory.forget_key`、03 §3.1.1、04 §5.6)。
//!
//! 書き込みでデータ根を開いたときに確かめる (ジャーナルの取り込みの後、裏の処理の前)。
//! - あれば読む。
//! - 無く、事実への指示の記録も無ければ、OS の乱数で作り、会話と同じ優先度で一度だけ書く。
//! - 食い違い・形の不正・「指示があるのに鍵が無い」ときは、作らずに事実の機能を止める (フェイルクローズ)。

use evorag_memory::{ForgetKey, KIND_DIRECTIVE, KIND_FACT_DIRECTIVE};
use evorag_store::{Read, Store, WriteOp, WriteRequest, WriterHandle, meta};

/// 鍵が使えない理由 (03 §3.1.1 のフェイルクローズ)。事実の抽出・統合・事実の表を止める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactsKeyProblem {
    /// ジャーナルの鍵が 2 つ以上あるか、DB の値と違う。
    Conflict,
    /// DB とジャーナルのどちらにも鍵が無いのに、事実への指示の記録がある。
    MissingWithDirectives,
    /// 記録の鍵の形が正しくない (64 桁の小文字の 16 進でない)。
    Malformed,
    /// 読むだけで開いた (鍵を作れない・ジャーナルにだけある鍵を入れられない)。
    ReadOnly,
}

/// 鍵を確かめた結果。
#[derive(Debug)]
pub enum FactsKey {
    /// 使える鍵。`created` は、この起動で作った。
    Ready {
        key: ForgetKey,
        created: bool,
    },
    Disabled(FactsKeyProblem),
}

impl FactsKey {
    pub fn key(&self) -> Option<&ForgetKey> {
        match self {
            FactsKey::Ready { key, .. } => Some(key),
            FactsKey::Disabled(_) => None,
        }
    }

    pub fn problem(&self) -> Option<FactsKeyProblem> {
        match self {
            FactsKey::Ready { .. } => None,
            FactsKey::Disabled(p) => Some(*p),
        }
    }
}

/// 指示の記録があるか (読めない記録も「ある」と数える。安全の側に倒す)。v0.4 の根の `fact_directive` と、v0.5 の
/// 根の `directive` (03 §3.1.1 のフェイルクローズ) の両方を見る (それぞれの根には、もう片方の種類の記録は無い)。
fn has_directives(store: &Store) -> Result<bool, String> {
    let tx = store.read().map_err(|e| e.to_string())?;
    let mut found = false;
    for kind in [KIND_FACT_DIRECTIVE, KIND_DIRECTIVE] {
        tx.for_each(kind, |r| {
            found = matches!(r, Read::Ok(_) | Read::Newer { .. } | Read::Corrupt { .. });
            !found
        })
        .map_err(|e| e.to_string())?;
        if found {
            break;
        }
    }
    Ok(found)
}

/// 鍵を確かめ、無ければ作る。`writer` が `None` (読むだけで開いた) なら作らない。
///
/// # Errors
/// DB を読めない・書けないとき (起動を止める)。
pub fn ensure(store: &Store, writer: Option<&WriterHandle>) -> Result<FactsKey, String> {
    if store.forget_key_conflict() {
        return Ok(FactsKey::Disabled(FactsKeyProblem::Conflict));
    }
    if let Some(hex) = store.meta(meta::FORGET_KEY).map_err(|e| e.to_string())? {
        return Ok(match ForgetKey::from_hex(&hex) {
            Some(key) => FactsKey::Ready {
                key,
                created: false,
            },
            None => FactsKey::Disabled(FactsKeyProblem::Malformed),
        });
    }
    if has_directives(store)? {
        return Ok(FactsKey::Disabled(FactsKeyProblem::MissingWithDirectives));
    }
    let Some(writer) = writer else {
        return Ok(FactsKey::Disabled(FactsKeyProblem::ReadOnly));
    };
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("os random source: {e}"))?;
    let key = ForgetKey::from_bytes(bytes);
    writer
        .submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaOnce {
            key: meta::FORGET_KEY.into(),
            value: key.to_hex(),
        }]))
        .map_err(|e| e.to_string())?;
    Ok(FactsKey::Ready { key, created: true })
}
