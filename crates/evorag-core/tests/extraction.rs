//! 事実の抽出と統合の sleep-time の手順 (04 §4 の手順 3・5、04 §5.2.1)。llama-server は記録の再生で置き換える。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{
    FactBody, KIND_FACT, KIND_SESSION, KIND_TURN, ModelProbeBody, SessionBody, TurnBody,
};
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_store::{Durability, Read, meta};
use evorag_types::api::{Command, Event};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const NO_FACTS: &str = r#"{"c": []}"#;
const LIVES: &str = "私は大阪に住んでいます";
const PET: &str = "猫を飼っています";
const LIVES_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "residence", "v": "大阪",
    "e": "私は大阪に住んでいます", "m": "fact", "tn": "current", "old": ""}]}"#;

/// 待ち合わせの上限 (遅い CI でも足りるように、長めにする)。
const WAIT: Duration = Duration::from_secs(30);

fn reply(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// 生成の上限で切れた答え (`finish_reason` が `length`)。
fn cut_at_limit(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{"finish_reason": "length"}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

fn http_error(status: u16) -> String {
    format!(r#"{{"http_error": {{"status": {status}, "body": "x"}}}}"#)
}

fn connect_error() -> String {
    r#"{"connect_error": "refused"}"#.to_owned()
}

/// 逐次表示の途中で llama-server がエラーを返す答え。
fn server_error() -> String {
    r#"{"events": [{"delta": "{"}, {"server_error": "slot failed"}]}"#.to_owned()
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

/// 起動の前に、応答のモデルの点検の結果を書いておく (型縛りを使えるか)。
fn prepare(root: &Path, structured_ok: bool) {
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
        structured_ok: Some(structured_ok),
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

/// 裏の処理の要る手順が無くなるまで待つ。
async fn settle(core: &Core) {
    core.settle_background_within(WAIT).await;
}

/// 発話し、応答を待つ (裏の処理は止めたまま、`core.hold(true)` の下で呼ぶ)。発話のターンの ID を返す。
async fn ask(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    turn.reply_to.expect("an answer to the utterance")
}

/// 裏の処理を止めたまま発話し、応答を待ってから裏の処理に戻す (抽出が次の応答の答えを先に使わないように)。
/// 発話のターンの ID を返す。
async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    core.hold(true);
    let turn = ask(core, rx, text).await;
    core.hold(false);
    settle(core).await;
    turn
}

/// 待たせている抽出をすぐに試し直させ、終わるまで待つ。
async fn retry(core: &Core) {
    core.retry_background_now();
    settle(core).await;
}

/// 抽出の要求の利用者のメッセージ (`[番号] 本文` の並び)。
fn input(texts: &[&str]) -> String {
    evorag_memory::extraction_input(texts.iter().copied())
}

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>) {
    core.command(Command::NewSession { title: "s".into() });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

fn open(root: &Path) -> evorag_store::Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(0)));
    evorag_store::Store::open(evorag_store::OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: evorag_memory::kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap()
    .0
}

/// 記録した事実と、抽出の進み具合 (処理を終えた最後の発話)。
fn stored(root: &Path) -> (Vec<FactBody>, Option<Id>) {
    let store = open(root);
    let r = store.read().unwrap();
    let mut facts = Vec::new();
    r.for_each(KIND_FACT, |x| {
        if let Read::Ok(e) = x {
            facts.push(e.body_as().unwrap());
        }
        true
    })
    .unwrap();
    let through = evorag_memory::load_extract_progress(&r).unwrap().through;
    (facts, through)
}

/// 発話から事実を書き、進み具合を進める。次の回は新しい発話だけを送り、作り直した Core も済んだ発話を
/// 送り直さない。要求は型縛りの補助の呼び出しの形 (裏のスロット・temperature 0・固定の seed)。
#[tokio::test(flavor = "multi_thread")]
async fn extraction_writes_facts_and_advances_the_watermark() {
    let dir = TempDir::new("extraction");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(NO_FACTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    assert!(core.facts_enabled());
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    let ev = until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    assert_eq!(
        ev.last(),
        Some(&Event::FactsChanged {
            added: 1,
            replaced: 0,
            contested: 0
        })
    );
    let u2 = say(&core, &mut rx, PET).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 4);
    let (first, second) = (&reqs[1], &reqs[3]);
    let system = evorag_memory::extraction_system("ja", evorag_memory::attributes());
    assert_eq!(first.messages[0].content, system);
    assert_eq!(
        first.messages[0].content.as_bytes(),
        second.messages[0].content.as_bytes(),
        "the system message is the same bytes every time"
    );
    assert_eq!(first.messages[1].content, format!("[1] {LIVES}"));
    assert_eq!(
        second.messages[1].content,
        format!("[1] {PET}"),
        "a processed utterance is not sent again"
    );
    assert_eq!(first.id_slot, Some(1), "the background slot");
    assert_eq!(first.generation.temperature, 0.0);
    assert_eq!(first.generation.seed, Some(1));
    let format = &first.extra["response_format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(
        format["json_schema"]["schema"]["properties"]["c"]["items"]["properties"]["t"]["enum"],
        serde_json::json!([1])
    );

    let (facts, through) = stored(dir.path());
    assert_eq!(through, Some(u2));
    assert_eq!(facts.len(), 1);
    let f = &facts[0];
    assert_eq!(
        (f.subject.as_str(), f.attribute.as_str(), f.value.as_deref()),
        ("self", "residence", Some("大阪"))
    );
    assert_eq!(f.state, "live");
    assert_eq!(f.sources.len(), 1);
    assert_eq!(f.sources[0].turn, u1);
    assert_eq!(f.sources[0].rev, 0);
    let span = f.sources[0].start.unwrap() as usize..f.sources[0].end.unwrap() as usize;
    assert_eq!(&LIVES[span], LIVES);

    // 作り直しても、済んだ発話は送らない。
    let idle = chat(&[]);
    let (core, _rx) = start(dir.path(), &idle, 9).await;
    settle(&core).await;
    core.shutdown().await;
    assert!(idle.requests().is_empty());
    assert_eq!(stored(dir.path()).1, Some(u2));
}

/// 型に合わない答えは 1 回だけやり直し、それでも合わなければ束を飛ばして進み具合を進める。
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_reply_is_retried_once_then_the_batch_is_skipped() {
    let dir = TempDir::new("extraction-invalid");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply("事実はありません"),
        reply(r#"{"c": "x"}"#),
        reply(REPLY),
        reply(NO_FACTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    assert_eq!(llm.requests().len(), 3, "the batch was tried twice");
    let u2 = say(&core, &mut rx, PET).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let reqs = llm.requests();
    assert_eq!(reqs[1].messages, reqs[2].messages, "the same request again");
    assert_eq!(reqs[4].messages[1].content, format!("[1] {PET}"));
    let (facts, through) = stored(dir.path());
    assert!(facts.is_empty());
    assert_eq!(through, Some(u2));
}

/// 挨拶・相槌だけの発話は送らず、進み具合だけを進める。
#[tokio::test(flavor = "multi_thread")]
async fn greetings_are_not_sent() {
    let dir = TempDir::new("extraction-greeting");
    prepare(dir.path(), true);
    let llm = chat(&[reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, "ありがとう！").await;
    core.shutdown().await;
    assert_eq!(llm.requests().len(), 1, "only the answer");
    let (facts, through) = stored(dir.path());
    assert!(facts.is_empty());
    assert_eq!(through, Some(u1));
}

/// `memory.forget_key` が使えない (03 §3.1.1 のフェイルクローズ) か、点検で型縛りが通らなかったら抽出しない。
#[tokio::test(flavor = "multi_thread")]
async fn no_extraction_when_facts_are_disabled_or_output_cannot_be_constrained() {
    for case in ["conflict", "no-schema"] {
        let dir = TempDir::new(&format!("extraction-{case}"));
        prepare(dir.path(), case == "conflict");
        if case == "conflict" {
            let (core, _rx) = start(dir.path(), &chat(&[]), 5).await;
            core.shutdown().await;
            raw_set_meta(dir.path(), meta::FORGET_KEY, &"0".repeat(64));
        }
        let llm = chat(&[reply(REPLY)]);
        let (core, mut rx) = start(dir.path(), &llm, 7).await;
        assert_eq!(core.facts_enabled(), case != "conflict", "{case}");
        new_session(&core, &mut rx).await;
        say(&core, &mut rx, LIVES).await;
        core.shutdown().await;
        assert_eq!(llm.requests().len(), 1, "{case}: only the answer");
        let (facts, through) = stored(dir.path());
        assert!(facts.is_empty(), "{case}");
        assert_eq!(through, None, "{case}");
    }
}

/// 利用者が操作したら、出している抽出の要求を落とし、進み具合を進めずに次の機会に同じ束を送り直す (04 §4 の「譲る」)。
/// verifies: AC-FR-301-1 (抽出の手順: 止めた束を次の機会に送り直し、止めずに通したときと同じ事実を書く)
#[tokio::test(flavor = "multi_thread")]
async fn extraction_yields_to_the_user_and_resumes() {
    let dir = TempDir::new("extraction-yield");
    prepare(dir.path(), true);
    let hang = r#"{"events": [{"delta": "{\"c\": ["}, {"hang": true}]}"#.to_owned();
    let llm = chat(&[reply(REPLY), hang, reply(LIVES_FACT)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    core.hold(true);
    core.command(Command::Send { text: LIVES.into() });
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    core.hold(false);
    // 要求が出るまで待つ (出たことを知らせる出来事が無いので、長めの上限で見る)。
    let deadline = tokio::time::Instant::now() + WAIT;
    while llm.requests().len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(llm.requests().len(), 2, "the extraction request is out");
    core.touch();
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.dropped_early(), 1, "the hanging request was dropped");
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[1].messages, reqs[2].messages, "the same batch again");
    let (facts, through) = stored(dir.path());
    assert_eq!(facts.len(), 1);
    assert_eq!(through, Some(facts[0].sources[0].turn));
}

/// 2 つの発話を 1 つの束で送る。`t: 2` の候補は、束の 2 つ目の発話のターンを出典にする。
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_two_maps_candidates_to_their_utterance() {
    let dir = TempDir::new("extraction-two");
    prepare(dir.path(), true);
    let lives_second = LIVES_FACT.replace(r#""t": 1"#, r#""t": 2"#);
    let llm = chat(&[reply(REPLY), reply(REPLY), reply(&lives_second)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    core.hold(true);
    let u1 = ask(&core, &mut rx, PET).await;
    let u2 = ask(&core, &mut rx, LIVES).await;
    core.hold(false);
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3, "one request for both utterances");
    assert_eq!(reqs[2].messages[1].content, input(&[PET, LIVES]));
    assert_eq!(
        reqs[2].extra["response_format"]["json_schema"]["schema"]["properties"]["c"]["items"]["properties"]
            ["t"]["enum"],
        serde_json::json!([1, 2])
    );
    let (facts, through) = stored(dir.path());
    assert_eq!(through, Some(u2));
    assert_eq!(facts.len(), 1);
    let source = &facts[0].sources[0];
    assert_ne!(source.turn, u1);
    assert_eq!(source.turn, u2, "t: 2 is the second utterance");
    let span = source.start.unwrap() as usize..source.end.unwrap() as usize;
    assert_eq!(&LIVES[span], LIVES);
}

/// サーバに繋がらないときは、進み具合を進めず、続いた回数にも入れない (戻れば同じ束を送り直す)。
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_failure_does_not_advance_the_watermark() {
    let dir = TempDir::new("extraction-connect");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        connect_error(),
        connect_error(),
        connect_error(),
        connect_error(),
        reply(LIVES_FACT),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    assert_eq!(llm.requests().len(), 2);
    for n in 3..=5 {
        retry(&core).await;
        assert_eq!(llm.requests().len(), n, "sent again while unreachable");
    }
    retry(&core).await;
    core.shutdown().await;
    assert_eq!(
        llm.remaining(),
        0,
        "the batch was sent again after the server came back"
    );
    let reqs = llm.requests();
    for r in &reqs[2..] {
        assert_eq!(r.messages, reqs[1].messages, "the same batch again");
    }
    let (facts, through) = stored(dir.path());
    assert_eq!(facts.len(), 1, "connection failures are not counted");
    assert_eq!(facts[0].sources[0].turn, u1);
    assert_eq!(through, Some(u1));
}

/// 4xx は要求そのものが受け付けられないので、再試行せずに束を飛ばして進み具合を進める (06 §7)。
#[tokio::test(flavor = "multi_thread")]
async fn http_400_skips_the_batch_and_advances() {
    let dir = TempDir::new("extraction-400");
    prepare(dir.path(), true);
    let llm = chat(&[reply(REPLY), http_error(400)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    core.shutdown().await;
    assert_eq!(llm.requests().len(), 2, "not retried");
    let (facts, through) = stored(dir.path());
    assert!(facts.is_empty());
    assert_eq!(through, Some(u1));
}

/// 5xx と逐次表示の途中の失敗は、進めずに次の機会に送り直す。同じ束で 3 回続いたら飛ばして進める。
#[tokio::test(flavor = "multi_thread")]
async fn three_failures_in_a_row_skip_the_batch() {
    let dir = TempDir::new("extraction-5xx");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        http_error(503),
        server_error(),
        http_error(500),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    assert_eq!(llm.requests().len(), 2);
    retry(&core).await;
    assert_eq!(
        llm.requests().len(),
        3,
        "sent again after the first failure"
    );
    retry(&core).await;
    assert_eq!(
        llm.requests().len(),
        4,
        "sent again after the second failure"
    );
    // 3 回目で飛ばしたので、もう送らない。
    retry(&core).await;
    core.shutdown().await;
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 4);
    for r in &reqs[2..] {
        assert_eq!(r.messages, reqs[1].messages, "the same batch again");
    }
    let (facts, through) = stored(dir.path());
    assert!(facts.is_empty());
    assert_eq!(through, Some(u1), "skipped after three failures");
}

/// 出力が生成の上限で切れたら、2 つの発話の束を半分に分けて、それぞれを送り直す。
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_batch_is_split_in_half() {
    let dir = TempDir::new("extraction-cut");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(REPLY),
        cut_at_limit(r#"{"c": [{"t": 2, "s": "self""#),
        reply(NO_FACTS),
        reply(LIVES_FACT),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    core.hold(true);
    ask(&core, &mut rx, PET).await;
    let u2 = ask(&core, &mut rx, LIVES).await;
    core.hold(false);
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 5);
    assert_eq!(reqs[2].messages[1].content, input(&[PET, LIVES]));
    assert_eq!(reqs[3].messages[1].content, input(&[PET]), "the first half");
    assert_eq!(
        reqs[4].messages[1].content,
        input(&[LIVES]),
        "the second half"
    );
    let (facts, through) = stored(dir.path());
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].sources[0].turn, u2);
    assert_eq!(through, Some(u2));
}

/// チャット以外のモードのセッションの発話は送らず、進み具合だけを進める (04 §5.2.1 の「対象と順」)。
#[tokio::test(flavor = "multi_thread")]
async fn a_non_chat_session_is_skipped() {
    let dir = TempDir::new("extraction-mode");
    prepare(dir.path(), true);
    let llm = chat(&[reply(REPLY)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    // 裏の処理を止めたまま終える (抽出させずに、セッションのモードを書き換えるため)。
    core.hold(true);
    let u1 = ask(&core, &mut rx, LIVES).await;
    core.shutdown().await;
    {
        let store = open(dir.path());
        let tok = store.take_write_token().unwrap();
        let r = store.read().unwrap();
        let Some(Read::Ok(turn)) = r.get(KIND_TURN, u1).unwrap() else {
            panic!("the utterance")
        };
        let session = turn.body_as::<TurnBody>().unwrap().session;
        let Some(Read::Ok(mut env)) = r.get(KIND_SESSION, session).unwrap() else {
            panic!("the session")
        };
        drop(r);
        let mut body: SessionBody = env.body_as().unwrap();
        body.mode = "coding".into();
        env.set_body(&body).unwrap();
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut env).unwrap();
        tx.commit().unwrap();
    }

    let idle = chat(&[]);
    let (core, _rx) = start(dir.path(), &idle, 9).await;
    settle(&core).await;
    core.shutdown().await;
    assert!(idle.requests().is_empty(), "nothing is sent");
    let (facts, through) = stored(dir.path());
    assert!(facts.is_empty());
    assert_eq!(through, Some(u1));
}
