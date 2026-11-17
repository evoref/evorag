//! `/forget` の命令 (04 §7.1〜§7.3・§7.8、07 §3)。
//!
//! - 見本 ([`preview`]): 対象と選択から、記録を変えずに計画を作り (読み取りの 1 回のトランザクションで)、見本と
//!   計画のハッシュを返す。ほかのターンを探すのに時間がかかるので、裏のスレッドで作り、命令の待ち行列を止めない。
//!   伏せ字にできないとき (04 §7.1 の「中止」) は、計画の代わりに警告を返す。
//! - 実行 ([`run`]): 見本の計画のハッシュを添えて [`forgets::execute`] に渡す。ロックの下で作り直した計画のハッシュ
//!   が違えば、実行せずに新しい見本を返す。書いたら、開いているセッションの伏せたターンを読み直して知らせ、新しい
//!   事実の一覧と忘れるの状態を知らせ、応答のモデルのすべてのスロットにキャッシュの消去を求める (04 §7.2)。
//! - `broken/` ([`discard_broken`]): 忘れるの後に、残っている壊れた DB のファイルの数を知らせ、利用者が選んだら
//!   消す (中身を読まないので伏せ字にはできない、04 §7.2)。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use evorag_llm::LlmError;
use evorag_memory::{
    ForgetAbort, ForgetEnv, KIND_TURN, Turn, TurnBody, attributes, hidden_marker, plan_forget,
    redaction_marker,
};
use evorag_store::{Read, ReadTxn};
use evorag_types::api::{
    Event, FactKey, FactList, ForgetOptions, ForgetPlanHash, ForgetPreview, ForgetSample,
    ForgetWarning, Role,
};
use evorag_types::{Id, Message, MessageKey};

use crate::{Inner, emit, fact_ops, forgets, masking, views};

/// 見本の 1 行で、変わる箇所の前と後に残す文字の数。
const SAMPLE_CONTEXT: usize = 24;
/// 見本の 1 行の長さの上限 (文字)。超えた分は「…」で省く。
const SAMPLE_MAX: usize = 120;
/// スロットのキャッシュの消去を 1 つ待つ長さ (応えない llama-server で終了を止めないため)。
const ERASE_WAIT: Duration = Duration::from_secs(5);
/// `/slots` で数えられないときに、`total_slots` から求めるスロットの数の上限。
const ERASE_MAX_SLOTS: u32 = 64;

/// 画面の選択を、計画の選択にする。
fn plan_options(o: ForgetOptions) -> evorag_memory::ForgetOptions {
    evorag_memory::ForgetOptions {
        redact: o.redact,
        history: o.history,
        echo: o.echo,
        others: o.others,
    }
}

/// 中止の理由を、見本の警告にする。行の参照の誤り (`RowChanged`・`NotAValue`) は警告ではない。
fn warning(a: &ForgetAbort) -> Option<ForgetWarning> {
    match a {
        ForgetAbort::UnreadableTurn(_) => Some(ForgetWarning::UnreadableTurn),
        ForgetAbort::StaleSource(_) => Some(ForgetWarning::StaleSource),
        ForgetAbort::ValueMismatch(_) => Some(ForgetWarning::ValueMismatch),
        ForgetAbort::MismatchedTurn(_) => Some(ForgetWarning::MismatchedTurn),
        ForgetAbort::RowChanged | ForgetAbort::NotAValue => None,
    }
}

/// 改行とタブを空白にする (見本は 1 行で出す)。
fn flat(chars: &[char]) -> String {
    chars
        .iter()
        .map(|c| if c.is_whitespace() { ' ' } else { *c })
        .collect()
}

/// 前と後の本文から、変わる箇所の周りを 1 行ずつ切り出す (04 §7.1 の「1 行ずつ見本として示す」)。
fn one_line(before: &str, after: &str) -> (String, String) {
    let b: Vec<char> = before.chars().collect();
    let a: Vec<char> = after.chars().collect();
    let prefix = b.iter().zip(&a).take_while(|(x, y)| x == y).count();
    let rest = b.len().min(a.len()) - prefix;
    let suffix = b
        .iter()
        .rev()
        .zip(a.iter().rev())
        .take(rest)
        .take_while(|(x, y)| x == y)
        .count();
    let start = prefix.saturating_sub(SAMPLE_CONTEXT);
    let cut = |text: &[char]| -> String {
        let end = (text.len() - suffix + SAMPLE_CONTEXT).min(text.len());
        let mut out = String::new();
        if start > 0 {
            out.push('…');
        }
        let body = &text[start..end];
        if body.len() > SAMPLE_MAX {
            out.push_str(&flat(&body[..SAMPLE_MAX]));
            out.push('…');
        } else {
            out.push_str(&flat(body));
            if end < text.len() {
                out.push('…');
            }
        }
        out
    };
    (cut(&b), cut(&a))
}

