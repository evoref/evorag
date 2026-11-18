//! 忘れるの当て直し・実行・退避の写し直しと、忘れるの指示と合わないターン (04 §7.2・§7.7、03 §4.2 の規則 12・
//! §6・§7.3)。
//!
//! - 起動時 ([`startup`]): 書き手を作った後、照会と命令を受け付ける前に当て直す (`Core::start` は、これを終えて
//!   から `Core` を返すので、照会と命令はその後にしか届かない)。`meta` の `forget.recheck` (前回書いた版が 0.3.0
//!   より古いデータ根を開いた、restore の後、03 §4.2 の規則 12) が立っていれば、事実への指示の関係と副索引を
//!   作り直し、すべての忘れるの指示の `forget_backups` の job を書いて、同じ依頼で印を消す。
//! - 当て直し ([`recheck`]): 索引の鍵 → 事実を書く経路のロックの順に取り、DB の層 ([`replay_forgets`]) を当てて
//!   書き、消した ID を索引から除き、ジャーナルの層 ([`journal_cuts`]) を書き直す。書き直しの入れ替えに失敗したら
//!   (Windows でほかのプロセスが開いている)、sleep-time のほかの手順の前に試し直す (`Step::Forget`)。
//! - 実行 ([`execute`]): 計画をロックの下で作り直して書く (`/forget` の命令は、これを呼ぶ)。
//! - 退避 ([`run_backup_jobs`]): 残っている `forget_backups` の job をまとめて 1 回の写し直しにし、すべての退避を
//!   写し終えたら job を消す。
//! - 合わないターン ([`check_turn`]): 本文が忘れるの指示と合わないターンは、想起・候補の帯・裏の LLM の呼び出し・
//!   書き出しから除き、状態の行と `doctor` に示す。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use evorag_memory::{
    ChunkBody, CorrectionBody, Directives, EmbeddingBody, FactBody, FactDirectiveBody, ForgetAbort,
    ForgetEnv, ForgetOptions, ForgetPreview, JOB_FORGET_BACKUPS, JobBody, KIND_CHUNK,
    KIND_CORRECTION, KIND_EMBEDDING, KIND_FACT, KIND_FACT_DIRECTIVE, KIND_JOB, KIND_NOTE,
    KIND_TURN, NoteBody, OP_FORGET, PlanHash, TurnBody, TurnForgets, TurnRole, ValueKey,
    attributes, block_mark, hidden_marker, journal_cuts, load_directives, loose_key, plan_forget,
    redaction_marker, replay_forgets, turn_rev_checked,
};
use evorag_store::forget::{Redaction, TurnOutcome, apply_to_turn};
use evorag_store::key::OwnedPart;
use evorag_store::recopy::{ForgetRecopy, LookupProbe, RecopyView, TurnInCopy};
use evorag_store::{Envelope, Read, ReadTxn, WriteOp, WriteRequest, meta};
use evorag_types::Id;
use evorag_types::api::FactKey;

use crate::Inner;

/// 事実の副索引の項目: 値の鍵の 16 進 (`evorag-memory` の `fact` の副索引と同じ名前)。退避を飛ばすかの判定で、
/// 忘れた値の事実がありうるかを引く (03 §7.3 の「飛ばす退避」)。
const FACT_VK_LOOKUP: &str = "vk";

/// 忘れるの状態 (状態の行と sleep-time の手順)。
#[derive(Debug, Default)]
pub(crate) struct State {
    /// 忘れるの指示と合わないターン (04 §7.2 の DB の層)。
    mismatched: BTreeSet<Id>,
    /// ジャーナルの書き直しが済んでいない (04 §7.2、04 §7.8)。
    journal_pending: bool,
    /// sleep-time のほかの手順の前に、当て直しを確かめ直す。
    recheck: bool,
    /// 再び覚えない印の鍵が使えないので、鍵に頼る層 (印に当たる事実の取り消し、退避の写しの事実) を当てていない。
    key_missing: bool,
}

/// 忘れるの状態 (状態の行と、試験のため)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetStatus {
    /// 忘れるの指示と合わないターン (想起・候補の帯・裏の LLM の呼び出し・書き出しから除いている)。
    pub mismatched: Vec<Id>,
    /// ジャーナルに、伏せ字の前の行が残っている (書き直しの入れ替えに失敗した。sleep-time に試し直す)。
    pub journal_pending: bool,
    /// `memory.forget_key` が使えないので、忘れた値に当たる事実の取り消し (と退避の写しの事実) を当てていない
    /// (ほかの層は当てた、03 §3.1.1)。
    pub key_missing: bool,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.forget.lock().unwrap_or_else(|e| e.into_inner())
}

