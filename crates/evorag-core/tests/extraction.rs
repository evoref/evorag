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
        structured_ok: Some(structured_ok),
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

/// LLM が数字と漢字の間に空白を足して写した根拠と値 (「7 月 22 日」) も、照合の形 (04 §5.6) で受け付け、
/// 記録する値は発話の書き方 (「7月22日」) にする (04 §5.2 の「記録する文字列」)。
#[tokio::test(flavor = "multi_thread")]
async fn a_spaced_copy_from_the_model_is_stored_as_the_user_wrote_it() {
    const BIRTHDAY: &str = "誕生日は7月22日です";
    const SPACED: &str = r#"{"c": [{"t": 1, "s": "self", "a": "birthday", "v": "7 月 22 日",
        "e": "誕生日は 7 月 22 日です", "m": "fact", "tn": "current", "old": ""}]}"#;
    let dir = TempDir::new("extraction-spaced");
    prepare(dir.path(), true);
    let llm = chat(&[reply(REPLY), reply(SPACED)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, BIRTHDAY).await;
    let ev = until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    assert_eq!(
        ev.last(),
        Some(&Event::FactsChanged {
            added: 1,
            replaced: 0,
            contested: 0
        })
    );
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let (facts, through) = stored(dir.path());
    assert_eq!(through, Some(u1));
    assert_eq!(facts.len(), 1);
    let f = &facts[0];
    assert_eq!(
        (f.subject.as_str(), f.attribute.as_str(), f.value.as_deref()),
        ("self", "birthday", Some("7月22日"))
    );
    assert_eq!(f.state, "live");
    let span = f.sources[0].start.unwrap() as usize..f.sources[0].end.unwrap() as usize;
    assert_eq!(&BIRTHDAY[span], BIRTHDAY);
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

const CORRECTS: &str = "大阪じゃなくて東京です";

/// 検証の LLM の出力 (04 §6 の出力の型)。
fn verify_reply(old: &str, new: &str, e: &str) -> String {
    reply(&serde_json::json!({"target": "fact", "old": old, "new": new, "e": e}).to_string())
}

/// 記録した訂正の本体。
fn corrections(root: &Path) -> Vec<evorag_memory::CorrectionBody> {
    let store = open(root);
    let r = store.read().unwrap();
    let mut out = Vec::new();
    r.for_each(evorag_memory::KIND_CORRECTION, |x| {
        if let Read::Ok(e) = x {
            out.push(e.body_as().unwrap());
        }
        true
    })
    .unwrap();
    out
}

/// 訂正の語のある発話は、抽出と同じ回で候補として書き、抽出の後の手順で 1 件ずつ検証する。検証を通った訂正は
/// 書き込みの入口で畳み、単値の属性の今の値を新しい値で置き換える (04 §6・§5.3)。検証の要求は抽出と同じ設定
/// (裏のスロット・temperature 0・固定の seed・型縛り) で、候補の発話と直前の応答を渡す。作り直した Core は検証
/// し直さない。
#[tokio::test(flavor = "multi_thread")]
async fn a_verified_correction_replaces_the_live_value() {
    let dir = TempDir::new("extraction-correction");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(NO_FACTS),
        verify_reply("大阪", "東京", CORRECTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    let u1 = say(&core, &mut rx, LIVES).await;
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    let u2 = say(&core, &mut rx, CORRECTS).await;
    let ev = until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    assert_eq!(
        ev.last(),
        Some(&Event::FactsChanged {
            added: 1,
            replaced: 1,
            contested: 0
        })
    );
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 5);
    let v = &reqs[4];
    assert_eq!(v.messages[0].content, evorag_memory::verify_system("ja"));
    assert_eq!(
        v.messages[1].content,
        evorag_memory::verify_input("ja", Some(REPLY), CORRECTS)
    );
    assert_eq!(v.id_slot, Some(1), "the background slot");
    assert_eq!(v.generation.temperature, 0.0);
    assert_eq!(v.generation.seed, Some(1));
    assert_eq!(
        v.extra["response_format"]["json_schema"]["schema"],
        evorag_memory::verify_schema()
    );

    let (facts, through) = stored(dir.path());
    assert_eq!(through, Some(u2));
    assert_eq!(facts.len(), 2);
    let old = facts
        .iter()
        .find(|f| f.value.as_deref() == Some("大阪"))
        .unwrap();
    let new = facts
        .iter()
        .find(|f| f.value.as_deref() == Some("東京"))
        .unwrap();
    assert_eq!(old.state, "superseded");
    assert_eq!(new.state, "live");
    assert_eq!(new.attribute, "residence");
    assert_eq!(new.sources.len(), 1);
    assert_eq!(new.sources[0].turn, u2);
    let span = new.sources[0].start.unwrap() as usize..new.sources[0].end.unwrap() as usize;
    assert_eq!(
        &CORRECTS[span], "東京",
        "the new value's span is the source"
    );
    assert_eq!(old.sources[0].turn, u1);
    // 訂正の発話の古い値の範囲も、置き換えた事実の出典になる (04 §5.3 の「終えた発言の出典」)。
    assert_eq!(old.sources.len(), 2);
    let ended = &old.sources[1];
    assert_eq!(ended.turn, u2);
    assert_eq!(
        &CORRECTS[ended.start.unwrap() as usize..ended.end.unwrap() as usize],
        "大阪"
    );

    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    let c = &cs[0];
    assert_eq!(c.stage, "verified");
    assert!(c.checked);
    assert_eq!(c.rejected, None);
    assert_eq!(c.turn, u2);
    assert!(
        c.previous.is_some_and(|p| p > u1 && p < u2),
        "the answer to u1"
    );
    assert_eq!(c.pattern, "{old}じゃなくて{new}");
    assert_eq!(c.dir_old.as_deref(), Some("大阪"));
    assert_eq!(c.dir_new.as_deref(), Some("東京"));
    assert_eq!(
        (c.target.as_deref(), c.old.as_deref(), c.new.as_deref()),
        (Some("fact"), Some("大阪"), Some("東京"))
    );

    // 作り直しても、検証した候補を検証し直さない。
    let idle = chat(&[]);
    let (core, _rx) = start(dir.path(), &idle, 9).await;
    settle(&core).await;
    core.shutdown().await;
    assert!(idle.requests().is_empty());
}

/// 検証の関門で落ちた訂正は事実を変えず、候補のまま理由を残して、検証し直さない。セッションを消すと、訂正の記録も
/// 消える (03 §4.2 の規則 12)。
///
/// verifies: AC-FR-306-1 (検証を通らない訂正は事実を変えない)
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_correction_leaves_the_facts_alone() {
    let dir = TempDir::new("extraction-correction-rejected");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(NO_FACTS),
        // 言い換えた新しい値は発話に無い。
        verify_reply("大阪", "東京都", CORRECTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    let u2 = say(&core, &mut rx, CORRECTS).await;
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0, "the candidate was verified once");

    let (facts, _) = stored(dir.path());
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].value.as_deref(), Some("大阪"));
    assert_eq!(facts[0].state, "live");
    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].stage, "candidate");
    assert!(cs[0].checked);
    assert_eq!(cs[0].rejected.as_deref(), Some("new_not_in_utterance"));
    assert_eq!(cs[0].new, None);

    let session = {
        let store = open(dir.path());
        let r = store.read().unwrap();
        let Some(Read::Ok(turn)) = r.get(KIND_TURN, u2).unwrap() else {
            panic!("the utterance")
        };
        turn.body_as::<TurnBody>().unwrap().session
    };
    let idle = chat(&[]);
    let (core, mut rx) = start(dir.path(), &idle, 9).await;
    settle(&core).await;
    assert!(idle.requests().is_empty(), "not verified again");
    core.command(Command::OpenSession { id: session });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    settle(&core).await;
    core.shutdown().await;
    assert!(
        corrections(dir.path()).is_empty(),
        "deleted with the session"
    );
}

