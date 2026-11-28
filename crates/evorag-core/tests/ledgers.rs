//! 区切りと、区切りごとの 1 回の呼び出し (B2) の裏の処理 (04 §4 の手順 2・3、§5.2・§5.2.1、§6、06 §2.1・§3.3)。
//! llama-server は記録の再生で置き換える (09 §3.3)。
//!
//! 会話は、コアを起動する前に v0.5 の根へ直接書いておく (応答の要求と B2 の要求が同じ記録の列を取り合わないように)。
//! 応答のモデルの点検の結果も書いておき (B2 の型の点検に通った)、起動の後の裏の処理が B2 を呼ぶ。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evorag_core::{Core, CoreConfig, MemoryConfig, MemoryRoute, PromptLanguage, RootFormat};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{
    BoxFuture, ChatRequest, ChatStream, Generation, LlmClient, LlmError, Role, ServerInfo,
};
use evorag_memory::{
    AssistantTurn, DirectiveBody, DirectiveOp, KIND_SEGMENT, KIND_SESSION, KIND_TURN, LedgerBody,
    ModelProbeBody, SegmentBody, TurnSpan, TurnState, Writes,
};
use evorag_store::testutil::TempDir;
use evorag_store::{
    Envelope, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle,
};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, Id, ManualClock, Timestamp, UuidV7Gen};
use sha2::{Digest, Sha256};

const NOW: i64 = 1_795_000_000_000;
const MIN: i64 = 60_000;

/// 応答のモデル (記録の再生の llama-server) の model_key。
fn key_of(file: &str, size: u64) -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(1),
        size: Some(size),
        n_vocab: Some(3),
        n_ctx_train: None,
    };
    evorag_llm::model_key(&format!("/m/{file}"), &meta, file)
}

fn chat_key() -> String {
    key_of("chat.gguf", 2)
}

/// `file` のモデルを動かす llama-server。`chats` は順に使う答え。
fn server(file: &str, size: u64, n_ctx: u32, healthy: bool, chats: &[String]) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/{file}", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": {n_ctx}}}}},
          "models": {{"data": [{{"id": "{file}", "meta": {{"n_params": 1, "size": {size}, "n_vocab": 3}}}}]}},
          "health": {healthy},
          "chats": [{}]
        }}"#,
        chats.join(", ")
    ))
    .expect("a cassette")
}

fn chat(chats: &[String]) -> ReplayClient {
    server("chat.gguf", 2, 8192, true, chats)
}

/// 生成を終えた答え。
fn out(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{"prompt_tokens": 120, "completion_tokens": 30}}}}]}}"#,
        serde_json::to_string(text).expect("a string")
    )
}

/// 単位の候補 1 つ (wire の形)。
fn unit(s: u32, a: &str, v: &str) -> String {
    format!(
        r#"{{"s": {s}, "l": "world", "e": "self", "a": "{a}", "v": "{v}", "m": "fact", "tn": "current", "old": "", "when": ""}}"#
    )
}

fn output(keys: &[&str], units: &[String], corrections: &str) -> String {
    format!(
        r#"{{"keys": {}, "units": [{}], "corrections": [{corrections}]}}"#,
        serde_json::to_string(keys).expect("keys"),
        units.join(", ")
    )
}

/// コアを起動する前に、v0.5 の根に会話と点検の結果を書く口。
struct Seed {
    store: Arc<Store>,
    handle: WriterHandle,
    thread: Option<std::thread::JoinHandle<()>>,
    clock: Arc<ManualClock>,
    ids: Arc<UuidV7Gen>,
}

impl Seed {
    fn open(root: &Path, at: i64, seed: u64) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
        let (kinds, plan) = evorag_memory::v05_root();
        let (store, _) = Store::open_with(
            OpenOptions {
                data_root: root.to_path_buf(),
                evorag_version: "0.5.0".into(),
                kinds,
                clock: clock.clone(),
                ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), seed)),
                config_file: None,
            },
            plan,
        )
        .expect("open the v0.5 root");
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).expect("a writer");
        let thread = std::thread::spawn(move || writer.run());
        Self {
            store,
            handle,
            thread: Some(thread),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), seed + 100)),
            clock,
        }
    }

    fn writes(&self) -> Writes<'_> {
        Writes {
            clock: &*self.clock,
            ids: &*self.ids,
        }
    }

    fn write(&self, ops: Vec<WriteOp>) {
        self.handle
            .submit_wait(WriteRequest::conversation(ops))
            .expect("write");
    }

    fn get(&self, kind: &str, id: Id) -> Envelope {
        match self.store.read().expect("read").get(kind, id).expect("get") {
            Some(Read::Ok(e)) => e,
            other => panic!("{kind} {id} is not readable: {other:?}"),
        }
    }

    fn session(&self, title: &str) -> Id {
        let (id, ops) = self.writes().create_session(title).expect("a session");
        self.write(ops);
        id
    }

    /// 往復 1 つ (発話と、あれば完了した応答)。1 秒ずつ時計を進める。
    fn pair(&self, session: Id, user: &str, reply: Option<&str>) -> (Id, Option<Id>) {
        self.clock.advance(1_000);
        let s = self.get(KIND_SESSION, session);
        let (u, ops) = self.writes().user_turn(&s, user).expect("an utterance");
        self.write(ops);
        let Some(reply) = reply else {
            return (u, None);
        };
        self.clock.advance(1_000);
        let s = self.get(KIND_SESSION, session);
        let ue = self.get(KIND_TURN, u);
        let (r, ops) = self
            .writes()
            .assistant_turn(
                &s,
                &ue,
                AssistantTurn {
                    state: TurnState::Completed,
                    text: reply.into(),
                    model_key: Some(chat_key()),
                    generation: None,
                    attached: Vec::new(),
                    error: None,
                    recall: None,
                    obs: None,
                },
            )
            .expect("a reply");
        self.write(ops);
        (u, Some(r))
    }

    /// 点検の結果 (型縛りと B2 の型の点検に通った)。
    fn probe(&self, role: &str, model_key: &str) {
        let ops = self
            .writes()
            .save_probe(
                None,
                &ModelProbeBody {
                    role: role.into(),
                    model_key: model_key.into(),
                    build_info: "b1-test".into(),
                    structured_ok: Some(true),
                    thinking_off: Some(true),
                    b2_ok: Some(true),
                    ..ModelProbeBody::default()
                },
            )
            .expect("a probe");
        self.write(ops);
    }

    /// 忘れるの指示を書く前に、忘れるの鍵を作っておく (指示があるのに鍵が無い根は、事実の機能を止める、03 §3.1.1)。
    fn directive(&self, body: &DirectiveBody) {
        let key = evorag_core::forget_key::ensure(&self.store, Some(&self.handle)).expect("a key");
        assert!(key.key().is_some());
        let (_, ops) = self.writes().directive(body).expect("a directive");
        self.write(ops);
    }

    fn segments(&self) -> Vec<SegmentBody> {
        let r = self.store.read().expect("read");
        let mut out = Vec::new();
        r.for_each(KIND_SEGMENT, |x| {
            if let Read::Ok(e) = x {
                out.push(e.body_as().expect("a segment"));
            }
            true
        })
        .expect("for each");
        out
    }
}

