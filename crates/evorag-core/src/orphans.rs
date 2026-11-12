//! 出典の無い派生の記録の突き合わせ (03 §4.2 の規則 12、04 §7)。
//!
//! 古い版 (v0.1.2 など) は、新しい版の種類 (チャンク・ノート・埋め込み・想起への操作) を知らないので、
//! セッションを消しても、それらを消さない。起動時に、出典の無くなったものを DB と索引から消し、退避からも
//! 除く job (03 §7.3) を同じ依頼で書く。出典の無いチャンクは `sync_chunks` が先に消す (その ID を受け取る)。

use std::collections::HashSet;
use std::sync::Arc;

use evorag_memory::{
    KIND_CHUNK, KIND_NOTE, KIND_RECALL_FEEDBACK, KIND_SESSION, KIND_TURN, NoteBody,
    RecallFeedbackBody,
};
use evorag_store::{Read, Store, WriteOp};
use evorag_types::Id;

use crate::Inner;

#[derive(Default)]
struct Plan {
    notes: Vec<Id>,
    embeddings: Vec<Id>,
    feedback: Vec<Id>,
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

fn plan(store: &Store) -> Option<Plan> {
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
            if sessions.contains(&n.session) && n.exchanges.iter().any(|t| turns.contains(t)) {
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
    Some(p)
}

/// 突き合わせる。`gone_chunks` は `sync_chunks` が消したチャンク (退避からも除く)。
pub(crate) async fn reconcile(inner: &Arc<Inner>, gone_chunks: Vec<Id>) {
    let _lock = inner.index_lock.lock().await;
    let store = inner.store.clone();
    let Ok(Some(p)) = tokio::task::spawn_blocking(move || plan(&store)).await else {
        return;
    };
    if gone_chunks.is_empty()
        && p.notes.is_empty()
        && p.embeddings.is_empty()
        && p.feedback.is_empty()
    {
        return;
    }
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
    for f in &p.feedback {
        // 想起への操作はジャーナルにも書く種類なので、消したこともジャーナルに残る。
        ops.extend(w.delete_feedback(*f));
    }
    let mut purge: Vec<Id> = gone_chunks.clone();
    purge.extend(p.notes.iter().chain(&p.embeddings).chain(&p.feedback));
    let Ok((_, job)) = evorag_memory::purge_job(&w, purge) else {
        return;
    };
    ops.push(job);
    tracing::info!(
        chunks = gone_chunks.len(),
        notes = p.notes.len(),
        embeddings = p.embeddings.len(),
        feedback = p.feedback.len(),
        "orphaned derived records removed"
    );
    if crate::write(inner, ops).await.is_err() {
        return;
    }
    // 索引から除く (チャンクの語彙の項目は sync_chunks が除いた)。
    let gone: HashSet<Id> = gone_chunks
        .into_iter()
        .chain(p.notes.iter().copied())
        .collect();
    crate::embedding::remove_everywhere(inner, &gone);
    if let Some(ix) = inner.index.clone() {
        let notes: HashSet<Id> = p.notes.into_iter().collect();
        if !notes.is_empty() {
            let _ = tokio::task::spawn_blocking(move || ix.remove(&notes)).await;
        }
    }
}
