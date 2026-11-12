//! 実際の埋め込みとリランカーの llama-server を使う想起の試験 (09 §3.4・§5.2)。`cargo xtask live-llm` で、
//! 明示したときだけ流す。
//!
//! - 接続先は環境変数 `EVORAG_LIVE_EMBED_URL` と `EVORAG_LIVE_RERANK_URL`。どちらかが無ければ、何もせずに
//!   知らせて終わる。応答の llama-server は使わない (応答は記録の再生で置き換える)。
//! - しきい値は、同梱の事例でその場で求める (05 §3.7)。
//! - 事例の問いは、1 件ずつ新しいセッションで送り、送り終えたらそのセッションを消す (前の問いが後の問いの
//!   候補に入らないように)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Endpoint, Generation, HttpClient, LlmClient, RetryPolicy};
use evorag_memory::{AssistantTurn, KIND_TURN, TurnState, Writes};
use evorag_store::testutil::TempDir;
use evorag_store::{OpenOptions, Read, Store, WriteRequest, Writer};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, Id, SystemClock, UuidV7Gen};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Deserialize)]
struct Cases {
    session: Vec<SessionCase>,
    case: Vec<Case>,
}

#[derive(Deserialize)]
struct SessionCase {
    id: String,
    title: Option<String>,
    turns: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    #[serde(default)]
    category: String,
    #[serde(default = "new_session")]
    session: String,
    gold: Vec<String>,
    query: String,
}

fn new_session() -> String {
    "new".into()
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn servers() -> Option<(String, String)> {
    match (
        std::env::var("EVORAG_LIVE_EMBED_URL"),
        std::env::var("EVORAG_LIVE_RERANK_URL"),
    ) {
        (Ok(e), Ok(r)) => Some((e, r)),
        _ => {
            eprintln!(
                "live recall: set EVORAG_LIVE_EMBED_URL and EVORAG_LIVE_RERANK_URL to run (skipped)"
            );
            None
        }
    }
}

fn client(url: &str) -> Arc<dyn LlmClient> {
    Arc::new(
        HttpClient::new(
            Endpoint::new(url, None, false).unwrap(),
            RetryPolicy::default(),
        )
        .unwrap(),
    )
}

/// 応答の llama-server の代わり (`replies` 回だけ答える)。
fn chat(replies: usize) -> ReplayClient {
    let one = r#"{"events": [{"delta": "了解です"}, {"done": {}}]}"#;
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![one; replies].join(", ")
    ))
    .unwrap()
}

fn config(root: &Path, embed: &str, rerank: &str) -> CoreConfig {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        prompt_language: PromptLanguage::Ja,
        generation: Generation::default(),
        request_extra: Default::default(),
        reserve_for_output: None,
        backup_dir: None,
        allow_unsafe_location: false,
        probe: evorag_core::Probe::system(),
        model_name: "live".into(),
        ignored_config: Vec::new(),
        embed: Some(client(embed)),
        rerank: Some(client(rerank)),
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
    }
}

/// 事例の会話をデータ根に書く。ターンの名前 (`<セッション>-tMM`) → ID と、セッションの名前 → ID を返す。
fn write_sessions(
    root: &Path,
    sessions: &[SessionCase],
) -> (HashMap<Id, String>, HashMap<String, Id>) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: env!("CARGO_PKG_VERSION").into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock.clone())),
        config_file: None,
    })
    .unwrap();
    let store = Arc::new(store);
    let (w, h) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || w.run());
    let writes = Writes {
        clock: &*clock,
        ids: &**store.ids(),
    };
    let read = |kind: &str, id: Id| match store.read().unwrap().get(kind, id).unwrap() {
        Some(Read::Ok(e)) => e,
        other => panic!("{other:?}"),
    };
    let mut names = HashMap::new();
    let mut ids = HashMap::new();
    for s in sessions {
        let (sid, ops) = writes
            .create_session(s.title.as_deref().unwrap_or(&s.id))
            .unwrap();
        h.submit_wait(WriteRequest::conversation(ops)).unwrap();
        ids.insert(s.id.clone(), sid);
        let mut user = None;
        for (i, turn) in s.turns.iter().enumerate() {
            let session = read(evorag_memory::KIND_SESSION, sid);
            let text = &turn[3..];
            let (id, ops) = if turn.starts_with("U: ") {
                writes.user_turn(&session, text).unwrap()
            } else {
                let u = read(KIND_TURN, user.expect("a user turn first"));
                writes
                    .assistant_turn(
                        &session,
                        &u,
                        AssistantTurn {
                            state: TurnState::Completed,
                            text: text.to_owned(),
                            model_key: None,
                            generation: None,
                            attached: Vec::new(),
                            error: None,
                            recall: None,
                            obs: None,
                        },
                    )
                    .unwrap()
            };
            h.submit_wait(WriteRequest::conversation(ops)).unwrap();
            if turn.starts_with("U: ") {
                user = Some(id);
            }
            names.insert(id, format!("{}-t{:02}", s.id, i + 1));
        }
    }
    h.shutdown();
    t.join().unwrap();
    (names, ids)
}

