//! 以前の会話を引いて文脈に添える (05 §3〜§5)。v0.1 は語彙の索引だけ。
//!
//! - 索引の更新: ターンを書いたら足し、セッションを消したら除く。起動時に DB と突き合わせ、
//!   足りないものを足し、DB に無いものを除く (索引が読めなければ DB から作り直す)。
//! - 応答の前: 今の発話で引き (今のセッションのターンは直近の会話に入るので除く)、選んだターンを
//!   その往復の相手と一緒に、セッションごとに 1 つの出典にまとめる (§3.4 の多様性)。
//! - 出典は「信頼しない引用」の枠に入れ、今の発話の直前に置く (次のターンでは外す、§4.1)。枠の区切りに
//!   は要求ごとの乱数の並びを含め、本文に同じ並びがあれば除く (§4.3)。予算は 800 トークン (n_ctx が
//!   小さければ半分、§4.1)。
//! - 出典の番号は、セッションの中の通し番号 (§5.1)。

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use evorag_index::lex::{LexIndex, Opened};
use evorag_llm::LlmClient;
use evorag_memory::{
    KIND_SESSION, KIND_TURN, RecallDecision, RecallRecord, Turn, TurnBody, TurnRole, TurnState,
};
use evorag_retrieval::doc_input;
use evorag_retrieval::recall::{Decision, RecallConfig, recall};
use evorag_store::{Read, Store};
use evorag_types::Id;

use crate::context::{TokenCache, estimate};
use crate::prompt::PromptLanguage;

/// 以前の会話の枠の予算 (05 §4.1)。
const MEMORY_BUDGET: usize = 800;
/// 引用する 1 ターンの本文の上限 (文字)。
const EXCERPT_CHARS: usize = 400;

/// 索引の置き場所 (05 §2.3)。
pub fn index_dir(data_root: &std::path::Path) -> std::path::PathBuf {
    data_root.join("index").join("memory").join("lex")
}

/// 索引を開く。開けなければ `None` (想起は Skip になる)。
pub fn open_index(data_root: &std::path::Path) -> (Option<Arc<LexIndex>>, bool) {
    match LexIndex::open(
        &index_dir(data_root),
        evorag_retrieval::normalize::RULES_VERSION,
    ) {
        Ok((ix, opened)) => (Some(Arc::new(ix)), opened != Opened::Ok),
        Err(_) => (None, false),
    }
}

/// 索引に入れるターンか (本文があり、失敗していない)。
fn indexable(b: &TurnBody) -> bool {
    !b.text.trim().is_empty() && b.state != TurnState::Failed && b.role != TurnRole::System
}

/// 起動時: DB と突き合わせる。`rebuild` なら作り直す。
pub fn sync(store: &Store, index: &LexIndex, rebuild: bool) -> std::io::Result<()> {
    let Ok(rtx) = store.read() else {
        return Ok(());
    };
    let mut turns: Vec<(Id, TurnBody)> = Vec::new();
    let _ = rtx.for_each(KIND_TURN, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<TurnBody>()
            && indexable(&b)
        {
            turns.push((e.id, b));
        }
        true
    });
    drop(rtx);
    if rebuild {
        return index.replace_all(
            turns
                .iter()
                .map(|(id, b)| doc_input(*id, b.session, &b.text))
                .collect(),
        );
    }
    let have = index.snapshot().ids();
    let db: HashSet<Id> = turns.iter().map(|(id, _)| *id).collect();
    let extra: HashSet<Id> = have.difference(&db).copied().collect();
    if !extra.is_empty() {
        index.remove(&extra)?;
    }
    let missing: Vec<_> = turns
        .iter()
        .filter(|(id, _)| !have.contains(id))
        .map(|(id, b)| doc_input(*id, b.session, &b.text))
        .collect();
    index.add(missing)
}

/// 書いたターンを索引に足す。
pub fn add_turns(store: &Store, index: &LexIndex, ids: &[Id]) -> std::io::Result<()> {
    let Ok(rtx) = store.read() else {
        return Ok(());
    };
    let mut docs = Vec::new();
    for id in ids {
        if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_TURN, *id)
            && let Ok(b) = e.body_as::<TurnBody>()
            && indexable(&b)
        {
            docs.push(doc_input(e.id, b.session, &b.text));
        }
    }
    drop(rtx);
    index.add(docs)
}

