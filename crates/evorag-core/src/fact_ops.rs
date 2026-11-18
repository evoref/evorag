//! 事実の一覧と事実への操作 (04 §7.1・§7.4〜§7.6、07 §3 の `/memory`・`/private`)。
//!
//! - 一覧はデータ根の全体のものなので、セッションを開いていなくても出す。出典のターンが 1 つも残っていない
//!   事実は出さない (04 §7.2 の「派生の記録に届かない経路」)。事実を作っていない・止めている理由を添える。
//! - 操作は、チャンクと索引の鍵 → 事実を書く経路のロックの順に取り、ロックの下で記録を読み直して行の参照を
//!   照合する (04 §7.1)。指した行が変わっていたら実行せず、同じ読み取りで作った新しい一覧を返す。前提が合わ
//!   なければ (private でない行への `/private off` など) 別の理由で断る。
//! - 指示は書き込みの入口 ([`evorag_memory::add_directives`]) で足して畳み直し、ジャーナルにも書く。コミットの
//!   直後に (知らせる前に)、private になった事実の検索の鍵を索引から除き (04 §5.5)、次のターンの前に事実の表を
//!   作り直させる (04 §5.4)。

use std::collections::BTreeSet;
use std::sync::Arc;

use evorag_memory::{
    DirectiveKind, Directives, FactCtx, FactSet, FactState, NewDirective, Safety, Scope,
    add_directives, attributes, block_mark, fact_history, fact_list, find_row, kept_after_delete,
    load_directives, load_forgets, rule_only, subject_name, subject_of,
};
use evorag_store::ReadTxn;
use evorag_types::api::{
    Event, FactGroupKey, FactKey, FactList, FactOp, FactRowRef, FactStatus, FactsUnavailable,
    ForgottenFact, ForgottenRef, PrivateScope,
};
use evorag_types::{Id, Message, MessageKey};

use crate::{Inner, emit};

/// 事実への操作の依頼。
pub(crate) enum Request {
    Private {
        target: FactKey,
        scope: PrivateScope,
    },
    PrivateOff(FactRowRef),
    Accept(FactKey),
    Revert(FactKey),
    Unblock(ForgottenRef),
}

impl Request {
    fn op(&self) -> FactOp {
        match self {
            Request::Private { .. } => FactOp::Private,
            Request::PrivateOff(_) => FactOp::PrivateOff,
            Request::Accept(_) => FactOp::Accept,
            Request::Revert(_) => FactOp::Revert,
            Request::Unblock(_) => FactOp::Unblock,
        }
    }
}

/// 事実を作っていない・止めている理由 (07 §3 の `/memory`)。
fn note(inner: &Inner) -> Option<FactsUnavailable> {
    if inner.facts_key.key().is_none() {
        return Some(FactsUnavailable::KeyUnavailable);
    }
    if !inner.sleep.enabled {
        return Some(FactsUnavailable::SleepTimeOff);
    }
    // 点検で型縛りを使えない・思考を切れないと分かった (点検がまだなら、止めてはいない)。
    let (_, server, _) = inner.conn();
    let probe = inner.store.read().ok().and_then(|r| {
        evorag_memory::load_probe(
            &r,
            evorag_memory::ROLE_CHAT,
            &server.model_key,
            &server.build_info,
        )
        .ok()
        .flatten()
    });
    match probe {
        Some((_, p)) if p.structured_ok == Some(false) || p.thinking_off == Some(false) => {
            Some(FactsUnavailable::StoppedByProbe)
        }
        _ => None,
    }
}

/// 一覧の材料 (出典の残っている事実、指示、削除で足した指示)。
struct Snapshot {
    set: FactSet,
    dirs: Directives,
    kept: BTreeSet<Id>,
}

fn snapshot(r: &ReadTxn<'_>) -> Option<Snapshot> {
    Some(Snapshot {
        set: crate::fact_table::surviving_facts(r)?,
        dirs: load_directives(r).ok()?,
        kept: kept_after_delete(r).ok()?,
    })
}

fn list_of(snap: &Snapshot, note: Option<FactsUnavailable>) -> FactList {
    let mut list = fact_list(&snap.set, &snap.dirs, attributes(), &snap.kept);
    list.note = note;
    list
}

