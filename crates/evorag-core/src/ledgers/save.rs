//! 台帳と区切りを書く (04 §4・§5.2.1、03 §3.4)。書く前の確かめ・忘れるの掃除・置き換えて消す台帳。

use std::collections::BTreeSet;
use std::sync::Arc;

use evorag_memory::b2;
use evorag_memory::corrections::CORRECTION_V1;
use evorag_memory::{
    KIND_PROPOSAL, KIND_TURN, LedgerBody, LedgerRead, LedgerStatus, RuleVersions, ScrubContext,
    SegmentBody, TurnTexts, ledger_valid, read_ledger, scrub_ledger, turn_rev_checked,
};
use evorag_store::{Envelope, Read, ReadTxn};
use evorag_types::Id;

use super::Target;
use super::snapshot::load_directives;
use crate::Inner;

/// 台帳を書いた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Saved {
    Written,
    /// 書かなかった: 出典のターンが読んだときから変わった・消えた (古い本文から作った台帳を捨てる)、か、読めない・
    /// 書けない。
    Discarded,
    /// 書かなかった: 今の選択の前の `partial` の方が、完結した要素が多い (前の `partial` を残す)。
    Kept,
}

/// 台帳の型の合った要素の数 (`keys`・`units`・`corrections` の合計)。
fn elements(l: &LedgerBody) -> usize {
    l.output.keys().items.len() + l.output.units().items.len() + l.output.corrections().items.len()
}

/// 置き換えて消す台帳 (04 §5.2.1 の「台帳を消すとき」)。書かずに前の台帳を残すなら `Err(Saved::Kept)`。
///
/// - `complete` を書くとき: 同じ区切りのほかの台帳のうち、直前の選択の最新の有効な台帳を除くものを消す
///   (`partial` と末尾の区切りの前の台帳を含む)。
/// - `partial` を書くとき: 今の選択の前の `partial` のどれかの方が完結した要素が多ければ書かない。ほかは、今の
///   選択の前の `partial` と `failed` を消す。
/// - `failed` を書くとき: 今の選択の前の `failed` だけを消す (使える `partial`・`complete` は残す)。
fn replaced(
    r: &ReadTxn<'_>,
    new: &LedgerBody,
    target: &Target,
    previous: Option<&str>,
) -> Option<Result<Vec<Envelope>, Saved>> {
    let seg = new.segment?;
    let mut found: Vec<(Envelope, LedgerBody)> = Vec::new();
    for (id, _) in r.links_to(seg, evorag_memory::REL_LEDGER_SEGMENT).ok()? {
        if let Some(Read::Ok(e)) = r.get(KIND_PROPOSAL, id).ok()?
            && let Some(LedgerRead::Segment(b)) = read_ledger(&e)
            && b.segment == Some(seg)
        {
            found.push((e, *b));
        }
    }
    let current = target.current();
    let mine = |b: &LedgerBody, s: LedgerStatus| current.owns(b) && b.status() == s;
    let out = match new.status() {
        LedgerStatus::Complete => {
            let rev = |id: Id| -> Option<u32> {
                match r.get(KIND_TURN, id).ok()? {
                    Some(Read::Ok(e)) => turn_rev_checked(&e),
                    _ => None,
                }
            };
            let versions = RuleVersions::current(CORRECTION_V1);
            let keep_previous = previous.and_then(|p| {
                found
                    .iter()
                    .filter(|(_, b)| {
                        !b.inputs.role_fallback
                            && b.variant == b2::VARIANT
                            && b.model_key == p
                            && matches!(b.status(), LedgerStatus::Complete | LedgerStatus::Partial)
                            && ledger_valid(b, rev, versions).is_valid()
                    })
                    .map(|(e, _)| e.id)
                    .max()
            });
            found
                .into_iter()
                .filter(|(e, _)| Some(e.id) != keep_previous)
                .map(|(e, _)| e)
                .collect()
        }
        LedgerStatus::Partial => {
            let n = elements(new);
            if found
                .iter()
                .any(|(_, b)| mine(b, LedgerStatus::Partial) && elements(b) > n)
            {
                return Some(Err(Saved::Kept));
            }
            found
                .into_iter()
                .filter(|(_, b)| mine(b, LedgerStatus::Partial) || mine(b, LedgerStatus::Failed))
                .map(|(e, _)| e)
                .collect()
        }
        _ => found
            .into_iter()
            .filter(|(_, b)| mine(b, LedgerStatus::Failed))
            .map(|(e, _)| e)
            .collect(),
    };
    Some(Ok(out))
}

