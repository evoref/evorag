//! 答え方の既定 (`/style`、FR-111、07 §3)。llama-server は記録の再生で置き換える (09 §3.3)。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, StyleItem};
use evorag_types::{ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const OK: &str = r#"{"events": [{"delta": "了解"}, {"done": {}}]}"#;

fn cassette(replies: usize) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![OK; replies].join(", ")
    ))
    .unwrap()
}

async fn start(root: &Path, llm: &ReplayClient, seed: u64) -> (Core, UnboundedReceiver<Event>) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let config = CoreConfig {
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
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    };
    let (core, rx, _) = Core::start(config, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    (core, rx)
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

async fn style(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    set: Option<(StyleItem, &str)>,
) -> Event {
    core.command(Command::Style {
        set: set.map(|(i, v)| (i, v.to_owned())),
    });
    until(rx, |e| {
        matches!(e, Event::StyleInfo { .. } | Event::Rejected { .. })
    })
    .await
    .pop()
    .unwrap()
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) {
    core.command(Command::Send { text: text.into() });
    until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
}

fn system(llm: &ReplayClient, i: usize) -> String {
    llm.requests()[i].messages[0].content.clone()
}

fn values(ev: &Event) -> (&[String; 3], bool) {
    match ev {
        Event::StyleInfo { values, changed } => (values, *changed),
        other => panic!("{other:?}"),
    }
}

/// verifies: AC-FR-111-1
#[tokio::test(flavor = "multi_thread")]
async fn a_chosen_style_takes_effect_at_once_and_survives_a_restart_and_a_lost_database() {
    let dir = TempDir::new("style");
    let llm = cassette(2);
    let (core, mut rx) = start(dir.path(), &llm, 21).await;
    let ev = style(&core, &mut rx, None).await;
    assert_eq!(
        values(&ev),
        (&["auto".into(), "auto".into(), "auto".into()], false)
    );

    core.command(Command::NewSession {
        title: "試験".into(),
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    say(&core, &mut rx, "一つ目").await;
    let ev = style(&core, &mut rx, Some((StyleItem::Length, "short"))).await;
    assert_eq!(
        values(&ev),
        (&["auto".into(), "short".into(), "auto".into()], true)
    );
    // 選び直しても同じ値なら「変えた」と言わない。
    let ev = style(&core, &mut rx, Some((StyleItem::Length, "short"))).await;
    assert!(!values(&ev).1);
    // 次の発話から効く (選べる既定の文が替わる)。
    say(&core, &mut rx, "二つ目").await;
    assert!(!system(&llm, 0).contains("短く"));
    assert!(
        system(&llm, 1).contains("答えは短く"),
        "{}",
        system(&llm, 1)
    );

    // 知らない値は断り、選べる値を示す。記録は変わらない。
    let ev = style(&core, &mut rx, Some((StyleItem::Format, "fancy"))).await;
    let Event::Rejected { error } = ev else {
        panic!("{ev:?}")
    };
    assert_eq!(error.key, MessageKey::StyleUnknownValue);
    core.shutdown().await;

    // 起動し直しても残る (データ根に記録した)。
    let llm = cassette(0);
    let (core, mut rx) = start(dir.path(), &llm, 22).await;
    let ev = style(&core, &mut rx, None).await;
    assert_eq!(values(&ev).0[1], "short");
    core.shutdown().await;

    // DB を失っても、ジャーナルから戻る (利用者の明示の操作、03 §6)。
    std::fs::remove_file(dir.path().join("evorag.redb")).unwrap();
    let (core, mut rx) = start(dir.path(), &llm, 23).await;
    let ev = style(&core, &mut rx, None).await;
    assert_eq!(values(&ev).0[1], "short");
    core.shutdown().await;
}

/// 新しい版で足した値の記録は auto として扱う (07 §3、03 §3.3)。
/// verifies: AC-FR-111-1 (知らない値)
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_stored_value_reads_as_auto() {
    let dir = TempDir::new("style-unknown");
    let llm = cassette(0);
    let (core, mut rx) = start(dir.path(), &llm, 31).await;
    style(&core, &mut rx, Some((StyleItem::Format, "plain"))).await;
    core.shutdown().await;

    // 新しい版が書いたことにして、値を知らないものに替える。
    {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_100_000)));
        let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.0.6".into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 32)),
            config_file: None,
        })
        .unwrap();
        let store = Arc::new(store);
        let settings = evorag_memory::load_settings(&store.read().unwrap()).unwrap();
        let ids = UuidV7Gen::with_seed(clock.clone(), 33);
        let writes = evorag_memory::Writes {
            clock: &*clock,
            ids: &ids,
        };
        let ops = writes
            .set_setting(
                &settings,
                evorag_memory::SETTING_ANSWER_FORMAT,
                "bullet-poetry",
            )
            .unwrap();
        let (writer, handle) = evorag_store::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        handle
            .submit_wait(evorag_store::WriteRequest::conversation(ops))
            .unwrap();
        handle.shutdown();
        t.join().unwrap();
    }

    let (core, mut rx) = start(dir.path(), &llm, 34).await;
    let ev = style(&core, &mut rx, None).await;
    assert_eq!(values(&ev).0[2], "auto");
    core.shutdown().await;
}
