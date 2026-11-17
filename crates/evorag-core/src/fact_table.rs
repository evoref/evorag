//! 事実の表と、事実から作る検索の鍵 (04 §4 の手順 6・7、04 §5.4・§5.5、05 §4.1 の順 4)。
//!
//! - **表**: 作った表を覚えておき、作り直す時機 (04 §5.4) のほかは同じバイト列を使い続ける (事実が変わらない
//!   ターンでは、表を含む要求の先頭が前のターンと一致する、AC-FR-304-5)。作り直す時機は次のとおり。
//!   - 起動の後 (裏で作る。応答が先に来たら、その応答の前に作る)。
//!   - sleep-time の手順 (統合で事実が変わった後、[`touch`]): 鍵を索引に合わせてから表を作り直し、版が
//!     変わったら次のターンの分を先に処理させる (05 §4.5)。
//!   - 忘れる・private・削除の後 ([`invalidate`]): 次のターンの前に必ず作り直す。
//!   - 表の予算 (`n_ctx` と生成に残す量から決まる) が変わったとき。
//! - 表を置くのは、事実の機能が使え (03 §3.1.1)、そのセッションでメモリを添えるとき (FR-409) だけ。
//! - **鍵**: `Live`・`Superseded` で private でない事実の「属性 値」を、語彙の索引に事実の ID で入れる。鍵の本文は
//!   事実の ID ごとに変わらない (値と属性は変わらない) ので、入れた事実の ID の集まりと、今の事実から求めた
//!   集まりの差だけを足し・除く。private・`Retracted` になった事実の鍵は、索引から除くまでの間も、想起が
//!   当たりを読み直して使わない (recall.rs の `hit_turns`)。鍵の埋め込みは作らない (語彙の索引だけ)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use evorag_memory::{
    FactTable, attributes, fact_keys, fact_table, load_facts, table_head, table_rows,
};
use evorag_store::Read;
use evorag_types::Id;

use crate::{Inner, context};

/// 事実の表と鍵の状態 (このプロセスの間だけ)。
pub(crate) struct State {
    /// 統合で事実が変わるたびに増える。鍵と表を合わせ終えた版 (`clean`) と違えば、sleep-time に作り直す。
    version: u64,
    clean: Option<u64>,
    /// 忘れる・private・削除の操作の数 (作った表がこれより前なら、次のターンの前に作り直す)。
    invalidations: u64,
    /// 作った表。
    built: Option<Built>,
    /// 語彙の索引に入れた事実の鍵 (事実の ID)。
    keyed: HashSet<Id>,
}

impl Default for State {
    /// 起動のときは、鍵は起動時の突き合わせ (`sync_chunks`) で、表は起動の後の準備 ([`prepare`]) で合わせる
    /// ので、sleep-time の手順は要らない状態で始める。
    fn default() -> Self {
        Self {
            version: 0,
            clean: Some(0),
            invalidations: 0,
            built: None,
            keyed: HashSet::new(),
        }
    }
}

/// 作った表と、作ったときの予算・操作の数。
struct Built {
    budget: usize,
    invalidations: u64,
    /// 置く行が無ければ `None` (表を置かない)。
    table: Option<FactTable>,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.fact_table.lock().unwrap_or_else(|e| e.into_inner())
}

/// 統合で事実が変わった (sleep-time の手順で、鍵と表を作り直す)。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 忘れる・private・削除の操作をした (04 §5.4 の「作り直す時機」)。次のターンの前に必ず表を作り直し、
/// 鍵も次の sleep-time の手順で合わせる。忘れる・private の操作の経路 (M3.4) から呼ぶ。
pub(crate) fn invalidate(inner: &Inner) {
    let mut st = state(inner);
    st.invalidations += 1;
    st.version += 1;
}

/// 起動時の突き合わせで、語彙の索引に入れた事実の鍵。
pub(crate) fn set_keyed(inner: &Inner, ids: HashSet<Id>) {
    state(inner).keyed = ids;
}

/// 語彙の索引に入れた事実の鍵 (埋め込みの対象から除くため)。
pub(crate) fn keyed(inner: &Inner) -> HashSet<Id> {
    state(inner).keyed.clone()
}

/// sleep-time の手順が要るか (統合で事実が変わった後に、鍵と表をまだ合わせていない)。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = state(inner);
        st.clean != Some(st.version)
    };
    due && inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
}

/// 今の接続の、事実の表の予算 (05 §4.1)。
fn budget(inner: &Inner) -> usize {
    let (_, server, _) = inner.conn();
    let n_ctx = server.n_ctx as usize;
    let reserve = context::reserve_for_output(n_ctx, inner.reserve);
    context::frame_budgets(n_ctx, reserve, true).facts
}

/// 事実を読み、トークンを数えて表を作る。記録を読めなければ `None` (前の表を使い続ける)。置く行が無ければ
/// `Some(None)`。`/tokenize` が使えず見積もったときは、予算を 2 割減らす (06 §8)。
async fn build(inner: &Inner, budget: usize) -> Option<Option<FactTable>> {
    let store = inner.store.clone();
    let lang = inner.language.as_str();
    let rows = tokio::task::spawn_blocking(move || {
        let r = store.read().ok()?;
        let loaded = load_facts(&r).ok()?;
        Some(table_rows(&loaded.set, attributes(), lang))
    })
    .await
    .ok()
    .flatten()?;
    if rows.is_empty() {
        return Some(None);
    }
    let (llm, server, _) = inner.conn();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut exact = true;
    let texts = std::iter::once(table_head(lang)).chain(rows.iter().map(|r| r.line.as_str()));
    for text in texts {
        if counts.contains_key(text) {
            continue;
        }
        let n = match inner.tokens.count(&*llm, &server.model_key, text).await {
            Some(n) => n,
            None => {
                exact = false;
                context::estimate(text)
            }
        };
        counts.insert(text, n);
    }
    let budget = if exact { budget } else { budget * 4 / 5 };
    let table = fact_table(&rows, lang, budget, |t| {
        counts
            .get(t)
            .copied()
            .unwrap_or_else(|| context::estimate(t))
    });
    if let Some(t) = &table
        && t.omitted > 0
    {
        tracing::info!(
            rows = t.facts.len(),
            omitted = t.omitted,
            budget,
            "fact table over budget; some facts were left out"
        );
    }
    Some(table)
}