/// 文脈に添えるもの。
pub struct Attached {
    /// 今の発話の直前に置く引用の枠 (添えるものが無ければ `None`)。
    pub quote: Option<String>,
    pub attached: Vec<Id>,
    pub record: RecallRecord,
    /// 観測値 (05 §3.6): リランクの値、帯、縮退の理由。
    pub obs: RecallObs,
}

/// 想起の観測値 (応答のターンの `obs` に写す)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecallObs {
    pub reranked: Vec<evorag_memory::Reranked>,
    pub reranker: Option<String>,
    pub threshold_version: Option<u32>,
    pub band: Vec<Id>,
    pub degraded: Vec<String>,
}

/// 応答の経路のリランクの使い方 (05 §3.4・§3.5)。
pub enum RerankMode {
    /// リランクしない。v0.1 の語彙の規則で選ぶ。値は縮退の理由 (`no_rerank`・`rerank_slow`)。
    Lexical(&'static str),
    /// リランクする。
    Use(RerankUse),
}

pub struct RerankUse {
    pub client: Arc<dyn LlmClient>,
    /// リランカーの model_key。
    pub key: String,
    /// 使えるしきい値・帯の下限・記録の版 (無ければ `no_threshold`)。
    pub threshold: Option<(f64, f64, u32)>,
    /// リランクする往復の数 (既定 6、点検の結果で減らす、§3.4)。
    pub top: usize,
}

/// リランクの締切 (05 §3.4)。
pub const RERANK_DEADLINE: std::time::Duration = std::time::Duration::from_millis(400);
/// 帯に出す往復の上限 (05 §3.4)。
const BAND_MAX: usize = 3;
/// リランカーに渡す往復の、発話と応答の長さの上限 (文字。入力の上限 512 トークンに収める、§3.3)。
const RERANK_USER_CHARS: usize = 400;
const RERANK_ANSWER_CHARS: usize = 800;

fn nonce() -> String {
    // 要求ごとに変わる乱数の並び (標準ライブラリの乱数の種を使う)。
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{:016x}", h.finish())
}

fn excerpt(text: &str, nonce: &str) -> String {
    let t = text.replace(nonce, "");
    let mut out: String = t.chars().take(EXCERPT_CHARS).collect();
    if t.chars().count() > EXCERPT_CHARS {
        out.push('…');
    }
    out
}

fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// 出典 1 つ (同じセッションの選んだターンと、その往復の相手)。
struct Source {
    session: Id,
    title: String,
    turns: Vec<Turn>,
}

fn role_label(lang: PromptLanguage, role: TurnRole) -> &'static str {
    match (lang, role) {
        (PromptLanguage::Ja, TurnRole::User) => "利用者",
        (PromptLanguage::Ja, _) => "アシスタント",
        (PromptLanguage::En, TurnRole::User) => "User",
        (PromptLanguage::En, _) => "Assistant",
    }
}

fn render_source(lang: PromptLanguage, n: usize, s: &Source, nonce: &str) -> String {
    let at = s
        .turns
        .first()
        .map(|t| crate::localtime::datetime(t.envelope.created_at.as_millis()))
        .unwrap_or_default();
    let title = s.title.replace(nonce, "");
    let mut out = match lang {
        PromptLanguage::Ja => format!("[{n}] {at} セッション「{title}」\n"),
        PromptLanguage::En => format!("[{n}] {at} session \"{title}\"\n"),
    };
    for t in &s.turns {
        out.push_str(role_label(lang, t.body.role));
        out.push_str(": ");
        out.push_str(&excerpt(&t.body.text, nonce));
        out.push('\n');
    }
    out
}

/// 往復の相手: 発話なら、その最後の完了した応答。応答なら、その発話。
fn partner(turns: &[Turn], id: Id, b: &TurnBody) -> Option<Id> {
    match b.role {
        TurnRole::User => turns
            .iter()
            .rev()
            .find(|t| t.body.reply_to == Some(id) && t.body.state != TurnState::Failed)
            .map(|t| t.envelope.id),
        _ => b.reply_to,
    }
}