impl Drop for Seed {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn config(
    root: &Path,
    at: i64,
    memory: MemoryConfig,
    sleep: evorag_core::SleepConfig,
) -> CoreConfig {
    config_with(
        root,
        Arc::new(ManualClock::new(Timestamp::from_millis(at))),
        memory,
        sleep,
    )
}

/// 時計を渡す形 (同じプロセスの中で時計を進める試験)。
fn config_with(
    root: &Path,
    clock: Arc<ManualClock>,
    memory: MemoryConfig,
    sleep: evorag_core::SleepConfig,
) -> CoreConfig {
    let at = clock.now().as_millis();
    let mut aux = evorag_llm::RequestExtra::new();
    aux.insert(
        "chat_template_kwargs".into(),
        serde_json::json!({"enable_thinking": false}),
    );
    CoreConfig {
        data_root: root.to_path_buf(),
        root_format: RootFormat::V05,
        config_file: None,
        evorag_version: "0.5.0".into(),
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
        rerank: None,
        memory,
        aux_request_extra: aux,
        probe_models: false,
        sleep,
        memory_enabled: true,
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, (at / 1_000) as u64)),
    }
}

/// コアを `at` の時刻で起動し、裏の処理が落ち着くまで待つ。
async fn run_core(root: &Path, at: i64, llm: &ReplayClient) -> Vec<(Id, LedgerBody)> {
    let (core, _rx, _) = Core::start(
        config(
            root,
            at,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.settle_background().await;
    let ledgers = core.ledger_bodies();
    core.shutdown().await;
    ledgers
}

/// 要求の利用者のメッセージ。
fn user(r: &ChatRequest) -> &str {
    r.messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .expect("a user message")
}

/// 要求のメッセージから求めた `input_hash` (04 §5.2.1 の「細目」: システムと利用者のメッセージを、u32be の長さを
/// 前に置いて呼び出しの順に並べたバイト列の SHA-256)。
fn input_hash(reqs: &[&ChatRequest]) -> String {
    let mut h = Sha256::new();
    for r in reqs {
        for role in [Role::System, Role::User] {
            let m = r
                .messages
                .iter()
                .find(|m| m.role == role)
                .expect("a message");
            h.update(u32::try_from(m.content.len()).expect("short").to_be_bytes());
            h.update(m.content.as_bytes());
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn is_b2(r: &ChatRequest) -> bool {
    r.extra
        .get("response_format")
        .and_then(|f| f.pointer("/json_schema/name"))
        .and_then(|n| n.as_str())
        == Some("b2_segment")
}

/// 閉じた区切り 1 つ (3 往復。3 つ目は訂正の候補)。
struct Basic {
    u: [Id; 3],
    r: [Id; 3],
}

fn seed_basic(root: &Path) -> Basic {
    let seed = Seed::open(root, NOW - 30 * MIN, 1);
    seed.probe("chat", &chat_key());
    let s = seed.session("暮らし");
    let (u1, r1) = seed.pair(s, "私は大阪に住んでいます。", Some("大阪はいい街ですね。"));
    let (u2, r2) = seed.pair(s, "週末は将棋を指します。", Some("将棋は奥が深いですね。"));
    let (u3, r3) = seed.pair(
        s,
        "いや、大阪じゃなくて京都に住んでいます。",
        Some("京都なのですね。"),
    );
    Basic {
        u: [u1, u2, u3],
        r: [r1.expect("r1"), r2.expect("r2"), r3.expect("r3")],
    }
}

fn basic_output() -> String {
    output(
        &["京都の暮らし", "将棋"],
        &[unit(1, "residence", "大阪"), unit(3, "residence", "京都")],
        r#"{"id": 1, "verdict": "accept"}"#,
    )
}

/// 閉じた区切りは 1 回の B2 の呼び出しで台帳になる。台帳は入力の対応表 (`sentences`・`refs`・`corr`・`through`)・
/// 版・呼んだ先 (`model_key`・`build_info`)・変種と、送ったバイト列の `input_hash` を持つ。閉じた区切りの記録も
/// 書く。同じ入力からは同じバイト列の要求になり、もう一度起動しても呼び直さない。
///
/// verifies: AC-FR-313-1 (区切りごとに 1 回の要求・番号は発話の文だけ・台帳の項目)
/// 04 §5.2.1 (`input_hash`・要求の形), 04 §6 (訂正の候補と直前の応答), 06 §6 (型縛り・temperature 0・固定の seed)
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_segment_is_called_once_and_its_ledger_records_the_inputs() {
    let dir = TempDir::new("b2-closed");
    let root = dir.path().join("a");
    let b = seed_basic(&root);
    let llm = chat(&[out(&basic_output())]);
    let ledgers = run_core(&root, NOW, &llm).await;

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 1, "one call for the segment");
    let req = &reqs[0];
    assert!(is_b2(req));
    assert_eq!(req.generation.temperature, 0.0);
    assert_eq!(req.generation.seed, Some(1));
    assert_eq!(
        req.generation.max_tokens,
        Some(evorag_memory::b2::MAX_TOKENS)
    );
    assert!(
        req.extra.contains_key("chat_template_kwargs"),
        "the aux extra is added"
    );
    let text = user(req);
    assert!(text.contains("[1] 私は大阪に住んでいます。"), "{text}");
    assert!(
        text.contains("[3] いや、大阪じゃなくて京都に住んでいます。"),
        "{text}"
    );
    assert!(text.contains("[参照のみ]\n大阪はいい街ですね。"), "{text}");
    assert!(text.contains("訂正の候補:"), "{text}");
    assert!(text.contains("(候補 1)"), "{text}");
    // 直前の応答 (候補の発話より前の最後の応答) を枠に入れる。
    assert!(text.contains("直前の応答:"), "{text}");

    assert_eq!(ledgers.len(), 1, "{ledgers:?}");
    let l = &ledgers[0].1;
    assert_eq!(l.call, "segment");
    assert_eq!(l.status, "complete");
    assert_eq!(l.variant, evorag_memory::b2::VARIANT);
    assert_eq!(l.model_key, chat_key());
    assert_eq!(l.build_info.as_deref(), Some("b1-test"));
    assert_eq!(l.failed_parts, 0);
    assert_eq!(l.attempts, 0);
    assert_eq!((l.tokens.input, l.tokens.output), (120, 30));
    let turns: Vec<(u32, Id)> = l.sentences.iter().map(|s| (s.s, s.turn)).collect();
    assert_eq!(turns, [(1, b.u[0]), (2, b.u[1]), (3, b.u[2])]);
    let refs: Vec<Id> = l.inputs.refs.iter().map(|r| r.turn).collect();
    assert_eq!(
        refs, b.r,
        "the replies of the pairs (the previous reply is r2)"
    );
    let through = l.inputs.through.expect("through");
    assert_eq!((through.turn, through.pairs), (b.u[2], 3));
    assert_eq!(
        l.inputs.correction,
        evorag_memory::corrections::CORRECTION_V1
    );
    assert_eq!(l.inputs.corr.len(), 1);
    assert_eq!(l.inputs.corr[0].turn, b.u[2]);
    assert!(!l.inputs.role_fallback);
    assert_eq!(
        l.inputs.role_model_key.as_deref(),
        Some(chat_key().as_str())
    );
    assert_eq!(l.inputs.forgets_seen, None);
    assert_eq!(
        l.inputs.input_hash.as_deref(),
        Some(input_hash(&[req]).as_str())
    );
    assert_eq!(l.output.keys().items, ["京都の暮らし", "将棋"]);
    assert_eq!(l.output.units().items.len(), 2);
    assert_eq!(l.output.corrections().items.len(), 1);

    // 閉じた区切りの記録。
    let seed = Seed::open(&root, NOW, 9);
    let segs = seed.segments();
    assert_eq!(segs.len(), 1);
    assert!(segs[0].closed);
    assert_eq!(segs[0].pairs, b.u);
    assert_eq!(segs[0].id(), l.segment);
    drop(seed);

    // 同じ会話の別のデータ根でも、要求のバイト列は同じ。
    let other = dir.path().join("b");
    seed_basic(&other);
    let llm2 = chat(&[out(&basic_output())]);
    let ledgers2 = run_core(&other, NOW, &llm2).await;
    assert_eq!(llm2.requests()[0].messages, req.messages);
    assert_eq!(ledgers2[0].1.inputs.input_hash, l.inputs.input_hash);

    // もう一度起動しても、閉じた区切りは呼び直さない。
    let again = chat(&[]);
    let after = run_core(&root, NOW + MIN, &again).await;
    assert!(again.requests().is_empty());
    assert_eq!(after.len(), 1);
}

/// 区切りの記録を失っても、台帳のある区切りは区切り直さずに台帳から戻し (閉じた区切りとして書き)、呼び直さない。
///
/// verifies: 04 §2 (台帳から戻す細目)
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_segment_is_restored_from_its_ledger() {
    let dir = TempDir::new("b2-restore");
    let root = dir.path().to_path_buf();
    let b = seed_basic(&root);
    let llm = chat(&[out(&basic_output())]);
    let ledgers = run_core(&root, NOW, &llm).await;
    assert_eq!(ledgers.len(), 1);
    {
        let seed = Seed::open(&root, NOW + MIN, 5);
        let id = seed.segments()[0].id().expect("an id");
        seed.write(vec![WriteOp::Delete {
            kind: KIND_SEGMENT.into(),
            id,
        }]);
        assert!(seed.segments().is_empty());
        // 区切り直すと、この往復も前の往復と同じ区切りに入る (戻せば、新しい区切りになる)。
        let session = seed
            .get(KIND_TURN, b.u[0])
            .body_as::<evorag_memory::TurnBody>()
            .expect("a turn")
            .session;
        seed.pair(session, "猫を飼っています。", None);
    }
    let llm = chat(&[out(&output(&["猫"], &[], ""))]);
    let after = run_core(&root, NOW + 2 * MIN, &llm).await;
    assert_eq!(llm.requests().len(), 1, "only the new segment is called");
    assert!(!user(&llm.requests()[0]).contains("大阪"));
    assert_eq!(after.len(), 2);
    let mut segs = Seed::open(&root, NOW + 3 * MIN, 6).segments();
    segs.sort_by_key(|s| s.pairs[0]);
    assert_eq!(segs.len(), 2);
    assert!(segs[0].closed);
    assert_eq!(segs[0].pairs, b.u);
    assert_eq!(segs[1].pairs.len(), 1);
}

/// 開いている末尾の区切りは、前の呼び出しから往復が 2 つ以上増えたときに呼び直し、前の台帳を置き換える。閉じた
/// ときは、台帳がすべての往復を入れていれば呼ばない。
///
/// verifies: AC-FR-312-1 (閉じた区切りに往復が足されず、開いている末尾の台帳だけが置き換わる)
#[tokio::test(flavor = "multi_thread")]
async fn the_open_tail_is_called_again_after_two_more_pairs() {
    let dir = TempDir::new("b2-tail");
    let root = dir.path().to_path_buf();
    let session = {
        let seed = Seed::open(&root, NOW - MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", Some("いいですね。"));
        s
    };
    let llm = chat(&[out(&output(&["大阪"], &[unit(1, "residence", "大阪")], ""))]);
    let l1 = run_core(&root, NOW, &llm).await;
    assert_eq!(llm.requests().len(), 1);
    assert_eq!(l1.len(), 1);
    assert_eq!(l1[0].1.inputs.through.expect("through").pairs, 1);

    // 1 往復だけ増えた: 呼ばない。
    Seed::open(&root, NOW + MIN, 2).pair(session, "猫を飼っています。", Some("かわいいですね。"));
    let llm = chat(&[]);
    let l2 = run_core(&root, NOW + MIN + 10_000, &llm).await;
    assert!(llm.requests().is_empty());
    assert_eq!(l2.len(), 1);

    // 2 往復増えた: 呼び直し、前の台帳を消す。
    let u3 = Seed::open(&root, NOW + 2 * MIN, 3)
        .pair(session, "週末は将棋を指します。", Some("いいですね。"))
        .0;
    let llm = chat(&[out(&output(
        &["大阪", "将棋"],
        &[unit(1, "residence", "大阪")],
        "",
    ))]);
    let l3 = run_core(&root, NOW + 2 * MIN + 10_000, &llm).await;
    assert_eq!(llm.requests().len(), 1);
    assert_eq!(l3.len(), 1, "the tail ledger is replaced");
    let through = l3[0].1.inputs.through.expect("through");
    assert_eq!((through.turn, through.pairs), (u3, 3));
    assert_ne!(l3[0].0, l1[0].0);

    // 10 分過ぎて閉じた: 台帳はすべての往復を入れているので呼ばない。区切りの記録は閉じる。
    let llm = chat(&[]);
    let l4 = run_core(&root, NOW + 20 * MIN, &llm).await;
    assert!(llm.requests().is_empty());
    assert_eq!(l4.len(), 1);
    let segs = Seed::open(&root, NOW + 20 * MIN, 4).segments();
    assert_eq!(segs.len(), 1);
    assert!(segs[0].closed);
}

/// 条件が成り立つまで待つ (10 秒で panic する)。
async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..1_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition did not hold within 10 s");
}

/// 試験のため: 台帳が条件に合うまで待つ。
async fn wait_ledgers(
    core: &Core,
    pred: impl Fn(&[(Id, LedgerBody)]) -> bool,
) -> Vec<(Id, LedgerBody)> {
    for _ in 0..500 {
        let l = core.ledger_bodies();
        if pred(&l) {
            return l;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the ledgers did not reach the expected state: {:?}",
        core.ledger_bodies()
    );
}

/// 利用者が操作したら、流れている B2 の要求を 1 秒以内に止め、型の上で完結した要素を `partial` の台帳に残す。
/// 次の機会に呼び直して `complete` の台帳で置き換える。
///
/// verifies: AC-NFR-105-1 (区切りの呼び出しが 1 秒以内に止まる), AC-FR-313-3 (次の機会に止めずに通したときと同じ台帳)
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_call_keeps_the_closed_elements_as_a_partial_ledger() {
    let dir = TempDir::new("b2-stop");
    let root = dir.path().to_path_buf();
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", Some("いいですね。"));
    }
    // 止めずに通したときの出力 (2 つ目の単位の途中で止める)。
    let full = output(
        &["大阪"],
        &[unit(1, "residence", "大阪"), unit(1, "hobby", "大阪の散歩")],
        "",
    );
    let head = &full[..full.find(r#""a": "hobby""#).expect("the second unit")];
    let hang = format!(
        r#"{{"events": [{{"delta": {}}}, {{"hang": true}}]}}"#,
        serde_json::to_string(head).expect("a string")
    );
    let llm = chat(&[hang, out(&full)]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    for _ in 0..500 {
        if !llm.requests().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(llm.requests().len(), 1, "the call is streaming");
    // 断片を受け取るまで待つ。
    wait_for(|| llm.delivered() >= 1).await;
    core.hold(true);
    let touched = Instant::now();
    core.touch();
    while llm.dropped_early() == 0 {
        assert!(
            touched.elapsed() < Duration::from_secs(1),
            "stopped within 1 s"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let partial = wait_ledgers(&core, |l| l.len() == 1).await;
    let l = &partial[0].1;
    assert_eq!(l.status, "partial");
    assert_eq!(l.output.keys().items, ["大阪"]);
    assert_eq!(l.output.units().items.len(), 1, "only the closed unit");
    assert!(!l.output.has("corrections"), "the section did not arrive");

    // 次の機会に呼び直し、止めずに通したときと同じ `complete` の台帳で置き換える。
    core.hold(false);
    let done = wait_ledgers(&core, |l| l.len() == 1 && l[0].1.status == "complete").await;
    assert_eq!(done[0].1.output.keys().items, ["大阪"]);
    assert_eq!(done[0].1.output.units().items.len(), 2);
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 2);
    core.shutdown().await;
}

/// 型が合わなければ 1 回だけやり直し、それでも合わなければ `failed` の台帳を書く。同じ呼ぶ先では呼び直さない。
///
/// verifies: 04 §5.2.1 (型が合わないとき), 06 §6 (1 回だけやり直す)
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_output_is_retried_once_then_a_failed_ledger_is_written() {
    let dir = TempDir::new("b2-invalid");
    let root = dir.path().to_path_buf();
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
    let llm = chat(&[out("これは JSON ではない"), out(r#"{"other": []}"#)]);
    let ledgers = run_core(&root, NOW, &llm).await;
    assert_eq!(llm.requests().len(), 2, "retried once");
    assert_eq!(ledgers.len(), 1);
    let l = &ledgers[0].1;
    assert_eq!(l.status, "failed");
    let invalid = l.output.invalid();
    assert_eq!(
        invalid.get("keys").map(String::as_str),
        Some("missing"),
        "{invalid:?}"
    );
    assert_eq!(l.sentences.len(), 1, "the inputs of the sent call");

    let llm = chat(&[]);
    let again = run_core(&root, NOW + MIN, &llm).await;
    assert!(
        llm.requests().is_empty(),
        "not called again with the same model"
    );
    assert_eq!(again.len(), 1);
}

/// 出力が上限で切れたら、区切りを往復の境で半分に分けて送り直し、1 つの台帳にまとめる。
///
/// verifies: 04 §5.2.1 (出力が上限で切れたとき・分けた呼び出しのまとめ方)
#[tokio::test(flavor = "multi_thread")]
async fn a_call_cut_at_the_output_limit_is_sent_in_halves() {
    let dir = TempDir::new("b2-length");
    let root = dir.path().to_path_buf();
    let (u1, u2) = {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        let u1 = seed
            .pair(s, "私は大阪に住んでいます。", Some("いいですね。"))
            .0;
        let u2 = seed
            .pair(s, "週末は将棋を指します。", Some("いいですね。"))
            .0;
        (u1, u2)
    };
    let cut =
        r#"{"events": [{"delta": "{\"keys\": [\"大"}, {"done": {"finish_reason": "length"}}]}"#;
    let llm = chat(&[
        cut.to_owned(),
        out(&output(&["大阪"], &[unit(1, "residence", "大阪")], "")),
        out(&output(&["将棋"], &[unit(2, "hobby", "将棋")], "")),
    ]);
    let ledgers = run_core(&root, NOW, &llm).await;
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3);
    assert!(user(&reqs[0]).contains("[1]") && user(&reqs[0]).contains("[2]"));
    assert!(user(&reqs[1]).contains("[1]") && !user(&reqs[1]).contains("[2]"));
    assert!(user(&reqs[2]).contains("[2]") && !user(&reqs[2]).contains("[1]"));
    assert_eq!(ledgers.len(), 1);
    let l = &ledgers[0].1;
    assert_eq!(l.status, "complete");
    assert_eq!(l.output.keys().items, ["大阪", "将棋"]);
    let turns: Vec<Id> = l.sentences.iter().map(|s| s.turn).collect();
    assert_eq!(turns, [u1, u2]);
    assert_eq!(
        l.inputs.input_hash.as_deref(),
        Some(input_hash(&[&reqs[1], &reqs[2]]).as_str())
    );
}

/// 5xx と途中で切れたストリームは台帳を書かずに次の機会に送り直し、同じ区切りで 3 回続いたら `failed` の台帳を
/// 書く (`attempts` は送り直した回数)。
///
/// verifies: 04 §5.2.1 (失敗の扱い), 06 §7 (逐次表示の途中で切れたら書かない)
#[tokio::test(flavor = "multi_thread")]
async fn server_errors_write_nothing_until_three_in_a_row() {
    let dir = TempDir::new("b2-5xx");
    let root = dir.path().to_path_buf();
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
    let llm = chat(&[
        r#"{"http_error": {"status": 503, "body": "busy"}}"#.to_owned(),
        r#"{"events": [{"delta": "{\"keys\": [\"大阪\"], "}, {"cut": "connection reset"}]}"#
            .to_owned(),
        r#"{"http_error": {"status": 500, "body": "oops"}}"#.to_owned(),
    ]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 1);
    assert!(
        core.ledger_bodies().is_empty(),
        "nothing is written on a 5xx"
    );
    core.retry_background_now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 2);
    assert!(
        core.ledger_bodies().is_empty(),
        "nothing is written on a cut stream"
    );
    core.retry_background_now();
    let ledgers = wait_ledgers(&core, |l| !l.is_empty()).await;
    assert_eq!(llm.requests().len(), 3);
    let l = &ledgers[0].1;
    assert_eq!(l.status, "failed");
    assert_eq!(l.attempts, 2);
    assert_eq!(
        l.output.invalid().get("units").map(String::as_str),
        Some("server_error")
    );
    core.shutdown().await;
}

/// 番号を振った文はあるのに予算に入らず 1 文も送れなかった区切りは、台帳を書かずに数え、往復が変わるまで呼ばない。
///
/// verifies: 04 §5.2.1 (全部を送れなかった区切り)
#[tokio::test(flavor = "multi_thread")]
async fn a_segment_with_nothing_sendable_is_counted_without_a_ledger() {
    let dir = TempDir::new("b2-unsent");
    let root = dir.path().to_path_buf();
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        // 句点の無い長い 1 文 (記録の再生の llama-server は 1 文字を 1 トークンと数える)。
        seed.pair(s, &"大阪の天気について長く話す".repeat(400), None);
    }
    let llm = chat(&[]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.settle_background().await;
    assert!(llm.requests().is_empty());
    assert!(core.ledger_bodies().is_empty());
    assert_eq!(core.ledger_stats().nothing_sent, 1);
    core.shutdown().await;
}

/// 忘れるの `[非表示]` の範囲は印に置き換えてから渡し、指示と合わないターンは渡さない。忘れるの指示の `turns` に
/// 入る発話は訂正の候補にしない。台帳は書く前に忘れるの指示で掃除する。
///
/// verifies: 04 §5.2 (忘れると private), 04 §6 (候補を見つける時機), 04 §4 (書く前に掃除する)
#[tokio::test(flavor = "multi_thread")]
async fn forgotten_text_never_reaches_the_call() {
    let dir = TempDir::new("b2-forget");
    let root = dir.path().to_path_buf();
    let forget = {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("趣味");
        let t1 = "趣味は将棋です。";
        let a1 = "将棋はいいですね。";
        let t2 = "いや、将棋じゃなくて囲碁です。";
        let (u1, r1) = seed.pair(s, t1, Some(a1));
        let (u2, _) = seed.pair(s, t2, Some("囲碁ですね。"));
        let (u3, _) = seed.pair(s, "猫を飼っています。", None);
        let span = |turn: Id, text: &str, rev: u32| {
            let start = text.find("将棋").expect("the value");
            TurnSpan {
                turn,
                rev,
                start: start as u32,
                end: (start + "将棋".len()) as u32,
            }
        };
        let mut f = DirectiveBody::new(DirectiveOp::Forget);
        f.entity = Some("self".into());
        f.aspect = Some("hobby".into());
        f.turns = vec![u1, u2];
        f.hidden = vec![
            span(u1, t1, 0),
            span(r1.expect("r1"), a1, 0),
            span(u2, t2, 0),
            // 改訂の番号が今のターンと合わない範囲 (当て直しが済んでいない): そのターンは渡さない。
            TurnSpan {
                turn: u3,
                rev: 7,
                start: 0,
                end: 3,
            },
        ];
        seed.directive(&f);
        u3
    };
    let llm = chat(&[out(&output(&["趣味"], &[], ""))]);
    let ledgers = run_core(&root, NOW, &llm).await;
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 1);
    let text = user(&reqs[0]);
    assert!(!text.contains("将棋"), "{text}");
    assert!(text.contains("趣味は[非表示]です。"), "{text}");
    assert!(text.contains("[非表示]はいいですね。"), "{text}");
    assert!(!text.contains("訂正の候補"), "{text}");
    assert!(!text.contains("猫"), "{text}");
    assert_eq!(ledgers.len(), 1);
    let l = &ledgers[0].1;
    assert!(l.inputs.corr.is_empty());
    assert!(l.sentences.iter().all(|s| s.turn != forget));
    assert!(l.inputs.forgets_seen.is_some());
    // 対象のターンを含む区切りの `keys` は掃除で除き、`input_hash` は null にする (04 §7.2)。
    assert!(l.output.keys().items.is_empty());
    assert_eq!(l.inputs.input_hash, None);
}

/// v0.4 の形式の根 (比較の台) では、区切りも台帳も作らず、B2 を呼ばない。
///
/// verifies: 08 §2.6 (旧エンジンの根は変えない)
#[tokio::test(flavor = "multi_thread")]
async fn a_v04_bench_root_is_not_segmented() {
    let dir = TempDir::new("b2-v04");
    let root = dir.path().to_path_buf();
    let llm = chat(&[]);
    let mut cfg = config(
        &root,
        NOW,
        MemoryConfig::default(),
        evorag_core::SleepConfig::immediate(),
    );
    cfg.root_format = RootFormat::V04Bench;
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    core.command(evorag_types::api::Command::NewSession {
        title: "話".into()
    });
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("an event")
            .expect("open");
        if matches!(ev, evorag_types::api::Event::SessionOpened { .. }) {
            break;
        }
    }
    core.settle_background().await;
    assert!(core.ledger_bodies().is_empty());
    assert!(llm.requests().iter().all(|r| !is_b2(r)));
    core.shutdown().await;
}

/// 中身を差し替えられる llama-server。
struct Swappable(Mutex<Arc<ReplayClient>>);

impl Swappable {
    fn now(&self) -> Arc<ReplayClient> {
        self.0.lock().expect("lock").clone()
    }
}

impl LlmClient for Swappable {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        let c = self.now();
        Box::pin(async move { c.health().await })
    }
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.server_info().await })
    }
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.count_tokens(text).await })
    }
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        let c = self.now();
        Box::pin(async move { c.chat(req).await })
    }
}

