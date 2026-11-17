//! 訂正の候補の検証の sleep-time の手順 (04 §4 の手順 4、04 §6)。
//!
//! - 対象: `checked` でない訂正の候補を、ID の順に 1 件ずつ。候補は抽出の手順 (extraction.rs) が、抽出と同じ発話の
//!   列と進み具合で見つけて書く。手順の並び (sleep.rs) で抽出の後に置くので、抽出の呼び出しをすべて終えてから流す。
//! - 呼び出し: 抽出と同じ設定 (型縛り・temperature 0・固定の seed・裏のスロット、06 §6)。渡すのは、候補の発話の型に
//!   当たった位置を含む 800 文字と、直前の応答の末尾の 800 文字 (信頼しない引用の枠)。要求の全体を `/tokenize` で
//!   数えて抽出と同じ予算に収め、超えれば直前の応答を半分に切って数え直し、2 回で収まらなければ飛ばす。
//! - 失敗の扱いは抽出と同じ (04 §5.2.1): 型が合わなければ 1 回だけやり直し、合わなければ飛ばす。4xx は飛ばす。
//!   締切切れと 5xx は次の機会に送り直し、同じ候補で 3 回続いたら飛ばす。繋がらないときは数えずに待つ。飛ばした
//!   候補も `checked` にして理由を残し、検証し直さない。
//! - 結果は、事実を書く経路のロックの下で、改訂の番号を読み直してから書く (変わっていれば `stale_source`)。関門
//!   (`evorag_memory::verify`) を通った訂正は、同じ依頼で書き込みの入口 (`evorag_memory::consolidate`) に渡す。

use std::collections::BTreeMap;
use std::sync::Arc;

use evorag_llm::{ChatMessage, ChatRequest, Generation, LlmClient, Role};
use evorag_memory::{
    CorrectionCandidate, Counting, Event as FactEvent, FactCtx, FactState, KIND_TURN, Prefiltered,
    Report, Safety, Tense, TurnBody, TurnRole, Unverifiable, Utterance, VERIFY_EXCERPT_CHARS,
    VERIFY_MAX_TOKENS, Verdict, VerifyContext, VerifyOutput, attributes, checked_op, consolidate,
    load_corrections, load_facts, parse_verify, session_turns, tail_excerpt, turn_rev,
    utterance_budget, utterance_excerpt, verify, verify_input, verify_schema, verify_system,
};
use evorag_store::{Read, Store, WriteOp};
use evorag_types::Id;
use serde_json::json;

use crate::Inner;
use crate::extraction::{Counts, FAILURE_LIMIT, Failure, count_tokens};

/// 要求が予算に収まらないとき、直前の応答を半分に切って数え直す回数 (04 §6 の「検証の時機」)。
const HALVINGS: usize = 2;

/// 検証の手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 候補が書かれるたびに増える。見終えた版と違えば、もう一度見る (起動したときは一度見る)。
    version: u64,
    clean: Option<u64>,
    /// まだ知らせていない事実の変化。
    unreported: Counts,
    /// 締切切れと 5xx が続いている候補と、続いた回数。
    failures: Option<(Id, u32)>,
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.correction.lock().unwrap_or_else(|e| e.into_inner())
}

/// 検証していない候補が書かれた。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 要るか: 見ていない候補があるかもしれず、抽出と同じ条件 (事実の機能・型縛り・スロットの文脈) を満たすとき。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = state(inner);
        st.clean != Some(st.version)
    };
    due && inner.facts_key.key().is_some()
        && inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
        && crate::notes::usable(inner)
        && crate::extraction::fits(inner)
}

