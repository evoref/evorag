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

/// セッションのターンと、この版では読めなかったターンの数。
#[derive(Debug, Clone, Default)]
pub struct SessionTurns {
    /// 読めたターン (元の順)。
    pub turns: Vec<Turn>,
    /// 新しい版が書いた、この版では読めないターンの数 (03 §4.2 の規則 11)。記録は消さずに残っている。
    pub unreadable: usize,
}

/// セッションのターンを、元の順 (ID の順 = 書いた順) に。
///
/// この版では読めないターンは読み飛ばし、ほかのターンを返す (03 §4.2 の規則 11)。
/// 読めなかった数も要るときは [`session_turns_counted`] を使う。
pub fn session_turns(tx: &ReadTxn<'_>, session: Id) -> Result<Vec<Turn>, MemoryError> {
    Ok(session_turns_counted(tx, session)?.turns)
}

/// セッションのターン (元の順) と、この版では読めなかったターンの数 (AC-FR-215-3)。
///
/// 読めないターンは、封筒を読めないもの (新しい版の形) と、封筒は読めるが本体をこの版の型として
/// 読めないもの (知らない列挙の値など)。どちらも読み飛ばして数えるだけで、消さず、書き換えない。
/// 壊れた記録は数えない (書き込みのときに隔離される、03 §8)。
pub fn session_turns_counted(tx: &ReadTxn<'_>, session: Id) -> Result<SessionTurns, MemoryError> {
    let mut out = SessionTurns::default();
    for (id, _) in tx.links_to(session, REL_IN_SESSION)? {
        match tx.get(KIND_TURN, id)? {
            Some(Read::Ok(envelope)) => match envelope.body_as() {
                Ok(body) => out.turns.push(Turn { envelope, body }),
                Err(_) => out.unreadable += 1,
            },
            Some(Read::Newer { .. }) => out.unreadable += 1,
            Some(Read::Corrupt { .. }) | None => {}
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

/// 読んだ設定 (`setting`)。
#[derive(Debug, Clone)]
pub struct Setting {
    pub envelope: Envelope,
    pub body: SettingBody,
}

/// 設定を名前ごとに読む。同じ名前が 2 件以上あれば `updated_at` の新しいものを採り、残りは
/// `duplicates` に入れる (書くときに消す、03 §3.3)。
#[derive(Debug, Default)]
pub struct Settings {
    pub by_name: std::collections::BTreeMap<String, Setting>,
    pub duplicates: Vec<Envelope>,
}

pub fn load_settings(tx: &ReadTxn<'_>) -> Result<Settings, MemoryError> {
    let mut out = Settings::default();
    tx.for_each(KIND_SETTING, |r| {
        if let Read::Ok(envelope) = r
            && let Ok(body) = envelope.body_as::<SettingBody>()
        {
            let s = Setting { envelope, body };
            match out.by_name.remove(&s.body.name) {
                Some(old) if old.envelope.updated_at > s.envelope.updated_at => {
                    out.duplicates.push(s.envelope);
                    out.by_name.insert(old.body.name.clone(), old);
                }
                Some(old) => {
                    out.duplicates.push(old.envelope);
                    out.by_name.insert(s.body.name.clone(), s);
                }
                None => {
                    out.by_name.insert(s.body.name.clone(), s);
                }
            }
        }
        true
    })?;
    Ok(out)
}
