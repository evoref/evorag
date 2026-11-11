//! ベンチマーク (09 §5.1): BM-103 (キー入力から描画まで)、BM-104 (トークンの受信から描画まで)。
//!
//! 長い会話 (500 ターン) を開いた画面で、状態の更新と 1 回の描画 (120 × 40 の試験用の端末) に
//! かかる時間を測る。`#[ignore]` にし、`cargo xtask bench` (release) で流す。
//! `EVORAG_BENCH_OUT` があれば `<そこ>/BM-<ID>.json` に書く。
//!
//! verifies: AC-NFR-103-1, AC-NFR-104-1

use std::time::Instant;

use evorag_tui::app::{App, Key, Status};
use evorag_tui::catalog::Catalog;
use evorag_types::api::{Event, Role, SessionView, TurnStatus, TurnView};
use evorag_types::{Id, Timestamp};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const TURNS: usize = 500;

fn id(n: usize) -> Id {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&(n as u64).to_be_bytes());
    Id::from_bytes(b)
}

fn app() -> App {
    let mut a = App::new(
        Catalog::load("ja"),
        Status {
            endpoint: "127.0.0.1:8080".into(),
            model: "bench".into(),
            n_ctx: 8192,
        },
    );
    let turns = (0..TURNS)
        .map(|i| TurnView {
            id: id(i + 1),
            session: id(0),
            role: if i % 2 == 0 { Role::User } else { Role::Assistant },
            status: if i % 2 == 0 {
                TurnStatus::Confirmed
            } else {
                TurnStatus::Completed
            },
            text: format!(
                "## 見出し {i}\n京都の紅葉と **嵐山** の朝の散歩について、{i} 回目の話です。\n```\nlet x = {i};\n```\n前に話した内容を踏まえて、もう少し詳しく教えてください。"
            ),
            model_key: None,
            reply_to: None,
            created_at: Timestamp::from_millis(1_762_650_000_000 + i as i64 * 60_000),
            sources: usize::from(i % 2 == 1),
            source_base: 1,
        })
        .collect();
    a.on_event(Event::SessionOpened {
        session: SessionView {
            id: id(0),
            title: "長い会話".into(),
            created_at: Timestamp::from_millis(0),
            updated_at: Timestamp::from_millis(0),
        },
        turns,
        unreadable: 0,
    });
    a
}

fn p95(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(f64::total_cmp);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    (mean, v[(v.len() * 95 / 100).min(v.len() - 1)])
}

fn record(id: &str, value: serde_json::Value) {
    println!("{id}: {value}");
    if let Some(dir) = std::env::var_os("EVORAG_BENCH_OUT") {
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut v = value;
        v["id"] = id.into();
        v["turns"] = TURNS.into();
        v["evorag"] = env!("CARGO_PKG_VERSION").into();
        v["os"] = std::env::consts::OS.into();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&v).unwrap() + "\n",
        )
        .unwrap();
    }
}

/// BM-103 / NFR-103: キー入力から描画まで。p95 16 ms 以内。
#[ignore = "benchmark: run with cargo xtask bench"]
#[test]
fn bm_103_key_to_render() {
    let mut a = app();
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| evorag_tui::render::draw(&a, f)).unwrap();
    let mut times = Vec::new();
    for (i, c) in "日本語の入力を続けて打つと、描画が間に合うかを確かめる。"
        .chars()
        .cycle()
        .take(300)
        .enumerate()
    {
        let t = Instant::now();
        if i % 60 == 59 {
            a.on_key(Key::Enter { newline: true });
        } else {
            a.on_key(Key::Char(c));
        }
        term.draw(|f| evorag_tui::render::draw(&a, f)).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (mean, p95) = p95(times);
    record(
        "BM-103",
        serde_json::json!({"mean_ms": mean, "p95_ms": p95, "keys": 300, "target_p95_ms": 16}),
    );
    assert!(p95 <= 16.0, "key to render p95 {p95:.2} ms > 16 ms");
}

/// BM-104 / NFR-104: トークンの受信から描画まで。p95 50 ms 以内。
#[ignore = "benchmark: run with cargo xtask bench"]
#[test]
fn bm_104_token_to_render() {
    let mut a = app();
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let reply_to = id(TURNS + 1);
    a.on_event(Event::ResponseStarted { reply_to });
    let mut times = Vec::new();
    for i in 0..1000 {
        let piece = if i % 40 == 39 {
            "\n"
        } else {
            "応答の断片"
        };
        let t = Instant::now();
        a.on_event(Event::Token {
            reply_to,
            text: piece.into(),
        });
        term.draw(|f| evorag_tui::render::draw(&a, f)).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (mean, p95) = p95(times);
    record(
        "BM-104",
        serde_json::json!({"mean_ms": mean, "p95_ms": p95, "tokens": 1000, "target_p95_ms": 50}),
    );
    assert!(p95 <= 50.0, "token to render p95 {p95:.2} ms > 50 ms");
}
