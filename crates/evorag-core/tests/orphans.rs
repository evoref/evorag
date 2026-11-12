//! 出典の無い派生の記録の突き合わせ (03 §4.2 の規則 12、AC-NFR-307-2)。古い版 (v0.1.2) でセッションを消した
//! データ根を、記録を直接書き換えてまねる。llama-server は記録の再生で置き換える。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{KIND_NOTE, ModelProbeBody, NoteBody, SEGMENT_QUIET_MS};
use evorag_store::Read;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, TurnView};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";

fn reply(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// 応答の llama-server (スロット 2 つ)。`chats` は順に使う答え。`busy` なら応答のスロットが処理中。
fn chat(chats: &[String], busy: bool) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}],
          "slots": [{{"id": 0, "is_processing": {busy}}}, {{"id": 1, "is_processing": false}}]
        }}"#,
        chats.join(", ")
    ))
    .unwrap()
}

fn chat_key() -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(1),
        size: Some(2),
        n_vocab: Some(3),
        n_ctx_train: None,
    };
    evorag_llm::model_key("/m/chat.gguf", &meta, "test")
}

/// 起動の前に、応答のモデルの点検の結果を書いておく (型縛りを使えるか)。
fn prepare(root: &Path, structured_ok: bool) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_640_000_000)));
    let m = DataRootModels::open(
        root,
        "0.0.6",
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, 3)),
    )
    .unwrap();
    m.save_probe(&ModelProbeBody {
        role: "chat".into(),
        model_key: chat_key(),
        build_info: "b1-test".into(),
        structured_ok: Some(structured_ok),
        thinking_off: Some(true),
        ..ModelProbeBody::default()
    })
    .unwrap();
}

fn config(root: &Path, clock: Arc<ManualClock>) -> CoreConfig {
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.6".into(),
        prompt_language: PromptLanguage::Ja,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "test".into(),
        ignored_config: Vec::new(),
        embed: None,
        rerank: None,
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
    }
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut got = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        let stop = pred(&ev);
        got.push(ev);
        if stop {
            return got;
        }
    }
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> TurnView {
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    turn.clone()
}

#[allow(dead_code)]
fn notes(root: &Path) -> Vec<NoteBody> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap();
    let rtx = store.read().unwrap();
    let mut out = Vec::new();
    rtx.for_each(KIND_NOTE, |r| {
        if let Read::Ok(e) = r {
            out.push(e.body_as().unwrap());
        }
        true
    })
    .unwrap();
    out
}

fn embedder() -> ReplayClient {
    ReplayClient::from_json(
        r#"{
          "props": {"build_info": "b1-test", "model_path": "/m/emb.gguf", "total_slots": 4,
                    "default_generation_settings": {"n_ctx": 8192}},
          "models": {"data": [{"id": "emb", "meta": {"n_params": 5, "size": 9, "n_vocab": 7}}]},
          "embed": {"dim": 16}
        }"#,
    )
    .unwrap()
}

fn open(root: &Path, clock: Arc<ManualClock>, seed: u64) -> evorag_store::Store {
    evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    })
    .unwrap()
    .0
}

fn file_contains(file: &Path, text: &str) -> bool {
    std::fs::read(file)
        .map(|b| b.windows(text.len()).any(|w| w == text.as_bytes()))
        .unwrap_or(false)
}

const T0: i64 = 1_762_650_000_000;
const DAY: i64 = 86_400_000;
/// 消すセッションのノートの鍵 (退避に残っていないことを、この文字で確かめる)。
const MARK: &str = "孤立した鍵の印";

