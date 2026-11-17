//! `evorag rebuild --facts` (03 §10、04 §2・§7.5 の「作り直しと訂正」)。
//!
//! - 実行の前に確かめる (読むだけ。退避も作らない): llama-server に繋がる、応答のモデルが記録と同じ、そのモデルの
//!   点検で型縛りが通り思考を切れる、要求がスロットの文脈に収まる、値の鍵 (`memory.forget_key`) が使える、
//!   忘れるの当て直しが済んでいる (04 §7.2)。どれかが欠ければ、何も書かずに止める。
//! - 抽出: チャットのセッションの利用者の発話を、最初からターンの ID の順に、sleep-time の抽出 (04 §5.2.1) と同じ
//!   前のふるい・束ね方・要求・関門で処理する。訂正の候補は探さず、書かない (04 §6 の「候補を見つける時機」)。
//!   繋がらなくなったら止める (何も書いていないので、事実は前のまま)。
//! - 入れ替え: 抽出をすべて終えてから、候補と、残っている検証済みの訂正を、事実の書き込みの入口
//!   (`evorag_memory::rebuild`) で入力の順に畳み直す (事実への指示を当て直す。訂正は検証し直さない、04 §7.5)。
//!   読める事実をすべて消して作り直した事実を書き、抽出の進み具合を最後の発話まで進める操作を、1 つの依頼
//!   (1 つのトランザクション) で書く。事実は派生の記録なので `compact.pending` は立てない (03 §8)。
//! - 正本 (会話・事実への指示・訂正の記録) は変えない。読めない (新しい `v` の) 事実は残す (03 §4.2 の規則 6)。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use evorag_llm::{
    ChatMessage, ChatRequest, Generation, LlmClient, LlmError, RequestExtra, Role, ServerInfo,
};
use evorag_memory::{
    Counting, Directives, EXTRACT_MAX_TOKENS, Event as FactEvent, ExtractionOutput, FactCtx,
    ForgetEnv, ForgetKey, GateEnv, GateReason, KIND_FACT, ROLE_CHAT, Safety, Screened, Utterance,
    attributes, check_candidates, extraction_fits, extraction_input, extraction_schema,
    extraction_system, hidden_marker, load_bindings, load_corrections, load_directives,
    load_extract_progress, load_probe, pack, parse_extraction, prefilter, redaction_marker,
    replay_forgets, user_utterances_after, utterance_budget,
};
use evorag_store::{Durability, Priority, Read, Store, WriteOp, WriteRequest, WriterHandle, meta};
use evorag_types::{Clock, Id, IdGen};
use serde_json::json;

use super::RebuildError;
use crate::PromptLanguage;
use crate::extraction::{FAILURE_LIMIT, Failure};
use crate::forget_key::{FactsKey, FactsKeyProblem};
use crate::probe::ChatError;

/// 締切切れと 5xx の後、同じ束を送り直すまでの待ち (1 回目。回ごとに伸ばす)。
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// 事実を作り直すときに使う llama-server と設定。
pub struct FactsSource {
    /// 応答のモデルの llama-server (設定の `llm.chat`)。
    pub llm: Arc<dyn LlmClient>,
    /// 補助の呼び出しに足す JSON (`llm.chat.aux_request_extra`、06 §6)。
    pub aux_request_extra: RequestExtra,
    /// データ根にプロンプトの言語の記録が無いときに使う言語 (記録は書かない、07 §6.1)。
    pub prompt_language: PromptLanguage,
}

/// 事実を作り直せない理由 (何も書かずに止めた)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsUnavailable {
    /// 値の鍵が使えない (03 §3.1.1 のフェイルクローズ)。
    Key(FactsKeyProblem),
    /// 接続しているモデルの点検の結果が無い (06 §3.3)。
    NotProbed,
    /// 点検で、型縛りの出力か思考を切るのが通らなかった (04 §5.2.1 の「止めるとき」)。
    ProbeFailed,
    /// 抽出の要求と生成の上限が、スロットの文脈に収まらない (04 §5.2.1 の「束ね方」)。
    ContextTooSmall { n_ctx: u32 },
    /// 忘れるの当て直し (04 §7.2) が済んでいない。
    ForgetPending,
    /// 新しい版の evorag が書いた (新しい `v` の) 事実がある。事実は、新しい `v` の記録があると今の `v` で作らない
    /// (03 §4.2 の規則 6) ので、作り直せない。
    NewerFacts { count: usize },
}

