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

/// 記録したモデル (`model_binding`)。
#[derive(Debug, Clone)]
pub struct Binding {
    pub envelope: Envelope,
    pub body: ModelBindingBody,
}

/// 役割ごとの記録したモデル。同じ役割が 2 件以上あれば `updated_at` の新しいものを採り、残りは
/// `duplicates` に入れる (書くときに消す、03 §3.3)。
#[derive(Debug, Default)]
pub struct Bindings {
    pub by_role: std::collections::BTreeMap<String, Binding>,
    pub duplicates: Vec<Envelope>,
}

pub fn load_bindings(tx: &ReadTxn<'_>) -> Result<Bindings, MemoryError> {
    let mut out = Bindings::default();
    tx.for_each(KIND_MODEL_BINDING, |r| {
        if let Read::Ok(envelope) = r
            && let Ok(body) = envelope.body_as::<ModelBindingBody>()
        {
            let b = Binding { envelope, body };
            match out.by_role.remove(&b.body.role) {
                Some(old) if old.envelope.updated_at > b.envelope.updated_at => {
                    out.duplicates.push(b.envelope);
                    out.by_role.insert(old.body.role.clone(), old);
                }
                Some(old) => {
                    out.duplicates.push(old.envelope);
                    out.by_role.insert(b.body.role.clone(), b);
                }
                None => {
                    out.by_role.insert(b.body.role.clone(), b);
                }
            }
        }
        true
    })?;
    Ok(out)
}

/// 保存した点検の結果 (役割・model_key・`build_info` が合うもののうち、新しいもの、06 §3.3)。
/// 埋め込みの記録の本体だけ (ベクトルは読まない。起動のたびに全件のベクトルを読まないため、NFR-101)。
pub fn embedding_bodies(tx: &ReadTxn<'_>) -> Result<Vec<(Id, EmbeddingBody)>, MemoryError> {
    let mut out = Vec::new();
    tx.for_each(KIND_EMBEDDING, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<EmbeddingBody>()
        {
            out.push((e.id, b));
        }
        true
    })?;
    Ok(out)
}

/// 埋め込みのベクトル (`blobs` の part 0)。読めなければ `None`。
pub fn embedding_vector(tx: &ReadTxn<'_>, id: Id) -> Result<Option<Vec<f32>>, MemoryError> {
    Ok(tx.blob(id, 0)?.map(|b| {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }))
}

/// チャンクの、あるモデルの埋め込みの記録の ID (同じチャンクを書き直すときに前のものを消す)。
pub fn embeddings_of_chunk(
    tx: &ReadTxn<'_>,
    chunk: Id,
    model_key: &str,
) -> Result<Vec<Id>, MemoryError> {
    let mut out = Vec::new();
    for (id, _) in tx.links_to(chunk, REL_EMBEDDING_OF)? {
        if let Some(Read::Ok(e)) = tx.get(KIND_EMBEDDING, id)?
            && e.body_as::<EmbeddingBody>()
                .is_ok_and(|b| b.model_key == model_key)
        {
            out.push(id);
        }
    }
    Ok(out)
}

/// 埋め込みのモデルの埋め込み (記録の ID、本体、ベクトル)。`model_key` が `None` ならすべて。
pub fn embeddings_of_model(
    tx: &ReadTxn<'_>,
    model_key: Option<&str>,
) -> Result<Vec<(Id, EmbeddingBody, Vec<f32>)>, MemoryError> {
    let mut found = Vec::new();
    tx.for_each(KIND_EMBEDDING, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<EmbeddingBody>()
            && model_key.is_none_or(|k| k == b.model_key)
        {
            found.push((e.id, b));
        }
        true
    })?;
    let mut out = Vec::with_capacity(found.len());
    for (id, b) in found {
        let v: Vec<f32> = tx
            .blob(id, 0)?
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        out.push((id, b, v));
    }
    Ok(out)
}

/// ターンのチャンク (05 §2.4)。
pub fn chunks_of(tx: &ReadTxn<'_>, turn: Id) -> Result<Vec<(Id, ChunkBody)>, MemoryError> {
    let mut out = Vec::new();
    for (id, _) in tx.links_to(turn, REL_CHUNK_OF)? {
        if let Some(Read::Ok(e)) = tx.get(KIND_CHUNK, id)?
            && let Ok(b) = e.body_as::<ChunkBody>()
        {
            out.push((id, b));
        }
    }
    out.sort_by_key(|(_, b)| b.start);
    Ok(out)
}

/// 応答のターンへの、想起への操作 (05 §5.2)。
pub fn feedback_for_answer(
    tx: &ReadTxn<'_>,
    answer: Id,
) -> Result<Vec<(Envelope, RecallFeedbackBody)>, MemoryError> {
    let mut out = Vec::new();
    for (id, _) in tx.links_to(answer, REL_FEEDBACK_ANSWER)? {
        if let Some(Read::Ok(e)) = tx.get(KIND_RECALL_FEEDBACK, id)?
            && let Ok(b) = e.body_as::<RecallFeedbackBody>()
        {
            out.push((e, b));
        }
    }
    Ok(out)
}

/// リランカーと対象の種類の、いちばん新しい版のしきい値の記録 (05 §3.7)。
pub fn load_threshold(
    tx: &ReadTxn<'_>,
    reranker_key: &str,
    target: &str,
) -> Result<Option<(Envelope, ThresholdBody)>, MemoryError> {
    let mut best: Option<(Envelope, ThresholdBody)> = None;
    tx.for_each(KIND_THRESHOLD, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<ThresholdBody>()
            && b.reranker_key == reranker_key
            && b.target == target
            && best.as_ref().is_none_or(|(_, old)| old.version < b.version)
        {
            best = Some((e, b));
        }
        true
    })?;
    Ok(best)
}

pub fn load_probe(
    tx: &ReadTxn<'_>,
    role: &str,
    model_key: &str,
    build_info: &str,
) -> Result<Option<(Envelope, ModelProbeBody)>, MemoryError> {
    let mut best: Option<(Envelope, ModelProbeBody)> = None;
    tx.for_each(KIND_MODEL_PROBE, |r| {
        if let Read::Ok(e) = r
            && let Ok(b) = e.body_as::<ModelProbeBody>()
            && b.role == role
            && b.model_key == model_key
            && b.build_info == build_info
            && best
                .as_ref()
                .is_none_or(|(old, _)| old.updated_at <= e.updated_at)
        {
            best = Some((e, b));
        }
        true
    })?;
    Ok(best)
}
