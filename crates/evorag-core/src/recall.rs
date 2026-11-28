//! 以前の会話を引いて文脈に添える (05 §3〜§5)。v0.1 は語彙の索引だけ。
//!
//! - 索引の更新: ターンを書いたら足し、セッションを消したら除く。起動時に DB と突き合わせ、
//!   足りないものを足し、DB に無いものを除く (索引が読めなければ DB から作り直す)。
//! - 応答の前: 今の発話で引き (今のセッションのターンは直近の会話に入るので除く)、選んだターンを
//!   その往復の相手と一緒に、セッションごとに 1 つの出典にまとめる (§3.4 の多様性)。
//! - 出典は「信頼しない引用」の枠に入れ、今の発話の直前に置く (次のターンでは外す、§4.1)。枠の区切りに
//!   は要求ごとの乱数の並びを含め、本文に同じ並びがあれば除く (§4.3)。予算は 800 トークン (n_ctx が
//!   小さければ半分、§4.1。`context::frame_budgets`)。
//! - 区切りの鍵 (04 §5.5、v0.5 の根) は、区切りのすべての往復を引くためだけに使う。当たったら区切りの往復の原文を
//!   添える (鍵は添えない、OQ-36 の 7)。項目は区切りの ID で、セッションに結び付けない値で入れ、今のセッションの
//!   区切りの項目は候補を集める段階で除く ([`CurrentOnly`])。
//! - 事実から作った検索の鍵 (04 §5.5) は、出典の往復を引くためだけに使う。当たったら、今の事実の状態を
//!   読み直し (private・`Retracted` になった事実の鍵は、索引に残っていても使わない、05 §3.2)、出典の
//!   往復の原文を添える (鍵は添えない、OQ-36 の 7)。
//! - 出典の番号は、セッションの中の通し番号 (§5.1)。

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use evorag_index::lex::{LexIndex, Opened};
use evorag_llm::LlmClient;
use evorag_memory::{
    ChunkBody, FactKey, KIND_CHUNK, KIND_NOTE, KIND_SEGMENT, KIND_SESSION, KIND_TURN, NoteBody,
    REL_SEGMENT_SESSION, RecallDecision, RecallRecord, SegmentBody, Turn, TurnBody, TurnRole,
    TurnState,
};
use evorag_retrieval::doc_input;
use evorag_retrieval::recall::{Decision, RecallConfig, recall};
use evorag_store::{Read, Store};
use evorag_types::Id;

use crate::context::{TokenCache, estimate};
use crate::prompt::PromptLanguage;

/// 引用する 1 ターンの本文の上限 (文字)。
const EXCERPT_CHARS: usize = 400;

/// 想起の数値 (05 §3.8、OQ-51。S-17 の 1・7 で決めた)。`windows = false` は v0.3 の形 (最上位の 1 往復を
/// 400 文字で添える。評価で起点と比べるために残す)。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecallTuning {
    /// 往復で選び、文の窓で詰める (v0.4、05 §3.4)。`false` なら v0.3 の形。
    pub windows: bool,
    /// 2 件目以降の下限の位置: しきい値から帯の下限へ向けた割合 (0 ならしきい値、1 なら帯の下限)。
    pub floor: f64,
    /// 添える往復の上限。
    pub max_exchanges: usize,
    /// 想起の予算 (トークン。n_ctx が小さいときは今と同じ比で減らす)。
    pub budget: usize,
    /// 先読みが当たらなかったとき・先読みが無いときの予算 (トークン。05 §3.8 の 5)。
    pub small_budget: usize,
    /// 窓に足す前後の文の数。
    pub window: usize,
    /// 1 ターンの中で中心にする当たった文の数。
    pub hits_per_turn: usize,
    /// これ以下の長さ (文字) のターンは、窓にせず全文を添える。
    pub short_chars: usize,
    /// 1 ターンの引用の上限 (文字)。
    pub turn_chars: usize,
    /// 件数・合計・一覧の意図がある問いで、候補と予算を広げる (S-17 の 7)。
    pub count_widen: bool,
    /// 広げるときのリランクの往復の数。
    pub count_top: usize,
    /// 広げるときの添える往復の上限。
    pub count_max: usize,
    /// 広げるときの予算。
    pub count_budget: usize,
}

impl Default for RecallTuning {
    fn default() -> Self {
        Self {
            windows: true,
            floor: 0.5,
            max_exchanges: 4,
            budget: 800,
            window: 1,
            hits_per_turn: 2,
            short_chars: 300,
            small_budget: 800,
            turn_chars: 700,
            count_widen: true,
            count_top: 10,
            count_max: 8,
            count_budget: 1200,
        }
    }
}

static TUNING: std::sync::RwLock<Option<RecallTuning>> = std::sync::RwLock::new(None);

