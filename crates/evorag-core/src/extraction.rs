//! 事実の抽出と統合の sleep-time の手順 (04 §4 の手順 3・5、04 §5.2・§5.2.1・§5.3)。
//!
//! - 対象: チャットのセッションの利用者の発話を、進み具合 (`job` の `extract_facts`) より後から、ターンの ID の
//!   順に処理する。前のふるいで送らないものは、理由ごとに数えて飛ばす。
//! - 束ね方: `/tokenize` の数 (キャッシュする、06 §8) で、発話の予算 (2,048 − システムのメッセージ − 余白)
//!   までに詰める。予算を超える 1 つの発話は送らずに数える。見積もりで数えたときは予算を 2 割減らす。
//!   プロンプトと生成の上限がスロットの文脈に収まらない構成では抽出しない。
//! - 呼び出し: 型縛りの補助の呼び出し (temperature 0・固定の seed・裏のスロット、06 §6)。型が合わなければ
//!   1 回だけやり直し、それでも合わなければその束を飛ばして進み具合を先へ進める。出力が上限で切れたら、
//!   束を半分に分けて送り直す (1 つの発話なら飛ばす)。
//! - 失敗: 4xx はその束を飛ばして進める。締切切れと 5xx は進めずに次の機会に送り直し、同じ束で 3 回続いたら
//!   飛ばす。繋がらないときは数えずに待つ。
//! - 書き込み: 事実を書く経路のロック (`Inner::facts_lock`) を取ってから記録を読み、書き込みの入口
//!   (`evorag_memory::consolidate`) で畳み、事実の書き込みと進み具合を 1 つの依頼で書き終えるまで放さない。
//!   出典の改訂の番号の照合も、このロックの下で読み直して行う (04 §4)。
//! - 訂正の候補 (04 §6): 同じ発話の列を同じ進み具合で処理するときに、前のふるいの後の本文に字句の規則を当て
//!   (LLM を使わない)、当たった候補を、その束の事実と同じ依頼で書く。検証は後の手順 (correction.rs) で行う。
//! - 利用者が操作したら、出している要求ごと落とす (sleep.rs)。書き込みは落とされない task で行い、その task
//!   にはロックを取ってから渡すので、書いた束までは残り、終了の処理はロックを取って書き終わりを待てる。
//!   次の機会に続きから処理する。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use evorag_llm::{ChatMessage, ChatRequest, Generation, LlmClient, LlmError, RequestExtra, Role};
use evorag_memory::{
    CandidateSkip, CorrectionBody, Counting, EXTRACT_MAX_TOKENS, Event as FactEvent,
    ExtractionOutput, FactCtx, GateEnv, Report, Safety, Screened, Utterance, attributes,
    candidate_ops, check_candidates, consolidate, extraction_fits, extraction_input,
    extraction_schema, extraction_system, find_correction, load_directives, load_extract_progress,
    pack, parse_extraction, prefilter, previous_answer, utterance_budget,
};
use evorag_store::{Store, WriteOp};
use evorag_types::Id;
use evorag_types::api::Event;
use serde_json::json;

use crate::Inner;
use crate::probe::ChatError;

/// 締切切れと 5xx が同じ束で続いたら、その束を飛ばす回数 (04 §5.2.1 の「失敗の扱い」)。
pub(crate) const FAILURE_LIMIT: u32 = 3;

/// 抽出の手順の状態 (このプロセスの間だけ)。
#[derive(Default)]
pub(crate) struct State {
    /// 会話が変わるたびに増える。処理し終えた版と違えば、もう一度見る。
    version: u64,
    clean: Option<u64>,
    /// まだ知らせていない事実の変化 (落とされた回の分も、次に終えた回でまとめて知らせる)。
    unreported: Counts,
    /// 締切切れと 5xx が続いている束 (束の最初に送る発話のターン) と、続いた回数。
    failures: Option<(Id, u32)>,
    /// スロットの文脈に収まらないと記録した構成 (model_key と `n_ctx`。同じ構成で繰り返し記録しない)。
    unfit_logged: Option<(String, u32)>,
}

/// 知らせる件数 (04 §5.3 の「知らせ」)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    added: usize,
    replaced: usize,
    contested: usize,
}