/// 1 回の手順の件数 (手順の終わりに、途中で止めたときも記録する)。
#[derive(Debug, Default)]
struct Tally {
    /// 検証していなかった候補。
    candidates: usize,
    /// 送った要求 (やり直しを含む)。
    requests: usize,
    /// 型に合わなかった答え。
    invalid_outputs: usize,
    /// 失敗した要求。
    failed: usize,
    /// 検証済みにした候補。
    verified: usize,
    /// 関門で落ちた・検証を終えられなかった候補 (理由ごと)。
    rejected: BTreeMap<&'static str, usize>,
    /// 検証済みの訂正で、事実を置き換えた。
    replaced: usize,
    /// 検証済みの訂正で、古い値が今の値と一致せず、事実を変えなかった。
    no_effect: usize,
    /// 検証済みの訂正で、古い値が 2 つ以上の (主語, 属性) の今の値と一致し、事実を変えなかった。
    ambiguous: usize,
    /// 検証済みの訂正で、古い値の穴の無い型なので、事実を変えなかった (学習にだけ使う)。
    unanchored: usize,
    /// 書き込みの入口で捨てた (出典の改訂の番号が合わない・新しい値が関門 6・7 に外れた・再び覚えない印)。
    dropped: usize,
}

impl Tally {
    fn count(&mut self, v: &Verdict) {
        match v.reason() {
            None => self.verified += 1,
            Some(r) => *self.rejected.entry(r).or_default() += 1,
        }
    }

    fn add(&mut self, r: &Report) {
        self.replaced += r.replaced;
        self.no_effect += r.correction_no_effect;
        self.ambiguous += r.correction_ambiguous;
        self.unanchored += r.correction_unanchored;
        self.dropped += r.stale_source + r.value_rejected + r.blocked;
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        if self.candidates == 0 {
            return;
        }
        tracing::info!(
            candidates = self.candidates,
            requests = self.requests,
            verified = self.verified,
            rejected = ?self.rejected,
            invalid_outputs = self.invalid_outputs,
            failed = self.failed,
            replaced = self.replaced,
            no_effect = self.no_effect,
            ambiguous = self.ambiguous,
            unanchored = self.unanchored,
            dropped = self.dropped,
            "correction verification pass"
        );
    }
}

/// 検証に渡す材料 (候補の発話・直前の応答・それより前の発話・今の値)。
struct Material {
    text: String,
    /// 直前の応答の保存した本文 (検証の関門が照合に使う)。
    previous: Option<String>,
    /// 直前の応答に `[非表示]` の置き換えを当てたもの (検証のプロンプトにだけ入れる、04 §6・§7.5)。
    previous_prompt: Option<String>,
    earlier: Vec<String>,
    live: Vec<String>,
}

enum Prepared {
    Ready(Material),
    /// 候補の発話が無い・読めない・改訂の番号が変わった。
    Stale,
}

/// 直前の応答 `previous` に `[非表示]` の置き換えを当てた本文 (検証のプロンプトに入れる形、04 §6・§7.5)。
/// 直前の応答が無ければ `Some(None)`。記録を読めない・伏せる値を確かめられなければ `None` (送らない)。
fn masked_previous(
    store: &Store,
    previous: Option<Id>,
    lang: crate::prompt::PromptLanguage,
) -> Option<Option<String>> {
    let Some(id) = previous else {
        return Some(None);
    };
    let r = store.read().ok()?;
    let e = match r.get(KIND_TURN, id).ok()? {
        Some(Read::Ok(e)) => e,
        _ => return Some(None),
    };
    let Ok(body) = e.body_as::<TurnBody>() else {
        return Some(None);
    };
    let hide = crate::masking::Hide::load(&r, lang)?;
    let t = evorag_memory::Turn { envelope: e, body };
    Some(Some(hide.text(&r, &t)))
}

/// 候補の材料を読む。記録を読めなければ `None`。
fn prepare(
    store: &Store,
    c: &CorrectionCandidate,
    lang: crate::prompt::PromptLanguage,
) -> Option<Prepared> {
    let r = store.read().ok()?;
    let turn = match r.get(KIND_TURN, c.turn).ok()? {
        Some(Read::Ok(e)) => e,
        _ => return Some(Prepared::Stale),
    };
    let Ok(body) = turn.body_as::<TurnBody>() else {
        return Some(Prepared::Stale);
    };
    if turn_rev(&turn) != c.rev {
        return Some(Prepared::Stale);
    }
    let mut previous = None;
    let mut previous_prompt = None;
    let mut earlier = Vec::new();
    for t in session_turns(&r, body.session).ok()? {
        if Some(t.envelope.id) == c.previous {
            previous = Some(t.body.text.clone());
            // 伏せる値を確かめられなければ、この回は検証しない (伏せない本文を送らない)。
            previous_prompt = Some(crate::masking::Hide::load(&r, lang)?.text(&r, &t));
        }
        // 同じセッションの、候補の発話より前の利用者の発話 (前のふるいの後の本文、04 §6 の「検証の時機」)。
        if t.envelope.id < c.turn && t.body.role == TurnRole::User {
            earlier.push(Prefiltered::from_stored(&t.body.text).text().to_owned());
        }
    }
    // 今の値は、時制 `current` の `Live` に限る (04 §6 の「事実への効き方」)。
    let live = load_facts(&r)
        .ok()?
        .set
        .facts
        .into_iter()
        .filter(|f| f.state == FactState::Live && f.tense == Tense::Current)
        .filter_map(|f| f.value)
        .collect();
    Some(Prepared::Ready(Material {
        text: body.text,
        previous,
        previous_prompt,
        earlier,
        live,
    }))
}

