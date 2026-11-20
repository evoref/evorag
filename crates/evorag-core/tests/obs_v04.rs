//! v0.4 の観測値 (05 §3.6、FR-420): 発話と応答のターンに残す項目。llama-server は記録の再生で置き換える。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{TurnBody, TurnRole};
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const START: i64 = 1_762_650_000_000;

fn cassette() -> ReplayClient {
    let ok = r#"{"events": [{"delta": "了解"}, {"done": {"finish_reason": "stop"}}]}"#;
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{ok}, {ok}, {ok}, {ok}]
        }}"#
    ))
    .unwrap()
}

fn config(root: &Path, clock: Arc<ManualClock>) -> CoreConfig {
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.4.0".into(),
        prompt_language: PromptLanguage::Ja,
        prompt_language_explicit: true,
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
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 5)),
    }
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        if pred(&ev) {
            return;
        }
    }
}

fn turns(root: &Path) -> Vec<TurnBody> {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::from_millis(START)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.4.0".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        config_file: None,
    })
    .unwrap();
    let r = store.read().unwrap();
    evorag_memory::list_sessions(&r, usize::MAX)
        .unwrap()
        .into_iter()
        .flat_map(|s| evorag_memory::session_turns(&r, s.envelope.id).unwrap())
        .map(|t| t.body)
        .collect()
}

/// verifies: AC-FR-420-1
#[tokio::test(flavor = "multi_thread")]
async fn utterances_and_answers_keep_the_v0_4_observations() {
    let dir = TempDir::new("core-obs-v04");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(START)));
    let llm = cassette();
    let (core, mut rx, _) = Core::start(config(dir.path(), clock.clone()), Arc::new(llm))
        .await
        .unwrap();
    // 起動の突き合わせが済むまで待つ (起動したときに残っていた手順を数え終える)。
    core.settle().await;
    core.command(Command::NewSession {
        title: "試験".into(),
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::Send {
        text: "最初の発話".into(),
    });
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    clock.advance(90_000);
    core.command(Command::Send {
        text: "二つ目の発話".into(),
    });
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.shutdown().await;

    let all = turns(dir.path());
    let users: Vec<&TurnBody> = all.iter().filter(|t| t.role == TurnRole::User).collect();
    let answers: Vec<&TurnBody> = all
        .iter()
        .filter(|t| t.role == TurnRole::Assistant)
        .collect();
    assert_eq!((users.len(), answers.len()), (2, 2));

    // 発話: UTC からのずれ、前の発話からの時間、起動したときに残っていた手順 (最初の発話だけ)。
    let offset = evorag_core::localtime::offset(START).map(|s| s / 60);
    let first = users[0]
        .obs
        .as_ref()
        .expect("the first utterance has observations");
    assert_eq!(first.tz_offset, offset);
    assert_eq!(first.idle, None, "no earlier utterance in the session");
    assert!(first.pending_at_start.is_some());
    let second = users[1].obs.as_ref().expect("observations");
    assert_eq!(second.idle, Some(90));
    assert_eq!(
        second.pending_at_start, None,
        "only the first utterance after the start"
    );

    // 応答: 問いの分析、先読み (v0.4 の M4.1 では無し) と応答が始まるまでの時間、作りの版。
    for a in &answers {
        let o = a.obs.as_ref().expect("answers have observations");
        assert!(o.query.is_some());
        let p = o.prefetch.as_ref().expect("prefetch observation");
        assert_eq!(p.kind, evorag_memory::PREFETCH_NONE);
        assert!(p.ttft_ms.is_some());
        assert!(o.versions.contains_key("recall") && o.versions.contains_key("context"));
        assert_eq!(o.tz_offset, None, "the offset belongs to the utterance");
    }
    // 下書きと引用の本文は観測値に入らない (ID と数値だけ)。
    for t in &all {
        let o = serde_json::to_string(&t.obs).unwrap();
        assert!(!o.contains("発話"), "{o}");
    }
}