/// 選んだターンを、その往復の相手と一緒に、セッションごとの出典にまとめる (スコアの順)。
fn sources(store: &Store, hits: &[Id]) -> Vec<Source> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut order: Vec<Id> = Vec::new();
    let mut picked: HashMap<Id, HashSet<Id>> = HashMap::new();
    let mut session_turns: HashMap<Id, Vec<Turn>> = HashMap::new();
    for id in hits {
        let Ok(Some(Read::Ok(e))) = rtx.get(KIND_TURN, *id) else {
            continue;
        };
        let Ok(b) = e.body_as::<TurnBody>() else {
            continue;
        };
        let turns = session_turns
            .entry(b.session)
            .or_insert_with(|| evorag_memory::session_turns(&rtx, b.session).unwrap_or_default());
        let set = picked.entry(b.session).or_default();
        if set.is_empty() {
            order.push(b.session);
        }
        set.insert(*id);
        set.extend(partner(turns, *id, &b));
    }
    order
        .into_iter()
        .map(|sid| {
            let title = evorag_memory::load_session(&rtx, sid)
                .map(|s| s.body.title)
                .unwrap_or_default();
            let set = &picked[&sid];
            let turns = session_turns
                .remove(&sid)
                .unwrap_or_default()
                .into_iter()
                .filter(|t| set.contains(&t.envelope.id))
                .collect();
            Source {
                session: sid,
                title,
                turns,
            }
        })
        .collect()
}

/// 1 つの往復 (05 §3.3)。
struct Exchange {
    /// 代表のターン (利用者の発話。発話の無い往復は、そのターン)。
    rep: Id,
    session: Id,
    /// セッションの中の代表のターンの位置 (帯で隣り合う往復を見分ける)。
    pos: usize,
    turns: Vec<Turn>,
}

/// 語彙の上位のターンを往復にまとめ、上位 `limit` 往復にする (05 §3.3 の 2)。
fn exchanges(store: &Store, ranked: &[Id], limit: usize) -> Vec<Exchange> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut session_turns: HashMap<Id, Vec<Turn>> = HashMap::new();
    let mut out: Vec<Exchange> = Vec::new();
    for id in ranked {
        if out.len() >= limit {
            break;
        }
        let Ok(Some(Read::Ok(e))) = rtx.get(KIND_TURN, *id) else {
            continue;
        };
        let Ok(b) = e.body_as::<TurnBody>() else {
            continue;
        };
        let turns = session_turns
            .entry(b.session)
            .or_insert_with(|| evorag_memory::session_turns(&rtx, b.session).unwrap_or_default());
        let other = partner(turns, *id, &b);
        let rep = match b.role {
            TurnRole::User => *id,
            _ => other.unwrap_or(*id),
        };
        if out.iter().any(|x| x.rep == rep) {
            continue;
        }
        let members: Vec<Turn> = turns
            .iter()
            .filter(|t| t.envelope.id == *id || Some(t.envelope.id) == other)
            .cloned()
            .collect();
        let pos = turns.iter().position(|t| t.envelope.id == rep).unwrap_or(0);
        out.push(Exchange {
            rep,
            session: b.session,
            pos,
            turns: members,
        });
    }
    out
}

/// リランカーに渡す往復の本文 (較正と同じ形、05 §3.7)。
fn exchange_text(lang: PromptLanguage, x: &Exchange) -> String {
    let pick = |role: TurnRole| {
        x.turns
            .iter()
            .find(|t| t.body.role == role)
            .map(|t| t.body.text.as_str())
            .unwrap_or("")
    };
    evorag_retrieval::calibrate::exchange_text(
        lang.as_str(),
        &head(pick(TurnRole::User), RERANK_USER_CHARS),
        &head(pick(TurnRole::Assistant), RERANK_ANSWER_CHARS),
    )
}

/// 帯の往復を選ぶ (05 §3.4): 最大 3 件、同じセッションの隣り合う往復を 2 つ以上出さない。
fn band(order: &[(usize, f32)], ex: &[Exchange], keep: impl Fn(usize, f32) -> bool) -> Vec<Id> {
    let mut picked: Vec<usize> = Vec::new();
    for &(i, s) in order {
        if picked.len() >= BAND_MAX {
            break;
        }
        if !keep(i, s) {
            continue;
        }
        let near = picked
            .iter()
            .any(|&j| ex[j].session == ex[i].session && ex[j].pos.abs_diff(ex[i].pos) <= 2);
        if !near {
            picked.push(i);
        }
    }
    picked.into_iter().map(|i| ex[i].rep).collect()
}

