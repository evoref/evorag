//! `evorag` の終了コードの試験 (07 §5)。実行ファイルを起動して確かめる。TUI は開かない経路だけ。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Store};
use evorag_types::{Clock, SystemClock, UuidV7Gen};

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

fn evorag_with_stdin(dir: &Path, config: &str, args: &[&str], stdin: &str) -> Output {
    use std::process::Stdio;
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, config).unwrap();
    let exe = std::env::var_os("NEXTEST_BIN_EXE_evorag")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_evorag").into());
    let mut child = Command::new(exe)
        .arg("--config")
        .arg(&cfg)
        .arg("--data-root")
        .arg(dir.join("data"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn make_data_root_with_backup(root: &Path) -> std::path::PathBuf {
    let clock = Arc::new(SystemClock);
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.5".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        config_file: None,
    })
    .unwrap();
    store.daily_backup(None).unwrap().unwrap()
}

/// verifies: AC-FR-205-1 (実行ファイルの層: 壊れた DB は broken/ へ移り、戻すかを尋ねられる)
#[test]
fn a_broken_database_is_moved_aside_and_the_user_is_asked() {
    let dir = TempDir::new("cli-broken");
    let url = fake_llama_server();
    make_data_root_with_backup(&dir.path().join("data"));
    std::fs::write(dir.path().join("data").join("evorag.redb"), b"garbage").unwrap();
    let out = evorag_with_stdin(
        dir.path(),
        &format!("[llm.chat]\nurl = \"{url}\"\n"),
        &[],
        "n\n",
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("broken"), "{err}");
    let broken = dir.path().join("data").join("broken");
    assert_eq!(std::fs::read_dir(broken).unwrap().count(), 1);
}

/// 03 §10: `evorag restore <退避>` で戻す。
#[test]
fn restore_puts_a_backup_in_place() {
    let dir = TempDir::new("cli-restore");
    let backup = make_data_root_with_backup(&dir.path().join("data"));
    let out = evorag_with_stdin(dir.path(), "", &["restore", backup.to_str().unwrap()], "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 今の DB は先に退避されている。
    let pre = dir.path().join("data").join("backups").join("pre-op");
    assert_eq!(std::fs::read_dir(pre).unwrap().count(), 1);
}

fn mtime_and_len(p: &Path) -> (std::time::SystemTime, u64) {
    let m = std::fs::metadata(p).unwrap();
    (m.modified().unwrap(), m.len())
}

/// verifies: AC-FR-207-1, AC-FR-206-1 (doctor が件数を報告する)
#[test]
fn doctor_reports_without_writing_anything() {
    use evorag_store::{Envelope, WriteOp, WriteRequest, Writer};
    let dir = TempDir::new("cli-doctor");
    let root = dir.path().join("data");
    let clock = Arc::new(SystemClock);
    let (bad, sid);
    {
        let (store, _) = Store::open(OpenOptions {
            data_root: root.clone(),
            evorag_version: "0.0.5".into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock.clone())),
            config_file: None,
        })
        .unwrap();
        let store = Arc::new(store);
        let (w, h) = Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || w.run());
        let writes = evorag_memory::Writes {
            clock: &*clock,
            ids: &**store.ids(),
        };
        let (s, ops) = writes.create_session("s").unwrap();
        sid = s;
        h.submit_wait(WriteRequest::conversation(ops)).unwrap();
        let (b, ops) = writes.create_session("壊す").unwrap();
        bad = b;
        h.submit_wait(WriteRequest::conversation(ops)).unwrap();
        // quarantine の記録を 1 件。
        let mut q = Envelope::new(store.ids().next_id(), "quarantine", 1, clock.now(), "");
        q.body = ciborium::Value::Map(vec![("reason".into(), "test".into())]);
        h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(q)]))
            .unwrap();
        store.daily_backup(None).unwrap();
        h.shutdown();
        t.join().unwrap();
    }
    evorag_store::testutil::raw_corrupt_record(&root, "session", bad);
    let _ = sid;

    let journal = std::fs::read_dir(root.join("journal"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let watched = [
        root.join("evorag.redb"),
        journal,
        root.join("lock"),
        root.join("lock.pid"),
    ];
    let before: Vec<_> = watched.iter().map(|p| mtime_and_len(p)).collect();
    let bytes_before = std::fs::read(&watched[0]).unwrap();
    for _ in 0..2 {
        let out = evorag_with_stdin(
            dir.path(),
            "[llm.chat]\nurl = \"http://127.0.0.1:9\"\n",
            &["doctor"],
            "",
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("quarantine の記録: 1 件"), "{text}");
        assert!(text.contains("壊れた記録 1 件"), "{text}");
        // 日次と、その週の最初の日次を残す週次の 2 個。
        assert!(text.contains("退避: 2 個"), "{text}");
    }
    let after: Vec<_> = watched.iter().map(|p| mtime_and_len(p)).collect();
    assert_eq!(before, after, "doctor must not write");
    assert_eq!(bytes_before, std::fs::read(&watched[0]).unwrap());
}

/// FR-213: `evorag backups list`。
#[test]
fn backups_list_shows_the_backups() {
    let dir = TempDir::new("cli-backups");
    make_data_root_with_backup(&dir.path().join("data"));
    let out = evorag_with_stdin(dir.path(), "", &["backups", "list"], "");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("daily"), "{text}");
}
