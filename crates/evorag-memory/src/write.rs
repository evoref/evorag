//! 応答の経路での書き込み (04 §3)。操作の列を組み立てるだけで、書くのは書き手。

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, Envelope, WriteOp};
use evorag_types::{Clock, Id, IdGen, Timestamp};

use crate::kinds::*;

/// 書き込みの操作を組み立てる。
///
/// セッションの封筒を受け取る関数には、読み直した最新の封筒を渡す (一覧の副索引を付け替えるため。
/// 古い封筒を渡すと古い副索引の項目が残る。読む側は、それを読み飛ばす)。
pub struct Writes<'a> {
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdGen,
}

/// 応答のターンの中身。
#[derive(Debug, Clone)]
pub struct AssistantTurn {
    pub state: TurnState,
    pub text: String,
    pub model_key: Option<String>,
    pub generation: Option<GenerationRecord>,
    pub attached: Vec<Id>,
    pub error: Option<String>,
}

fn envelope<T: serde::Serialize>(
    id: Id,
    kind: &str,
    now: Timestamp,
    body: &T,
) -> Result<Envelope, BodyError> {
    let mut e = Envelope::new(id, kind, 1, now, "");
    e.set_body(body)?;
    Ok(e)
}

fn touch_session(session: &Envelope, now: Timestamp, ops: &mut Vec<WriteOp>) {
    // 一覧の並びの副索引を、新しい更新時刻に付け替える。
    ops.push(WriteOp::DeleteLookup {
        kind: KIND_SESSION.into(),
        field: LOOKUP_UPDATED_AT.into(),
        value: OwnedPart::Int(session.updated_at.as_millis()),
        id: session.id,
    });
    let mut s = session.clone();
    s.updated_at = now;
    ops.push(WriteOp::PutLookup {
        kind: KIND_SESSION.into(),
        field: LOOKUP_UPDATED_AT.into(),
        value: OwnedPart::Int(now.as_millis()),
        id: s.id,
    });
    ops.push(WriteOp::Put(s));
}

impl Writes<'_> {
    /// 新しいセッション。
    pub fn create_session(&self, title: &str) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let now = self.clock.now();
        let e = envelope(
            id,
            KIND_SESSION,
            now,
            &SessionBody {
                title: title.to_owned(),
                mode: "chat".into(),
                state: SessionState::Active,
            },
        )?;
        Ok((
            id,
            vec![
                WriteOp::Put(e),
                WriteOp::PutLookup {
                    kind: KIND_SESSION.into(),
                    field: LOOKUP_UPDATED_AT.into(),
                    value: OwnedPart::Int(now.as_millis()),
                    id,
                },
            ],
        ))
    }

    /// セッションの題名を変える。
    pub fn rename_session(
        &self,
        session: &Envelope,
        title: &str,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let mut body: SessionBody = session.body_as()?;
        body.title = title.to_owned();
        let mut e = session.clone();
        e.set_body(&body)?;
        let mut ops = Vec::new();
        touch_session(&e, self.clock.now(), &mut ops);
        Ok(ops)
    }

    /// 発話のターン (状態「応答待ち」)。LLM に送る前に書く (04 §3 の 1、FR-201)。
    pub fn user_turn(
        &self,
        session: &Envelope,
        text: &str,
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let now = self.clock.now();
        let e = envelope(
            id,
            KIND_TURN,
            now,
            &TurnBody {
                session: session.id,
                role: TurnRole::User,
                state: TurnState::Pending,
                text: text.to_owned(),
                model_key: None,
                generation: None,
                attached: Vec::new(),
                reply_to: None,
                error: None,
            },
        )?;
        let mut ops = vec![
            WriteOp::Put(e),
            WriteOp::PutLink {
                from: id,
                rel: REL_IN_SESSION.into(),
                to: session.id,
                attrs: Vec::new(),
            },
        ];
        touch_session(session, now, &mut ops);
        Ok((id, ops))
    }

    /// 応答のターン (完了・中断・失敗、04 §3 の 3)。発話のターンを「確定」にする。
    pub fn assistant_turn(
        &self,
        session: &Envelope,
        user_turn: &Envelope,
        turn: AssistantTurn,
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let now = self.clock.now();
        let e = envelope(
            id,
            KIND_TURN,
            now,
            &TurnBody {
                session: session.id,
                role: TurnRole::Assistant,
                state: turn.state,
                text: turn.text,
                model_key: turn.model_key,
                generation: turn.generation,
                attached: turn.attached,
                reply_to: Some(user_turn.id),
                error: turn.error,
            },
        )?;
        let mut ops = vec![
            WriteOp::Put(e),
            WriteOp::PutLink {
                from: id,
                rel: REL_IN_SESSION.into(),
                to: session.id,
                attrs: Vec::new(),
            },
            WriteOp::PutLink {
                from: id,
                rel: REL_REPLY_TO.into(),
                to: user_turn.id,
                attrs: Vec::new(),
            },
        ];
        let mut user: TurnBody = user_turn.body_as()?;
        if user.state == TurnState::Pending {
            user.state = TurnState::Confirmed;
            let mut u = user_turn.clone();
            u.set_body(&user)?;
            u.updated_at = now;
            ops.push(WriteOp::Put(u));
        }
        touch_session(session, now, &mut ops);
        Ok((id, ops))
    }

    /// 信号 (評価・再生成、13 §2、FR-311)。
    pub fn signal(
        &self,
        kind: SignalKind,
        target: Id,
        reason: Option<String>,
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let e = envelope(
            id,
            KIND_SIGNAL,
            self.clock.now(),
            &SignalBody {
                kind,
                target,
                reason,
            },
        )?;
        Ok((
            id,
            vec![
                WriteOp::Put(e),
                WriteOp::PutLink {
                    from: id,
                    rel: REL_TARGET.into(),
                    to: target,
                    attrs: Vec::new(),
                },
            ],
        ))
    }
}

/// セッションを消す操作 (04 §7、FR-309 の DB の分)。ターン、ターンを対象にした信号、関係、
/// 副索引、セッションを 1 つの依頼で消す。ジャーナルの行と語彙の索引から消すのは、それぞれを
/// 持つ層が足す。
pub fn delete_session(
    tx: &evorag_store::ReadTxn<'_>,
    session: &Envelope,
) -> Result<Vec<WriteOp>, crate::MemoryError> {
    let mut ops = Vec::new();
    for (turn, _) in tx.links_to(session.id, REL_IN_SESSION)? {
        for (signal, _) in tx.links_to(turn, REL_TARGET)? {
            ops.push(WriteOp::Delete {
                kind: KIND_SIGNAL.into(),
                id: signal,
            });
            ops.push(WriteOp::DeleteLinksOf(signal));
        }
        ops.push(WriteOp::Delete {
            kind: KIND_TURN.into(),
            id: turn,
        });
        ops.push(WriteOp::DeleteLinksOf(turn));
    }
    ops.push(WriteOp::DeleteLookup {
        kind: KIND_SESSION.into(),
        field: LOOKUP_UPDATED_AT.into(),
        value: OwnedPart::Int(session.updated_at.as_millis()),
        id: session.id,
    });
    ops.push(WriteOp::Delete {
        kind: KIND_SESSION.into(),
        id: session.id,
    });
    ops.push(WriteOp::DeleteLinksOf(session.id));
    Ok(ops)
}
