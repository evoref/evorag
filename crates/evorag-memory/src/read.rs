//! 会話の層の読み出し。

use evorag_store::key::OwnedPart;
use evorag_store::{Envelope, LookupRange, Read, ReadTxn, StoreError};
use evorag_types::Id;

use crate::kinds::*;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Body(#[from] evorag_store::BodyError),
    #[error("record {0} not found")]
    NotFound(Id),
}

/// 読んだセッション。
#[derive(Debug, Clone)]
pub struct Session {
    pub envelope: Envelope,
    pub body: SessionBody,
}

/// 読んだターン。
#[derive(Debug, Clone)]
pub struct Turn {
    pub envelope: Envelope,
    pub body: TurnBody,
}

fn ok(r: Option<Read>) -> Option<Envelope> {
    match r {
        Some(Read::Ok(e)) => Some(e),
        // 読めない記録は読み飛ばす (壊れた記録は書き込みのときに隔離される、03 §8)。
        _ => None,
    }
}

pub fn load_session(tx: &ReadTxn<'_>, id: Id) -> Result<Session, MemoryError> {
    let envelope = ok(tx.get(KIND_SESSION, id)?).ok_or(MemoryError::NotFound(id))?;
    let body = envelope.body_as()?;
    Ok(Session { envelope, body })
}

/// セッションを、更新の新しい順に `limit` 件まで。
pub fn list_sessions(tx: &ReadTxn<'_>, limit: usize) -> Result<Vec<Session>, MemoryError> {
    let entries = tx.lookup(
        KIND_SESSION,
        LOOKUP_UPDATED_AT,
        &LookupRange::Between {
            from: None,
            to: None,
        },
        true,
        Some(limit),
    )?;
    let mut out = Vec::new();
    for e in entries {
        // 副索引の値が記録の更新時刻と合わない項目は、付け替え損ねた古い項目なので読み飛ばす。
        if let Ok(s) = load_session(tx, e.id)
            && e.value == OwnedPart::Int(s.envelope.updated_at.as_millis())
        {
            out.push(s);
        }
    }
    Ok(out)
}

/// セッションのターンを、元の順 (ID の順 = 書いた順) に。
pub fn session_turns(tx: &ReadTxn<'_>, session: Id) -> Result<Vec<Turn>, MemoryError> {
    let mut out = Vec::new();
    for (id, _) in tx.links_to(session, REL_IN_SESSION)? {
        if let Some(envelope) = ok(tx.get(KIND_TURN, id)?) {
            let body = envelope.body_as()?;
            out.push(Turn { envelope, body });
        }
    }
    Ok(out)
}

/// 応答を書いていない発話 (起動したときに「応答なし」として示す、AC-FR-201-1)。
pub fn pending_user_turns(tx: &ReadTxn<'_>, session: Id) -> Result<Vec<Turn>, MemoryError> {
    Ok(session_turns(tx, session)?
        .into_iter()
        .filter(|t| t.body.role == TurnRole::User && t.body.state == TurnState::Pending)
        .collect())
}
