//! 文脈の組み立て (05 §4.1・§4.2)。
//!
//! - 予算 = `n_ctx` − 生成に残す量 (既定 `max(1024, n_ctx の 25%)`)。
//! - 並べ方: システムプロンプト (凍結の核と選べる既定) → 履歴 → 今の発話 (引用・日時・本文)。
//!   前のターンの要求と先頭が一致するように組み立てる (ADR-0017)。
//! - 古い発言は、履歴が予算を超えたら、予算の 6 割になるまで往復の単位でまとめて省く。省いた位置は
//!   次のターンに引き継ぎ、予算を超えるまで動かさない (1 往復ずつ省くと、省くたびに全体を処理し直す)。
//! - トークンの数は `/tokenize` で数え、本文のハッシュと model_key でキャッシュする (06 §8、FR-506)。
//!   数えられないときは文字数から見積もり (日本語 1 文字 = 1、英数字 4 文字 = 1)、予算を 2 割減らす。

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use evorag_llm::{ChatMessage, LlmClient};
use evorag_types::Id;

/// 1 つの発言に足す、役割や区切りのぶんの見込み (トークン)。
const PER_MESSAGE_OVERHEAD: usize = 8;

/// トークンの数のキャッシュ。
#[derive(Default)]
pub struct TokenCache {
    counts: Mutex<HashMap<(u64, String), usize>>,
    /// `/tokenize` を呼んだ回数 (試験で確かめる)。
    calls: Mutex<usize>,
}

fn hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// 文字数からの見積もり (06 §8 の暫定)。
pub fn estimate(text: &str) -> usize {
    let (mut wide, mut narrow) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

impl TokenCache {
    /// 数える。`/tokenize` が使えないときは `None` (見積もりに縮退する)。
    pub async fn count(&self, llm: &dyn LlmClient, model_key: &str, text: &str) -> Option<usize> {
        let key = (hash(text), model_key.to_owned());
        if let Some(n) = self.lock().get(&key) {
            return Some(*n);
        }
        *self.calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let n = llm.count_tokens(text).await.ok()?;
        self.lock().insert(key, n);
        Some(n)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(u64, String), usize>> {
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 生成に残す量 (05 §4.1)。
pub fn reserve_for_output(n_ctx: usize, configured: Option<usize>) -> usize {
    configured.unwrap_or_else(|| (n_ctx / 4).max(1024))
}

/// 履歴の 1 つの発言。
pub struct Past {
    /// このターンの ID。
    pub turn: Id,
    /// 往復の ID (発話のターンの ID。応答なら、その発話)。省くときは往復ごとに省く。
    pub exchange: Id,
    pub message: ChatMessage,
}

/// 組み立てた要求の発言の列と、履歴に入れた最も古いターン。
pub struct Assembled {
    pub messages: Vec<ChatMessage>,
    /// 次のターンが履歴を始める位置 (05 §4.2)。応答のターンの観測値に残す。
    pub history_from: Id,
}

/// システムプロンプト・履歴 (古い順)・今の発話から、予算に入る発言の列を作る。
///
/// `from` は前のターンが履歴を始めた位置 (無ければセッションの最初から)。今の発話は必ず入れる。
#[allow(clippy::too_many_arguments)]
pub async fn assemble(
    llm: &dyn LlmClient,
    cache: &TokenCache,
    model_key: &str,
    n_ctx: usize,
    reserve: usize,
    system: ChatMessage,
    past: Vec<Past>,
    current: (Id, ChatMessage),
    from: Option<Id>,
) -> Assembled {
    let mut exact = true;
    let mut count = async |text: &str| match cache.count(llm, model_key, text).await {
        Some(n) => n,
        None => {
            exact = false;
            estimate(text)
        }
    };
    let system_cost = count(&system.content).await + PER_MESSAGE_OVERHEAD;
    let current_cost = count(&current.1.content).await + PER_MESSAGE_OVERHEAD;
    // 前のターンが省いた位置より前は数えもしない。
    let start = from.map_or(0, |f| {
        past.iter().position(|p| p.turn >= f).unwrap_or(past.len())
    });
    let mut costs = Vec::with_capacity(past.len() - start);
    for p in &past[start..] {
        costs.push(count(&p.message.content).await + PER_MESSAGE_OVERHEAD);
    }
    let mut budget = n_ctx.saturating_sub(reserve);
    if !exact {
        budget = budget * 4 / 5;
    }
    let history_budget = budget.saturating_sub(system_cost + current_cost);
    let mut total: usize = costs.iter().sum();
    let mut keep = start;
    if total > history_budget {
        // 予算の 6 割になるまで、往復ごとにまとめて省く。
        let target = history_budget * 6 / 10;
        let mut i = 0;
        while i < costs.len() && total > target {
            let ex = past[start + i].exchange;
            while i < costs.len() && past[start + i].exchange == ex {
                total -= costs[i];
                i += 1;
            }
        }
        keep = start + i;
    }
    let history_from = past.get(keep).map_or(current.0, |p| p.turn);
    let mut messages = Vec::with_capacity(past.len() - keep + 2);
    messages.push(system);
    messages.extend(past.into_iter().skip(keep).map(|p| p.message));
    messages.push(current.1);
    Assembled {
        messages,
        history_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_wide_and_narrow_characters() {
        assert_eq!(estimate("日本語"), 3);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn reserve_is_a_quarter_but_at_least_1024() {
        assert_eq!(reserve_for_output(8192, None), 2048);
        assert_eq!(reserve_for_output(2048, None), 1024);
        assert_eq!(reserve_for_output(8192, Some(100)), 100);
    }
}