/// 今の状態。
pub(crate) fn status(inner: &Inner) -> ForgetStatus {
    let st = state(inner);
    ForgetStatus {
        mismatched: st.mismatched.iter().copied().collect(),
        journal_pending: st.journal_pending,
        key_missing: st.key_missing,
    }
}

/// sleep-time に当て直しを確かめ直すか。
pub(crate) fn needed(inner: &Inner) -> bool {
    state(inner).recheck
}

// ---------------------------------------------------------------------------------------------------------------
// 合わないターン
// ---------------------------------------------------------------------------------------------------------------

/// ターンの本文と、そのターンの忘れるの指示の突き合わせ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TurnCheck {
    /// 指示がすべて当たっている (または指示が無い)。
    Current,
    /// 当て直しで伏せ字にできる (当て直しが済んでいない)。
    Pending,
    /// 合わない (本文が記録と合わない・先の指示が無い・指示の記録が正しくない・読めない)。
    Mismatched,
}

/// ターン `turn` を、そのターンの忘れるの指示 (`forgets`) と突き合わせる。`marker` は伏せ字の印。
pub(crate) fn check_turn(forgets: &TurnForgets, turn: &Envelope, marker: &str) -> TurnCheck {
    let Some(steps) = forgets.redactions(turn.id) else {
        return TurnCheck::Mismatched;
    };
    if !steps.is_empty() {
        match apply_to_turn(turn, &steps, marker) {
            TurnOutcome::Unchanged => {}
            TurnOutcome::Changed { .. } => return TurnCheck::Pending,
            TurnOutcome::NeedsEarlier | TurnOutcome::Mismatch => return TurnCheck::Mismatched,
        }
    }
    let (Some(rev), Ok(body)) = (turn_rev_checked(turn), turn.body_as::<TurnBody>()) else {
        // `[非表示]` の範囲だけのターンで本体を読めないものは、どの経路も読まないので数えない。
        return if steps.is_empty() {
            TurnCheck::Current
        } else {
            TurnCheck::Mismatched
        };
    };
    match forgets.marks(turn.id, &body.text, rev, marker) {
        Some(_) => TurnCheck::Current,
        None => TurnCheck::Mismatched,
    }
}