impl Counts {
    pub(crate) fn add(&mut self, r: &Report) {
        self.added += r.added;
        self.replaced += r.replaced;
        self.contested += r.contested;
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 状態の行に件数を出す (04 §5.3 の「知らせ」)。何も変わっていなければ出さない。
    pub(crate) fn emit(self, inner: &Inner) {
        if !self.is_empty() {
            crate::emit(
                inner,
                Event::FactsChanged {
                    added: self.added,
                    replaced: self.replaced,
                    contested: self.contested,
                },
            );
        }
    }
}

fn state(inner: &Inner) -> std::sync::MutexGuard<'_, State> {
    inner.extraction.lock().unwrap_or_else(|e| e.into_inner())
}

/// 会話が変わった (ターンを書いた、セッションを消した)。
pub(crate) fn touch(inner: &Inner) {
    state(inner).version += 1;
}

/// 要るか: 見ていない会話の変更があり、事実の機能が使え (03 §3.1.1)、型縛りの補助の呼び出しを使え
/// (06 §3.3 の点検で通った)、要求がスロットの文脈に収まるとき (04 §5.2.1 の「束ね方」と「止めるとき」)。
pub(crate) fn needed(inner: &Inner) -> bool {
    let due = {
        let st = state(inner);
        st.clean != Some(st.version)
    };
    due && inner.facts_key.key().is_some()
        && inner.startup_done.load(std::sync::atomic::Ordering::SeqCst)
        && crate::notes::usable(inner)
        && fits(inner)
}

/// 抽出の要求 (プロンプトと生成の上限) がスロットの文脈に収まるか。収まらなければ、構成ごとに 1 回だけ記録する
/// (`doctor` の報告は後の版)。
pub(crate) fn fits(inner: &Inner) -> bool {
    let server = inner.conn().1;
    if extraction_fits(server.n_ctx as usize) {
        return true;
    }
    let config = (server.model_key, server.n_ctx);
    let mut st = state(inner);
    if st.unfit_logged.as_ref() != Some(&config) {
        tracing::warn!(
            n_ctx = config.1,
            needed = evorag_memory::REQUEST_TOKENS + EXTRACT_MAX_TOKENS as usize,
            "the slot context is too small for fact extraction; facts are not extracted"
        );
        st.unfit_logged = Some(config);
    }
    false
}

/// 進み具合より後の利用者の発話 1 つと、前のふるいの結果。
struct Item {
    turn: Id,
    rev: u32,
    text: String,
    screened: Screened,
    /// 字句の規則で見つけた訂正の候補 (04 §6)。
    correction: Option<CorrectionBody>,
}

impl Item {
    /// 送る本文 (前のふるいの後)。送らない発話なら `None`。
    fn sent_text(&self) -> Option<&str> {
        match &self.screened {
            Screened::Send(p) => Some(p.text()),
            Screened::Skip(_) => None,
        }
    }
}

/// 進み具合より後の利用者の発話を読み、前のふるいを当てる。訂正の候補も探し、直前の応答を引いておく。
/// 忘れるの指示と合わない発話 (04 §7.2) は、抽出にも訂正の候補にも使わない。
fn plan(store: &Store, lang: crate::prompt::PromptLanguage) -> Option<Vec<Item>> {
    let r = store.read().ok()?;
    let through = load_extract_progress(&r).ok()?.through;
    let hide = crate::masking::Hide::load(&r, lang)?;
    let mut items = Vec::new();
    for u in evorag_memory::user_utterances_after(&r, through).ok()? {
        if let Ok(Some(evorag_store::Read::Ok(e))) = r.get(evorag_memory::KIND_TURN, u.turn)
            && hide.excluded(&e)
        {
            continue;
        }
        let correction = match find_correction(&u.mode, &u.text) {
            Some(m) => {
                let previous = previous_answer(&r, u.session, u.turn).ok()?;
                Some(CorrectionBody::candidate(u.turn, u.rev, previous, &m))
            }
            None => None,
        };
        items.push(Item {
            screened: prefilter(&u.mode, &u.text),
            turn: u.turn,
            rev: u.rev,
            text: u.text,
            correction,
        });
    }
    Some(items)
}