impl std::fmt::Display for FactsUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(p) => write!(f, "memory.forget_key is not usable: {}", key_problem(*p)),
            Self::NotProbed => f.write_str("the connected model has not been probed"),
            Self::ProbeFailed => f.write_str(
                "the connected model failed the structured output or thinking-off probe",
            ),
            Self::ContextTooSmall { n_ctx } => {
                write!(
                    f,
                    "the slot context ({n_ctx} tokens) is too small for extraction"
                )
            }
            Self::ForgetPending => f.write_str("a forget has not been replayed yet"),
            Self::NewerFacts { count } => {
                write!(f, "{count} facts were written by a newer evorag")
            }
        }
    }
}

/// 値の鍵が使えない理由の説明 (ログとエラーの表示用。利用者への文言は CLI が文言キーで出す)。
fn key_problem(p: FactsKeyProblem) -> &'static str {
    match p {
        FactsKeyProblem::Conflict => "the journal and the database disagree",
        FactsKeyProblem::MissingWithDirectives => "it is missing although directives exist",
        FactsKeyProblem::Malformed => "the recorded key is malformed",
        FactsKeyProblem::ReadOnly => "the data root is read-only",
    }
}

/// 抽出を止めた理由 (llama-server の失敗。何も書かずに止める、03 §10)。
#[derive(Debug)]
pub enum ExtractionStop {
    /// サーバに繋がらない。
    Unreachable(LlmError),
    /// 要求を断られた (4xx)。
    Rejected(LlmError),
    /// 締切切れか 5xx が、同じ束で `attempts` 回続いた。`detail` は最後の失敗。
    Failed { attempts: u32, detail: String },
}

impl std::fmt::Display for ExtractionStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "llama-server is unreachable: {e}"),
            Self::Rejected(e) => write!(f, "llama-server rejected the extraction request: {e}"),
            Self::Failed { attempts, detail } => write!(
                f,
                "the extraction request failed {attempts} times in a row: {detail}"
            ),
        }
    }
}

/// 事実の作り直しの結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactsRebuilt {
    /// 処理した利用者の発話 (チャットのセッションの外のものを含む)。
    pub utterances: usize,
    /// 送らずに飛ばした発話 (理由ごと。前のふるいの理由 (`mode`・`empty`・`greeting`・`short`)、`too_long`、
    /// `forget_mismatch`、抽出できなかった `truncated`・`invalid_output`)。
    pub skipped: BTreeMap<&'static str, usize>,
    /// 送った要求 (やり直しと、分けて送り直したものを含む)。
    pub requests: usize,
    /// 消した事実。
    pub removed: usize,
    /// 作り直した事実。
    pub facts: usize,
    /// 当て直した検証済みの訂正。
    pub corrections: usize,
    /// 再び覚えない印に当たって捨てた候補 (関門 8 と書き込みの入口の分)。
    pub blocked: usize,
    /// 読めない (新しい `v`・知らない形の) ので残した事実。
    pub kept_unknown: usize,
}

/// 抽出できずに飛ばした理由 (作り直しが部分的になった理由。前のふるいで送らなかった発話は含まない)。
/// 4xx と、締切切れか 5xx が続いたときは、飛ばさずに作り直しを止める ([`ExtractionStop`])。
pub const LOST_REASONS: [&str; 2] = ["truncated", "invalid_output"];

impl FactsRebuilt {
    /// 飛ばした発話の合計。
    #[must_use]
    pub fn skipped_total(&self) -> usize {
        self.skipped.values().sum()
    }

    /// 理由ごとの飛ばした発話の数 (無ければ 0)。
    #[must_use]
    pub fn skipped_for(&self, reason: &str) -> usize {
        self.skipped.get(reason).copied().unwrap_or(0)
    }

    /// 抽出できずに飛ばした発話 ([`LOST_REASONS`]。出力が上限で切れた・型に合わない)。0 でなければ、作り直しは
    /// 部分的で、これらの発話の事実は作られていない。
    #[must_use]
    pub fn lost(&self) -> usize {
        LOST_REASONS.iter().map(|r| self.skipped_for(r)).sum()
    }

    /// 送らなかった発話 (前のふるい・予算を超える・当て直しで本文が合わない)。
    #[must_use]
    pub fn not_sent(&self) -> usize {
        self.skipped_total() - self.lost()
    }

