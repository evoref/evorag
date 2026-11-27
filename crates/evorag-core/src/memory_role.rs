//! memory の役割 (06 §2.1、FR-515、v0.5): 区切りの呼び出し (B2) を、どのサーバとモデルで出すか。
//!
//! - `llm.memory` を設定しなければ、chat のサーバ・モデル・記録で呼ぶ (既定。memory の記録は作らない)。
//! - 設定したときは、embed・rerank と同じく記録と照合し (roles.rs)、点検 (06 §3.3) に通れば使う。
//! - 繋がらない・記録と違う・点検に通らないときは、chat のサーバとモデルで呼ぶ (縮退)。B2 の呼ぶ側は、
//!   台帳の `inputs` に `role_fallback` と `role_model_key` を書く (04 §5.2.1)。
//! - 呼ぶ先 (縮退の間は chat) の点検に通らないときは、区切りの呼び出しを止める。会話と想起は続く。
//! - 呼ぶ先の点検がまだのときは、済むまで呼ばない (点検は裏の処理の最初の手順で流れる。ノートの鍵と同じ)。

use std::sync::Arc;

use evorag_llm::{LlmClient, LlmError, RequestExtra};
use evorag_types::api::{Event, MemoryFallback, MemoryState};

use crate::Inner;
use crate::roles::RoleState;

/// 設定 `llm.memory` (06 §2.1、07 §4.2)。既定は設定なし (chat と同じサーバとモデル)。
#[derive(Clone, Default)]
pub struct MemoryConfig {
    /// memory の役割の llama-server。`None` なら chat のサーバとモデルで呼ぶ。
    pub client: Option<Arc<dyn LlmClient>>,
    /// 設定 `llm.memory.request_extra` (memory の呼び出しに足す JSON)。memory を設定しないときと縮退の間は、
    /// chat の補助の呼び出しの指定 (`llm.chat.aux_request_extra`) が効く。
    pub request_extra: RequestExtra,
}

/// 区切りの呼び出しを出す先 (B2 の呼ぶ側が使う)。
///
/// **約束**: 呼ぶ側は、区切りのまとまりを呼ぶ前ごとに [`MemoryCall::verify`] で、呼ぶ先のモデルが選んだ時と
/// 同じかを確かめる (動いている間に llama-server のモデルが差し替わったときに、違うモデルで台帳を作らない
/// ため、06 §2.1 の細目)。違えば呼ばずに [`crate::Core::memory_route`] で選び直す。
#[derive(Clone)]
pub struct MemoryCall {
    pub client: Arc<dyn LlmClient>,
    /// 実際に呼ぶモデル (台帳の `model_key`)。
    pub model_key: String,
    /// 縮退で chat を使うときの理由 (台帳の `inputs.role_fallback` は `is_some()`)。
    pub fallback: Option<MemoryFallback>,
    /// 意図した memory の役割の model_key (台帳の `inputs.role_model_key`)。memory を設定しないときは chat の
    /// model_key。縮退の間は memory の記録の model_key (まだ記録していなければ `None`)。
    pub role_model_key: Option<String>,
    /// 要求に足す JSON (設定の指定と、点検が通った思考を切る指定、06 §6)。
    pub extra: RequestExtra,
    /// 使うスロット (スロットが 2 つ以上なら最後のスロット、06 §4)。
    pub slot: Option<u32>,
}

impl MemoryCall {
    /// 縮退で chat を使うか (台帳の `inputs.role_fallback`)。
    #[must_use]
    pub fn is_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// 呼ぶ先の `/props` の model_key が、選んだ時の model_key と同じかを確かめる (06 §2.1 の細目)。
    ///
    /// # Errors
    ///
    /// 繋がらない ([`VerifyError::Unreachable`]。memory の役割なら [`crate::Core::memory_unreachable`] を呼ぶ)、
    /// モデルが違う ([`VerifyError::Changed`])。
    pub async fn verify(&self) -> Result<(), VerifyError> {
        let server = self
            .client
            .server_info()
            .await
            .map_err(VerifyError::Unreachable)?;
        if server.model_key == self.model_key {
            Ok(())
        } else {
            Err(VerifyError::Changed {
                connected: server.model_key,
            })
        }
    }
}

/// [`MemoryCall::verify`] の失敗。
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// 呼ぶ先に繋がらない。
    #[error("the segment call target is not reachable: {0}")]
    Unreachable(LlmError),
    /// 呼ぶ先のモデルが、選んだ時と違う。
    #[error("the segment call target now runs another model ({connected})")]
    Changed { connected: String },
}

impl std::fmt::Debug for MemoryCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCall")
            .field("model_key", &self.model_key)
            .field("fallback", &self.fallback)
            .field("role_model_key", &self.role_model_key)
            .field("extra", &self.extra)
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

/// 区切りの呼び出しを出せるか。
#[derive(Debug, Clone)]
pub enum MemoryRoute {
    /// この先に出す。
    Call(MemoryCall),
    /// 呼ぶ先のモデルの点検がまだ (済むまで呼ばない)。
    Pending,
    /// 呼ぶ先のモデルが点検に通らない (chat でも呼べない) ので止める。会話は続く。
    Stopped,
}

/// 点検の結果から見た、型縛りの補助の呼び出しに使えるか。
enum Probed {
    /// まだ点検していない。
    Pending,
    /// 型縛りの出力が型に合い、思考を切れる。
    Usable(Box<evorag_memory::ModelProbeBody>),
    /// 点検に通らない (型縛りの出力が空・型に合わない・思考を切れない・点検が失敗した)。
    Unusable,
}

