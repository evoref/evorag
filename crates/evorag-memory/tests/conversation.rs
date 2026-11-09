//! 会話の層の試験 (04 §2・§3、03 §3.3)。ストアと書き手を実際に使う。

use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use evorag_memory::*;
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store, WriteRequest, Writer, WriterHandle};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

struct Env {
    store: Arc<Store>,
    writer: WriterHandle,
    thread: Option<JoinHandle<()>>,
    clock: Arc<ManualClock>,
    ids: Arc<UuidV7Gen>,
}

impl Env {
    fn open(root: &Path) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
        let ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), 11));
        let (store, _) = Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.3".into(),
            kinds: kinds(),
            clock: clock.clone(),
            ids: ids.clone(),
            config_file: None,
        })
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

    fn write(&self, ops: Vec<evorag_store::WriteOp>) {
        self.writer
            .submit_wait(WriteRequest::conversation(ops))
            .unwrap();
    }

    fn session(&self, id: evorag_types::Id) -> evorag_store::Envelope {
        load_session(&self.store.read().unwrap(), id)
            .unwrap()
            .envelope
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.writer.shutdown();
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

fn reply(text: &str, state: TurnState) -> AssistantTurn {
    AssistantTurn {
        state,
        text: text.into(),
        model_key: Some("0123456789abcdef0123456789abcdef".into()),
        generation: Some(GenerationRecord {
            temperature: 0.7,
            top_p: 0.95,
            max_tokens: None,
            seed: None,
        }),
        attached: Vec::new(),
        error: None,
    }
}

/// verifies: AC-FR-104-1 (データの層: 作ったセッションを開き直すと、ターンが元の順に読める)
#[test]
fn a_conversation_is_read_back_in_order_after_reopening() {
    let dir = TempDir::new("conv-order");
    let (sid, user1, user2);
    {
        let env = Env::open(dir.path());
        let (id, ops) = env.writes().create_session("はじめての会話").unwrap();
        sid = id;
        env.write(ops);
        let (u, ops) = env
            .writes()
            .user_turn(&env.session(sid), "こんにちは")
            .unwrap();
        user1 = u;
        env.write(ops);
        env.clock.advance(1_000);
        let u1 = env.store.read().unwrap().get(KIND_TURN, user1).unwrap();
        let Some(evorag_store::Read::Ok(u1)) = u1 else {
            panic!()
        };
        let (_, ops) = env
            .writes()
            .assistant_turn(
                &env.session(sid),
                &u1,
                reply("こんにちは。", TurnState::Completed),
            )
            .unwrap();
        env.write(ops);
        let (u, ops) = env.writes().user_turn(&env.session(sid), "元気?").unwrap();
        user2 = u;
        env.write(ops);
    }

    let env = Env::open(dir.path());
    let rtx = env.store.read().unwrap();
    let turns = session_turns(&rtx, sid).unwrap();
    let summary: Vec<(TurnRole, TurnState, &str)> = turns
        .iter()
        .map(|t| (t.body.role, t.body.state, t.body.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (TurnRole::User, TurnState::Confirmed, "こんにちは"),
            (TurnRole::Assistant, TurnState::Completed, "こんにちは。"),
            (TurnRole::User, TurnState::Pending, "元気?"),
        ]
    );
    assert_eq!(turns[1].body.reply_to, Some(user1));
    assert_eq!(rtx.links_to(user1, REL_REPLY_TO).unwrap().len(), 1);
    // 応答を書いていない発話は「応答なし」として拾える (AC-FR-201-1 のデータの層)。
    let pending = pending_user_turns(&rtx, sid).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].envelope.id, user2);
}

#[test]
fn sessions_are_listed_newest_first_without_duplicates() {
    let dir = TempDir::new("conv-list");
    let env = Env::open(dir.path());
    let (a, ops) = env.writes().create_session("A").unwrap();
    env.write(ops);
    env.clock.advance(10);
    let (b, ops) = env.writes().create_session("B").unwrap();
    env.write(ops);
    env.clock.advance(10);
    // A に発話すると A が先頭に来る。
    let stale_a = env.session(a);
    let (_, ops) = env.writes().user_turn(&stale_a, "x").unwrap();
    env.write(ops);
    let names = |env: &Env| -> Vec<String> {
        list_sessions(&env.store.read().unwrap(), 10)
            .unwrap()
            .into_iter()
            .map(|s| s.body.title)
            .collect()
    };
    assert_eq!(names(&env), ["A", "B"]);

    // 古い封筒で付け替えても (古い副索引が残っても)、一覧に二重に出ない。
    env.clock.advance(10);
    let ops = env.writes().rename_session(&stale_a, "A2").unwrap();
    env.write(ops);
    assert_eq!(names(&env), ["A2", "B"]);
    let _ = b;
}

