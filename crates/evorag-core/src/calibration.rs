//! リランクのしきい値の較正 (05 §3.7、FR-413)。setup と、起動の後の裏の処理で使う。

use evorag_llm::{LlmClient, LlmError};
use evorag_memory::{METHOD_BUNDLED, TARGET_CONVERSATION, ThresholdBody};
pub use evorag_retrieval::calibrate::CaseResult;
use evorag_retrieval::calibrate::{Bundled, calibrate as compute, cases_id};

/// 記録が今の同梱の事例で求めたものか (違えば求め直す)。
pub fn is_current(body: &ThresholdBody, lang: &str) -> bool {
    body.cases == cases_id(lang)
}

/// 同梱の事例 (プロンプトの言語 `lang`) でしきい値を求め、記録の本体を作る。`version` は前の版 + 1。
/// `done` は済んだ問いの結果 (裏の処理で落とされても残り、次は続きから求める)。
/// `progress` は (済んだ数, 全体の数) で呼ぶ。
pub async fn calibrate(
    rerank: &dyn LlmClient,
    reranker_key: &str,
    lang: &str,
    version: u32,
    done: &std::sync::Mutex<Vec<CaseResult>>,
    progress: &mut (dyn FnMut(usize, usize) + Send),
) -> Result<ThresholdBody, LlmError> {
    let bundled = Bundled::load(lang);
    let results = bundled.run(rerank, done, progress).await?;
    let mut body = ThresholdBody {
        reranker_key: reranker_key.to_owned(),
        target: TARGET_CONVERSATION.to_owned(),
        version,
        method: METHOD_BUNDLED.to_owned(),
        cases: bundled.id(),
        ..ThresholdBody::default()
    };
    if let Some(c) = compute(&results) {
        body.passed = c.passed;
        body.threshold = Some(c.threshold);
        body.band_low = Some(c.band_low);
        body.stats = [
            ("rank_ok", c.rank_ok),
            ("rank_n", c.rank_n),
            ("value_ok", c.value_ok),
            ("value_n", c.value_n),
            ("para_above", c.para_above),
            ("para_band", c.para_band),
            ("para_n", c.para_n),
            ("none_below", c.none_below),
            ("none_n", c.none_n),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    }
    Ok(body)
}

/// 使えるしきい値と帯の下限 (合格した記録だけ)。
pub fn usable(body: &ThresholdBody) -> Option<(f64, f64)> {
    if !body.passed {
        return None;
    }
    Some((body.threshold?, body.band_low?))
}
