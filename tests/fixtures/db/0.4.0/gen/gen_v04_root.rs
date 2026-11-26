//! v0.4 のデータ根の見本 (`tests/fixtures/db/0.4.0/root/`) を、v0.4.0 のコードの書き込みの経路で作る
//! (09 §3.6 の「v0.4 のデータ根からの取り込み」、M5.1)。
//!
//! v0.4.0 のタグを展開した木の `crates/evorag-core/examples/` に置いて、次で走らせる:
//!
//! ```text
//! cargo run -p evorag-core --example gen_v04_root --locked -- <空の出力先のフォルダ>
//! ```
//!
//! llama-server は使わない。応答と裏の補助の呼び出し (抽出・ノート・訂正) は、要求の型縛りの名前で振り分けて、
//! この中に固定した答えを返す (`Scripted`)。リランカーは `ReplayClient` の再生 (共通の文字の数 − 10)。
//! 会話はすべて架空のもの。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::{ReplayClient, toy_rerank};
use evorag_llm::{
    BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, ServerInfo, SlotState,
};
use evorag_memory::{
    KIND_TURN, METHOD_BUNDLED, ModelProbeBody, REL_IN_SESSION, TARGET_CONVERSATION, ThresholdBody,
    TurnBody, TurnRole, TurnState,
};
use evorag_retrieval::calibrate::exchange_text;
use evorag_store::{BodyPatch, Envelope, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer};
use evorag_types::api::{
    Command, Event, FactKey, FactList, FactRow, FactRowRef, FactStatus, ForgetOptions,
    PrivateScope, StyleItem, TurnView,
};
use evorag_types::{Clock, Id, IdGen, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

/// v0.4.0 として書く (`max_writer` が 0.4.0 になる、03 §12.4 の取り込める根の (1))。
const VERSION: &str = "0.4.0";
/// 最初の時刻: 2026-11-15T00:00:00Z (日本時間の 09:00)。
const T0: i64 = 1_794_700_800_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const WAIT: Duration = Duration::from_secs(60);

const REPLY: &str = "了解です";
const PLANT: &str = "モンステラの水やりは、どれくらいの頻度がいい？";
const CAR: &str = "車のオイル交換の頻度は、どのくらいがいい？";
const ASK: &str = "前に聞いたモンステラの水やりの頻度、もう一度教えて";

/// 抽出の答え: 発話にこの語が含まれれば、(属性, 値, 根拠) の候補を返す。
const FACTS: &[(&str, &str, &str, &str)] = &[
    (
        "大阪に住んで",
        "residence",
        "大阪",
        "私は大阪に住んでいます",
    ),
    ("東京に住んで", "residence", "東京", "東京に住んでるよ"),
    ("猫を飼って", "pet", "猫", "猫を飼っています"),
    (
        "パン屋で働いて",
        "occupation",
        "パン屋",
        "パン屋で働いています",
    ),
    (
        "花屋で働いて",
        "occupation",
        "花屋",
        "転職して、今は花屋で働いています",
    ),
    (
        "えびは食べられない",
        "dietary",
        "えび",
        "えびは食べられない",
    ),
    (
        "かにも食べられない",
        "dietary",
        "かに",
        "かにも食べられない",
    ),
    ("趣味は将棋", "hobby", "将棋", "趣味は将棋です"),
    (
        "チョコレートが好き",
        "likes",
        "チョコレート",
        "チョコレートが好きです",
    ),
    ("誕生日は5月3日", "birthday", "5月3日", "誕生日は5月3日です"),
];

fn server_json(chats: &str) -> String {
    format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{chats}],
          "slots": [{{"id": 0, "is_processing": false}}, {{"id": 1, "is_processing": false}}]
        }}"#
    )
}

fn events(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{"finish_reason": "stop"}}}}]}}"#,
        serde_json::to_string(text).expect("a string encodes")
    )
}

