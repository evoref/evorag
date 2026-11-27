//! 事実の一覧と事実への操作 (04 §7.1・§7.4〜§7.6、07 §3 の `/memory`・`/private`)。llama-server は記録の再生で
//! 置き換える。試験の値は架空のもの。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{ChatRequest, Generation};
use evorag_memory::ModelProbeBody;
use evorag_store::testutil::TempDir;
use evorag_types::api::{
    Command, Event, FactGroupKey, FactKey, FactList, FactOp, FactRow, FactRowRef, FactStatus,
    FactSubject, PrivateCause, PrivateScope,
};
use evorag_types::{Id, ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const LIVES_AND_PET: &str = "私は大阪に住んでいて、猫を飼っています";
const TWO_FACTS: &str = r#"{"c": [
    {"t": 1, "s": "self", "a": "residence", "v": "大阪", "e": "私は大阪に住んでいて", "m": "fact", "tn": "current", "old": ""},
    {"t": 1, "s": "self", "a": "pet", "v": "猫", "e": "猫を飼っています", "m": "fact", "tn": "current", "old": ""}]}"#;
const LIVES: &str = "私は大阪に住んでいます";
const LIVES_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "residence", "v": "大阪",
    "e": "私は大阪に住んでいます", "m": "fact", "tn": "current", "old": ""}]}"#;
/// 主語の語も変化の語も無いので、今の値 (大阪) と矛盾する値になる (04 §5.3)。
const TOKYO: &str = "東京に住んでるよ";
const TOKYO_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "residence", "v": "東京",
    "e": "東京に住んでるよ", "m": "fact", "tn": "current", "old": ""}]}"#;
const SHRIMP: &str = "実は、えびは食べられないんです";
const SHRIMP_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "dietary", "v": "えび",
    "e": "えびは食べられない", "m": "fact", "tn": "current", "old": ""}]}"#;
const CRAB: &str = "実は、かにも食べられないんです";
const CRAB_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "dietary", "v": "かに",
    "e": "かにも食べられない", "m": "fact", "tn": "current", "old": ""}]}"#;

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
        evorag_core::RootFormat::V04Bench,
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
        root_format: evorag_core::RootFormat::V04Bench,
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: "0.0.6".into(),
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
        memory: Default::default(),
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

/// 命令を送り、事実の一覧・操作の結果のイベントを待つ。
async fn fact_event(core: &Core, rx: &mut UnboundedReceiver<Event>, cmd: Command) -> Event {
    core.command(cmd);
    let ev = until(rx, |e| {
        matches!(
            e,
            Event::FactsInfo { .. }
                | Event::FactsUpdated { .. }
                | Event::FactOpRejected { .. }
                | Event::FactHistoryInfo { .. }
                | Event::ForgottenInfo { .. }
        )
    })
    .await;
    ev.into_iter().last().unwrap()
}

async fn facts(core: &Core, rx: &mut UnboundedReceiver<Event>) -> FactList {
    match fact_event(core, rx, Command::ShowFacts).await {
        Event::FactsInfo { list } => list,
        other => panic!("{other:?}"),
    }
}