/// 今の一覧を作る。記録を読めなければ `None`。
pub(crate) fn current_list(inner: &Inner) -> Option<FactList> {
    let r = inner.store.read().ok()?;
    Some(list_of(&snapshot(&r)?, note(inner)))
}

fn rejected(inner: &Inner, key: MessageKey, list: Option<FactList>) {
    emit(
        inner,
        Event::FactOpRejected {
            error: Message::new(key),
            list,
        },
    );
}

fn unreadable(inner: &Inner) {
    emit(
        inner,
        Event::FactOpRejected {
            error: Message::new(MessageKey::FactsNotSaved)
                .arg("detail", "the records cannot be read"),
            list: None,
        },
    );
}

/// `ShowFacts`: 事実の一覧 (04 §7.1)。
pub(crate) fn show_facts(inner: &Inner) {
    match current_list(inner) {
        Some(list) => emit(inner, Event::FactsInfo { list }),
        None => unreadable(inner),
    }
}

/// `ShowFactHistory`: (主語, 属性) の置き換えの履歴 (04 §7.4)。
pub(crate) fn show_history(inner: &Inner, group: &FactGroupKey) {
    let history = inner
        .store
        .read()
        .ok()
        .and_then(|r| crate::fact_table::surviving_facts(&r))
        .map(|set| fact_history(&set, attributes(), group));
    match history {
        Some(history) => emit(inner, Event::FactHistoryInfo { history }),
        None => unreadable(inner),
    }
}

/// 忘れた事実の一覧 (印を解いていないもの、忘れた順。値は持たない、04 §7.6)。
fn forgotten(inner: &Inner, r: &ReadTxn<'_>) -> Option<Vec<ForgottenFact>> {
    let forgets = load_forgets(r).ok()?;
    let mut out = Vec::new();
    for f in forgets.into_iter().filter(|f| !f.unblocked) {
        let mut titles: Vec<String> = Vec::new();
        let mut seen: BTreeSet<Id> = BTreeSet::new();
        let mut gone: BTreeSet<Id> = BTreeSet::new();
        for t in &f.turns {
            let session = match r.get(evorag_memory::KIND_TURN, *t) {
                Ok(Some(evorag_store::Read::Ok(e))) => e
                    .body_as::<evorag_memory::TurnBody>()
                    .ok()
                    .map(|b| b.session),
                _ => None,
            };
            match session {
                Some(s) if seen.insert(s) => {
                    titles.push(crate::recall::session_title(&inner.store, s).unwrap_or_default());
                }
                Some(_) => {}
                None => {
                    gone.insert(*t);
                }
            }
        }
        out.push(ForgottenFact {
            target: ForgottenRef { directive: f.id },
            subject: subject_of(&f.subject),
            attr: evorag_types::api::AttrInfo {
                multi: attributes()
                    .get(&f.attribute)
                    .is_some_and(|a| a.cardinality == evorag_memory::Cardinality::Multi),
                name: f.attribute,
            },
            at: f.id.timestamp(),
            values: f.marks,
            redacted: f.redact,
            sessions: titles,
            // 消えたターンからはセッションが分からないので、ターンの数で数える。
            deleted_turns: gone.len(),
        });
    }
    Some(out)
}

/// `ShowForgotten`: 忘れた事実の一覧 (04 §7.6)。
pub(crate) fn show_forgotten(inner: &Inner) {
    let items = inner.store.read().ok().and_then(|r| forgotten(inner, &r));
    match items {
        Some(items) => emit(inner, Event::ForgottenInfo { items }),
        None => unreadable(inner),
    }
}

/// 操作の計画 (ロックの下で記録を読んで決める)。
enum Plan {
    /// 指した行が変わっていた (新しい一覧を返す)。忘れた事実の一覧の行なら `None`。
    Outdated(Option<FactList>),
    /// 前提が合わない。
    NotAllowed,
    /// 戻す値が忘れた値。
    Blocked,
    /// 書く。
    Write(evorag_memory::DirectivesAdded),
}

/// 行の参照に合う事実 (主語・属性・値の鍵・ID)。
struct Hit {
    subject: String,
    attribute: String,
    vk: evorag_memory::ValueKey,
    id: Id,
    private: bool,
}