/// 型に合わない検証の答えは 1 回だけやり直し、それでも合わなければ候補を飛ばして (`checked` にして理由を残し)、
/// 検証し直さない (04 §6 の「型が合わない・失敗したときの扱いは抽出と同じ」)。
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_verification_is_retried_once_then_skipped() {
    let dir = TempDir::new("extraction-correction-invalid");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(NO_FACTS),
        reply("訂正です"),
        reply(r#"{"target": "both"}"#),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, CORRECTS).await;
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let reqs = llm.requests();
    assert_eq!(reqs[2].messages, reqs[3].messages, "the same request again");
    assert_eq!(
        reqs[2].messages[1].content,
        evorag_memory::verify_input("ja", None, CORRECTS),
        "no answer before the first utterance"
    );
    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].previous, None);
    assert_eq!(cs[0].rejected.as_deref(), Some("invalid_output"));
    assert!(stored(dir.path()).0.is_empty());

    let idle = chat(&[]);
    let (core, _rx) = start(dir.path(), &idle, 9).await;
    settle(&core).await;
    core.shutdown().await;
    assert!(idle.requests().is_empty(), "not verified again");
}

/// 遅れて届く答え (要求が出ている間に、利用者の操作や削除を挟むため)。
fn slow_reply(text: &str) -> String {
    format!(
        r#"{{"events": [{{"wait_ms": 1500}}, {{"delta": {}}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// 要求が `n` 件出るまで待つ (出たことを知らせる出来事が無いので、長めの上限で見る)。
async fn until_requests(llm: &ReplayClient, n: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while llm.requests().len() < n && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(llm.requests().len(), n, "the request is out");
}

/// 抽出の呼び出しの間にセッションを消すと、読んだ発話の訂正の候補は書かない (書く前にロックの下でターンと改訂の
/// 番号を確かめる、04 §6 の「候補を見つける時機」)。
#[tokio::test(flavor = "multi_thread")]
async fn a_session_deleted_during_extraction_leaves_no_correction() {
    let dir = TempDir::new("extraction-correction-deleted");
    prepare(dir.path(), true);
    let llm = chat(&[reply(REPLY), slow_reply(NO_FACTS)]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    core.hold(true);
    ask(&core, &mut rx, CORRECTS).await;
    core.hold(false);
    until_requests(&llm, 2).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.requests().len(), 2, "nothing to verify");
    assert!(corrections(dir.path()).is_empty());
}

/// 検証の呼び出しの間に候補の発話の改訂の番号が変わったら (伏せ字など)、結果の代わりに `stale_source` を書き、
/// 事実を変えない (04 §6 の「検証の時機」)。
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_change_during_verification_is_recorded_as_stale() {
    let dir = TempDir::new("extraction-correction-stale");
    prepare(dir.path(), true);
    let ok = serde_json::json!({"target": "fact", "old": "大阪", "new": "東京", "e": CORRECTS})
        .to_string();
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(NO_FACTS),
        slow_reply(&ok),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    core.hold(true);
    let u2 = ask(&core, &mut rx, CORRECTS).await;
    core.hold(false);
    until_requests(&llm, 5).await;
    assert!(core.bump_turn_revision(u2).await);
    settle(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].stage, "candidate");
    assert!(cs[0].checked);
    assert_eq!(cs[0].rejected.as_deref(), Some("stale_source"));
    let (facts, _) = stored(dir.path());
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].value.as_deref(), Some("大阪"));
    assert_eq!(facts[0].state, "live");
}