/// 1 回の呼び出しの結果。
enum Reply {
    Parsed(VerifyOutput),
    Truncated,
    Invalid,
    Failed(Failure),
}

/// 検証の要求 (06 §6: temperature 0・固定の seed・固定のスロット、`aux_request_extra`、`response_format`)。
fn request(inner: &Inner, slot: Option<u32>, system: &str, input: &str) -> ChatRequest {
    let mut extra = inner.aux_request_extra.clone();
    extra.insert(
        "response_format".into(),
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "correction",
                "strict": true,
                "schema": verify_schema()
            }
        }),
    );
    ChatRequest {
        messages: vec![
            ChatMessage::new(Role::System, system),
            ChatMessage::new(Role::User, input),
        ],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(VERIFY_MAX_TOKENS),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// 送って型で読む。型に合わなければ 1 回だけやり直す (06 §6)。切れた答えはやり直さない。
async fn call(
    inner: &Inner,
    llm: &dyn LlmClient,
    slot: Option<u32>,
    system: &str,
    input: &str,
    tally: &mut Tally,
) -> Reply {
    let req = request(inner, slot, system, input);
    for _ in 0..2 {
        tally.requests += 1;
        let c = match crate::probe::run_chat(llm, req.clone()).await {
            Ok(c) => c,
            Err(e) => {
                let failure = Failure::of(&e);
                tracing::warn!(error = %e, ?failure, "correction verification request failed");
                tally.failed += 1;
                return Reply::Failed(failure);
            }
        };
        if c.finish_reason.as_deref() == Some("length") {
            return Reply::Truncated;
        }
        if let Some(out) = parse_verify(&c.text) {
            return Reply::Parsed(out);
        }
        tally.invalid_outputs += 1;
    }
    Reply::Invalid
}

/// 締切切れか 5xx を、候補について数える。続いた回数を返す。
fn count_failure(inner: &Inner, id: Id) -> u32 {
    let mut st = state(inner);
    let n = match st.failures {
        Some((x, n)) if x == id => n + 1,
        _ => 1,
    };
    st.failures = Some((id, n));
    n
}

/// 検証の利用者のメッセージを、予算 (2,048 − システムのメッセージ − 64。見積もりなら 2 割減らす、04 §5.2.1) に
/// 収める。直前の応答は末尾の 800 文字から始め、収まらなければ半分ずつ 2 回まで切る。収まらなければ `None`。
///
/// 直前の応答には、想起で添えるときと同じ `[非表示]` の置き換え (04 §7.5) を当てたもの (`Material::previous_prompt`)
/// を渡す (04 §6 の「検証の時機」)。検証の関門は、置き換えの前の保存した本文で照合する。
async fn fit_input(
    inner: &Inner,
    llm: &dyn LlmClient,
    model_key: &str,
    budget: usize,
    counting: Counting,
    previous: Option<&str>,
    utterance: &str,
) -> Option<String> {
    let mut limit = VERIFY_EXCERPT_CHARS;
    for _ in 0..=HALVINGS {
        let input = verify_input(
            inner.language.as_str(),
            previous.map(|p| tail_excerpt(p, limit)),
            utterance,
        );
        let mut c = counting;
        let n = count_tokens(inner, llm, model_key, &input, &mut c).await;
        // この要求だけ見積もりになったら、予算を 2 割減らして比べる (06 §8)。
        let budget = if c == counting {
            budget
        } else {
            budget * 4 / 5
        };
        if n <= budget {
            return Some(input);
        }
        limit /= 2;
    }
    None
}

/// 事実を書く経路のロックの下で、改訂の番号を読み直して検証の結果の記録を書き、検証済みの訂正を同じ依頼で畳む
/// (04 §5.2.1・§6)。落とされない task で行い、ロックは spawn の前に取って渡す (extraction.rs の `save` と同じ)。
/// 書いた結果 (改訂の番号が変わっていれば `stale_source`) と畳みの報告を返す。記録が無い・もう検証したなら、何も
/// 書かずに `Some(None)`。書けなければ `None`。
async fn save(inner: &Arc<Inner>, id: Id, verdict: Verdict) -> Option<Option<(Verdict, Report)>> {
    let lock = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let w = worker.clone();
        type Built = (Vec<WriteOp>, Option<Verdict>, Report);
        let built = tokio::task::spawn_blocking(move || -> Option<Built> {
            let key = w.facts_key.key()?;
            let now = w.clock.now();
            let tx = w.store.read().ok()?;
            let Some((op, written)) = checked_op(&tx, id, verdict, now).ok()? else {
                return Some((Vec::new(), None, Report::default()));
            };
            let mut ops = vec![op];
            let mut report = Report::default();
            if let Verdict::Verified(v) = &written {
                let safety = Safety::bundled();
                let ctx = FactCtx {
                    key,
                    attributes: attributes(),
                    safety: &safety,
                    ids: &*w.ids,
                    now,
                };
                // 畳めなくても (記録を読めない・本体を書けない)、検証の結果の記録は書く。検証し直しで LLM を
                // 呼び続けないため。検証済みの記録は残るので、事実の作り直し (`rebuild --facts`) で当て直せる。
                match consolidate(&tx, &ctx, vec![FactEvent::Correction(v.clone())]) {
                    Ok((fact_ops, r)) => {
                        ops.extend(fact_ops);
                        report = r;
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "a verified correction could not be consolidated; only the record is written"
                    ),
                }
            }
            Some((ops, Some(written), report))
        })
        .await
        .ok()
        .flatten();
        let (ops, written, report) = built?;
        let Some(written) = written else {
            return Some(None);
        };
        if let Err(e) = crate::write_derived(&worker, ops).await {
            tracing::warn!(error = %e, "a correction could not be written");
            return None;
        }
        state(&worker).unreported.add(&report);
        crate::fact_table::facts_changed(&worker, &report.withdrawn);
        Some(Some((written, report)))
    });
    task.await.ok().flatten()
}