/// memory の役割の呼ぶ先を確かめて (区切りを呼ぶ前ごと)、モデルが差し替わっていた・繋がらなかったら呼ばずに縮退し、
/// chat のモデルで呼んだ台帳に `role_fallback` と意図した `role_model_key` を書く。
async fn fallback_after(swap_to: ReplayClient, label: &str) {
    let dir = TempDir::new(label);
    let root = dir.path().to_path_buf();
    let memory_key = key_of("qwen3-4b.gguf", 9);
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        seed.probe("memory", &memory_key);
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
    let memory = Arc::new(Swappable(Mutex::new(Arc::new(server(
        "qwen3-4b.gguf",
        9,
        8192,
        true,
        &[],
    )))));
    let llm = chat(&[out(&output(&["大阪"], &[unit(1, "residence", "大阪")], ""))]);
    let sleep = evorag_core::SleepConfig {
        idle: Duration::from_secs(2),
        ..evorag_core::SleepConfig::immediate()
    };
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig {
                client: Some(memory.clone() as Arc<dyn LlmClient>),
                request_extra: Default::default(),
            },
            sleep,
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.hold(true);
    core.settle().await;
    let MemoryRoute::Call(c) = core.memory_route() else {
        panic!("the memory role is used: {:?}", core.memory_route());
    };
    assert!(!c.is_fallback());
    *memory.0.lock().expect("lock") = Arc::new(swap_to.clone());
    core.hold(false);
    let ledgers = wait_ledgers(&core, |l| !l.is_empty()).await;
    core.settle_background().await;
    core.shutdown().await;
    assert!(
        swap_to.requests().is_empty(),
        "the changed server is not called"
    );
    assert_eq!(llm.requests().len(), 1, "called with the chat model");
    let l = &ledgers[0].1;
    assert_eq!(l.model_key, chat_key());
    assert!(l.inputs.role_fallback);
    assert_eq!(
        l.inputs.role_model_key.as_deref(),
        Some(memory_key.as_str())
    );
}