/// 表を作り直して覚える。前と版が変わったか (表の有無の変化を含む) を返す。読めなければ `None`。
async fn rebuild(inner: &Inner) -> Option<bool> {
    let budget = budget(inner);
    let invalidations = state(inner).invalidations;
    let table = build(inner, budget).await?;
    let mut st = state(inner);
    let before = st
        .built
        .as_ref()
        .and_then(|b| b.table.as_ref())
        .map(|t| t.version.clone());
    let changed = before != table.as_ref().map(|t| t.version.clone());
    st.built = Some(Built {
        budget,
        invalidations,
        table,
    });
    Some(changed)
}

/// 応答 (と、次のターンの分の先の処理) のシステムプロンプトに置く事実の表 (05 §4.1 の順 4)。
/// 覚えた表が今のものなら、それをそのまま使う (作り直さない)。
pub(crate) async fn for_turn(inner: &Inner, session: Id) -> Option<FactTable> {
    if inner.facts_key.key().is_none() || !crate::memory_on(inner, session) {
        return None;
    }
    let budget = budget(inner);
    {
        let st = state(inner);
        if let Some(b) = &st.built
            && b.budget == budget
            && b.invalidations == st.invalidations
        {
            return b.table.clone();
        }
    }
    // 読めなければ、前に作った表を使い続ける。
    let _ = rebuild(inner).await;
    state(inner).built.as_ref().and_then(|b| b.table.clone())
}

/// 起動の後に表を作っておく (最初の応答の経路で数えないように)。事実の機能が使えなければ作らない。
pub(crate) async fn prepare(inner: &Inner) {
    if inner.facts_key.key().is_none() || state(inner).built.is_some() {
        return;
    }
    let _ = rebuild(inner).await;
}

/// 語彙の索引の事実の鍵を、今の事実に合わせる (04 §4 の手順 6)。足す・除くのは鍵の差だけ。チャンクと索引を
/// 変える処理の鍵の下の、落とされない task で行う (embedding.rs の冒頭)。合わせられたら `true`。
async fn sync_keys(inner: &Arc<Inner>) -> bool {
    let Some(ix) = inner.index.clone() else {
        return true;
    };
    // 鍵は spawn の前に取る (終了の処理が鍵を取れたら、出した保存は終わっている)。
    let lock = inner.index_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let have = keyed(&worker);
        let store = worker.store.clone();
        let planned = tokio::task::spawn_blocking(move || {
            let r = store.read().ok()?;
            let keys = fact_keys(&load_facts(&r).ok()?.set);
            // 出典のターンが 1 つも残っていない事実の鍵は入れない (起動時の突き合わせと同じ)。
            let keys: Vec<_> = keys
                .into_iter()
                .filter(|k| {
                    k.turns.iter().any(|t| {
                        matches!(r.get(evorag_memory::KIND_TURN, *t), Ok(Some(Read::Ok(_))))
                    })
                })
                .collect();
            let want: HashSet<Id> = keys.iter().map(|k| k.fact).collect();
            let remove: HashSet<Id> = have.difference(&want).copied().collect();
            let add: Vec<_> = keys
                .iter()
                .filter(|k| !have.contains(&k.fact))
                .map(crate::recall::fact_doc)
                .collect();
            Some((want, remove, add))
        })
        .await
        .ok()
        .flatten();
        let Some((want, remove, add)) = planned else {
            return false;
        };
        if remove.is_empty() && add.is_empty() {
            return true;
        }
        let (removed, added) = (remove.len(), add.len());
        let done = tokio::task::spawn_blocking(move || {
            if !remove.is_empty() {
                ix.remove(&remove)?;
            }
            ix.add(add)
        })
        .await;
        if !matches!(done, Ok(Ok(()))) {
            tracing::warn!("fact keys could not be written to the lexical index");
            return false;
        }
        tracing::info!(added, removed, "fact keys synced");
        state(&worker).keyed = want;
        true
    });
    task.await.unwrap_or(false)
}

/// sleep-time の手順 (04 §4 の手順 6・7): 鍵を索引に合わせ、表を作り直す。表の版が変わったら、続けて次の
/// ターンの分を先に処理させる (05 §4.5、04 §5.4)。合わせ終えたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = state(inner).version;
    if !sync_keys(inner).await {
        return false;
    }
    if inner.facts_key.key().is_some() {
        let Some(changed) = rebuild(inner).await else {
            return false;
        };
        if changed
            && !crate::is_running(inner)
            && let Some(session) = crate::current_session(inner)
            && crate::memory_on(inner, session)
        {
            tracing::info!("fact table changed; preparing the next turn");
            crate::precompute::warm_next(inner, session);
        }
    }
    state(inner).clean = Some(version);
    true
}