/// 応答の前に引いて、文脈に添えるものを作る。出典の番号は `base` から振る (05 §5.1)。
#[allow(clippy::too_many_arguments)]
pub async fn attach(
    store: &Store,
    index: Option<&LexIndex>,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    n_ctx: usize,
    reserve: usize,
    session: Id,
    user_turn: Id,
    base: u32,
    mode: &RerankMode,
) -> Attached {
    // 埋め込みの候補は M2.5 で足す。それまでは、語彙の候補だけで動く (05 §3.5 の `no_embed`)。
    let mut obs = RecallObs {
        degraded: vec![evorag_memory::DEGRADED_NO_EMBED.to_owned()],
        ..RecallObs::default()
    };
    let skip = |decision, obs: RecallObs| Attached {
        quote: None,
        attached: Vec::new(),
        record: RecallRecord {
            decision,
            sources: Vec::new(),
        },
        obs,
    };
    let Some(index) = index else {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return skip(RecallDecision::Skip, obs);
    };
    let (query, previous) = {
        let turns = store
            .read()
            .ok()
            .and_then(|r| evorag_memory::session_turns(&r, session).ok())
            .unwrap_or_default();
        let mut users = turns
            .iter()
            .filter(|t| t.body.role == TurnRole::User && t.envelope.id <= user_turn)
            .rev();
        let query = users
            .next()
            .map(|t| t.body.text.clone())
            .unwrap_or_default();
        (query, users.next().map(|t| t.body.text.clone()))
    };
    let snap = index.snapshot();
    // 今のセッションのターンは直近の会話に入るので、候補から除く (05 §3.2)。
    let r = recall(
        &snap,
        &query,
        previous.as_deref(),
        &|d| d.session == session,
        &RecallConfig::default(),
    );
    if r.decision == Decision::Skip {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return skip(RecallDecision::Skip, obs);
    }

    // 添える出典 (セッションごとにまとめる前のターン) を決める。
    let hits: Vec<Id> = match mode {
        RerankMode::Lexical(reason) => {
            obs.degraded.push((*reason).to_owned());
            if r.decision == Decision::Abstain {
                return skip(RecallDecision::Abstain, obs);
            }
            r.hits.iter().map(|h| h.id).collect()
        }
        RerankMode::Use(u) => {
            let ranked: Vec<Id> = r.candidates.iter().map(|h| h.id).collect();
            let ex = exchanges(store, &ranked, u.top);
            if ex.is_empty() {
                return skip(RecallDecision::Abstain, obs);
            }
            // 追い質問は、直前の発話を足した問いでリランクする (05 §3.1)。
            let q = match previous
                .as_deref()
                .filter(|_| evorag_retrieval::recall::is_follow_up(&query))
            {
                Some(p) => format!("{p}\n{query}"),
                None => query.clone(),
            };
            let docs: Vec<String> = ex.iter().map(|x| exchange_text(lang, x)).collect();
            let scored =
                match tokio::time::timeout(RERANK_DEADLINE, u.client.rerank(&q, &docs)).await {
                    Ok(Ok(s)) if s.len() == ex.len() => Ok(s),
                    Ok(_) => Err(evorag_memory::DEGRADED_RERANK_ERROR),
                    Err(_) => Err(evorag_memory::DEGRADED_RERANK_DEADLINE),
                };
            let scores = match scored {
                Ok(s) => s,
                Err(reason) => {
                    // その要求だけ、v0.1 の規則に戻す (05 §3.5)。
                    obs.degraded.push(reason.to_owned());
                    if r.decision == Decision::Abstain {
                        return skip(RecallDecision::Abstain, obs);
                    }
                    return finish(
                        store,
                        lang,
                        llm,
                        tokens,
                        model_key,
                        n_ctx,
                        reserve,
                        session,
                        base,
                        r.hits.iter().map(|h| h.id).collect(),
                        obs,
                    )
                    .await;
                }
            };
            // 値の高い順。同じ値は新しい往復を先にする (05 §3.4)。
            let mut order: Vec<(usize, f32)> = scores.iter().copied().enumerate().collect();
            order.sort_by(|a, b| b.1.total_cmp(&a.1).then(ex[b.0].rep.cmp(&ex[a.0].rep)));
            obs.reranked = order
                .iter()
                .map(|&(i, score)| evorag_memory::Reranked {
                    id: ex[i].rep,
                    score,
                })
                .collect();
            obs.reranker = Some(u.key.clone());
            match u.threshold {
                Some((thr, band_low, version)) => {
                    obs.threshold_version = Some(version);
                    let (top, top_score) = order[0];
                    let fire = f64::from(top_score) >= thr;
                    obs.band = band(&order, &ex, |i, s| {
                        (!fire || i != top) && f64::from(s) >= band_low && f64::from(s) < thr
                    });
                    if !fire {
                        return skip(RecallDecision::Abstain, obs);
                    }
                    ex[top].turns.iter().map(|t| t.envelope.id).collect()
                }
                None => {
                    // しきい値が無い: 添えずに、上位 3 件を帯に出す (05 §3.5 の `no_threshold`)。
                    obs.degraded
                        .push(evorag_memory::DEGRADED_NO_THRESHOLD.to_owned());
                    obs.band = band(&order, &ex, |_, _| true);
                    return skip(RecallDecision::Abstain, obs);
                }
            }
        }
    };
    finish(
        store, lang, llm, tokens, model_key, n_ctx, reserve, session, base, hits, obs,
    )
    .await
}