/// verifies: AC-FR-515-1 (記録と違うときは chat に出て、台帳に使ったモデルが残る), 06 §2.1 (呼ぶ前に確かめる)
#[tokio::test(flavor = "multi_thread")]
async fn a_swapped_memory_model_falls_back_to_chat() {
    fallback_after(server("other.gguf", 5, 8192, true, &[]), "b2-swapped").await;
}

/// verifies: AC-FR-515-1 (繋がらないときは chat に出て、台帳に使ったモデルが残る)
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_memory_server_falls_back_to_chat() {
    fallback_after(
        server("qwen3-4b.gguf", 9, 8192, false, &[]),
        "b2-unreachable",
    )
    .await;
}

/// v0.5 の根の chat の点検は、B2 の型で小さな固定の区切りを呼ぶ。スロットの文脈の長さが B2 に足りなければ呼ばず
/// に止める。
///
/// verifies: 06 §3.3 (B2 の型の点検、文脈の長さ)
#[tokio::test(flavor = "multi_thread")]
async fn the_chat_probe_on_a_v05_root_calls_the_b2_shape() {
    let structured = r#"{"events": [{"delta": "{\"n\": 3}"}, {"done": {}}]}"#.to_owned();
    let speed = r#"{"events": [{"delta": "1"}, {"wait_ms": 20}, {"delta": " 2 3"}, {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#.to_owned();
    for (n_ctx, ok) in [(8192, true), (4096, false)] {
        let dir = TempDir::new("b2-probe");
        let root = dir.path().to_path_buf();
        // 点検の順: 型縛り → 速さ → B2 の型。
        let mut chats = vec![structured.clone(), speed.clone()];
        if ok {
            chats.push(out(&output(&["大阪"], &[unit(1, "residence", "大阪")], "")));
        }
        let llm = server("chat.gguf", 2, n_ctx, true, &chats);
        let mut cfg = config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        );
        cfg.probe_models = true;
        let (core, _rx, _) = Core::start(cfg, Arc::new(llm.clone()))
            .await
            .expect("start");
        core.settle().await;
        core.settle_probe().await;
        let p = core.probe_result("chat").expect("probed");
        assert_eq!(p.b2_ok, Some(ok), "n_ctx {n_ctx}: {p:?}");
        let b2_calls = llm.requests().iter().filter(|r| is_b2(r)).count();
        assert_eq!(b2_calls, usize::from(ok), "n_ctx {n_ctx}");
        if ok {
            assert!(matches!(core.memory_route(), MemoryRoute::Call(_)));
        } else {
            assert!(matches!(core.memory_route(), MemoryRoute::Stopped));
        }
        core.shutdown().await;
    }
}

