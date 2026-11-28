//! 区切り 1 つを B2 で呼び、台帳を書くまで (04 §5.2.1)。止め方 ([`Yield`]) と、要求の逐次表示の受け取り
//! ([`stream`]) と、呼び出しの進め方 ([`super::flow`]) の配線。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evorag_llm::{ChatEvent, ChatRequest, LlmClient, LlmError};
use evorag_memory::b2::{
    self, Call, CallerInputs, Candidate, CountMismatch, Counting, Input, Layout, LedgerMeta, Pair,
    Plan, StreamEnd,
};
use evorag_memory::corrections::CORRECTION_V1;
use evorag_memory::{LedgerTokens, Through};
use evorag_types::Id;
use tokio::sync::futures::Notified;

use super::flow::{self, Ended, Finish, Flow, Health, Next};
use super::save::{Saved, save};
use super::snapshot::{OwnedText, Work};
use super::{HaltReason, Target, state};
use crate::{Inner, MemoryCall};

/// 1 回の B2 の要求の締切 (04 §5.2.1 の「失敗の扱い」の締切切れ)。生成の上限 (2,048 トークン) を遅いモデル
/// (12B 級で約 6 tok/s) で出し切る時間 (約 340 秒) に、プロンプトの処理と余裕を足した値 (暫定)。
const DEADLINE: Duration = Duration::from_secs(600);
/// 組み立てた利用者のメッセージを数え直して計画を直す回数の上限 (予算は直すたびに小さくなるので、ふつうは数回で
/// 終わる)。
const REFIT_ROUNDS: usize = 64;

/// 利用者の操作を待つ口。一度鳴ったら、以後の待ちはすぐに終わる。
pub(super) struct Yield<'a> {
    notified: Pin<Box<Notified<'a>>>,
    fired: bool,
}

impl<'a> Yield<'a> {
    pub(super) fn new(inner: &'a Inner) -> Self {
        let mut notified = Box::pin(inner.activity.touched_notify().notified());
        notified.as_mut().enable();
        Self {
            notified,
            fired: false,
        }
    }

    /// `fut` を流し、利用者が操作したら落として `None` を返す。
    pub(super) async fn run<T>(&mut self, fut: impl Future<Output = T>) -> Option<T> {
        if self.fired {
            return None;
        }
        tokio::select! {
            biased;
            () = self.notified.as_mut() => {
                self.fired = true;
                None
            }
            v = fut => Some(v),
        }
    }
}

/// 1 回の要求の終わり方。
enum Streamed {
    /// 生成を終えた (`length` なら上限で切れた)。
    Finished {
        text: String,
        length: bool,
        usage: Option<evorag_llm::Usage>,
    },
    /// 利用者の操作で止めた (ストリームを落として接続を閉じた)。受け取ったところまでの本文。
    Stopped { text: String },
    /// 失敗した。
    Failed(Failure),
}

/// 失敗の種類 (04 §5.2.1 の「失敗の扱い」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Failure {
    /// 4xx (408・429 を除く。要求そのものが受け付けられない)。
    Rejected,
    /// 締切切れ・5xx・408・429・途中の切断・ルーターのモデルの読み込みの失敗 (書かずに次の機会に送り直す)。
    Transient,
    /// 繋がらない (回数に入れず、書かずに待つ)。
    Unreachable,
    /// 構成の誤り (接続先の形・ループバック以外・ルーターのモデルの名前が無い)。
    Misconfigured,
}

impl Failure {
    pub(super) fn of(e: &LlmError) -> Self {
        match e {
            LlmError::Connect(_) => Self::Unreachable,
            LlmError::Http {
                status: 408 | 429, ..
            } => Self::Transient,
            LlmError::Http { status, .. } if (400..500).contains(status) => Self::Rejected,
            LlmError::BadEndpoint(_)
            | LlmError::RemoteNotAllowed(_)
            | LlmError::RouterNeedsModel { .. } => Self::Misconfigured,
            _ => Self::Transient,
        }
    }
}