fn hit(snap: &Snapshot, key: &FactKey) -> Option<Hit> {
    let f = find_row(&snap.set, key)?;
    Some(Hit {
        subject: f.subject.clone(),
        attribute: f.attribute.clone(),
        vk: f.vk?,
        id: f.id,
        private: f.private,
    })
}

fn directive(
    kind: DirectiveKind,
    subject: &str,
    attribute: &str,
    vk: Option<evorag_memory::ValueKey>,
    fact: Option<Id>,
) -> NewDirective {
    NewDirective {
        kind,
        subject: subject.to_owned(),
        attribute: attribute.to_owned(),
        vk,
        fact,
    }
}

/// 依頼を照合して、足す指示を決める。
fn plan(
    r: &ReadTxn<'_>,
    ctx: &FactCtx<'_>,
    req: &Request,
    note: Option<FactsUnavailable>,
) -> Result<Plan, String> {
    let snap = snapshot(r).ok_or("the facts cannot be read")?;
    let outdated = || Plan::Outdated(Some(list_of(&snap, note)));
    let list = match req {
        Request::Private { target, scope } => {
            let Some(h) = hit(&snap, target) else {
                return Ok(outdated());
            };
            if target.status == FactStatus::Superseded || h.private {
                return Ok(Plan::NotAllowed);
            }
            let scope = match scope {
                PrivateScope::Successors => Scope::Successors,
                PrivateScope::ValueOnly => Scope::ValueOnly,
            };
            vec![directive(
                DirectiveKind::Private { scope },
                &h.subject,
                &h.attribute,
                Some(h.vk),
                Some(h.id),
            )]
        }
        Request::PrivateOff(FactRowRef::Value(target)) => {
            let Some(f) = find_row(&snap.set, target) else {
                return Ok(outdated());
            };
            // その行に効いているすべての private の指示を解く (04 §7.5 の「private の解除」)。
            let cover = snap.dirs.private_cover(f);
            if cover.is_empty() {
                return Ok(Plan::NotAllowed);
            }
            cover
                .into_iter()
                .map(|(d, _)| {
                    directive(
                        DirectiveKind::PrivateOff { target: d.id },
                        &d.subject,
                        &d.attribute,
                        None,
                        Some(f.id),
                    )
                })
                .collect()
        }
        Request::PrivateOff(FactRowRef::RuleOnly(group)) => {
            let Some(ids) = rule_only(&snap.set, &snap.dirs, group) else {
                return Ok(outdated());
            };
            let subject = subject_name(&group.subject);
            ids.into_iter()
                .map(|id| {
                    directive(
                        DirectiveKind::PrivateOff { target: id },
                        subject,
                        &group.attribute,
                        None,
                        None,
                    )
                })
                .collect()
        }
        Request::Accept(target) => {
            let Some(h) = hit(&snap, target) else {
                return Ok(outdated());
            };
            if target.status != FactStatus::Contested {
                return Ok(Plan::NotAllowed);
            }
            vec![directive(
                DirectiveKind::Accept,
                &h.subject,
                &h.attribute,
                Some(h.vk),
                Some(h.id),
            )]
        }
        Request::Revert(target) => {
            let Some(h) = hit(&snap, target) else {
                return Ok(outdated());
            };
            if target.status != FactStatus::Superseded {
                return Ok(Plan::NotAllowed);
            }
            // 忘れた値は戻さない (04 §7.5 の「再び覚えない印と訂正・戻す」)。
            if snap
                .dirs
                .is_blocked(&block_mark(ctx.key, &h.subject, &h.attribute, &h.vk))
            {
                return Ok(Plan::Blocked);
            }
            // 同じ値がすでに今の値なら、戻すものが無い。
            let current = snap.set.facts.iter().any(|f| {
                f.subject == h.subject
                    && f.attribute == h.attribute
                    && f.vk == Some(h.vk)
                    && f.state == FactState::Live
            });
            if current {
                return Ok(Plan::NotAllowed);
            }
            vec![directive(
                DirectiveKind::Revert,
                &h.subject,
                &h.attribute,
                Some(h.vk),
                Some(h.id),
            )]
        }
        Request::Unblock(target) => {
            let forgets = load_forgets(r).map_err(|e| e.to_string())?;
            let Some(f) = forgets.iter().find(|f| f.id == target.directive) else {
                return Ok(Plan::Outdated(None));
            };
            if f.unblocked {
                return Ok(Plan::NotAllowed);
            }
            vec![directive(
                DirectiveKind::Unblock { target: f.id },
                &f.subject,
                &f.attribute,
                None,
                None,
            )]
        }
    };
    add_directives(r, ctx, list)
        .map(Plan::Write)
        .map_err(|e| e.to_string())
}

