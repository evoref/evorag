//! HTTP の実装 (reqwest、TLS なし、プロキシなし)。

use std::time::Duration;

use serde_json::{Value, json};

use crate::endpoint::Endpoint;
use crate::model_key::model_key;
use crate::sse::SseParser;
use crate::types::{ChatEvent, ChatRequest, ModelMeta, ServerInfo};
use crate::{BoxFuture, ChatStream, LlmClient, LlmError};

/// 再試行の待ち時間 (06 §7: 0.5 s、1 s、2 s の 3 回)。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub delays: Vec<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            delays: vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ],
        }
    }
}

pub struct HttpClient {
    endpoint: Endpoint,
    http: reqwest::Client,
    retry: RetryPolicy,
}

impl HttpClient {
    pub fn new(endpoint: Endpoint, retry: RetryPolicy) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            // 外へ通信するのは設定した llama-server だけ (P2)。環境変数のプロキシも使わない。
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| LlmError::BadEndpoint(e.to_string()))?;
        Ok(Self {
            endpoint,
            http,
            retry,
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.get(self.endpoint.join(path)))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.post(self.endpoint.join(path)))
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.endpoint.api_key() {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    /// 送って、2xx の応答を返す。接続の失敗と 5xx だけを再試行する (06 §7、FR-507)。
    async fn send(
        &self,
        make: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, LlmError> {
        let mut attempt = 0;
        loop {
            let result = make().send().await;
            let retryable = match &result {
                Err(e) => e.is_connect() || e.is_timeout(),
                Ok(r) => r.status().is_server_error(),
            };
            if retryable && attempt < self.retry.delays.len() {
                tokio::time::sleep(self.retry.delays[attempt]).await;
                attempt += 1;
                continue;
            }
            return match result {
                Err(e) => Err(LlmError::Connect(e.to_string())),
                Ok(r) if r.status().is_success() => Ok(r),
                Ok(r) => {
                    let status = r.status().as_u16();
                    let body = r.text().await.unwrap_or_default();
                    Err(LlmError::Http {
                        status,
                        body: body.chars().take(500).collect(),
                    })
                }
            };
        }
    }

    async fn json(&self, make: impl Fn() -> reqwest::RequestBuilder) -> Result<Value, LlmError> {
        let r = self.send(make).await?;
        let bytes = r
            .bytes()
            .await
            .map_err(|e| LlmError::Protocol(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| LlmError::Protocol(format!("bad JSON: {e}")))
    }
}

/// `/props` と `/v1/models` の JSON から能力を読む (06 §3)。
pub(crate) fn parse_server_info(props: &Value, models: &Value) -> Result<ServerInfo, LlmError> {
    let missing = |what: &str| LlmError::Protocol(format!("missing {what}"));
    let n_ctx = props
        .pointer("/default_generation_settings/n_ctx")
        .and_then(Value::as_u64)
        .ok_or_else(|| missing("default_generation_settings.n_ctx in /props"))?;
    let total_slots = props
        .get("total_slots")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let model_path = props
        .get("model_path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let build_info = props
        .get("build_info")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let first = models
        .pointer("/data/0")
        .ok_or_else(|| missing("data[0] in /v1/models"))?;
    let model_name = first
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let m = first.get("meta");
    let num = |k: &str| m.and_then(|m| m.get(k)).and_then(Value::as_u64);
    let meta = ModelMeta {
        n_params: num("n_params"),
        size: num("size"),
        n_vocab: num("n_vocab"),
        n_ctx_train: num("n_ctx_train"),
    };
    let key = model_key(&model_path, &meta, &model_name);
    Ok(ServerInfo {
        build_info,
        model_path,
        model_name,
        total_slots: u32::try_from(total_slots).unwrap_or(u32::MAX),
        n_ctx: u32::try_from(n_ctx).unwrap_or(u32::MAX),
        meta,
        model_key: key,
    })
}

pub(crate) fn chat_body(req: &ChatRequest) -> Value {
    let g = &req.generation;
    let mut body = json!({
        "messages": req.messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "temperature": g.temperature,
        "top_p": g.top_p,
    });
    let o = body.as_object_mut().expect("object");
    if let Some(m) = g.max_tokens {
        o.insert("max_tokens".into(), m.into());
    }
    if let Some(s) = g.seed {
        o.insert("seed".into(), s.into());
    }
    if let Some(s) = req.id_slot {
        o.insert("id_slot".into(), s.into());
    }
    if let Some(m) = &req.model {
        o.insert("model".into(), m.clone().into());
    }
    body
}

impl LlmClient for HttpClient {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async move {
            // /health は API キーなしで呼べる。
            self.send(|| self.http.get(self.endpoint.join("/health")))
                .await
                .map(drop)
        })
    }

    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        Box::pin(async move {
            let props = self.json(|| self.get("/props")).await?;
            let models = self.json(|| self.get("/v1/models")).await?;
            parse_server_info(&props, &models)
        })
    }

    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        Box::pin(async move {
            let v = self
                .json(|| self.post("/tokenize").json(&json!({"content": text})))
                .await?;
            v.get("tokens")
                .and_then(Value::as_array)
                .map(Vec::len)
                .ok_or_else(|| LlmError::Protocol("missing tokens in /tokenize".into()))
        })
    }

    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        Box::pin(async move {
            let body = chat_body(&req);
            let resp = self
                .send(|| self.post("/v1/chat/completions").json(&body))
                .await?;
            Ok(Box::new(HttpChatStream {
                resp: Some(resp),
                parser: SseParser::default(),
            }) as Box<dyn ChatStream>)
        })
    }
}

