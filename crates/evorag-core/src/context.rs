//! 文脈の組み立て (05 §4.1)。v0.1 の M1.3 は、システムプロンプトと直近の会話だけ。
//!
//! - 予算 = `n_ctx` − 生成に残す量 (既定 `max(1024, n_ctx の 25%)`)。
//! - 直近の会話は、新しいものから入れ、入らない古いものを省く。
//! - トークンの数は `/tokenize` で数え、本文のハッシュと model_key でキャッシュする (06 §8、FR-506)。
//!   数えられないときは文字数から見積もり (日本語 1 文字 = 1、英数字 4 文字 = 1)、予算を 2 割減らす。

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use evorag_llm::{ChatMessage, LlmClient};

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

/// システムプロンプトと、直近の会話 (古い順) から、予算に入る発言の列を作る。
/// 最後の発言 (今の発話) は必ず入れる。
pub async fn assemble(
    llm: &dyn LlmClient,
    cache: &TokenCache,
    model_key: &str,
    n_ctx: usize,
    reserve: usize,
    system: ChatMessage,
    history: Vec<ChatMessage>,
) -> Vec<ChatMessage> {
    let mut exact = true;
    let mut count = async |text: &str| match cache.count(llm, model_key, text).await {
        Some(n) => n,
        None => {
            exact = false;
            estimate(text)
        }
    };
    let system_cost = count(&system.content).await + PER_MESSAGE_OVERHEAD;
    let mut costs = Vec::with_capacity(history.len());
    for m in &history {
        costs.push(count(&m.content).await + PER_MESSAGE_OVERHEAD);
    }
    let mut budget = n_ctx.saturating_sub(reserve);
    if !exact {
        budget = budget * 4 / 5;
    }
    let mut left = budget.saturating_sub(system_cost);
    let mut keep = 0;
    for (i, c) in costs.iter().enumerate().rev() {
        if *c > left && i + 1 != costs.len() {
            break;
        }
        left = left.saturating_sub(*c);
        keep += 1;
    }
    let start = history.len() - keep;
    let mut out = Vec::with_capacity(keep + 1);
    out.push(system);
    out.extend(history.into_iter().skip(start));
    out
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
