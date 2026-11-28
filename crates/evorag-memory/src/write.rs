//! 応答の経路での書き込み (04 §3)。操作の列を組み立てるだけで、書くのは書き手。

use evorag_store::key::OwnedPart;
use evorag_store::{BodyError, BodyPatch, Envelope, WriteOp};
use evorag_types::{Clock, Id, IdGen, Timestamp};

use crate::import_v04::{KIND_CORRECTION, KIND_NOTE, REL_CORRECTION_OF, REL_NOTE_OF};
use crate::kinds::*;

/// 書いたチャンク (ID と本体)。
pub type Chunk = (Id, ChunkBody);

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

/// セッションの更新時刻を `now` にし、`fields` を差し替える。セッションは書き手のトランザクションの中で読み直す
/// (外で読んだ封筒を書き戻すと、その間の題名の変更や削除を上書きするため、04 §7.7 の 3)。一覧の並びの副索引も、
/// 書き手が読み直した封筒から付け替える (`Patch` は種類の `derive` を作り直す)。
fn touch_session(session: &Envelope, now: Timestamp, fields: BodyPatch, ops: &mut Vec<WriteOp>) {
    ops.push(WriteOp::Patch {
        kind: KIND_SESSION.into(),
        id: session.id,
        fields,
        updated_at: Some(now),
    });
}

/// 派生の記録を書く前提: 出典のターン `turn` が残っていて、改訂の番号が `rev` (読んだときの番号) のまま
/// (04 §7.7 の 4)。合わなければ書き手が依頼の全体を断る。
pub fn expect_turn(turn: Id, rev: u32) -> WriteOp {
    WriteOp::Expect {
        kind: KIND_TURN.into(),
        id: turn,
        field: Some((TURN_REV_FIELD.into(), u64::from(rev))),
    }
}