/// 届いたイベントのうち、`pred` に合うものまで待つ。
async fn until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    pred: impl Fn(&Event) -> bool,
) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("an event in time")
            .expect("the channel is open");
        if pred(&ev) {
            return;
        }
    }
}

/// 台帳のあるセッションを消すと、区切りと台帳も同じ依頼で消え、退避の写し直しの job の ID に入る。起動し直しても
/// 残らない。
///
/// verifies: 04 §5.2.1 (セッションを消すとき。AC-FR-309-4 のうち区切りと台帳の分)
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_removes_its_segments_and_ledgers() {
    let dir = TempDir::new("b2-delete");
    let root = dir.path().to_path_buf();
    let b = seed_basic(&root);
    let llm = chat(&[out(&basic_output())]);
    let ledgers = run_core(&root, NOW, &llm).await;
    assert_eq!(ledgers.len(), 1);
    let ledger = ledgers[0].0;
    let segment = ledgers[0].1.segment.expect("a segment");
    // 削除の操作の ID (退避の写し直しの job に入るもの) に、区切りと台帳が入る。
    let session = {
        let seed = Seed::open(&root, NOW + MIN, 7);
        let session = seed
            .get(KIND_TURN, b.u[0])
            .body_as::<evorag_memory::TurnBody>()
            .expect("a turn")
            .session;
        let r = seed.store.read().expect("read");
        let s = evorag_memory::load_session(&r, session).expect("the session");
        let ops = evorag_memory::delete_session(&r, &s.envelope, &seed.writes()).expect("ops");
        let ids = evorag_memory::deleted_ids(&ops);
        assert!(ids.contains(&ledger) && ids.contains(&segment), "{ids:?}");
        session
    };
    // 画面からの削除。
    let llm = chat(&[]);
    let (core, mut rx, _) = Core::start(
        config(
            &root,
            NOW + 2 * MIN,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.command(Command::OpenSession { id: session });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle_background().await;
    assert!(core.ledger_bodies().is_empty());
    core.shutdown().await;
    assert!(Seed::open(&root, NOW + 3 * MIN, 8).segments().is_empty());
    let llm = chat(&[]);
    assert!(run_core(&root, NOW + 4 * MIN, &llm).await.is_empty());
    assert!(llm.requests().is_empty());
}

/// 古い版が消したセッションの区切りと台帳 (出典のセッションとターンが無い) は、書き出しに含めず、起動時の突き合わせで
/// 消す。
///
/// verifies: 03 §4.2 (規則 12 の突き合わせ), 03 §11 (出典の無い派生の記録を書き出さない)
#[tokio::test(flavor = "multi_thread")]
async fn sourceless_segments_and_ledgers_are_not_exported_and_are_removed_at_start() {
    let dir = TempDir::new("b2-orphans");
    let root = dir.path().join("root");
    let b = seed_basic(&root);
    let llm = chat(&[out(&basic_output())]);
    assert_eq!(run_core(&root, NOW, &llm).await.len(), 1);
    {
        // セッションとターンだけを消す (区切りと台帳を知らない古い版の削除)。
        let seed = Seed::open(&root, NOW + MIN, 7);
        let session = seed
            .get(KIND_TURN, b.u[0])
            .body_as::<evorag_memory::TurnBody>()
            .expect("a turn")
            .session;
        let mut ops = Vec::new();
        for t in b.u.iter().chain(&b.r) {
            ops.push(WriteOp::Delete {
                kind: KIND_TURN.into(),
                id: *t,
            });
        }
        ops.push(WriteOp::Delete {
            kind: KIND_SESSION.into(),
            id: session,
        });
        seed.write(ops);
        assert_eq!(seed.segments().len(), 1);
    }
    let out_dir = dir.path().join("export");
    let checks = evorag_core::export_checks::ExportFilter::new();
    let report = evorag_store::export::export(
        &root,
        &out_dir,
        &evorag_store::export::ExportOptions {
            evorag_version: "0.5.0",
            now_ms: NOW,
            exclude: &[],
            checks: &checks,
        },
    )
    .expect("export");
    assert_eq!(report.kinds.get("proposal"), None, "{:?}", report.kinds);
    assert_eq!(report.kinds.get("segment"), None, "{:?}", report.kinds);
    assert!(report.skipped_by_check >= 2);
    let llm = chat(&[]);
    assert!(run_core(&root, NOW + 2 * MIN, &llm).await.is_empty());
    assert!(Seed::open(&root, NOW + 3 * MIN, 8).segments().is_empty());
}

/// 閉じた区切り 1 つのセッションを `n` 個作る。
fn seed_sessions(root: &Path, n: usize) {
    let seed = Seed::open(root, NOW - 30 * MIN, 1);
    seed.probe("chat", &chat_key());
    for i in 0..n {
        let s = seed.session(&format!("話 {i}"));
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
}

/// 4xx は台帳を書かずに続けての失敗に数え、3 区切りで呼び出しを止める (4 つ目の区切りは呼ばない)。
///
/// verifies: 04 §5.2.1 (失敗の扱い・続けて失敗したとき)
#[tokio::test(flavor = "multi_thread")]
async fn rejected_calls_write_nothing_and_halt_after_three_segments() {
    let dir = TempDir::new("b2-4xx");
    let root = dir.path().to_path_buf();
    seed_sessions(&root, 4);
    let rejected = r#"{"http_error": {"status": 400, "body": "bad request_extra"}}"#.to_owned();
    let llm = chat(&[rejected.clone(), rejected.clone(), rejected]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 3, "the fourth segment is not called");
    assert!(
        core.ledger_bodies().is_empty(),
        "no failed ledger for a 4xx"
    );
    assert_eq!(
        core.ledger_stats().halted,
        Some(evorag_core::HaltReason::Failures)
    );
    core.retry_background_now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 3, "stays halted for the same target");
    core.shutdown().await;
}

/// 呼ぶ前に確かめたスロットの文脈の長さが B2 に足りなければ、呼ばない。
///
/// verifies: 06 §2.1 (文脈の長さ)
#[tokio::test(flavor = "multi_thread")]
async fn a_too_small_context_stops_before_the_call() {
    let dir = TempDir::new("b2-ctx");
    let root = dir.path().to_path_buf();
    let memory_key = key_of("qwen3-4b.gguf", 9);
    {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        seed.probe("memory", &memory_key);
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
    let memory = Arc::new(Swappable(Mutex::new(Arc::new(server(
        "qwen3-4b.gguf",
        9,
        8192,
        true,
        &[],
    )))));
    let llm = chat(&[]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig {
                client: Some(memory.clone() as Arc<dyn LlmClient>),
                request_extra: Default::default(),
            },
            evorag_core::SleepConfig {
                idle: Duration::from_secs(2),
                ..evorag_core::SleepConfig::immediate()
            },
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.hold(true);
    core.settle().await;
    let small = server("qwen3-4b.gguf", 9, 4096, true, &[]);
    *memory.0.lock().expect("lock") = Arc::new(small.clone());
    core.hold(false);
    core.settle_background().await;
    assert!(small.requests().is_empty());
    assert!(llm.requests().is_empty());
    assert!(core.ledger_bodies().is_empty());
    core.shutdown().await;
}

/// 同じプロセスの中で、開いている末尾の区切りが閉じる時刻 (最後のターンから 10 分) になると、手順がまた動いて
/// 区切りを閉じる (台帳がすべての往復を入れているので呼ばない)。
///
/// verifies: 04 §2 (区切りを閉じる時機)
#[tokio::test(flavor = "multi_thread")]
async fn the_tail_closes_on_the_timer_in_the_same_process() {
    let dir = TempDir::new("b2-timer");
    let root = dir.path().to_path_buf();
    {
        let seed = Seed::open(&root, NOW - MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None);
    }
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let llm = chat(&[out(&output(&["大阪"], &[unit(1, "residence", "大阪")], ""))]);
    let (core, _rx, _) = Core::start(
        config_with(
            &root,
            clock.clone(),
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 1);
    assert!(core.segment_bodies().iter().all(|s| !s.closed));
    clock.set(Timestamp::from_millis(NOW + 11 * MIN));
    wait_for(|| {
        let segs = core.segment_bodies();
        !segs.is_empty() && segs.iter().all(|s| s.closed)
    })
    .await;
    core.settle_background().await;
    assert_eq!(
        llm.requests().len(),
        1,
        "the ledger already covers the pairs"
    );
    core.shutdown().await;
}

/// `full` の出力のうち `cut` の前まで流して止まる答え。
fn hang_before(full: &str, cut: &str) -> String {
    let head = &full[..full.find(cut).expect("the cut")];
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"hang": true}}]}}"#,
        serde_json::to_string(head).expect("a string")
    )
}

