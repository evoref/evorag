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
    ChunkBody, KIND_CHUNK, KIND_SESSION, KIND_TURN, RecallDecision, RecallRecord, Turn, TurnBody,
    TurnRole, TurnState,
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

/// 索引の置き場所 (05 §2.3)。v0.2 はチャンクを単位にした `chunk-lex/`。
pub fn index_dir(data_root: &std::path::Path) -> std::path::PathBuf {
    data_root.join("index").join("memory").join("chunk-lex")
}

/// 索引を開く。開けなければ `None` (想起は Skip になる)。v0.1 の索引 (`lex/`) は使わないので消す
/// (05 §2.3。v0.1.2 に戻すと、v0.1.2 が作り直す)。
pub fn open_index(data_root: &std::path::Path) -> (Option<Arc<LexIndex>>, bool) {
    let old = data_root.join("index").join("memory").join("lex");
    if old.is_dir() {
        let _ = std::fs::remove_dir_all(&old);
    }
    match LexIndex::open(
        &index_dir(data_root),
        evorag_retrieval::normalize::RULES_VERSION,
    ) {
        Ok((ix, opened)) => (Some(Arc::new(ix)), opened != Opened::Ok),
        Err(_) => (None, false),
    }
}

/// 索引に入れるターンか (本文があり、失敗していない)。
pub(crate) fn indexable(b: &TurnBody) -> bool {
    !b.text.trim().is_empty() && b.state != TurnState::Failed && b.role != TurnRole::System
}

/// ターンの本文から、チャンクの本体を作る (05 §2.4)。
pub(crate) fn chunk_bodies(turn: Id, b: &TurnBody) -> Vec<ChunkBody> {
    evorag_retrieval::chunk::split(&b.text)
        .into_iter()
        .map(|s| ChunkBody {
            turn,
            session: b.session,
            start: s.start as u32,
            end: s.end as u32,
            rules: evorag_retrieval::chunk::CHUNK_RULES_VERSION,
        })
        .collect()
}

/// チャンクを索引の 1 件にする。
pub(crate) fn chunk_doc(id: Id, c: &ChunkBody, text: &str) -> evorag_index::lex::DocInput {
    let span = evorag_retrieval::chunk::Span {
        start: c.start as usize,
        end: c.end as usize,
    };
    doc_input(id, c.session, &evorag_retrieval::chunk::slice(text, span))
}

/// 起動時の突き合わせの計画 (DB を読むだけ)。
#[derive(Default)]
pub(crate) struct SyncPlan {
    /// 元のターンが無い・区切り方の版が古いチャンク (消す)。
    pub delete: Vec<Id>,
    /// チャンクの無いターンの、新しいチャンク。
    pub create: Vec<ChunkBody>,
    /// 残すチャンクの、索引の 1 件。
    pub keep: Vec<evorag_index::lex::DocInput>,
    /// ターンの本文 (新しいチャンクの索引の 1 件を作るため)。
    pub texts: HashMap<Id, String>,
}

/// DB のターンとチャンクを突き合わせる (05 §2.3。古い版が消したターンのチャンクも、ここで片付ける)。
pub(crate) fn sync_plan(store: &Store) -> SyncPlan {
    let Ok(rtx) = store.read() else {
        return SyncPlan::default();
    };
    let mut turns: HashMap<Id, TurnBody> = HashMap::new();
    let _ = rtx.for_each(KIND_TURN, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<TurnBody>()
            && indexable(&b)
        {
            turns.insert(e.id, b);
        }
        true
    });
    let mut chunks: Vec<(Id, ChunkBody)> = Vec::new();
    let _ = rtx.for_each(KIND_CHUNK, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<ChunkBody>()
        {
            chunks.push((e.id, b));
        }
        true
    });
    drop(rtx);
    let mut plan = SyncPlan::default();
    let mut covered: HashSet<Id> = HashSet::new();
    for (id, c) in chunks {
        match turns.get(&c.turn) {
            Some(t) if c.rules == evorag_retrieval::chunk::CHUNK_RULES_VERSION => {
                covered.insert(c.turn);
                plan.keep.push(chunk_doc(id, &c, &t.text));
            }
            _ => plan.delete.push(id),
        }
    }
    for (id, t) in &turns {
        if !covered.contains(id) {
            plan.create.extend(chunk_bodies(*id, t));
        }
    }
    plan.texts = turns.into_iter().map(|(id, t)| (id, t.text)).collect();
    plan
}

