//! 文脈の並べ方 (05 §4.1・§4.2、ADR-0017)。llama-server は記録の再生で置き換える (09 §3.3)。
//!
//! 応答が始まるまでの時間は CI で測れないので、再生のサーバが受け取った要求を並べ、
//! 連続する要求の先頭のバイト列が一致することを確かめる (OQ-35)。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{ChatMessage, Generation};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const OK: &str = r#"{"events": [{"delta": "了解"}, {"done": {}}]}"#;
const HOUR: i64 = 3_600_000;

fn cassette(n_ctx: u32, replies: usize) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": {n_ctx}}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![OK; replies].join(", ")
    ))
    .unwrap()
}

async fn start(
    root: &Path,
    llm: &ReplayClient,
    reserve: Option<usize>,
) -> (Core, UnboundedReceiver<Event>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let config = CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.6".into(),
        prompt_language: PromptLanguage::Ja,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: reserve,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "test".into(),
        ignored_config: Vec::new(),
        memory_enabled: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 11)),
    };
    let (core, rx, _) = Core::start(config, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    (core, rx, clock)
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

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>, title: &str) {
    core.command(Command::NewSession {
        title: title.into(),
    });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) {
    core.command(Command::Send { text: text.into() });
    until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
}

fn messages(llm: &ReplayClient, i: usize) -> Vec<ChatMessage> {
    llm.requests()[i].messages.clone()
}

/// 前の要求 `a` の今の発話までが、次の要求 `b` の先頭とバイト列で一致する。前の要求の今の発話に
/// 引用が付いていたら、次の要求ではその引用を外した形になっている。
fn assert_prefix(a: &[ChatMessage], b: &[ChatMessage]) {
    let n = a.len() - 1;
    assert!(b.len() > n, "the next request is shorter");
    for i in 0..n {
        assert_eq!(a[i].role, b[i].role, "message {i}");
        assert_eq!(a[i].content, b[i].content, "message {i} changed");
    }
    assert!(
        a[n].content.ends_with(&b[n].content),
        "the last utterance changed beyond dropping the quote:\n{}\n---\n{}",
        a[n].content,
        b[n].content
    );
    assert!(
        !b[n].content.contains("<<引用-"),
        "the quote stayed in the history"
    );
}

/// verifies: AC-FR-414-1, AC-FR-414-3
#[tokio::test(flavor = "multi_thread")]
async fn consecutive_requests_share_their_prefix_even_after_a_recall() {
    let dir = TempDir::new("context-prefix");
    let llm = cassette(8192, 5);
    let (core, mut rx, clock) = start(dir.path(), &llm, None).await;
    new_session(&core, &mut rx, "観葉植物の相談").await;
    say(
        &core,
        &mut rx,
        "モンステラの水やりは、どれくらいの頻度がいい？",
    )
    .await;

    new_session(&core, &mut rx, "別の話").await;
    say(&core, &mut rx, "今日は晴れていますね").await;
    clock.advance(HOUR);
    // 以前の会話が添えられる (引用は今の発話の直前)。
    say(
        &core,
        &mut rx,
        "前に聞いたモンステラの水やりの頻度、もう一度教えて",
    )
    .await;
    clock.advance(HOUR);
    say(&core, &mut rx, "ありがとう").await;
    clock.advance(HOUR);
    say(&core, &mut rx, "では、また").await;
    core.shutdown().await;

    let reqs: Vec<_> = (1..5).map(|i| messages(&llm, i)).collect();
    assert!(
        reqs[1].last().unwrap().content.contains("<<引用-"),
        "the recall must fire for this test"
    );
    for w in reqs.windows(2) {
        assert_prefix(&w[0], &w[1]);
    }
    // 時計を進めても、前の発話の日時は記録した時刻のまま (AC-FR-414-3)。今の発話は新しい時刻。
    let first = &reqs[3][1].content;
    assert!(first.starts_with("[2025-11-09"), "{first}");
    let last = &reqs[3].last().unwrap().content;
    assert_ne!(
        first.split_once(']').unwrap().0,
        last.split_once(']').unwrap().0
    );
}

/// verifies: AC-FR-414-2
#[tokio::test(flavor = "multi_thread")]
async fn old_turns_are_dropped_in_bulk_and_the_cut_stays_put() {
    let dir = TempDir::new("context-trim");
    let turns = 14;
    // 予算は n_ctx − 予備 = 2,000 トークン (再生のサーバは文字数で数える)。システムプロンプトと今の発話を
    // 引いた残りに、長い発話の往復が 4〜5 つ入る。
    let llm = cassette(2200, turns);
    let (core, mut rx, _) = start(dir.path(), &llm, Some(200)).await;
    new_session(&core, &mut rx, "長い話").await;
    for k in 0..turns {
        say(&core, &mut rx, &format!("{k:02} {}", "あ".repeat(200))).await;
    }
    core.shutdown().await;

    let reqs: Vec<_> = (0..turns).map(|i| messages(&llm, i)).collect();
    // 履歴の最初の発言 (システムプロンプトの次)。履歴が無ければ今の発話。
    let firsts: Vec<String> = reqs.iter().map(|m| m[1].content.clone()).collect();
    let cuts: Vec<usize> = (1..turns).filter(|&i| firsts[i] != firsts[i - 1]).collect();
    assert!(!cuts.is_empty(), "the history never got too long");
    for &i in &cuts {
        // 省くときは、往復 2 つ以上をまとめて省く (足した往復 1 つより多く減る)。
        assert!(
            reqs[i].len() + 2 <= reqs[i - 1].len(),
            "request {i} dropped too little: {} -> {}",
            reqs[i - 1].len(),
            reqs[i].len()
        );
    }
    // 省いた後は、予算を超えるまで省く位置が変わらない (その間の要求は先頭が一致する)。
    let first_cut = cuts[0];
    assert!(
        (first_cut + 1..turns).any(|i| firsts[i] == firsts[i - 1]),
        "the cut moved on every turn: {cuts:?}"
    );
    for i in first_cut + 1..turns {
        if !cuts.contains(&i) {
            assert_prefix(&reqs[i - 1], &reqs[i]);
        }
    }
}
