//! llama-server のクライアント (06)。TLS を持たない (02 §4.3 の R3)。
//!
//! - `LlmClient`: コアから見た接続の口 (02 §6)。実際の HTTP の実装 (`HttpClient`) と、記録した
//!   応答を流す再生の実装 (`replay::ReplayClient`、09 §3.3) がある。
//! - 逐次表示の `ChatStream` を落とすと、接続を閉じる。llama-server は接続が閉じると生成を
//!   取り消す (06 §4、S-02)。

mod endpoint;
mod http;
mod model_key;
pub mod replay;
mod sse;
mod types;

use std::future::Future;
use std::pin::Pin;

pub use endpoint::Endpoint;
pub use http::{HttpClient, RetryPolicy};
pub use model_key::{file_name, model_key};
pub use types::{
    ChatEvent, ChatMessage, ChatRequest, Generation, ModelMeta, RequestExtra, Role, ServerInfo,
    Usage,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    #[error("invalid endpoint: {0}")]
    BadEndpoint(String),
    /// ループバック以外の接続先 (06 §9)。
    #[error("connecting to `{0}` is not allowed (set llm.allow_remote = true and use https)")]
    RemoteNotAllowed(String),
    /// 繋がらない (再試行の後)。
    #[error("cannot connect to llama-server: {0}")]
    Connect(String),
    #[error("llama-server returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// 応答の形が想定と違う。
    #[error("unexpected response from llama-server: {0}")]
    Protocol(String),
    /// 逐次表示の途中で llama-server がエラーを返した。
    #[error("llama-server reported an error: {0}")]
    Server(String),
    /// 逐次表示の途中で切れた (再試行しない、06 §7)。
    #[error("the response stream was cut: {0}")]
    Cut(String),
}

/// 逐次表示の応答。落とすと接続を閉じる。
pub trait ChatStream: Send {
    /// 次の出来事。`Done` の後と、エラーの後は `None`。
    fn next(&mut self) -> BoxFuture<'_, Option<Result<ChatEvent, LlmError>>>;
}

/// llama-server への接続の口 (02 §6)。
pub trait LlmClient: Send + Sync {
    /// `/health` (06 §3 の 1)。
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>>;
    /// `/props` と `/v1/models` (06 §3 の 2・3)。
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>>;
    /// `/tokenize` で数える (06 §8)。
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>>;
    /// `/v1/chat/completions` を逐次表示で始める。接続の失敗と 5xx は始まる前だけ再試行する。
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>>;
    /// `/v1/embeddings` (embed の役割、06 §2、v0.2)。入力の順に、1 つずつのベクトルを返す。
    /// 既定は「使えない」(chat のサーバは 501 で断る、S-02)。
    fn embed<'a>(&'a self, inputs: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, LlmError>> {
        let _ = inputs;
        Box::pin(async { Err(not_supported("embeddings")) })
    }
    /// `/v1/rerank` (rerank の役割、06 §2、v0.2)。文書の順に、生のスコアを返す (0〜1 に写さない、S-02)。
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        let _ = (query, documents);
        Box::pin(async { Err(not_supported("rerank")) })
    }
}

/// 役割に合わないサーバへの要求 (llama-server の 501 と同じに扱う)。
fn not_supported(what: &str) -> LlmError {
    LlmError::Http {
        status: 501,
        body: format!("{what} is not supported by this server"),
    }
}