/// 語彙の当たり (チャンク) を、ターンの当たりにまとめる。ターンの値は、そのチャンクの最も良い値
/// (05 §2.4。並びはチャンクの値の順なので、最初に出たものを残す)。
fn turn_hits(
    store: &Store,
    hits: &[evorag_retrieval::recall::Hit],
) -> Vec<evorag_retrieval::recall::Hit> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        let turn = match rtx.get(KIND_CHUNK, h.id) {
            Ok(Some(Read::Ok(e))) => match e.body_as::<ChunkBody>() {
                Ok(c) => c.turn,
                Err(_) => continue,
            },
            _ => continue,
        };
        if seen.insert(turn) {
            out.push(evorag_retrieval::recall::Hit { id: turn, ..*h });
        }
    }
    out
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
    pub user_attached: Vec<Id>,
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

/// ベクトルの候補を引く用意 (05 §3.2)。埋め込みのモデルが使え、索引を作り直していないときだけ。
pub struct VectorQuery {
    pub client: Arc<dyn LlmClient>,
    /// 埋め込みのモデルの model_key (問いの埋め込みのキャッシュの鍵)。
    pub key: String,
    pub index: Arc<evorag_index::vec::VecIndex>,
    pub cache: Arc<QueryCache>,
}

/// 問いの埋め込みの締切 (05 §3.4)。
pub const EMBED_DEADLINE: std::time::Duration = std::time::Duration::from_millis(200);
/// 問いの埋め込みのキャッシュの長さ (05 §3.1 の 4)。
const QUERY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// 語彙とベクトルのそれぞれで集める候補の数 (05 §3.2)。
const CANDIDATES: usize = 50;
/// RRF の k (05 §3.3)。
const RRF_K: f64 = 60.0;

/// 問いの埋め込みのキャッシュ (同じ問いは 5 分の間使い回す。入力が止まったときに先に作る、05 §4.5)。
#[derive(Default)]
pub struct QueryCache(std::sync::Mutex<HashMap<CacheKey, (std::time::Instant, Vec<f32>)>>);

/// 問いの埋め込みのキャッシュの鍵 (埋め込みのモデルの model_key と、問いの本文)。
type CacheKey = (String, String);

impl QueryCache {
    pub fn get(&self, key: &str, text: &str) -> Option<Vec<f32>> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, (t, _)| t.elapsed() < QUERY_CACHE_TTL);
        m.get(&(key.to_owned(), text.to_owned()))
            .map(|(_, v)| v.clone())
    }

    pub fn put(&self, key: &str, text: &str, v: Vec<f32>) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(
            (key.to_owned(), text.to_owned()),
            (std::time::Instant::now(), v),
        );
    }
}

/// 問いを埋め込む (キャッシュにあればそれ。締切を過ぎたら `None`)。
pub async fn embed_query(v: &VectorQuery, text: &str) -> Option<Vec<f32>> {
    if let Some(e) = v.cache.get(&v.key, text) {
        return Some(e);
    }
    let input = [text.to_owned()];
    match tokio::time::timeout(EMBED_DEADLINE, v.client.embed(&input)).await {
        Ok(Ok(mut out)) if out.len() == 1 && !out[0].is_empty() => {
            let e = out.pop()?;
            v.cache.put(&v.key, text, e.clone());
            Some(e)
        }
        _ => None,
    }
}

