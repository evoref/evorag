//! 記録した llama-server の応答を流す実装 (09 §3.3)。試験と、llama-server の無い開発で使う。
//!
//! 記録 (cassette) は JSON で、`tests/llm-cassettes/` に置く。作った会話だけを使う (11 §9.7)。
//!
//! ```json
//! {
//!   "props": { "...": "/props の応答" },
//!   "models": { "...": "/v1/models の応答" },
//!   "health": true,
//!   "chats": [
//!     { "events": [ {"delta": "こん"}, {"wait_ms": 10}, {"delta": "にちは"}, {"done": {"finish_reason": "stop"}} ] },
//!     { "events": [ {"delta": "途中"}, {"cut": "connection reset"} ] },
//!     { "events": [ {"delta": "長い"}, {"hang": true} ] },
//!     { "http_error": {"status": 503, "body": "busy"} },
//!     { "connect_error": "refused" }
//!   ]
//! }
//! ```
//!
//! `chats` は要求の順に 1 つずつ使う。`hang` は落とされるまで待ち続ける (中断の試験)。
//! トークンの数は、文字の数で数える (決定的にするため)。
//!
//! embed と rerank の役割のサーバ (v0.2) は、次を足す。どちらも本文から決定的に作る。
//!
//! ```json
//! { "embed": {"dim": 8} }       // /v1/embeddings: 文字の bi-gram から作った、長さ 1 のベクトル
//! { "rerank": true }            // /v1/rerank: 問いと文書で共通の文字の数 − 10 を生のスコアにする
//! ```

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::http::parse_server_info;
use crate::types::{ChatEvent, ChatRequest, ServerInfo, Usage};
use crate::{BoxFuture, ChatStream, LlmClient, LlmError};

#[derive(Debug, Clone, Deserialize)]
pub struct Cassette {
    pub props: Value,
    pub models: Value,
    #[serde(default = "yes")]
    pub health: bool,
    #[serde(default)]
    pub chats: Vec<ChatScript>,
    /// `/v1/embeddings` に答える (embed の役割)。無ければ 501。
    #[serde(default)]
    pub embed: Option<EmbedScript>,
    /// `/v1/rerank` に答える (rerank の役割)。無ければ 501。
    #[serde(default)]
    pub rerank: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedScript {
    pub dim: usize,
}

/// 文字の bi-gram を `dim` 個の箱に数えて、長さ 1 にしたベクトル (決定的)。
pub fn toy_embedding(text: &str, dim: usize) -> Vec<f32> {
    let n = dim.max(1);
    let mut v = vec![0f32; n];
    let chars: Vec<char> = text.chars().collect();
    for w in chars.windows(2) {
        let h = (u32::from(w[0]).wrapping_mul(31)).wrapping_add(u32::from(w[1]));
        v[h as usize % n] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// 問いと文書で共通の文字の数 − 10 (生のスコアの形、決定的)。
pub fn toy_rerank(query: &str, document: &str) -> f32 {
    let q: std::collections::HashSet<char> = query.chars().collect();
    let d: std::collections::HashSet<char> = document.chars().collect();
    q.intersection(&d).count() as f32 - 10.0
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ChatScript {
    Events { events: Vec<Step> },
    HttpError { http_error: HttpError },
    ConnectError { connect_error: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpError {
    pub status: u16,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Delta { delta: String },
    Reasoning { reasoning: String },
    Wait { wait_ms: u64 },
    Done { done: DoneStep },
    Cut { cut: String },
    ServerError { server_error: String },
    Hang { hang: bool },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DoneStep {
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, Default)]
struct State {
    chats: VecDeque<ChatScript>,
    requests: Vec<ChatRequest>,
    /// 終わる前に落とされた (接続を閉じた) 応答の数。
    dropped_early: usize,
    /// `/v1/embeddings` と `/v1/rerank` を呼んだ回数。
    embed_calls: usize,
    rerank_calls: usize,
}

/// 記録を流す `LlmClient`。複製すると同じ状態を共有する。
#[derive(Clone)]
pub struct ReplayClient {
    cassette: Arc<Cassette>,
    state: Arc<Mutex<State>>,
}

impl ReplayClient {
    pub fn new(cassette: Cassette) -> Self {
        let state = State {
            chats: cassette.chats.iter().cloned().collect(),
            ..State::default()
        };
        Self {
            cassette: Arc::new(cassette),
            state: Arc::new(Mutex::new(state)),
        }
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        Ok(Self::new(serde_json::from_str(text)?))
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text).map_err(std::io::Error::other)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 受け取った chat の要求 (順に)。
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.lock().requests.clone()
    }

    /// 終わる前に落とされた応答の数 (中断で接続を閉じた数)。
    pub fn dropped_early(&self) -> usize {
        self.lock().dropped_early
    }

    /// `/v1/embeddings` と `/v1/rerank` を呼んだ回数。
    pub fn aux_calls(&self) -> (usize, usize) {
        let st = self.lock();
        (st.embed_calls, st.rerank_calls)
    }

    /// まだ使っていない chat の記録の数。
    pub fn remaining(&self) -> usize {
        self.lock().chats.len()
    }
}

impl LlmClient for ReplayClient {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async move {
            if self.cassette.health {
                Ok(())
            } else {
                Err(LlmError::Connect("replay: server is down".into()))
            }
        })
    }

    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        Box::pin(async move {
            if !self.cassette.health {
                return Err(LlmError::Connect("replay: server is down".into()));
            }
            parse_server_info(&self.cassette.props, &self.cassette.models)
        })
    }

    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        Box::pin(async move { Ok(text.chars().count()) })
    }

    fn embed<'a>(&'a self, inputs: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, LlmError>> {
        Box::pin(async move {
            if !self.cassette.health {
                return Err(LlmError::Connect("replay: server is down".into()));
            }
            self.lock().embed_calls += 1;
            match &self.cassette.embed {
                Some(e) => Ok(inputs.iter().map(|t| toy_embedding(t, e.dim)).collect()),
                None => Err(LlmError::Http {
                    status: 501,
                    body: "replay: no embeddings".into(),
                }),
            }
        })
    }

    fn rerank<'a>(
        &'a self,
        query: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        Box::pin(async move {
            if !self.cassette.health {
                return Err(LlmError::Connect("replay: server is down".into()));
            }
            self.lock().rerank_calls += 1;
            if self.cassette.rerank {
                Ok(documents.iter().map(|d| toy_rerank(query, d)).collect())
            } else {
                Err(LlmError::Http {
                    status: 501,
                    body: "replay: no rerank".into(),
                })
            }
        })
    }

    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        Box::pin(async move {
            let script = {
                let mut st = self.lock();
                st.requests.push(req);
                st.chats.pop_front()
            };
            match script {
                None => Err(LlmError::Protocol("replay: no recorded chat left".into())),
                Some(ChatScript::ConnectError { connect_error }) => {
                    Err(LlmError::Connect(connect_error))
                }
                Some(ChatScript::HttpError { http_error }) => Err(LlmError::Http {
                    status: http_error.status,
                    body: http_error.body,
                }),
                Some(ChatScript::Events { events }) => Ok(Box::new(ReplayStream {
                    steps: events.into(),
                    finished: false,
                    state: self.state.clone(),
                })
                    as Box<dyn ChatStream>),
            }
        })
    }
}