/// 1 回の手順の件数 (手順の終わりに、途中で止めたときも、落とされたときも記録する)。
#[derive(Debug, Default)]
struct Tally {
    /// 処理の対象の発話。
    utterances: usize,
    /// 送った要求 (やり直しと、分けて送り直したものを含む)。
    requests: usize,
    /// 送らずに飛ばした発話 (前のふるいの理由、`too_long`、`invalid_output`、`truncated`、`rejected`、`failed`)。
    skipped: BTreeMap<&'static str, usize>,
    /// 関門と属性の一覧の確認で捨てた候補 (理由ごと)。通った候補の別に数える理由 (`old_ignored` など) も含む。
    gate: BTreeMap<&'static str, usize>,
    /// 型に合わなかった答え (やり直しの前のものも数える)。
    invalid_outputs: usize,
    /// 生成の上限で切れた答え。
    truncated: usize,
    /// 失敗した要求 (4xx・5xx・締切切れ・繋がらない)。
    failed: usize,
    /// 字句の規則で見つけた訂正の候補 (04 §6)。
    corrections: usize,
}

impl Tally {
    fn skip(&mut self, reason: &'static str, n: usize) {
        if n > 0 {
            *self.skipped.entry(reason).or_default() += n;
        }
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        if self.utterances == 0 {
            return;
        }
        tracing::info!(
            utterances = self.utterances,
            requests = self.requests,
            skipped = ?self.skipped,
            gate = ?self.gate,
            invalid_outputs = self.invalid_outputs,
            truncated = self.truncated,
            too_long = self.skipped.get("too_long").copied().unwrap_or(0),
            failed = self.failed,
            corrections = self.corrections,
            "fact extraction pass"
        );
    }
}

/// 失敗の扱いの分け方 (04 §5.2.1 の「失敗の扱い」、06 §7)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// サーバに繋がらない (回数に入れず、進めずに待つ)。
    Unreachable,
    /// 4xx: 要求そのものが受け付けられない (再試行せず、束を飛ばして進める)。
    Rejected,
    /// 締切切れ・5xx・逐次表示の途中の失敗 (進めずに次の機会に送り直す。同じ束で続いたら飛ばす)。
    Transient,
}

impl Failure {
    pub(crate) fn of(e: &ChatError) -> Self {
        match e {
            ChatError::TimedOut => Self::Transient,
            ChatError::Llm(LlmError::Http { status, .. }) if (400..500).contains(status) => {
                Self::Rejected
            }
            ChatError::Llm(
                LlmError::Http { .. }
                | LlmError::Server(_)
                | LlmError::Cut(_)
                | LlmError::Protocol(_),
            ) => Self::Transient,
            ChatError::Llm(
                LlmError::Connect(_)
                | LlmError::BadEndpoint(_)
                | LlmError::RemoteNotAllowed(_)
                | LlmError::RouterNeedsModel { .. }
                | LlmError::RouterModel { .. },
            ) => Self::Unreachable,
        }
    }
}

/// 1 回の呼び出しの結果。
enum Reply {
    Parsed(ExtractionOutput),
    /// 生成が上限で切れた (`finish_reason` が `length`)。
    Truncated,
    /// 型に合わない答えが 2 回続いた。
    Invalid,
    Failed(Failure),
}

/// 束の処理を止めて、進み具合を進めない理由。
enum Stop {
    /// 応答のスロットが処理中 (利用者の応答を先に通す)。
    Busy,
    Failed(Failure),
}

/// 抽出の要求 (06 §6: temperature 0・固定の seed・固定のスロット、`aux_request_extra`、`response_format`)。
fn request(inner: &Inner, slot: Option<u32>, system: &str, texts: &[&str]) -> ChatRequest {
    extraction_request(&inner.aux_request_extra, slot, system, texts)
}

/// 抽出の要求の形 (sleep-time の抽出と `rebuild --facts` で共通、06 §6): 型縛り (`response_format`)・
/// temperature 0・固定の seed・指定のスロット・補助の呼び出しに足す JSON (`aux_request_extra`)。
pub(crate) fn extraction_request(
    aux_request_extra: &RequestExtra,
    slot: Option<u32>,
    system: &str,
    texts: &[&str],
) -> ChatRequest {
    let mut extra = aux_request_extra.clone();
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
            ChatMessage::new(Role::System, system),
            ChatMessage::new(Role::User, extraction_input(texts.iter().copied())),
        ],
        generation: Generation {
            temperature: 0.0,
            top_p: 1.0,
            max_tokens: Some(EXTRACT_MAX_TOKENS),
            seed: Some(1),
        },
        id_slot: slot,
        model: None,
        extra,
    }
}