    /// 前のふるいで送らなかった発話 (チャットでない・空・挨拶・短い)。
    #[must_use]
    pub fn filtered(&self) -> usize {
        self.not_sent() - self.skipped_for("too_long") - self.skipped_for("forget_mismatch")
    }

    fn skip(&mut self, reason: &'static str, n: usize) {
        if n > 0 {
            *self.skipped.entry(reason).or_default() += n;
        }
    }
}

/// 確かめた結果 (退避の前に、読むだけで求める)。
pub(super) struct Checked {
    server: ServerInfo,
    language: &'static str,
    /// 値の鍵。`None` なら、鍵も事実への指示も無い (書き手を作ってから作る)。
    key: Option<ForgetKey>,
    /// 当て直しで本文が記録と合わないターン (裏の LLM の呼び出しから除く、04 §7.2)。
    excluded: BTreeSet<Id>,
}

/// 実行の前に確かめる (読むだけ)。
pub(super) async fn check(
    store: &Store,
    src: &FactsSource,
    clock: &dyn Clock,
    ids: &dyn IdGen,
) -> Result<Checked, RebuildError> {
    let server = src
        .llm
        .server_info()
        .await
        .map_err(RebuildError::LlmUnreachable)?;
    let read = |e: &dyn std::fmt::Display| RebuildError::Read(e.to_string());
    let language = match store.meta(meta::PROMPT_LANGUAGE).map_err(|e| read(&e))? {
        Some(s) => PromptLanguage::parse(&s).unwrap_or(src.prompt_language),
        None => src.prompt_language,
    }
    .as_str();
    {
        let tx = store.read().map_err(|e| read(&e))?;
        // 応答のモデルを記録と照合する (06 §3.1)。違うモデルで作り直さない。
        let bindings = load_bindings(&tx).map_err(|e| read(&e))?;
        if let Some(b) = bindings.by_role.get(ROLE_CHAT)
            && b.body.current.model_key != server.model_key
        {
            return Err(RebuildError::ModelMismatch {
                recorded: b.body.current.file_name.clone(),
                connected: evorag_llm::file_name(&server.model_path).to_owned(),
            });
        }
        let newer = newer_facts(&tx).map_err(|e| read(&e))?;
        if newer > 0 {
            return Err(FactsUnavailable::NewerFacts { count: newer }.into());
        }
        match load_probe(&tx, ROLE_CHAT, &server.model_key, &server.build_info)
            .map_err(|e| read(&e))?
        {
            None => return Err(FactsUnavailable::NotProbed.into()),
            Some((_, p)) if p.structured_ok != Some(true) || p.thinking_off == Some(false) => {
                return Err(FactsUnavailable::ProbeFailed.into());
            }
            Some(_) => {}
        }
    }
    if !extraction_fits(server.n_ctx as usize) {
        return Err(FactsUnavailable::ContextTooSmall {
            n_ctx: server.n_ctx,
        }
        .into());
    }
    let key = match crate::forget_key::ensure(store, None).map_err(|e| read(&e))? {
        FactsKey::Ready { key, .. } => Some(key),
        // 鍵も指示も無い (読むだけの確かめでは作らない)。
        FactsKey::Disabled(FactsKeyProblem::ReadOnly) => None,
        FactsKey::Disabled(p) => return Err(FactsUnavailable::Key(p).into()),
    };
    let mut excluded = BTreeSet::new();
    if let Some(key) = &key {
        let tx = store.read().map_err(|e| read(&e))?;
        let env = ForgetEnv {
            key,
            attributes: attributes(),
            marker: redaction_marker(language),
            hidden_marker: hidden_marker(language),
            ids,
            now: clock.now(),
        };
        let replay = replay_forgets(&tx, &env).map_err(|e| read(&e))?;
        if replay.changed() {
            return Err(FactsUnavailable::ForgetPending.into());
        }
        excluded.extend(replay.mismatched);
    }
    Ok(Checked {
        server,
        language,
        key,
        excluded,
    })
}

