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
    KIND_CHUNK, KIND_CORRECTION, KIND_NOTE, KIND_PROPOSAL, KIND_RECALL_FEEDBACK, KIND_SEGMENT,
    KIND_SESSION, KIND_TURN, LedgerRead, NoteBody, RecallFeedbackBody, SegmentBody, read_ledger,
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
    /// 出典のセッションか往復のターンが無い区切りと、出典のターンが無い台帳 (v0.5 の根、04 §5.2.1 の「台帳を
    /// 消すとき」)。
    segments: Vec<evorag_store::Envelope>,
    ledgers: Vec<evorag_store::Envelope>,
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
    let turns = ids_of(&r, KIND_TURN)?;
    let sessions = ids_of(&r, KIND_SESSION)?;
    let chunks = ids_of(&r, KIND_CHUNK)?;
    let mut p = Plan::default();
    let mut notes = HashSet::new();
    r.for_each(KIND_NOTE, |x| {
        if let Read::Ok(e) = x
            && let Ok(n) = e.body_as::<NoteBody>()
        {
            // 出典が欠けた・改訂の番号が今と合わない (伏せ字の前の本文から作った) ノートも消す (04 §7.7 の 1・4)。
            if sessions.contains(&n.session) && evorag_memory::note_is_current(&r, &n) {
                notes.insert(e.id);
            } else {
                p.notes.push(e.id);
            }
        }
        true
    })
    .ok()?;
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
    // 区切りは、セッションか往復のターンがすべて無ければ消す。台帳は、出典のターン (`sentences`・`refs`・`corr`・
    // `through`) がすべて無ければ消す (区切りの記録を失った台帳は、区切りを戻すのに使うので残す、04 §2)。
    let (segments, ledgers) = sourceless_segments_and_ledgers(&r, &sessions, &turns)?;
    // 埋め込みは、対象 (チャンク・ノート・台帳) が無いか消すものなら消す (区切りの鍵の埋め込みは台帳を指す、
    // 04 §5.5)。v0.4 の形式の根には台帳の種類が無い。
    let gone: HashSet<Id> = ledgers.iter().map(|e| e.id).collect();
    let kept_ledgers: HashSet<Id> = ids_of(&r, KIND_PROPOSAL)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !gone.contains(id))
        .collect();
    for (id, b) in evorag_memory::embedding_bodies(&r).ok()? {
        if !chunks.contains(&b.target)
            && !notes.contains(&b.target)
            && !kept_ledgers.contains(&b.target)
        {
            p.embeddings.push(id);
        }
    }
    p.segments = segments;
    p.ledgers = ledgers;
    // 事実は、出典のターンの記録が DB に無ければ、その出典を外す (03 §4.2 の規則 12 の「見つけ方」)。
    // 読めなければ、事実は次の起動で突き合わせる (ほかの派生の記録の掃除は止めない)。
    if let Ok(facts) = evorag_memory::drop_turns(&r, &|t| !turns.contains(&t), ids, now)
        && !facts.ops.is_empty()
    {
        p.facts = Some(facts);
    }
    Some(p)
}

/// 出典の無い区切りと台帳。
fn sourceless_segments_and_ledgers(
    r: &evorag_store::ReadTxn<'_>,
    sessions: &HashSet<Id>,
    turns: &HashSet<Id>,
) -> Option<(Vec<evorag_store::Envelope>, Vec<evorag_store::Envelope>)> {
    let mut segments = Vec::new();
    r.for_each(KIND_SEGMENT, |x| {
        if let Read::Ok(e) = x
            && let Ok(b) = e.body_as::<SegmentBody>()
            && (!sessions.contains(&b.session) || b.pairs.iter().all(|t| !turns.contains(t)))
        {
            segments.push(e);
        }
        true
    })
    .ok()?;
    let mut ledgers = Vec::new();
    r.for_each(KIND_PROPOSAL, |x| {
        if let Read::Ok(e) = x
            && let Some(LedgerRead::Segment(b)) = read_ledger(&e)
        {
            let sources: Vec<Id> = b
                .sources()
                .into_iter()
                .map(|s| s.turn)
                .chain(b.inputs.corr.iter().map(|c| c.turn))
                .chain(b.inputs.through.map(|t| t.turn))
                .collect();
            if !sources.is_empty() && sources.iter().all(|t| !turns.contains(t)) {
                ledgers.push(e);
            }
        }
        true
    })
    .ok()?;
    Some((segments, ledgers))
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
        && p.segments.is_empty()
        && p.ledgers.is_empty()
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
    for e in p.segments.iter().chain(&p.ledgers) {
        ops.extend(evorag_memory::delete_segment_or_ledger(e));
    }
    for f in &p.feedback {
        // 想起への操作はジャーナルにも書く種類なので、消したこともジャーナルに残る。
        ops.extend(w.delete_feedback(*f));
    }
    // ジャーナルに書く種類を消したら、次の起動で compact して跡を除く (03 §3.1.1。派生の記録だけの削除では
    // 立てない)。
    if !p.feedback.is_empty() {
        ops.push(WriteOp::SetCompactPending);
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
            .chain(p.segments.iter().map(|e| &e.id))
            .chain(p.ledgers.iter().map(|e| &e.id))
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
        segments = p.segments.len(),
        ledgers = p.ledgers.len(),
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
