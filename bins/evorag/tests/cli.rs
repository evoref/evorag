//! `evorag` の終了コードの試験 (07 §5)。実行ファイルを起動して確かめる。TUI は開かない経路だけ。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store};
use evorag_types::{SystemClock, UuidV7Gen};

fn evorag(dir: &Path, config: &str) -> Output {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, config).unwrap();
    // 実行時の値を先に見る (no-network の CI は、試験を別の場所に写して走らせる)。
    let exe = std::env::var_os("NEXTEST_BIN_EXE_evorag")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_evorag").into());
    Command::new(exe)
        .arg("--config")
        .arg(&cfg)
        .arg("--data-root")
        .arg(dir.join("data"))
        .output()
        .unwrap()
}

/// `/health`・`/props`・`/v1/models` に答える、ループバックの偽の llama-server。
fn fake_llama_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut sock in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.starts_with("GET /props") {
                r#"{"build_info":"b1-test","model_path":"/m/t.gguf","total_slots":1,"default_generation_settings":{"n_ctx":4096}}"#
            } else if req.starts_with("GET /v1/models") {
                r#"{"data":[{"id":"t"}]}"#
            } else {
                r#"{"status":"ok"}"#
            };
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

/// verifies: AC-FR-503-1
#[test]
fn exits_with_3_and_guidance_when_llama_server_is_unreachable() {
    let dir = TempDir::new("cli-down");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = evorag(
        dir.path(),
        &format!("[llm.chat]\nurl = \"http://127.0.0.1:{port}\"\n"),
    );
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("evorag setup"), "{err}");
    assert!(!dir.path().join("data").join("evorag.redb").exists());
}

/// FR-904・07 §5: 設定の誤りは終了コード 4。
#[test]
fn exits_with_4_on_a_bad_config() {
    let dir = TempDir::new("cli-config");
    let out = evorag(dir.path(), "[llm.chat]\nurll = \"x\"\n");
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 2"));
    // ループバック以外の接続先も設定の誤り (FR-509)。
    let out = evorag(dir.path(), "[llm.chat]\nurl = \"http://192.0.2.1:8080\"\n");
    assert_eq!(out.status.code(), Some(4));
}

/// verifies: AC-FR-203-1 (同じデータ根の 2 つ目は、理由を示して終了コード 2)
#[test]
fn exits_with_2_when_the_data_root_is_in_use() {
    let dir = TempDir::new("cli-locked");
    let url = fake_llama_server();
    let clock = Arc::new(SystemClock);
    let (_held, _) = Store::open(OpenOptions {
        data_root: dir.path().join("data"),
        evorag_version: "0.0.4".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        config_file: None,
    })
    .unwrap();
    let out = evorag(dir.path(), &format!("[llm.chat]\nurl = \"{url}\"\n"));
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&std::process::id().to_string()),
        "the pid is shown: {err}"
    );
}
