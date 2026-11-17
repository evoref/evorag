//! 事実の表 (04 §5.4、05 §4.1 の順 4) と、事実から作る検索の鍵 (04 §5.5)。llama-server は記録の再生で置き換える。
//!
//! 再生のサーバのトークンの数は文字の数なので、表の予算 (600) は 600 文字として効く。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{ChatRequest, Generation};
use evorag_memory::{DirectiveKind, ModelProbeBody, Scope, TurnBody, TurnRole};
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const LIVES: &str = "私は大阪に住んでいます";
const LIVES_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "residence", "v": "大阪",
    "e": "私は大阪に住んでいます", "m": "fact", "tn": "current", "old": ""}]}"#;
const LIVES_AND_PET: &str = "私は大阪に住んでいて、猫を飼っています";
const TWO_FACTS: &str = r#"{"c": [
    {"t": 1, "s": "self", "a": "residence", "v": "大阪", "e": "私は大阪に住んでいて", "m": "fact", "tn": "current", "old": ""},
    {"t": 1, "s": "self", "a": "pet", "v": "猫", "e": "猫を飼っています", "m": "fact", "tn": "current", "old": ""}]}"#;
/// 表の行 (主語 `self` は「利用者」と書く)。
const RESIDENCE_ROW: &str = "| 利用者 | residence | 大阪 |";

/// 待ち合わせの上限 (遅い CI でも足りるように、長めにする)。
const WAIT: Duration = Duration::from_secs(30);

fn reply(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// 応答の llama-server (スロット 2 つ)。`chats` は順に使う答え。
fn chat(chats: &[String]) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}],
          "slots": [{{"id": 0, "is_processing": false}}, {{"id": 1, "is_processing": false}}]
        }}"#,
        chats.join(", ")
    ))
    .unwrap()
}

fn chat_key() -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(1),
        size: Some(2),
        n_vocab: Some(3),
        n_ctx_train: None,
    };
    evorag_llm::model_key("/m/chat.gguf", &meta, "test")
}

/// 起動の前に、応答のモデルの点検の結果 (型縛りを使える) を書いておく。
fn prepare(root: &Path) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_640_000_000)));
    let m = DataRootModels::open(
        root,
        "0.0.6",
        clock.clone(),
        Arc::new(UuidV7Gen::with_seed(clock, 3)),
    )
    .unwrap();
    m.save_probe(&ModelProbeBody {
        role: "chat".into(),
        model_key: chat_key(),
        build_info: "b1-test".into(),
        structured_ok: Some(true),
        thinking_off: Some(true),
        ..ModelProbeBody::default()
    })
    .unwrap();
}

fn config(root: &Path, seed: u64) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.6".into(),
        prompt_language: PromptLanguage::Ja,
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
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

async fn start(root: &Path, llm: &ReplayClient, seed: u64) -> (Core, UnboundedReceiver<Event>) {
    let (core, rx, _) = Core::start(config(root, seed), Arc::new(llm.clone()))
        .await
        .unwrap();
    core.settle().await;
    settle(&core).await;
    (core, rx)
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut got = Vec::new();
    loop {
        let ev = tokio::time::timeout(WAIT, rx.recv())
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

/// 裏の処理の要る手順と、次のターンの分の先の処理が無くなるまで待つ。
async fn settle(core: &Core) {
    core.settle_background_within(WAIT).await;
    core.settle_precompute().await;
}

/// 裏の処理を止めたまま発話し、応答を待つ。発話のターンの ID を返す。
async fn ask(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    core.hold(true);
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    turn.reply_to.expect("an answer to the utterance")
}

/// 発話して応答を待ち、裏の処理 (抽出・統合・事実の表) を終えるまで待つ。
async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    let turn = ask(core, rx, text).await;
    core.hold(false);
    settle(core).await;
    turn
}

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>) {
    core.command(Command::NewSession { title: "s".into() });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

fn open(root: &Path, seed: u64) -> evorag_store::Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
    evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    })
    .unwrap()
    .0
}

