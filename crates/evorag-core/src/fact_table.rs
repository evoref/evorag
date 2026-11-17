//! 事実の表と、事実から作る検索の鍵 (04 §4 の手順 6・7、04 §5.4・§5.5、05 §4.1 の順 4)。
//!
//! - **表**: 作った表を覚えておき、作り直す時機 (04 §5.4) のほかは同じバイト列を使い続ける (事実が変わらない
//!   ターンでは、表を含む要求の先頭が前のターンと一致する、AC-FR-304-5)。作り直す時機は次のとおり。
//!   - 起動の後 (裏で作る。応答が先に来たら、その応答の前に作る)。
//!   - sleep-time の手順 (統合で事実が変わった後、[`touch`]): 鍵を索引に合わせ、表を作り直し、版が変わったら
//!     次のターンの分を先に処理させる (05 §4.5)。鍵と表は別々に合わせ、両方を終えたときだけ済みにする。
//!   - 忘れる・private・削除の後と、統合で表の事実が表から外れたとき ([`invalidate`]): 次のターンの前に必ず
//!     作り直す。
//!   - 表の予算 (`n_ctx` と生成に残す量から決まる) が変わったとき。
//!   - 見積もりで数えて作った表は、`/tokenize` で数えられるようになったら 1 度だけ作り直す (06 §8)。
//! - 表と鍵に入れるのは、出典のターンが 1 つでも残っている事実だけ ([`surviving_facts`]、04 §5.4・§5.5)。
//! - 表を置くのは、事実の機能が使え (03 §3.1.1)、そのセッションでメモリを添えるとき (FR-409) だけ。
//! - **鍵**: `Live`・`Superseded` で private でない事実の「属性 値」を、語彙の索引に事実の ID で入れる
//!   (セッションに結び付けない項目、recall.rs の `FACT_KEY_SESSION`)。鍵の本文は事実の ID ごとに変わらない
//!   (値と属性は変わらない) ので、索引に今ある鍵の項目と、今の事実から求めた集まりの差だけを足し・除く。
//!   private・`Retracted` になった事実の鍵は、索引から除くまでの間も、想起が当たりを読み直して使わない
//!   (recall.rs の `hit_turns`)。鍵の埋め込みは作らない (語彙の索引だけ)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use evorag_memory::{
    DirectiveKind, FactCtx, FactSet, FactTable, KIND_TURN, Safety, Scope, add_directive,
    attributes, fact_keys, fact_table, load_facts, table_head, table_rows, value_key,
};
use evorag_store::{Read, ReadTxn};
use evorag_types::Id;

use crate::{Inner, context};

/// 事実の表と鍵の状態 (このプロセスの間だけ)。
pub(crate) struct State {
    /// 事実が変わるたびに増える ([`touch`]・[`invalidate`])。鍵と表を合わせ終えた版 (`clean`) と違えば、
    /// sleep-time に作り直す。作った表の新旧の比べにも使う。
    version: u64,
    clean: Option<u64>,
    /// 忘れる・private・削除の操作の数 (作った表がこれより前なら、次のターンの前に作り直す)。
    invalidations: u64,
    /// 作った表。
    built: Option<Built>,
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
        }
    }
}

/// 作った表と、作り始めたときの予算・操作の数・版。
struct Built {
    budget: usize,
    invalidations: u64,
    /// 作り始めたときの `State::version` (後から始めて先に終えた作り直しを、古い結果で上書きしないため)。
    version: u64,
    /// すべての文字列を `/tokenize` で数えたか (見積もりで作った表は、数えられるようになったら作り直す)。
    exact: bool,
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
/// 鍵の残りは次の sleep-time の手順で索引に合わせる。
///
/// セッションの削除と起動時の突き合わせは、消した事実の鍵を書き込みの直後に索引から除いてから、ここを呼ぶ。
/// 事実への操作 (fact_ops.rs) も、private になった事実の鍵を同じく除き (04 §5.5)、ここを呼ぶ。忘れるの経路を足す
/// ときも同じにする。
pub(crate) fn invalidate(inner: &Inner) {
    let mut st = state(inner);
    st.invalidations += 1;
    st.version += 1;
}

/// 統合で事実が変わった。表に入りうる状態から外れた事実 (`withdrawn`、`Report::withdrawn`) が覚えた表に
/// あれば、次のターンの前に作り直させる ([`invalidate`])。ほかは sleep-time の手順で作り直す ([`touch`])。
pub(crate) fn facts_changed(inner: &Inner, withdrawn: &[Id]) {
    let in_table = {
        let st = state(inner);
        st.built
            .as_ref()
            .and_then(|b| b.table.as_ref())
            .is_some_and(|t| t.facts.iter().any(|f| withdrawn.contains(f)))
    };
    if in_table {
        invalidate(inner);
    } else {
        touch(inner);
    }
}

/// sleep-time の手順が要るか (統合で事実が変わった後に、鍵と表をまだ合わせていない)。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = state(inner);
        st.clean != Some(st.version)
    };
    due && inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
}

