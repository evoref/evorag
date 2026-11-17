//! 事実の抽出と統合の sleep-time の手順 (04 §4 の手順 3・5、04 §5.2・§5.2.1・§5.3)。
//!
//! - 対象: チャットのセッションの利用者の発話を、進み具合 (`job` の `extract_facts`) より後から、ターンの ID の
//!   順に処理する。前のふるいで送らないものは、理由ごとに数えて飛ばす。
//! - 束ね方: `/tokenize` の数 (キャッシュする、06 §8) で 1 回 800 トークンまでに詰める。
//! - 呼び出し: 型縛りの補助の呼び出し (temperature 0・固定の seed・裏のスロット、06 §6)。型が合わなければ
//!   1 回だけやり直し、それでも合わなければその束を飛ばして進み具合を先へ進める。
//! - 書き込み: 事実を書く経路のロック (`Inner::facts_lock`) を取ってから記録を読み、書き込みの入口
//!   (`evorag_memory::consolidate`) で畳み、事実の書き込みと進み具合を 1 つの依頼で書き終えるまで放さない。
//! - 利用者が操作したら、出している要求ごと落とす (sleep.rs)。書き込みは落とされない task で行うので、
//!   書いた束までは残り、次の機会に続きから処理する。

use std::collections::BTreeMap;
use std::sync::Arc;

use evorag_llm::{ChatMessage, ChatRequest, Generation, LlmClient, Role};
use evorag_memory::{
    Event as FactEvent, FactCtx, GateEnv, Report, Safety, Screened, Utterance, attributes,
    check_candidates, consolidate, extraction_input, extraction_schema, extraction_system,
    load_directives, load_extract_progress, pack, parse_extraction, prefilter,
};
use evorag_store::{Store, WriteOp};
use evorag_types::Id;
use evorag_types::api::Event;
use serde_json::json;

use crate::Inner;

/// 抽出の呼び出しの生成の上限 (トークン)。候補の JSON は短いが、束の発話が多いときに切れないように。
const MAX_OUTPUT_TOKENS: u32 = 1_024;

/// 抽出の手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話が変わるたびに増える。処理し終えた版と違えば、もう一度見る。
    version: u64,
    clean: Option<u64>,
    /// まだ知らせていない事実の変化 (落とされた回の分も、次に終えた回でまとめて知らせる)。
    unreported: Counts,
}

/// 知らせる件数 (04 §5.3 の「知らせ」)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    added: usize,
    replaced: usize,
    contested: usize,
}

impl Counts {
    fn add(&mut self, r: &Report) {
        self.added += r.added;
        self.replaced += r.replaced;
        self.contested += r.contested;
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.extraction.lock().unwrap_or_else(|e| e.into_inner())
}

/// 会話が変わった (ターンを書いた、セッションを消した)。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 要るか: 見ていない会話の変更があり、事実の機能が使え (03 §3.1.1)、型縛りの補助の呼び出しを使える
/// (06 §3.3 の点検で通った) とき (04 §5.2.1 の「止めるとき」)。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = state(inner);
        st.clean != Some(st.version)
    };
    due && inner.facts_key.key().is_some()
        && inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
        && crate::notes::usable(inner)
}

/// 進み具合より後の利用者の発話 1 つと、前のふるいの結果。
struct Item {
    turn: Id,
    rev: u32,
    text: String,
    screened: Screened,
}

/// 進み具合より後の利用者の発話を読み、前のふるいを当てる。
fn plan(store: &Store) -> Option<Vec<Item>> {
    let r = store.read().ok()?;
    let through = load_extract_progress(&r).ok()?.through;
    let items = evorag_memory::user_utterances_after(&r, through)
        .ok()?
        .into_iter()
        .map(|u| Item {
            screened: prefilter(&u.mode, &u.text),
            turn: u.turn,
            rev: u.rev,
            text: u.text,
        })
        .collect();
    Some(items)
}

