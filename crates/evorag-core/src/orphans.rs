//! 出典の無い派生の記録の突き合わせ (03 §4.2 の規則 12、04 §7)。
//!
//! 古い版 (v0.1.2 など) は、新しい版の種類 (チャンク・ノート・埋め込み・想起への操作・訂正・事実) を知らない
//! ので、セッションを消しても、それらを消さない。起動時に、出典の無くなったものを DB と索引から消し、退避からも
//! 除く job (03 §7.3) を同じ依頼で書く。出典の無いチャンクは `sync_chunks` が先に消す (その ID を受け取る)。
//! 事実は、出典のターンの一部が無ければその出典を外し、すべて無ければ消す (04 §5.3 の「出典の無い事実の
//! 掃除」。セッションの削除と同じ書き込みの入口 `drop_turns` を通す)。

use std::collections::HashSet;
use std::sync::Arc;

use evorag_memory::{
    KIND_CHUNK, KIND_CORRECTION, KIND_NOTE, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_TURN,
    NoteBody, RecallFeedbackBody,
};
use evorag_store::{Read, Store, WriteOp};
use evorag_types::{Id, IdGen, Timestamp};

use crate::Inner;

#[derive(Default)]
struct Plan {
    notes: Vec<Id>,
    embeddings: Vec<Id>,
    feedback: Vec<Id>,
    corrections: Vec<Id>,
    /// 事実の書き込み (出典を外す・消す)。何も変えなければ `None`。
    facts: Option<evorag_memory::SourcesDropped>,
}

fn ids_of(r: &evorag_store::ReadTxn<'_>, kind: &str) -> Option<HashSet<Id>> {
    let mut out = HashSet::new();
    r.for_each(kind, |x| {
        out.insert(match x {
            Read::Ok(e) => e.id,
            Read::Corrupt { id, .. } | Read::Newer { id, .. } => id,
        });
        true
    })
    .ok()?;
    Some(out)
}

fn plan(store: &Store, ids: &dyn IdGen, now: Timestamp) -> Option<Plan> {
    let r = store.read().ok()?;
    let mut revs: std::collections::HashMap<Id, u32> = std::collections::HashMap::new();
    r.for_each(KIND_TURN, |x| {
        match x {
            Read::Ok(e) => revs.insert(e.id, evorag_memory::turn_rev(&e)),
            Read::Corrupt { id, .. } | Read::Newer { id, .. } => revs.insert(id, 0),
        };
        true
    })
    .ok()?;
    let turns: HashSet<Id> = revs.keys().copied().collect();
    let sessions = ids_of(&r, KIND_SESSION)?;
    let chunks = ids_of(&r, KIND_CHUNK)?;
    let mut p = Plan::default();
    let mut notes = HashSet::new();
    r.for_each(KIND_NOTE, |x| {
        if let Read::Ok(e) = x
            && let Ok(n) = e.body_as::<NoteBody>()
        {
            // 出典が欠けた・改訂の番号が今と合わない (伏せ字の前の本文から作った) ノートも消す (04 §7.7 の 1・4)。
            if sessions.contains(&n.session)
                && evorag_memory::note_matches(&n, |t| revs.get(&t).copied())
            {
                notes.insert(e.id);
            } else {
                p.notes.push(e.id);
            }
        }
        true
    })
    .ok()?;
    for (id, b) in evorag_memory::embedding_bodies(&r).ok()? {
        if !chunks.contains(&b.target) && !notes.contains(&b.target) {
            p.embeddings.push(id);
        }
    }
    r.for_each(KIND_RECALL_FEEDBACK, |x| {
        if let Read::Ok(e) = x
            && let Ok(f) = e.body_as::<RecallFeedbackBody>()
            && (!turns.contains(&f.answer) || !turns.contains(&f.target))
        {
            p.feedback.push(e.id);
        }
        true
    })
    .ok()?;
    // 訂正は、出典のターン (候補の発話と直前の応答) がすべて無くなったら消す (04 §5.3 の「出典の無い事実の掃除」)。
    r.for_each(KIND_CORRECTION, |x| {
        if let Read::Ok(e) = x
            && let Some((turn, previous)) = evorag_memory::correction_sources(&e)
            && !turns.contains(&turn)
            && previous.is_none_or(|p| !turns.contains(&p))
        {
            p.corrections.push(e.id);
        }
        true
    })
    .ok()?;
    // 事実は、出典のターンの記録が DB に無ければ、その出典を外す (03 §4.2 の規則 12 の「見つけ方」)。
    // 読めなければ、事実は次の起動で突き合わせる (ほかの派生の記録の掃除は止めない)。
    if let Ok(facts) = evorag_memory::drop_turns(&r, &|t| !turns.contains(&t), ids, now)
        && !facts.ops.is_empty()
    {
        p.facts = Some(facts);
    }
    Some(p)
}