/// 事実への操作をする (04 §7.4〜§7.6)。結果は `FactsUpdated` か `FactOpRejected` で知らせる。
///
/// 操作は落とされない task で行い、命令の待ち行列を鍵の待ちで止めない。終了の処理は、この task を
/// `background` で待つ (書いてから知らせるまでを途中で止めない)。
pub(crate) fn run(inner: &Arc<Inner>, req: Request) {
    if inner.facts_key.key().is_none() {
        rejected(inner, MessageKey::FactsDisabled, None);
        return;
    }
    let worker = inner.clone();
    let task = tokio::spawn(async move { execute(&worker, req).await });
    inner
        .background
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(task);
}

async fn execute(inner: &Arc<Inner>, req: Request) {
    let op = req.op();
    // 鍵の順は index_lock → facts_lock (セッションの削除と同じ)。読んでから書き、索引から除くまで持つ。
    let _index = inner.index_lock.clone().lock_owned().await;
    let _facts = inner.facts_lock.clone().lock_owned().await;
    let note = note(inner);
    let worker = inner.clone();
    let planned = tokio::task::spawn_blocking(move || -> Result<Plan, String> {
        let key = worker
            .facts_key
            .key()
            .ok_or("the forget key is unavailable")?;
        let safety = Safety::bundled();
        let ctx = FactCtx {
            key,
            attributes: attributes(),
            safety: &safety,
            ids: &*worker.ids,
            now: worker.clock.now(),
        };
        let r = worker.store.read().map_err(|e| e.to_string())?;
        plan(&r, &ctx, &req, note)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|p| p);
    let added = match planned {
        Ok(Plan::Write(added)) => added,
        Ok(Plan::Outdated(list)) => {
            rejected(inner, MessageKey::FactsOutdated, list);
            if op == FactOp::Unblock {
                show_forgotten(inner);
            }
            return;
        }
        Ok(Plan::NotAllowed) => {
            rejected(inner, MessageKey::FactsNotAllowed, None);
            return;
        }
        Ok(Plan::Blocked) => {
            rejected(inner, MessageKey::FactsBlocked, None);
            return;
        }
        Err(detail) => {
            emit(
                inner,
                Event::FactOpRejected {
                    error: Message::new(MessageKey::FactsNotSaved).arg("detail", detail),
                    list: None,
                },
            );
            return;
        }
    };
    if let Err(detail) = crate::write(inner, added.ops).await {
        emit(
            inner,
            Event::FactOpRejected {
                error: Message::new(MessageKey::FactsNotSaved).arg("detail", detail),
                list: None,
            },
        );
        return;
    }
    // 印を解くは事実を変えない (次の抽出から、その値を覚えられるだけ)。
    let slow_next_turn = op != FactOp::Unblock;
    if slow_next_turn {
        // コミットの直後に、次のターンの前に事実の表を必ず作り直させる (04 §5.4。この間に始まる応答に古い表を
        // 送らないため)。鍵の残りは次の sleep-time の手順で索引に合わせる。
        crate::fact_table::invalidate(inner);
    }
    // 続けて、知らせる前に、private になった事実の検索の鍵を索引から除く (04 §5.5)。除けなくても、
    // 想起は当たりの事実を読み直して private の鍵を使わない (recall.rs の `hit_turns`)。鍵の残りは次の
    // sleep-time の手順で索引に合わせる。
    if !added.hidden.is_empty()
        && let Some(ix) = inner.index.clone()
    {
        let gone: std::collections::HashSet<Id> = added.hidden.iter().copied().collect();
        let removed = tokio::task::spawn_blocking(move || ix.remove(&gone)).await;
        if !matches!(removed, Ok(Ok(()))) {
            tracing::warn!(
                "keys of facts made private could not be removed from the lexical index"
            );
        }
    }
    match current_list(inner) {
        Some(list) => emit(
            inner,
            Event::FactsUpdated {
                op,
                list,
                slow_next_turn,
            },
        ),
        None => unreadable(inner),
    }
}