/// 束を送り、型で読む。型に合わなければ 1 回だけやり直す (06 §6)。切れた答えはやり直さない。
async fn call(
    inner: &Inner,
    llm: &dyn LlmClient,
    slot: Option<u32>,
    system: &str,
    texts: &[&str],
    tally: &mut Tally,
) -> Reply {
    let req = request(inner, slot, system, texts);
    for _ in 0..2 {
        tally.requests += 1;
        let c = match crate::probe::run_chat(llm, req.clone()).await {
            Ok(c) => c,
            Err(e) => {
                let failure = Failure::of(&e);
                tracing::warn!(error = %e, ?failure, "fact extraction request failed");
                tally.failed += 1;
                return Reply::Failed(failure);
            }
        };
        if c.finish_reason.as_deref() == Some("length") {
            tally.truncated += 1;
            return Reply::Truncated;
        }
        if let Some(out) = parse_extraction(&c.text, texts.len()) {
            return Reply::Parsed(out);
        }
        tally.invalid_outputs += 1;
    }
    Reply::Invalid
}

/// 事実への指示 (関門 8 の再び覚えない印) を読む。読めなければ空。
async fn directives(inner: &Inner) -> evorag_memory::Directives {
    let store = inner.store.clone();
    tokio::task::spawn_blocking(move || {
        store
            .read()
            .ok()
            .and_then(|r| load_directives(&r).ok())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// 束の候補に、属性の一覧の確認と関門 1〜8 を当て、通ったものを統合の出来事にする。捨てた理由は数える。
fn gate(
    inner: &Inner,
    directives: &evorag_memory::Directives,
    items: &[&Item],
    out: ExtractionOutput,
    tally: &mut Tally,
) -> Vec<FactEvent> {
    let Some(key) = inner.facts_key.key() else {
        return Vec::new();
    };
    let sent = items
        .iter()
        .map(|it| (it.turn, it.rev, it.text.as_str(), &it.screened));
    gate_batch(key, directives, sent, out, &mut tally.gate)
}

/// 束の候補に、属性の一覧の確認と関門 1〜8 を当て、通ったものを統合の出来事にする (sleep-time の抽出と
/// `rebuild --facts` で共通)。`items` は束の発話 (ターン・改訂の番号・保存した本文・前のふるいの結果) で、
/// 答えの発話ごとの候補と順に組にする。捨てた候補は理由ごとに `rejected` に数え、通った候補の別に数える理由
/// (`evidence_normalized`・`evidence_loose`・`old_ignored`、04 §5.2) も同じ表に数える。
pub(crate) fn gate_batch<'a>(
    key: &evorag_memory::ForgetKey,
    directives: &evorag_memory::Directives,
    items: impl IntoIterator<Item = (Id, u32, &'a str, &'a Screened)>,
    out: ExtractionOutput,
    rejected: &mut BTreeMap<&'static str, usize>,
) -> Vec<FactEvent> {
    let safety = Safety::bundled();
    let env = GateEnv {
        attributes: attributes(),
        safety: &safety,
        key,
        directives,
    };
    let mut events = Vec::new();
    for ((turn, rev, text, screened), raws) in items.into_iter().zip(out.by_utterance) {
        let Screened::Send(filtered) = screened else {
            continue;
        };
        let u = Utterance {
            turn,
            rev,
            text,
            filtered,
        };
        for r in check_candidates(&env, &u, &raws) {
            match r {
                Ok(a) => {
                    for count in a.counts() {
                        *rejected.entry(count).or_default() += 1;
                    }
                    events.push(FactEvent::Candidate(a.candidate));
                }
                Err(reason) => *rejected.entry(reason.as_str()).or_default() += 1,
            }
        }
    }
    if out.out_of_batch > 0 {
        *rejected.entry("out_of_batch").or_default() += out.out_of_batch;
    }
    events
}

/// 束の発話を送って候補を得る。出力が上限で切れたら半分に分けて送り直す (04 §5.2.1)。型が合わない・切れた
/// 1 つの発話・4xx の部分は飛ばして数え、ほかの部分は続ける。進み具合を進めずに止めるときは `Err`。
async fn extract_batch(
    inner: &Inner,
    llm: &dyn LlmClient,
    slot: Option<u32>,
    system: &str,
    sent: &[&Item],
    tally: &mut Tally,
) -> Result<Vec<FactEvent>, Stop> {
    let mut events = Vec::new();
    let mut queue: VecDeque<&[&Item]> = VecDeque::from([sent]);
    while let Some(part) = queue.pop_front() {
        if crate::notes::response_slot_busy(llm).await {
            return Err(Stop::Busy);
        }
        let texts: Vec<&str> = part.iter().filter_map(|it| it.sent_text()).collect();
        match call(inner, llm, slot, system, &texts, tally).await {
            Reply::Parsed(out) => {
                let directives = directives(inner).await;
                events.extend(gate(inner, &directives, part, out, tally));
            }
            Reply::Truncated if part.len() > 1 => {
                let (head, tail) = part.split_at(part.len() / 2);
                tracing::info!(
                    utterances = part.len(),
                    "fact candidates were cut at the output limit; sending the batch in halves"
                );
                queue.push_front(tail);
                queue.push_front(head);
            }
            Reply::Truncated => {
                // 事実は派生物なので、止まり続けるより、この発話を失って先へ進む (04 §5.2.1)。
                tracing::warn!("fact candidates for one utterance were cut; it is skipped");
                tally.skip("truncated", part.len());
            }
            Reply::Invalid => {
                tracing::warn!(
                    utterances = part.len(),
                    "fact candidates did not match the schema; the batch is skipped"
                );
                tally.skip("invalid_output", part.len());
            }
            Reply::Failed(Failure::Rejected) => tally.skip("rejected", part.len()),
            Reply::Failed(f @ (Failure::Transient | Failure::Unreachable)) => {
                return Err(Stop::Failed(f));
            }
        }
    }
    Ok(events)
}

/// 締切切れか 5xx を、束 (最初に送る発話のターン `first`) について数える。続いた回数を返す。
fn count_failure(inner: &Inner, first: Id) -> u32 {
    let mut st = state(inner);
    let n = match st.failures {
        Some((id, n)) if id == first => n + 1,
        _ => 1,
    };
    st.failures = Some((first, n));
    n
}

/// 事実を書く経路のロックの下で、記録を読んで畳み、事実と訂正の候補と進み具合を 1 つの依頼で書く
/// (04 §5.2.1・§6)。落とされない task で行う (読んだ後に落とされて、ロックの外で書きかけにならないように)。
/// ロックは spawn の前に取って task に渡す (終了の処理がロックを取れたら、出した書き込みは終わっている)。
async fn save(
    inner: &Arc<Inner>,
    events: Vec<FactEvent>,
    corrections: Vec<CorrectionBody>,
    through: Id,
    model_key: String,
) -> bool {
    let lock = inner.facts_lock.clone().lock_owned().await;
    let worker = inner.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let w = worker.clone();
        let built = tokio::task::spawn_blocking(move || -> Option<(Vec<WriteOp>, Report, bool)> {
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
            // 訂正の候補。このロックの下で、ターンがあり改訂の番号が読んだときと合うものだけを書く (読んでから
            // 書くまでに削除・忘れるが入ったとき、消した本文の写しを書き戻さないため、04 §6)。そのターンに記録が
            // もうあれば書かない。
            let mut written = 0;
            let mut stale = 0;
            for c in &corrections {
                match candidate_ops(&tx, &*w.ids, now, c).ok()? {
                    Ok(more) => {
                        ops.extend(more);
                        written += 1;
                    }
                    Err(CandidateSkip::Stale) => stale += 1,
                    // そのターンに記録がもうある、または忘れたターン (04 §6 の「候補を見つける時機」)。
                    Err(CandidateSkip::Exists | CandidateSkip::Forgotten) => {}
                }
            }
            if stale > 0 {
                tracing::info!(
                    stale,
                    "correction candidates whose turn changed or went away were not written"
                );
            }
            let new_corrections = written > 0;
            let progress = load_extract_progress(&tx).ok()?;
            // 進み具合は戻さない。
            if progress.through.is_none_or(|t| t < through) {
                ops.push(progress.advance(through, &model_key, &*w.ids, now).ok()?);
            }
            Some((ops, report, new_corrections))
        })
        .await
        .ok()
        .flatten();
        let Some((ops, report, new_corrections)) = built else {
            return false;
        };
        if let Err(e) = crate::write_derived(&worker, ops).await {
            tracing::warn!(error = %e, "facts could not be written");
            return false;
        }
        if new_corrections {
            // 検証の手順 (04 §4 の手順 4) に、検証していない候補があることを知らせる。
            crate::correction::touch(&worker);
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
            subject_rejected = report.subject_rejected,
            "facts consolidated"
        );
        state(&worker).unreported.add(&report);
        // 事実の検索の鍵と事実の表は、続く sleep-time の手順で作り直す (04 §4 の手順 6・7)。表の事実が表から
        // 外れたら、次のターンの前に作り直す (04 §5.4)。
        crate::fact_table::facts_changed(&worker, &report.withdrawn);
        true
    });
    task.await.unwrap_or(false)
}