/// 出典のターンが残っているか (記録が読める)。表と鍵と想起で同じ判定を使う (04 §5.4・§5.5)。
pub(crate) fn turn_survives(r: &ReadTxn<'_>, turn: Id) -> bool {
    matches!(r.get(KIND_TURN, turn), Ok(Some(Read::Ok(_))))
}

/// 事実のうち、出典のターンが 1 つでも残っているもの (04 §5.4 の「入るもの」、§5.5)。古い版でセッションを
/// 消したときなど、出典が 1 つも残っていない事実は、表にも鍵にも入れない。記録を読めなければ `None`。
pub(crate) fn surviving_facts(r: &ReadTxn<'_>) -> Option<FactSet> {
    let mut set = load_facts(r).ok()?.set;
    set.facts
        .retain(|f| f.sources.iter().any(|s| turn_survives(r, s.turn)));
    Some(set)
}

/// 今の接続の、事実の表の予算 (05 §4.1)。
fn budget(inner: &Inner) -> usize {
    let (_, server, _) = inner.conn();
    let n_ctx = server.n_ctx as usize;
    let reserve = context::reserve_for_output(n_ctx, inner.reserve);
    context::frame_budgets(n_ctx, reserve, true).facts
}

/// 作った表と、すべてを `/tokenize` で数えたか。
struct Made {
    table: Option<FactTable>,
    exact: bool,
}