/// 要求を逐次表示で送り、終わるまで受け取る。利用者が操作したら、ストリームを落として止める (06 §4)。
async fn stream(llm: &dyn LlmClient, req: ChatRequest, stop: &mut Yield<'_>) -> Streamed {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let opened = stop
        .run(tokio::time::timeout_at(deadline, llm.chat(req)))
        .await;
    let mut s = match opened {
        None => {
            return Streamed::Stopped {
                text: String::new(),
            };
        }
        Some(Err(_)) => {
            tracing::warn!("a segment call timed out before it started");
            return Streamed::Failed(Failure::Transient);
        }
        Some(Ok(Err(e))) => {
            tracing::warn!(error = %e, "a segment call could not start");
            return Streamed::Failed(Failure::of(&e));
        }
        Some(Ok(Ok(s))) => s,
    };
    let mut text = String::new();
    loop {
        let ev = stop.run(tokio::time::timeout_at(deadline, s.next())).await;
        match ev {
            // 止める: ストリームを落として接続を閉じる (llama-server は生成を取り消す)。
            None => {
                drop(s);
                return Streamed::Stopped { text };
            }
            Some(Err(_)) => {
                tracing::warn!("a segment call timed out");
                return Streamed::Failed(Failure::Transient);
            }
            Some(Ok(None)) => return Streamed::Failed(Failure::Transient),
            Some(Ok(Some(Err(e)))) => {
                tracing::warn!(error = %e, "a segment call failed while streaming");
                return Streamed::Failed(Failure::of(&e));
            }
            Some(Ok(Some(Ok(ChatEvent::Delta(d))))) => text.push_str(&d),
            Some(Ok(Some(Ok(ChatEvent::Reasoning(_))))) => {}
            Some(Ok(Some(Ok(ChatEvent::Done {
                finish_reason,
                usage,
            })))) => {
                return Streamed::Finished {
                    text,
                    length: finish_reason.as_deref() == Some("length"),
                    usage,
                };
            }
        }
    }
}

/// 区切り 1 つを処理した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Done {
    /// 台帳を書いた。
    Written,
    /// 書かなかった (組み立てられない・計画の誤り・書く前の確かめに合わない・前の `partial` の方が多い)。次へ
    /// 進む。
    Skipped,
    /// 1 文も送れなかった (数えて飛ばす)。
    Unsent,
    /// 4xx で書かなかった (続けての失敗に数えた)。次へ進む。
    Rejected,
    /// 応答のスロットが処理中 (待たせる)。
    Busy,
    /// 利用者の操作で止めた (止めたところまでを書いた後)。手順を終える。
    Stopped,
    /// 締切切れ・5xx で、書かずに次の機会に送り直す。手順を終える。
    Retry,
    /// 繋がらない。手順を終える。
    Unreachable,
    /// 続けて失敗したか構成の誤りなので、呼び出しを止めた。
    Halted,
}

/// 計画を立てられなかった理由。
enum Unfit {
    /// 利用者の操作で止めた。
    Stopped,
    /// 計画の誤り (数の並びが行と合わない。起きない)。
    Plan(CountMismatch),
}

impl From<CountMismatch> for Unfit {
    fn from(e: CountMismatch) -> Self {
        Self::Plan(e)
    }
}

/// 計画から呼び出しを作り、台帳の `through` を区切りのもの ([`Work::through`]) にする。
fn render(layout: &Layout, plan: &Plan, through: Option<Through>) -> Call {
    let mut call = layout.render(plan);
    if let Some(t) = through {
        call.through = t;
    }
    call
}

/// 呼び出しの出力を数えて、台帳に書くまで。
pub(super) struct Calling<'a> {
    pub(super) inner: &'a Arc<Inner>,
    pub(super) call: &'a MemoryCall,
    pub(super) target: &'a Target,
    pub(super) forgets_seen: Option<Id>,
}