/// 派生の記録を書く前提: 記録 `kind`/`id` が残っている (04 §7.7 の 4)。
pub fn expect_record(kind: &str, id: Id) -> WriteOp {
    WriteOp::Expect {
        kind: kind.into(),
        id,
        field: None,
    }
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
        // 本体を読めないセッションは書き換えない (03 §4.2 の規則 11)。
        let _: SessionBody = session.body_as()?;
        let mut ops = Vec::new();
        touch_session(
            session,
            self.clock.now(),
            BodyPatch::new().set("title", &title)?,
            &mut ops,
        );
        Ok(ops)
    }

    /// このセッションでメモリを添えるかを変える (`/sources`、FR-409)。
    pub fn set_session_memory(
        &self,
        session: &Envelope,
        memory: bool,
    ) -> Result<Vec<WriteOp>, BodyError> {
        let _: SessionBody = session.body_as()?;
        let mut ops = Vec::new();
        touch_session(
            session,
            self.clock.now(),
            BodyPatch::new().set("memory", &Some(memory))?,
            &mut ops,
        );
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

    /// しきい値の記録を足す (05 §3.7)。版は呼ぶ側が前の版 + 1 にする。前の版は消さない。
    pub fn save_threshold(&self, body: &ThresholdBody) -> Result<Vec<WriteOp>, BodyError> {
        Ok(vec![WriteOp::Put(envelope(
            self.ids.next_id(),
            KIND_THRESHOLD,
            self.clock.now(),
            body,
        )?)])
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
        self.user_turn_with(session, text, None)
    }

    /// 利用者の発話を、観測値 (05 §3.6 の発話のターンの項目、v0.4) をつけて書く。
    ///
    /// # Errors
    ///
    /// 本体を符号化できないとき。
    pub fn user_turn_with(
        &self,
        session: &Envelope,
        text: &str,
        obs: Option<TurnObs>,
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
                obs,
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
        touch_session(session, now, BodyPatch::new(), &mut ops);
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
        // 発話は状態だけを書き手のトランザクションの中で差し替える (本文と改訂の番号は読み直したものを残す。応答を
        // 作っている間の伏せ字を上書きしないため、04 §7.3・§7.7 の 3)。
        let user: TurnBody = user_turn.body_as()?;
        if user.state == TurnState::Pending {
            ops.push(WriteOp::Patch {
                kind: KIND_TURN.into(),
                id: user_turn.id,
                fields: BodyPatch::new().set("state", &TurnState::Confirmed)?,
                updated_at: Some(now),
            });
        }
        touch_session(session, now, BodyPatch::new(), &mut ops);
        Ok((id, ops))
    }

    /// 信号 (評価・再生成、13 §2、FR-311)。
    /// ターンのチャンクを書く (05 §2.4)。前のチャンク `old` は消す。返すのは新しいチャンクの ID と本体。
    pub fn chunks(
        &self,
        old: &[Id],
        bodies: &[ChunkBody],
    ) -> Result<(Vec<Chunk>, Vec<WriteOp>), BodyError> {
        let mut ops = Vec::new();
        for id in old {
            ops.push(WriteOp::Delete {
                kind: KIND_CHUNK.into(),
                id: *id,
            });
            ops.push(WriteOp::DeleteLinksOf(*id));
        }
        let now = self.clock.now();
        let mut made = Vec::new();
        for b in bodies {
            let id = self.ids.next_id();
            ops.push(WriteOp::Put(envelope(id, KIND_CHUNK, now, b)?));
            ops.push(WriteOp::PutLink {
                from: id,
                rel: REL_CHUNK_OF.into(),
                to: b.turn,
                attrs: Vec::new(),
            });
            made.push((id, *b));
        }
        Ok((made, ops))
    }

    /// 埋め込みを書く (05 §2.2)。ベクトルは `blobs` の part 0 に、f32 のリトルエンディアンで置く。
    pub fn embedding(
        &self,
        body: &EmbeddingBody,
        vector: &[f32],
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        Ok((
            id,
            vec![
                WriteOp::Put(envelope(id, KIND_EMBEDDING, self.clock.now(), body)?),
                WriteOp::PutBlob { id, part: 0, bytes },
                WriteOp::PutLink {
                    from: id,
                    rel: REL_EMBEDDING_OF.into(),
                    to: body.target,
                    attrs: Vec::new(),
                },
            ],
        ))
    }

    /// ノートを書く (04 §2)。同じ区切りの前のノート `old` (とその埋め込み `old_embeddings`) は消す。
    /// 本体の `src_revs` (鍵を作るときに読んだ出典のターンと改訂の番号、03 §3.3) ごとに、書き手が、出典が
    /// 残っていて番号が合うことを確かめてから書く (合わなければ依頼の全体を断る、04 §7.7 の 4)。
    pub fn note(
        &self,
        old: &[Id],
        old_embeddings: &[Id],
        body: &NoteBody,
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let mut ops = Vec::new();
        for e in old_embeddings {
            ops.extend(self.delete_embedding(*e));
        }
        for id in old {
            ops.push(WriteOp::Delete {
                kind: KIND_NOTE.into(),
                id: *id,
            });
            ops.push(WriteOp::DeleteLinksOf(*id));
        }
        ops.extend(body.src_revs.iter().map(|s| expect_turn(s.turn, s.rev)));
        let id = self.ids.next_id();
        ops.push(WriteOp::Put(envelope(
            id,
            KIND_NOTE,
            self.clock.now(),
            body,
        )?));
        for turn in &body.exchanges {
            ops.push(WriteOp::PutLink {
                from: id,
                rel: REL_NOTE_OF.into(),
                to: *turn,
                attrs: Vec::new(),
            });
        }
        Ok((id, ops))
    }

    /// 埋め込みを消す (記録・ベクトル・関係)。
    pub fn delete_embedding(&self, id: Id) -> Vec<WriteOp> {
        vec![
            WriteOp::Delete {
                kind: KIND_EMBEDDING.into(),
                id,
            },
            WriteOp::DeleteBlobs(id),
            WriteOp::DeleteLinksOf(id),
        ]
    }

    /// 想起への操作を記録する (05 §5.2)。
    pub fn recall_feedback(
        &self,
        body: &RecallFeedbackBody,
    ) -> Result<(Id, Vec<WriteOp>), BodyError> {
        let id = self.ids.next_id();
        let e = envelope(id, KIND_RECALL_FEEDBACK, self.clock.now(), body)?;
        Ok((
            id,
            vec![
                WriteOp::Put(e),
                WriteOp::PutLink {
                    from: id,
                    rel: REL_FEEDBACK_ANSWER.into(),
                    to: body.answer,
                    attrs: Vec::new(),
                },
                WriteOp::PutLink {
                    from: id,
                    rel: REL_FEEDBACK_TARGET.into(),
                    to: body.target,
                    attrs: Vec::new(),
                },
            ],
        ))
    }

    /// 想起への操作を取り消す (記録を消す。ジャーナルにも削除の行が書かれる)。
    pub fn delete_feedback(&self, id: Id) -> Vec<WriteOp> {
        vec![
            WriteOp::Delete {
                kind: KIND_RECALL_FEEDBACK.into(),
                id,
            },
            WriteOp::DeleteLinksOf(id),
        ]
    }

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
///
/// 事実は、そのセッションのターンの出典を外し、出典の無くなった事実を消す (04 §7.1・§7.4。書き込みの
/// 入口 [`crate::facts::drop_turns`] を通す。private でなくなる事実には、その値だけの private の指示を足す、
/// 04 §7.5)。`writes` の時計は書き換えた事実の更新時刻に、ID は足す指示に使う。呼ぶ側は、事実を書く経路の
/// ロックを `tx` を開く前から書き終えるまで持つ (04 §5.2.1 の「書き込みの直列化」)。消した事実の ID は
/// [`deleted_ids`] に入る (退避と索引から除く。事実の検索の鍵の ID は事実の ID と同じ、04 §5.5)。
///
/// # Errors
/// 記録を読めない・本体を書けないとき。
pub fn delete_session(
    tx: &evorag_store::ReadTxn<'_>,
    session: &Envelope,
    writes: &Writes<'_>,
) -> Result<Vec<WriteOp>, crate::MemoryError> {
    let turns: std::collections::BTreeSet<Id> = tx
        .links_to(session.id, REL_IN_SESSION)?
        .into_iter()
        .map(|(turn, _)| turn)
        .collect();
    let mut ops =
        crate::facts::drop_turns(tx, &|t| turns.contains(&t), writes.ids, writes.clock.now())?.ops;
    let mut feedback = std::collections::BTreeSet::new();
    let mut notes = std::collections::BTreeSet::new();
    for &turn in &turns {
        // ノートは出典の往復と一緒に消す (鍵の埋め込みを含む、03 §4.2 の規則 12)。
        for (n, _) in tx.links_to(turn, REL_NOTE_OF)? {
            if notes.insert(n) {
                for (emb, _) in tx.links_to(n, REL_EMBEDDING_OF)? {
                    ops.push(WriteOp::Delete {
                        kind: KIND_EMBEDDING.into(),
                        id: emb,
                    });
                    ops.push(WriteOp::DeleteBlobs(emb));
                    ops.push(WriteOp::DeleteLinksOf(emb));
                }
                ops.push(WriteOp::Delete {
                    kind: KIND_NOTE.into(),
                    id: n,
                });
                ops.push(WriteOp::DeleteLinksOf(n));
            }
        }
        // 訂正の候補と検証済みの訂正は、候補の発話と一緒に消す (出典の無い派生の記録、03 §4.2 の規則 12)。
        // 直前の応答は同じセッションにあるので、出典のターンはすべて消える。
        for (c, _) in tx.links_to(turn, REL_CORRECTION_OF)? {
            ops.push(WriteOp::Delete {
                kind: KIND_CORRECTION.into(),
                id: c,
            });
            ops.push(WriteOp::DeleteLinksOf(c));
        }
        for (signal, _) in tx.links_to(turn, REL_TARGET)? {
            ops.push(WriteOp::Delete {
                kind: KIND_SIGNAL.into(),
                id: signal,
            });
            ops.push(WriteOp::DeleteLinksOf(signal));
        }
        // チャンクは元のターンと一緒に消す (出典の無い派生の記録、03 §4.2 の規則 12)。
        for (c, _) in tx.links_to(turn, REL_CHUNK_OF)? {
            // チャンクの埋め込みも消す (ベクトルを含む)。
            for (emb, _) in tx.links_to(c, REL_EMBEDDING_OF)? {
                ops.push(WriteOp::Delete {
                    kind: KIND_EMBEDDING.into(),
                    id: emb,
                });
                ops.push(WriteOp::DeleteBlobs(emb));
                ops.push(WriteOp::DeleteLinksOf(emb));
            }
            ops.push(WriteOp::Delete {
                kind: KIND_CHUNK.into(),
                id: c,
            });
            ops.push(WriteOp::DeleteLinksOf(c));
        }
        // 想起への操作は、問いの応答と対象の往復のどちらが消えても消す (出典の無い派生の記録、03 §4.2 の規則 12)。
        for rel in [REL_FEEDBACK_ANSWER, REL_FEEDBACK_TARGET] {
            for (f, _) in tx.links_to(turn, rel)? {
                if feedback.insert(f) {
                    ops.push(WriteOp::Delete {
                        kind: KIND_RECALL_FEEDBACK.into(),
                        id: f,
                    });
                    ops.push(WriteOp::DeleteLinksOf(f));
                }
            }
        }
        ops.push(WriteOp::Delete {
            kind: KIND_TURN.into(),
            id: turn,
        });
        ops.push(WriteOp::DeleteLinksOf(turn));
    }
    ops.extend(segment_and_ledger_deletes(tx, session.id, &turns)?);
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

/// セッションの区切りと台帳を消す操作 (04 §5.2.1 の「台帳を消すとき」、03 §4.2 の規則 12、v0.5 の根)。区切りは
/// セッションへの関係から、台帳は区切りへの関係 (区切りの記録を失った台帳を含む) と、出典のターンへの関係から
/// 集める。読めない記録も、封筒と関係を消す。
fn segment_and_ledger_deletes(
    tx: &evorag_store::ReadTxn<'_>,
    session: Id,
    turns: &std::collections::BTreeSet<Id>,
) -> Result<Vec<WriteOp>, crate::MemoryError> {
    use crate::ledger::{KIND_PROPOSAL, REL_LEDGER_SEGMENT, REL_LEDGER_TURN};
    use crate::segment::{KIND_SEGMENT, REL_SEGMENT_SESSION};
    let mut segments = std::collections::BTreeSet::new();
    let mut ledgers = std::collections::BTreeSet::new();
    for (seg, _) in tx.links_to(session, REL_SEGMENT_SESSION)? {
        segments.insert(seg);
        ledgers.extend(
            tx.links_to(seg, REL_LEDGER_SEGMENT)?
                .into_iter()
                .map(|(l, _)| l),
        );
    }
    for &turn in turns {
        for (l, _) in tx.links_to(turn, REL_LEDGER_TURN)? {
            ledgers.insert(l);
            // 区切りの記録を失った台帳の区切りへの関係 (区切りの ID は台帳の `segment`)。
            for (seg, _) in tx.links_from(l, REL_LEDGER_SEGMENT)? {
                segments.insert(seg);
            }
        }
    }
    let mut ops = Vec::new();
    // 台帳の区切りの鍵の埋め込み (台帳を指す、04 §5.5) も消す (ベクトルを含む)。
    for &l in &ledgers {
        ops.extend(ledger_embedding_deletes(tx, l)?);
    }
    for (kind, ids) in [(KIND_SEGMENT, &segments), (KIND_PROPOSAL, &ledgers)] {
        for &id in ids {
            match tx.get(kind, id)? {
                Some(evorag_store::Read::Ok(e)) => ops.extend(delete_segment_or_ledger(&e)),
                Some(_) => {
                    ops.push(WriteOp::Delete {
                        kind: kind.into(),
                        id,
                    });
                    ops.push(WriteOp::DeleteLinksOf(id));
                }
                None => {}
            }
        }
    }
    Ok(ops)
}

/// 台帳 `ledger` を指す埋め込み (区切りの鍵の埋め込み、04 §5.5) を消す操作 (ベクトルと関係を含む)。台帳を消す
/// 依頼 (セッションの削除・置き換え・出典の無い台帳の片付け) に入れる。
///
/// # Errors
/// 関係か記録を読めないとき。
pub fn ledger_embedding_deletes(
    tx: &evorag_store::ReadTxn<'_>,
    ledger: Id,
) -> Result<Vec<WriteOp>, crate::MemoryError> {
    let mut ops = Vec::new();
    for (emb, _) in crate::read::embeddings_of(tx, ledger)? {
        ops.push(WriteOp::Delete {
            kind: KIND_EMBEDDING.into(),
            id: emb,
        });
        ops.push(WriteOp::DeleteBlobs(emb));
        ops.push(WriteOp::DeleteLinksOf(emb));
    }
    Ok(ops)
}

/// 区切りか台帳の記録を、関係と副索引ごと消す操作 (ほかの種類の記録は封筒と関係だけを消す)。
#[must_use]
pub fn delete_segment_or_ledger(e: &Envelope) -> Vec<WriteOp> {
    match e.kind.as_str() {
        crate::segment::KIND_SEGMENT => {
            crate::derived::delete_ops(e, crate::segment::derive_segment)
        }
        crate::ledger::KIND_PROPOSAL => crate::derived::delete_ops(e, crate::ledger::derive_ledger),
        _ => vec![
            WriteOp::Delete {
                kind: e.kind.clone(),
                id: e.id,
            },
            WriteOp::DeleteLinksOf(e.id),
        ],
    }
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
            directive: None,
        },
    )?;
    Ok((id, WriteOp::Put(e)))
}

