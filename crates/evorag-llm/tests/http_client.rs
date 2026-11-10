//! HTTP の実装の試験。ループバックに台本どおりに応える小さなサーバを立てる (外へは繋がない)。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evorag_llm::{
    ChatEvent, ChatMessage, ChatRequest, Endpoint, Generation, HttpClient, LlmClient, LlmError,
    RetryPolicy, Role,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 1 回の要求への応え方。
#[derive(Clone)]
enum Reply {
    Json(u16, String),
    /// 逐次表示の断片を送る。`close` が真なら `[DONE]` を送らずに接続を閉じる。
    Sse {
        chunks: Vec<String>,
        close: bool,
    },
    /// 断片を送った後、接続が閉じられるまで待つ (閉じられたら `closed` を立てる)。
    SseThenHang(Vec<String>),
}

#[derive(Default)]
struct Seen {
    requests: Mutex<Vec<String>>,
    count: AtomicUsize,
    closed_by_client: AtomicBool,
}

async fn read_request(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = sock.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            return Some(String::from_utf8_lossy(&buf).to_string());
        }
    }
}

fn sse(content: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(content).unwrap()
    )
}

async fn serve(replies: Vec<Reply>) -> (String, Arc<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Seen::default());
    let s = seen.clone();
    tokio::spawn(async move {
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let s = s.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                // 1 つの接続で複数の要求が来ても応える (keep-alive)。
                while let Some(req) = read_request(&mut sock).await {
                    s.requests.lock().unwrap().push(req);
                    s.count.fetch_add(1, Ordering::SeqCst);
                    let reply = replies.lock().unwrap().pop_front();
                    match reply {
                        Some(Reply::Json(status, body)) => {
                            let head = format!(
                                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(head.as_bytes()).await;
                            let _ = sock.write_all(body.as_bytes()).await;
                        }
                        Some(Reply::Sse { chunks, close }) => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                                .await;
                            for c in chunks {
                                let _ = sock.write_all(c.as_bytes()).await;
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            if !close {
                                let _ = sock.write_all(b"data: [DONE]\n\n").await;
                            }
                            return;
                        }
                        Some(Reply::SseThenHang(chunks)) => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                                .await;
                            for c in chunks {
                                let _ = sock.write_all(c.as_bytes()).await;
                            }
                            // 相手が閉じるまで読む (読めなくなったら閉じられた)。
                            let mut b = [0u8; 16];
                            loop {
                                match tokio::time::timeout(
                                    Duration::from_secs(5),
                                    sock.read(&mut b),
                                )
                                .await
                                {
                                    Ok(Ok(0)) | Ok(Err(_)) => {
                                        s.closed_by_client.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                    Ok(Ok(_)) => continue,
                                    Err(_) => return,
                                }
                            }
                        }
                        None => return,
                    }
                }
            });
        }
    });
    (url, seen)
}

fn client(url: &str, key: Option<&str>) -> HttpClient {
    HttpClient::new(
        Endpoint::new(url, key.map(str::to_owned), false).unwrap(),
        RetryPolicy {
            delays: vec![Duration::from_millis(10); 3],
        },
    )
    .unwrap()
}

fn req() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::new(Role::User, "こんにちは")],
        generation: Generation::default(),
        id_slot: Some(0),
        model: None,
        extra: Default::default(),
    }
}

async fn collect(c: &HttpClient) -> Result<Vec<Result<ChatEvent, LlmError>>, LlmError> {
    let mut s = c.chat(req()).await?;
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    Ok(out)
}

/// verifies: AC-FR-101-1 (クライアントの層: 断片の順に出来事が届く)
#[tokio::test]
async fn streams_deltas_in_order() {
    let (url, seen) = serve(vec![Reply::Sse {
        chunks: vec![sse("こん"), sse("にち"), sse("は")],
        close: false,
    }])
    .await;
    let events = collect(&client(&url, None)).await.unwrap();
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Ok(ChatEvent::Delta(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["こん", "にち", "は"]);
    assert!(matches!(events.last(), Some(Ok(ChatEvent::Done { .. }))));
    let r = seen.requests.lock().unwrap()[0].clone();
    assert!(r.starts_with("POST /v1/chat/completions"));
    assert!(r.contains("\"stream\":true") && r.contains("\"id_slot\":0"));
}

/// verifies: AC-FR-507-1 (5xx は再試行する)
#[tokio::test]
async fn server_errors_are_retried_up_to_three_times() {
    let (url, seen) = serve(vec![
        Reply::Json(503, "busy".into()),
        Reply::Json(500, "oops".into()),
        Reply::Sse {
            chunks: vec![sse("ok")],
            close: false,
        },
    ])
    .await;
    collect(&client(&url, None)).await.unwrap();
    assert_eq!(seen.count.load(Ordering::SeqCst), 3);

    let (url, seen) = serve(vec![Reply::Json(503, "busy".into()); 5]).await;
    let err = collect(&client(&url, None)).await.unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 503, .. }));
    assert_eq!(seen.count.load(Ordering::SeqCst), 4, "1 try + 3 retries");
}