/// 同梱の事例でしきい値を求めて記録する (05 §3.7)。
async fn calibrate(root: &Path, rerank: &str) {
    let c = client(rerank);
    let info = c.server_info().await.expect("the reranker answers");
    let done = Mutex::new(Vec::new());
    let body =
        evorag_core::calibration::calibrate(&*c, &info.model_key, "ja", 1, &done, &mut |_, _| {})
            .await
            .expect("calibration");
    println!(
        "threshold: passed = {}, threshold = {:?}, band_low = {:?}",
        body.passed, body.threshold, body.band_low
    );
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    DataRootModels::open(
        root,
        env!("CARGO_PKG_VERSION"),
        clock.clone(),
        Arc::new(UuidV7Gen::new(clock)),
    )
    .unwrap()
    .save_threshold(&body)
    .unwrap();
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("closed");
        if pred(&ev) {
            return ev;
        }
    }
}

/// 1 件の問いを送り、添えたターンの ID を返す。`session` が `None` なら新しいセッションで送り、後で消す。
async fn ask(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    session: Option<Id>,
    query: &str,
) -> (Vec<Id>, Option<Id>, String) {
    match session {
        Some(id) => core.command(Command::OpenSession { id }),
        None => core.command(Command::NewSession {
            title: "問い".into(),
        }),
    }
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::Send { text: query.into() });
    let Event::ResponseFinished { turn, .. } =
        until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await
    else {
        unreachable!()
    };
    core.settle().await;
    let body = core.turn_body(turn.id).expect("the answer");
    let attached = body.attached.clone();
    let top = body
        .obs
        .as_ref()
        .and_then(|o| o.reranked.first())
        .map(|r| r.id);
    let obs = body
        .obs
        .map(|o| {
            format!(
                "reranked {:?} degraded {:?}",
                o.reranked.iter().map(|r| r.score).collect::<Vec<_>>(),
                o.degraded
            )
        })
        .unwrap_or_default();
    if session.is_none() {
        core.command(Command::DeleteSession);
        until(rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
        core.settle().await;
    }
    (attached, top, obs)
}

/// 事例を流した結果。
struct Outcome {
    /// 正しく添えた数と、関係のある問いの数。
    right: usize,
    related: usize,
    /// 正しい往復がリランクの最上位だった数。
    top: usize,
    /// 何も添えなかった数と、関係の無い問いの数。
    abstained: usize,
    none: usize,
    misses: Vec<String>,
}