/// 一覧の値の行 (属性, 値, 鍵, private の理由)。
fn value_rows(list: &FactList) -> Vec<(String, String, FactKey, Vec<PrivateCause>)> {
    list.groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .filter_map(|r| match r {
            FactRow::Value {
                key,
                value,
                private,
            } => Some((
                key.attribute.clone(),
                value.clone(),
                key.clone(),
                private
                    .as_ref()
                    .map(|p| p.causes.clone())
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .collect()
}

fn key_of(list: &FactList, value: &str) -> FactKey {
    value_rows(list)
        .into_iter()
        .find(|r| r.1 == value)
        .unwrap_or_else(|| panic!("{value} in {list:?}"))
        .2
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

fn current(req: &ChatRequest) -> String {
    user_side(&req.messages)
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

/// `/private` で、値は事実の表から外れ、検索の鍵は知らせの前に索引から除かれ、想起で添える往復の中の値は
/// `[非表示]` になる。`/private off` で、事実の表と検索の鍵が private にする前と同じに戻る。一覧は、セッションを
/// 開いていなくても出る。
///
/// verifies: AC-FR-308-1, AC-FR-308-2, AC-FR-310-2
#[tokio::test(flavor = "multi_thread")]
async fn private_hides_the_value_everywhere_and_private_off_restores_it() {
    let dir = TempDir::new("fact-ops-private");
    prepare(dir.path());
    let llm = chat(&[
        reply(REPLY),
        reply(TWO_FACTS),
        reply(REPLY),
        reply(REPLY),
        reply(REPLY),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES_AND_PET).await;
    let before_keys = core.indexed_fact_keys();
    assert_eq!(before_keys, 2);
    // 対照: private にする前の表。
    new_session(&core, &mut rx).await;
    ask(&core, &mut rx, "うん").await;

    let list = facts(&core, &mut rx).await;
    let cat = key_of(&list, "猫");
    assert_eq!(cat.status, FactStatus::Live);
    let ev = fact_event(
        &core,
        &mut rx,
        Command::SetFactPrivate {
            target: cat.clone(),
            scope: PrivateScope::Successors,
        },
    )
    .await;
    // 知らせの前に、private にした事実の鍵は索引から除かれている (04 §5.5)。
    assert_eq!(core.indexed_fact_keys(), 1);
    let Event::FactsUpdated {
        op: FactOp::Private,
        list,
        slow_next_turn: true,
    } = ev
    else {
        panic!("{ev:?}")
    };
    let rows = value_rows(&list);
    let cat_row = rows.iter().find(|r| r.1 == "猫").expect("the row stays");
    assert_eq!(
        cat_row.3,
        [PrivateCause::Chosen {
            scope: PrivateScope::Successors
        }]
    );
    // もう private の行は、もう一度 private にできない。
    let ev = fact_event(
        &core,
        &mut rx,
        Command::SetFactPrivate {
            target: cat.clone(),
            scope: PrivateScope::ValueOnly,
        },
    )
    .await;
    assert!(
        matches!(&ev, Event::FactOpRejected { error, list: None } if error.key == MessageKey::FactsNotAllowed),
        "{ev:?}"
    );

    // 次のターン: 表を作り直して猫が消え、住まいの鍵で引いた前の往復の中の猫は [非表示] になる。
    ask(&core, &mut rx, "residence").await;
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnsetFactPrivate {
            target: FactRowRef::Value(cat.clone()),
        },
    )
    .await;
    assert!(
        matches!(
            ev,
            Event::FactsUpdated {
                op: FactOp::PrivateOff,
                ..
            }
        ),
        "{ev:?}"
    );
    ask(&core, &mut rx, "うん").await;
    core.hold(false);
    settle(&core).await;
    let after_keys = core.indexed_fact_keys();
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let reqs = answers(&llm);
    let (before, hidden, quoted, restored) = (&reqs[1], &reqs[2], &reqs[2], &reqs[3]);
    assert!(
        system(before).contains("| 利用者 | pet | 猫 |"),
        "{}",
        system(before)
    );
    assert!(!system(hidden).contains("猫"), "{}", system(hidden));
    let q = current(quoted);
    assert!(q.contains("<<引用-"), "recalled by the residence key: {q}");
    assert!(q.contains("[非表示]を飼っています"), "{q}");
    assert!(!q.contains("猫"), "{q}");
    // private を解くと、表と鍵が private にする前と同じに戻る。
    assert_eq!(system(restored), system(before));
    assert_eq!(after_keys, before_keys);

    // 一覧はセッションを開いていなくても出る。
    let again = chat(&[]);
    let (core, mut rx) = start(dir.path(), &again, 9).await;
    let list = facts(&core, &mut rx).await;
    core.shutdown().await;
    let mut values: Vec<String> = value_rows(&list).into_iter().map(|r| r.1).collect();
    values.sort();
    assert_eq!(values, ["大阪", "猫"]);
    assert!(list.note.is_none());
}

/// 指した行が変わっていたら実行せず、同じ読み取りで作った新しい一覧を返す。前提が合わない操作は別の理由で断る
/// (04 §7.1)。
///
/// verifies: AC-FR-310-2
#[tokio::test(flavor = "multi_thread")]
async fn an_outdated_row_or_a_wrong_precondition_is_rejected() {
    let dir = TempDir::new("fact-ops-outdated");
    prepare(dir.path());
    let llm = chat(&[reply(REPLY), reply(LIVES_FACT)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    let list = facts(&core, &mut rx).await;
    let osaka = key_of(&list, "大阪");
    // 表示したときと状態の種類が違う (矛盾として表示した、と偽る) 行は、変わった行として扱う。
    let mut stale = osaka.clone();
    stale.status = FactStatus::Contested;
    let ev = fact_event(&core, &mut rx, Command::AcceptFact { target: stale }).await;
    let Event::FactOpRejected {
        error,
        list: Some(fresh),
    } = ev
    else {
        panic!("{ev:?}")
    };
    assert_eq!(error.key, MessageKey::FactsOutdated);
    assert_eq!(fresh, list, "the fresh list from the same read");
    // 事実の ID は照合に使わない。
    let mut other_id = osaka.clone();
    other_id.fact = Some(Id::from_bytes([7; 16]));
    let rejected = |ev: &Event, key: MessageKey| matches!(ev, Event::FactOpRejected { error, list: None } if error.key == key);
    // 矛盾でない行は採れない。private でない行は解けない。履歴でない行は戻せない。
    let ev = fact_event(&core, &mut rx, Command::AcceptFact { target: other_id }).await;
    assert!(rejected(&ev, MessageKey::FactsNotAllowed), "{ev:?}");
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnsetFactPrivate {
            target: FactRowRef::Value(osaka.clone()),
        },
    )
    .await;
    assert!(rejected(&ev, MessageKey::FactsNotAllowed), "{ev:?}");
    // 指示だけが残る属性ではない行を、そうだとして解こうとした。
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnsetFactPrivate {
            target: FactRowRef::RuleOnly(FactGroupKey {
                subject: FactSubject::User,
                attribute: "residence".into(),
            }),
        },
    )
    .await;
    assert!(
        matches!(&ev, Event::FactOpRejected { error, list: Some(_) } if error.key == MessageKey::FactsOutdated),
        "{ev:?}"
    );
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
}

/// 忘れるの指示 (印だけ) を書く (忘れるの操作は `/forget` で足す。ここでは印の効き目だけを試す)。
fn write_forget_mark(root: &Path, attribute: &str, value: &str) {
    let store = Arc::new(open(root, 21));
    let (w, h) = evorag_store::Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || w.run());
    let facts_key = evorag_core::forget_key::ensure(&store, Some(&h)).unwrap();
    let key = facts_key.key().expect("the forget key");
    let vk = evorag_memory::value_key(key, value);
    let mark = evorag_memory::block_mark(key, "self", attribute, &vk);
    let body = evorag_memory::FactDirectiveBody {
        op: "forget".into(),
        subject: "self".into(),
        attribute: attribute.into(),
        vk: None,
        norm: Some(evorag_memory::NORM_V1),
        fact: None,
        target: None,
        scope: None,
        marks: vec![mark.to_hex()],
        vks: vec![vk.to_hex()],
        turns: Vec::new(),
        redactions: Vec::new(),
        hidden: Vec::new(),
        history: false,
        redact: false,
        origin: evorag_memory::ORIGIN_USER.into(),
    };
    let id = store.ids().next_id();
    let mut env = evorag_store::Envelope::new(
        id,
        evorag_memory::KIND_FACT_DIRECTIVE,
        1,
        Timestamp::from_millis(1_762_660_000_000),
        "",
    );
    env.set_body(&body).unwrap();
    h.submit_wait(evorag_store::WriteRequest::conversation(vec![
        evorag_store::WriteOp::Put(env),
    ]))
    .unwrap();
    h.shutdown();
    t.join().unwrap();
}

/// `/memory accept` で矛盾の値を今の値にし、`/memory history`・`/memory revert` で履歴の値を戻す。忘れた値は、
/// 起動時の当て直しで履歴から取り消し (04 §7.2)、`/memory forgotten`・`/memory unblock` で印を解ける
/// (04 §7.4〜§7.6)。採る・戻すの後は、次のターンの前に事実の表を作り直す。
#[tokio::test(flavor = "multi_thread")]
async fn accept_revert_and_unblocking_a_forgotten_value() {
    let dir = TempDir::new("fact-ops-accept");
    prepare(dir.path());
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(TOKYO_FACT),
        reply(REPLY),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    say(&core, &mut rx, TOKYO).await;
    let list = facts(&core, &mut rx).await;
    let tokyo = key_of(&list, "東京");
    assert_eq!(tokyo.status, FactStatus::Contested);
    let ev = fact_event(&core, &mut rx, Command::AcceptFact { target: tokyo }).await;
    let Event::FactsUpdated {
        op: FactOp::Accept,
        list,
        ..
    } = ev
    else {
        panic!("{ev:?}")
    };
    let rows = value_rows(&list);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2.status),
        ("東京", FactStatus::Live)
    );
    assert_eq!(list.groups[0].history, 1);
    // 次のターンの表は、採った値になる。
    ask(&core, &mut rx, "うん").await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    assert!(
        system(&answers(&llm)[2]).ends_with("| 利用者 | residence | 東京 |"),
        "{}",
        system(&answers(&llm)[2])
    );

    // 大阪を忘れた値にする (印だけ)。起動時の当て直しが、印に当たる履歴の値を取り消す (04 §7.2 の当て直しの
    // 一覧の 2)。
    write_forget_mark(dir.path(), "residence", "大阪");
    let again = chat(&[]);
    let (core, mut rx) = start(dir.path(), &again, 9).await;
    let group = FactGroupKey {
        subject: FactSubject::User,
        attribute: "residence".into(),
    };
    let ev = fact_event(
        &core,
        &mut rx,
        Command::ShowFactHistory {
            group: group.clone(),
        },
    )
    .await;
    let Event::FactHistoryInfo { history } = ev else {
        panic!("{ev:?}")
    };
    assert!(history.items.is_empty(), "{history:?}");
    let ev = fact_event(&core, &mut rx, Command::ShowForgotten).await;
    let Event::ForgottenInfo { items } = ev else {
        panic!("{ev:?}")
    };
    let [forgotten] = &items[..] else {
        panic!("{items:?}")
    };
    assert_eq!(forgotten.attr.name, "residence");
    assert_eq!(forgotten.values, 1);
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnblockFact {
            target: forgotten.target,
        },
    )
    .await;
    assert!(
        matches!(
            ev,
            Event::FactsUpdated {
                op: FactOp::Unblock,
                slow_next_turn: false,
                ..
            }
        ),
        "{ev:?}"
    );
    let ev = fact_event(&core, &mut rx, Command::ShowForgotten).await;
    assert!(
        matches!(&ev, Event::ForgottenInfo { items } if items.is_empty()),
        "{ev:?}"
    );
    // 印を解いても、取り消した値は戻らない (忘れた値は記録に残さない)。今の値はそのまま。
    let list = facts(&core, &mut rx).await;
    let rows = value_rows(&list);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, "東京");
    let ev = fact_event(&core, &mut rx, Command::ShowFactHistory { group }).await;
    let Event::FactHistoryInfo { history } = ev else {
        panic!("{ev:?}")
    };
    assert!(history.items.is_empty(), "{history:?}");
    core.shutdown().await;
}

/// 後継まで効く private の指示は、指示の後に言われた値も private にし、理由を見分けて示す。`/private off` は、
/// その行に効いているすべての指示を解くので、同じ指示で private になっていたほかの行も外れる。値が 1 つも無く
/// 指示だけが残る属性の行も、解ける (04 §7.1・§7.5)。
///
/// verifies: AC-FR-308-2
#[tokio::test(flavor = "multi_thread")]
async fn private_off_of_a_successors_rule_clears_every_row_it_covers() {
    let dir = TempDir::new("fact-ops-successors");
    prepare(dir.path());
    let llm = chat(&[
        reply(REPLY),
        reply(SHRIMP_FACT),
        reply(REPLY),
        reply(CRAB_FACT),
        reply(REPLY),
        reply(TWO_FACTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, SHRIMP).await;
    let list = facts(&core, &mut rx).await;
    let shrimp = key_of(&list, "えび");
    let ev = fact_event(
        &core,
        &mut rx,
        Command::SetFactPrivate {
            target: shrimp.clone(),
            scope: PrivateScope::Successors,
        },
    )
    .await;
    assert!(matches!(ev, Event::FactsUpdated { .. }), "{ev:?}");
    say(&core, &mut rx, CRAB).await;
    let list = facts(&core, &mut rx).await;
    let causes: Vec<(String, Vec<PrivateCause>)> =
        value_rows(&list).into_iter().map(|r| (r.1, r.3)).collect();
    assert_eq!(
        causes,
        [
            (
                "えび".to_owned(),
                vec![PrivateCause::Chosen {
                    scope: PrivateScope::Successors
                }]
            ),
            ("かに".to_owned(), vec![PrivateCause::Successor]),
        ]
    );
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnsetFactPrivate {
            target: FactRowRef::Value(shrimp),
        },
    )
    .await;
    let Event::FactsUpdated { list, .. } = ev else {
        panic!("{ev:?}")
    };
    assert!(value_rows(&list).iter().all(|r| r.3.is_empty()), "{list:?}");

    // 指示だけが残る属性: 猫を後継まで private にしてから、猫を言ったセッションを消す。
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES_AND_PET).await;
    let list = facts(&core, &mut rx).await;
    let cat = key_of(&list, "猫");
    fact_event(
        &core,
        &mut rx,
        Command::SetFactPrivate {
            target: cat,
            scope: PrivateScope::Successors,
        },
    )
    .await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    let list = facts(&core, &mut rx).await;
    let pet = list
        .groups
        .iter()
        .find(|g| g.group.attribute == "pet")
        .expect("the pet attribute keeps a row");
    let [FactRow::RuleOnly { group, directives }] = &pet.rows[..] else {
        panic!("{pet:?}")
    };
    assert_eq!(directives.len(), 1);
    let ev = fact_event(
        &core,
        &mut rx,
        Command::UnsetFactPrivate {
            target: FactRowRef::RuleOnly(group.clone()),
        },
    )
    .await;
    let Event::FactsUpdated { list, .. } = ev else {
        panic!("{ev:?}")
    };
    assert!(
        list.groups.iter().all(|g| g.group.attribute != "pet"),
        "{list:?}"
    );
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
}

/// 今の発話の側 (v0.4 から、引用の枠は発話の直前の別のメッセージ、05 §4.1)。枠があれば、枠と発話をつなぐ。
fn user_side(messages: &[evorag_llm::ChatMessage]) -> String {
    let n = messages.len();
    if n >= 2
        && messages[n - 2].role == evorag_llm::Role::User
        && messages[n - 2].content.starts_with("<<引用-")
    {
        format!("{}{}", messages[n - 2].content, messages[n - 1].content)
    } else {
        messages[n - 1].content.clone()
    }
}
