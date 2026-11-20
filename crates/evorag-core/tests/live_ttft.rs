//! BM-107: 発話を送ってから最初のトークンまで (TTFT) を、投機的な先読みの当たりの種類ごとに測る
//! (09 §5.1、NFR-107、05 §4.5)。実際の llama-server (応答・埋め込み・リランカー) が要る。`cargo xtask live-llm`
//! で流す。
//!
//! - 作った英語の会話 (以前のセッション 6 つと、今のセッションの履歴 約 2k トークン) をデータ根に書き、
//!   裏の処理 (点検・較正・埋め込みなど) を済ませてから、今のセッションで問う。
//! - 種類: `none` (入力が止まったことを知らせずに送る)、`a` (下書きと同じ本文を送る)、`b` (句読点だけ違う
//!   本文を送る。添えるものは同じ)、`c` (別の話題の下書きの後に送る)。各 `ROUNDS` 回。
//! - 記録: 種類ごとの TTFT (応答の経路を始めてから最初の本文まで) の p50・p95、llama-server が処理した
//!   トークン数、添えた引用のトークン数。結果は `BM-107.json` (`EVORAG_BENCH_OUT`)。

mod eval;

use std::sync::Arc;
use std::time::Duration;

use eval::{Servers, Session, Speaker};
use evorag_core::{Core, PromptLanguage};
use evorag_memory::TurnBody;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, ManualClock, SystemClock, Timestamp};

/// 種類ごとの回数。
const ROUNDS: usize = 5;
const SETTLE: Duration = Duration::from_secs(1800);
const ANSWER: Duration = Duration::from_secs(300);

/// 以前のセッションの話題 (題名, 利用者の発話, 応答の後ろにある細部, 問い)。
const TOPICS: &[(&str, &str, &str, &str)] = &[
    (
        "sourdough",
        "My sourdough starter smells like acetone. What should I do?",
        "Feed it twice a day at a 1:1:1 ratio and keep it near 24 degrees for a week",
        "What feeding ratio did you suggest for my sourdough starter?",
    ),
    (
        "bike",
        "The rear derailleur on my old road bike keeps skipping gears.",
        "turn the barrel adjuster a quarter turn counterclockwise until the chain climbs smoothly",
        "How far did you tell me to turn the barrel adjuster on my bike?",
    ),
    (
        "cello",
        "I just started learning the cello. My bow keeps sliding toward the fingerboard.",
        "keep the bow halfway between the bridge and the fingerboard and tilt it slightly toward the scroll",
        "Where did you say I should keep my cello bow?",
    ),
    (
        "fig",
        "My fiddle leaf fig is dropping leaves after I repotted it.",
        "water it only when the top five centimeters of soil are dry and keep it out of drafts",
        "When should I water my fiddle leaf fig again?",
    ),
    (
        "laptop",
        "I'm choosing a laptop for photo editing on a budget.",
        "prioritize 32 gigabytes of memory over a faster processor for large photo libraries",
        "How much memory did you recommend for my photo editing laptop?",
    ),
    (
        "running",
        "I'm training for my first 10k run in two months.",
        "do one interval session of six 400 meter repeats each week and keep the other runs easy",
        "What interval session did you suggest for my 10k training?",
    ),
];

/// 長い応答 (細部を後ろに置く)。
fn long_answer(detail: &str) -> String {
    let filler = "Start slowly, write down what you observe each day, and change only one thing at a time so you can tell what made the difference.";
    format!("{filler} {filler} {filler} The most useful step is this: {detail}. {filler}")
}

fn sessions(start: Timestamp) -> Vec<Session> {
    let mut out: Vec<Session> = TOPICS
        .iter()
        .enumerate()
        .map(|(i, (title, user, detail, _))| Session {
            title: (*title).into(),
            start: Timestamp::from_millis(start.as_millis() + i as i64 * 3_600_000),
            turns: vec![
                (Speaker::User, (*user).into()),
                (Speaker::Assistant, long_answer(detail)),
            ],
        })
        .collect();
    // 今のセッション: 履歴 約 2k トークン (話題と関係の無いやり取り)。
    let chat_turns = (0..8)
        .flat_map(|k| {
            [
                (
                    Speaker::User,
                    format!("Let's plan day {k} of my week. What should I cook for dinner?"),
                ),
                (
                    Speaker::Assistant,
                    format!(
                        "For day {k}, a simple plan works best. {} {}",
                        "Pick one vegetable, one protein and one grain, cook the grain first, and keep leftovers for lunch.",
                        "Shopping once for three days saves time and reduces waste."
                    ),
                ),
            ]
        })
        .collect();
    out.push(Session {
        title: "today".into(),
        start: Timestamp::from_millis(start.as_millis() + 10 * 3_600_000),
        turns: chat_turns,
    });
    out
}

fn quantile(mut v: Vec<f64>, q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[((v.len() as f64 - 1.0) * q).round() as usize])
}