/// 計画の見本を、画面の見本にする。ターンの役割は記録から読む。
fn to_api(
    r: &ReadTxn<'_>,
    p: &evorag_memory::ForgetPreview,
    target: FactKey,
    options: ForgetOptions,
    plan: Result<ForgetPlanHash, ForgetWarning>,
) -> ForgetPreview {
    let role = |turn: Id| match r.get(KIND_TURN, turn) {
        Ok(Some(Read::Ok(e))) => e
            .body_as::<TurnBody>()
            .map_or(Role::User, |b| views::role(b.role)),
        _ => Role::User,
    };
    ForgetPreview {
        target,
        options,
        samples: p
            .samples
            .iter()
            .map(|s| {
                let (before, after) = one_line(&s.before, &s.after);
                ForgetSample {
                    turn: s.turn,
                    role: role(s.turn),
                    before,
                    after,
                }
            })
            .collect(),
        other_turns: p.other_turns,
        other_matches: p.other_matches,
        history: p.history,
        affected: p.affected.clone(),
        unredacted_sources: p.unredacted_sources.len(),
        source_outside: p.source_outside,
        whole_turns: p.whole_turns,
        mismatched_turns: p.mismatched_turns.len(),
        skipped_unreadable: p.skipped_unreadable,
        plan,
    }
}

/// 伏せ字にできない見本 (計画が無いので、件数は数えない)。
fn blocked(target: FactKey, options: ForgetOptions, w: ForgetWarning) -> ForgetPreview {
    ForgetPreview {
        target,
        options,
        samples: Vec::new(),
        other_turns: 0,
        other_matches: 0,
        history: 0,
        affected: Vec::new(),
        unredacted_sources: 0,
        source_outside: 0,
        whole_turns: 0,
        mismatched_turns: 0,
        skipped_unreadable: 0,
        plan: Err(w),
    }
}

/// 見本を作った結果。
enum Previewed {
    Ready(Box<ForgetPreview>),
    /// 指した行が無い (一覧が変わった)。
    Outdated,
    /// 指した行が値の行でない。
    NotAValue,
}

fn make_preview(
    inner: &Inner,
    key: &evorag_memory::ForgetKey,
    target: FactKey,
    options: ForgetOptions,
) -> Result<Previewed, String> {
    let lang = inner.language.as_str();
    let r = inner.store.read().map_err(|e| e.to_string())?;
    let env = ForgetEnv {
        key,
        attributes: attributes(),
        marker: redaction_marker(lang),
        hidden_marker: hidden_marker(lang),
        ids: &*inner.ids,
        now: inner.clock.now(),
    };
    let planned =
        plan_forget(&r, &env, &target, plan_options(options)).map_err(|e| e.to_string())?;
    Ok(match planned {
        Ok(plan) => {
            let hash = ForgetPlanHash::new(plan.hash.to_string());
            Previewed::Ready(Box::new(to_api(
                &r,
                &plan.preview,
                target,
                options,
                Ok(hash),
            )))
        }
        Err(ForgetAbort::RowChanged) => Previewed::Outdated,
        Err(ForgetAbort::NotAValue) => Previewed::NotAValue,
        Err(a) => {
            let w = warning(&a).unwrap_or(ForgetWarning::UnreadableTurn);
            Previewed::Ready(Box::new(blocked(target, options, w)))
        }
    })
}

fn rejected(inner: &Inner, error: Message, preview: Option<ForgetPreview>, list: Option<FactList>) {
    emit(
        inner,
        Event::ForgetRejected {
            error,
            preview,
            list,
        },
    );
}

fn not_saved(inner: &Inner, detail: String) {
    rejected(
        inner,
        Message::new(MessageKey::ForgetNotSaved).arg("detail", detail),
        None,
        None,
    );
}

/// 指した行が変わっていた: 同じ時点の新しい一覧を添えて断る (04 §7.1)。
fn outdated(inner: &Inner) {
    rejected(
        inner,
        Message::new(MessageKey::FactsOutdated),
        None,
        fact_ops::current_list(inner),
    );
}