enum CallError {
    /// 繋がらない・断られた (後で試し直す。進み具合は進めない)。
    Failed,
    /// 型に合わない答えが 2 回続いた (この束は飛ばす)。
    Invalid,
}

/// 抽出の要求 (06 §6: temperature 0・固定の seed・固定のスロット、`aux_request_extra`、`response_format`)。
fn request(inner: &Inner, slot: Option<u32>, texts: &[&str]) -> ChatRequest {
    let mut extra = inner.aux_request_extra.clone();
    extra.insert(
        "response_format".into(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "fact_candidates",
                "strict": true,
                "schema": extraction_schema(attributes(), texts.len())
            }
        }),
    );
    ChatRequest {
        messages: vec![
            ChatMessage::new(
                Role::System,
                extraction_system(inner.language.as_str(), attributes()),
            ),
            ChatMessage::new(Role::User, extraction_input(texts.iter().copied())),
        ],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(MAX_OUTPUT_TOKENS),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// 束を送り、型で読む。型に合わなければ 1 回だけやり直す (06 §6)。
async fn call(
    inner: &Inner,
    llm: &dyn LlmClient,
    slot: Option<u32>,
    texts: &[&str],
) -> Result<evorag_memory::ExtractionOutput, CallError> {
    let req = request(inner, slot, texts);
    for _ in 0..2 {
        let c = crate::probe::run_chat(llm, req.clone())
            .await
            .map_err(|_| CallError::Failed)?;
        if let Some(out) = parse_extraction(&c.text, texts.len()) {
            return Ok(out);
        }
    }
    Err(CallError::Invalid)
}

/// 束の候補に、属性の一覧の確認と関門 1〜8 を当て、通ったものを統合の出来事にする。捨てた理由は数える。
fn gate(
    inner: &Inner,
    items: &[&Item],
    out: evorag_memory::ExtractionOutput,
    dropped: &mut BTreeMap<&'static str, usize>,
) -> Vec<FactEvent> {
    let Some(key) = inner.facts_key.key() else {
        return Vec::new();
    };
    let directives = inner
        .store
        .read()
        .ok()
        .and_then(|r| load_directives(&r).ok())
        .unwrap_or_default();
    let safety = Safety::bundled();
    let env = GateEnv {
        attributes: attributes(),
        safety: &safety,
        key,
        directives: &directives,
    };
    let mut events = Vec::new();
    for (item, raws) in items.iter().zip(out.by_utterance) {
        let Screened::Send(filtered) = &item.screened else {
            continue;
        };
        let u = Utterance {
            turn: item.turn,
            rev: item.rev,
            text: &item.text,
            filtered,
        };
        for r in check_candidates(&env, &u, &raws) {
            match r {
                Ok(a) => events.push(FactEvent::Candidate(a.candidate)),
                Err(reason) => *dropped.entry(reason.as_str()).or_default() += 1,
            }
        }
    }
    if out.out_of_batch > 0 {
        *dropped.entry("out_of_batch").or_default() += out.out_of_batch;
    }
    events
}

/// 事実を書く経路のロックの下で、記録を読んで畳み、事実と進み具合を 1 つの依頼で書く (04 §5.2.1)。
/// 落とされない task で行う (読んだ後に落とされて、ロックの外で書きかけにならないように)。
async fn save(inner: &Arc<Inner>, events: Vec<FactEvent>, through: Id, model_key: String) -> bool {
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = worker.facts_lock.lock().await;
        let w = worker.clone();
        let built = tokio::task::spawn_blocking(move || -> Option<(Vec<WriteOp>, Report)> {
            let key = w.facts_key.key()?;
            let safety = Safety::bundled();
            let now = w.clock.now();
            let ctx = FactCtx {
                key,
                attributes: attributes(),
                safety: &safety,
                ids: &*w.ids,
                now,
            };
            let tx = w.store.read().ok()?;
            let (mut ops, report) = consolidate(&tx, &ctx, events)
                .map_err(|e| tracing::warn!(error = %e, "facts could not be consolidated"))
                .ok()?;
            let progress = load_extract_progress(&tx).ok()?;
            // 進み具合は戻さない。
            if progress.through.is_none_or(|t| t < through) {
                ops.push(progress.advance(through, &model_key, &*w.ids, now).ok()?);
            }
            Some((ops, report))
        })
        .await
        .ok()
        .flatten();
        let Some((ops, report)) = built else {
            return false;
        };
        if let Err(e) = crate::write_derived(&worker, ops).await {
            tracing::warn!(error = %e, "facts could not be written");
            return false;
        }
        tracing::info!(
            added = report.added,
            replaced = report.replaced,
            contested = report.contested,
            merged = report.merged,
            already = report.already,
            unknown_attribute = report.unknown_attribute,
            blocked = report.blocked,
            negation_dropped = report.negation_dropped,
            stale_source = report.stale_source,
            value_rejected = report.value_rejected,
            "facts consolidated"
        );
        state(&worker).unreported.add(&report);
        true
    });
    task.await.unwrap_or(false)
}