struct ReplayStream {
    steps: VecDeque<Step>,
    finished: bool,
    state: Arc<Mutex<State>>,
}

impl Drop for ReplayStream {
    fn drop(&mut self) {
        if !self.finished {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .dropped_early += 1;
        }
    }
}

impl ChatStream for ReplayStream {
    fn next(&mut self) -> BoxFuture<'_, Option<Result<ChatEvent, LlmError>>> {
        Box::pin(async move {
            loop {
                if self.finished {
                    return None;
                }
                let Some(step) = self.steps.pop_front() else {
                    self.finished = true;
                    return Some(Err(LlmError::Cut("the stream ended before [DONE]".into())));
                };
                match step {
                    Step::Delta { delta } => return Some(Ok(ChatEvent::Delta(delta))),
                    Step::Reasoning { reasoning } => {
                        return Some(Ok(ChatEvent::Reasoning(reasoning)));
                    }
                    Step::Wait { wait_ms } => {
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                    Step::Done { done } => {
                        self.finished = true;
                        let usage = (done.prompt_tokens.is_some()
                            || done.completion_tokens.is_some())
                        .then(|| Usage {
                            prompt_tokens: done.prompt_tokens.unwrap_or(0),
                            completion_tokens: done.completion_tokens.unwrap_or(0),
                        });
                        return Some(Ok(ChatEvent::Done {
                            finish_reason: done.finish_reason,
                            usage,
                        }));
                    }
                    Step::Cut { cut } => {
                        self.finished = true;
                        return Some(Err(LlmError::Cut(cut)));
                    }
                    Step::ServerError { server_error } => {
                        self.finished = true;
                        return Some(Err(LlmError::Server(server_error)));
                    }
                    Step::Hang { .. } => std::future::pending::<()>().await,
                }
            }
        })
    }
}
