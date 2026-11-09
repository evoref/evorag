//! ヘッドレスのチャット経路の試験 (04 §3、06 §3・§7)。llama-server は記録の再生で置き換える (09 §3.3)。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::{Core, CoreConfig, PromptLanguage, StartError};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Generation, Role};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event, TurnStatus};
use evorag_types::{ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

fn cassette(n_ctx: u32, chats: &str) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": {n_ctx}}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{chats}]
        }}"#
    ))
    .unwrap()
}

const HELLO: &str = r#"{"events": [{"delta": "こん"}, {"delta": "にち"}, {"delta": "は"}, {"done": {"finish_reason": "stop"}}]}"#;
const OK: &str = r#"{"events": [{"delta": "了解"}, {"done": {}}]}"#;
const HANG: &str = r#"{"events": [{"delta": "考え"}, {"hang": true}]}"#;
const CUT: &str = r#"{"events": [{"delta": "途中"}, {"cut": "connection reset"}]}"#;

fn config(root: &Path, lang: PromptLanguage) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.3".into(),
        prompt_language: lang,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "test".into(),
        models: Vec::new(),
        connect: None,
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 5)),
    }
}

async fn start(root: &Path, llm: &ReplayClient) -> (Core, UnboundedReceiver<Event>) {
    let (core, rx, _) = Core::start(config(root, PromptLanguage::Ja), Arc::new(llm.clone()))
        .await
        .unwrap();
    (core, rx)
}

/// `pred` に合うイベントまで集める。
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

fn finished(ev: &Event) -> bool {
    matches!(ev, Event::ResponseFinished { .. })
}

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>) {
    core.command(Command::NewSession {
        title: "試験".into(),
    });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

/// verifies: AC-FR-101-1, AC-FR-508-1, AC-FR-501-1
#[tokio::test(flavor = "multi_thread")]
async fn tokens_arrive_in_order_and_the_reply_is_saved_with_the_model_key() {
    let dir = TempDir::new("core-hello");
    let llm = cassette(8192, HELLO);
    let (core, mut rx, report) = Core::start(
        config(dir.path(), PromptLanguage::Ja),
        Arc::new(llm.clone()),
    )
    .await
    .unwrap();
    assert_eq!((report.server.n_ctx, report.server.total_slots), (8192, 2));
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "こんにちは".into(),
    });
    let events = until(&mut rx, finished).await;

    let tokens: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Token { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tokens, ["こん", "にち", "は"]);
    assert!(matches!(events[0], Event::UserTurnSaved { .. }));
    let Some(Event::ResponseFinished { turn, notice }) = events.last() else {
        panic!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.text, "こんにちは");
    assert_eq!(notice, &None);
    assert_eq!(
        turn.model_key.as_deref(),
        Some(core.server().model_key.as_str())
    );

    // 送った要求: システムプロンプト + 発話。応答は最初のスロット。
    let req = &llm.requests()[0];
    assert_eq!(req.messages[0].role, Role::System);
    assert_eq!(req.messages.last().unwrap().content, "こんにちは");
    assert_eq!(req.id_slot, Some(0));
    core.shutdown().await;
}

/// verifies: AC-FR-102-1
#[tokio::test(flavor = "multi_thread")]
async fn interrupting_closes_the_connection_and_saves_the_partial_reply() {
    let dir = TempDir::new("core-interrupt");
    let llm = cassette(8192, HANG);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "長い話を".into(),
    });
    until(&mut rx, |e| matches!(e, Event::Token { .. })).await;
    core.command(Command::Interrupt);
    let events = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn, notice }) = events.last() else {
        panic!()
    };
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(turn.text, "考え");
    assert_eq!(
        notice.as_ref().map(|m| m.key),
        Some(MessageKey::Interrupted)
    );
    assert_eq!(llm.dropped_early(), 1, "the stream was not dropped");
    core.shutdown().await;
}