/// verifies: AC-NFR-307-2 (v0.1.2 でセッションを消した後に v0.2 で開くと、出典の無くなった派生の記録と想起への
/// 操作が DB と退避から消え、索引から除かれる。v0.1.2 で足した会話はチャンクと埋め込みが作られる)
#[tokio::test(flavor = "multi_thread")]
async fn records_left_by_an_older_version_are_cleaned_up() {
    let dir = TempDir::new("orphans");
    prepare(dir.path(), true);
    // 1. v0.2 で 2 つのセッションに会話し、ノートと埋め込みを作る (ノートは新しいセッションから作る)。
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0)));
    let keys_b = reply(r#"{"keys": ["車の手入れ"]}"#);
    let keys_a = reply(&format!(r#"{{"keys": ["{MARK}"]}}"#));
    let llm = chat(&[reply(REPLY), reply(REPLY), keys_b, keys_a], false);
    let mut cfg = config(dir.path(), clock.clone());
    cfg.embed = Some(Arc::new(embedder()));
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    core.command(Command::NewSession {
        title: "植物".into(),
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    let a = say(&core, &mut rx, "モンステラの水やり").await;
    core.command(Command::NewSession {
        title: "車".into()
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    let b = say(&core, &mut rx, "オイル交換").await;
    clock.advance(SEGMENT_QUIET_MS);
    core.settle_background().await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0, "both notes were made");

    // 2. 古い版をまねる: 想起への操作を足し、その日の退避を作ってから、セッション A のターンとセッションだけを
    //    消し、セッション B に発話を足す (チャンクは作らない)。
    let added;
    {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0 + DAY)));
        let store = Arc::new(open(dir.path(), clock.clone(), 31));
        let (w, h) = evorag_store::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || w.run());
        let writes = evorag_memory::Writes {
            clock: &*clock,
            ids: &**store.ids(),
        };
        let (_, ops) = writes
            .recall_feedback(&evorag_memory::RecallFeedbackBody {
                op: evorag_memory::FEEDBACK_IRRELEVANT.into(),
                answer: a.id,
                target: a.reply_to.unwrap(),
                score: None,
                reranker: None,
            })
            .unwrap();
        h.submit_wait(evorag_store::WriteRequest::conversation(ops))
            .unwrap();
        store
            .daily_backup(None)
            .unwrap()
            .expect("a backup for the day");
        let (session_a, turns_a, session_b) = {
            let r = store.read().unwrap();
            let sa = evorag_memory::load_session(&r, a.session).unwrap();
            let ta: Vec<_> = evorag_memory::session_turns(&r, a.session)
                .unwrap()
                .into_iter()
                .map(|t| t.envelope.id)
                .collect();
            let sb = evorag_memory::load_session(&r, b.session).unwrap();
            (sa.envelope, ta, sb.envelope)
        };
        let mut ops = Vec::new();
        for id in turns_a {
            ops.push(evorag_store::WriteOp::Delete {
                kind: evorag_memory::KIND_TURN.into(),
                id,
            });
            ops.push(evorag_store::WriteOp::DeleteLinksOf(id));
        }
        ops.push(evorag_store::WriteOp::DeleteLookup {
            kind: evorag_memory::KIND_SESSION.into(),
            field: "updated_at".into(),
            value: evorag_store::key::OwnedPart::Int(session_a.updated_at.as_millis()),
            id: session_a.id,
        });
        ops.push(evorag_store::WriteOp::Delete {
            kind: evorag_memory::KIND_SESSION.into(),
            id: session_a.id,
        });
        ops.push(evorag_store::WriteOp::DeleteLinksOf(session_a.id));
        let (id, more) = writes.user_turn(&session_b, "古い版で足した発話").unwrap();
        ops.extend(more);
        added = id;
        h.submit_wait(evorag_store::WriteRequest::conversation(ops))
            .unwrap();
        h.shutdown();
        t.join().unwrap();
    }

    // 3. v0.2 で開き直す (同じ日。退避の整理と、足りないチャンクと埋め込みを作るのを待つ)。
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0 + DAY + 1_000)));
    let mut cfg = config(dir.path(), clock.clone());
    cfg.ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), 41));
    cfg.embed = Some(Arc::new(embedder()));
    let (core, _rx, _) = Core::start(cfg, Arc::new(chat(&[], false))).await.unwrap();
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;

    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0 + DAY + 2_000)));
    let store = open(dir.path(), clock, 51);
    let r = store.read().unwrap();
    let mut notes = Vec::new();
    r.for_each(KIND_NOTE, |x| {
        if let Read::Ok(e) = x {
            notes.push((e.id, e.body_as::<NoteBody>().unwrap()));
        }
        true
    })
    .unwrap();
    assert_eq!(notes.len(), 1, "only the note of the kept session");
    assert_eq!(notes[0].1.session, b.session);
    let mut feedback = 0;
    r.for_each(evorag_memory::KIND_RECALL_FEEDBACK, |_| {
        feedback += 1;
        true
    })
    .unwrap();
    assert_eq!(feedback, 0, "the feedback on the deleted session is gone");
    let mut targets: std::collections::HashSet<_> = notes.iter().map(|(id, _)| *id).collect();
    let mut added_chunks = Vec::new();
    r.for_each(evorag_memory::KIND_CHUNK, |x| {
        if let Read::Ok(e) = x {
            let c: evorag_memory::ChunkBody = e.body_as().unwrap();
            assert_ne!(c.session, a.session, "no chunk of the deleted session");
            if c.turn == added {
                added_chunks.push(e.id);
            }
            targets.insert(e.id);
        }
        true
    })
    .unwrap();
    assert!(
        !added_chunks.is_empty(),
        "the turn added by the older version is chunked"
    );
    let embeddings = evorag_memory::embedding_bodies(&r).unwrap();
    assert!(
        embeddings.iter().all(|(_, b)| targets.contains(&b.target)),
        "no embedding without its chunk or note"
    );
    assert!(
        embeddings
            .iter()
            .any(|(_, b)| added_chunks.contains(&b.target)),
        "the added turn is embedded"
    );
    let mut jobs = 0;
    r.for_each(evorag_memory::KIND_JOB, |_| {
        jobs += 1;
        true
    })
    .unwrap();
    assert_eq!(jobs, 0, "the purge job is done");
    let daily = dir.path().join("backups").join("daily");
    let backups: Vec<_> = std::fs::read_dir(&daily)
        .unwrap()
        .map(|e| e.unwrap().path().join("evorag.redb"))
        .collect();
    assert!(!backups.is_empty());
    for b in backups {
        assert!(!file_contains(&b, MARK), "{}", b.display());
    }
}