/// `/tokenize` で数える (キャッシュする、06 §8)。使えなければ見積もり、`counting` を見積もりにする。
pub(crate) async fn count_tokens(
    inner: &Inner,
    llm: &dyn LlmClient,
    model_key: &str,
    text: &str,
    counting: &mut Counting,
) -> usize {
    match inner.tokens.count(llm, model_key, text).await {
        Some(n) => n,
        None => {
            *counting = Counting::Estimated;
            crate::estimate(text)
        }
    }
}

/// 手順を動かす。1 つでも束を書けたら `true`。
pub(crate) async fn run(inner: &Arc<Inner>) -> bool {
    let version = state(inner).version;
    let store = inner.store.clone();
    let lang = inner.language;
    let Ok(Some(items)) = tokio::task::spawn_blocking(move || plan(&store, lang)).await else {
        return false;
    };
    let mut tally = Tally::default();
    tally.utterances = items.len();
    let (llm, server, _) = inner.conn();
    // 裏の処理は最後のスロット (06 §4)。
    let slot = (server.total_slots > 1).then(|| server.total_slots - 1);

    // システムのメッセージと発話を数える。1 つでも見積もりなら、予算を 2 割減らす (06 §8)。
    let system = extraction_system(inner.language.as_str(), attributes());
    let mut counting = Counting::Exact;
    let system_tokens = count_tokens(inner, &*llm, &server.model_key, &system, &mut counting).await;
    let mut tokens = Vec::with_capacity(items.len());
    for it in &items {
        tokens.push(match &it.screened {
            Screened::Send(p) => {
                Some(count_tokens(inner, &*llm, &server.model_key, p.text(), &mut counting).await)
            }
            Screened::Skip(reason) => {
                tally.skip(reason.as_str(), 1);
                None
            }
        });
    }
    let budget = utterance_budget(system_tokens, counting);
    if budget == 0 {
        tracing::warn!(
            system_tokens,
            "the extraction system message leaves no room for utterances; facts are not extracted"
        );
        return false;
    }
    let packing = pack(&tokens, budget);
    tally.skip("too_long", packing.too_long.len());
    if !items.is_empty() {
        tracing::info!(
            utterances = items.len(),
            batches = packing.batches.len(),
            budget,
            ?counting,
            "fact extraction planned"
        );
    }

    tally.corrections = items.iter().filter(|it| it.correction.is_some()).count();
    let mut progressed = false;
    let mut start = 0;
    for batch in packing.batches {
        let through = items[batch.through].turn;
        // 束が覆う発話 (送らない発話を含む) の訂正の候補を、束の事実と同じ依頼で書く (04 §6)。
        let corrections: Vec<CorrectionBody> = items[start..=batch.through]
            .iter()
            .filter_map(|it| it.correction.clone())
            .collect();
        start = batch.through + 1;
        let sent: Vec<&Item> = batch.send.iter().map(|&i| &items[i]).collect();
        let events = match sent.first() {
            None => Vec::new(),
            Some(first) => {
                let first = first.turn;
                match extract_batch(inner, &*llm, slot, &system, &sent, &mut tally).await {
                    Ok(events) => {
                        state(inner).failures = None;
                        events
                    }
                    Err(Stop::Busy | Stop::Failed(Failure::Unreachable)) => return progressed,
                    Err(Stop::Failed(_)) => {
                        let n = count_failure(inner, first);
                        if n < FAILURE_LIMIT {
                            return progressed;
                        }
                        tracing::warn!(
                            utterances = sent.len(),
                            failures = n,
                            "fact extraction kept failing for this batch; it is skipped"
                        );
                        state(inner).failures = None;
                        tally.skip("failed", sent.len());
                        Vec::new()
                    }
                }
            }
        };
        if !save(
            inner,
            events,
            corrections,
            through,
            server.model_key.clone(),
        )
        .await
        {
            return progressed;
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
