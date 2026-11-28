//! 区切りの鍵を語彙とベクトルの索引に入れ、想起で区切りの往復を引く (04 §5.5、04 §4 の手順 5、05 §2.4・§3.2、
//! v0.5 の根)。llama-server と埋め込みのサーバは記録の再生で置き換える (09 §3.3)。
//!
//! 会話と指示は、コアを起動する前に v0.5 の根へ直接書いておく (v0.5 の根の忘れる・private の操作は M5.4 で入る。
//! それまでの指示は取り込みで入るので、起動の後の突き合わせで当てる)。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::{Core, CoreConfig, MemoryConfig, PromptLanguage, RootFormat};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{
    AssistantTurn, DirectiveBody, DirectiveOp, EmbeddingBody, KIND_PROPOSAL, KIND_SESSION,
    KIND_TURN, LedgerBody, ModelProbeBody, TurnState, UNIT_SEGMENT_KEY, Writes,
};
use evorag_store::testutil::TempDir;
use evorag_store::{
    Envelope, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle,
};
use evorag_types::api::{Command, Event};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};

const NOW: i64 = 1_795_000_000_000;
const MIN: i64 = 60_000;

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

/// 応答のモデル (記録の再生の llama-server)。`chats` は順に使う答え。
fn chat(chats: &[String]) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "chat.gguf", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "health": true,
          "chats": [{}]
        }}"#,
        chats.join(", ")
    ))
    .expect("a cassette")
}

/// 埋め込みのサーバ (文字の bi-gram から作る決定的なベクトル)。
fn embedder() -> ReplayClient {
    ReplayClient::from_json(
        r#"{
          "props": {"build_info": "b1-test", "model_path": "/m/emb.gguf", "total_slots": 4,
                    "default_generation_settings": {"n_ctx": 8192}},
          "models": {"data": [{"id": "emb", "meta": {"n_params": 5, "size": 9, "n_vocab": 7}}]},
          "embed": {"dim": 8}
        }"#,
    )
    .expect("an embedder")
}

/// 生成を終えた答え。
fn out(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{"prompt_tokens": 120, "completion_tokens": 30}}}}]}}"#,
        serde_json::to_string(text).expect("a string")
    )
}

/// B2 の出力 (鍵だけ)。
fn keys_output(keys: &[&str]) -> String {
    out(&format!(
        r#"{{"keys": {}, "units": [], "corrections": []}}"#,
        serde_json::to_string(keys).expect("keys")
    ))
}

/// コアを起動する前に、v0.5 の根に会話・点検の結果・指示を書く口。
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

    /// 往復 1 つ (発話と完了した応答)。1 秒ずつ時計を進める。
    fn pair(&self, session: Id, user: &str, reply: &str) -> (Id, Id) {
        self.clock.advance(1_000);
        let s = self.get(KIND_SESSION, session);
        let (u, ops) = self.writes().user_turn(&s, user).expect("an utterance");
        self.write(ops);
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
        (u, r)
    }

    /// 点検の結果 (型縛りと B2 の型の点検に通った)。
    fn probe(&self) {
        let ops = self
            .writes()
            .save_probe(
                None,
                &ModelProbeBody {
                    role: "chat".into(),
                    model_key: chat_key(),
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

    /// 値の鍵 (04 §5.6) の 16 進 (忘れるの鍵が無ければ作る)。
    fn vk(&self, value: &str) -> String {
        let key = evorag_core::forget_key::ensure(&self.store, Some(&self.handle)).expect("a key");
        evorag_memory::value_key(key.key().expect("ready"), value).to_hex()
    }

    /// 指示を書く。
    fn directive(&self, body: &DirectiveBody) -> Id {
        let (id, ops) = self.writes().directive(body).expect("a directive");
        self.write(ops);
        id
    }

    /// 自分の趣味の値 `value` についての指示 (`op` は忘れる・private)。
    fn value_directive(&self, op: DirectiveOp, value: &str) -> Id {
        let mut d = DirectiveBody::new(op);
        d.entity = Some("self".into());
        d.aspect = Some("hobby".into());
        d.vks = vec![self.vk(value)];
        self.directive(&d)
    }

    /// 解く指示 (`private_off`)。
    fn private_off(&self, target: Id) {
        let mut d = DirectiveBody::new(DirectiveOp::PrivateOff);
        d.target = Some(target);
        self.directive(&d);
    }

    /// 記録した台帳 (区切りの呼び出しのもの)。
    fn ledgers(&self) -> Vec<(Envelope, LedgerBody)> {
        let r = self.store.read().expect("read");
        let mut out = Vec::new();
        r.for_each(KIND_PROPOSAL, |x| {
            if let Read::Ok(e) = x
                && let Some(evorag_memory::LedgerRead::Segment(b)) = evorag_memory::read_ledger(&e)
            {
                out.push((e, *b));
            }
            true
        })
        .expect("for each");
        out
    }

    /// 台帳の `keys` の節を書き換える (台帳の掃除 (04 §7.2、M5.4) が鍵を除いた後の形)。
    fn rewrite_keys(&self, ledger: &Envelope, keys: &[&str]) {
        let mut e = ledger.clone();
        let Some(evorag_memory::LedgerRead::Segment(mut b)) = evorag_memory::read_ledger(&e) else {
            panic!("a segment ledger");
        };
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        b.output.set_section("keys", &keys).expect("keys");
        e.set_body(&*b).expect("a body");
        self.write(vec![WriteOp::Put(e)]);
    }

    /// 区切りの鍵の埋め込みの記録。
    fn key_embeddings(&self) -> Vec<EmbeddingBody> {
        self.key_embedding_records()
            .into_iter()
            .map(|(_, b)| b)
            .collect()
    }

    /// 区切りの鍵の埋め込みの記録 (ID と本体)。
    fn key_embedding_records(&self) -> Vec<(Id, EmbeddingBody)> {
        let r = self.store.read().expect("read");
        evorag_memory::embedding_bodies(&r)
            .expect("embeddings")
            .into_iter()
            .filter(|(_, b)| b.unit == UNIT_SEGMENT_KEY)
            .collect()
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

fn config(root: &Path, at: i64, embed: Option<&ReplayClient>) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
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
        embed: embed.map(|e| Arc::new(e.clone()) as Arc<dyn LlmClient>),
        rerank: None,
        memory: MemoryConfig::default(),
        aux_request_extra: aux,
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, (at / 1_000) as u64)),
    }
}