/// verifies: AC-FR-106-1
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reply_keeps_the_utterance_and_can_be_resent() {
    let dir = TempDir::new("core-fail");
    let llm = cassette(8192, &format!("{CUT}, {OK}"));
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "質問".into(),
    });
    let events = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn, notice }) = events.last() else {
        panic!()
    };
    assert_eq!(turn.status, TurnStatus::Failed);
    assert_eq!(turn.text, "途中");
    assert_eq!(
        notice.as_ref().map(|m| m.key),
        Some(MessageKey::LlmStreamCut)
    );
    let sid = turn.session;

    core.command(Command::Resend);
    let events = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn, .. }) = events.last() else {
        panic!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    // 同じ発話を送り直した。失敗した応答は送らない。
    let reqs = llm.requests();
    assert_eq!(reqs[1].messages.last().unwrap().content, "質問");
    assert!(reqs[1].messages.iter().all(|m| m.content != "途中"));
    let turns = core.turns(sid);
    assert_eq!(turns.len(), 3, "utterance, failed reply, new reply");
    assert_eq!(turns[0].status, TurnStatus::Confirmed);
    core.shutdown().await;
}

/// verifies: AC-FR-103-1
#[tokio::test(flavor = "multi_thread")]
async fn retry_creates_a_new_reply_keeps_the_old_one_and_records_a_signal() {
    let dir = TempDir::new("core-retry");
    let llm = cassette(8192, &format!("{HELLO}, {OK}"));
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "やあ".into(),
    });
    let first = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn: old, .. }) = first.last() else {
        panic!()
    };
    core.command(Command::Retry);
    let second = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn: new, .. }) = second.last() else {
        panic!()
    };
    assert_ne!(old.id, new.id);
    assert_eq!(new.reply_to, old.reply_to);
    let turns = core.turns(old.session);
    assert_eq!(turns.len(), 3);
    // 作り直しの要求には、前の応答を含めない。
    let reqs = llm.requests();
    assert!(reqs[1].messages.iter().all(|m| m.content != "こんにちは"));
    core.shutdown().await;
    assert_eq!(signals_on(dir.path(), old.id), 1);
}

fn signals_on(root: &Path, target: evorag_types::Id) -> usize {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    let (store, _) = evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.3".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap();
    store
        .read()
        .unwrap()
        .links_to(target, evorag_memory::REL_TARGET)
        .unwrap()
        .len()
}

/// verifies: AC-FR-503-1 (コアの層: 起動しない。終了コードと案内は bin で判定する)
#[tokio::test(flavor = "multi_thread")]
async fn start_fails_when_llama_server_is_down() {
    let dir = TempDir::new("core-down");
    let llm = ReplayClient::from_json(r#"{"props": {}, "models": {}, "health": false}"#).unwrap();
    let r = Core::start(config(dir.path(), PromptLanguage::Ja), Arc::new(llm)).await;
    assert!(matches!(r, Err(StartError::LlmUnreachable(_))));
    // データ根も作らない (繋がらないまま DB を作らない)。
    assert!(!dir.path().join("evorag.redb").exists());
}

/// verifies: AC-FR-506-1, AC-FR-406-1 (n_ctx は 1,100 で、4,096 より厳しい)
#[tokio::test(flavor = "multi_thread")]
async fn the_context_fits_the_budget_and_token_counts_are_cached() {
    let dir = TempDir::new("core-budget");
    // n_ctx 1100 - 予備 1024 = 76 トークン (文字数で数える記録の再生)。
    let llm = cassette(1100, &[OK; 4].join(", "));
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    let long = "あ".repeat(20);
    for _ in 0..3 {
        core.command(Command::Send { text: long.clone() });
        until(&mut rx, finished).await;
    }
    let calls_before = core.tokenize_calls();
    core.command(Command::Send {
        text: "最後".into(),
    });
    until(&mut rx, finished).await;
    // 新しく数えたのは、新しい発話と前の応答 (と日時の変わらないシステムプロンプトは数えない)。
    assert!(
        core.tokenize_calls() - calls_before <= 2,
        "{}",
        core.tokenize_calls() - calls_before
    );
    let last = llm.requests().last().unwrap().clone();
    assert_eq!(last.messages.last().unwrap().content, "最後");
    // 古い発話は予算に入らず省かれた。
    let kept = last.messages.iter().filter(|m| m.content == long).count();
    assert!(kept < 3, "kept {kept} long utterances");
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_while_generating_is_rejected_and_shutdown_saves_the_partial_reply() {
    let dir = TempDir::new("core-busy");
    let llm = cassette(8192, HANG);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "一つ目".into(),
    });
    let events = until(&mut rx, |e| matches!(e, Event::Token { .. })).await;
    let Some(Event::UserTurnSaved { turn }) = events.first() else {
        panic!()
    };
    let sid = turn.session;
    core.command(Command::Send {
        text: "二つ目".into(),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    let Some(Event::Rejected { error }) = ev.last() else {
        panic!()
    };
    assert_eq!(error.key, MessageKey::Busy);

    let reader = core.clone();
    core.shutdown().await;
    let turns = reader.turns(sid);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].status, TurnStatus::Interrupted);
    assert_eq!(turns[1].text, "考え");
}

