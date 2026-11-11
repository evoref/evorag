//! embed・rerank の役割の接続と照合 (06 §3・§3.1、FR-512、ADR-0018)。
//!
//! - 設定していない役割は `Unset` (その役割は無いものとして縮退する、05 §3.5)。
//! - 繋がらなければ `Waiting`。使わずに、ときどき試す (llama-server を同時に起動して、読み込みが
//!   evorag の起動に間に合わなかったときのため、06 §3 の表)。
//! - 記録と同じモデルなら `Ready`。記録が無ければ、繋がっているモデルを記録して `Ready` にする。
//! - 記録と違えば `Mismatch`。使わない (応答のモデルと違い、起動は止めない)。

use std::sync::Arc;

use evorag_llm::LlmClient;
use evorag_memory::{Bindings, ModelOrigin};
use evorag_types::api::{ModelRoleInfo, RoleStatus};

/// 繋がらない役割を試し直す間隔 (06 §3 の表)。
pub(crate) const RETRY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub(crate) enum RoleState {
    Unset,
    Waiting,
    Ready(ModelOrigin),
    Mismatch(ModelOrigin),
}

pub(crate) struct AuxRole {
    pub role: &'static str,
    pub client: Option<Arc<dyn LlmClient>>,
    /// データ根に記録したモデル。
    pub recorded: Option<ModelOrigin>,
    pub state: RoleState,
}

impl AuxRole {
    pub fn new(
        role: &'static str,
        client: Option<Arc<dyn LlmClient>>,
        bindings: &Bindings,
    ) -> Self {
        let state = if client.is_some() {
            RoleState::Waiting
        } else {
            RoleState::Unset
        };
        Self {
            role,
            client,
            recorded: bindings.by_role.get(role).map(|b| b.body.current.clone()),
            state,
        }
    }

    pub fn info(&self) -> ModelRoleInfo {
        let pair = |o: &ModelOrigin| (o.file_name.clone(), o.model_key.clone());
        let (connected, status) = match &self.state {
            RoleState::Unset => (None, RoleStatus::Unset),
            RoleState::Waiting => (None, RoleStatus::Waiting),
            RoleState::Ready(o) => (Some(pair(o)), RoleStatus::Ready),
            RoleState::Mismatch(o) => (Some(pair(o)), RoleStatus::Mismatch),
        };
        ModelRoleInfo {
            role: self.role.to_owned(),
            recorded: self.recorded.as_ref().map(pair),
            connected,
            status,
        }
    }

    /// 使える (記録と同じモデルに繋がっている) ときだけ、その接続。
    pub fn ready_client(&self) -> Option<Arc<dyn LlmClient>> {
        match self.state {
            RoleState::Ready(_) => self.client.clone(),
            _ => None,
        }
    }
}

/// 確かめた結果。
pub(crate) enum Checked {
    Waiting,
    /// 使える。`record` なら、まだ記録していないので記録する。
    Ready {
        origin: ModelOrigin,
        record: bool,
    },
    Mismatch(ModelOrigin),
}

/// 繋がっているモデルを、記録と比べる。
pub(crate) async fn check(client: &dyn LlmClient, recorded: Option<&ModelOrigin>) -> Checked {
    if client.health().await.is_err() {
        return Checked::Waiting;
    }
    let Ok(server) = client.server_info().await else {
        return Checked::Waiting;
    };
    let o = crate::origin(&server);
    match recorded {
        None => Checked::Ready {
            origin: o,
            record: true,
        },
        Some(r) if r.model_key == o.model_key => Checked::Ready {
            origin: o,
            record: false,
        },
        Some(_) => Checked::Mismatch(o),
    }
}