/// 起動して裏の処理が落ち着いたときの、区切りの鍵の索引の様子。
#[derive(Debug)]
struct Seen {
    keys: Vec<(Id, Vec<String>)>,
    vectors: BTreeSet<Id>,
}

/// コアを `at` の時刻で起動し、裏の処理が落ち着くまで待つ。
async fn run_core(root: &Path, at: i64, llm: &ReplayClient, embed: Option<&ReplayClient>) -> Seen {
    let (core, _rx, _) = Core::start(config(root, at, embed), Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    core.settle_background().await;
    let seen = Seen {
        keys: core.indexed_segment_keys(),
        vectors: core.vector_segment_keys().into_iter().collect(),
    };
    core.shutdown().await;
    seen
}

/// 閉じた区切り 1 つ (2 往復)。区切りの ID を返す。
fn seed_hobby(root: &Path) -> Id {
    let seed = Seed::open(root, NOW - 30 * MIN, 1);
    seed.probe();
    let s = seed.session("趣味");
    let (u1, _) = seed.pair(s, "週末は将棋を指します。", "将棋は奥が深いですね。");
    seed.pair(s, "京都に住んでいます。", "京都はいい街ですね。");
    evorag_memory::segment_id(u1)
}

fn names(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// 区切りの鍵の埋め込みの記録の ID (使い直したら変わらず、埋め込み直したら新しい ID になる)。
fn embedding_ids(root: &Path) -> BTreeSet<Id> {
    Seed::open(root, NOW + 100 * MIN, 50)
        .key_embedding_records()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// 埋め込みの記録の値の鍵 (並べたもの)。
fn vks(embeddings: &[EmbeddingBody]) -> Vec<String> {
    let mut out: Vec<String> = embeddings.iter().filter_map(|b| b.vk.clone()).collect();
    out.sort();
    out
}

/// 台帳を書いたら、使う台帳の `keys` のうち検査を通ったものを区切りの ID の項目として索引に入れ、鍵ごとに埋め込む
/// (記録は (台帳の ID, 鍵の値の鍵) で指す)。起動し直しても入れ直さず、埋め込みの要求も出さない。
///
/// verifies: 04 §5.5 (区切りの鍵の検査・項目の ID・埋め込みの指し方), 04 §4 (手順 5)
#[tokio::test(flavor = "multi_thread")]
async fn keys_of_the_used_ledger_are_indexed_and_embedded() {
    let dir = TempDir::new("segkeys-index");
    let root = dir.path().to_path_buf();
    let seg = seed_hobby(&root);
    let llm = chat(&[keys_output(&[
        "将棋の趣味",
        "https://example.com",
        "京都の暮らし",
    ])]);
    let emb = embedder();
    let seen = run_core(&root, NOW, &llm, Some(&emb)).await;
    assert_eq!(
        seen.keys,
        [(seg, names(&["将棋の趣味", "京都の暮らし"]))],
        "the URL fails gate 6"
    );
    assert!(seen.vectors.contains(&seg), "{seen:?}");

    let seed = Seed::open(&root, NOW + MIN, 9);
    let (ledger, _) = seed.ledgers().pop().expect("a ledger");
    let embeddings = seed.key_embeddings();
    assert_eq!(embeddings.len(), 2, "{embeddings:?}");
    assert!(
        embeddings
            .iter()
            .all(|b| b.target == ledger.id && b.key.is_none())
    );
    let mut want = vec![seed.vk("将棋の趣味"), seed.vk("京都の暮らし")];
    want.sort();
    assert_eq!(vks(&embeddings), want);
    drop(seed);
    assert!(
        root.join("index")
            .join("memory")
            .join("segment-keys.json")
            .is_file(),
        "the fingerprints are kept next to the index"
    );

    // 起動し直しても、鍵の項目と行はそのまま (埋め込みの要求を出さない)。
    let before_emb2 = embedding_ids(&root);
    let emb2 = embedder();
    let again = run_core(&root, NOW + 2 * MIN, &chat(&[]), Some(&emb2)).await;
    assert_eq!(again.keys, seen.keys);
    assert_eq!(again.vectors, seen.vectors);
    assert_eq!(
        embedding_ids(&root),
        before_emb2,
        "nothing is embedded again"
    );
}

/// 鍵だけが合う問いは、区切りの鍵で当たり、鍵ではなく区切りの往復の原文が添えられる。鍵が無ければ添わない。
///
/// verifies: 04 §5.5 (区切りの鍵の指す往復は区切りのすべての往復), 05 §2.4 (ターンの値は鍵の当たりを含む),
/// OQ-36 の 7 (鍵は索引にし、添えるのは原文)
#[tokio::test(flavor = "multi_thread")]
async fn a_query_that_hits_only_a_segment_key_attaches_the_segment_turns() {
    const ASKED: &str = "モンステラを育てていて、水やりは週に一回にしています。";
    const ANSWERED: &str = "それくらいが目安ですね。";
    const QUERY: &str = "観葉植物の世話について前に話したことを教えて";
    for (case, keys) in [
        ("keys", vec!["観葉植物の世話", "植物の水分管理"]),
        ("no-keys", vec![]),
    ] {
        let dir = TempDir::new(&format!("segkeys-hit-{case}"));
        let root = dir.path().to_path_buf();
        {
            let seed = Seed::open(&root, NOW - 30 * MIN, 1);
            seed.probe();
            let s = seed.session("植物");
            seed.pair(s, ASKED, ANSWERED);
        }
        let llm = chat(&[keys_output(&keys), out("覚えています")]);
        let (core, mut rx, _) = Core::start(config(&root, NOW, None), Arc::new(llm.clone()))
            .await
            .expect("start");
        core.settle().await;
        core.settle_background().await;
        assert_eq!(
            core.indexed_segment_keys().len(),
            usize::from(!keys.is_empty())
        );
        core.hold(true);
        core.command(Command::NewSession {
            title: "今日".into(),
        });
        until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
        core.command(Command::Send { text: QUERY.into() });
        until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
        core.settle().await;
        core.shutdown().await;
        let last = llm
            .requests()
            .last()
            .map(|r| {
                r.messages
                    .iter()
                    .filter(|m| m.role != evorag_llm::Role::System)
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .expect("the answer request");
        if keys.is_empty() {
            assert!(!last.contains(ASKED), "{last}");
        } else {
            assert!(last.contains(ASKED) && last.contains(ANSWERED), "{last}");
            assert!(
                !last.contains("植物の水分管理"),
                "the key itself is not attached: {last}"
            );
        }
    }
}

/// private の指示に当たる鍵は、索引の項目だけを除き、埋め込みは残す。`/private off` で同じ鍵が戻り、記録した埋め込みを
/// 使い直す (埋め込みの要求を出さない)。
///
/// verifies: 04 §5.5 (導出の時点のふるい・埋め込みを残す), 04 §7.1 (private は消しはしない)
#[tokio::test(flavor = "multi_thread")]
async fn private_drops_the_item_keeps_the_embedding_and_private_off_restores_it() {
    let dir = TempDir::new("segkeys-private");
    let root = dir.path().to_path_buf();
    let seg = seed_hobby(&root);
    let llm = chat(&[keys_output(&["将棋", "京都の暮らし"])]);
    let emb = embedder();
    let seen = run_core(&root, NOW, &llm, Some(&emb)).await;
    assert_eq!(seen.keys, [(seg, names(&["将棋", "京都の暮らし"]))]);

    let private = {
        let seed = Seed::open(&root, NOW + MIN, 9);
        seed.value_directive(DirectiveOp::Private, "将棋")
    };
    let before_emb2 = embedding_ids(&root);
    let emb2 = embedder();
    let after = run_core(&root, NOW + 2 * MIN, &chat(&[]), Some(&emb2)).await;
    assert_eq!(after.keys, [(seg, names(&["京都の暮らし"]))]);
    assert!(after.vectors.contains(&seg));
    assert_eq!(
        embedding_ids(&root),
        before_emb2,
        "the kept key's embedding is reused"
    );
    assert_eq!(
        Seed::open(&root, NOW + 3 * MIN, 10).key_embeddings().len(),
        2,
        "the private key's embedding is kept"
    );

    Seed::open(&root, NOW + 4 * MIN, 11).private_off(private);
    let before_emb3 = embedding_ids(&root);
    let emb3 = embedder();
    let back = run_core(&root, NOW + 5 * MIN, &chat(&[]), Some(&emb3)).await;
    assert_eq!(back.keys, seen.keys);
    assert!(back.vectors.contains(&seg));
    assert_eq!(
        embedding_ids(&root),
        before_emb3,
        "both embeddings are reused"
    );
}

/// 忘れるの指示に当たる鍵は索引から除き、その埋め込みも消す (導出の時点のふるい)。台帳を掃除して鍵を除いた後は、
/// 掃除の前と後の検査の結果の差の埋め込みを消し、残った鍵の埋め込みは使い続ける。
///
/// verifies: 04 §5.5 (忘れた値・埋め込み), 04 §7.2 (掃除で除いた区切りの鍵の項目とその埋め込みを消す)
#[tokio::test(flavor = "multi_thread")]
async fn forgotten_and_scrubbed_keys_lose_their_items_and_embeddings() {
    let dir = TempDir::new("segkeys-forget");
    let root = dir.path().to_path_buf();
    let seg = seed_hobby(&root);
    let llm = chat(&[keys_output(&["将棋", "京都の暮らし", "週末の過ごし方"])]);
    let emb = embedder();
    run_core(&root, NOW, &llm, Some(&emb)).await;
    let kept = {
        let seed = Seed::open(&root, NOW + MIN, 9);
        assert_eq!(seed.key_embeddings().len(), 3);
        seed.value_directive(DirectiveOp::Forget, "将棋");
        let mut v = vec![seed.vk("京都の暮らし"), seed.vk("週末の過ごし方")];
        v.sort();
        v
    };
    let before_emb2 = embedding_ids(&root);
    let emb2 = embedder();
    let after = run_core(&root, NOW + 2 * MIN, &chat(&[]), Some(&emb2)).await;
    assert_eq!(
        after.keys,
        [(seg, names(&["京都の暮らし", "週末の過ごし方"]))]
    );
    assert!(after.vectors.contains(&seg));
    assert!(
        embedding_ids(&root).is_subset(&before_emb2),
        "nothing is embedded again"
    );
    assert_eq!(
        vks(&Seed::open(&root, NOW + 3 * MIN, 10).key_embeddings()),
        kept
    );

    // 掃除で `keys` から鍵を除いた台帳 (M5.4 の忘れるの手順が書く形)。
    {
        let seed = Seed::open(&root, NOW + 4 * MIN, 11);
        let (ledger, _) = seed.ledgers().pop().expect("a ledger");
        seed.rewrite_keys(&ledger, &["京都の暮らし"]);
    }
    let before_emb3 = embedding_ids(&root);
    let emb3 = embedder();
    let scrubbed = run_core(&root, NOW + 5 * MIN, &chat(&[]), Some(&emb3)).await;
    assert_eq!(scrubbed.keys, [(seg, names(&["京都の暮らし"]))]);
    assert!(scrubbed.vectors.contains(&seg));
    assert!(
        embedding_ids(&root).is_subset(&before_emb3),
        "nothing is embedded again"
    );
    let seed = Seed::open(&root, NOW + 6 * MIN, 12);
    assert_eq!(vks(&seed.key_embeddings()), [seed.vk("京都の暮らし")]);
}

/// セッションを消すと、その区切りの鍵の項目 (語彙とベクトル) と、台帳の区切りの鍵の埋め込みが消える。
///
/// verifies: 04 §7.1 (セッションの削除: 区切りの鍵と埋め込みを消す), AC-FR-309-1 (消した会話が索引に残らない)
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_removes_its_segment_keys_and_embeddings() {
    let dir = TempDir::new("segkeys-delete");
    let root = dir.path().to_path_buf();
    let seg = seed_hobby(&root);
    let llm = chat(&[keys_output(&["将棋", "京都の暮らし"])]);
    let emb = embedder();
    let (core, mut rx, _) = Core::start(config(&root, NOW, Some(&emb)), Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    core.settle_background().await;
    assert_eq!(core.indexed_segment_keys().len(), 1);
    assert!(core.vector_segment_keys().contains(&seg));
    let session = core.segment_bodies().pop().expect("a segment").session;
    core.command(Command::OpenSession { id: session });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    // 知らせの時点で、索引から除いてある (04 §7.7 の 2)。
    assert!(core.indexed_segment_keys().is_empty());
    assert!(core.vector_segment_keys().is_empty());
    core.settle_background().await;
    core.shutdown().await;
    assert!(Seed::open(&root, NOW + MIN, 9).key_embeddings().is_empty());
    let emb2 = embedder();
    let after = run_core(&root, NOW + 2 * MIN, &chat(&[]), Some(&emb2)).await;
    assert!(
        after.keys.is_empty() && after.vectors.is_empty(),
        "{after:?}"
    );
}

/// 記録した指紋が今の作り方と合わない (区切りの鍵の作り方の版が変わった) か、指紋のファイルが無いときは、区切りの鍵の
/// 項目を作り直す。埋め込みは記録から使い直す。
///
/// verifies: 04 §5.5 (版: 変われば区切りの鍵の項目をすべて作り直す)
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_fingerprint_rebuilds_the_items() {
    let dir = TempDir::new("segkeys-version");
    let root = dir.path().to_path_buf();
    let seg = seed_hobby(&root);
    let emb = embedder();
    let seen = run_core(
        &root,
        NOW,
        &chat(&[keys_output(&["将棋", "京都の暮らし"])]),
        Some(&emb),
    )
    .await;
    let file = root.join("index").join("memory").join("segment-keys.json");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("the file")).expect("json");
    let hash = original["segments"][seg.to_string()].clone();
    assert!(hash.is_string(), "{original}");
    // 前の版の作り方で入れた項目 (指紋が違う)。
    let mut old = original.clone();
    old["segment_keys"] = serde_json::json!(0);
    old["segments"][seg.to_string()] = serde_json::json!("00");
    std::fs::write(&file, serde_json::to_vec(&old).expect("json")).expect("write");
    let before_emb2 = embedding_ids(&root);
    let emb2 = embedder();
    let again = run_core(&root, NOW + MIN, &chat(&[]), Some(&emb2)).await;
    assert_eq!(again.keys, seen.keys);
    assert_eq!(again.vectors, seen.vectors, "the rows are filled again");
    assert_eq!(
        embedding_ids(&root),
        before_emb2,
        "the recorded embeddings are reused"
    );
    let now: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("the file")).expect("json");
    assert_eq!(now["segments"][seg.to_string()], hash);
    assert_eq!(
        now["segment_keys"],
        serde_json::json!(evorag_memory::segment_keys::SEGMENT_KEYS_VERSION)
    );
    // 指紋のファイルが無くても作り直す。
    std::fs::remove_file(&file).expect("remove");
    let emb3 = embedder();
    let rebuilt = run_core(&root, NOW + 2 * MIN, &chat(&[]), Some(&emb3)).await;
    assert_eq!(rebuilt.keys, seen.keys);
    assert_eq!(rebuilt.vectors, seen.vectors);
    assert!(file.is_file());
}

/// v0.4 の形式の根 (比較の台) では、区切りの鍵を作らず、指紋のファイルも書かない。
///
/// verifies: 08 §2.6 (旧エンジンの根は変えない)
#[tokio::test(flavor = "multi_thread")]
async fn a_v04_bench_root_has_no_segment_keys() {
    let dir = TempDir::new("segkeys-v04");
    let root = dir.path().to_path_buf();
    let mut cfg = config(&root, NOW, None);
    cfg.root_format = RootFormat::V04Bench;
    let llm = chat(&[out("はい")]);
    let (core, mut rx, _) = Core::start(cfg, Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    core.hold(true);
    core.command(Command::NewSession {
        title: "話".into()
    });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::Send {
        text: "週末は将棋を指します。".into(),
    });
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.hold(false);
    core.settle_background().await;
    assert!(core.indexed_segment_keys().is_empty());
    core.shutdown().await;
    assert!(
        !root
            .join("index")
            .join("memory")
            .join("segment-keys.json")
            .exists()
    );
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