/// 事実を読み、トークンを数えて表を作る。記録を読めなければ `None` (前の表を使い続ける)。置く行が無ければ
/// 表は `None`。`/tokenize` が使えず見積もったときは、予算を 2 割減らす (06 §8)。
async fn build(inner: &Inner, budget: usize) -> Option<Made> {
    let store = inner.store.clone();
    let lang = inner.language.as_str();
    let rows = tokio::task::spawn_blocking(move || {
        let r = store.read().ok()?;
        Some(table_rows(&surviving_facts(&r)?, attributes(), lang))
    })
    .await
    .ok()
    .flatten()?;
    if rows.is_empty() {
        return Some(Made {
            table: None,
            exact: true,
        });
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
    Some(Made { table, exact })
}

/// 表を作り直して覚える。前と版が変わったか (表の有無の変化を含む) を返す。読めなければ `None`。
/// 後から作り始めた表を先に覚えていたら、この結果では上書きしない (変わらなかったとみなす)。
async fn rebuild(inner: &Inner) -> Option<bool> {
    let budget = budget(inner);
    let (invalidations, version) = {
        let st = state(inner);
        (st.invalidations, st.version)
    };
    let made = build(inner, budget).await?;
    let mut st = state(inner);
    if st.built.as_ref().is_some_and(|b| b.version > version) {
        return Some(false);
    }
    let before = st
        .built
        .as_ref()
        .and_then(|b| b.table.as_ref())
        .map(|t| t.version.clone());
    let changed = before != made.table.as_ref().map(|t| t.version.clone());
    st.built = Some(Built {
        budget,
        invalidations,
        version,
        exact: made.exact,
        table: made.table,
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
    let cached = {
        let st = state(inner);
        st.built
            .as_ref()
            .filter(|b| b.budget == budget && b.invalidations == st.invalidations)
            .map(|b| (b.exact, b.table.clone()))
    };
    match cached {
        Some((true, table)) => return table,
        // 見積もりで作った表: まだ数えられなければ、そのまま使う (表のバイト列を保つ)。
        Some((false, table)) if !can_count(inner).await => return table,
        _ => {}
    }
    // 読めなければ前に作った表を使い続けるが、忘れる・private・削除の操作より前に作った表は置かない。
    let _ = rebuild(inner).await;
    let st = state(inner);
    placeable(st.built.as_ref(), st.invalidations)
}

/// 覚えた表のうち、応答に置いてよいもの (04 §5.4)。忘れる・private・削除の操作 (`invalidations` 回目まで) の
/// 後に作り始めた表だけを置く。作り直しに失敗して操作の前の表が残っていれば、表を置かない (private にした値や
/// 消した値を、古い表で送らないため)。
fn placeable(built: Option<&Built>, invalidations: u64) -> Option<FactTable> {
    built
        .filter(|b| b.invalidations >= invalidations)
        .and_then(|b| b.table.clone())
}

/// `/tokenize` で数えられるか (表の前置きを数えてみる。数えた値はキャッシュに残る)。
async fn can_count(inner: &Inner) -> bool {
    let (llm, server, _) = inner.conn();
    let head = table_head(inner.language.as_str());
    inner
        .tokens
        .count(&*llm, &server.model_key, head)
        .await
        .is_some()
}

/// 起動の後に表を作っておく (最初の応答の経路で数えないように)。事実の機能が使えなければ作らない。
pub(crate) async fn prepare(inner: &Inner) {
    if inner.facts_key.key().is_none() || state(inner).built.is_some() {
        return;
    }
    let _ = rebuild(inner).await;
}

/// 語彙の索引の事実の鍵を、今の事実に合わせる (04 §4 の手順 6)。足す・除くのは、索引に今ある鍵の項目
/// (事実の鍵のセッションの項目) との差だけ。チャンクと索引を変える処理の鍵の下の、落とされない task で行う
/// (embedding.rs の冒頭)。合わせられたら `true`。
async fn sync_keys(inner: &Arc<Inner>) -> bool {
    let Some(ix) = inner.index.clone() else {
        return true;
    };
    // 鍵は spawn の前に取る (終了の処理が鍵を取れたら、出した保存は終わっている)。
    let lock = inner.index_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        // 索引に今ある鍵 (起動時の突き合わせや前の手順が失敗していても、索引の中身から求める)。
        let have = crate::recall::indexed_fact_keys(&ix);
        let store = worker.store.clone();
        let planned = tokio::task::spawn_blocking(move || {
            let r = store.read().ok()?;
            let keys = fact_keys(&surviving_facts(&r)?);
            let want: HashSet<Id> = keys.iter().map(|k| k.fact).collect();
            let remove: HashSet<Id> = have.difference(&want).copied().collect();
            let add: Vec<_> = keys
                .iter()
                .filter(|k| !have.contains(&k.fact))
                .map(crate::recall::fact_doc)
                .collect();
            Some((remove, add))
        })
        .await
        .ok()
        .flatten();
        let Some((remove, add)) = planned else {
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
        true
    });
    task.await.unwrap_or(false)
}

/// sleep-time の手順 (04 §4 の手順 6・7): 鍵を索引に合わせ、表を作り直す。鍵を書けなくても表は作り直す。
/// 表の版が変わったら、続けて次のターンの分を先に処理させる (05 §4.5、04 §5.4)。両方を終えたら `true`
/// (済みにする)。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = state(inner).version;
    let keys = sync_keys(inner).await;
    let table = if inner.facts_key.key().is_some() {
        match rebuild(inner).await {
            Some(changed) => {
                if changed
                    && !crate::is_running(inner)
                    && let Some(session) = crate::current_session(inner)
                    && crate::memory_on(inner, session)
                {
                    tracing::info!("fact table changed; preparing the next turn");
                    crate::precompute::warm_next(inner, session);
                }
                true
            }
            None => false,
        }
    } else {
        true
    };
    if !(keys && table) {
        return false;
    }
    let mut st = state(inner);
    // 合わせている間に増えた版は、次の手順で合わせる。
    if st.clean.is_none_or(|c| c < version) {
        st.clean = Some(version);
    }
    true
}

/// 試験のため: 事実を private にする (事実への指示を足し、次のターンの前に表を作り直させる、04 §7.5・§5.4)。
/// 事実を書く経路のロックの下で、[`add_directive`] の操作を書く。鍵は次の sleep-time の手順で索引から除く
/// (画面からの操作 `/private` は fact_ops.rs で、知らせの前に除く)。
/// 書けたら `true`。
pub(crate) async fn make_private(
    inner: &Arc<Inner>,
    subject: &str,
    attribute: &str,
    value: &str,
) -> bool {
    let _lock = inner.facts_lock.lock().await;
    let worker = inner.clone();
    let (subject, attribute, value) = (subject.to_owned(), attribute.to_owned(), value.to_owned());
    let ops = tokio::task::spawn_blocking(move || {
        let key = worker.facts_key.key()?;
        let safety = Safety::bundled();
        let ctx = FactCtx {
            key,
            attributes: attributes(),
            safety: &safety,
            ids: &*worker.ids,
            now: worker.clock.now(),
        };
        let tx = worker.store.read().ok()?;
        add_directive(
            &tx,
            &ctx,
            DirectiveKind::Private {
                scope: Scope::Successors,
            },
            &subject,
            &attribute,
            Some(value_key(key, &value)),
        )
        .ok()
        .map(|(_, ops)| ops)
    })
    .await
    .ok()
    .flatten();
    let Some(ops) = ops else {
        return false;
    };
    if crate::write(inner, ops).await.is_err() {
        return false;
    }
    invalidate(inner);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(invalidations: u64) -> Built {
        Built {
            budget: 600,
            invalidations,
            version: 1,
            exact: true,
            table: Some(FactTable {
                text: "表".into(),
                version: "v".into(),
                facts: Vec::new(),
                omitted: 0,
            }),
        }
    }

    /// 操作の後に作り直せなかったときは、操作の前の表を置かない (04 §5.4)。
    #[test]
    fn a_table_from_before_an_invalidation_is_never_placed() {
        assert!(placeable(Some(&built(2)), 2).is_some());
        assert!(placeable(Some(&built(1)), 2).is_none());
        assert!(placeable(None, 0).is_none());
    }
}