/// 要求の型縛りの名前で答えを選ぶ llama-server の代わり。
struct Scripted {
    base: ReplayClient,
    calls: Mutex<BTreeMap<String, usize>>,
}

impl Scripted {
    fn new() -> Self {
        Self {
            base: ReplayClient::from_json(&server_json("")).expect("the cassette parses"),
            calls: Mutex::new(BTreeMap::new()),
        }
    }

    fn answer(&self, req: &ChatRequest) -> String {
        let name = req
            .extra
            .get("response_format")
            .and_then(|f| f.pointer("/json_schema/name"))
            .and_then(|n| n.as_str())
            .unwrap_or("answer")
            .to_owned();
        *self
            .calls
            .lock()
            .expect("lock poisoned")
            .entry(name.clone())
            .or_default() += 1;
        match name.as_str() {
            "fact_candidates" => {
                let input = req.messages.last().map_or("", |m| m.content.as_str());
                let mut out = Vec::new();
                for line in input.lines() {
                    let Some((n, text)) = line
                        .strip_prefix('[')
                        .and_then(|l| l.split_once("] "))
                        .and_then(|(n, t)| n.parse::<u32>().ok().map(|n| (n, t)))
                    else {
                        continue;
                    };
                    for (needle, attr, value, evidence) in FACTS {
                        if text.contains(needle) {
                            out.push(serde_json::json!({
                                "t": n, "s": "self", "a": attr, "v": value, "e": evidence,
                                "m": "fact", "tn": "current", "old": ""
                            }));
                        }
                    }
                }
                serde_json::json!({ "c": out }).to_string()
            }
            "note_keys" => r#"{"keys": ["会話"]}"#.to_owned(),
            "correction" => r#"{"target": "response", "old": "", "new": "", "e": ""}"#.to_owned(),
            _ => REPLY.to_owned(),
        }
    }

    fn calls(&self) -> BTreeMap<String, usize> {
        self.calls.lock().expect("lock poisoned").clone()
    }
}

impl LlmClient for Scripted {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        self.base.health()
    }
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        self.base.server_info()
    }
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        self.base.count_tokens(text)
    }
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        let one = ReplayClient::from_json(&server_json(&events(&self.answer(&req))))
            .expect("the cassette parses");
        Box::pin(async move { one.chat(req).await })
    }
    fn slots(&self) -> BoxFuture<'_, Result<Vec<SlotState>, LlmError>> {
        self.base.slots()
    }
    fn is_loopback(&self) -> bool {
        self.base.is_loopback()
    }
    fn erase_slot(&self, id: u32) -> BoxFuture<'_, Result<(), LlmError>> {
        self.base.erase_slot(id)
    }
}

fn reranker() -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(
            r#"{
              "props": {"build_info": "b1-test", "model_path": "/m/rr.gguf", "total_slots": 4,
                        "default_generation_settings": {"n_ctx": 8192}},
              "models": {"data": [{"id": "rr", "meta": {"n_params": 5, "size": 6, "n_vocab": 7}}]},
              "rerank": true
            }"#,
        )
        .expect("the cassette parses"),
    )
}

fn model_key(path: &str, id: &str, meta: (u64, u64, u64)) -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(meta.0),
        size: Some(meta.1),
        n_vocab: Some(meta.2),
        n_ctx_train: None,
    };
    evorag_llm::model_key(path, &meta, id)
}

fn score(user: &str) -> f64 {
    f64::from(toy_rerank(ASK, &exchange_text("ja", user, REPLY)))
}