/// 手順を動かす。1 件でも検証の結果を書けたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = state(inner).version;
    let store = inner.store.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let r = store.read().ok()?;
        load_corrections(&r).ok()
    })
    .await;
    let Ok(Some(loaded)) = loaded else {
        return false;
    };
    let mut tally = Tally::default();
    tally.candidates = loaded.unchecked.len();
    let (llm, server, _) = inner.conn();
    // 裏の処理は最後のスロット (06 §4)。
    let slot = (server.total_slots > 1).then(|| server.total_slots - 1);
    let system = verify_system(inner.language.as_str());
    let mut progressed = false;
    let mut budget: Option<(usize, Counting)> = None;
    for c in loaded.unchecked {
        let Some(key) = inner.facts_key.key() else {
            return progressed;
        };
        let store = inner.store.clone();
        let candidate = c.clone();
        let lang = inner.language;
        let prepared = tokio::task::spawn_blocking(move || prepare(&store, &candidate, lang))
            .await
            .ok()
            .flatten();
        let Some(prepared) = prepared else {
            return progressed;
        };
        let verdict = match prepared {
            Prepared::Stale => Verdict::Unverifiable(Unverifiable::StaleSource),
            Prepared::Ready(m) => {
                if crate::notes::response_slot_busy(&*llm).await {
                    return progressed;
                }
                // システムのメッセージは 1 回だけ数える (`/tokenize` の数はキャッシュする、06 §8)。
                let (limit, counting) = match budget {
                    Some(b) => b,
                    None => {
                        let mut counting = Counting::Exact;
                        let n =
                            count_tokens(inner, &*llm, &server.model_key, &system, &mut counting)
                                .await;
                        let b = (utterance_budget(n, counting), counting);
                        budget = Some(b);
                        b
                    }
                };
                // 型の照合は、非同期の実行器を塞がないように別のスレッドで行う。
                let m = Arc::new(m);
                let excerpt = {
                    let (m, pattern) = (m.clone(), c.pattern.clone());
                    tokio::task::spawn_blocking(move || {
                        let filtered = Prefiltered::from_stored(&m.text);
                        utterance_excerpt(filtered.text(), &pattern).to_owned()
                    })
                    .await
                };
                let Ok(utterance) = excerpt else {
                    return progressed;
                };
                // 送る直前に、置き換えを当て直す (材料を読んでから送るまでの間に private にした値も
                // 伏せる、04 §7.5)。当て直した本文が変われば、その本文で収め直す。2 回続けて変われば、この回は
                // 送らない。
                let mut previous = m.previous_prompt.clone();
                let mut input = None;
                for _ in 0..2 {
                    let fitted = fit_input(
                        inner,
                        &*llm,
                        &server.model_key,
                        limit,
                        counting,
                        previous.as_deref(),
                        &utterance,
                    )
                    .await;
                    let store = inner.store.clone();
                    let (id, lang) = (c.previous, inner.language);
                    let again =
                        tokio::task::spawn_blocking(move || masked_previous(&store, id, lang))
                            .await
                            .ok()
                            .flatten();
                    let Some(again) = again else {
                        return progressed;
                    };
                    if again == previous {
                        input = Some(fitted);
                        break;
                    }
                    previous = again;
                }
                let Some(input) = input else {
                    return progressed;
                };
                match input {
                    None => Verdict::Unverifiable(Unverifiable::TooLong),
                    Some(input) => {
                        match call(inner, &*llm, slot, &system, &input, &mut tally).await {
                            Reply::Parsed(out) => {
                                let (m, c, key) = (m.clone(), c.clone(), key.clone());
                                let judged = tokio::task::spawn_blocking(move || {
                                    let filtered = Prefiltered::from_stored(&m.text);
                                    let safety = Safety::bundled();
                                    let ctx = VerifyContext {
                                        utterance: Utterance {
                                            turn: c.turn,
                                            rev: c.rev,
                                            text: &m.text,
                                            filtered: &filtered,
                                        },
                                        // 関門は切る前の保存した本文で照合する。
                                        previous: m.previous.as_deref(),
                                        earlier: &m.earlier,
                                        live: &m.live,
                                        key: &key,
                                        safety: &safety,
                                    };
                                    match verify(&c, &out, &ctx) {
                                        Ok(v) => Verdict::Verified(v),
                                        Err(r) => Verdict::Rejected(r),
                                    }
                                })
                                .await;
                                let Ok(v) = judged else {
                                    return progressed;
                                };
                                v
                            }
                            Reply::Truncated => Verdict::Unverifiable(Unverifiable::Truncated),
                            Reply::Invalid => Verdict::Unverifiable(Unverifiable::InvalidOutput),
                            Reply::Failed(Failure::Rejected) => {
                                Verdict::Unverifiable(Unverifiable::RequestRejected)
                            }
                            Reply::Failed(Failure::Unreachable) => return progressed,
                            Reply::Failed(Failure::Transient) => {
                                if count_failure(inner, c.id) < FAILURE_LIMIT {
                                    return progressed;
                                }
                                tracing::warn!(
                                    "correction verification kept failing; the candidate is skipped"
                                );
                                Verdict::Unverifiable(Unverifiable::Failed)
                            }
                        }
                    }
                }
            }
        };
        state(inner).failures = None;
        let Some(saved) = save(inner, c.id, verdict).await else {
            return progressed;
        };
        if let Some((written, report)) = saved {
            tally.count(&written);
            tally.add(&report);
        }
        progressed = true;
    }
    let changed = {
        let mut st = state(inner);
        st.clean = Some(version);
        std::mem::take(&mut st.unreported)
    };
    changed.emit(inner);
    progressed
}