struct HttpChatStream {
    /// `None` は終わった (接続は閉じた)。
    resp: Option<reqwest::Response>,
    parser: SseParser,
}

impl ChatStream for HttpChatStream {
    fn next(&mut self) -> BoxFuture<'_, Option<Result<ChatEvent, LlmError>>> {
        Box::pin(async move {
            loop {
                if let Some(ev) = self.parser.next_event() {
                    if ev.is_err() || self.parser.is_done() {
                        // 終わった・失敗した。応答を落として接続を閉じる。
                        self.resp = None;
                    }
                    return Some(ev);
                }
                let resp = self.resp.as_mut()?;
                match resp.chunk().await {
                    Ok(Some(bytes)) => self.parser.push(&bytes),
                    Ok(None) => {
                        self.resp = None;
                        return Some(Err(LlmError::Cut("the stream ended before [DONE]".into())));
                    }
                    Err(e) => {
                        self.resp = None;
                        return Some(Err(LlmError::Cut(e.to_string())));
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, Generation, Role};

    #[test]
    fn server_info_is_read_from_props_and_models() {
        let props = json!({
            "build_info": "b6000-abc",
            "model_path": "/m/Qwen.gguf",
            "total_slots": 2,
            "default_generation_settings": {"n_ctx": 8192}
        });
        let models = json!({"data": [{"id": "qwen", "meta": {"n_params": 1, "size": 2, "n_vocab": 3, "n_ctx_train": 4}}]});
        let info = parse_server_info(&props, &models).unwrap();
        assert_eq!(info.n_ctx, 8192);
        assert_eq!(info.total_slots, 2);
        assert_eq!(info.model_name, "qwen");
        assert_eq!(info.meta.n_ctx_train, Some(4));
        assert_eq!(info.model_key, model_key("Qwen.gguf", &info.meta, "qwen"));
        assert!(parse_server_info(&json!({}), &models).is_err());
    }

    #[test]
    fn chat_body_has_stream_and_optional_fields_only_when_set() {
        let mut req = ChatRequest {
            messages: vec![ChatMessage::new(Role::User, "hi")],
            generation: Generation::default(),
            id_slot: None,
            model: None,
        };
        let b = chat_body(&req);
        assert_eq!(b["stream"], true);
        assert_eq!(b["messages"][0]["role"], "user");
        assert!(b.get("id_slot").is_none() && b.get("model").is_none());
        req.id_slot = Some(0);
        req.model = Some("m".into());
        let b = chat_body(&req);
        assert_eq!(b["id_slot"], 0);
        assert_eq!(b["model"], "m");
    }
}