/// 落とされない task で行う (終了の処理は `background` で待つ)。
fn spawn(inner: &Arc<Inner>, job: impl Future<Output = ()> + Send + 'static) {
    let task = tokio::spawn(job);
    let mut bg = inner.background.lock().unwrap_or_else(|e| e.into_inner());
    bg.retain(|t| !t.is_finished());
    bg.push(task);
}

/// `PreviewForget`: 忘れるの見本 (04 §7.1 の「見本と計画」)。記録は変えない。
pub(crate) fn preview(inner: &Arc<Inner>, target: FactKey, options: ForgetOptions) {
    let Some(key) = inner.facts_key.key().cloned() else {
        rejected(inner, Message::new(MessageKey::FactsDisabled), None, None);
        return;
    };
    let worker = inner.clone();
    spawn(inner, async move {
        let w = worker.clone();
        let made = tokio::task::spawn_blocking(move || make_preview(&w, &key, target, options))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match made {
            Ok(Previewed::Ready(preview)) => {
                emit(&worker, Event::ForgetPreviewInfo { preview: *preview })
            }
            Ok(Previewed::Outdated) => outdated(&worker),
            Ok(Previewed::NotAValue) => rejected(
                &worker,
                Message::new(MessageKey::FactsNotAllowed),
                None,
                None,
            ),
            Err(detail) => not_saved(&worker, detail),
        }
    });
}

/// `ForgetFact`: 忘れる (04 §7.1〜§7.3)。結果は `FactForgotten` か `ForgetRejected` で知らせる。
pub(crate) fn run(
    inner: &Arc<Inner>,
    target: FactKey,
    options: ForgetOptions,
    plan: ForgetPlanHash,
) {
    if inner.facts_key.key().is_none() {
        rejected(inner, Message::new(MessageKey::FactsDisabled), None, None);
        return;
    }
    let worker = inner.clone();
    spawn(inner, async move {
        let executed = forgets::execute(
            &worker,
            target.clone(),
            plan_options(options),
            Some(plan.as_str().to_owned()),
        )
        .await;
        match executed {
            forgets::Executed::Done {
                turns,
                journal_pending,
                ..
            } => done(&worker, &turns, journal_pending).await,
            forgets::Executed::Changed { preview, hash } => {
                // 見本の後に会話が増えた・裏の処理で事実が変わった: 新しい見本を出す (利用者が見ていない範囲を
                // 伏せないため)。
                let fresh = worker.store.read().ok().map(|r| {
                    to_api(
                        &r,
                        &preview,
                        target,
                        options,
                        Ok(ForgetPlanHash::new(hash.to_string())),
                    )
                });
                rejected(
                    &worker,
                    Message::new(MessageKey::ForgetOutdated),
                    fresh,
                    None,
                );
            }
            forgets::Executed::Aborted(ForgetAbort::RowChanged) => outdated(&worker),
            forgets::Executed::Aborted(ForgetAbort::NotAValue) => rejected(
                &worker,
                Message::new(MessageKey::FactsNotAllowed),
                None,
                None,
            ),
            forgets::Executed::Aborted(a) => {
                let w = warning(&a).unwrap_or(ForgetWarning::UnreadableTurn);
                rejected(
                    &worker,
                    Message::new(MessageKey::ForgetAborted),
                    Some(blocked(target, options, w)),
                    None,
                );
            }
            forgets::Executed::Busy => {
                rejected(&worker, Message::new(MessageKey::ForgetBusy), None, None);
            }
            forgets::Executed::Failed(detail) => not_saved(&worker, detail),
        }
    });
}

/// 書いた後: 開いているセッションの伏せたターンを読み直して知らせ、新しい一覧と忘れるの状態を知らせ、スロットの
/// キャッシュの消去を求める。
async fn done(inner: &Arc<Inner>, turns: &[Id], journal_pending: bool) {
    if let Some(session) = crate::current_session(inner) {
        let shown = reread(inner, session, turns);
        if !shown.is_empty() {
            emit(
                inner,
                Event::TurnsRedacted {
                    session,
                    turns: shown,
                },
            );
        }
    }
    match fact_ops::current_list(inner) {
        Some(list) => emit(
            inner,
            Event::FactForgotten {
                list,
                redacted: turns.len(),
                journal_pending,
                broken: broken_entries(inner).len(),
            },
        ),
        None => not_saved(inner, "the facts cannot be read after the forget".into()),
    }
    forgets::emit_state(inner);
    erase_slots(inner).await;
}