/// 忘れるの指示と合わない (と、当て直しが済んでいない) ターン。
fn unsettled_turns(r: &ReadTxn<'_>, marker: &str) -> Result<BTreeSet<Id>, String> {
    let forgets = TurnForgets::load(r).map_err(|e| e.to_string())?;
    let mut out = BTreeSet::new();
    for t in forgets.turns() {
        match r.get(KIND_TURN, t).map_err(|e| e.to_string())? {
            Some(Read::Ok(e)) => {
                if check_turn(&forgets, &e, marker) != TurnCheck::Current {
                    out.insert(t);
                }
            }
            Some(_) if forgets.has_redactions(t) => {
                out.insert(t);
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------
// 起動時と sleep-time の当て直し
// ---------------------------------------------------------------------------------------------------------------

/// 起動時の当て直し (04 §7.2 の「起動の順」)。`Core::start` が、書き手を作った後、照会と命令を受け付ける前に
/// 呼ぶ。
pub(crate) async fn startup(inner: &Arc<Inner>) {
    recreate_backup_jobs(inner).await;
    let outcome = recheck(inner).await;
    if outcome.changed {
        tracing::info!("forgets were replayed at startup");
    }
    // sleep-time の前にも残りが無いかを確かめる (04 §7.2)。
    state(inner).recheck = true;
}

/// `meta` の `forget.recheck` が立っていれば (03 §4.2 の規則 12 の (1) と restore の後)、事実への指示の関係と
/// 副索引を作り直し、すべての忘れるの指示の `forget_backups` の job を書き、同じ依頼で印を消す。書けなければ
/// 印を残し、次の起動で続ける。
async fn recreate_backup_jobs(inner: &Arc<Inner>) {
    match inner.store.meta(meta::FORGET_RECHECK) {
        Ok(Some(v)) if v == meta::FORGET_RECHECK_SET => {}
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(error = %e, "the forget recheck flag could not be read");
            return;
        }
    }
    let store = inner.store.clone();
    let ids = inner.ids.clone();
    let now = inner.clock.now();
    let planned = tokio::task::spawn_blocking(move || -> Result<Vec<WriteOp>, String> {
        let r = store.read().map_err(|e| e.to_string())?;
        let mut ops = evorag_memory::rebuild_directive_indexes(&r).map_err(|e| e.to_string())?;
        ops.extend(
            evorag_memory::forget_backups_for_all(&r, &*ids, now).map_err(|e| e.to_string())?,
        );
        ops.push(WriteOp::ClearForgetRecheck);
        Ok(ops)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    match planned {
        Ok(ops) => match crate::write(inner, ops).await {
            Ok(()) => crate::flag_cleanup(inner),
            Err(e) => tracing::warn!(error = %e, "forget backup jobs could not be recreated"),
        },
        Err(e) => tracing::warn!(error = %e, "forget directives could not be read"),
    }
}

/// 当て直しの結果。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Outcome {
    /// DB の記録を変えた。
    pub(crate) changed: bool,
}

/// 忘れるの当て直しを確かめ、残りがあれば当てる (起動時と sleep-time)。何度当てても同じ。落とされない task で
/// 行い、その終わりを待つ。
pub(crate) async fn recheck(inner: &Arc<Inner>) -> Outcome {
    // 鍵の順は index_lock → facts_lock (04 §7.2 の「ロックの順」)。
    let index = inner.index_lock.clone().lock_owned().await;
    let facts = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    tokio::spawn(async move {
        let _guards = (index, facts);
        recheck_locked(&worker).await
    })
    .await
    .unwrap_or_default()
}

async fn recheck_locked(inner: &Arc<Inner>) -> Outcome {
    let mut out = Outcome::default();
    let mut failed = false;
    let lang = inner.language.as_str();
    let marker = redaction_marker(lang);
    // 再び覚えない印の鍵が使えなければ (03 §3.1.1)、鍵に頼る層 (印に当たる事実の取り消し) だけを飛ばし、ほかの層
    // (ターンの本文・span・消す派生の記録・ジャーナル) は当てる。印は鍵で作る HMAC なので、どの印にも当たらない
    // 置き換えの鍵 (すべて 0) で当て直すと、取り消しだけが起きない。
    let key = inner.facts_key.key().cloned();
    let key_missing = key.is_none();
    {
        let key = key.unwrap_or_else(|| evorag_memory::ForgetKey::from_bytes([0; 32]));
        let store = inner.store.clone();
        let ids = inner.ids.clone();
        let now = inner.clock.now();
        let planned = tokio::task::spawn_blocking(move || {
            let r = store.read().map_err(|e| e.to_string())?;
            let env = ForgetEnv {
                key: &key,
                attributes: attributes(),
                marker,
                hidden_marker: hidden_marker(lang),
                ids: &*ids,
                now,
            };
            let forgets = evorag_memory::latest_forget(&r)
                .map_err(|e| e.to_string())?
                .is_some();
            replay_forgets(&r, &env)
                .map(|replay| (replay, forgets))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        if key_missing && planned.as_ref().is_ok_and(|(_, forgets)| *forgets) {
            tracing::warn!(
                "memory.forget_key is not usable; facts matching forgotten values are not retracted"
            );
            state(inner).key_missing = true;
        }
        match planned.map(|(replay, _)| replay) {
            Ok(replay) if replay.changed() => {
                // 先に済ませる処理を、書く前に止める (04 §7.2 の「捨てるキャッシュ」)。
                drop(inner.pre.stop());
                // 伏せ字にするターンの `Patch` は、書き手が伏せ字の後の `put` の行を先にジャーナルに書く。
                match crate::write(inner, replay.ops).await {
                    Ok(()) => {
                        out.changed = true;
                        tracing::info!(
                            patched = replay.patched.len(),
                            retracted = replay.retracted.len(),
                            deleted = replay.deleted.len(),
                            "forgets replayed"
                        );
                        let gone: HashSet<Id> = replay
                            .deleted
                            .iter()
                            .chain(&replay.retracted)
                            .copied()
                            .collect();
                        after_change(inner, gone).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "replayed forgets could not be written");
                        failed = true;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "forgets could not be replayed");
                failed = true;
            }
        }
    }
    let journal_pending = rewrite_journal(inner).await;
    let store = inner.store.clone();
    let unsettled = tokio::task::spawn_blocking(move || {
        let r = store.read().map_err(|e| e.to_string())?;
        unsettled_turns(&r, marker)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let before = status(inner);
    let mut st = state(inner);
    match unsettled {
        Ok(set) => {
            if !set.is_empty() {
                tracing::warn!(
                    n = set.len(),
                    "turns do not match their forget directives; they are left out of recall, background calls and export"
                );
            }
            st.mismatched = set;
        }
        Err(e) => {
            tracing::warn!(error = %e, "turns could not be checked against forget directives");
            failed = true;
        }
    }
    st.journal_pending = journal_pending;
    st.recheck = failed || journal_pending;
    drop(st);
    if status(inner) != before {
        emit_state(inner);
    }
    out
}

/// 忘れるの状態を画面に知らせる (状態の行、04 §7.2・§7.8)。
pub(crate) fn emit_state(inner: &Inner) {
    let s = status(inner);
    crate::emit(
        inner,
        evorag_types::api::Event::ForgetState {
            state: evorag_types::api::ForgetState {
                mismatched: s.mismatched.len(),
                journal_pending: s.journal_pending,
                key_missing: s.key_missing,
            },
        },
    );
}

/// 記録を消した・書き換えた後: 知らせる前に、消した ID を今と前の世代のすべての索引から除き (除けなかった分は
/// `purge_indexes` の job から試し直す)、先に済ませた処理の結果と問いの埋め込みのキャッシュを捨て (04 §7.2 の
/// 「捨てるキャッシュ」)、事実の表とノートと抽出を見直させる。呼び手は `index_lock` を持ち、書く前に先に済ませる
/// 処理を止めてある。
async fn after_change(inner: &Arc<Inner>, gone: HashSet<Id>) {
    let worker = inner.clone();
    let _ = tokio::task::spawn_blocking(move || crate::remove_from_indexes(&worker, &gone)).await;
    crate::flag_cleanup(inner);
    inner.query_cache.clear();
    inner.rerank_cache.clear();
    crate::fact_table::invalidate(inner);
    crate::notes::touch(inner);
    crate::extraction::touch(inner);
}

/// ジャーナルの層 (04 §7.2): 伏せ字の後の行のあるターンの、それより前の行を除く書き直し。書き直しが済んで
/// いなければ (入れ替えに失敗した・求められない) `true`。
async fn rewrite_journal(inner: &Arc<Inner>) -> bool {
    let store = inner.store.clone();
    let cuts = tokio::task::spawn_blocking(move || {
        let r = store.read().ok()?;
        journal_cuts(&r, &|id| store.journal_rev_span(id)).ok()
    })
    .await
    .ok()
    .flatten();
    let Some(cuts) = cuts else {
        tracing::warn!("the journal rewrite for forgets could not be planned");
        return true;
    };
    if cuts.is_empty() {
        return false;
    }
    let req = WriteRequest::conversation(vec![WriteOp::RewriteJournal(cuts)]);
    match crate::write_outcome(inner, req).await {
        Ok(o) => rewrite_failed(o.journal_rewrite.as_ref()),
        Err(e) => {
            tracing::warn!(error = %e, "the journal could not be rewritten for forgets");
            true
        }
    }
}

/// ジャーナルの書き直しが失敗したか (知らせる)。
fn rewrite_failed(
    r: Option<&Result<evorag_store::RewriteReport, evorag_store::RewriteError>>,
) -> bool {
    match r {
        Some(Err(e)) => {
            tracing::warn!(
                error = %e,
                swap = e.is_swap(),
                "the journal still has lines from before a redaction; the rewrite is retried in sleep-time"
            );
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 実行
// ---------------------------------------------------------------------------------------------------------------

/// 忘れるを実行した結果。
#[derive(Debug, Clone)]
pub(crate) enum Executed {
    /// 書いた。
    Done {
        directive: Id,
        /// 伏せ字か `[非表示]` の範囲を足したターン (ID の順。開いているセッションにあれば、画面に読み直させる)。
        turns: Vec<Id>,
        /// ジャーナルの書き直しが済んでいない (sleep-time に試し直す)。
        journal_pending: bool,
    },
    /// 計画のハッシュが見本と違った。実行せず、新しい見本を返す (04 §7.1 の「見本と計画」)。
    Changed {
        preview: Box<ForgetPreview>,
        hash: PlanHash,
    },
    /// 操作の全体を中止した (04 §7.1 の「中止」)。何も書いていない。
    Aborted(ForgetAbort),
    /// 応答を生成している (04 §7.1 の「受け付けないとき」)。
    Busy,
    /// 記録を読めない・書けない、事実の機能を止めている。
    Failed(String),
}

/// 忘れるを実行する (04 §7.1〜§7.3)。索引の鍵 → 事実を書く経路のロックの順に取り、その下で計画を作り直し、
/// `expect` (見本の計画のハッシュの 16 進) があればハッシュを比べてから書く。書いた後に、知らせる前に、消した
/// ID を索引から除き、キャッシュを捨てる。退避の写し直しは、続けてした操作をまとめるため、少し待ってから始める
/// ([`schedule_backups`])。
pub(crate) async fn execute(
    inner: &Arc<Inner>,
    target: FactKey,
    options: ForgetOptions,
    expect: Option<String>,
) -> Executed {
    if crate::is_running(inner) {
        return Executed::Busy;
    }
    let Some(key) = inner.facts_key.key().cloned() else {
        return Executed::Failed("facts are disabled: memory.forget_key is not usable".into());
    };
    let index = inner.index_lock.clone().lock_owned().await;
    let facts = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    tokio::spawn(async move {
        let _guards = (index, facts);
        // ロックを待つ間に生成が始まっていたら受け付けない (04 §7.1 の「受け付けないとき」)。
        if crate::is_running(&worker) {
            return Executed::Busy;
        }
        // 先に済ませる処理を、ロックを取った直後に止める (書いた後に捨てるキャッシュに、書く前の結果を足させない)。
        drop(worker.pre.stop());
        execute_locked(&worker, key, target, options, expect).await
    })
    .await
    .unwrap_or_else(|e| Executed::Failed(e.to_string()))
}

async fn execute_locked(
    inner: &Arc<Inner>,
    key: evorag_memory::ForgetKey,
    target: FactKey,
    options: ForgetOptions,
    expect: Option<String>,
) -> Executed {
    let store = inner.store.clone();
    let ids = inner.ids.clone();
    let now = inner.clock.now();
    let lang = inner.language.as_str();
    let planned = tokio::task::spawn_blocking(move || {
        let r = store.read().map_err(|e| e.to_string())?;
        let env = ForgetEnv {
            key: &key,
            attributes: attributes(),
            marker: redaction_marker(lang),
            hidden_marker: hidden_marker(lang),
            ids: &*ids,
            now,
        };
        plan_forget(&r, &env, &target, options).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let plan = match planned {
        Ok(Ok(p)) => p,
        Ok(Err(abort)) => return Executed::Aborted(abort),
        Err(e) => return Executed::Failed(e),
    };
    if expect.is_some_and(|h| h != plan.hash.to_string()) {
        return Executed::Changed {
            preview: Box::new(plan.preview),
            hash: plan.hash,
        };
    }
    let outcome = match crate::write_outcome(inner, WriteRequest::conversation(plan.ops)).await {
        Ok(o) => o,
        Err(e) => return Executed::Failed(e),
    };
    let journal_pending = rewrite_failed(outcome.journal_rewrite.as_ref());
    let gone: HashSet<Id> = plan
        .derived
        .iter()
        .chain(&plan.retracted)
        .copied()
        .collect();
    after_change(inner, gone).await;
    {
        let mut st = state(inner);
        if journal_pending {
            st.journal_pending = true;
            st.recheck = true;
        }
    }
    schedule_backups(inner);
    let turns: BTreeSet<Id> = plan
        .redacted_turns
        .iter()
        .chain(&plan.hidden_turns)
        .copied()
        .collect();
    Executed::Done {
        directive: plan.directive,
        turns: turns.into_iter().collect(),
        journal_pending,
    }
}

/// 退避の写し直しを、コミットの直後に始める (03 §7.3)。写し直しの途中に足された job は、それが終わった後の次の
/// 回にまとめて写す (続けてした操作を 1 世代 1 回の写しにまとめる)。終了のときは、次の退避に移る前に止め、job
/// から次の起動で続ける。
pub(crate) fn schedule_backups(inner: &Arc<Inner>) {
    crate::flag_cleanup(inner);
    {
        let mut b = inner.forget_batch.lock().unwrap_or_else(|e| e.into_inner());
        if b.running {
            b.again = true;
            return;
        }
        b.running = true;
        b.again = false;
    }
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let stopping = |w: &Inner| w.shutting_down.load(std::sync::atomic::Ordering::SeqCst);
        loop {
            if !stopping(&worker) {
                crate::run_cleanup_jobs(&worker).await;
            }
            let mut b = worker
                .forget_batch
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if b.again && !stopping(&worker) {
                b.again = false;
                continue;
            }
            b.running = false;
            b.again = false;
            break;
        }
    });
    let mut bg = inner.background.lock().unwrap_or_else(|e| e.into_inner());
    bg.retain(|t| !t.is_finished());
    bg.push(task);
}

/// 忘れるの退避の写し直しの回 ([`schedule_backups`])。
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// 写し直しの回が動いている。
    running: bool,
    /// 動いている間に、次の回が要ることになった。
    again: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// 退避の写し直し
// ---------------------------------------------------------------------------------------------------------------

/// 残っている `forget_backups` の job。
fn backup_jobs(r: &ReadTxn<'_>) -> Result<Vec<(Id, JobBody)>, String> {
    let mut out = Vec::new();
    r.for_each(KIND_JOB, |x| {
        if let Read::Ok(e) = x
            && let Ok(b) = e.body_as::<JobBody>()
            && b.task == JOB_FORGET_BACKUPS
        {
            out.push((e.id, b));
        }
        true
    })
    .map_err(|e| e.to_string())?;
    Ok(out)
}

/// 残っている `forget_backups` の job の数。
pub(crate) fn pending_backup_jobs(r: &ReadTxn<'_>) -> usize {
    backup_jobs(r).map_or(0, |j| j.len())
}

/// 残っている `forget_backups` の job をまとめて 1 回の写し直しにし、すべての退避を写し終えたら job を消す
/// (03 §7.3)。呼び手は `cleanup_lock` を持つ。済んだら (job が無かったときも) `true`。
pub(crate) async fn run_backup_jobs(inner: &Arc<Inner>) -> bool {
    let worker = inner.clone();
    let result = tokio::task::spawn_blocking(move || recopy_backups(&worker))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    let done = match result {
        Ok(done) => done,
        Err(e) => {
            tracing::warn!(error = %e, "backups could not be rewritten for forgets; the job is kept");
            return false;
        }
    };
    if !done.jobs.is_empty() {
        let ops = done
            .jobs
            .iter()
            .map(|id| WriteOp::Delete {
                kind: KIND_JOB.into(),
                id: *id,
            })
            .collect();
        if crate::write_derived(inner, ops).await.is_err() {
            return false;
        }
    }
    done.complete
}

/// 写し直しの結果。
#[derive(Debug, Default)]
struct Recopied {
    /// 済んだ (消してよい) job。
    jobs: Vec<Id>,
    /// 残す job が無い。偽なら、job を残して試し直す (鍵が使えない・指示を読めない)。
    complete: bool,
}

/// 退避を写し直し、済んだ job を返す。
fn recopy_backups(inner: &Inner) -> Result<Recopied, String> {
    let rules = {
        let r = inner.store.read().map_err(|e| e.to_string())?;
        let jobs = backup_jobs(&r)?;
        if jobs.is_empty() {
            return Ok(Recopied {
                jobs: Vec::new(),
                complete: true,
            });
        }
        // 鍵が使えなければ、鍵の要らない判定 (伏せ字・ID・出典の改訂の番号) だけで写し、job は残す
        // (印に当たる事実は、鍵が使えるようになってから除く、03 §3.1.1)。
        RecopyRules::load(&r, inner.facts_key.key().cloned(), jobs)?
    };
    if !crate::enough_space(inner) {
        return Err("not enough free space to rewrite backups".into());
    }
    let marker = redaction_marker(inner.language.as_str());
    let drop = |e: &Envelope, v: &RecopyView<'_>| rules.drops(e, v);
    let plan = ForgetRecopy {
        remove: rules.remove.clone(),
        turns: rules.turns.clone(),
        drop_turns: rules.drop_turns.clone(),
        marker: marker.into(),
        candidates: rules.candidates.clone(),
        scan_from: rules.scan_from,
        drop: &drop,
    };
    let stop = || {
        inner
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
    };
    let count = inner
        .store
        .forget_backups(inner.backup_dir.as_deref(), &plan, &stop)
        .map_err(|e| e.to_string())?;
    tracing::info!(
        rewritten = count.rewritten,
        skipped = count.skipped,
        jobs = rules.jobs.len(),
        kept = rules.kept.len(),
        "backups rewritten for forgets"
    );
    if rules.key.is_none() {
        tracing::warn!(
            "memory.forget_key is not usable; the forget backup jobs are kept until facts can be matched"
        );
        return Ok(Recopied {
            jobs: Vec::new(),
            complete: false,
        });
    }
    Ok(Recopied {
        complete: rules.kept.is_empty(),
        jobs: rules.jobs,
    })
}

/// 伏せ字にしないで忘れた指示 1 つの、写しで除くノートと訂正の材料。
#[derive(Debug)]
struct Unredacted {
    directive: Id,
    /// 指示の `turns` と `[非表示]` の範囲のターン (その指示より前の訂正を除く)。
    turns: BTreeSet<Id>,
    /// `[非表示]` の範囲を持つ往復 (利用者の発話の ID。その指示を見ずに作ったノートを除く)。
    exchanges: BTreeSet<Id>,
}

/// 退避の写しで除く記録の判定 (03 §7.3 の「写し方」)。生きている DB から読んだ材料で、写しの中の記録を見て決める。
struct RecopyRules {
    /// まとめた job のうち、写し終えたら消せる job の ID。
    jobs: Vec<Id>,
    /// 指示を読めないので残す job (写しは、読める材料の分だけ行う)。
    kept: Vec<Id>,
    /// 再び覚えない印の鍵。無ければ、印に当たる事実を判定しない。
    key: Option<evorag_memory::ForgetKey>,
    remove: BTreeSet<Id>,
    turns: BTreeMap<Id, Vec<Redaction>>,
    /// 指示の記録が形として正しくない伏せ字のターン (写しから記録だけを除く)。
    drop_turns: BTreeSet<Id>,
    candidates: Vec<LookupProbe>,
    /// この ID 以上の記録を持つ退避は飛ばさない (副索引で引けない訂正・ノートと、照合の鍵だけで印に当たる事実を
    /// 除きうるため。値を忘れた指示の job では最小の ID にして、記録のある退避をすべて調べる)。
    scan_from: Option<Id>,
    dirs: Directives,
    /// すべての忘れるの指示の `vks`。
    vks: BTreeSet<ValueKey>,
    /// 伏せ字にするターンのセッション (出典の改訂の番号を持たない古いノートを除く)。
    sessions: BTreeSet<Id>,
    unredacted: Vec<Unredacted>,
}

impl RecopyRules {
    fn load(
        r: &ReadTxn<'_>,
        key: Option<evorag_memory::ForgetKey>,
        jobs: Vec<(Id, JobBody)>,
    ) -> Result<Self, String> {
        let mut bodies: Vec<(Id, FactDirectiveBody)> = Vec::new();
        r.for_each(KIND_FACT_DIRECTIVE, |x| {
            if let Read::Ok(e) = x
                && let Ok(b) = e.body_as::<FactDirectiveBody>()
                && b.op == OP_FORGET
            {
                bodies.push((e.id, b));
            }
            true
        })
        .map_err(|e| e.to_string())?;
        let forgets = TurnForgets::from_directives(bodies.clone());
        let readable: BTreeSet<Id> = bodies.iter().map(|(id, _)| *id).collect();
        // 指示を読めない job は消さずに残す (印・伏せ字の材料が足りないまま、済んだことにしない)。
        let (ok_jobs, kept): (Vec<_>, Vec<_>) = jobs
            .iter()
            .partition(|(_, j)| j.directive.is_none_or(|d| readable.contains(&d)));
        for (id, j) in &kept {
            tracing::warn!(
                job = %id,
                directive = ?j.directive,
                "the forget directive of a backup job cannot be read; the job is kept"
            );
        }
        let directives: BTreeSet<Id> = jobs.iter().filter_map(|(_, j)| j.directive).collect();
        let remove: BTreeSet<Id> = jobs.iter().flat_map(|(_, j)| j.ids.clone()).collect();
        let mut turns = BTreeMap::new();
        let mut drop_turns = BTreeSet::new();
        for (id, b) in &bodies {
            if !directives.contains(id) {
                continue;
            }
            for red in &b.redactions {
                // 生きている DB から読んだそのターンのすべての指示を、指示の ID の順に当てる。指示の記録が正しく
                // なければ、そのターンは写しから記録だけを除く (restore はジャーナルから伏せ字の後の行を足す)。
                match forgets.redactions(red.turn) {
                    Some(steps) => {
                        turns.insert(red.turn, steps);
                    }
                    None => {
                        drop_turns.insert(red.turn);
                    }
                }
            }
        }
        // 訂正とノートは副索引で引けないので、job の指示より後の記録を持つ退避は飛ばさない。
        let scan_from = directives.iter().copied().min();
        // 事実は照合の鍵 (04 §5.6) でも印に当たる (「7月22日」を忘れたときの「7 月 22 日」)。照合の鍵で当たる事実の
        // 値の鍵は `vks` に無く、副索引 (`candidates`) では引けないので、値を忘れた指示の job があれば、記録のある
        // 退避は飛ばさずにすべて調べる (安全の側。変えるものが無ければ書き直さない)。
        let forgets_values = bodies
            .iter()
            .any(|(id, b)| directives.contains(id) && !b.vks.is_empty());
        let scan_from = if forgets_values {
            Some(Id::from_bytes([0; 16]))
        } else {
            scan_from
        };
        let read_turn = |t: Id| -> Option<TurnBody> {
            match r.get(KIND_TURN, t).ok()?? {
                Read::Ok(e) => e.body_as::<TurnBody>().ok(),
                _ => None,
            }
        };
        let sessions = turns
            .keys()
            .chain(&drop_turns)
            .filter_map(|t| read_turn(*t).map(|b| b.session))
            .collect();
        let vks: BTreeSet<ValueKey> = bodies
            .iter()
            .flat_map(|(_, b)| b.vks.iter().filter_map(|h| ValueKey::from_hex(h)))
            .collect();
        let candidates = vks
            .iter()
            .map(|vk| LookupProbe {
                kind: KIND_FACT.into(),
                field: FACT_VK_LOOKUP.into(),
                value: OwnedPart::Str(vk.to_hex()),
            })
            .collect();
        let unredacted = bodies
            .iter()
            .filter(|(_, b)| !b.redact)
            .map(|(id, b)| Unredacted {
                directive: *id,
                turns: b
                    .turns
                    .iter()
                    .copied()
                    .chain(b.hidden.iter().map(|h| h.turn))
                    .collect(),
                exchanges: b
                    .hidden
                    .iter()
                    .map(|h| match read_turn(h.turn) {
                        Some(t) if t.role != TurnRole::User => t.reply_to.unwrap_or(h.turn),
                        _ => h.turn,
                    })
                    .collect(),
            })
            .collect();
        Ok(Self {
            jobs: ok_jobs.into_iter().map(|(id, _)| *id).collect(),
            kept: kept.into_iter().map(|(id, _)| *id).collect(),
            key,
            remove,
            turns,
            drop_turns,
            candidates,
            scan_from,
            dirs: load_directives(r).map_err(|e| e.to_string())?,
            vks,
            sessions,
            unredacted,
        })
    }

    /// 写しの記録 `e` を除くか (03 §7.3 の「写し方」)。
    fn drops(&self, e: &Envelope, view: &RecopyView<'_>) -> bool {
        let stale = |turn: Id, rev: u32| match view.turn(turn) {
            Some(TurnInCopy::Kept { rev: now }) => now != rev,
            Some(TurnInCopy::Dropped) => true,
            None => false,
        };
        match e.kind.as_str() {
            // 印 (印を解く指示の相手でない指示のもの) に当たる事実。取り消さずに消す (04 §7.5)。
            // 鍵が無ければ判定しない (job を残し、鍵が使えるようになってから除く)。
            KIND_FACT => {
                let Some(key) = &self.key else {
                    return false;
                };
                // 値の鍵の印と照合の鍵の印の両方で引く (当て直しの取り消しと同じ、04 §7.2)。
                e.body_as::<FactBody>().is_ok_and(|b| {
                    b.state != "retracted"
                        && b.vk
                            .as_deref()
                            .and_then(ValueKey::from_hex)
                            .into_iter()
                            .chain(b.value.as_deref().map(|v| loose_key(key, v)))
                            .any(|vk| {
                                self.dirs.is_blocked(&block_mark(
                                    key,
                                    &b.subject,
                                    &b.attribute,
                                    &vk,
                                ))
                            })
                })
            }
            // 伏せ字にしたターンの訂正、忘れた値の訂正、伏せ字にしないで忘れた指示より前の対象のターンの訂正。
            KIND_CORRECTION => e.body_as::<CorrectionBody>().is_ok_and(|b| {
                self.turns.contains_key(&b.turn)
                    || self.drop_turns.contains(&b.turn)
                    || match &self.key {
                        // 値の鍵と照合の鍵の両方で比べる (当て直しの訂正の消し方と同じ、04 §7.5)。
                        Some(key) => b.value_keys(key).iter().any(|v| self.vks.contains(v)),
                        None => [b.old_vk.as_deref(), b.new_vk.as_deref()]
                            .into_iter()
                            .flatten()
                            .filter_map(ValueKey::from_hex)
                            .any(|v| self.vks.contains(&v)),
                    }
                    || self
                        .unredacted
                        .iter()
                        .any(|u| u.turns.contains(&b.turn) && e.id < u.directive)
            }),
            // 出典の改訂の番号が写しのターンと合わないチャンク。
            KIND_CHUNK => e
                .body_as::<ChunkBody>()
                .is_ok_and(|b| stale(b.turn, b.src_rev())),
            // 除いたチャンク・ノートの埋め込み (前の回の判定で除いたものを見る)。
            KIND_EMBEDDING => e
                .body_as::<EmbeddingBody>()
                .is_ok_and(|b| view.is_dropped(b.target)),
            KIND_NOTE => e.body_as::<NoteBody>().is_ok_and(|b| {
                let recorded = |t: Id| b.src_revs.iter().find(|s| s.turn == t).map(|s| s.rev);
                b.src_revs.iter().any(|s| stale(s.turn, s.rev))
                    || b.exchanges
                        .iter()
                        .any(|t| recorded(*t).is_none() && stale(*t, 0))
                    // 出典の改訂の番号を持たない古いノートは、応答の番号を確かめられないので、伏せ字にするターンの
                    // セッションのものを除く (作り直せる)。
                    || (b.src_revs.is_empty() && self.sessions.contains(&b.session))
                    || self.unredacted.iter().any(|u| {
                        b.exchanges.iter().any(|x| u.exchanges.contains(x))
                            && b.forgets_seen.is_none_or(|s| s < u.directive)
                    })
            }),
            _ => false,
        }
    }
}