fn probed(inner: &Inner, role: &str, model_key: &str, build_info: &str) -> Probed {
    let found = inner
        .store
        .read()
        .ok()
        .and_then(|r| evorag_memory::load_probe(&r, role, model_key, build_info).ok())
        .flatten();
    match found {
        // 点検し直す結果 (要求の付け足しを直した後など) は、点検し直すまで使わない (06 §3.3 の例外)。
        None => Probed::Pending,
        Some((_, p)) if !crate::probe_fresh(inner, &p) => Probed::Pending,
        Some((_, p)) if p.structured_ok == Some(true) && p.thinking_off != Some(false) => {
            Probed::Usable(Box::new(p))
        }
        Some(_) => Probed::Unusable,
    }
}

/// 裏の処理のスロット (スロットが 2 つ以上なら最後のスロット、06 §4)。
fn last_slot(slots: u32) -> Option<u32> {
    (slots > 1).then(|| slots - 1)
}

/// chat のサーバとモデルで呼ぶ (既定、または縮退)。
fn via_chat(
    inner: &Inner,
    fallback: Option<MemoryFallback>,
    role_model_key: Option<String>,
) -> MemoryRoute {
    let (client, server, _) = inner.conn();
    match probed(
        inner,
        evorag_memory::ROLE_CHAT,
        &server.model_key,
        &server.build_info,
    ) {
        Probed::Pending => MemoryRoute::Pending,
        Probed::Unusable => MemoryRoute::Stopped,
        Probed::Usable(_) => MemoryRoute::Call(MemoryCall {
            client,
            model_key: server.model_key.clone(),
            fallback,
            role_model_key,
            extra: inner.aux_extra(),
            slot: last_slot(server.total_slots),
        }),
    }
}

/// 今の状態と、区切りの呼び出しを出す先。
pub(crate) fn resolve(inner: &Inner) -> (MemoryState, MemoryRoute) {
    let memory = inner
        .aux
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|a| a.role == evorag_memory::ROLE_MEMORY)
        .and_then(|a| {
            let client = a.client.clone()?;
            Some((
                a.state.clone(),
                client,
                a.recorded.as_ref().map(|o| o.model_key.clone()),
                a.slots,
            ))
        });
    let Some((state, client, recorded, slots)) = memory else {
        let key = inner.conn().1.model_key;
        let route = via_chat(inner, None, Some(key));
        let state = match route {
            MemoryRoute::Stopped => MemoryState::Stopped,
            _ => MemoryState::SameAsChat,
        };
        return (state, route);
    };
    let fallback = |reason: MemoryFallback| {
        let route = via_chat(inner, Some(reason), recorded.clone());
        let state = match route {
            MemoryRoute::Stopped => MemoryState::Stopped,
            _ => MemoryState::Fallback(reason),
        };
        (state, route)
    };
    match state {
        RoleState::Ready(o) => match probed(
            inner,
            evorag_memory::ROLE_MEMORY,
            &o.model_key,
            &o.build_info,
        ) {
            Probed::Pending => (MemoryState::Own, MemoryRoute::Pending),
            Probed::Unusable => fallback(MemoryFallback::ProbeFailed),
            Probed::Usable(p) => (
                MemoryState::Own,
                MemoryRoute::Call(MemoryCall {
                    client,
                    model_key: o.model_key.clone(),
                    fallback: None,
                    role_model_key: Some(o.model_key),
                    extra: crate::probe::aux_extra_with(&inner.memory_request_extra, Some(&p)),
                    slot: last_slot(slots),
                }),
            ),
        },
        RoleState::Mismatch(_) => fallback(MemoryFallback::Mismatch),
        RoleState::Waiting | RoleState::Unset => fallback(MemoryFallback::Waiting),
    }
}

/// 状態を求め直し、変わっていれば知らせる (状態の行、06 §2.1)。
pub(crate) fn refresh(inner: &Inner) {
    refresh_after_probe(inner, false);
}

/// [`refresh`] の、点検の後の形。`announced` は、その点検で使えない機能を既に知らせた (画面は、点検に通らない
/// ことによる縮退と停止の知らせを重ねない)。
pub(crate) fn refresh_after_probe(inner: &Inner, announced: bool) {
    let (state, _) = resolve(inner);
    let changed = {
        let mut shown = inner.memory_shown.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *shown != Some(state);
        *shown = Some(state);
        changed
    };
    if changed {
        tracing::info!(?state, "memory role changed");
        crate::emit(
            inner,
            Event::MemoryRole {
                state,
                by_probe: announced,
            },
        );
    }
}

/// 設定した memory のサーバに繋がらなかった (B2 の呼び出しが接続の失敗で終わった)。繋がるまで chat で代行し、
/// 30 秒ごとに試し直す (06 §3 の表)。memory を設定していなければ何もしない。
pub(crate) fn mark_unreachable(inner: &Arc<Inner>) {
    let changed = {
        let mut aux = inner.aux.lock().unwrap_or_else(|e| e.into_inner());
        match aux
            .iter_mut()
            .find(|a| a.role == evorag_memory::ROLE_MEMORY && a.client.is_some())
        {
            Some(a) if matches!(a.state, RoleState::Ready(_)) => {
                a.state = RoleState::Waiting;
                true
            }
            _ => false,
        }
    };
    if changed {
        refresh(inner);
        crate::ensure_role_retry(inner);
    }
}