/// 起動の前に、応答のモデルの点検の結果 (型縛りを使える) と、リランカーのしきい値を書く。
fn prepare(root: &Path) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0 - HOUR)));
    let m = DataRootModels::open(
        root,
        VERSION,
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, 3)),
    )
    .expect("the data root opens");
    m.save_probe(&ModelProbeBody {
        role: "chat".into(),
        model_key: model_key("/m/chat.gguf", "test", (1, 2, 3)),
        build_info: "b1-test".into(),
        structured_ok: Some(true),
        thinking_off: Some(true),
        ..ModelProbeBody::default()
    })
    .expect("the probe is saved");
    m.save_threshold(&ThresholdBody {
        reranker_key: model_key("/m/rr.gguf", "rr", (5, 6, 7)),
        target: TARGET_CONVERSATION.into(),
        version: 1,
        method: METHOD_BUNDLED.into(),
        cases: evorag_retrieval::calibrate::cases_id("ja"),
        passed: true,
        threshold: Some(score(PLANT) + 1.0),
        band_low: Some(score(CAR) - 1.0),
        stats: Default::default(),
    })
    .expect("the threshold is saved");
}

fn config(root: &Path, seed: u64, clock: Arc<ManualClock>) -> CoreConfig {
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: VERSION.into(),
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
        rerank: Some(reranker() as Arc<dyn LlmClient>),
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

struct Run {
    core: Core,
    rx: UnboundedReceiver<Event>,
    clock: Arc<ManualClock>,
    llm: Arc<Scripted>,
}

impl Run {
    async fn start(root: &Path, seed: u64, at: i64) -> (Self, evorag_core::StartReport) {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
        let llm = Arc::new(Scripted::new());
        let (core, rx, report) = Core::start(
            config(root, seed, clock.clone()),
            llm.clone() as Arc<dyn LlmClient>,
        )
        .await
        .expect("v0.4.0 opens the data root");
        let run = Self {
            core,
            rx,
            clock,
            llm,
        };
        run.core.settle().await;
        run.settle().await;
        (run, report)
    }

    async fn settle(&self) {
        self.core.settle_background_within(WAIT).await;
        self.core.settle_precompute().await;
    }

    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut got = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, self.rx.recv())
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

    async fn new_session(&mut self, title: &str) -> Id {
        self.clock.advance(10 * MINUTE);
        self.core.command(Command::NewSession {
            title: title.into(),
        });
        let ev = self
            .until(|e| matches!(e, Event::SessionOpened { .. }))
            .await;
        match ev.last() {
            Some(Event::SessionOpened { session, .. }) => session.id,
            other => panic!("{other:?}"),
        }
    }

    /// 発話して応答を待ち、裏の処理 (抽出・統合・事実の表・ノート) を終えるまで待つ。応答のターンを返す。
    async fn say(&mut self, text: &str) -> TurnView {
        self.clock.advance(2 * MINUTE);
        self.core.hold(true);
        self.core.command(Command::Send { text: text.into() });
        let ev = self
            .until(|e| matches!(e, Event::ResponseFinished { .. } | Event::Rejected { .. }))
            .await;
        let turn = match ev.last() {
            Some(Event::ResponseFinished { turn, .. }) => turn.clone(),
            other => panic!("{text}: {other:?}"),
        };
        self.core.settle().await;
        self.core.settle_precompute().await;
        self.core.hold(false);
        self.settle().await;
        turn
    }

    async fn fact_event(&mut self, cmd: Command) -> Event {
        self.clock.advance(MINUTE);
        self.core.command(cmd);
        self.until(|e| {
            matches!(
                e,
                Event::FactsInfo { .. }
                    | Event::FactsUpdated { .. }
                    | Event::FactOpRejected { .. }
                    | Event::FactHistoryInfo { .. }
                    | Event::ForgottenInfo { .. }
            )
        })
        .await
        .pop()
        .expect("an event")
    }

    async fn facts(&mut self) -> FactList {
        match self.fact_event(Command::ShowFacts).await {
            Event::FactsInfo { list } => list,
            other => panic!("{other:?}"),
        }
    }

    async fn key_of(&mut self, value: &str) -> FactKey {
        let list = self.facts().await;
        find(&list, value).unwrap_or_else(|| panic!("{value} is not in {list:?}"))
    }

    async fn expect_updated(&mut self, cmd: Command) {
        let ev = self.fact_event(cmd).await;
        assert!(matches!(ev, Event::FactsUpdated { .. }), "{ev:?}");
        self.settle().await;
    }

    async fn forget(&mut self, value: &str, redact: bool) {
        let key = self.key_of(value).await;
        let options = ForgetOptions {
            redact,
            ..ForgetOptions::default()
        };
        self.clock.advance(MINUTE);
        self.core.command(Command::PreviewForget {
            target: key.clone(),
            options,
        });
        let ev = self
            .until(|e| {
                matches!(
                    e,
                    Event::ForgetPreviewInfo { .. } | Event::ForgetRejected { .. }
                )
            })
            .await;
        let plan = match ev.last() {
            Some(Event::ForgetPreviewInfo { preview }) => {
                preview.plan.clone().expect("the forget is planned")
            }
            other => panic!("{value}: {other:?}"),
        };
        self.core.command(Command::ForgetFact {
            target: key,
            options,
            plan,
        });
        let ev = self
            .until(|e| {
                matches!(
                    e,
                    Event::FactForgotten { .. } | Event::ForgetRejected { .. }
                )
            })
            .await;
        assert!(
            matches!(ev.last(), Some(Event::FactForgotten { .. })),
            "{value}: {:?}",
            ev.last()
        );
        self.settle().await;
    }

    async fn delete_session(&mut self, id: Id) {
        self.clock.advance(MINUTE);
        self.core.command(Command::OpenSession { id });
        self.until(|e| matches!(e, Event::SessionOpened { .. }))
            .await;
        self.core.command(Command::DeleteSession);
        self.until(|e| matches!(e, Event::SessionDeleted { .. }))
            .await;
        self.core.settle().await;
        self.settle().await;
    }

    async fn style(&mut self, item: StyleItem, value: &str) {
        self.core.command(Command::Style {
            set: Some((item, value.into())),
        });
        let ev = self
            .until(|e| matches!(e, Event::StyleInfo { .. } | Event::Rejected { .. }))
            .await;
        assert!(
            matches!(ev.last(), Some(Event::StyleInfo { .. })),
            "{:?}",
            ev.last()
        );
    }

    async fn shutdown(self) -> BTreeMap<String, usize> {
        let calls = self.llm.calls();
        self.core.shutdown().await;
        calls
    }
}