/// 裏の処理が待機に戻るまで待つ。
async fn until_idle(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) {
    until(rx, |e| {
        matches!(
            e,
            Event::BackgroundState {
                state: evorag_types::api::BackgroundState::Idle
            }
        )
    })
    .await;
}

/// 流れている要求を、断片を受け取った後に止め、裏の処理が待機に戻るまで待つ。
async fn stop_after_delivery(
    core: &Core,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    llm: &ReplayClient,
    delivered: usize,
) {
    wait_for(|| llm.delivered() >= delivered).await;
    let dropped = llm.dropped_early();
    core.hold(true);
    let touched = Instant::now();
    core.touch();
    wait_for(|| llm.dropped_early() > dropped).await;
    assert!(
        touched.elapsed() < Duration::from_secs(1),
        "stopped within 1 s"
    );
    until_idle(rx).await;
}

/// `failed` を書いても使える `partial` は残し、完結した要素の少ない `partial` は書かない。
///
/// verifies: 04 §5.2.1 (`partial`・`failed` を書くとき)
#[tokio::test(flavor = "multi_thread")]
async fn a_usable_partial_survives_a_failed_or_smaller_partial() {
    let dir = TempDir::new("b2-keep-partial");
    let root = dir.path().to_path_buf();
    seed_sessions(&root, 1);
    let full = output(
        &["大阪"],
        &[unit(1, "residence", "大阪"), unit(1, "hobby", "大阪の散歩")],
        "",
    );
    let llm = chat(&[
        hang_before(&full, r#""a": "hobby""#),
        hang_before(&full, r#""units""#),
        out("JSON ではない"),
        out("JSON ではない"),
    ]);
    let (core, mut rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    stop_after_delivery(&core, &mut rx, &llm, 1).await;
    let first = wait_ledgers(&core, |l| l.len() == 1).await;
    assert_eq!(first[0].1.status, "partial");
    // 2 回目は `keys` だけで止める: 要素が少ないので書かない。
    core.hold(false);
    stop_after_delivery(&core, &mut rx, &llm, 2).await;
    let after = core.ledger_bodies();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, first[0].0, "the larger partial is kept");
    // 3 回目は型が合わず `failed`: `partial` は残る。
    core.hold(false);
    let both = wait_ledgers(&core, |l| l.len() == 2).await;
    let statuses: Vec<&str> = both.iter().map(|(_, l)| l.status.as_str()).collect();
    assert_eq!(statuses, ["partial", "failed"]);
    assert_eq!(both[0].0, first[0].0);
    core.hold(true);
    core.shutdown().await;
}

/// `/props` と `/tokenize` を止められる llama-server。止めた要求が落とされると数える。
struct Gate {
    inner: ReplayClient,
    hang_info: AtomicBool,
    hang_count: AtomicBool,
    entered: AtomicUsize,
    dropped: Arc<AtomicUsize>,
}

/// 落とされたら数える。
struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Gate {
    fn new(inner: ReplayClient) -> Self {
        Self {
            inner,
            hang_info: AtomicBool::new(false),
            hang_count: AtomicBool::new(false),
            entered: AtomicUsize::new(0),
            dropped: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn hang(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let _guard = Dropped(self.dropped.clone());
        std::future::pending::<()>().await;
    }
}

impl LlmClient for Gate {
    fn health(&self) -> BoxFuture<'_, Result<(), LlmError>> {
        self.inner.health()
    }
    fn server_info(&self) -> BoxFuture<'_, Result<ServerInfo, LlmError>> {
        Box::pin(async move {
            if self.hang_info.load(Ordering::SeqCst) {
                self.hang().await;
            }
            self.inner.server_info().await
        })
    }
    fn count_tokens<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<usize, LlmError>> {
        Box::pin(async move {
            if self.hang_count.load(Ordering::SeqCst) {
                self.hang().await;
            }
            self.inner.count_tokens(text).await
        })
    }
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<Box<dyn ChatStream>, LlmError>> {
        self.inner.chat(req)
    }
}

/// 最初のトークンの前・`/tokenize` の最中・呼ぶ先の確かめ (`/props`) の最中に操作しても、1 秒以内に止まり、
/// 台帳を書かない。
///
/// verifies: AC-NFR-105-1 (区切りの呼び出しのどの時点でも 1 秒以内に止まる)
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_at_any_point_ends_within_a_second_without_a_ledger() {
    for point in ["first token", "tokenize", "verify"] {
        let dir = TempDir::new("b2-stop-points");
        let root = dir.path().to_path_buf();
        seed_sessions(&root, 1);
        let replay = chat(&[r#"{"events": [{"hang": true}]}"#.to_owned()]);
        let gate = Arc::new(Gate::new(replay.clone()));
        let (core, mut rx, _) = Core::start(
            config(
                &root,
                NOW,
                MemoryConfig::default(),
                evorag_core::SleepConfig {
                    idle: Duration::from_secs(2),
                    ..evorag_core::SleepConfig::immediate()
                },
            ),
            gate.clone() as Arc<dyn LlmClient>,
        )
        .await
        .expect("start");
        core.hold(true);
        core.settle().await;
        match point {
            "tokenize" => gate.hang_count.store(true, Ordering::SeqCst),
            "verify" => gate.hang_info.store(true, Ordering::SeqCst),
            _ => {}
        }
        core.hold(false);
        if point == "first token" {
            wait_for(|| replay.requests().len() == 1).await;
        } else {
            wait_for(|| gate.entered.load(Ordering::SeqCst) >= 1).await;
        }
        core.hold(true);
        let touched = Instant::now();
        core.touch();
        if point == "first token" {
            wait_for(|| replay.dropped_early() == 1).await;
        } else {
            wait_for(|| gate.dropped.load(Ordering::SeqCst) >= 1).await;
        }
        assert!(
            touched.elapsed() < Duration::from_secs(1),
            "{point}: stopped within 1 s"
        );
        until_idle(&mut rx).await;
        assert!(core.ledger_bodies().is_empty(), "{point}");
        gate.hang_count.store(false, Ordering::SeqCst);
        gate.hang_info.store(false, Ordering::SeqCst);
        core.shutdown().await;
    }
}

/// 呼び出しの間に出典のターンが伏せ字で書き換わったら (改訂の番号が進んだら)、掃除で番号を合わせずに台帳を捨てる。
///
/// verifies: 04 §4 (書く前に改訂の番号を確かめ、合わなければ捨てる), 04 §7.5 (読んだ後に足された忘れる)
#[tokio::test(flavor = "multi_thread")]
async fn a_ledger_is_discarded_when_a_turn_is_redacted_during_the_call() {
    let dir = TempDir::new("b2-redacted");
    let root = dir.path().to_path_buf();
    let u = {
        let seed = Seed::open(&root, NOW - 30 * MIN, 1);
        seed.probe("chat", &chat_key());
        let s = seed.session("話");
        seed.pair(s, "私は大阪に住んでいます。", None).0
    };
    let slow = format!(
        r#"{{"events": [{{"delta": {}}}, {{"wait_ms": 2000}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(&output(&["大阪"], &[unit(1, "residence", "大阪")], ""))
            .expect("a string")
    );
    let llm = chat(&[slow]);
    let (core, _rx, _) = Core::start(
        config(
            &root,
            NOW,
            MemoryConfig::default(),
            evorag_core::SleepConfig::immediate(),
        ),
        Arc::new(llm.clone()),
    )
    .await
    .expect("start");
    core.settle().await;
    wait_for(|| llm.delivered() >= 1).await;
    assert!(core.bump_turn_revision(u).await);
    core.settle_background().await;
    assert_eq!(llm.requests().len(), 1);
    assert!(
        core.ledger_bodies().is_empty(),
        "the stale ledger is discarded"
    );
    core.shutdown().await;
}

/// B2 の型の点検が 4xx で失敗したら、通らないものとして保存する (点検し直し続けない)。
///
/// verifies: 06 §3.3 (B2 の型の点検の細目)
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_b2_probe_is_saved_as_failed() {
    let dir = TempDir::new("b2-probe-4xx");
    let root = dir.path().to_path_buf();
    let structured = r#"{"events": [{"delta": "{\"n\": 3}"}, {"done": {}}]}"#.to_owned();
    let speed = r#"{"events": [{"delta": "1"}, {"done": {"prompt_tokens": 300, "completion_tokens": 11}}]}"#.to_owned();
    let rejected = r#"{"http_error": {"status": 400, "body": "grammar"}}"#.to_owned();
    let llm = chat(&[structured, speed, rejected]);
    let mut cfg = config(
        &root,
        NOW,
        MemoryConfig::default(),
        evorag_core::SleepConfig::immediate(),
    );
    cfg.probe_models = true;
    let (core, _rx, _) = Core::start(cfg, Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    core.settle_probe().await;
    let p = core.probe_result("chat").expect("saved");
    assert_eq!(p.b2_ok, Some(false), "{p:?}");
    assert!(matches!(core.memory_route(), MemoryRoute::Stopped));
    core.shutdown().await;
}