/// セッション `session` のターンのうち `turns` にあるものを、印の範囲を添えて読み直す (04 §7.3)。
fn reread(inner: &Inner, session: Id, turns: &[Id]) -> Vec<evorag_types::api::TurnView> {
    let Ok(r) = inner.store.read() else {
        return Vec::new();
    };
    let hide = masking::Hide::load(&r, inner.language);
    turns
        .iter()
        .filter_map(|&id| match r.get(KIND_TURN, id) {
            Ok(Some(Read::Ok(e))) => {
                let body = e.body_as::<TurnBody>().ok()?;
                (body.session == session)
                    .then(|| views::display(hide.as_ref(), &Turn { envelope: e, body }))
            }
            _ => None,
        })
        .collect()
}

/// ログに出すエラーの分類 (llama-server の応答の本文は出さない)。
fn error_kind(e: &LlmError) -> String {
    match e {
        LlmError::Http { status, .. } => format!("http {status}"),
        LlmError::Connect(_) => "connect".into(),
        LlmError::Cut(_) => "cut".into(),
        LlmError::Server(_) => "server".into(),
        LlmError::Protocol(_) => "protocol".into(),
        _ => "other".into(),
    }
}

/// 応答 (`chat`) の役割のモデルの、すべてのスロットにキャッシュの消去を求める (04 §7.2、06 §4)。構成を見分け
/// ない。断られたら (既定の構成の 501 など) 無視する。スロットは `/slots` で数え、数えられなければ `total_slots`
/// から求める。
pub(crate) async fn erase_slots(inner: &Inner) {
    let (client, server, _) = inner.conn();
    let ids: Vec<u32> = match client.slots().await {
        Ok(slots) if !slots.is_empty() => slots.iter().map(|s| s.id).collect(),
        _ => (0..server.total_slots.clamp(1, ERASE_MAX_SLOTS)).collect(),
    };
    let mut erased = 0usize;
    for id in &ids {
        match tokio::time::timeout(ERASE_WAIT, client.erase_slot(*id)).await {
            Ok(Ok(())) => erased += 1,
            Ok(Err(e)) => {
                tracing::debug!(slot = id, error = %error_kind(&e), "the slot cache was not erased");
            }
            Err(_) => tracing::debug!(slot = id, "the slot cache erase timed out"),
        }
    }
    tracing::info!(
        slots = ids.len(),
        erased,
        "asked llama-server to erase the slot caches"
    );
}

/// `broken/` に残っている壊れた DB のファイルの置き場所 (03 §8 の `broken/<日時>/`)。
fn broken_entries(inner: &Inner) -> Vec<PathBuf> {
    let dir = inner
        .store
        .data_root()
        .join(evorag_store::restore::BROKEN_DIR);
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// `DiscardBroken`: `broken/` のファイルを消す (利用者が選んだとき、04 §7.2)。
pub(crate) fn discard_broken(inner: &Arc<Inner>) {
    let mut removed = 0;
    let mut failed = 0;
    for p in broken_entries(inner) {
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match r {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(error = %e, "a broken database file could not be removed");
                failed += 1;
            }
        }
    }
    emit(inner, Event::BrokenDiscarded { removed, failed });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_is_one_line_around_the_change() {
        let (b, a) = one_line(
            "私は唐津に住んでいます。\n次の行",
            "私は[伏せ字]に住んでいます。\n次の行",
        );
        assert_eq!(b, "私は唐津に住んでいます。 次の行");
        assert_eq!(a, "私は[伏せ字]に住んでいます。 次の行");

        let long = format!("{}唐津{}", "あ".repeat(100), "い".repeat(100));
        let red = format!("{}[伏せ字]{}", "あ".repeat(100), "い".repeat(100));
        let (b, a) = one_line(&long, &red);
        let ctx = "あ".repeat(SAMPLE_CONTEXT);
        assert_eq!(b, format!("…{ctx}唐津{}…", "い".repeat(SAMPLE_CONTEXT)));
        assert_eq!(a, format!("…{ctx}[伏せ字]{}…", "い".repeat(SAMPLE_CONTEXT)));
        assert!(!b.contains('\n') && b.chars().count() <= SAMPLE_MAX + 2);
    }

    #[test]
    fn an_unchanged_text_still_gives_a_line() {
        let (b, a) = one_line("同じ", "同じ");
        assert_eq!((b.as_str(), a.as_str()), ("同じ", "同じ"));
    }
}