/// verifies: AC-FR-311-1 (データの層: 信号は対象のターンへの関係を持つ)
#[test]
fn signals_link_to_their_target_turn() {
    let dir = TempDir::new("conv-signal");
    let env = Env::open(dir.path());
    let (sid, ops) = env.writes().create_session("s").unwrap();
    env.write(ops);
    let (u, ops) = env.writes().user_turn(&env.session(sid), "q").unwrap();
    env.write(ops);
    let (sig, ops) = env
        .writes()
        .signal(SignalKind::Bad, u, Some("違う".into()))
        .unwrap();
    env.write(ops);
    let rtx = env.store.read().unwrap();
    assert_eq!(rtx.links_to(u, REL_TARGET).unwrap()[0].0, sig);
    let Some(evorag_store::Read::Ok(e)) = rtx.get(KIND_SIGNAL, sig).unwrap() else {
        panic!()
    };
    let body: SignalBody = e.body_as().unwrap();
    assert_eq!(body.kind, SignalKind::Bad);
    assert_eq!(body.reason.as_deref(), Some("違う"));
}

/// verifies: AC-FR-105-1 (データの層: 消したセッションは一覧から消え、ターン・信号・関係も残らない)
#[test]
fn deleting_a_session_removes_its_turns_signals_and_links() {
    let dir = TempDir::new("conv-delete");
    let env = Env::open(dir.path());
    let (keep, ops) = env.writes().create_session("残す").unwrap();
    env.write(ops);
    let (gone, ops) = env.writes().create_session("消す").unwrap();
    env.write(ops);
    let (u, ops) = env.writes().user_turn(&env.session(gone), "q").unwrap();
    env.write(ops);
    let (sig, ops) = env.writes().signal(SignalKind::Good, u, None).unwrap();
    env.write(ops);

    let ops = {
        let rtx = env.store.read().unwrap();
        delete_session(&rtx, &env.session(gone)).unwrap()
    };
    env.write(ops);

    let rtx = env.store.read().unwrap();
    let titles: Vec<String> = list_sessions(&rtx, 10)
        .unwrap()
        .into_iter()
        .map(|s| s.body.title)
        .collect();
    assert_eq!(titles, ["残す"]);
    assert!(rtx.get(KIND_TURN, u).unwrap().is_none());
    assert!(rtx.get(KIND_SIGNAL, sig).unwrap().is_none());
    assert!(rtx.links_to(gone, REL_IN_SESSION).unwrap().is_empty());
    assert!(rtx.links_from(u, REL_IN_SESSION).unwrap().is_empty());
    assert!(load_session(&rtx, keep).is_ok());
    let census = env.store.census().unwrap();
    assert_eq!(census.known[KIND_SESSION].ok, 1);
    assert_eq!(census.known.get(KIND_TURN).map_or(0, |c| c.ok), 0);
}

fn journal_text(root: &Path) -> String {
    let dir = root.join(evorag_store::JOURNAL_DIR);
    let mut out = String::new();
    for e in std::fs::read_dir(dir).unwrap() {
        out.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    out
}

/// verifies: AC-FR-202-2
#[test]
fn the_conversation_is_rebuilt_from_the_journal_alone() {
    let dir = TempDir::new("conv-journal-rebuild");
    let (sid, gone, before, signal);
    {
        let env = Env::open(dir.path());
        let (id, ops) = env.writes().create_session("残る").unwrap();
        sid = id;
        env.write(ops);
        let (g, ops) = env.writes().create_session("消す").unwrap();
        gone = g;
        env.write(ops);
        let (u, ops) = env
            .writes()
            .user_turn(&env.session(sid), "こんにちは")
            .unwrap();
        env.write(ops);
        env.clock.advance(1_000);
        let Some(evorag_store::Read::Ok(u1)) = env.store.read().unwrap().get(KIND_TURN, u).unwrap()
        else {
            panic!()
        };
        let (a, ops) = env
            .writes()
            .assistant_turn(
                &env.session(sid),
                &u1,
                reply("どうも", TurnState::Completed),
            )
            .unwrap();
        env.write(ops);
        let (s, ops) = env.writes().signal(SignalKind::Good, a, None).unwrap();
        signal = s;
        env.write(ops);
        let ops = env
            .writes()
            .rename_session(&env.session(sid), "改名")
            .unwrap();
        env.write(ops);
        let ops = {
            let rtx = env.store.read().unwrap();
            delete_session(&rtx, &env.session(gone)).unwrap()
        };
        env.write(ops);
        env.write(vec![evorag_store::WriteOp::SetMetaOnce {
            key: "prompt.language".into(),
            value: "ja".into(),
        }]);
        let rtx = env.store.read().unwrap();
        before = session_turns(&rtx, sid)
            .unwrap()
            .into_iter()
            .map(|t| (t.envelope.id, t.body.state, t.body.text))
            .collect::<Vec<_>>();
    }
    std::fs::remove_file(dir.path().join(evorag_store::DB_FILE)).unwrap();

    let env = Env::open(dir.path());
    let rtx = env.store.read().unwrap();
    let after: Vec<_> = session_turns(&rtx, sid)
        .unwrap()
        .into_iter()
        .map(|t| (t.envelope.id, t.body.state, t.body.text))
        .collect();
    assert_eq!(after, before);
    let titles: Vec<String> = list_sessions(&rtx, 10)
        .unwrap()
        .into_iter()
        .map(|s| s.body.title)
        .collect();
    assert_eq!(titles, ["改名"], "the deleted session must stay deleted");
    assert!(load_session(&rtx, gone).is_err());
    assert_eq!(rtx.links_to(after[1].0, REL_TARGET).unwrap()[0].0, signal);
    assert_eq!(rtx.links_to(after[0].0, REL_REPLY_TO).unwrap().len(), 1);
    drop(rtx);
    assert_eq!(
        env.store.meta("prompt.language").unwrap().as_deref(),
        Some("ja")
    );
}

/// 09 §4: DB だけのコミットに失敗したときの行は、起動時にジャーナルから取り込まれる。
#[test]
fn journal_lines_missing_from_the_database_are_replayed_at_startup() {
    let dir = TempDir::new("conv-journal-replay");
    let (sid, u);
    {
        let env = Env::open(dir.path());
        let (id, ops) = env.writes().create_session("s").unwrap();
        sid = id;
        env.write(ops);
        let (t, ops) = env
            .writes()
            .user_turn(&env.session(sid), "失われかけた発話")
            .unwrap();
        u = t;
        env.write(ops);
    }
    // DB から発話を消し、取り込んだ連番を戻す (ジャーナルにだけある状態を作る)。
    evorag_store::testutil::raw_remove_record(dir.path(), KIND_TURN, u);
    evorag_store::testutil::raw_set_meta(dir.path(), "journal.last_seq", "1");
    let env = Env::open(dir.path());
    let rtx = env.store.read().unwrap();
    let turns = session_turns(&rtx, sid).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].body.text, "失われかけた発話");
}

