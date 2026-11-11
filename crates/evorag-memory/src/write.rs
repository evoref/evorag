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
    pub recall: Option<RecallRecord>,
    pub obs: Option<TurnObs>,
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
                memory: None,
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

    /// このセッションでメモリを添えるかを変える (`/sources`、FR-409)。
    pub fn set_session_memory(
        &self,
        session: &Envelope,
        memory: bool,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let mut body: SessionBody = session.body_as()?;
        body.memory = Some(memory);
        let mut e = session.clone();
        e.set_body(&body)?;
        let mut ops = Vec::new();
        touch_session(&e, self.clock.now(), &mut ops);
        Ok(ops)
    }

    /// 設定を書く (03 §3.3 の `setting`)。同じ名前の記録があれば上書きし、重複していれば消す。
    pub fn set_setting(
        &self,
        current: &crate::read::Settings,
        name: &str,
        value: &str,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let now = self.clock.now();
        let body = SettingBody {
            name: name.to_owned(),
            value: value.to_owned(),
        };
        let mut ops = Vec::new();
        let e = match current.by_name.get(name) {
            Some(s) => {
                let mut e = s.envelope.clone();
                e.set_body(&body)?;
                e.updated_at = now;
                e
            }
            None => envelope(self.ids.next_id(), KIND_SETTING, now, &body)?,
        };
        ops.push(WriteOp::Put(e));
        for d in &current.duplicates {
            if d.body_as::<SettingBody>().is_ok_and(|b| b.name == name) {
                ops.push(WriteOp::Delete {
                    kind: KIND_SETTING.into(),
                    id: d.id,
                });
            }
        }
        Ok(ops)
    }

    /// 役割のモデルを記録する (06 §3.1)。記録があれば、前のモデルを履歴に移して書き換える。
    pub fn bind_model(
        &self,
        current: &crate::read::Bindings,
        role: &str,
        origin: ModelOrigin,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let now = self.clock.now();
        let mut ops = Vec::new();
        let e = match current.by_role.get(role) {
            Some(b) => {
                let mut body = b.body.clone();
                if body.current.model_key != origin.model_key {
                    body.history.push(body.current.clone());
                }
                body.current = origin;
                let mut e = b.envelope.clone();
                e.set_body(&body)?;
                e.updated_at = now;
                e
            }
            None => envelope(
                self.ids.next_id(),
                KIND_MODEL_BINDING,
                now,
                &ModelBindingBody {
                    role: role.to_owned(),
                    current: origin,
                    history: Vec::new(),
                },
            )?,
        };
        ops.push(WriteOp::Put(e));
        for d in &current.duplicates {
            if d.body_as::<ModelBindingBody>()
                .is_ok_and(|b| b.role == role)
            {
                ops.push(WriteOp::Delete {
                    kind: KIND_MODEL_BINDING.into(),
                    id: d.id,
                });
            }
        }
        Ok(ops)
    }

    /// 点検の結果を書く (06 §3.3)。同じ役割・model_key・`build_info` の古い結果は消す。
    pub fn save_probe(
        &self,
        old: Option<&Envelope>,
        body: &ModelProbeBody,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let now = self.clock.now();
        let mut ops = Vec::new();
        if let Some(o) = old {
            ops.push(WriteOp::Delete {
                kind: KIND_MODEL_PROBE.into(),
                id: o.id,
            });
        }
        ops.push(WriteOp::Put(envelope(
            self.ids.next_id(),
            KIND_MODEL_PROBE,
            now,
            body,
        )?));
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
                recall: None,
                obs: None,
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
                recall: turn.recall,
                obs: turn.obs,
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

/// 消した記録を退避から除く job (03 §7.3)。削除と同じ依頼に入れる。
pub fn purge_job(writes: &Writes<'_>, ids: Vec<Id>) -> Result<(Id, WriteOp), BodyError> {
    let id = writes.ids.next_id();
    let e = envelope(
        id,
        KIND_JOB,
        writes.clock.now(),
        &JobBody {
            task: JOB_PURGE_BACKUPS.into(),
            ids,
        },
    )?;
    Ok((id, WriteOp::Put(e)))
}

/// 残っている退避の整理の job (起動時に続きを行う)。
pub fn pending_purge_jobs(
    tx: &evorag_store::ReadTxn<'_>,
) -> Result<Vec<(Id, Vec<Id>)>, crate::MemoryError> {
    let mut out = Vec::new();
    tx.for_each(KIND_JOB, |r| {
        if let evorag_store::Read::Ok(e) = r
            && let Ok(b) = e.body_as::<JobBody>()
            && b.task == JOB_PURGE_BACKUPS
        {
            out.push((e.id, b.ids));
        }
        true
    })?;
    Ok(out)
}

/// セッションを消す操作が対象にする記録の ID (退避の整理に渡す)。
pub fn deleted_ids(ops: &[WriteOp]) -> Vec<Id> {
    ops.iter()
        .filter_map(|op| match op {
            WriteOp::Delete { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}