/// 事例を流す。
async fn run(file: &str, embed: &str, rerank: &str) -> Outcome {
    let text = std::fs::read_to_string(workspace().join(file)).expect("the cases");
    let cases: Cases = toml::from_str(&text).expect("valid cases");
    let dir = TempDir::new("live-recall");
    let (names, sessions) = write_sessions(dir.path(), &cases.session);
    calibrate(dir.path(), rerank).await;
    let (core, mut rx, _) = Core::start(
        config(dir.path(), embed, rerank),
        Arc::new(chat(cases.case.len())),
    )
    .await
    .unwrap();
    core.settle().await;
    core.settle_background_within(Duration::from_secs(600))
        .await;
    let (mut right, mut related, mut top_right, mut abstained, mut none) = (0, 0, 0, 0, 0);
    let mut misses = Vec::new();
    // 既存のセッションでの問いは、そのセッションに会話を足すので最後に流す。
    let mut order: Vec<&Case> = cases.case.iter().filter(|c| c.session == "new").collect();
    order.extend(cases.case.iter().filter(|c| c.session != "new"));
    for c in order {
        let session = (c.session != "new").then(|| sessions[&c.session]);
        let (attached, top, obs) = ask(&core, &mut rx, session, &c.query).await;
        let chosen: Vec<&str> = attached
            .iter()
            .filter_map(|id| names.get(id).map(String::as_str))
            .collect();
        if c.gold.is_empty() {
            none += 1;
            if attached.is_empty() {
                abstained += 1;
            } else {
                misses.push(format!(
                    "{} (none) attached {chosen:?}: {} [{obs}]",
                    c.id, c.query
                ));
            }
        } else {
            related += 1;
            if top
                .and_then(|t| names.get(&t))
                .is_some_and(|n| c.gold.contains(n))
            {
                top_right += 1;
            }
            if c.gold.iter().any(|g| chosen.contains(&g.as_str())) {
                right += 1;
            } else {
                misses.push(format!(
                    "{} ({}) attached {chosen:?}, wanted {:?}: {} [{obs}]",
                    c.id, c.category, c.gold, c.query
                ));
            }
        }
    }
    core.shutdown().await;
    Outcome {
        right,
        related,
        top: top_right,
        abstained,
        none,
        misses,
    }
}

/// リランカーのある構成で、関係の無い問いの 80% 以上で何も添えず、正しく添える割合が 80% 以上。
/// verifies: AC-FR-402-3
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs embedding and reranking llama-servers (cargo xtask live-llm)"]
async fn live_recall_cases_with_a_reranker() {
    let Some((embed, rerank)) = servers() else {
        return;
    };
    let Outcome {
        right,
        related,
        abstained,
        none,
        misses,
        ..
    } = run("tests/retrieval/recall-v0.1.toml", &embed, &rerank).await;
    println!(
        "live recall: {right}/{related} attached the right exchange; nothing attached on {abstained}/{none} unrelated questions"
    );
    println!("missed: {misses:#?}");
    assert!(
        abstained * 10 >= none * 8,
        "AC-FR-402-3: abstained on {abstained}/{none} (missed {misses:#?})"
    );
    assert!(
        right * 10 >= related * 8,
        "AC-FR-402-3: {right}/{related} (missed {misses:#?})"
    );
}

/// 共通の文字の無い言い換えの事例で、正しい往復がベクトルの候補に入り、リランクの最上位になる (添えるかは
/// リランクの値としきい値で決める、ADR-0019。語彙の索引だけでは候補に入らない)。
/// verifies: AC-FR-403-1
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs embedding and reranking llama-servers (cargo xtask live-llm)"]
async fn live_paraphrases_reach_the_right_exchange_through_vectors() {
    let Some((embed, rerank)) = servers() else {
        return;
    };
    // 事例が「漢字と英数字を共有しない」ことを先に確かめる (事例の誤りで合格しないように)。
    let text =
        std::fs::read_to_string(workspace().join("tests/retrieval/recall-v0.2-para.toml")).unwrap();
    let cases: Cases = toml::from_str(&text).unwrap();
    let content = |s: &str| -> std::collections::HashSet<char> {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    for c in &cases.case {
        let (sid, n) = c.gold[0].split_once("-t").unwrap();
        let s = cases.session.iter().find(|s| s.id == sid).unwrap();
        let i: usize = n.parse().unwrap();
        let exchange = format!("{}{}", s.turns[i - 1], s.turns[i]);
        let shared: Vec<char> = content(&c.query)
            .intersection(&content(&exchange[..]))
            .copied()
            .collect();
        assert!(shared.is_empty(), "{}: shares {shared:?}", c.id);
    }
    let o = run("tests/retrieval/recall-v0.2-para.toml", &embed, &rerank).await;
    println!(
        "live paraphrases: the right exchange ranked first in {}/{}, attached in {}/{} (missed {:#?})",
        o.top, o.related, o.right, o.related, o.misses
    );
    assert!(
        o.top * 10 >= o.related * 8,
        "AC-FR-403-1: ranked first in {}/{} (missed {:#?})",
        o.top,
        o.related,
        o.misses
    );
}