fn find(list: &FactList, value: &str) -> Option<FactKey> {
    list.groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .find_map(|r| match r {
            FactRow::Value { key, value: v, .. } if v == value => Some(key.clone()),
            _ => None,
        })
}

/// 閉じたデータ根を v0.4.0 のストアと書き手で直接開く (古い版の書き込みや壊れをまねる)。
struct Closed {
    store: Arc<Store>,
    handle: evorag_store::WriterHandle,
    thread: std::thread::JoinHandle<()>,
    clock: Arc<ManualClock>,
    ids: UuidV7Gen,
}

impl Closed {
    fn open(root: &Path, at: i64) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
        let (store, _) = Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: VERSION.into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 41)),
            config_file: None,
        })
        .expect("the store opens");
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).expect("the writer starts");
        let thread = std::thread::spawn(move || writer.run());
        Self {
            store,
            handle,
            thread,
            ids: UuidV7Gen::with_seed(clock.clone(), 42),
            clock,
        }
    }

    fn write(&self, ops: Vec<WriteOp>) {
        self.handle
            .submit_wait(WriteRequest::conversation(ops))
            .expect("the write is applied");
    }

    fn turn(&self, id: Id) -> TurnBody {
        let tx = self.store.read().expect("a read transaction");
        match tx.get(KIND_TURN, id).expect("the turn is read") {
            Some(Read::Ok(e)) => e.body_as().expect("a turn body"),
            other => panic!("{other:?}"),
        }
    }

    fn close(self) {
        self.handle.shutdown();
        self.thread.join().expect("the writer thread ends");
    }
}