/// 手順を動かす。1 つでも束を書けたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = state(inner).version;
    let store = inner.store.clone();
    let Ok(Some(items)) = tokio::task::spawn_blocking(move || plan(&store)).await else {
        return false;
    };
    let (llm, server, _) = inner.conn();
    // 裏の処理は最後のスロット (06 §4)。
    let slot = (server.total_slots > 1).then(|| server.total_slots - 1);

    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut tokens = Vec::with_capacity(items.len());
    for it in &items {
        tokens.push(match &it.screened {
            Screened::Send(p) => Some(
                inner
                    .tokens
                    .count(&*llm, &server.model_key, p.text())
                    .await
                    .unwrap_or_else(|| crate::estimate(p.text())),
            ),
            Screened::Skip(reason) => {
                *skipped.entry(reason.as_str()).or_default() += 1;
                None
            }
        });
    }
    let packing = pack(&tokens);
    if !packing.too_long.is_empty() {
        skipped.insert("too_long", packing.too_long.len());
    }
    if !items.is_empty() {
        tracing::info!(
            utterances = items.len(),
            batches = packing.batches.len(),
            ?skipped,
            "fact extraction planned"
        );
    }

    let mut progressed = false;
    let mut dropped: BTreeMap<&'static str, usize> = BTreeMap::new();
    for batch in packing.batches {
        let through = items[batch.through].turn;
        let sent: Vec<&Item> = batch.send.iter().map(|&i| &items[i]).collect();
        let mut events = Vec::new();
        if !sent.is_empty() {
            if crate::notes::response_slot_busy(&*llm).await {
                return progressed;
            }
            let texts: Vec<&str> = sent
                .iter()
                .filter_map(|it| match &it.screened {
                    Screened::Send(p) => Some(p.text()),
                    Screened::Skip(_) => None,
                })
                .collect();
            match call(inner, &*llm, slot, &texts).await {
                Ok(out) => events = gate(inner, &sent, out, &mut dropped),
                Err(CallError::Invalid) => {
                    // 事実は派生物なので、止まり続けるより、この束を失って先へ進む (04 §5.2.1)。
                    tracing::warn!(
                        utterances = sent.len(),
                        "fact candidates did not match the schema; the batch is skipped"
                    );
                    *skipped.entry("invalid_output").or_default() += sent.len();
                }
                Err(CallError::Failed) => return progressed,
            }
        }
        if !save(inner, events, through, server.model_key.clone()).await {
            return progressed;
        }
        progressed = true;
    }
    if !dropped.is_empty() {
        tracing::info!(?dropped, "fact candidates dropped by the gates");
    }
    let changed = {
        let mut st = state(inner);
        st.clean = Some(version);
        std::mem::take(&mut st.unreported)
    };
    if !changed.is_empty() {
        crate::emit(
            inner,
            Event::FactsChanged {
                added: changed.added,
                replaced: changed.replaced,
                contested: changed.contested,
            },
        );
    }
    progressed
}