/// 新しい `v` の事実の数。新しい版の記録があると、今の版は事実を作れない (03 §4.2 の規則 6。種類 `fact` は
/// `create_when_newer_present = false`)。入れ替えの書き込みで断られる前に、何も書かずに止めるために数える。
fn newer_facts(tx: &evorag_store::ReadTxn<'_>) -> Result<usize, evorag_store::StoreError> {
    let current = evorag_memory::kinds()
        .iter()
        .find(|k| k.name == KIND_FACT)
        .map_or(u32::MAX, |k| k.current_v);
    let mut n = 0;
    tx.for_each(KIND_FACT, |r| {
        match r {
            Read::Ok(e) if e.v > current => n += 1,
            Read::Newer { .. } => n += 1,
            _ => {}
        }
        true
    })?;
    Ok(n)
}

/// 処理する利用者の発話 1 つと、前のふるいの結果。
struct Item {
    turn: Id,
    rev: u32,
    text: String,
    screened: Screened,
}

impl Item {
    fn sent_text(&self) -> Option<&str> {
        match &self.screened {
            Screened::Send(p) => Some(p.text()),
            Screened::Skip(_) => None,
        }
    }
}

/// 1 回の呼び出しの結果。
enum Reply {
    Parsed(ExtractionOutput),
    /// 生成が上限で切れた。
    Truncated,
    /// 型に合わない答えが 2 回続いた。
    Invalid,
    /// 4xx。
    Rejected(LlmError),
    /// 締切切れ・5xx・逐次表示の途中の失敗 (元の失敗の説明)。
    Transient(String),
    /// サーバに繋がらない。
    Unreachable(LlmError),
}

/// 抽出の 1 回の作り直しの環境。
struct Extractor<'a> {
    llm: &'a dyn LlmClient,
    aux_request_extra: &'a RequestExtra,
    slot: Option<u32>,
    system: String,
    key: &'a ForgetKey,
    directives: &'a Directives,
    out: &'a mut FactsRebuilt,
    gate_rejected: BTreeMap<&'static str, usize>,
}

impl Extractor<'_> {
    /// 抽出の要求 (sleep-time の抽出と同じ形: 型縛り・temperature 0・固定の seed・裏のスロット、06 §6)。
    fn request(&self, texts: &[&str]) -> ChatRequest {
        let mut extra = self.aux_request_extra.clone();
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
                ChatMessage::new(Role::System, self.system.clone()),
                ChatMessage::new(Role::User, extraction_input(texts.iter().copied())),
            ],
            generation: Generation {
                temperature: 0.0,
                top_p: 1.0,
                max_tokens: Some(EXTRACT_MAX_TOKENS),
                seed: Some(1),
            },
            id_slot: self.slot,
            model: None,
            extra,
        }
    }

    /// 束を送り、型で読む。型に合わなければ 1 回だけやり直す。
    async fn call(&mut self, texts: &[&str]) -> Reply {
        let req = self.request(texts);
        for _ in 0..2 {
            self.out.requests += 1;
            let c = match crate::probe::run_chat(self.llm, req.clone()).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "fact extraction request failed (rebuild)");
                    return match (Failure::of(&e), e) {
                        (Failure::Unreachable, ChatError::Llm(e)) => Reply::Unreachable(e),
                        (Failure::Rejected, ChatError::Llm(e)) => Reply::Rejected(e),
                        (_, e) => Reply::Transient(e.to_string()),
                    };
                }
            };
            if c.finish_reason.as_deref() == Some("length") {
                return Reply::Truncated;
            }
            if let Some(out) = parse_extraction(&c.text, texts.len()) {
                return Reply::Parsed(out);
            }
        }
        Reply::Invalid
    }

    /// 候補に、属性の一覧の確認と関門 1〜8 を当て、通ったものを統合の出来事にする。
    fn gate(&mut self, items: &[&Item], out: ExtractionOutput) -> Vec<FactEvent> {
        let safety = Safety::bundled();
        let env = GateEnv {
            attributes: attributes(),
            safety: &safety,
            key: self.key,
            directives: self.directives,
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
                    Err(reason) => *self.gate_rejected.entry(reason.as_str()).or_default() += 1,
                }
            }
        }
        if out.out_of_batch > 0 {
            *self.gate_rejected.entry("out_of_batch").or_default() += out.out_of_batch;
        }
        events
    }

    /// 1 つの束を処理する。切れたら半分に分けて送り直し、型が合わない・切れた 1 つの発話の部分は飛ばして数える。
    /// 締切切れと 5xx は待って送り直し、同じ部分で `FAILURE_LIMIT` 回続いたら止める。4xx と繋がらないときも
    /// 止める (`Err`。作り直しは何も書かない、03 §10)。
    async fn batch(&mut self, sent: &[&Item]) -> Result<Vec<FactEvent>, ExtractionStop> {
        let mut events = Vec::new();
        let mut queue: VecDeque<&[&Item]> = VecDeque::from([sent]);
        while let Some(part) = queue.pop_front() {
            let texts: Vec<&str> = part.iter().filter_map(|it| it.sent_text()).collect();
            let mut failures = 0u32;
            loop {
                match self.call(&texts).await {
                    Reply::Parsed(out) => events.extend(self.gate(part, out)),
                    Reply::Truncated if part.len() > 1 => {
                        let (head, tail) = part.split_at(part.len() / 2);
                        queue.push_front(tail);
                        queue.push_front(head);
                    }
                    Reply::Truncated => self.out.skip("truncated", part.len()),
                    Reply::Invalid => self.out.skip("invalid_output", part.len()),
                    Reply::Rejected(e) => return Err(ExtractionStop::Rejected(e)),
                    Reply::Transient(detail) => {
                        failures += 1;
                        if failures < FAILURE_LIMIT {
                            tokio::time::sleep(RETRY_DELAY * failures).await;
                            continue;
                        }
                        return Err(ExtractionStop::Failed {
                            attempts: failures,
                            detail,
                        });
                    }
                    Reply::Unreachable(e) => return Err(ExtractionStop::Unreachable(e)),
                }
                break;
            }
        }
        Ok(events)
    }
}