/// 索引の項目だけを除く job (04 §7.7 の 2)。
pub fn index_job(writes: &Writes<'_>, ids: Vec<Id>) -> Result<(Id, WriteOp), BodyError> {
    let id = writes.ids.next_id();
    let e = envelope(
        id,
        KIND_JOB,
        writes.clock.now(),
        &JobBody {
            task: JOB_PURGE_INDEXES.into(),
            ids,
            directive: None,
        },
    )?;
    Ok((id, WriteOp::Put(e)))
}

/// 残っている整理の job。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupJob {
    pub job: Id,
    /// 索引 (と `backups` なら退避) から除く ID。
    pub ids: Vec<Id>,
    /// 退避からも除く ([`JOB_PURGE_BACKUPS`])。偽なら索引だけ ([`JOB_PURGE_INDEXES`])。
    pub backups: bool,
}

/// 残っている整理の job (索引と退避の掃除。起動時と sleep-time に続きを行う)。
pub fn pending_cleanup_jobs(
    tx: &evorag_store::ReadTxn<'_>,
) -> Result<Vec<CleanupJob>, crate::MemoryError> {
    let mut out = Vec::new();
    tx.for_each(KIND_JOB, |r| {
        if let evorag_store::Read::Ok(e) = r
            && let Ok(b) = e.body_as::<JobBody>()
            && (b.task == JOB_PURGE_BACKUPS || b.task == JOB_PURGE_INDEXES)
        {
            out.push(CleanupJob {
                job: e.id,
                ids: b.ids,
                backups: b.task == JOB_PURGE_BACKUPS,
            });
        }
        true
    })?;
    Ok(out)
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