/// 07 §6.1: プロンプトの言語は最初の起動で記録し、後で設定を変えても変わらない。
/// verifies: AC-FR-909-1, AC-FR-909-2
#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_language_is_fixed_per_data_root() {
    let dir = TempDir::new("core-lang");
    let llm = cassette(8192, "");
    let (core, _rx, report) = Core::start(
        config(dir.path(), PromptLanguage::En),
        Arc::new(llm.clone()),
    )
    .await
    .unwrap();
    assert_eq!(report.prompt_language, PromptLanguage::En);
    core.shutdown().await;
    let (core, _rx, report) = Core::start(config(dir.path(), PromptLanguage::Ja), Arc::new(llm))
        .await
        .unwrap();
    assert_eq!(report.prompt_language, PromptLanguage::En);
    assert!(report.prompt_language_mismatch);
    core.shutdown().await;
}

/// verifies: AC-FR-105-1 (コアの層: 消したセッションは一覧から消える。確認は画面の層)
#[tokio::test(flavor = "multi_thread")]
async fn sessions_can_be_renamed_and_deleted() {
    let dir = TempDir::new("core-session-ops");
    let llm = cassette(8192, OK);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "残す?".into(),
    });
    until(&mut rx, finished).await;
    core.command(Command::RenameSession {
        title: "改名した".into(),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::SessionRenamed { .. })).await;
    let Some(Event::SessionRenamed { session }) = ev.last() else {
        panic!()
    };
    assert_eq!(session.title, "改名した");
    assert_eq!(core.sessions(10)[0].title, "改名した");

    core.command(Command::DeleteSession);
    let ev = until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    let Some(Event::SessionDeleted { id }) = ev.last() else {
        panic!()
    };
    assert_eq!(*id, session.id);
    assert!(core.sessions(10).is_empty());
    assert!(core.turns(session.id).is_empty());
    // 開いているセッションは無くなる。
    core.command(Command::Send { text: "x".into() });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    let Some(Event::Rejected { error }) = ev.last() else {
        panic!()
    };
    assert_eq!(error.key, MessageKey::NoSession);
    core.shutdown().await;
}

/// verifies: AC-FR-311-1 (`/good`・`/bad 理由` は直前の応答への関係を持つ信号になる)
#[tokio::test(flavor = "multi_thread")]
async fn ratings_are_recorded_against_the_last_reply() {
    let dir = TempDir::new("core-rate");
    let llm = cassette(8192, OK);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Rate {
        good: true,
        reason: None,
    });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    let Some(Event::Rejected { error }) = ev.last() else {
        panic!()
    };
    assert_eq!(error.key, MessageKey::NothingToRate);

    core.command(Command::Send {
        text: "評価して".into(),
    });
    let ev = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    let reply = turn.id;
    core.command(Command::Rate {
        good: true,
        reason: None,
    });
    until(&mut rx, |e| matches!(e, Event::Rated { good: true })).await;
    core.command(Command::Rate {
        good: false,
        reason: Some("違う".into()),
    });
    until(&mut rx, |e| matches!(e, Event::Rated { good: false })).await;
    core.shutdown().await;
    assert_eq!(signals_on(dir.path(), reply), 2);
}