async fn until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    pred: impl Fn(&Event) -> bool,
) -> Option<Event> {
    eval::until(rx, ANSWER, pred).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs chat, embedding and reranking llama-servers (cargo xtask live-llm)"]
async fn bm_107_ttft_by_prefetch_kind() {
    let Some(servers) = Servers::from_env().filter(|s| s.aux.is_some()) else {
        eprintln!(
            "BM-107: set EVORAG_LIVE_LLM_URL, EVORAG_LIVE_EMBED_URL and EVORAG_LIVE_RERANK_URL (skipped)"
        );
        return;
    };
    eval::init_log();
    let dir = TempDir::new("bm-107");
    let day_ago = SystemClock.now().as_millis() - 24 * 3_600_000;
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(day_ago)));
    eval::write_sessions(
        dir.path(),
        &clock,
        &sessions(Timestamp::from_millis(day_ago)),
    );
    let system: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut cfg = eval::config(dir.path(), PromptLanguage::En, &servers, system);
    cfg.precompute = true;
    cfg.generation.max_tokens = Some(48);
    let chat = servers.chat_client();
    let (core, mut rx, _) = Core::start(cfg, chat.clone())
        .await
        .expect("the core starts");
    core.settle().await;
    assert!(
        eval::settle(&core, SETTLE).await,
        "background steps did not settle"
    );
    // 今のセッション (最後に書いたもの) を開く。次のターンの分が先に処理される。
    let today = core
        .sessions(100)
        .into_iter()
        .find(|s| s.title == "today")
        .expect("the current session")
        .id;
    core.command(Command::OpenSession { id: today });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.settle_precompute().await;

    let mut rows = Vec::new();
    for round in 0..ROUNDS {
        for kind in ["none", "a", "b", "c"] {
            let (_, _, _, question) = TOPICS[(round + rows.len()) % TOPICS.len()];
            let other = TOPICS[(round + rows.len() + 3) % TOPICS.len()].3;
            match kind {
                "a" => core.command(Command::InputIdle {
                    text: question.into(),
                }),
                "b" => core.command(Command::InputIdle {
                    text: question.trim_end_matches('?').to_owned(),
                }),
                "c" => core.command(Command::InputIdle { text: other.into() }),
                _ => {}
            }
            core.settle_precompute().await;
            core.command(Command::Send {
                text: question.into(),
            });
            let Some(Event::ResponseFinished { turn, .. }) =
                until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await
            else {
                panic!("no answer");
            };
            core.settle().await;
            core.settle_precompute().await;
            rows.push((kind, turn.id));
        }
    }
    core.shutdown().await;

    let body = |id| -> TurnBody { eval::turn_body(dir.path(), id) };
    let mut out = serde_json::Map::new();
    let mut per_row = Vec::new();
    for kind in ["none", "a", "b", "c"] {
        let mut ttft = Vec::new();
        let mut processed = Vec::new();
        let mut quoted = Vec::new();
        let mut got = std::collections::BTreeMap::<String, usize>::new();
        for (k, id) in rows.iter().filter(|(k, _)| *k == kind) {
            let obs = body(*id).obs.unwrap_or_default();
            let p = obs.prefetch.unwrap_or_default();
            *got.entry(p.kind.clone()).or_default() += 1;
            if let Some(t) = p.ttft_ms {
                ttft.push(f64::from(t) / 1000.0);
            }
            if let Some(n) = p.processed {
                processed.push(f64::from(n));
            }
            let tokens = obs.packed.map_or(0, |pk| pk.tokens);
            quoted.push(f64::from(tokens));
            per_row.push(serde_json::json!({
                "asked_as": k, "kind": p.kind, "ttft_ms": p.ttft_ms, "processed": p.processed,
                "quote_tokens": tokens,
            }));
        }
        println!(
            "BM-107 {kind}: ttft p50 {:?} s, processed p50 {:?}, quote tokens mean {:.0}, kinds {got:?}",
            quantile(ttft.clone(), 0.5),
            quantile(processed.clone(), 0.5),
            quoted.iter().sum::<f64>() / quoted.len().max(1) as f64
        );
        out.insert(
            kind.to_owned(),
            serde_json::json!({
                "rounds": ttft.len(),
                "observed_kinds": got,
                "ttft_p50_s": quantile(ttft.clone(), 0.5),
                "ttft_p95_s": quantile(ttft, 0.95),
                "processed_p50": quantile(processed, 0.5),
                "quote_tokens_mean": quoted.iter().sum::<f64>() / quoted.len().max(1) as f64,
            }),
        );
    }
    let v = serde_json::json!({
        "id": "BM-107",
        "evorag": env!("CARGO_PKG_VERSION"),
        "date": eval::today(),
        "setup": servers.setup(),
        "models": eval::models(&servers).await,
        "method": "synthetic English history of about 2k tokens; each kind asked ROUNDS times in the same session; TTFT from the start of the response path to the first content token (obs.prefetch.ttft_ms)",
        "by_kind": out,
        "rows": per_row,
    });
    eval::write_json(&eval::out_dir().join("BM-107.json"), &v);
}
