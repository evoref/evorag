//! 要求・応答・イベントの型。

use serde::{Deserialize, Serialize};

/// 発言の役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// 生成の設定 (07 §4 の `llm.chat.generation`)。ターンに記録する (06 §7)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub temperature: f64,
    pub top_p: f64,
    /// 生成するトークンの上限 (`None` はサーバに任せる)。
    pub max_tokens: Option<u32>,
    pub seed: Option<u64>,
}

impl Default for Generation {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.95,
            max_tokens: None,
            seed: None,
        }
    }
}

/// chat の要求。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub generation: Generation,
    /// 使うスロット (06 §4)。
    pub id_slot: Option<u32>,
    /// ルーターモードのときのモデルの名前。
    pub model: Option<String>,
}

/// トークンの使用量 (サーバが返したときだけ)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

/// 逐次表示の 1 つの出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// 応答の本文の断片。
    Delta(String),
    /// 思考の部分の断片 (思考を出すモデルだけ。本文とは分けて扱う)。
    Reasoning(String),
    /// 生成が終わった。
    Done {
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// `/v1/models` の `data[0].meta` (06 §3、§5)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMeta {
    pub n_params: Option<u64>,
    pub size: Option<u64>,
    pub n_vocab: Option<u64>,
    pub n_ctx_train: Option<u64>,
}

/// 起動時に確かめたサーバの能力 (06 §3、FR-501)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub build_info: String,
    pub model_path: String,
    /// `/v1/models` の `data[0].id`。
    pub model_name: String,
    pub total_slots: u32,
    /// スロットごとの文脈の長さ (`default_generation_settings.n_ctx`)。
    pub n_ctx: u32,
    pub meta: ModelMeta,
    /// 06 §5。
    pub model_key: String,
}
