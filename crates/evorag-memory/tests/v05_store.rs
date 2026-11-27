//! v0.5 の根の種類を、実際のストアに書いて読む試験 (03 §3.3・§3.4・§6・§7.1・§11、04 §2・§5.1・§5.2.1・§7.5)。

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use evorag_memory::*;
use evorag_store::export::{ExportOptions, NoChecks, export};
use evorag_store::key::{OwnedPart, Part};
use evorag_store::testutil::TempDir;
use evorag_store::{
    DB_FILE, Durability, Envelope, JOURNAL_DIR, LookupEntry, LookupRange, OpenOptions, Priority,
    Read, Store, StoreError, WriteError, WriteOp, WriteRequest, Writer, WriterHandle,
};
use evorag_types::{Clock, Id, ManualClock, Timestamp, UuidV7Gen};

const T0: i64 = 1_767_614_400_000;

struct Env {
    store: Arc<Store>,
    writer: WriterHandle,
    thread: Option<JoinHandle<()>>,
    clock: Arc<ManualClock>,
    ids: Arc<UuidV7Gen>,
}

impl Env {
    fn open(root: &Path) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0)));
        let ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), 11));
        let (kinds, plan) = v05_root();
        let (store, _) = Store::open_with(
            OpenOptions {
                data_root: root.to_path_buf(),
                evorag_version: "0.5.0".into(),
                kinds,
                clock: clock.clone(),
                ids: ids.clone(),
                config_file: None,
            },
            plan,
        )
        .unwrap();
        let store = Arc::new(store);
        let (w, writer) = Writer::new(store.clone()).unwrap();
        let thread = Some(std::thread::spawn(move || w.run()));
        Self {
            store,
            writer,
            thread,
            clock,
            ids,
        }
    }

    fn writes(&self) -> Writes<'_> {
        Writes {
            clock: &*self.clock,
            ids: &*self.ids,
        }
    }

    fn submit(&self, req: WriteRequest) -> Result<evorag_store::WriteOutcome, WriteError> {
        self.writer.submit_wait(req)
    }

    fn write(&self, ops: Vec<WriteOp>) {
        self.submit(WriteRequest::conversation(ops)).unwrap();
    }

    fn get(&self, kind: &str, id: Id) -> Option<Envelope> {
        match self.store.read().unwrap().get(kind, id).unwrap() {
            Some(Read::Ok(e)) => Some(e),
            _ => None,
        }
    }

    fn lookup(&self, kind: &str, field: &str, value: Part<'_>) -> Vec<Id> {
        self.store
            .read()
            .unwrap()
            .lookup(kind, field, &LookupRange::Eq(value), false, None)
            .unwrap()
            .into_iter()
            .map(|e: LookupEntry| e.id)
            .collect()
    }

    fn all(&self, kind: &str, field: &str) -> Vec<LookupEntry> {
        self.store
            .read()
            .unwrap()
            .lookup(
                kind,
                field,
                &LookupRange::Between {
                    from: None,
                    to: None,
                },
                false,
                None,
            )
            .unwrap()
    }

    /// セッションと、利用者の発話 2 つ。
    fn conversation(&self) -> (Id, Id, Id) {
        let w = self.writes();
        let (session, ops) = w.create_session("見本").unwrap();
        let env = put_of(&ops, session);
        self.write(ops);
        let (u1, ops) = w.user_turn(&env, "私は見本の町に住んでいます。").unwrap();
        self.write(ops);
        let (u2, ops) = w.user_turn(&env, "来月、見本の島へ行きます。").unwrap();
        self.write(ops);
        (session, u1, u2)
    }

    fn close(mut self) {
        self.writer.shutdown();
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

fn put_of(ops: &[WriteOp], id: Id) -> Envelope {
    ops.iter()
        .find_map(|op| match op {
            WriteOp::Put(e) if e.id == id => Some(e.clone()),
            _ => None,
        })
        .expect("the written record")
}

fn ledger_for(segment: Id, u1: Id, u2: Id, status: LedgerStatus) -> LedgerBody {
    let mut output = LedgerOutput::default();
    output
        .set_section(LedgerOutput::KEYS, &vec!["住まい"])
        .unwrap();
    output
        .set_section(
            LedgerOutput::UNITS,
            &vec![LedgerUnitOut {
                sentence: 1,
                layer: LedgerLayer::World,
                entity: "self".into(),
                aspect: "residence".into(),
                value: "見本の町".into(),
                mood: LedgerMood::Fact,
                tense: LedgerTense::Current,
                old: String::new(),
                when: String::new(),
            }],
        )
        .unwrap();
    LedgerBody {
        call: CALL_SEGMENT.into(),
        segment: Some(segment),
        schema: LEDGER_SCHEMA,
        variant: "b2-ja-1".into(),
        model_key: "m1".into(),
        sentences: vec![
            SentenceSpan {
                s: 1,
                turn: u1,
                rev: 0,
                start: 0,
                end: 42,
                extra: Default::default(),
            },
            SentenceSpan {
                s: 2,
                turn: u2,
                rev: 0,
                start: 0,
                end: 39,
                extra: Default::default(),
            },
        ],
        inputs: LedgerInputs {
            prefilter: 1,
            splitter: 1,
            correction: 1,
            forgets_seen: None,
            input_hash: None,
            role_fallback: false,
            role_model_key: None,
            through: Some(Through { turn: u2, pairs: 2 }),
            corr: Vec::new(),
            refs: Vec::new(),
            extra: Default::default(),
        },
        output,
        status: status.name().unwrap().into(),
        attempts: 0,
        tokens: LedgerTokens::default(),
        scrubbed: 0,
        failed_parts: 0,
        extra: Default::default(),
    }
}

fn unit_body(private: bool, u1: Id) -> UnitBody {
    UnitBody {
        layer: UnitLayer::World,
        entity: "self".into(),
        aspect: "residence".into(),
        free: false,
        value: "見本の町".into(),
        vk: if private { "cd" } else { "ab" }.repeat(32),
        norm: NORM_V1,
        when: Some(UnitWhen {
            from: Some("2026-01".into()),
            to: None,
            basis: WhenBasis::Stated,
        }),
        tense: UnitTense::Current,
        state: Some(UnitState::Live),
        against: None,
        by: None,
        state_at: None,
        private,
        anchors: vec![Anchor {
            turn: u1,
            rev: 0,
            start: 6,
            end: 18,
            s_start: 0,
            s_end: 42,
            kind: AnchorKind::Said,
            when: None,
        }],
    }
}

fn journal_text(root: &Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(root.join(JOURNAL_DIR)).unwrap() {
        out.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    out
}

/// 指示・区切り・台帳・単位を書いて読み、副索引と関係で引ける。指示だけがジャーナルに入る。
/// verifies: 03 §3.3 (v0.5 の種類の関係と副索引), 03 §6 (v0.5 の根のジャーナルの書く物: `directive` を書き、`segment`・`proposal`・`unit` を書かない)
#[test]
fn v05_records_round_trip_and_are_found_through_their_indexes() {
    let dir = TempDir::new("v05-records");
    let env = Env::open(dir.path());
    let (session, u1, u2) = env.conversation();
    let w = env.writes();

    // 指示: 忘れると、それを解く印。
    let forget = DirectiveBody {
        entity: Some("self".into()),
        aspect: Some("residence".into()),
        vks: vec!["ab".repeat(32), "ef".repeat(32)],
        turns: vec![u1],
        hidden: vec![TurnSpan {
            turn: u2,
            rev: 0,
            start: 0,
            end: 6,
        }],
        ..DirectiveBody::new(DirectiveOp::Forget)
    };
    let (forget_id, ops) = w.directive(&forget).unwrap();
    env.write(ops);
    let unblock = DirectiveBody {
        target: Some(forget_id),
        ..DirectiveBody::new(DirectiveOp::Unblock)
    };
    let (unblock_id, ops) = w.directive(&unblock).unwrap();
    env.write(ops);

    // 区切りと台帳 (同じコミット)、単位。
    let seg = SegmentBody {
        session,
        pairs: vec![u1, u2],
        closed: true,
        chars: 30,
    };
    let seg_id = seg.id().unwrap();
    let ledger = ledger_for(seg_id, u1, u2, LedgerStatus::Complete);
    let (ledger_id, req) = w
        .ledger(
            &ledger,
            Some(SegmentUpdate {
                body: &seg,
                previous: None,
            }),
            &[],
        )
        .unwrap();
    env.submit(req).unwrap();
    let (open_unit, private_unit) = (Id::from_bytes([0x31; 16]), Id::from_bytes([0x32; 16]));
    let now = env.clock.now();
    let mut ops = unit_ops(open_unit, now, &unit_body(false, u1), None).unwrap();
    ops.extend(unit_ops(private_unit, now, &unit_body(true, u1), None).unwrap());
    env.submit(units_request(ops)).unwrap();

    // 書いて読める。
    assert_eq!(
        env.get(KIND_DIRECTIVE, forget_id)
            .unwrap()
            .body_as::<DirectiveBody>()
            .unwrap(),
        forget
    );
    assert_eq!(
        env.get(KIND_SEGMENT, seg_id)
            .unwrap()
            .body_as::<SegmentBody>()
            .unwrap(),
        seg
    );
    assert_eq!(
        env.get(KIND_PROPOSAL, ledger_id)
            .unwrap()
            .body_as::<LedgerBody>()
            .unwrap(),
        ledger
    );
    assert_eq!(
        env.get(KIND_UNIT, private_unit)
            .unwrap()
            .body_as::<UnitBody>()
            .unwrap(),
        unit_body(true, u1)
    );

    // 指示の副索引と関係。
    let vk = "ef".repeat(32);
    assert_eq!(
        env.lookup(KIND_DIRECTIVE, LOOKUP_DIRECTIVE_VK, Part::Str(&vk)),
        [forget_id]
    );
    assert_eq!(
        env.lookup(KIND_DIRECTIVE, LOOKUP_DIRECTIVE_ENT, Part::Str("self")),
        [forget_id]
    );
    for t in [u1, u2] {
        assert_eq!(
            env.lookup(KIND_DIRECTIVE, LOOKUP_DIRECTIVE_TURN, Part::Id(t)),
            [forget_id],
            "{t}"
        );
    }
    let rtx = env.store.read().unwrap();
    assert_eq!(
        rtx.links_to(forget_id, REL_DIRECTIVE_CANCELS)
            .unwrap()
            .into_iter()
            .map(|l| l.0)
            .collect::<Vec<_>>(),
        [unblock_id]
    );
    // 区切り: セッションと順の副索引、往復のターンへの関係。
    assert_eq!(
        rtx.links_to(u2, REL_SEGMENT_TURN).unwrap().len(),
        1,
        "segment → its pairs"
    );
    assert_eq!(
        rtx.links_from(seg_id, REL_SEGMENT_SESSION).unwrap()[0].0,
        session
    );
    // 台帳: 区切りと選択の副索引、区切りと出典のターンへの関係。
    assert_eq!(
        rtx.links_from(ledger_id, REL_LEDGER_SEGMENT).unwrap()[0].0,
        seg_id
    );
    assert_eq!(rtx.links_from(ledger_id, REL_LEDGER_TURN).unwrap().len(), 2);
    // 単位: 根拠のターンへの関係。
    assert_eq!(rtx.links_to(u1, REL_UNIT_ANCHOR).unwrap().len(), 2);
    drop(rtx);
    assert_eq!(
        env.lookup(KIND_SEGMENT, LOOKUP_SEGMENT_SESSION, Part::Id(session)),
        [seg_id]
    );
    assert_eq!(
        env.all(KIND_SEGMENT, LOOKUP_SEGMENT_FIRST)
            .into_iter()
            .map(|e| (e.value, e.id))
            .collect::<Vec<_>>(),
        [(OwnedPart::Id(u1), seg_id)]
    );
    assert_eq!(
        env.lookup(KIND_PROPOSAL, LOOKUP_LEDGER_SEGMENT, Part::Id(seg_id)),
        [ledger_id]
    );
    let sel = selection_key("m1", "b2-ja-1");
    assert_eq!(
        env.lookup(KIND_PROPOSAL, LOOKUP_LEDGER_SELECTION, Part::Str(&sel)),
        [ledger_id]
    );
    let ea = entity_aspect_key("self", "residence");
    let mut both = vec![open_unit, private_unit];
    both.sort();
    assert_eq!(env.lookup(KIND_UNIT, LOOKUP_UNIT_EA, Part::Str(&ea)), both);
    assert_eq!(env.lookup(KIND_UNIT, LOOKUP_UNIT_SRC, Part::Id(u1)), both);
    // private の単位は `vk` と `date` の副索引を持たない。
    assert_eq!(
        env.all(KIND_UNIT, LOOKUP_UNIT_VK)
            .into_iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        [open_unit]
    );
    assert_eq!(
        env.lookup(KIND_UNIT, LOOKUP_UNIT_DATE, Part::Str("2026-01")),
        [open_unit]
    );

    // ジャーナルには指示だけ (会話の種類と) が入る。
    let journal = journal_text(dir.path());
    for id in [forget_id, unblock_id, u1] {
        assert!(journal.contains(&id.to_string()), "{id}");
    }
    for id in [seg_id, ledger_id, open_unit, private_unit] {
        assert!(!journal.contains(&id.to_string()), "{id} is not journaled");
    }
    env.close();
}

/// 台帳と閉じた区切りは `Immediate` で、開いている末尾の区切りと単位は `None` で書く (どれも会話の追記より後に回す)。
/// 台帳を置き換えるときは、前の台帳を消すことを同じコミットにする。書く前に出典のターンの改訂の番号を確かめる。
/// verifies: 03 §3.4 (台帳と閉じた区切りは `Immediate`、導出の結果は `None`), 04 §5.2.1 (置き換えは同じ `Immediate` のコミット)
#[test]
fn ledgers_and_closed_segments_are_written_immediately_and_replaced_in_one_commit() {
    let dir = TempDir::new("v05-durability");
    let env = Env::open(dir.path());
    let (session, u1, u2) = env.conversation();
    let w = env.writes();
    let mut open = SegmentBody {
        session,
        pairs: vec![u1],
        closed: false,
        chars: 15,
    };
    let (seg_id, req) = w.segment_request(&open, None).unwrap();
    assert_eq!(
        (req.durability, req.priority),
        (Durability::None, Priority::Background)
    );
    env.submit(req).unwrap();

    // 末尾の区切りの台帳 (`partial`)。
    let partial = ledger_for(seg_id, u1, u2, LedgerStatus::Partial);
    let (first, req) = w.ledger(&partial, None, &[]).unwrap();
    assert_eq!(
        (req.durability, req.priority),
        (Durability::Immediate, Priority::Background)
    );
    env.submit(req).unwrap();

    // 往復が足されて閉じた: 区切りの更新と、前の台帳の削除と、新しい台帳を 1 つのコミットで書く。
    open.pairs.push(u2);
    open.closed = true;
    let previous_seg = env.get(KIND_SEGMENT, seg_id).unwrap();
    let previous_ledger = env.get(KIND_PROPOSAL, first).unwrap();
    let complete = ledger_for(seg_id, u1, u2, LedgerStatus::Complete);
    let (second, req) = w
        .ledger(
            &complete,
            Some(SegmentUpdate {
                body: &open,
                previous: Some(&previous_seg),
            }),
            &[previous_ledger],
        )
        .unwrap();
    assert_eq!(req.durability, Durability::Immediate);
    env.submit(req).unwrap();
    assert!(env.get(KIND_PROPOSAL, first).is_none());
    assert_eq!(
        env.lookup(KIND_PROPOSAL, LOOKUP_LEDGER_SEGMENT, Part::Id(seg_id)),
        [second]
    );
    let rtx = env.store.read().unwrap();
    assert!(rtx.links_from(first, REL_LEDGER_TURN).unwrap().is_empty());
    drop(rtx);
    let seg = env.get(KIND_SEGMENT, seg_id).unwrap();
    assert_eq!(seg.created_at, previous_seg.created_at);
    assert!(seg.body_as::<SegmentBody>().unwrap().closed);

    // 閉じた区切りは変えない。
    assert!(matches!(
        w.segment_request(&open, Some(&seg)),
        Err(SegmentError::Closed)
    ));
    let (_, req) = w
        .segment_request(
            &SegmentBody {
                session,
                pairs: vec![u2],
                closed: true,
                chars: 10,
            },
            None,
        )
        .unwrap();
    assert_eq!(req.durability, Durability::Immediate);

    // 単位は `None`。
    let req = units_request(
        unit_ops(
            Id::from_bytes([0x41; 16]),
            env.clock.now(),
            &unit_body(false, u1),
            None,
        )
        .unwrap(),
    );
    assert_eq!(
        (req.durability, req.priority),
        (Durability::None, Priority::Background)
    );

    // 出典のターンの改訂の番号が合わない台帳は書かない (依頼の全体を断る)。
    let mut stale = ledger_for(seg_id, u1, u2, LedgerStatus::Complete);
    stale.sentences[0].rev = 1;
    let (stale_id, req) = w.ledger(&stale, None, &[]).unwrap();
    assert!(matches!(
        env.submit(req),
        Err(WriteError::Store(StoreError::Stale { .. }))
    ));
    assert!(env.get(KIND_PROPOSAL, stale_id).is_none());
    env.close();

    // 開き直しても、台帳と閉じた区切りが残る。
    let env = Env::open(dir.path());
    assert!(env.get(KIND_PROPOSAL, second).is_some());
    assert!(env.get(KIND_SEGMENT, seg_id).is_some());
    env.close();
}

/// 退避と書き出しに、指示・区切り・台帳を含め、単位を含めない。
/// verifies: 03 §7.1 (v0.5 の根の退避に単位を写さない), 03 §11 (v0.5 の根の書き出しの範囲)
#[test]
fn units_are_left_out_of_backups_and_exports_but_ledgers_are_kept() {
    let dir = TempDir::new("v05-copies");
    let env = Env::open(dir.path());
    let (session, u1, u2) = env.conversation();
    let w = env.writes();
    let (directive, ops) = w
        .directive(&DirectiveBody {
            entity: Some("self".into()),
            vks: vec!["ab".repeat(32)],
            ..DirectiveBody::new(DirectiveOp::Forget)
        })
        .unwrap();
    env.write(ops);
    let seg = SegmentBody {
        session,
        pairs: vec![u1, u2],
        closed: true,
        chars: 30,
    };
    let seg_id = seg.id().unwrap();
    let (ledger, req) = w
        .ledger(
            &ledger_for(seg_id, u1, u2, LedgerStatus::Complete),
            Some(SegmentUpdate {
                body: &seg,
                previous: None,
            }),
            &[],
        )
        .unwrap();
    env.submit(req).unwrap();
    let unit = Id::from_bytes([0x51; 16]);
    env.submit(units_request(
        unit_ops(unit, env.clock.now(), &unit_body(false, u1), None).unwrap(),
    ))
    .unwrap();

    let backup = env.store.daily_backup(None).unwrap().unwrap();
    env.close();
    let other = TempDir::new("v05-copies-backup");
    std::fs::copy(backup.join(DB_FILE), other.path().join(DB_FILE)).unwrap();
    let copy = Env::open(other.path());
    assert!(copy.get(KIND_UNIT, unit).is_none());
    assert!(copy.all(KIND_UNIT, LOOKUP_UNIT_EA).is_empty());
    for (kind, id) in [
        (KIND_DIRECTIVE, directive),
        (KIND_SEGMENT, seg_id),
        (KIND_PROPOSAL, ledger),
    ] {
        assert!(copy.get(kind, id).is_some(), "{kind}");
    }
    assert_eq!(
        copy.lookup(KIND_PROPOSAL, LOOKUP_LEDGER_SEGMENT, Part::Id(seg_id)),
        [ledger]
    );
    copy.close();

    let out = dir.path().with_extension("export");
    let _ = std::fs::remove_dir_all(&out);
    let report = export(
        dir.path(),
        &out,
        &ExportOptions {
            evorag_version: "0.5.0",
            now_ms: T0,
            exclude: &[],
            checks: &NoChecks,
        },
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&out);
    assert_eq!(report.kinds.get(KIND_UNIT), None);
    for kind in [KIND_DIRECTIVE, KIND_SEGMENT, KIND_PROPOSAL] {
        assert_eq!(report.kinds.get(kind), Some(&1), "{kind}");
    }
}

/// 置き換える前の台帳が無ければ (同じ依頼を送り直した)、依頼の全体を断る。区切りも、外で読んだ後に書き換えられて
/// いれば (その間に閉じた)、古い本体の上に書かない。
/// verifies: 04 §5.2.1 (置き換える前の台帳が残っていることを確かめる), 04 §2 (区切りは読んだときの本体と同じであることを確かめて書く)
#[test]
fn a_resent_replacement_and_a_stale_segment_are_refused() {
    let dir = TempDir::new("v05-resend");
    let env = Env::open(dir.path());
    let (session, u1, u2) = env.conversation();
    let w = env.writes();
    let open = SegmentBody {
        session,
        pairs: vec![u1],
        closed: false,
        chars: 15,
    };
    let (seg_id, req) = w.segment_request(&open, None).unwrap();
    env.submit(req).unwrap();
    let (first, req) = w
        .ledger(
            &ledger_for(seg_id, u1, u2, LedgerStatus::Partial),
            None,
            &[],
        )
        .unwrap();
    env.submit(req).unwrap();
    let old = env.get(KIND_PROPOSAL, first).unwrap();
    let complete = ledger_for(seg_id, u1, u2, LedgerStatus::Complete);
    let (_, req) = w
        .ledger(&complete, None, std::slice::from_ref(&old))
        .unwrap();
    env.submit(req.clone()).unwrap();
    // 送り直し: 前の台帳はもう無いので、2 つ目の台帳を書かない。
    let (resent, req) = w.ledger(&complete, None, &[old]).unwrap();
    assert!(matches!(
        env.submit(req),
        Err(WriteError::Store(StoreError::Stale { .. }))
    ));
    assert!(env.get(KIND_PROPOSAL, resent).is_none());
    assert_eq!(
        env.lookup(KIND_PROPOSAL, LOOKUP_LEDGER_SEGMENT, Part::Id(seg_id))
            .len(),
        1
    );

    // 区切り: 読んだ後に閉じられた。
    let read = env.get(KIND_SEGMENT, seg_id).unwrap();
    let closed = SegmentBody {
        pairs: vec![u1, u2],
        closed: true,
        chars: 30,
        ..open.clone()
    };
    let (_, req) = w.segment_request(&closed, Some(&read)).unwrap();
    env.submit(req).unwrap();
    let grown = SegmentBody {
        pairs: vec![u1, u2],
        closed: false,
        chars: 30,
        ..open.clone()
    };
    let (_, req) = w.segment_request(&grown, Some(&read)).unwrap();
    assert!(matches!(
        env.submit(req),
        Err(WriteError::Store(StoreError::Stale { .. }))
    ));
    // 無かったはずの区切りを新しく作る依頼も、その間に作られていれば断る。
    let (_, req) = w.segment_request(&grown, None).unwrap();
    assert!(matches!(
        env.submit(req),
        Err(WriteError::Store(StoreError::Stale { .. }))
    ));
    assert!(
        env.get(KIND_SEGMENT, seg_id)
            .unwrap()
            .body_as::<SegmentBody>()
            .unwrap()
            .closed
    );
    env.close();
}

/// `end` の位置が決まらないとき (位置が指示自身の ID) は、指示の副索引 `turn` にその ID を入れない。前の封筒の違う
/// 単位の上には書かない。
/// verifies: 03 §3.3 (指示の副索引 `turn` は根拠と伏せ字のターン), 04 §7.5 (`end` の位置が決まらなければ指示の ID)
#[test]
fn an_end_without_a_position_does_not_index_its_own_id_as_a_turn() {
    let dir = TempDir::new("v05-end-self");
    let env = Env::open(dir.path());
    let (_, u1, _) = env.conversation();
    let w = env.writes();
    let end = DirectiveBody {
        entity: Some("self".into()),
        vks: vec!["ab".repeat(32)],
        ..DirectiveBody::new(DirectiveOp::End)
    };
    let (_, mut ops) = w.directive(&end).unwrap();
    // 位置が決まらないときの形にする (封筒の ID を位置にする)。
    let WriteOp::Put(e) = &mut ops[0] else {
        panic!("the directive is put first");
    };
    let own = e.id;
    let mut body: DirectiveBody = e.body_as().unwrap();
    body.turn = Some(own);
    e.set_body(&body).unwrap();
    let rederived = evorag_store::Envelope::clone(e);
    let spec = kinds_v05()
        .into_iter()
        .find(|k| k.name == KIND_DIRECTIVE)
        .unwrap();
    let mut ops = vec![WriteOp::Put(rederived.clone())];
    ops.extend(spec.derive.unwrap()(&rederived));
    env.write(ops);
    assert!(
        env.lookup(KIND_DIRECTIVE, LOOKUP_DIRECTIVE_TURN, Part::Id(own))
            .is_empty()
    );
    // 違う単位の封筒の上には書かない。
    let other = Id::from_bytes([0x44; 16]);
    let previous = unit_ops(other, env.clock.now(), &unit_body(false, u1), None).unwrap();
    let WriteOp::Put(prev) = &previous[0] else {
        panic!("the unit is put first");
    };
    assert!(matches!(
        unit_ops(
            Id::from_bytes([0x45; 16]),
            env.clock.now(),
            &unit_body(false, u1),
            Some(prev)
        ),
        Err(RecordError::WrongPrevious { .. })
    ));
    env.close();
}