/// verifies: AC-FR-507-1 (4xx は再試行しない)
#[tokio::test]
async fn client_errors_are_not_retried() {
    let (url, seen) = serve(vec![
        Reply::Json(400, "bad".into()),
        Reply::Json(200, "{}".into()),
    ])
    .await;
    let err = collect(&client(&url, None)).await.unwrap_err();
    assert_eq!(
        err,
        LlmError::Http {
            status: 400,
            body: "bad".into()
        }
    );
    assert_eq!(seen.count.load(Ordering::SeqCst), 1);
}

/// verifies: AC-FR-507-1 (接続の失敗は再試行し、尽きたら Connect)
#[tokio::test]
async fn connection_failures_are_retried_then_reported() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let started = std::time::Instant::now();
    let err = collect(&client(&url, None)).await.unwrap_err();
    assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(30),
        "waited between retries"
    );
}

/// verifies: AC-FR-507-1 (逐次表示の途中の切断は再試行しない)
#[tokio::test]
async fn a_stream_cut_midway_is_reported_and_not_retried() {
    let (url, seen) = serve(vec![
        Reply::Sse {
            chunks: vec![sse("途中")],
            close: true,
        },
        Reply::Sse {
            chunks: vec![sse("二回目")],
            close: false,
        },
    ])
    .await;
    let events = collect(&client(&url, None)).await.unwrap();
    assert_eq!(events[0], Ok(ChatEvent::Delta("途中".into())));
    assert!(matches!(events.last(), Some(Err(LlmError::Cut(_)))));
    assert_eq!(seen.count.load(Ordering::SeqCst), 1);
}

/// verifies: AC-FR-102-1 (クライアントの層: 落とすと接続が閉じる)
#[tokio::test]
async fn dropping_the_stream_closes_the_connection() {
    let (url, seen) = serve(vec![Reply::SseThenHang(vec![sse("書きかけ")])]).await;
    let c = client(&url, None);
    let mut s = c.chat(req()).await.unwrap();
    assert_eq!(
        s.next().await,
        Some(Ok(ChatEvent::Delta("書きかけ".into())))
    );
    drop(s);
    for _ in 0..100 {
        if seen.closed_by_client.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the connection was not closed after dropping the stream");
}

/// verifies: AC-FR-501-1 (クライアントの層)
#[tokio::test]
async fn health_needs_no_key_and_props_sends_the_key() {
    let props = r#"{"build_info":"b1-abc","model_path":"/m/q.gguf","total_slots":2,"default_generation_settings":{"n_ctx":8192}}"#;
    let models = r#"{"data":[{"id":"q","meta":{"n_params":1,"size":2,"n_vocab":3}}]}"#;
    let (url, seen) = serve(vec![
        Reply::Json(200, "{\"status\":\"ok\"}".into()),
        Reply::Json(200, props.into()),
        Reply::Json(200, models.into()),
        Reply::Json(200, "{\"tokens\":[1,2,3,4]}".into()),
    ])
    .await;
    let c = client(&url, Some("k3y"));
    c.health().await.unwrap();
    let info = c.server_info().await.unwrap();
    assert_eq!((info.n_ctx, info.total_slots), (8192, 2));
    assert_eq!(c.count_tokens("abcd").await.unwrap(), 4);
    let reqs = seen.requests.lock().unwrap().clone();
    assert!(reqs[0].starts_with("GET /health"));
    assert!(!reqs[0].to_ascii_lowercase().contains("authorization"));
    assert!(reqs[1].starts_with("GET /props"));
    assert!(reqs[1].contains("Bearer k3y") || reqs[1].contains("bearer k3y"));
    assert!(reqs[3].starts_with("POST /tokenize"));
}