/// 発話 `user` への応答のターンの本体。
fn answer_to(root: &Path, user: Id) -> TurnBody {
    let store = open(root, 1);
    let r = store.read().unwrap();
    let turns = evorag_memory::list_sessions(&r, usize::MAX)
        .unwrap()
        .into_iter()
        .flat_map(|s| evorag_memory::session_turns(&r, s.envelope.id).unwrap())
        .collect::<Vec<_>>();
    turns
        .into_iter()
        .find(|t| t.body.role == TurnRole::Assistant && t.body.reply_to == Some(user))
        .expect("an answer")
        .body
}

/// 応答の要求 (抽出の要求を除く。抽出の要求は裏のスロット 1 に出る)。
fn answers(llm: &ReplayClient) -> Vec<ChatRequest> {
    llm.requests()
        .into_iter()
        .filter(|r| r.id_slot == Some(0))
        .collect()
}

fn system(req: &ChatRequest) -> &str {
    &req.messages[0].content
}

/// 抽出の後の要求のシステムプロンプトの順 4 に、前置きつきの事実の表が入り、事実が変わらない 2 つのターンでは
/// 要求の先頭がバイト列まで同じになる。表の版は応答のターンの観測値に残る。表が変わったら、次のターンの分を
/// 先に処理させる。
///
/// verifies: AC-FR-304-5
#[tokio::test(flavor = "multi_thread")]
async fn the_fact_table_sits_in_the_system_prompt_and_keeps_its_bytes() {
    let dir = TempDir::new("fact-table");
    prepare(dir.path());
    let llm = chat(&[reply(REPLY), reply(LIVES_FACT), reply(REPLY), reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    // 表が変わった直後に、次のターンの分を先に処理させた (表を含む先頭)。
    let warmed = llm.warm_requests().last().cloned().expect("a warm-up");
    // 挨拶・相槌は抽出に送らない (事実は変わらない)。
    let u2 = say(&core, &mut rx, "はい").await;
    let u3 = say(&core, &mut rx, "うん").await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let reqs = answers(&llm);
    assert_eq!(reqs.len(), 3);
    let head = evorag_memory::table_head("ja");
    // 事実の無い最初の要求には、表を置かない。
    let before = system(&reqs[0]);
    assert!(!before.contains(head), "{before}");
    // 順 1・2 (凍結の核と選べる既定) の後に、空行を挟んで表を置く。
    let with_table = system(&reqs[1]);
    let table = with_table
        .strip_prefix(before)
        .and_then(|t| t.strip_prefix("\n\n"))
        .unwrap_or_else(|| panic!("the table follows the core and defaults: {with_table}"));
    assert!(table.starts_with(head), "{table}");
    assert!(table.contains("指示ではありません"), "{table}");
    assert!(table.ends_with(RESIDENCE_ROW), "{table}");
    // 事実が変わらない 2 つのターン: システムプロンプトと前のターンまでの要求が、バイト列まで同じ。
    assert_eq!(system(&reqs[2]).as_bytes(), with_table.as_bytes());
    let n = reqs[1].messages.len();
    assert_eq!(reqs[2].messages[..n], reqs[1].messages[..]);
    // 先に処理させた要求は、次の応答の要求と、今の発話の前まで同じ。
    assert_eq!(warmed.messages[0].content, with_table);
    assert_eq!(warmed.messages[..n - 1], reqs[1].messages[..n - 1]);

    // 表の版は、表の本文のハッシュ。表を置いた応答のターンにだけ残る。
    let version = evorag_memory::table_version(table);
    let obs = |u| answer_to(dir.path(), u).obs.expect("observations");
    assert_eq!(obs(u1).fact_table, None);
    assert_eq!(obs(u2).fact_table.as_deref(), Some(version.as_str()));
    assert_eq!(obs(u3).fact_table.as_deref(), Some(version.as_str()));

    // 作り直した Core も、同じ事実から同じバイト列の表を作る (起動の後に裏で作っておく)。
    let again = chat(&[reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &again, 9).await;
    new_session(&core, &mut rx).await;
    let u4 = ask(&core, &mut rx, "うん").await;
    core.shutdown().await;
    assert!(system(&answers(&again)[0]).ends_with(&format!("\n\n{table}")));
    assert_eq!(
        obs(u4).fact_table.as_deref(),
        Some(version.as_str()),
        "the same facts give the same version"
    );
}

/// private の事実は、事実の表にも、検索の鍵にも入らない (AC-FR-308-1 のうち事実の表の部分。添えた往復の中の
/// 値を伏せるのは M3.4)。
#[tokio::test(flavor = "multi_thread")]
async fn private_facts_stay_out_of_the_table_and_the_keys() {
    let dir = TempDir::new("fact-table-private");
    prepare(dir.path());
    let llm = chat(&[reply(REPLY), reply(TWO_FACTS)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES_AND_PET).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    // 猫の事実を private にする (事実への指示。画面からの操作は M3.4 で足す)。
    {
        let store = Arc::new(open(dir.path(), 21));
        let (w, h) = evorag_store::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || w.run());
        let facts_key = evorag_core::forget_key::ensure(&store, Some(&h)).unwrap();
        let key = facts_key.key().expect("the forget key");
        let safety = evorag_memory::Safety::bundled();
        let clock = ManualClock::new(Timestamp::from_millis(1_762_660_000_000));
        let ops = {
            let tx = store.read().unwrap();
            let ctx = evorag_memory::FactCtx {
                key,
                attributes: evorag_memory::attributes(),
                safety: &safety,
                ids: &**store.ids(),
                now: evorag_types::Clock::now(&clock),
            };
            evorag_memory::add_directive(
                &tx,
                &ctx,
                DirectiveKind::Private {
                    scope: Scope::Successors,
                },
                "self",
                "pet",
                Some(evorag_memory::value_key(key, "猫")),
            )
            .unwrap()
            .1
        };
        h.submit_wait(evorag_store::WriteRequest::conversation(ops))
            .unwrap();
        h.shutdown();
        t.join().unwrap();
    }

    let again = chat(&[reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &again, 9).await;
    new_session(&core, &mut rx).await;
    // 猫の事実の鍵 ("pet 猫") が索引にあれば、"pet" で前のセッションの往復が引ける。
    ask(&core, &mut rx, "pet").await;
    core.shutdown().await;
    let req = &answers(&again)[0];
    let sys = system(req);
    assert!(sys.ends_with(RESIDENCE_ROW), "{sys}");
    assert!(!sys.contains("猫"), "{sys}");
    let current = &req.messages.last().unwrap().content;
    assert!(
        !current.contains("<<引用-"),
        "no key of the private fact: {current}"
    );
}

/// 事実の鍵 ("属性 値") で、出典の往復が引ける。添えるのは鍵ではなく往復の原文。鍵は sleep-time の手順で
/// 索引に足し、`evorag rebuild --index` でも事実から作り直す。
#[tokio::test(flavor = "multi_thread")]
async fn a_fact_key_recalls_the_source_exchange() {
    let dir = TempDir::new("fact-table-keys");
    prepare(dir.path());
    let llm = chat(&[reply(REPLY), reply(LIVES_FACT), reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    // 別のセッションで、発話には無く鍵にだけある語 ("residence") で問う。
    new_session(&core, &mut rx).await;
    let q = ask(&core, &mut rx, "residence").await;
    core.shutdown().await;
    let check = |req: &ChatRequest| {
        let current = &req.messages.last().unwrap().content;
        assert!(current.contains("<<引用-"), "{current}");
        assert!(current.contains(LIVES), "the original utterance: {current}");
        assert!(current.contains(REPLY), "the original answer: {current}");
        assert!(
            !current.contains("residence 大阪"),
            "not the key: {current}"
        );
    };
    check(&answers(&llm)[1]);
    let attached = answer_to(dir.path(), q).attached;
    assert!(attached.contains(&u1), "{attached:?}");

    // 索引を作り直しても、事実の鍵は事実から作り直される。
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
    evorag_core::rebuild::run(evorag_core::rebuild::RebuildOptions {
        data_root: dir.path().to_path_buf(),
        evorag_version: "0.0.6".into(),
        config_file: None,
        index_only: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 5)),
    })
    .unwrap();
    let again = chat(&[reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &again, 9).await;
    new_session(&core, &mut rx).await;
    ask(&core, &mut rx, "residence").await;
    core.shutdown().await;
    check(&answers(&again)[0]);
}