/// 突き合わせる。`gone_chunks` は `sync_chunks` が消したチャンク (退避からも除く)。
pub(crate) async fn reconcile(inner: &Arc<Inner>, gone_chunks: Vec<Id>) {
    let _lock = inner.index_lock.lock().await;
    // 事実を読んでから書き終えるまで、事実を書く経路のロックを持つ (04 §5.2.1。鍵の順は index_lock →
    // facts_lock)。
    let _facts = inner.facts_lock.lock().await;
    let store = inner.store.clone();
    let ids = inner.ids.clone();
    let now = inner.clock.now();
    let Ok(Some(mut p)) = tokio::task::spawn_blocking(move || plan(&store, &*ids, now)).await
    else {
        return;
    };
    if gone_chunks.is_empty()
        && p.notes.is_empty()
        && p.embeddings.is_empty()
        && p.feedback.is_empty()
        && p.corrections.is_empty()
        && p.facts.is_none()
    {
        return;
    }
    // 事実を書き換えたら (出典を外した・消した・private の写しを計算し直した)、表を作り直させる。
    let facts_changed = p.facts.is_some();
    let facts = p.facts.take().unwrap_or_default();
    let w = crate::writes(inner);
    let mut ops = Vec::new();
    for e in &p.embeddings {
        ops.extend(w.delete_embedding(*e));
    }
    for n in &p.notes {
        ops.push(WriteOp::Delete {
            kind: KIND_NOTE.into(),
            id: *n,
        });
        ops.push(WriteOp::DeleteLinksOf(*n));
    }
    for c in &p.corrections {
        ops.push(WriteOp::Delete {
            kind: KIND_CORRECTION.into(),
            id: *c,
        });
        ops.push(WriteOp::DeleteLinksOf(*c));
    }
    for f in &p.feedback {
        // 想起への操作はジャーナルにも書く種類なので、消したこともジャーナルに残る。
        ops.extend(w.delete_feedback(*f));
    }
    // 消した事実 (同じ値に合わせて消した矛盾の値も含む) は、操作の列から求める。
    let gone_facts = evorag_memory::deleted_ids(&facts.ops);
    ops.extend(facts.ops);
    let mut purge: Vec<Id> = gone_chunks.clone();
    purge.extend(
        p.notes
            .iter()
            .chain(&p.embeddings)
            .chain(&p.feedback)
            .chain(&p.corrections)
            .chain(&gone_facts),
    );
    let purge_ids = purge.clone();
    let Ok((_, job)) = evorag_memory::purge_job(&w, purge) else {
        return;
    };
    ops.push(job);
    tracing::info!(
        chunks = gone_chunks.len(),
        notes = p.notes.len(),
        embeddings = p.embeddings.len(),
        feedback = p.feedback.len(),
        corrections = p.corrections.len(),
        facts_deleted = gone_facts.len(),
        facts_trimmed = facts.dropped.trimmed,
        "orphaned derived records removed"
    );
    if crate::write(inner, ops).await.is_err() {
        return;
    }
    // 今と前の世代のすべての索引から除く (ノートの鍵と、消した事実の検索の鍵を含む。ID は事実の ID、04 §5.5)。
    // 除けなかった分は、この後の整理の job (起動時) が続ける (04 §7.7 の 2)。
    let gone: HashSet<Id> = purge_ids.into_iter().collect();
    let worker = inner.clone();
    let _ = tokio::task::spawn_blocking(move || crate::remove_from_indexes(&worker, &gone)).await;
    if facts_changed {
        // 表を作り直し、鍵の残りを次の sleep-time の手順で合わせる (04 §5.4・§5.5)。
        crate::fact_table::invalidate(inner);
    }
}