/// 検証の締切切れと 5xx は次の機会に送り直し、同じ候補で 3 回続いたら `failed` として飛ばす (04 §5.2.1)。
#[tokio::test(flavor = "multi_thread")]
async fn three_verification_failures_in_a_row_skip_the_candidate() {
    let dir = TempDir::new("extraction-correction-5xx");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(NO_FACTS),
        http_error(503),
        server_error(),
        http_error(500),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, CORRECTS).await;
    assert_eq!(llm.requests().len(), 3);
    retry(&core).await;
    assert_eq!(
        llm.requests().len(),
        4,
        "sent again after the first failure"
    );
    retry(&core).await;
    assert_eq!(
        llm.requests().len(),
        5,
        "sent again after the second failure"
    );
    retry(&core).await;
    core.shutdown().await;
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 5, "skipped after the third failure");
    for r in &reqs[3..] {
        assert_eq!(r.messages, reqs[2].messages, "the same candidate again");
    }
    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    assert!(cs[0].checked);
    assert_eq!(cs[0].rejected.as_deref(), Some("failed"));
}

/// サーバに繋がらないときは数えず、候補を検証していないまま待つ。戻れば検証して、事実を置き換える。
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_leaves_the_candidate_unchecked() {
    let dir = TempDir::new("extraction-correction-connect");
    prepare(dir.path(), true);
    let llm = chat(&[
        reply(REPLY),
        reply(LIVES_FACT),
        reply(REPLY),
        reply(NO_FACTS),
        connect_error(),
        connect_error(),
        connect_error(),
        connect_error(),
        verify_reply("大阪", "東京", CORRECTS),
    ]);
    let (core, mut rx) = start(dir.path(), &llm, 7).await;
    new_session(&core, &mut rx).await;
    say(&core, &mut rx, LIVES).await;
    until(&mut rx, |e| matches!(e, Event::FactsChanged { .. })).await;
    say(&core, &mut rx, CORRECTS).await;
    assert_eq!(llm.requests().len(), 5);
    for n in 6..=8 {
        retry(&core).await;
        assert_eq!(llm.requests().len(), n, "sent again while unreachable");
    }
    // 4 回続いても飛ばさない (繋がらない失敗は数えない)。
    retry(&core).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let cs = corrections(dir.path());
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].stage, "verified");
    assert_eq!(cs[0].rejected, None);
    let (facts, _) = stored(dir.path());
    let live: Vec<_> = facts.iter().filter(|f| f.state == "live").collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].value.as_deref(), Some("東京"));
}