/// verifies: AC-FR-202-1
#[tokio::test(flavor = "multi_thread")]
async fn a_journal_failure_keeps_the_utterance_out_of_the_database() {
    let dir = TempDir::new("core-journal-fail");
    let llm = cassette(8192, OK);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    let sid = core.sessions(1)[0].id;
    core.inject_journal_failure();
    core.command(Command::Send {
        text: "保存できない発話".into(),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::SendRejected { .. })).await;
    let Some(Event::SendRejected { text, error }) = ev.last() else {
        panic!()
    };
    assert_eq!(text, "保存できない発話");
    assert_eq!(error.key, MessageKey::SendNotSaved);
    assert!(core.turns(sid).is_empty(), "nothing may reach the database");
    assert!(llm.requests().is_empty(), "nothing may be sent to the LLM");
    // 次は書ける。
    core.command(Command::Send { text: "次".into() });
    until(&mut rx, finished).await;
    assert_eq!(core.turns(sid).len(), 2);
    core.shutdown().await;
}

fn file_contains(file: &Path, text: &str) -> bool {
    std::fs::read(file)
        .map(|b| b.windows(text.len()).any(|w| w == text.as_bytes()))
        .unwrap_or(false)
}

/// verifies: AC-NFR-506-1 の退避の部分 (03 §7.3: 消したセッションの本文は、退避からも除かれる)
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_purges_it_from_the_backups() {
    let dir = TempDir::new("core-purge");
    let llm = cassette(8192, OK);
    let (core, mut rx) = start(dir.path(), &llm).await;
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "退避から消える本文".into(),
    });
    until(&mut rx, finished).await;
    core.shutdown().await;

    // 次の日に起動し直すと、起動時の退避に本文が入る。
    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.clock = Arc::new(ManualClock::new(Timestamp::from_millis(
        1_762_650_000_000 + 86_400_000,
    )));
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    let sid = core.sessions(1)[0].id;
    core.command(Command::OpenSession { id: sid });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.shutdown().await;

    let daily = dir.path().join("backups").join("daily");
    let backups: Vec<_> = std::fs::read_dir(&daily)
        .unwrap()
        .map(|e| e.unwrap().path().join("evorag.redb"))
        .collect();
    assert_eq!(backups.len(), 2);
    for b in backups {
        assert!(!file_contains(&b, "退避から消える本文"), "{}", b.display());
    }
}

fn unsafe_place(_: &Path) -> evorag_store::location::Verdict {
    evorag_store::location::Verdict::Unsafe("synced folder (test)".into())
}

fn no_space(_: &Path) -> Option<u64> {
    Some(0)
}

/// verifies: AC-FR-216-1 (コアの層: 既定で起動を拒否し、設定で警告だけにできる)
#[tokio::test(flavor = "multi_thread")]
async fn an_unsafe_location_is_refused_unless_allowed() {
    let dir = TempDir::new("core-unsafe");
    let llm = cassette(8192, "");
    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.probe.location = unsafe_place;
    let r = Core::start(cfg, Arc::new(llm.clone())).await;
    assert!(matches!(r, Err(StartError::UnsafeLocation(_))));
    assert!(!dir.path().join("evorag.redb").exists());

    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.probe.location = unsafe_place;
    cfg.allow_unsafe_location = true;
    let (core, _rx, report) = Core::start(cfg, Arc::new(llm)).await.unwrap();
    assert_eq!(report.location_warnings.len(), 1);
    core.shutdown().await;
}

/// verifies: AC-FR-217-1
#[tokio::test(flavor = "multi_thread")]
async fn low_space_pauses_background_work_but_the_conversation_continues() {
    let dir = TempDir::new("core-low-space");
    let llm = cassette(8192, OK);
    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.probe.free_bytes = no_space;
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm)).await.unwrap();
    let ev = until(&mut rx, |e| matches!(e, Event::BackgroundPaused { .. })).await;
    let Some(Event::BackgroundPaused { reason }) = ev.last() else {
        panic!()
    };
    assert_eq!(reason.key, MessageKey::LowSpace);
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "空きが無くても話せる".into(),
    });
    let ev = until(&mut rx, finished).await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    core.shutdown().await;
    assert!(
        !dir.path().join("backups").join("daily").exists(),
        "no backup was taken"
    );
}