impl Calling<'_> {
    async fn count(
        &self,
        text: &str,
        counting: &mut Counting,
        stop: &mut Yield<'_>,
    ) -> Option<usize> {
        let llm = &*self.call.client;
        let n = stop
            .run(self.inner.tokens.count(llm, &self.call.model_key, text))
            .await?;
        Some(n.unwrap_or_else(|| {
            *counting = Counting::Estimated;
            crate::estimate(text)
        }))
    }

    /// 計画を予算に収め (組み立てた利用者のメッセージを `/tokenize` で数えられるなら確かめて直し)、呼び出しを作る。
    /// 1 文も送れなければ `Ok(None)`。
    async fn fit(
        &self,
        layout: &Layout,
        through: Option<Through>,
        stop: &mut Yield<'_>,
    ) -> Result<Option<(Plan, Call)>, Unfit> {
        if layout.is_empty() {
            let counts = vec![0; layout.pieces().len()];
            let plan = layout.plan(&counts, 0)?;
            let call = render(layout, &plan, through);
            return Ok(Some((plan, call)));
        }
        let mut counting = Counting::Exact;
        let system = self
            .count(layout.system(), &mut counting, stop)
            .await
            .ok_or(Unfit::Stopped)?;
        let pieces: Vec<String> = layout.pieces().map(str::to_owned).collect();
        let mut counts = Vec::with_capacity(pieces.len());
        for p in &pieces {
            counts.push(
                self.count(p, &mut counting, stop)
                    .await
                    .ok_or(Unfit::Stopped)?,
            );
        }
        let budget = b2::user_budget(system, counting);
        let mut plan = layout.plan(&counts, budget)?;
        let mut call = render(layout, &plan, through);
        if counting == Counting::Exact {
            let mut i = 0;
            let mut rounds = 0;
            while i < call.requests.len() && rounds < REFIT_ROUNDS {
                rounds += 1;
                let mut exact = Counting::Exact;
                let user = call.requests[i].user.clone();
                let measured = self
                    .count(&user, &mut exact, stop)
                    .await
                    .ok_or(Unfit::Stopped)?;
                if exact != Counting::Exact {
                    break;
                }
                match layout.refit(&plan, i, &counts, budget, measured)? {
                    Some((p, _)) => {
                        plan = p;
                        call = render(layout, &plan, through);
                    }
                    None => i += 1,
                }
            }
        }
        if plan.nothing_sent() {
            return Ok(None);
        }
        Ok(Some((plan, call)))
    }

    /// 続けての失敗を数え、止めるなら止める (止めたら真)。
    fn count_health(&self, health: Health) -> bool {
        let mut st = state(self.inner);
        if flow::tally(&mut st.consecutive, health) {
            tracing::warn!(
                failures = st.consecutive,
                "segment calls kept failing; they are stopped until the call target or its settings change"
            );
            st.halted = Some((self.target.halt_key(), HaltReason::Failures));
            true
        } else {
            false
        }
    }

    /// 区切り 1 つを呼び、台帳を書く (04 §5.2.1 の「呼び出しの進め方」)。`build_info` は、呼ぶ前に確かめた呼ぶ先の
    /// `build_info` (台帳に書く)。
    pub(super) async fn segment(
        &self,
        work: &Work,
        build_info: &str,
        stop: &mut Yield<'_>,
    ) -> Done {
        let inner = self.inner;
        let pairs: Vec<Pair<'_>> = work
            .pairs
            .iter()
            .map(|p| Pair {
                utterance: p.utterance.view(),
                reply: p.reply.as_ref().map(OwnedText::view),
            })
            .collect();
        if pairs.is_empty() {
            tracing::info!("no pair of a segment can be read; it is not called");
            return Done::Skipped;
        }
        let candidates: Vec<Candidate<'_>> = work
            .candidates
            .iter()
            .map(|c| Candidate {
                turn: c.turn,
                pattern: c.pattern,
                new: c.new.clone(),
                old: c.old.clone(),
                previous: c.previous.as_ref().map(OwnedText::view),
            })
            .collect();
        let input = Input {
            language: inner.language,
            pairs: &pairs,
            candidates: &candidates,
            correction_version: CORRECTION_V1,
        };
        let layout = match Layout::new(&input) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "a segment call could not be laid out; it is skipped");
                return Done::Skipped;
            }
        };
        let through = work.through();
        let (mut plan, mut call) = match self.fit(&layout, through, stop).await {
            Err(Unfit::Stopped) => return Done::Stopped,
            Err(Unfit::Plan(e)) => {
                tracing::warn!(error = %e, "a segment call could not be planned; it is skipped");
                return Done::Skipped;
            }
            Ok(None) => {
                tracing::warn!(
                    sentences = layout.sentence_count(),
                    "no sentence of a segment fits the request budget; no ledger is written"
                );
                state(inner).unsent.insert(work.id, work.body.pairs.len());
                return Done::Unsent;
            }
            Ok(Some(x)) => x,
        };
        state(inner).unsent.remove(&work.id);
        if !call.requests.is_empty() && crate::notes::response_slot_busy(&*self.call.client).await {
            return Done::Busy;
        }

        let started = Instant::now();
        let mut tokens = LedgerTokens::default();
        let mut flow = Flow::default();
        let mut next = flow.start(call.requests.len());
        let (stopped, gave_up) = loop {
            let i = match next {
                Next::Send(i) => i,
                Next::Halve(i) => {
                    next = if let Some((p, _)) = plan.halve(i, &layout) {
                        tracing::info!(
                            "a segment call was cut at the output limit; it is sent in halves"
                        );
                        plan = p;
                        call = render(&layout, &plan, through);
                        flow.halved(call.requests.len())
                    } else {
                        flow.unsplittable(call.requests.len())
                    };
                    continue;
                }
                Next::Finish(Finish::Write { stopped, gave_up }) => break (stopped, gave_up),
                Next::Finish(Finish::Stopped) => return Done::Stopped,
                Next::Finish(Finish::Retry) => return Done::Retry,
                Next::Finish(Finish::Unreachable) => return Done::Unreachable,
                Next::Finish(Finish::Rejected) => {
                    // 設定の誤りが多く、設定を直しても呼ぶ先が同じなら呼び直されないので、台帳を書かない。
                    return if self.count_health(Health::Error) {
                        Done::Halted
                    } else {
                        Done::Rejected
                    };
                }
                Next::Finish(Finish::Misconfigured) => {
                    tracing::warn!(
                        "the segment call target is misconfigured; calls are stopped until it changes"
                    );
                    state(inner).halted = Some((self.target.halt_key(), HaltReason::Misconfigured));
                    return Done::Halted;
                }
            };
            let request = &call.requests[i];
            let req = super::chat_request(request, &self.call.extra, self.call.slot);
            let ended = match stream(&*self.call.client, req, stop).await {
                Streamed::Stopped { text } => Ended::Read {
                    reading: b2::read_output(request, &text, StreamEnd::Stopped),
                    stopped: true,
                },
                Streamed::Finished {
                    text,
                    length,
                    usage,
                } => {
                    if let Some(u) = usage {
                        let to_u32 = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
                        tokens.input = tokens.input.saturating_add(to_u32(u.prompt_tokens));
                        tokens.output = tokens.output.saturating_add(to_u32(u.completion_tokens));
                    }
                    let end = if length {
                        StreamEnd::Length
                    } else {
                        StreamEnd::Finished
                    };
                    Ended::Read {
                        reading: b2::read_output(request, &text, end),
                        stopped: false,
                    }
                }
                Streamed::Failed(Failure::Transient) => {
                    let mut st = state(inner);
                    let n = st.server_errors.entry(work.id).or_default();
                    *n += 1;
                    Ended::Transient { count: *n }
                }
                Streamed::Failed(Failure::Rejected) => Ended::Rejected,
                Streamed::Failed(Failure::Unreachable) => Ended::Unreachable,
                Streamed::Failed(Failure::Misconfigured) => Ended::Misconfigured,
            };
            next = flow.on(ended, call.requests.len());
        };
        tokens.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let results = flow.into_results();
        let health = flow::health(&results, gave_up);
        if health == Health::Error && self.count_health(health) {
            // 続けて失敗した区切りには `failed` の台帳を書かずに止める (04 §5.2.1 の「続けて失敗したとき」)。
            return Done::Halted;
        }
        if health == Health::Answered {
            flow::tally(&mut state(inner).consecutive, health);
        }
        // 締切切れと 5xx で送り直した回数 (諦めた呼び出しの最後の失敗は、送り直していない)。
        let failures = state(inner)
            .server_errors
            .get(&work.id)
            .copied()
            .unwrap_or(0);
        let attempts = failures.saturating_sub(u32::from(gave_up));
        let caller = CallerInputs {
            forgets_seen: self.forgets_seen,
            role_fallback: self.call.is_fallback(),
            role_model_key: self.call.role_model_key.clone(),
        };
        let meta = LedgerMeta {
            segment: work.id,
            model_key: &self.target.model_key,
            build_info: Some(build_info).filter(|b| !b.is_empty()),
            caller: &caller,
            attempts,
            tokens,
        };
        let body = match b2::ledger_body(&call, &results, &meta) {
            Ok(b) => b,
            Err(b2::AssembleError::NoResult) if stopped => return Done::Stopped,
            Err(e) => {
                tracing::warn!(error = %e, "a ledger could not be assembled");
                return Done::Skipped;
            }
        };
        let first = work.body.pairs.first().copied();
        let saved = save(inner, body, first, self.target.clone()).await;
        state(inner).server_errors.remove(&work.id);
        match (saved, stopped) {
            (_, true) => Done::Stopped,
            (Saved::Written, false) => Done::Written,
            (Saved::Discarded | Saved::Kept, false) => Done::Skipped,
        }
    }
}