/// 台帳を、単位を書く経路のロックの下で書く (04 §4・§5.2.1)。落とされない task で行う。
///
/// 1. `sentences` と `refs` のすべてのターンがあり、改訂の番号が読んだときと合うことを確かめ、合わなければ捨てる
///    (呼び出しの間に伏せ字にした・消したターンの台帳を書かない。掃除の位置の写し直しで番号を合わせる前に見る)。
/// 2. 効いているすべての忘れるの指示で掃除する (`forgets_seen` に関わらない、04 §7.5)。
/// 3. 置き換えて消す台帳と一緒に書く。書き手もターンの改訂の番号を確かめる。
pub(super) async fn save(
    inner: &Arc<Inner>,
    mut body: LedgerBody,
    first: Option<Id>,
    target: Target,
) -> Saved {
    let lock = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let w = worker.clone();
        let built = tokio::task::spawn_blocking(move || -> Result<evorag_store::WriteRequest, Saved> {
            let key = w.facts_key.key().ok_or(Saved::Discarded)?;
            let r = w.store.read().map_err(|_| Saved::Discarded)?;
            for s in body.sources() {
                let now = match r.get(KIND_TURN, s.turn) {
                    Ok(Some(Read::Ok(e))) => turn_rev_checked(&e),
                    _ => None,
                };
                if now != Some(s.rev) {
                    tracing::info!(
                        "a ledger was discarded (a turn was redacted or deleted while it was made)"
                    );
                    return Err(Saved::Discarded);
                }
            }
            let directives = load_directives(&r, w.language).ok_or(Saved::Discarded)?;
            let mut turns: BTreeSet<Id> = body.sources().into_iter().map(|s| s.turn).collect();
            turns.extend(body.inputs.corr.iter().map(|c| c.turn));
            let mut owned: Vec<(Id, u32, String)> = Vec::new();
            let mut missing: Vec<Id> = Vec::new();
            for t in turns {
                match r.get(KIND_TURN, t) {
                    Ok(Some(Read::Ok(e))) => {
                        match (turn_rev_checked(&e), e.body_as::<evorag_memory::TurnBody>()) {
                            (Some(rev), Ok(b)) => owned.push((t, rev, b.text)),
                            _ => missing.push(t),
                        }
                    }
                    _ => missing.push(t),
                }
            }
            let mut texts = TurnTexts::new();
            for (t, rev, text) in &owned {
                texts.insert(*t, *rev, text);
            }
            for t in &missing {
                texts.unreadable(*t);
            }
            let scrubbed = scrub_ledger(
                &mut body,
                &ScrubContext {
                    key,
                    directives: &directives,
                    first_turn: first,
                    texts: &texts,
                },
            )
            .map_err(|e| {
                tracing::warn!(error = %e, "a ledger could not be scrubbed");
                Saved::Discarded
            })?;
            if scrubbed.changed() {
                tracing::info!(
                    removed = scrubbed.removed(),
                    "a ledger was scrubbed by a forget added while it was made"
                );
            }
            let previous = previous_key(&r, &w);
            let replaces = replaced(&r, &body, &target, previous.as_deref()).ok_or(Saved::Discarded)??;
            let (_, mut req) = crate::writes(&w)
                .ledger(&body, None, &replaces)
                .map_err(|e| {
                    tracing::warn!(error = %e, "a ledger could not be written");
                    Saved::Discarded
                })?;
            // 消す台帳の区切りの鍵の埋め込みも、同じ依頼で消す (04 §5.5)。
            for e in &replaces {
                let ops = evorag_memory::ledger_embedding_deletes(&r, e.id).map_err(|_| Saved::Discarded)?;
                req.ops.extend(ops);
            }
            Ok(req)
        })
        .await
        .unwrap_or(Err(Saved::Discarded));
        let req = match built {
            Ok(req) => req,
            Err(saved) => return saved,
        };
        match crate::write_outcome(&worker, req).await {
            Ok(_) => Saved::Written,
            Err(e) => {
                // 読んでから書くまでに、ターンが変わった・消えた (伏せ字・削除)。古い本文から作った台帳を捨てる。
                tracing::info!(error = %e, "a ledger was discarded (its turns changed while it was made)");
                Saved::Discarded
            }
        }
    });
    task.await.unwrap_or(Saved::Discarded)
}

/// 直前の選択の model_key (06 §3.1 の履歴の前の model_key。`llm.memory` を設定したときは memory の、しないときは
/// chat の記録)。
pub(super) fn previous_key(r: &ReadTxn<'_>, inner: &Inner) -> Option<String> {
    let role = if inner.memory_configured() {
        evorag_memory::ROLE_MEMORY
    } else {
        evorag_memory::ROLE_CHAT
    };
    let bindings = evorag_memory::load_bindings(r).ok()?;
    let b = bindings.by_role.get(role)?;
    b.body.history.last().map(|o| o.model_key.clone())
}

/// 区切りを書く (閉じた区切りは `Immediate`、開いている末尾は裏の処理の結果として、03 §3.4)。呼ぶ側は、読んだ時から
/// 単位を書く経路のロックを持つ (読んでから書くまでに消したセッションの区切りを書き戻さないため)。書けなければ偽。
pub(super) async fn write_segments(
    inner: &Arc<Inner>,
    writes: Vec<(SegmentBody, Option<Envelope>)>,
) -> bool {
    for (body, previous) in writes {
        let req = match crate::writes(inner).segment_request(&body, previous.as_ref()) {
            Ok((_, req)) => req,
            Err(e) => {
                tracing::warn!(error = %e, "a segment could not be written");
                return false;
            }
        };
        if let Err(e) = crate::write_outcome(inner, req).await {
            // その間に区切りが変わった (前の本体の確かめに合わない)。次の回で決め直す。
            tracing::info!(error = %e, "a segment was not written (it changed meanwhile)");
            return false;
        }
    }
    true
}
