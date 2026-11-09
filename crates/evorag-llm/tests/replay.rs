//! 記録の再生の実装の試験 (09 §3.3)。

use std::path::PathBuf;
use std::time::Duration;

use evorag_llm::replay::ReplayClient;
use evorag_llm::{ChatEvent, ChatMessage, ChatRequest, Generation, LlmClient, LlmError, Role};

fn cassette(name: &str) -> ReplayClient {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    ReplayClient::load(&dir.join("../../tests/llm-cassettes").join(name)).unwrap()
}

fn req(text: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::new(Role::User, text)],
        generation: Generation::default(),
        id_slot: None,
        model: None,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn plays_the_recorded_chats_in_order() {
    let c = cassette("basic.json");
    c.health().await.unwrap();
    let info = c.server_info().await.unwrap();
    assert_eq!(info.n_ctx, 8192);
    assert_eq!(info.model_key.len(), 32);

    let mut s = c.chat(req("a")).await.unwrap();
    let mut got = Vec::new();
    while let Some(e) = s.next().await {
        got.push(e.unwrap());
    }
    assert_eq!(got[0], ChatEvent::Delta("こんにちは。".into()));
    assert!(matches!(got[2], ChatEvent::Done { usage: Some(_), .. }));
    drop(s);

    let mut s = c.chat(req("b")).await.unwrap();
    assert_eq!(
        s.next().await,
        Some(Ok(ChatEvent::Delta("途中まで".into())))
    );
    assert!(matches!(s.next().await, Some(Err(LlmError::Cut(_)))));
    drop(s);
    assert_eq!(c.dropped_early(), 0);

    // 落とされるまで待つ応答を、途中で落とす (中断)。
    let mut s = c.chat(req("c")).await.unwrap();
    assert_eq!(s.next().await, Some(Ok(ChatEvent::Delta("考え中".into()))));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), s.next())
            .await
            .is_err()
    );
    drop(s);
    assert_eq!(c.dropped_early(), 1);

    assert!(matches!(
        c.chat(req("d")).await,
        Err(LlmError::Http { status: 503, .. })
    ));
    assert_eq!(c.remaining(), 0);
    let texts: Vec<String> = c
        .requests()
        .iter()
        .map(|r| r.messages[0].content.clone())
        .collect();
    assert_eq!(texts, ["a", "b", "c", "d"]);
}