/// `/tokenize` で数える。使えなければ見積もり、`counting` を見積もりにする (06 §8)。
async fn count_tokens(llm: &dyn LlmClient, text: &str, counting: &mut Counting) -> usize {
    match llm.count_tokens(text).await {
        Ok(n) => n,
        Err(_) => {
            *counting = Counting::Estimated;
            crate::estimate(text)
        }
    }
}

/// 抽出した候補と、進み具合を進める先 (最後の発話)。
pub(super) struct Extracted {
    events: Vec<FactEvent>,
    through: Option<Id>,
}

/// すべての発話から候補を抽出する (何も書かない)。llama-server が失敗したら `Extraction` で止める。
pub(super) async fn extract(
    store: &Store,
    src: &FactsSource,
    checked: &Checked,
    key: &ForgetKey,
    out: &mut FactsRebuilt,
) -> Result<Extracted, RebuildError> {
    let read = |e: &dyn std::fmt::Display| RebuildError::Read(e.to_string());
    let (utterances, directives) = {
        let tx = store.read().map_err(|e| read(&e))?;
        (
            user_utterances_after(&tx, None).map_err(|e| read(&e))?,
            load_directives(&tx).map_err(|e| read(&e))?,
        )
    };
    out.utterances = utterances.len();
    let through = utterances.last().map(|u| u.turn);
    let items: Vec<Item> = utterances
        .into_iter()
        .filter(|u| {
            let keep = !checked.excluded.contains(&u.turn);
            if !keep {
                out.skip("forget_mismatch", 1);
            }
            keep
        })
        .map(|u| Item {
            screened: prefilter(&u.mode, &u.text),
            turn: u.turn,
            rev: u.rev,
            text: u.text,
        })
        .collect();

    let llm = &*src.llm;
    let system = extraction_system(checked.language, attributes());
    let mut counting = Counting::Exact;
    let system_tokens = count_tokens(llm, &system, &mut counting).await;
    let mut tokens = Vec::with_capacity(items.len());
    for it in &items {
        tokens.push(match &it.screened {
            Screened::Send(p) => Some(count_tokens(llm, p.text(), &mut counting).await),
            Screened::Skip(reason) => {
                out.skip(reason.as_str(), 1);
                None
            }
        });
    }
    let budget = utterance_budget(system_tokens, counting);
    if budget == 0 {
        return Err(FactsUnavailable::ContextTooSmall {
            n_ctx: checked.server.n_ctx,
        }
        .into());
    }
    let packing = pack(&tokens, budget);
    out.skip("too_long", packing.too_long.len());
    tracing::info!(
        utterances = items.len(),
        batches = packing.batches.len(),
        budget,
        ?counting,
        "fact extraction planned (rebuild)"
    );
    let server = &checked.server;
    let mut ex = Extractor {
        llm,
        aux_request_extra: &src.aux_request_extra,
        // 裏の処理は最後のスロット (06 §4)。
        slot: (server.total_slots > 1).then(|| server.total_slots - 1),
        system,
        key,
        directives: &directives,
        out,
        gate_rejected: BTreeMap::new(),
    };
    let mut events = Vec::new();
    for batch in &packing.batches {
        let sent: Vec<&Item> = batch.send.iter().map(|&i| &items[i]).collect();
        if sent.is_empty() {
            continue;
        }
        events.extend(ex.batch(&sent).await.map_err(RebuildError::Extraction)?);
    }
    let gate = std::mem::take(&mut ex.gate_rejected);
    tracing::info!(
        utterances = ex.out.utterances,
        requests = ex.out.requests,
        skipped = ?ex.out.skipped,
        ?gate,
        candidates = events.len(),
        "fact extraction pass (rebuild)"
    );
    ex.out.blocked += gate
        .get(GateReason::Forgotten.as_str())
        .copied()
        .unwrap_or(0);
    Ok(Extracted { events, through })
}