/// 07 §2.2・§4.2: 思考は 1 回だけ知らせ、本文に入れない。request_extra は要求に足される。
#[tokio::test(flavor = "multi_thread")]
async fn thinking_is_announced_once_and_not_saved() {
    let dir = TempDir::new("core-thinking");
    let llm = cassette(
        8192,
        r#"{"events": [{"reasoning": "考え1"}, {"reasoning": "考え2"}, {"delta": "答え"}, {"done": {}}]}"#,
    );
    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.request_extra.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({"enable_thinking": false}),
    );
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    new_session(&core, &mut rx).await;
    core.command(Command::Send {
        text: "考えて".into(),
    });
    let events = until(&mut rx, finished).await;
    let thinking = events
        .iter()
        .filter(|e| matches!(e, Event::Thinking { .. }))
        .count();
    assert_eq!(thinking, 1);
    let Some(Event::ResponseFinished { turn, .. }) = events.last() else {
        panic!()
    };
    assert_eq!(turn.text, "答え");
    assert_eq!(
        llm.requests()[0].extra["chat_template_kwargs"]["enable_thinking"],
        false
    );
    core.shutdown().await;
}

/// FR-108: `/model` で今のモデルと候補を示し、候補に切り替える。繋がらなければ今のまま。
#[tokio::test(flavor = "multi_thread")]
async fn the_model_can_be_switched_to_a_configured_choice() {
    let dir = TempDir::new("core-model");
    let a = cassette(8192, OK);
    let b = cassette(4096, OK);
    let down = ReplayClient::from_json(r#"{"props": {}, "models": {}, "health": false}"#).unwrap();
    let mut cfg = config(dir.path(), PromptLanguage::Ja);
    cfg.model_name = "a".into();
    cfg.models = vec![
        evorag_core::ModelChoice {
            name: "b".into(),
            url: "replay-b".into(),
        },
        evorag_core::ModelChoice {
            name: "down".into(),
            url: "replay-down".into(),
        },
    ];
    let (bb, dd) = (b.clone(), down.clone());
    cfg.connect = Some(Arc::new(move |u: &str| {
        Ok(match u {
            "replay-b" => Arc::new(bb.clone()) as Arc<dyn evorag_llm::LlmClient>,
            _ => Arc::new(dd.clone()) as Arc<dyn evorag_llm::LlmClient>,
        })
    }));
    let (core, mut rx, _) = Core::start(cfg, Arc::new(a.clone())).await.unwrap();
    new_session(&core, &mut rx).await;

    core.command(Command::Model { name: None });
    let ev = until(&mut rx, |e| matches!(e, Event::ModelInfo { .. })).await;
    let Some(Event::ModelInfo {
        current,
        choices,
        n_ctx,
        ..
    }) = ev.last()
    else {
        panic!()
    };
    assert_eq!((current.as_str(), *n_ctx), ("a", 8192));
    assert_eq!(choices, &["b", "down"]);

    core.command(Command::Model {
        name: Some("nope".into()),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    let Some(Event::Rejected { error }) = ev.last() else {
        panic!()
    };
    assert_eq!(error.key, MessageKey::UnknownModel);

    core.command(Command::Model {
        name: Some("down".into()),
    });
    until(&mut rx, |e| matches!(e, Event::Rejected { .. })).await;
    assert_eq!(
        core.server().n_ctx,
        8192,
        "a failed switch keeps the current model"
    );

    core.command(Command::Model {
        name: Some("b".into()),
    });
    let ev = until(&mut rx, |e| matches!(e, Event::ModelInfo { .. })).await;
    let Some(Event::ModelInfo { current, n_ctx, .. }) = ev.last() else {
        panic!()
    };
    assert_eq!((current.as_str(), *n_ctx), ("b", 4096));
    core.command(Command::Send {
        text: "新しいモデルへ".into(),
    });
    until(&mut rx, finished).await;
    assert_eq!(b.requests().len(), 1);
    assert!(a.requests().is_empty());
    core.shutdown().await;
}