/// 評価のため: 想起の数値を差し替える (プロセス全体)。
#[doc(hidden)]
pub fn set_tuning(t: RecallTuning) {
    *TUNING.write().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

fn tuning() -> RecallTuning {
    TUNING
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_default()
}

/// 想起の作りの版 (観測値の `versions`、05 §3.6)。選び方・窓・予算を変えたら上げる。
pub(crate) const RECALL_VERSION: &str = "0.4.0";

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

/// ターンの本文 (改訂の番号 `rev`) から、チャンクの本体を作る (05 §2.4)。作ったときの改訂の番号を `src_rev` に
/// 持つ (03 §3.3)。
pub(crate) fn chunk_bodies(turn: Id, rev: u32, b: &TurnBody) -> Vec<ChunkBody> {
    evorag_retrieval::chunk::split(&b.text)
        .into_iter()
        .map(|s| ChunkBody {
            turn,
            session: b.session,
            start: s.start as u32,
            end: s.end as u32,
            rules: evorag_retrieval::chunk::CHUNK_RULES_VERSION,
            src_rev: Some(rev),
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

/// ノートの鍵を索引の項目にする (鍵ごとに 1 件。ID はノートの ID、04 §2)。
pub(crate) fn note_docs(id: Id, n: &NoteBody) -> Vec<evorag_index::lex::DocInput> {
    n.keys.iter().map(|k| doc_input(id, n.session, k)).collect()
}

/// 事実の検索の鍵の項目のセッション (04 §5.5)。1 つの事実の出典は複数のセッションにまたがるので、鍵は
/// どのセッションにも属さない値で索引に入れ、今のセッションの往復は当たりをターンに直すときに除く
/// ([`hit_turns`])。
const FACT_KEY_SESSION: Id = Id::from_bytes([0; 16]);

/// 事実の検索の鍵を索引の項目にする (ID は事実の ID、04 §5.5)。
pub(crate) fn fact_doc(k: &FactKey) -> evorag_index::lex::DocInput {
    doc_input(k.fact, FACT_KEY_SESSION, &k.text)
}

/// 区切りの鍵の項目のセッション (04 §5.5)。鍵の項目はセッションに結び付けないので、どのセッションにも属さない
/// 値で語彙とベクトルの索引に入れる。事実の鍵の値とは分ける (事実の鍵の手順が区切りの鍵の項目を除かないため)。
/// 今のセッションの区切りの項目は、候補を集める段階で除く ([`CurrentOnly`])。
pub(crate) const SEGMENT_KEY_SESSION: Id =
    Id::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

/// 区切りの鍵を語彙の索引の項目にする (鍵ごとに 1 件。ID は区切りの ID、04 §5.5)。
pub(crate) fn segment_docs<'k>(
    segment: Id,
    keys: impl IntoIterator<Item = &'k str>,
) -> Vec<evorag_index::lex::DocInput> {
    keys.into_iter()
        .map(|k| doc_input(segment, SEGMENT_KEY_SESSION, k))
        .collect()
}

/// 索引の項目が区切りの鍵か。
pub(crate) fn is_segment_key(d: &evorag_index::lex::Doc) -> bool {
    d.session == SEGMENT_KEY_SESSION
}

/// 語彙の索引に今ある事実の鍵の項目 (事実の ID)。
pub(crate) fn indexed_fact_keys(index: &LexIndex) -> HashSet<Id> {
    index.snapshot().ids_where(is_fact_key)
}

/// 索引の項目が事実の鍵か。
pub(crate) fn is_fact_key(d: &evorag_index::lex::Doc) -> bool {
    d.session == FACT_KEY_SESSION
}

/// セッションに結び付けない索引の項目のうち、今のセッション `current` の往復だけを指すもの。想起の候補を集める
/// 段階で除く (今のセッションのターンを添えないのと同じ、04 §5.5、05 §3.2)。
struct CurrentOnly {
    session: Id,
    /// 今のセッションのターンだけを出典とする事実 (の鍵の ID)。
    facts: HashSet<Id>,
    /// 今のセッションの区切り (の鍵の ID)。
    segments: HashSet<Id>,
}

impl CurrentOnly {
    fn of(store: &Store, current: Id) -> Self {
        Self {
            session: current,
            facts: current_only_facts(store, current),
            segments: current_segments(store, current),
        }
    }

    /// 語彙の索引の項目を除くか。
    fn skips(&self, d: &evorag_index::lex::Doc) -> bool {
        d.session == self.session
            || (is_fact_key(d) && self.facts.contains(&d.id))
            || (is_segment_key(d) && self.segments.contains(&d.id))
    }

    /// ベクトルの索引の行を除くか。
    fn skips_row(&self, row: &evorag_index::vec::VecRow) -> bool {
        row.session == self.session
            || (row.session == SEGMENT_KEY_SESSION && self.segments.contains(&row.id))
    }
}

/// セッション `current` の区切り (v0.5 の根。区切りの無い根では空)。
fn current_segments(store: &Store, current: Id) -> HashSet<Id> {
    store
        .read()
        .ok()
        .and_then(|r| r.links_to(current, REL_SEGMENT_SESSION).ok())
        .map(|v| v.into_iter().map(|(id, _)| id).collect())
        .unwrap_or_default()
}

/// 今のセッション `current` のターンだけを出典とする事実 (残っている出典のターンが、今のセッションの外に
/// 1 つも無いもの)。その鍵は想起の候補を集める段階で除く (今のセッションのターンを添えないのと同じ、04 §5.5、
/// 05 §3.2)。事実は数百件の見込みなので、問いごとに読む。
fn current_only_facts(store: &Store, current: Id) -> HashSet<Id> {
    let Ok(rtx) = store.read() else {
        return HashSet::new();
    };
    let Ok(loaded) = evorag_memory::load_facts(&rtx) else {
        return HashSet::new();
    };
    let mut sessions: HashMap<Id, Option<Id>> = HashMap::new();
    let mut session_of = |turn: Id| {
        *sessions
            .entry(turn)
            .or_insert_with(|| match rtx.get(KIND_TURN, turn) {
                Ok(Some(Read::Ok(e))) => e.body_as::<TurnBody>().ok().map(|b| b.session),
                _ => None,
            })
    };
    loaded
        .set
        .facts
        .iter()
        .filter_map(evorag_memory::fact_key)
        .filter(|k| {
            !k.turns
                .iter()
                .any(|t| session_of(*t).is_some_and(|s| s != current))
        })
        .map(|k| k.fact)
        .collect()
}

/// 索引の当たり (チャンク・ノート・区切りの鍵・事実の鍵) が指すターン。ノートは、指す往復の発話のターンのすべて
/// (04 §2)。区切りの鍵は、区切りのすべての往復の発話のターンのうち読めるもの (今のセッションの区切りは無し、
/// 04 §5.5)。事実の鍵は、今の状態で鍵を持つ事実の出典のターンのうち、今のセッション `current` に無いもの
/// (新しい順、04 §5.5、05 §3.2)。
fn hit_turns(rtx: &evorag_store::ReadTxn<'_>, id: Id, current: Id) -> Vec<Id> {
    if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_CHUNK, id) {
        return e
            .body_as::<ChunkBody>()
            .map(|c| vec![c.turn])
            .unwrap_or_default();
    }
    if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_NOTE, id) {
        // 出典が DB に無い・改訂の番号が今と合わないノート (消した・伏せ字にした前の本文から作ったもの) は
        // 除く (04 §7.7 の 1)。
        return e
            .body_as::<NoteBody>()
            .ok()
            .filter(|n| evorag_memory::note_is_current(rtx, n))
            .map(|n| n.exchanges)
            .unwrap_or_default();
    }
    if let Ok(Some(Read::Ok(e))) = rtx.get(KIND_SEGMENT, id) {
        // 消したセッションの区切りは DB に無い (05 §3.2 の「DB に無い記録」)。往復の発話のターンが見えなくなっても
        // 区切りは変わらないので、読めるターンだけを指す。
        return e
            .body_as::<SegmentBody>()
            .ok()
            .filter(|s| s.session != current)
            .map(|s| {
                s.pairs
                    .into_iter()
                    .filter(|t| matches!(rtx.get(KIND_TURN, *t), Ok(Some(Read::Ok(_)))))
                    .collect()
            })
            .unwrap_or_default();
    }
    let Some(key) = evorag_memory::load_fact(rtx, id)
        .ok()
        .flatten()
        .and_then(|f| evorag_memory::fact_key(&f))
    else {
        return Vec::new();
    };
    key.turns
        .into_iter()
        .filter(|t| match rtx.get(KIND_TURN, *t) {
            Ok(Some(Read::Ok(e))) => e.body_as::<TurnBody>().is_ok_and(|b| b.session != current),
            _ => false,
        })
        .collect()
}

/// 起動時の突き合わせの計画 (DB を読むだけ)。
#[derive(Default)]
pub(crate) struct SyncPlan {
    /// 元のターンが無い・区切り方の版が古いチャンク (消す)。
    pub delete: Vec<Id>,
    /// チャンクの無いターンの、新しいチャンク。
    pub create: Vec<ChunkBody>,
    /// 残すチャンク・ノートの鍵・事実の鍵の、索引の 1 件。
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
    let mut revs: HashMap<Id, u32> = HashMap::new();
    let _ = rtx.for_each(KIND_TURN, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<TurnBody>()
            && indexable(&b)
            && let Some(rev) = evorag_memory::turn_rev_checked(&e)
        {
            revs.insert(e.id, rev);
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
    let mut notes: Vec<(Id, NoteBody)> = Vec::new();
    let _ = rtx.for_each(KIND_NOTE, |r| {
        // 出典が今と合うノートだけ (04 §7.7 の 1。合わないノートは、ノートの手順が作り直す)。
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<NoteBody>()
            && evorag_memory::note_is_current(&rtx, &b)
        {
            notes.push((e.id, b));
        }
        true
    });
    // 事実の鍵 (出典のターンの残っているもの。表と同じ判定、04 §5.4・§5.5)。
    let facts = crate::fact_table::surviving_facts(&rtx)
        .map(|set| evorag_memory::fact_keys(&set))
        .unwrap_or_default();
    drop(rtx);
    let mut plan = SyncPlan::default();
    // ノートの鍵 (出典の往復の残っているもの。出典の無いノートは、ノートの手順が消す)。
    for (id, n) in &notes {
        if n.exchanges.iter().any(|t| turns.contains_key(t)) {
            plan.keep.extend(note_docs(*id, n));
        }
    }
    plan.keep.extend(facts.iter().map(fact_doc));
    let mut covered: HashSet<Id> = HashSet::new();
    for (id, c) in chunks {
        match turns.get(&c.turn) {
            // 伏せ字の前の本文から作ったチャンク (出典の改訂の番号が今と合わない) は使わずに作り直す
            // (04 §7.7 の 1)。
            Some(t)
                if c.rules == evorag_retrieval::chunk::CHUNK_RULES_VERSION
                    && revs.get(&c.turn) == Some(&c.src_rev()) =>
            {
                covered.insert(c.turn);
                plan.keep.push(chunk_doc(id, &c, &t.text));
            }
            _ => plan.delete.push(id),
        }
    }
    for (id, t) in &turns {
        if !covered.contains(id) {
            plan.create.extend(chunk_bodies(*id, revs[id], t));
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
    current: Id,
) -> Vec<evorag_retrieval::recall::Hit> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        for turn in hit_turns(&rtx, h.id, current) {
            if seen.insert(turn) {
                out.push(evorag_retrieval::recall::Hit { id: turn, ..*h });
            }
        }
    }
    out
}
/// 文脈に添えるもの。
#[derive(Clone)]
pub struct Attached {
    /// 今の発話の直前に置く引用の枠 (添えるものが無ければ `None`)。
    pub quote: Option<String>,
    /// 引用の枠の中身 (乱数の区切りを除く)。先読みの枠と同じかを比べる (05 §4.5)。
    pub body: String,
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
    /// 候補の上位 (05 §3.6、v0.4)。
    pub candidates: Vec<evorag_memory::RecallCandidate>,
    /// 問いの分析の結果 (05 §3.1 の 5、v0.4)。
    pub query: Option<evorag_memory::QueryObs>,
    /// 引用に詰めた窓 (05 §3.8、v0.4)。
    pub packed: Option<evorag_memory::Packed>,
}

/// 観測値に残す候補の数 (05 §3.6)。
const CANDIDATES_OBS: usize = 30;

/// 融合の順 `ranked` の上位を、往復の代表のターンにまとめ、語彙 (`lists[0]`) とベクトル (`lists[1]`) の
/// それぞれの順位を添える (05 §3.6、v0.4)。
fn candidate_obs(
    store: &Store,
    lists: &[Vec<Id>],
    ranked: &[Id],
) -> Vec<evorag_memory::RecallCandidate> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let rank = |list: Option<&Vec<Id>>, id: Id| -> Option<u16> {
        list?
            .iter()
            .position(|x| *x == id)
            .and_then(|p| u16::try_from(p + 1).ok())
    };
    let mut out: Vec<evorag_memory::RecallCandidate> = Vec::new();
    for id in ranked {
        if out.len() >= CANDIDATES_OBS {
            break;
        }
        let rep = match rtx.get(KIND_TURN, *id) {
            Ok(Some(Read::Ok(e))) => match e.body_as::<TurnBody>() {
                Ok(b) if b.role != TurnRole::User => b.reply_to.unwrap_or(*id),
                _ => *id,
            },
            _ => continue,
        };
        if out.iter().any(|c| c.id == rep) {
            continue;
        }
        out.push(evorag_memory::RecallCandidate {
            id: rep,
            lex: rank(lists.first(), *id),
            vec: rank(lists.get(1), *id),
        });
    }
    out
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
    /// リランクの結果のキャッシュ (05 §4.5)。
    pub cache: Arc<RerankCache>,
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

/// 選んだターンを、その往復の相手と一緒に、セッションごとの出典にまとめる (スコアの順)。本文には `[非表示]` の
/// 置き換えを当てる (04 §7.5、05 §3.4)。
fn sources(store: &Store, hits: &[Id], lang: PromptLanguage) -> Vec<Source> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    // 伏せる値を確かめられなければ、今回は添えない (帯・詳細にも出さない、04 §7.5)。
    let Some(hide) = crate::masking::Hide::load(&rtx, lang) else {
        return Vec::new();
    };
    let mut order: Vec<Id> = Vec::new();
    let mut picked: HashMap<Id, HashSet<Id>> = HashMap::new();
    let mut session_turns: HashMap<Id, Vec<Turn>> = HashMap::new();
    for id in hits {
        let Ok(Some(Read::Ok(e))) = rtx.get(KIND_TURN, *id) else {
            continue;
        };
        // 忘れるの指示と合わないターンは添えない・帯に出さない (04 §7.2)。
        if hide.excluded(&e) {
            continue;
        }
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
            let mut turns: Vec<Turn> = session_turns
                .remove(&sid)
                .unwrap_or_default()
                .into_iter()
                .filter(|t| set.contains(&t.envelope.id) && !hide.excluded(&t.envelope))
                .collect();
            hide.apply(&rtx, &mut turns);
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

/// 語彙の上位のターンを往復にまとめ、上位 `limit` 往復にする (05 §3.3 の 2)。本文には `[非表示]` の置き換えを
/// 当てる (リランクの要求にも private の値を送らない、04 §7.5)。
fn exchanges(store: &Store, ranked: &[Id], limit: usize, lang: PromptLanguage) -> Vec<Exchange> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    // 伏せる値を確かめられなければ、リランクにも送らない (今回は添えない)。
    let Some(hide) = crate::masking::Hide::load(&rtx, lang) else {
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
        // 忘れるの指示と合わないターンは、リランクにも送らない (04 §7.2)。
        if hide.excluded(&e) {
            continue;
        }
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
        let mut members: Vec<Turn> = turns
            .iter()
            .filter(|t| t.envelope.id == *id || Some(t.envelope.id) == other)
            .filter(|t| !hide.excluded(&t.envelope))
            .cloned()
            .collect();
        hide.apply(&rtx, &mut members);
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

    /// すべて捨てる (忘れるの後、04 §7.2 の「捨てるキャッシュ」)。
    pub(crate) fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// リランクの結果のキャッシュ (入力が止まったときに先に済ませた結果を、同じ発話を送ったときに使う、
/// 05 §4.5)。鍵はリランカーの model_key・問い・往復の本文で、候補が変われば使わない。保存しない。
#[derive(Default)]
pub struct RerankCache(std::sync::Mutex<Vec<(std::time::Instant, RerankKey, Vec<f32>)>>);

type RerankKey = (String, String, Vec<String>);

/// リランクの結果のキャッシュに残す数。
const RERANK_CACHE_MAX: usize = 8;

impl RerankCache {
    fn get(&self, key: &RerankKey) -> Option<Vec<f32>> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|(t, _, _)| t.elapsed() < QUERY_CACHE_TTL);
        m.iter()
            .find(|(_, k, _)| k == key)
            .map(|(_, _, s)| s.clone())
    }

    fn put(&self, key: RerankKey, scores: Vec<f32>) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|(_, k, _)| *k != key);
        if m.len() >= RERANK_CACHE_MAX {
            m.remove(0);
        }
        m.push((std::time::Instant::now(), key, scores));
    }

    /// すべて捨てる (忘れるの後、04 §7.2 の「捨てるキャッシュ」)。
    pub(crate) fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// 問いの埋め込みとリランクの締切。応答の経路は 05 §3.4 の値。先に済ませる処理は利用者を待たせない
/// ので長くする (遅いサーバでも、送ったときにキャッシュから使えるように)。
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub embed: std::time::Duration,
    pub rerank: std::time::Duration,
}

pub const RESPONSE_DEADLINES: Deadlines = Deadlines {
    embed: EMBED_DEADLINE,
    rerank: RERANK_DEADLINE,
};

pub const PRECOMPUTE_DEADLINES: Deadlines = Deadlines {
    embed: std::time::Duration::from_secs(5),
    rerank: std::time::Duration::from_secs(5),
};

/// 問いを埋め込む (キャッシュにあればそれ。締切を過ぎたら `None`)。
pub async fn embed_query(
    v: &VectorQuery,
    text: &str,
    deadline: std::time::Duration,
) -> Option<Vec<f32>> {
    if let Some(e) = v.cache.get(&v.key, text) {
        return Some(e);
    }
    let input = [text.to_owned()];
    match tokio::time::timeout(deadline, v.client.embed(&input)).await {
        Ok(Ok(mut out)) if out.len() == 1 && !out[0].is_empty() => {
            let e = out.pop()?;
            v.cache.put(&v.key, text, e.clone());
            Some(e)
        }
        _ => None,
    }
}

/// ベクトルの当たり (チャンク) を、ターンの順位にする (チャンクの最も良い順位をターンの順位にする)。
fn vector_turns(store: &Store, rows: &[(evorag_index::vec::VecRow, f32)], current: Id) -> Vec<Id> {
    let Ok(rtx) = store.read() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (row, _) in rows {
        for turn in hit_turns(&rtx, row.id, current) {
            if seen.insert(turn) {
                out.push(turn);
            }
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

/// 評価のため (09 §5.2 の検索の評価): リランクの前の順位 (語彙とベクトルの候補を RRF で合わせたもの) を、
/// 上位 `limit` 往復のターンの並びで返す。応答の経路 (`select_for`) と同じ部品で組み立てる。締切は設けない。
pub(crate) async fn eval_exchanges(
    store: &Store,
    index: Option<&LexIndex>,
    session: Id,
    query: &str,
    vectors: Option<&VectorQuery>,
    limit: usize,
) -> Vec<Vec<Id>> {
    let Some(index) = index else {
        return Vec::new();
    };
    let snap = index.snapshot();
    let current_only = CurrentOnly::of(store, session);
    let r = recall(
        &snap,
        query,
        None,
        &|d| current_only.skips(d),
        &RecallConfig::default(),
    );
    let mut lists: Vec<Vec<Id>> = vec![
        turn_hits(store, &r.candidates, session)
            .iter()
            .map(|h| h.id)
            .collect(),
    ];
    if let Some(v) = vectors
        && let Some(qv) = embed_query(v, query, std::time::Duration::from_secs(30)).await
    {
        let rows = v
            .index
            .snapshot()
            .search(&qv, CANDIDATES, &|row| current_only.skips_row(row));
        lists.push(vector_turns(store, &rows, session));
    }
    // ID だけを返すので、置き換えの言語はどちらでもよい。
    exchanges(store, &rrf(&lists), limit, PromptLanguage::Ja)
        .into_iter()
        .map(|x| x.turns.iter().map(|t| t.envelope.id).collect())
        .collect()
}

/// 自動で選んだ結果 (添えるターン、判定点、観測値)。
struct Selection {
    hits: Vec<Id>,
    decision: RecallDecision,
    obs: RecallObs,
    /// 引いた問い (追い質問なら直前の発話を足したもの)。窓の文の点数に使う。
    query: String,
    intent: evorag_retrieval::window::QueryIntent,
}

/// セッションの `upto` までの発話の本文 (新しい順)。
fn user_texts(store: &Store, session: Id, upto: Id, lang: PromptLanguage) -> Vec<String> {
    let Ok(r) = store.read() else {
        return Vec::new();
    };
    let turns = evorag_memory::session_turns(&r, session).unwrap_or_default();
    // 忘れるの指示と合わない発話は、問いの本文 (語彙・埋め込み・リランクの問い) に使わない (04 §7.2)。確かめ
    // られなければ使わない。
    let hide = crate::masking::Hide::load(&r, lang);
    turns
        .iter()
        .rev()
        .filter(|t| t.body.role == TurnRole::User && t.envelope.id <= upto)
        .filter(|t| hide.as_ref().is_some_and(|h| !h.excluded(&t.envelope)))
        .map(|t| t.body.text.clone())
        .collect()
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
    let mut users = user_texts(store, session, user_turn, lang).into_iter();
    let query = users.next().unwrap_or_default();
    let previous = users.next();
    select_for(
        store,
        index,
        lang,
        session,
        query,
        previous,
        mode,
        vectors,
        RESPONSE_DEADLINES,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn select_for(
    store: &Store,
    index: Option<&LexIndex>,
    lang: PromptLanguage,
    session: Id,
    query: String,
    previous: Option<String>,
    mode: &RerankMode,
    vectors: Option<&VectorQuery>,
    deadlines: Deadlines,
) -> Selection {
    let mut obs = RecallObs::default();
    let t = tuning();
    let intent = evorag_retrieval::window::analyze(&query);
    let widen = t.windows && t.count_widen && intent.count;
    obs.query = Some(evorag_memory::QueryObs {
        count: intent.count,
        date: intent.date,
        explicit: intent.explicit,
        widened: widen,
    });
    if vectors.is_none() {
        // 埋め込みが無い・記録と違う・作り直しの途中 (05 §3.5 の `no_embed`)。
        obs.degraded
            .push(evorag_memory::DEGRADED_NO_EMBED.to_owned());
    }
    let q_for_window = query.clone();
    let none = |decision, obs: RecallObs| Selection {
        hits: Vec::new(),
        decision,
        obs,
        query: q_for_window.clone(),
        intent,
    };
    let Some(index) = index else {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return none(RecallDecision::Skip, obs);
    };
    let snap = index.snapshot();
    // 今のセッションのターンは直近の会話に入るので、候補から除く (05 §3.2)。今のセッションだけを出典とする
    // 事実の鍵と、今のセッションの区切りの鍵も除く (04 §5.5)。
    let current_only = CurrentOnly::of(store, session);
    let mut r = recall(
        &snap,
        &query,
        previous.as_deref(),
        &|d| current_only.skips(d),
        &RecallConfig::default(),
    );
    if r.decision == Decision::Skip {
        obs.degraded = vec![evorag_memory::DEGRADED_NO_INDEX.to_owned()];
        return none(RecallDecision::Skip, obs);
    }
    // 索引の単位はチャンク。ターンにまとめる (05 §2.4)。
    r.hits = turn_hits(store, &r.hits, session);
    r.candidates = turn_hits(store, &r.candidates, session);
    {
        let lex: Vec<Id> = r.candidates.iter().map(|h| h.id).collect();
        obs.candidates = candidate_obs(store, std::slice::from_ref(&lex), &lex);
    }
    let lexical = |obs: RecallObs| {
        // ターンに直して何も残らない当たり (鍵の事実が読み直しで外れたなど) は、添えない。
        if r.decision == Decision::Abstain || r.hits.is_empty() {
            none(RecallDecision::Abstain, obs)
        } else {
            Selection {
                hits: r.hits.iter().map(|h| h.id).collect(),
                decision: RecallDecision::Fire,
                obs,
                query: q_for_window.clone(),
                intent,
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
        match embed_query(v, &q, deadlines.embed).await {
            Some(qv) => {
                let rows = v
                    .index
                    .snapshot()
                    .search(&qv, CANDIDATES, &|row| current_only.skips_row(row));
                lists.push(vector_turns(store, &rows, session));
            }
            // 問いの埋め込みが締切に間に合わない: その要求だけ語彙の候補で (05 §3.5)。
            None => obs
                .degraded
                .push(evorag_memory::DEGRADED_EMBED_DEADLINE.to_owned()),
        }
    }
    let ranked = rrf(&lists);
    obs.candidates = candidate_obs(store, &lists, &ranked);
    let top_n = if widen { t.count_top.max(u.top) } else { u.top };
    let ex = exchanges(store, &ranked, top_n, lang);
    if ex.is_empty() {
        return none(RecallDecision::Abstain, obs);
    }
    let docs: Vec<String> = ex.iter().map(|x| exchange_text(lang, x)).collect();
    let key: RerankKey = (u.key.clone(), q.clone(), docs);
    let reranked = match u.cache.get(&key) {
        Some(s) => Ok(Ok(s)),
        None => tokio::time::timeout(deadlines.rerank, u.client.rerank(&q, &key.2)).await,
    };
    let scores = match reranked {
        Ok(Ok(s)) if s.len() == ex.len() => {
            u.cache.put(key, s.clone());
            s
        }
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
    // 2 件目以降は、下限 (しきい値と帯の下限の間) 以上の往復も採る (v0.4、05 §3.4)。
    let mut chosen = vec![top];
    if fire && t.windows {
        let floor = thr - t.floor * (thr - band_low);
        let max = if widen { t.count_max } else { t.max_exchanges };
        for &(i, s) in order.iter().skip(1) {
            if chosen.len() >= max {
                break;
            }
            if f64::from(s) >= floor {
                chosen.push(i);
            }
        }
    }
    obs.band = band(&order, &ex, |i, s| {
        (!fire || !chosen.contains(&i)) && f64::from(s) >= band_low && f64::from(s) < thr
    });
    if !fire {
        return none(RecallDecision::Abstain, obs);
    }
    Selection {
        hits: chosen
            .iter()
            .flat_map(|&i| ex[i].turns.iter().map(|t| t.envelope.id))
            .collect(),
        query: q_for_window.clone(),
        intent,
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
    budget: usize,
    large: bool,
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
            body: String::new(),
            attached: Vec::new(),
            record: RecallRecord {
                decision: sel.decision,
                sources: Vec::new(),
            },
            obs: sel.obs,
        };
    }
    pack(
        store, lang, llm, tokens, model_key, budget, large, session, base, sel,
    )
    .await
}

/// 選んだ往復を引用にする (v0.4 は文の窓、05 §3.8)。`budget` は文脈の枠の予算 (n_ctx が小さいときは減って
/// いる)。`large` は先読みが当たったときの大きな予算を使うか (05 §3.8 の 5)。
#[allow(clippy::too_many_arguments)]
async fn pack(
    store: &Store,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    budget: usize,
    large: bool,
    session: Id,
    base: u32,
    sel: Selection,
) -> Attached {
    let t = tuning();
    if t.windows {
        let b = if t.count_widen && sel.intent.count {
            t.count_budget
        } else if large {
            t.budget
        } else {
            t.small_budget
        };
        // n_ctx が小さいとき (frame_budgets が減らしたとき) は、同じ比で減らす。
        let budget = b * budget / crate::context::RECALL_BUDGET;
        return finish_windows(
            store, lang, llm, tokens, model_key, budget, session, base, sel, &t,
        )
        .await;
    }
    finish(
        store, lang, llm, tokens, model_key, budget, session, base, sel.hits, sel.obs,
    )
    .await
}

/// 入力が止まったときに、送る前の発話 `draft` で想起し、大きな予算で引用の枠を作る (投機的な先読み、
/// 05 §4.5)。問いの埋め込みとリランクの結果はキャッシュに残る。保存しない。
#[allow(clippy::too_many_arguments)]
pub async fn draft_attach(
    store: &Store,
    index: Option<&LexIndex>,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    budget: usize,
    session: Id,
    draft: String,
    base: u32,
    mode: &RerankMode,
    vectors: Option<&VectorQuery>,
) -> Attached {
    // 送ると、今の最後の発話が「直前の発話」になる。
    let previous = user_texts(store, session, Id::from_bytes([0xff; 16]), lang)
        .into_iter()
        .next();
    let sel = select_for(
        store,
        index,
        lang,
        session,
        draft,
        previous,
        mode,
        vectors,
        PRECOMPUTE_DEADLINES,
    )
    .await;
    if sel.hits.is_empty() {
        return Attached {
            quote: None,
            body: String::new(),
            attached: Vec::new(),
            record: RecallRecord {
                decision: sel.decision,
                sources: Vec::new(),
            },
            obs: sel.obs,
        };
    }
    pack(
        store, lang, llm, tokens, model_key, budget, true, session, base, sel,
    )
    .await
}

/// 先読みで作った引用のターンを、今も添えてよいか (添える直前のふるい分け、05 §3.4)。DB に無い・忘れるの
/// 指示と合わないターンがあれば偽。
pub fn still_attachable(store: &Store, ids: &[Id], lang: PromptLanguage) -> bool {
    let Ok(rtx) = store.read() else {
        return false;
    };
    let Some(hide) = crate::masking::Hide::load(&rtx, lang) else {
        return false;
    };
    ids.iter().all(|id| match rtx.get(KIND_TURN, *id) {
        Ok(Some(Read::Ok(e))) => !hide.excluded(&e),
        _ => false,
    })
}

/// 1 ターンを窓にした引用の本文 (v0.4、05 §3.4)。`scores` はそのターンの文ごとの点数。
fn window_text(
    text: &str,
    sents: &[evorag_retrieval::window::Sentence],
    scores: &[f64],
    role: TurnRole,
    t: &RecallTuning,
) -> (String, Vec<(usize, usize)>) {
    if text.chars().count() <= t.short_chars || sents.is_empty() {
        return head_span(text, t.turn_chars);
    }
    let mut ranked: Vec<usize> = (0..sents.len()).filter(|&i| scores[i] > 0.0).collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    ranked.truncate(t.hits_per_turn);
    // 2 つ目以降の当たった文は、最も高い文の半分以上の点数のものだけ (機能語だけが重なる文を除く)。
    if let Some(&best) = ranked.first() {
        let top = scores[best];
        ranked.retain(|&i| scores[i] * 2.0 >= top);
    }
    if ranked.is_empty() {
        // 当たった文が無い (言い換え): 発話は短い頭、応答は頭から。
        let n = if role == TurnRole::User {
            t.short_chars / 2
        } else {
            t.turn_chars / 2
        };
        return head_span(text, n);
    }
    let body = |i: usize| &text[sents[i].start..sents[i].end];
    let is_table = |i: usize| body(i).trim_start().starts_with('|');
    let is_list = |i: usize| {
        let b = body(i).trim_start();
        b.starts_with("- ")
            || b.starts_with("* ")
            || b.starts_with('・')
            || b.split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &h in &ranked {
        let lo = h.saturating_sub(t.window);
        let hi = (h + t.window).min(sents.len() - 1);
        ranges.push((lo, hi));
        // 表や箇条書きの中なら、見出しの行 (表の 1 行目、箇条の前の文) を足す。
        if is_table(h) {
            let mut s0 = h;
            while s0 > 0 && is_table(s0 - 1) {
                s0 -= 1;
            }
            ranges.push((s0, s0));
        } else if is_list(h) {
            let mut s0 = h;
            while s0 > 0 && is_list(s0 - 1) {
                s0 -= 1;
            }
            if s0 > 0 {
                ranges.push((s0 - 1, s0 - 1));
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    let mut out = String::new();
    if merged[0].0 > 0 {
        out.push_str("… ");
    }
    for (k, (lo, hi)) in merged.iter().enumerate() {
        if k > 0 {
            out.push_str(" … ");
        }
        out.push_str(&text[sents[*lo].start..sents[*hi].end]);
    }
    if merged.last().is_some_and(|l| l.1 + 1 < sents.len()) {
        out.push_str(" …");
    }
    let spans = merged
        .iter()
        .map(|(lo, hi)| (sents[*lo].start, sents[*hi].end))
        .collect();
    (head_ellipsis(&out, t.turn_chars), spans)
}

/// 頭から `n` 文字 (と、その範囲)。
fn head_span(text: &str, n: usize) -> (String, Vec<(usize, usize)>) {
    let end = text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    (head_ellipsis(text, n), vec![(0, end)])
}

fn head_ellipsis(text: &str, n: usize) -> String {
    let mut out: String = text.chars().take(n).collect();
    if text.chars().count() > n {
        out.push('…');
    }
    out
}

/// 選んだ往復を出典にまとめ、文の窓で詰める (v0.4、05 §3.4)。値の高い往復の出典から、予算に入るだけ。
#[allow(clippy::too_many_arguments)]
async fn finish_windows(
    store: &Store,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    budget: usize,
    session: Id,
    base: u32,
    sel: Selection,
    t: &RecallTuning,
) -> Attached {
    let nonce = nonce();
    let srcs: Vec<Source> = sources(store, &sel.hits, lang)
        .into_iter()
        .filter(|s| s.session != session && !s.turns.is_empty())
        .collect();
    // 文の点数の重みは、添える候補のすべての文で求める。
    let texts: Vec<Vec<String>> = srcs
        .iter()
        .map(|s| {
            s.turns
                .iter()
                .map(|t| t.body.text.replace(&nonce, ""))
                .collect()
        })
        .collect();
    let sents: Vec<Vec<Vec<evorag_retrieval::window::Sentence>>> = texts
        .iter()
        .map(|ts| {
            ts.iter()
                .map(|x| evorag_retrieval::window::sentences(x))
                .collect()
        })
        .collect();
    let mut flat: Vec<&str> = Vec::new();
    for (si, ts) in texts.iter().enumerate() {
        for (ti, x) in ts.iter().enumerate() {
            for s in &sents[si][ti] {
                flat.push(&x[s.start..s.end]);
            }
        }
    }
    let all_scores = evorag_retrieval::window::sentence_scores(&sel.query, &flat);
    // 各ターンの文の点数の始まり (all_scores の中の位置)。
    let mut offs: Vec<Vec<usize>> = Vec::with_capacity(sents.len());
    let mut at = 0;
    for ss in &sents {
        offs.push(
            ss.iter()
                .map(|x| {
                    let o = at;
                    at += x.len();
                    o
                })
                .collect(),
        );
    }
    // 出典 `si` の引用の本文と窓 (番号 `n`、数値 `tt`)。
    let render = |si: usize, n: usize, tt: &RecallTuning| {
        let s = &srcs[si];
        let when = s
            .turns
            .first()
            .map(|t| crate::localtime::datetime(t.envelope.created_at.as_millis()))
            .unwrap_or_default();
        let title = s.title.replace(&nonce, "");
        let mut text = match lang {
            PromptLanguage::Ja => format!("[{n}] {when} セッション「{title}」\n"),
            PromptLanguage::En => format!("[{n}] {when} session \"{title}\"\n"),
        };
        let mut these = Vec::new();
        for (ti, turn) in s.turns.iter().enumerate() {
            let o = offs[si][ti];
            let scores = &all_scores[o..o + sents[si][ti].len()];
            let (w, spans) =
                window_text(&texts[si][ti], &sents[si][ti], scores, turn.body.role, tt);
            text.push_str(role_label(lang, turn.body.role));
            text.push_str(": ");
            text.push_str(&w);
            text.push('\n');
            // 窓の範囲は、添えたときの本文 ([非表示] の置き換えの後) のバイト位置。
            let rev = evorag_memory::turn_rev(&turn.envelope);
            these.extend(spans.into_iter().map(|(a, b)| evorag_memory::WindowRef {
                turn: turn.envelope.id,
                rev,
                start: u32::try_from(a).unwrap_or(u32::MAX),
                end: u32::try_from(b).unwrap_or(u32::MAX),
            }));
        }
        (text, these)
    };
    // 最上位の出典が予算に入らないときに使う、狭い窓 (当たった文だけ、05 §3.8 の 4)。
    let tight = RecallTuning {
        window: 0,
        hits_per_turn: 1,
        short_chars: t.short_chars.min(120),
        turn_chars: t.turn_chars.min(240),
        ..*t
    };
    let mut body = String::new();
    let mut used = 0;
    let mut attached = Vec::new();
    let mut counts = Vec::new();
    let mut windows: Vec<evorag_memory::WindowRef> = Vec::new();
    for (si, s) in srcs.iter().enumerate() {
        let n = base as usize + counts.len();
        let mut tries = vec![*t];
        if counts.is_empty() {
            tries.push(tight);
        }
        for tt in &tries {
            let (text, these) = render(si, n, tt);
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
            windows.extend(these);
            break;
        }
    }
    let mut obs = sel.obs;
    obs.packed = Some(evorag_memory::Packed {
        windows,
        tokens: u32::try_from(used).unwrap_or(u32::MAX),
        budget: u32::try_from(budget).unwrap_or(u32::MAX),
    });
    if counts.is_empty() {
        return Attached {
            quote: None,
            body: String::new(),
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
        body,
        attached,
        record: RecallRecord {
            decision: RecallDecision::Fire,
            sources: counts,
        },
        obs,
    }
}

/// 選んだターンを出典にまとめ、予算の中で引用の枠を作る。
#[allow(clippy::too_many_arguments)]
async fn finish(
    store: &Store,
    lang: PromptLanguage,
    llm: &dyn LlmClient,
    tokens: &TokenCache,
    model_key: &str,
    budget: usize,
    session: Id,
    base: u32,
    hits: Vec<Id>,
    obs: RecallObs,
) -> Attached {
    let nonce = nonce();
    let mut body = String::new();
    let mut used = 0;
    let mut attached = Vec::new();
    let mut counts = Vec::new();
    for s in sources(store, &hits, lang) {
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
            body: String::new(),
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
        body,
        attached,
        record: RecallRecord {
            decision: RecallDecision::Fire,
            sources: counts,
        },
        obs,
    }
}

/// 往復 (代表のターンと、その相手) のターン。帯の詳細に使う (05 §5.2)。本文には `[非表示]` の置き換えを当てる。
pub fn exchange_turns(store: &Store, target: Id, lang: PromptLanguage) -> Option<Vec<Turn>> {
    sources(store, &[target], lang)
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

/// 応答の出典 `n` (このセッションの通し番号) のターン。本文には `[非表示]` の置き換えを当てる (表示に使う)。
pub fn source_turns(
    store: &Store,
    answer: &TurnBody,
    n: usize,
    lang: PromptLanguage,
) -> Option<Vec<Turn>> {
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
    let hide = crate::masking::Hide::load(&rtx, lang)?;
    // 忘れるの指示と合わないターンは出さない (04 §7.2)。
    out.retain(|t| !hide.excluded(&t.envelope));
    hide.apply(&rtx, &mut out);
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

    /// 今のセッションの区切りの鍵の項目と行は、候補を集める段階で除く。ほかのセッションの区切りの鍵と、セッションに
    /// 結び付く項目 (チャンク) の今のセッションの判定は、そのまま。
    ///
    /// verifies: 04 §5.5 (鍵の項目はセッションに結び付けない), 05 §3.2 (今のセッションのターンは除く)
    #[test]
    fn segment_keys_of_the_current_session_are_skipped() {
        let id = |b: u8| Id::from_bytes([b; 16]);
        let (current, other) = (id(1), id(2));
        let (mine, theirs) = (id(3), id(4));
        let only = CurrentOnly {
            session: current,
            facts: HashSet::new(),
            segments: [mine].into_iter().collect(),
        };
        let doc = |id: Id, session: Id| evorag_index::lex::Doc {
            id,
            session,
            len: 1,
        };
        assert!(only.skips(&doc(mine, SEGMENT_KEY_SESSION)));
        assert!(!only.skips(&doc(theirs, SEGMENT_KEY_SESSION)));
        assert!(
            only.skips(&doc(id(5), current)),
            "a chunk of the current session"
        );
        assert!(!only.skips(&doc(id(5), other)));
        let row = |id: Id, session: Id| evorag_index::vec::VecRow {
            id,
            session,
            unit: evorag_index::vec::Unit::Key,
        };
        assert!(only.skips_row(&row(mine, SEGMENT_KEY_SESSION)));
        assert!(!only.skips_row(&row(theirs, SEGMENT_KEY_SESSION)));
        // 区切りの鍵の値でない行は、区切りの ID と同じでも除かない (ノートの鍵などの別の項目)。
        assert!(!only.skips_row(&row(mine, other)));
    }

    /// verifies: AC-FR-402-1 (語彙とベクトルの順位を RRF で合わせる、05 §3.3)
    #[test]
    fn rrf_prefers_turns_found_by_both_and_breaks_ties_by_recency() {
        let id = |n: u8| Id::from_bytes([n; 16]);
        // 両方に出たもの (3) が先。次に片方の 1 位 (1)。片方の 2 位どうし (2 と 4) は同点で、新しい
        // (ID の大きい) 方が先。
        let fused = rrf(&[vec![id(1), id(2), id(3)], vec![id(3), id(4)]]);
        assert_eq!(fused, [id(3), id(1), id(4), id(2)]);
    }

    fn windows_of(text: &str, query: &str, t: &RecallTuning) -> String {
        let sents = evorag_retrieval::window::sentences(text);
        let flat: Vec<&str> = sents.iter().map(|s| &text[s.start..s.end]).collect();
        let scores = evorag_retrieval::window::sentence_scores(query, &flat);
        window_text(text, &sents, &scores, TurnRole::Assistant, t).0
    }

    fn filler(n: usize) -> String {
        (0..n)
            .map(|i| format!("Step {i} is about something unrelated and fairly generic."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// verifies: AC-FR-417-2 (応答の 400 文字より後ろにある、問いに当たった文が窓に入る)
    #[test]
    fn a_late_sentence_that_answers_the_question_is_quoted() {
        let t = RecallTuning {
            windows: true,
            ..RecallTuning::default()
        };
        let text = format!(
            "{} Your golden retriever is named Biscuit. {}",
            filler(12),
            filler(3).replace("Step", "Note")
        );
        assert!(text.find("Biscuit").unwrap() > 400);
        let w = windows_of(&text, "What is the name of my golden retriever?", &t);
        assert!(w.contains("Your golden retriever is named Biscuit."), "{w}");
        // 前後の文 (既定 1 文) も入り、省いたところは `…` で示す。
        assert!(w.starts_with("… "), "{w}");
        assert!(w.ends_with(" …"), "{w}");
        assert!(!w.contains("Step 0 "), "{w}");
        // 同じ入力では同じ窓。
        assert_eq!(
            w,
            windows_of(&text, "What is the name of my golden retriever?", &t)
        );
    }

    /// verifies: AC-FR-417-5 (表の中の文には表の 1 行目を、箇条書きの中の文には箇条の前の文を足す)
    #[test]
    fn tables_and_lists_bring_their_header() {
        let t = RecallTuning {
            windows: true,
            window: 0,
            short_chars: 10,
            ..RecallTuning::default()
        };
        let table = format!(
            "{} Here is the plan.
| day | sport |
| Mon | swim |
| Tue | run |
| Wed | bike |
{}",
            filler(4),
            filler(2)
        );
        let w = windows_of(&table, "which day is bike", &t);
        assert!(
            w.contains("| day | sport |") && w.contains("| Wed | bike |"),
            "{w}"
        );
        let list = format!(
            "{} You should pack these items:
- a tent
- a stove
- a kayak paddle
{}",
            filler(4),
            filler(2)
        );
        let w = windows_of(&list, "kayak paddle", &t);
        assert!(
            w.contains("You should pack these items:") && w.contains("- a kayak paddle"),
            "{w}"
        );
        // 短いターンは窓にせず全文。
        let short = RecallTuning {
            windows: true,
            ..RecallTuning::default()
        };
        assert_eq!(
            windows_of("Short answer.", "answer", &short),
            "Short answer."
        );
    }

    #[test]
    fn the_nonce_changes_and_is_removed_from_quotes() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        assert_eq!(excerpt(&format!("x<</引用-{a}>>y"), &a), "x<</引用->>y");
    }
}