fn main() {
    let out: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: gen_v04_root <empty output folder>");
    if out.exists() {
        assert!(
            std::fs::read_dir(&out)
                .expect("the output folder is readable")
                .next()
                .is_none(),
            "the output folder {} is not empty",
            out.display()
        );
    }
    std::fs::create_dir_all(&out).expect("the output folder is created");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    rt.block_on(generate(&out));
}

async fn generate(root: &Path) {
    prepare(root);
    let mut ids: BTreeMap<&str, Id> = BTreeMap::new();

    // 1 回目: 会話と、事実への操作。
    let (mut run, _) = Run::start(root, 7, T0).await;
    let life = run.new_session("暮らしの話").await;
    ids.insert("session:暮らしの話", life);
    for text in [
        "私は大阪に住んでいます",
        "猫を飼っています",
        "パン屋で働いています",
        "実は、えびは食べられないんです",
        "実は、かにも食べられないんです",
        "趣味は将棋です",
        "チョコレートが好きです",
        "誕生日は5月3日です",
    ] {
        let t = run.say(text).await;
        ids.insert(text, t.reply_to.expect("an utterance"));
    }
    // 評価 (信号)。
    run.core.command(Command::Rate {
        good: true,
        reason: None,
    });
    run.until(|e| matches!(e, Event::Rated { .. })).await;

    let recent = run.new_session("近況").await;
    ids.insert("session:近況", recent);
    let t = run.say("東京に住んでるよ").await;
    ids.insert("東京に住んでるよ", t.reply_to.expect("an utterance"));

    // 想起への操作 (候補の帯の「無関係」と「添えて作り直す」)。
    let s = run.new_session("観葉植物").await;
    ids.insert("session:観葉植物", s);
    let plant = run.say(PLANT).await.reply_to.expect("an utterance");
    let s = run.new_session("車").await;
    ids.insert("session:車", s);
    let car = run.say(CAR).await.reply_to.expect("an utterance");
    let s = run.new_session("今日").await;
    ids.insert("session:今日", s);
    let answer = run.say(ASK).await;
    let band: Vec<Id> = answer.band.iter().map(|b| b.target).collect();
    assert!(
        band.contains(&plant) && band.contains(&car),
        "the band {band:?} has both exchanges"
    );
    run.core.command(Command::ToggleIrrelevant {
        answer: answer.id,
        target: car,
    });
    run.until(|e| matches!(e, Event::IrrelevantChanged { .. }))
        .await;
    run.core.command(Command::AttachBand {
        answer: answer.id,
        target: plant,
    });
    let ev = run
        .until(|e| matches!(e, Event::ResponseFinished { .. } | Event::Rejected { .. }))
        .await;
    assert!(
        matches!(ev.last(), Some(Event::ResponseFinished { .. })),
        "{:?}",
        ev.last()
    );
    run.settle().await;

    // 採る・戻す。
    let tokyo = run.key_of("東京").await;
    assert_eq!(tokyo.status, FactStatus::Contested);
    run.expect_updated(Command::AcceptFact { target: tokyo })
        .await;
    let ev = run
        .fact_event(Command::ShowFactHistory {
            group: evorag_types::api::FactGroupKey {
                subject: evorag_types::api::FactSubject::User,
                attribute: "residence".into(),
            },
        })
        .await;
    let Event::FactHistoryInfo { history } = ev else {
        panic!("{ev:?}")
    };
    let osaka = history
        .items
        .iter()
        .find(|h| h.value == "大阪")
        .expect("大阪 is in the history")
        .key
        .clone();
    run.expect_updated(Command::RevertFact { target: osaka })
        .await;

    // private とその解除。えびは private のまま残す。
    let cat = run.key_of("猫").await;
    run.expect_updated(Command::SetFactPrivate {
        target: cat.clone(),
        scope: PrivateScope::Successors,
    })
    .await;
    let cat = run.key_of("猫").await;
    run.expect_updated(Command::UnsetFactPrivate {
        target: FactRowRef::Value(cat),
    })
    .await;
    let shrimp = run.key_of("えび").await;
    run.expect_updated(Command::SetFactPrivate {
        target: shrimp,
        scope: PrivateScope::ValueOnly,
    })
    .await;

    // 忘れる: 伏せ字にする (かに)・しない (将棋)・印を解く (チョコレート)・後で本文を合わなくする (5月3日)。
    run.forget("かに", true).await;
    run.forget("将棋", false).await;
    run.forget("チョコレート", true).await;
    let ev = run.fact_event(Command::ShowForgotten).await;
    let Event::ForgottenInfo { items } = ev else {
        panic!("{ev:?}")
    };
    let likes = items
        .iter()
        .find(|f| f.attr.name == "likes")
        .expect("the likes forget is listed")
        .target;
    run.expect_updated(Command::UnblockFact { target: likes })
        .await;
    run.forget("5月3日", true).await;

    // 置き換えの連鎖の新しい側を消す (今の値が無くなる、OQ-49)。
    let work = run.new_session("仕事").await;
    ids.insert("session:仕事(削除)", work);
    let t = run.say("転職して、今は花屋で働いています").await;
    ids.insert(
        "転職して、今は花屋で働いています",
        t.reply_to.expect("an utterance"),
    );
    let florist = run.key_of("花屋").await;
    assert_eq!(florist.status, FactStatus::Live);
    run.delete_session(work).await;
    let list = run.facts().await;
    assert!(find(&list, "花屋").is_none() && find(&list, "パン屋").is_none());

    // ただのセッションの削除 (後で、このセッションの ID に、セッションの無いターンを足す)。
    let chat = run.new_session("雑談").await;
    ids.insert("session:雑談(削除)", chat);
    run.say("今日はいい天気ですね").await;
    run.delete_session(chat).await;

    // 答え方の既定。
    run.style(StyleItem::Length, "short").await;
    run.style(StyleItem::Format, "plain").await;
    let end1 = run.clock.now().as_millis();
    let calls = run.shutdown().await;
    println!("run 1 calls: {calls:?}");

    // 直接の書き込み: セッションの無いターン (古い版で戻したセッションに足した会話をまねる) と、伏せ字の記録と
    // 合わないターン (伏せ字の後の番号のまま本文を変える)。
    let closed = Closed::open(root, end1 + HOUR);
    let orphan = closed.ids.next_id();
    let mut env = Envelope::new(orphan, KIND_TURN, 1, closed.clock.now(), "");
    env.set_body(&TurnBody {
        session: chat,
        role: TurnRole::User,
        state: TurnState::Pending,
        text: "さっきの続きだけど、明日は雨らしいよ".into(),
        model_key: None,
        generation: None,
        attached: Vec::new(),
        reply_to: None,
        error: None,
        recall: None,
        obs: None,
    })
    .expect("a turn body encodes");
    closed.write(vec![
        WriteOp::Put(env),
        WriteOp::PutLink {
            from: orphan,
            rel: REL_IN_SESSION.into(),
            to: chat,
            attrs: Vec::new(),
        },
    ]);
    ids.insert("orphan-turn", orphan);
    let birthday = ids["誕生日は5月3日です"];
    let redacted = closed.turn(birthday).text;
    let tampered = redacted.replace("です", "でした");
    assert_ne!(redacted, tampered, "{redacted}");
    closed.write(vec![WriteOp::Patch {
        kind: KIND_TURN.into(),
        id: birthday,
        fields: BodyPatch::new()
            .set("text", &tampered)
            .expect("a text encodes"),
        updated_at: None,
    }]);
    closed.close();

    // 2 回目: v0.4.0 で開き直す (ジャーナルを取り込み、忘れるを当て直し、`forget.recheck` を残さない)。
    let (run, report) = Run::start(root, 9, end1 + 2 * HOUR).await;
    println!("run 2 forget: {:?}", report.forget);
    let calls = run.shutdown().await;
    println!("run 2 calls: {calls:?}");

    for (name, id) in &ids {
        println!("id {name}: {id}");
    }
}