/// 選んだターンを出典にまとめ、予算の中で引用の枠を作る。
#[allow(clippy::too_many_arguments)]
async fn finish(
    store: &Store,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    n_ctx: usize,
    reserve: usize,
    session: Id,
    base: u32,
    hits: Vec<Id>,
    obs: RecallObs,
) -> Attached {
    // n_ctx が小さく、直近の会話に 2,000 トークンを残せないときは半分にする (05 §4.1)。
    let mut budget = MEMORY_BUDGET;
    if n_ctx < 8192 && n_ctx.saturating_sub(reserve + MEMORY_BUDGET) < 2000 {
        budget /= 2;
    }
    let nonce = nonce();
    let mut body = String::new();
    let mut used = 0;
    let mut attached = Vec::new();
    let mut counts = Vec::new();
    for s in sources(store, &hits) {
        if s.session == session || s.turns.is_empty() {
            continue;
        }
        let text = render_source(lang, base as usize + counts.len(), &s, &nonce);
        let cost = match tokens.count(llm, model_key, &text).await {
            Some(n) => n,
            None => estimate(&text) * 5 / 4,
        };
        if used + cost > budget {
            continue;
        }
        used += cost;
        body.push_str(&text);
        counts.push(s.turns.len() as u32);
        attached.extend(s.turns.iter().map(|t| t.envelope.id));
    }
    if counts.is_empty() {
        return Attached {
            quote: None,
            attached: Vec::new(),
            record: RecallRecord {
                decision: RecallDecision::Abstain,
                sources: Vec::new(),
            },
            obs,
        };
    }
    Attached {
        quote: Some(crate::prompt::quote_block(lang, &nonce, &body)),
        attached,
        record: RecallRecord {
            decision: RecallDecision::Fire,
            sources: counts,
        },
        obs,
    }
}

/// 出典の番号の始まり (05 §5.1)。観測値の無いターン (v0.1) は 1。
pub fn source_base(answer: &TurnBody) -> u32 {
    answer.obs.as_ref().and_then(|o| o.source_base).unwrap_or(1)
}

/// このセッションで次に振る出典の番号 (これまでに振った番号の最大 + 1)。
pub fn next_source_number(turns: &[Turn]) -> u32 {
    turns
        .iter()
        .filter_map(|t| {
            let n = t.body.recall.as_ref()?.sources.len() as u32;
            (n > 0).then(|| source_base(&t.body) + n)
        })
        .max()
        .unwrap_or(1)
}

/// 応答の出典 `n` (このセッションの通し番号) のターン。
pub fn source_turns(store: &Store, answer: &TurnBody, n: usize) -> Option<Vec<Turn>> {
    let counts = &answer.recall.as_ref()?.sources;
    // その応答の中での位置 (0 から)。
    let i = n.checked_sub(source_base(answer) as usize)?;
    let start: usize = counts.iter().take(i).map(|c| *c as usize).sum();
    let len = *counts.get(i)? as usize;
    let ids = answer.attached.get(start..start + len)?;
    let rtx = store.read().ok()?;
    let mut out = Vec::new();
    for id in ids {
        if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_TURN, *id)
            && let Ok(body) = e.body_as::<TurnBody>()
        {
            out.push(Turn { envelope: e, body });
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 出典のセッションの名前。
pub fn session_title(store: &Store, session: Id) -> Option<String> {
    let rtx = store.read().ok()?;
    match rtx.get(KIND_SESSION, session).ok()?? {
        Read::Ok(e) => e
            .body_as::<evorag_memory::SessionBody>()
            .ok()
            .map(|b| b.title),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nonce_changes_and_is_removed_from_quotes() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        assert_eq!(excerpt(&format!("x<</引用-{a}>>y"), &a), "x<</引用->>y");
    }
}