/// ベクトルの当たり (チャンク) を、ターンの順位にする (チャンクの最も良い順位をターンの順位にする)。
fn vector_turns(store: &Store, rows: &[(evorag_index::vec::VecRow, f32)]) -> Vec<Id> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (row, _) in rows {
        if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_CHUNK, row.id)
            && let Ok(c) = e.body_as::<ChunkBody>()
            && seen.insert(c.turn)
        {
            out.push(c.turn);
        }
    }
    out
}

/// 順位の列を RRF (k = 60) で 1 つにする (05 §3.3。値は混ぜない)。同点は新しいターンを先にする。
fn rrf(lists: &[Vec<Id>]) -> Vec<Id> {
    let mut score: HashMap<Id, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(Id, f64)> = score.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    out.into_iter().map(|(id, _)| id).collect()
}

/// 自動で選んだ結果 (添えるターン、判定点、観測値)。
struct Selection {
    hits: Vec<Id>,
    decision: RecallDecision,
    obs: RecallObs,
}

/// 引いて、添えるターンを自動で選ぶ (05 §3.3〜§3.5)。
async fn select(
    store: &Store,
    index: Option<&LexIndex>,
    lang: PromptLanguage,
    session: Id,
    user_turn: Id,
    mode: &RerankMode,
    vectors: Option<&VectorQuery>,
) -> Selection {
    let mut obs = RecallObs::default();
    if vectors.is_none() {
        // 埋め込みが無い・記録と違う・作り直しの途中 (05 §3.5 の `no_embed`)。
        obs.degraded
            .push(evorag_memory::DEGRADED_NO_EMBED.to_owned());
    }
    let none = |decision, obs: RecallObs| Selection {
        hits: Vec::new(),
        decision,
        obs,
    };
    let Some(index) = index else {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return none(RecallDecision::Skip, obs);
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
    let mut r = recall(
        &snap,
        &query,
        previous.as_deref(),
        &|d| d.session == session,
        &RecallConfig::default(),
    );
    if r.decision == Decision::Skip {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return none(RecallDecision::Skip, obs);
    }
    // 索引の単位はチャンク。ターンにまとめる (05 §2.4)。
    r.hits = turn_hits(store, &r.hits);
    r.candidates = turn_hits(store, &r.candidates);
    let lexical = |obs: RecallObs| {
        if r.decision == Decision::Abstain {
            none(RecallDecision::Abstain, obs)
        } else {
            Selection {
                hits: r.hits.iter().map(|h| h.id).collect(),
                decision: RecallDecision::Fire,
                obs,
            }
        }
    };
    let u = match mode {
        RerankMode::Lexical(reason) => {
            obs.degraded.push((*reason).to_owned());
            return lexical(obs);
        }
        RerankMode::Use(u) => u,
    };
    // 追い質問は、直前の発話を足した問いで引き、リランクする (05 §3.1)。
    let q = match previous
        .as_deref()
        .filter(|_| evorag_retrieval::recall::is_follow_up(&query))
    {
        Some(p) => format!("{p}\n{query}"),
        None => query.clone(),
    };
    // 語彙とベクトルの候補を RRF で 1 つの順位にする (05 §3.2・§3.3)。
    let mut lists: Vec<Vec<Id>> = vec![r.candidates.iter().map(|h| h.id).collect()];
    if let Some(v) = vectors {
        match embed_query(v, &q).await {
            Some(qv) => {
                let rows = v
                    .index
                    .snapshot()
                    .search(&qv, CANDIDATES, &|row| row.session == session);
                lists.push(vector_turns(store, &rows));
            }
            // 問いの埋め込みが締切に間に合わない: その要求だけ語彙の候補で (05 §3.5)。
            None => obs
                .degraded
                .push(evorag_memory::DEGRADED_EMBED_DEADLINE.to_owned()),
        }
    }
    let ranked = rrf(&lists);
    let ex = exchanges(store, &ranked, u.top);
    if ex.is_empty() {
        return none(RecallDecision::Abstain, obs);
    }
    let docs: Vec<String> = ex.iter().map(|x| exchange_text(lang, x)).collect();
    let scores = match tokio::time::timeout(RERANK_DEADLINE, u.client.rerank(&q, &docs)).await {
        Ok(Ok(s)) if s.len() == ex.len() => s,
        failed => {
            // その要求だけ、v0.1 の規則に戻す (05 §3.5)。
            obs.degraded.push(
                match failed {
                    Err(_) => evorag_memory::DEGRADED_RERANK_DEADLINE,
                    Ok(_) => evorag_memory::DEGRADED_RERANK_ERROR,
                }
                .to_owned(),
            );
            return lexical(obs);
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
    let Some((thr, band_low, version)) = u.threshold else {
        // しきい値が無い: 添えずに、上位 3 件を帯に出す (05 §3.5 の `no_threshold`)。
        obs.degraded
            .push(evorag_memory::DEGRADED_NO_THRESHOLD.to_owned());
        obs.band = band(&order, &ex, |_, _| true);
        return none(RecallDecision::Abstain, obs);
    };
    obs.threshold_version = Some(version);
    let (top, top_score) = order[0];
    let fire = f64::from(top_score) >= thr;
    obs.band = band(&order, &ex, |i, s| {
        (!fire || i != top) && f64::from(s) >= band_low && f64::from(s) < thr
    });
    if !fire {
        return none(RecallDecision::Abstain, obs);
    }
    Selection {
        hits: ex[top].turns.iter().map(|t| t.envelope.id).collect(),
        decision: RecallDecision::Fire,
        obs,
    }
}

/// 応答の前に引いて、文脈に添えるものを作る。出典の番号は `base` から振る (05 §5.1)。
/// `extra` は、利用者が帯から選んで添えた往復 (05 §5.2 の「添えて作り直す」)。自動で選んだものに足す。
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
    vectors: Option<&VectorQuery>,
    extra: &[Id],
) -> Attached {
    let mut sel = select(store, index, lang, session, user_turn, mode, vectors).await;
    for id in extra {
        if !sel.hits.contains(id) {
            sel.hits.push(*id);
        }
    }
    sel.obs.user_attached = extra.to_vec();
    if sel.hits.is_empty() {
        return Attached {
            quote: None,
            attached: Vec::new(),
            record: RecallRecord {
                decision: sel.decision,
                sources: Vec::new(),
            },
            obs: sel.obs,
        };
    }
    finish(
        store, lang, llm, tokens, model_key, n_ctx, reserve, session, base, sel.hits, sel.obs,
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

/// 往復 (代表のターンと、その相手) のターン。帯の詳細に使う (05 §5.2)。
pub fn exchange_turns(store: &Store, target: Id) -> Option<Vec<Turn>> {
    sources(store, &[target])
        .into_iter()
        .next()
        .map(|s| s.turns)
        .filter(|t| !t.is_empty())
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

    /// verifies: AC-FR-402-1 (語彙とベクトルの順位を RRF で合わせる、05 §3.3)
    #[test]
    fn rrf_prefers_turns_found_by_both_and_breaks_ties_by_recency() {
        let id = |n: u8| Id::from_bytes([n; 16]);
        // 両方に出たもの (3) が先。次に片方の 1 位 (1)。片方の 2 位どうし (2 と 4) は同点で、新しい
        // (ID の大きい) 方が先。
        let fused = rrf(&[vec![id(1), id(2), id(3)], vec![id(3), id(4)]]);
        assert_eq!(fused, [id(3), id(1), id(4), id(2)]);
    }

    #[test]
    fn the_nonce_changes_and_is_removed_from_quotes() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        assert_eq!(excerpt(&format!("x<</引用-{a}>>y"), &a), "x<</引用->>y");
    }
}