/// FR-309 のジャーナルの分: 消したセッションの本文は、ジャーナルのファイルからも消える。
#[test]
fn deleting_a_session_removes_its_text_from_the_journal() {
    let dir = TempDir::new("conv-journal-delete");
    let env = Env::open(dir.path());
    let (sid, ops) = env.writes().create_session("消す題名").unwrap();
    env.write(ops);
    let (_, ops) = env
        .writes()
        .user_turn(&env.session(sid), "秘密の本文")
        .unwrap();
    env.write(ops);
    assert!(journal_text(dir.path()).contains("秘密の本文"));
    let ops = {
        let rtx = env.store.read().unwrap();
        delete_session(&rtx, &env.session(sid)).unwrap()
    };
    env.write(ops);
    let text = journal_text(dir.path());
    assert!(!text.contains("秘密の本文"), "{text}");
    assert!(!text.contains("消す題名"), "{text}");
    assert!(text.contains("\"delete\""));
}

/// 03 §6: 規則で断られた書き込みは、ジャーナルにも書かない (起動時に検査なしで当たらないように)。
#[test]
fn a_rejected_write_is_not_journaled() {
    let dir = TempDir::new("conv-journal-rejected");
    let env = Env::open(dir.path());
    let (sid, ops) = env.writes().create_session("s").unwrap();
    env.write(ops);
    let (_, mut ops) = env
        .writes()
        .user_turn(&env.session(sid), "断られる本文")
        .unwrap();
    if let evorag_store::WriteOp::Put(e) = &mut ops[0] {
        e.v = 99; // 今の版で書けない `v`
    }
    assert!(
        env.writer
            .submit_wait(WriteRequest::conversation(ops))
            .is_err()
    );
    assert!(!journal_text(dir.path()).contains("断られる本文"));
}

/// 取り込みで、上書きする記録から作った副索引を消す (一覧の副索引が重ならない)。
#[test]
fn rebuilding_from_the_journal_leaves_one_lookup_per_session() {
    let dir = TempDir::new("conv-journal-lookup");
    let sid;
    {
        let env = Env::open(dir.path());
        let (id, ops) = env.writes().create_session("s").unwrap();
        sid = id;
        env.write(ops);
        for i in 0..3 {
            env.clock.advance(10);
            let ops = env
                .writes()
                .rename_session(&env.session(sid), &format!("s{i}"))
                .unwrap();
            env.write(ops);
        }
    }
    std::fs::remove_file(dir.path().join(evorag_store::DB_FILE)).unwrap();
    let env = Env::open(dir.path());
    let rtx = env.store.read().unwrap();
    let all = rtx
        .lookup(
            KIND_SESSION,
            "updated_at",
            &evorag_store::LookupRange::Between {
                from: None,
                to: None,
            },
            false,
            None,
        )
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].id, sid);
}