/// 事実を入れ替える: 候補と検証済みの訂正を畳み直し、事実と進み具合を 1 つの依頼で書く。
pub(super) fn swap(
    store: &Store,
    handle: &WriterHandle,
    key: &ForgetKey,
    checked: &Checked,
    extracted: Extracted,
    (clock, ids): (&dyn Clock, &dyn IdGen),
    out: &mut FactsRebuilt,
) -> Result<(), RebuildError> {
    let read = |e: &dyn std::fmt::Display| RebuildError::Read(e.to_string());
    let fail = |e: &dyn std::fmt::Display| RebuildError::Write(e.to_string());
    let now = clock.now();
    let model_key = checked.server.model_key.as_str();
    let safety = Safety::bundled();
    let ctx = FactCtx {
        key,
        attributes: attributes(),
        safety: &safety,
        ids,
        now,
    };
    let tx = store.read().map_err(|e| read(&e))?;
    let before = evorag_memory::load_facts(&tx).map_err(|e| read(&e))?;
    out.kept_unknown = before.unknown;
    // 残っている検証済みの訂正を、候補と一緒に入力の順で当て直す (検証し直さない、04 §7.5)。
    let corrections = load_corrections(&tx).map_err(|e| read(&e))?.verified;
    out.corrections = corrections.len();
    let mut events = extracted.events;
    events.extend(corrections.into_iter().map(FactEvent::Correction));
    let (mut ops, report) = evorag_memory::rebuild(&tx, &ctx, events).map_err(|e| read(&e))?;
    out.blocked += report.blocked;
    out.removed = evorag_memory::deleted_ids(&ops).len();
    out.facts = ops
        .iter()
        .filter(|op| matches!(op, WriteOp::Put(e) if e.kind == KIND_FACT))
        .count();
    // 進み具合を最後の発話まで進める (戻さない)。次の起動の抽出は、それより後の発話だけを処理する。
    if let Some(through) = extracted.through {
        let progress = load_extract_progress(&tx).map_err(|e| read(&e))?;
        if progress.through.is_none_or(|t| t < through) {
            ops.push(
                progress
                    .advance(through, model_key, ids, now)
                    .map_err(|e| fail(&e))?,
            );
        }
    }
    drop(tx);
    tracing::info!(
        removed = out.removed,
        facts = out.facts,
        corrections = out.corrections,
        added = report.added,
        replaced = report.replaced,
        contested = report.contested,
        blocked = report.blocked,
        stale_source = report.stale_source,
        value_rejected = report.value_rejected,
        "facts rebuilt"
    );
    if ops.is_empty() {
        return Ok(());
    }
    handle
        .submit_wait(WriteRequest {
            ops,
            durability: Durability::Immediate,
            priority: Priority::Background,
        })
        .map(|_| ())
        .map_err(|e| fail(&e))
}

/// 値の鍵 (確かめで無かったときは、書き手を作ってから作る)。
pub(super) fn key(
    store: &Store,
    handle: &WriterHandle,
    checked: &Checked,
) -> Result<ForgetKey, RebuildError> {
    if let Some(k) = &checked.key {
        return Ok(k.clone());
    }
    match crate::forget_key::ensure(store, Some(handle)).map_err(RebuildError::Write)? {
        FactsKey::Ready { key, .. } => Ok(key),
        FactsKey::Disabled(p) => Err(FactsUnavailable::Key(p).into()),
    }
}
