//! `evorag rebuild --facts` (03 §10、04 §7.5 の「作り直しと訂正」「作り直しで変わりうること」)。llama-server は
//! 記録の再生で置き換える。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::DataRootModels;
use evorag_core::rebuild::{
    ExtractionStop, FactsSource, FactsUnavailable, RebuildError, RebuildOptions,
};
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{
    CorrectionBody, DirectiveKind, FactBody, FactCtx, FactDirectiveBody, ForgetEnv, ForgetKey,
    ForgetOptions, KIND_CORRECTION, KIND_FACT, KIND_FACT_DIRECTIVE, KIND_SESSION, KIND_TURN,
    ModelOrigin, ModelProbeBody, NORM_V1, ROLE_CHAT, Safety, Scope, Writes, add_directive,
    attributes, block_mark, load_bindings, load_facts, plan_forget, row_key, value_key,
};
use evorag_store::testutil::TempDir;
use evorag_store::{
    Envelope, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer, WriterHandle, meta,
};
use evorag_types::api::{Command, Event};
use evorag_types::{Clock, Id, IdGen, ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
const LIVES: &str = "私は大阪に住んでいます";
const PET: &str = "猫を飼っています";
const WAIT: Duration = Duration::from_secs(30);

fn reply(text: &str) -> String {
    format!(
        r#"{{"events": [{{"delta": {}}}, {{"done": {{}}}}]}}"#,
        serde_json::to_string(text).unwrap()
    )
}

/// 抽出の候補 1 件 (言い切り・今の値)。
fn cand(t: usize, a: &str, v: &str, e: &str) -> String {
    serde_json::json!({"t": t, "s": "self", "a": a, "v": v, "e": e, "m": "fact", "tn": "current", "old": ""})
        .to_string()
}

/// 抽出の答え (候補の並び)。
fn answer(cands: &[String]) -> String {
    reply(&format!(r#"{{"c": [{}]}}"#, cands.join(", ")))
}

/// 応答の llama-server (スロット 2 つ)。`chats` は順に使う答え。`health` が偽なら繋がらない。
fn server(chats: &[String], health: bool) -> ReplayClient {
    server_with(chats, health, 8192)
}

/// スロットの文脈の長さ `n_ctx` を選べる [`server`]。
fn server_with(chats: &[String], health: bool, n_ctx: u32) -> ReplayClient {
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 2,
                     "default_generation_settings": {{"n_ctx": {n_ctx}}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "health": {health},
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

/// 応答のモデルの点検の結果を書いておく (型縛りを使える)。
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

fn open(root: &Path, seed: u64) -> Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_645_000_000)));
    Store::open(OpenOptions {
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

/// 作り直す。`round` で時刻と ID の種を変える。
async fn rebuild(
    root: &Path,
    llm: &ReplayClient,
    round: u64,
) -> Result<evorag_core::rebuild::RebuildReport, RebuildError> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(
        1_762_660_000_000 + 1_000_000 * round as i64,
    )));
    evorag_core::rebuild::run(RebuildOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        config_file: None,
        index_only: false,
        facts: Some(FactsSource {
            llm: Arc::new(llm.clone()),
            aux_request_extra: Default::default(),
            prompt_language: PromptLanguage::Ja,
        }),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 40 + round)),
    })
    .await
}

/// 事実の比べる形 (ID と時刻を除く。置き換えの相手は値で示す)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    attribute: String,
    value: Option<String>,
    state: String,
    other: Option<String>,
    private: bool,
    sources: Vec<(Id, u32, Option<u32>, Option<u32>)>,
}

/// 記録した事実 (封筒と本体。今の版 (`v` = 1) のものだけ)。
fn fact_records(root: &Path) -> Vec<(Envelope, FactBody)> {
    let store = open(root, 1);
    let r = store.read().unwrap();
    let mut out = Vec::new();
    r.for_each(KIND_FACT, |x| {
        if let Read::Ok(e) = x
            && e.v == 1
        {
            let b = e.body_as().unwrap();
            out.push((e, b));
        }
        true
    })
    .unwrap();
    out
}

fn rows(root: &Path) -> Vec<Row> {
    let records = fact_records(root);
    let values: BTreeMap<Id, Option<String>> = records
        .iter()
        .map(|(e, b)| (e.id, b.value.clone()))
        .collect();
    let mut out: Vec<Row> = records
        .into_iter()
        .map(|(_, b)| {
            let other =
                b.by.or(b.against)
                    .map(|id| values.get(&id).cloned().flatten().unwrap_or_default());
            Row {
                attribute: b.attribute,
                value: b.value,
                state: b.state,
                other,
                private: b.private,
                sources: b
                    .sources
                    .iter()
                    .map(|s| (s.turn, s.rev, s.start, s.end))
                    .collect(),
            }
        })
        .collect();
    out.sort();
    out
}

fn count(root: &Path, kind: &str) -> usize {
    let store = open(root, 1);
    let r = store.read().unwrap();
    let mut n = 0;
    r.for_each(kind, |_| {
        n += 1;
        true
    })
    .unwrap();
    n
}

fn through(root: &Path) -> Option<Id> {
    let store = open(root, 1);
    let r = store.read().unwrap();
    evorag_memory::load_extract_progress(&r).unwrap().through
}

fn pre_op_backups(root: &Path) -> usize {
    std::fs::read_dir(root.join("backups").join("pre-op"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// 試験の DB を直接書く手段 (TUI の代わり)。
struct Db {
    store: Arc<Store>,
    handle: WriterHandle,
    thread: Option<std::thread::JoinHandle<()>>,
    clock: Arc<ManualClock>,
    ids: Arc<UuidV7Gen>,
}

const KEY: [u8; 32] = [7; 32];

impl Db {
    fn open(root: &Path, seed: u64) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(
            1_762_650_000_000 + 1_000_000 * seed as i64,
        )));
        let ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), seed));
        let (store, _) = Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.0.6".into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: ids.clone(),
            config_file: None,
        })
        .unwrap();
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).unwrap();
        let thread = std::thread::spawn(move || writer.run());
        Self {
            store,
            handle,
            thread: Some(thread),
            clock,
            ids,
        }
    }

    fn submit(&self, ops: Vec<WriteOp>) {
        self.handle
            .submit_wait(WriteRequest::conversation(ops))
            .unwrap();
    }

    fn key() -> ForgetKey {
        ForgetKey::from_bytes(KEY)
    }

    fn writes(&self) -> Writes<'_> {
        Writes {
            clock: &*self.clock,
            ids: &*self.ids,
        }
    }

    /// 値の鍵を書く (新しいデータ根で、起動が作るのと同じ)。
    fn set_key(&self) {
        self.submit(vec![WriteOp::SetMetaOnce {
            key: meta::FORGET_KEY.into(),
            value: Self::key().to_hex(),
        }]);
    }

    fn session(&self) -> Id {
        let (id, ops) = self.writes().create_session("s").unwrap();
        self.submit(ops);
        id
    }

    fn say(&self, session: Id, text: &str) -> Id {
        let s = match self
            .store
            .read()
            .unwrap()
            .get(KIND_SESSION, session)
            .unwrap()
        {
            Some(Read::Ok(e)) => e,
            other => panic!("{other:?}"),
        };
        self.clock.advance(60_000);
        let (id, ops) = self.writes().user_turn(&s, text).unwrap();
        self.submit(ops);
        id
    }

    fn private(&self, attribute: &str, value: &str) {
        let key = Self::key();
        let safety = Safety::bundled();
        let ctx = FactCtx {
            key: &key,
            attributes: attributes(),
            safety: &safety,
            ids: &*self.ids,
            now: self.clock.now(),
        };
        let (_, ops) = {
            let tx = self.store.read().unwrap();
            add_directive(
                &tx,
                &ctx,
                DirectiveKind::Private {
                    scope: Scope::Successors,
                },
                "self",
                attribute,
                Some(value_key(&key, value)),
            )
            .unwrap()
        };
        self.submit(ops);
    }

    /// 伏せ字にしないで忘れる (04 §7.1)。
    fn forget(&self, attribute: &str, value: &str) -> Id {
        self.forget_with(attribute, value, false)
    }

    /// 忘れる。`redact` なら出典の発話を伏せ字にする。忘れるの指示の ID を返す。
    fn forget_with(&self, attribute: &str, value: &str, redact: bool) -> Id {
        let key = Self::key();
        let vk = value_key(&key, value);
        let env = ForgetEnv {
            key: &key,
            attributes: attributes(),
            marker: "[伏せ字]",
            hidden_marker: "[非表示]",
            ids: &*self.ids,
            now: self.clock.now(),
        };
        let plan = {
            let tx = self.store.read().unwrap();
            let set = load_facts(&tx).unwrap().set;
            let fact = set
                .facts
                .iter()
                .find(|f| f.attribute == attribute && f.vk == Some(vk))
                .expect("the fact to forget");
            plan_forget(
                &tx,
                &env,
                &row_key(fact).unwrap(),
                ForgetOptions {
                    redact,
                    ..ForgetOptions::default()
                },
            )
            .unwrap()
            .expect("a plan")
        };
        self.submit(plan.ops);
        plan.directive
    }

    /// 忘れるの指示 `target` の印を解く (04 §7.5)。
    fn unblock(&self, attribute: &str, target: Id) {
        let key = Self::key();
        let safety = Safety::bundled();
        let ctx = FactCtx {
            key: &key,
            attributes: attributes(),
            safety: &safety,
            ids: &*self.ids,
            now: self.clock.now(),
        };
        let (_, ops) = {
            let tx = self.store.read().unwrap();
            add_directive(
                &tx,
                &ctx,
                DirectiveKind::Unblock { target },
                "self",
                attribute,
                None,
            )
            .unwrap()
        };
        self.submit(ops);
    }

    /// 記録を本体のまま書く (関係と副索引は書かない)。
    fn put(&self, kind: &str, v: u32, body: &impl serde::Serialize) -> Id {
        let id = self.ids.next_id();
        let mut env = Envelope::new(id, kind, v, self.clock.now(), "");
        env.set_body(body).unwrap();
        self.submit(vec![WriteOp::Put(env)]);
        id
    }

    /// 記録を読む。
    fn get(&self, kind: &str, id: Id) -> Envelope {
        match self.store.read().unwrap().get(kind, id).unwrap() {
            Some(Read::Ok(e)) => e,
            other => panic!("{other:?}"),
        }
    }

    /// 検証済みの訂正の記録 (「{old}じゃなくて{new}」の型) を書く。
    fn verified_correction(&self, turn: Id, text: &str, old: &str, new: &str) {
        let key = Self::key();
        let pos = |v: &str| {
            let s = text.find(v).expect("in the text");
            (s as u32, (s + v.len()) as u32)
        };
        let (start, end) = pos(new);
        let (old_start, old_end) = pos(old);
        let body = CorrectionBody {
            stage: "verified".into(),
            turn,
            rev: 0,
            previous: None,
            pattern: "{old}じゃなくて{new}".into(),
            dir_old: Some(old.into()),
            dir_new: Some(new.into()),
            checked: true,
            rejected: None,
            target: Some("fact".into()),
            old: Some(old.into()),
            new: Some(new.into()),
            old_vk: Some(value_key(&key, old).to_hex()),
            new_vk: Some(value_key(&key, new).to_hex()),
            norm: Some(NORM_V1),
            start: Some(start),
            end: Some(end),
            old_start: Some(old_start),
            old_end: Some(old_end),
        };
        let mut env = Envelope::new(self.ids.next_id(), KIND_CORRECTION, 1, self.clock.now(), "");
        env.set_body(&body).unwrap();
        self.submit(vec![WriteOp::Put(env)]);
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 少しずつの抽出 (TUI の裏の処理) と比べる
// ---------------------------------------------------------------------------------------------------------------

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

/// 裏の処理を止めたまま発話し、応答を待ってから裏の処理に戻す。発話のターンの ID を返す。
async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    core.hold(true);
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    let id = turn.reply_to.expect("an answer to the utterance");
    core.hold(false);
    core.settle_background_within(WAIT).await;
    id
}

/// 少しずつ抽出した事実と、作り直した事実が同じ。作り直しは、すべての発話を最初から 1 本の列で送り、進み具合を
/// 最後の発話まで進め、退避を作る。
#[tokio::test(flavor = "multi_thread")]
async fn rebuild_makes_the_same_facts_as_incremental_extraction() {
    let dir = TempDir::new("rebuild-facts-same");
    prepare(dir.path());
    let lives = cand(1, "residence", "大阪", LIVES);
    let pet = cand(1, "pet", "猫", PET);
    let llm = server(
        &[
            reply(REPLY),
            answer(std::slice::from_ref(&lives)),
            reply(REPLY),
            answer(&[pet]),
        ],
        true,
    );
    let (core, mut rx, _) = Core::start(config(dir.path(), 7), Arc::new(llm.clone()))
        .await
        .unwrap();
    core.settle().await;
    core.command(Command::NewSession { title: "s".into() });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    say(&core, &mut rx, LIVES).await;
    let u2 = say(&core, &mut rx, PET).await;
    core.shutdown().await;
    assert_eq!(llm.remaining(), 0);
    let incremental = rows(dir.path());
    assert_eq!(incremental.len(), 2, "{incremental:?}");
    assert_eq!(through(dir.path()), Some(u2));

    let again = server(&[answer(&[lives, cand(2, "pet", "猫", PET)])], true);
    let backups = pre_op_backups(dir.path());
    let r = rebuild(dir.path(), &again, 1).await.unwrap();
    let reqs = again.requests();
    assert_eq!(reqs.len(), 1, "one batch for the whole conversation");
    assert_eq!(
        reqs[0].messages[1].content,
        evorag_memory::extraction_input([LIVES, PET])
    );
    assert_eq!(reqs[0].id_slot, Some(1), "the background slot");
    assert_eq!(
        reqs[0].messages[0].content,
        evorag_memory::extraction_system("ja", attributes())
    );
    assert!(r.backup.join("evorag.redb").is_file());
    assert_eq!(pre_op_backups(dir.path()), backups + 1);
    let f = r.facts.expect("the facts report");
    assert_eq!((f.utterances, f.removed, f.facts), (2, 2, 2), "{f:?}");
    assert_eq!(rows(dir.path()), incremental);
    assert_eq!(through(dir.path()), Some(u2));
    let store = open(dir.path(), 1);
    assert!(
        !store.compact_pending().unwrap(),
        "facts are derived records; compact is not requested (03 §8)"
    );
    assert_eq!(f.lost(), 0);
}

// ---------------------------------------------------------------------------------------------------------------
// 事実への指示と訂正の当て直し
// ---------------------------------------------------------------------------------------------------------------

const OSAKA: &str = "大阪に住んでいます";
const SHRIMP: &str = "私はえびがどうしても食べられないんです";
const HIKING: &str = "週末の趣味は山の登山です";
const MOVED: &str = "大阪じゃなくて東京に住んでいます";

/// 4 つの発話を 1 つの束で送ったときの答え (訂正の発話からは候補を出さない)。
fn four() -> String {
    answer(&[
        cand(1, "residence", "大阪", OSAKA),
        cand(2, "dietary", "えび", "えびがどうしても食べられない"),
        cand(3, "hobby", "登山", HIKING),
    ])
}

/// 作り直しは、事実への指示 (private・忘れる) と、残っている検証済みの訂正を当て直す。訂正の候補は書かない。
///
/// verifies: AC-FR-307-3 (rebuild --facts の後も、忘れた事実が蘇らず、private が外れない)
#[tokio::test(flavor = "multi_thread")]
async fn rebuild_reapplies_directives_and_verified_corrections() {
    let dir = TempDir::new("rebuild-facts-directives");
    prepare(dir.path());
    let (t1, t2, _hiking, t4) = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        (
            db.say(s, OSAKA),
            db.say(s, SHRIMP),
            db.say(s, HIKING),
            db.say(s, MOVED),
        )
    };

    // 1 回目: 事実の無いデータ根から作る。訂正の型に当たる発話があっても、候補を書かない。
    let llm = server(&[four()], true);
    let r = rebuild(dir.path(), &llm, 1).await.unwrap();
    let f = r.facts.unwrap();
    assert_eq!((f.utterances, f.removed, f.facts), (4, 0, 3), "{f:?}");
    assert_eq!(
        count(dir.path(), KIND_CORRECTION),
        0,
        "no correction candidates"
    );
    assert_eq!(through(dir.path()), Some(t4));
    let first = rows(dir.path());
    assert_eq!(
        first
            .iter()
            .map(|r| (r.attribute.as_str(), r.value.as_deref(), r.state.as_str()))
            .collect::<Vec<_>>(),
        [
            ("dietary", Some("えび"), "live"),
            ("hobby", Some("登山"), "live"),
            ("residence", Some("大阪"), "live"),
        ]
    );

    // 利用者の操作 (private・伏せ字にしない忘れる) と、検証済みの訂正。
    {
        let db = Db::open(dir.path(), 6);
        db.private("dietary", "えび");
        db.forget("hobby", "登山");
        db.verified_correction(t4, MOVED, "大阪", "東京");
    }
    let before = rows(dir.path());
    assert!(
        before
            .iter()
            .any(|r| r.attribute == "hobby" && r.state == "retracted")
    );

    // 2 回目: 同じ候補から作り直す。
    // 前の作り直しが点検の結果を消したので、起動で点検し直したことにする。
    prepare(dir.path());
    let llm = server(&[four()], true);
    let r = rebuild(dir.path(), &llm, 2).await.unwrap();
    let f = r.facts.unwrap();
    assert_eq!(f.corrections, 1, "{f:?}");
    assert!(f.blocked >= 1, "the forgotten value was blocked: {f:?}");
    assert_eq!(
        count(dir.path(), KIND_CORRECTION),
        1,
        "no new correction records"
    );
    let span = |text: &str, v: &str| {
        let s = text.find(v).unwrap() as u32;
        (Some(s), Some(s + v.len() as u32))
    };
    let after = rows(dir.path());
    // 出典の範囲は、抽出の候補では根拠の範囲、訂正では値の範囲 (04 §5.1・§5.3)。
    let src = |t: Id, text: &str, v: &str| {
        let (s, e) = span(text, v);
        (t, 0, s, e)
    };
    assert_eq!(
        after,
        vec![
            Row {
                attribute: "dietary".into(),
                value: Some("えび".into()),
                state: "live".into(),
                other: None,
                private: true,
                sources: vec![src(t2, SHRIMP, "えびがどうしても食べられない")],
            },
            Row {
                attribute: "residence".into(),
                value: Some("大阪".into()),
                state: "superseded".into(),
                other: Some("東京".into()),
                private: false,
                sources: vec![src(t1, OSAKA, OSAKA), src(t4, MOVED, "大阪")],
            },
            Row {
                attribute: "residence".into(),
                value: Some("東京".into()),
                state: "live".into(),
                other: None,
                private: false,
                sources: vec![src(t4, MOVED, "東京")],
            },
        ],
        "private stays private, the forgotten value is not recreated (no retracted record), and \
         the verified correction is applied"
    );
}

/// F13: 忘れた今の値より前の矛盾の値は、作り直すと今の値になる (作り直しの差を受け入れる、04 §7.5)。
#[tokio::test(flavor = "multi_thread")]
async fn f13_a_contested_value_before_a_forgotten_current_value_becomes_current() {
    let dir = TempDir::new("rebuild-facts-f13");
    prepare(dir.path());
    let b1 = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let (sa, sb, sc) = (db.session(), db.session(), db.session());
        db.say(sa, "大阪に住んでます");
        let b1 = db.say(sb, "東京に住んでるよ");
        db.say(sc, "大阪に住んでるから近いよ");
        b1
    };
    let three = || {
        answer(&[
            cand(1, "residence", "大阪", "大阪に住んでます"),
            cand(2, "residence", "東京", "東京に住んでるよ"),
            cand(3, "residence", "大阪", "大阪に住んでる"),
        ])
    };
    rebuild(dir.path(), &server(&[three()], true), 1)
        .await
        .unwrap();
    let states: Vec<(Option<String>, String)> = rows(dir.path())
        .into_iter()
        .map(|r| (r.value, r.state))
        .collect();
    assert_eq!(
        states,
        [
            (Some("大阪".into()), "live".into()),
            (Some("東京".into()), "contested".into()),
        ]
    );
    {
        let db = Db::open(dir.path(), 6);
        db.forget("residence", "大阪");
    }
    let forgotten = rows(dir.path());
    assert_eq!(
        forgotten
            .iter()
            .map(|r| (r.value.as_deref(), r.state.as_str()))
            .collect::<Vec<_>>(),
        [(None, "retracted"), (Some("東京"), "contested")],
        "{forgotten:?}"
    );

    prepare(dir.path());
    rebuild(dir.path(), &server(&[three()], true), 2)
        .await
        .unwrap();
    let after = rows(dir.path());
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].value.as_deref(), Some("東京"));
    assert_eq!(after[0].state, "live");
    assert_eq!(
        after[0].sources.iter().map(|s| s.0).collect::<Vec<_>>(),
        [b1]
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 止めるとき
// ---------------------------------------------------------------------------------------------------------------

/// 繋がらないときは何も書かずに止める。確かめの段階で繋がらなければ退避も作らない。抽出の途中で繋がらなく
/// なっても、前の事実と進み具合と派生データは変わらない。点検の結果が無ければ止める。
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_leaves_the_old_facts_intact() {
    let dir = TempDir::new("rebuild-facts-down");
    let t4 = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, OSAKA);
        db.say(s, SHRIMP);
        db.say(s, HIKING);
        db.say(s, MOVED)
    };
    // 点検の結果が無い。
    let err = rebuild(dir.path(), &server(&[four()], true), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::FactsUnavailable(FactsUnavailable::NotProbed)
        ),
        "{err:?}"
    );
    assert_eq!(pre_op_backups(dir.path()), 0);

    prepare(dir.path());
    rebuild(dir.path(), &server(&[four()], true), 2)
        .await
        .unwrap();
    let facts = fact_records(dir.path());
    let ids: Vec<Id> = facts.iter().map(|(e, _)| e.id).collect();
    assert_eq!(ids.len(), 3);
    let backups = pre_op_backups(dir.path());
    let chunks = count(dir.path(), evorag_memory::KIND_CHUNK);

    prepare(dir.path());
    // サーバが止まっている: 確かめで止め、退避も作らない。
    let err = rebuild(dir.path(), &server(&[], false), 3)
        .await
        .unwrap_err();
    assert!(matches!(err, RebuildError::LlmUnreachable(_)), "{err:?}");
    assert_eq!(pre_op_backups(dir.path()), backups);

    // 抽出の途中で繋がらなくなった。
    let err = rebuild(
        dir.path(),
        &server(&[r#"{"connect_error": "refused"}"#.to_owned()], true),
        4,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::Extraction(ExtractionStop::Unreachable(_))
        ),
        "{err:?}"
    );
    let kept: Vec<Id> = fact_records(dir.path()).iter().map(|(e, _)| e.id).collect();
    assert_eq!(kept, ids, "the old facts are kept as they were");
    assert_eq!(through(dir.path()), Some(t4));
    assert_eq!(count(dir.path(), evorag_memory::KIND_CHUNK), chunks);
}

/// 抽出できずに飛ばした発話 (型に合わない答え・4xx) は理由ごとに数え、作り直しが部分的だったことを返す。前のふるい
/// で送らなかった発話は数えない。
#[tokio::test(flavor = "multi_thread")]
async fn skipped_batches_are_reported_as_a_partial_rebuild() {
    let dir = TempDir::new("rebuild-facts-partial");
    prepare(dir.path());
    {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, OSAKA);
        db.say(s, "こんにちは");
    }
    // 型に合わない答えが 2 回続く (束を飛ばす)。
    let r = rebuild(
        dir.path(),
        &server(&[reply("事実はありません"), reply(r#"{"c": "x"}"#)], true),
        1,
    )
    .await
    .unwrap();
    let f = r.facts.unwrap();
    assert_eq!(f.skipped_for("invalid_output"), 1, "{f:?}");
    assert_eq!((f.lost(), f.not_sent(), f.facts), (1, 1, 0), "{f:?}");
}

/// 抽出の途中の締切切れか 5xx が `FAILURE_LIMIT` 回続いたとき、または 4xx のときは、何も書かずに止める (前の事実と
/// 進み具合を残す、03 §10)。
#[tokio::test(flavor = "multi_thread")]
async fn repeated_failures_and_4xx_stop_without_writing() {
    let dir = TempDir::new("rebuild-facts-stop");
    prepare(dir.path());
    let t4 = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, OSAKA);
        db.say(s, SHRIMP);
        db.say(s, HIKING);
        db.say(s, MOVED)
    };
    rebuild(dir.path(), &server(&[four()], true), 1)
        .await
        .unwrap();
    let ids: Vec<Id> = fact_records(dir.path()).iter().map(|(e, _)| e.id).collect();
    assert_eq!(ids.len(), 3);

    let e503 = r#"{"http_error": {"status": 503, "body": "busy"}}"#.to_owned();
    prepare(dir.path());
    let llm = server(&[e503.clone(), e503.clone(), e503], true);
    let err = rebuild(dir.path(), &llm, 2).await.unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::Extraction(ExtractionStop::Failed { attempts: 3, .. })
        ),
        "{err:?}"
    );
    assert_eq!(llm.requests().len(), 3, "the same batch was sent 3 times");
    let kept: Vec<Id> = fact_records(dir.path()).iter().map(|(e, _)| e.id).collect();
    assert_eq!(kept, ids, "the old facts are kept");
    assert_eq!(through(dir.path()), Some(t4));

    let llm = server(
        &[r#"{"http_error": {"status": 400, "body": "bad"}}"#.to_owned()],
        true,
    );
    let err = rebuild(dir.path(), &llm, 3).await.unwrap_err();
    assert!(
        matches!(err, RebuildError::Extraction(ExtractionStop::Rejected(_))),
        "{err:?}"
    );
    assert_eq!(llm.requests().len(), 1, "a 4xx is not retried");
    let kept: Vec<Id> = fact_records(dir.path()).iter().map(|(e, _)| e.id).collect();
    assert_eq!(kept, ids, "the old facts are kept");
}

/// 確かめに外れたら (忘れるの当て直しが済んでいない・記録したモデルと違う・文脈が足りない)、退避を作らずに止める。
#[tokio::test(flavor = "multi_thread")]
async fn failed_checks_stop_before_the_backup() {
    let fixture = |name: &str| {
        let dir = TempDir::new(name);
        prepare(dir.path());
        {
            let db = Db::open(dir.path(), 5);
            db.set_key();
            let s = db.session();
            db.say(s, OSAKA);
            db.say(s, SHRIMP);
            db.say(s, HIKING);
            db.say(s, MOVED);
        }
        dir
    };

    // 忘れるの指示だけがあり、印に当たる事実がまだ取り消されていない (当て直しが要る)。
    let dir = fixture("rebuild-facts-forget-pending");
    rebuild(dir.path(), &server(&[four()], true), 1)
        .await
        .unwrap();
    prepare(dir.path());
    {
        let db = Db::open(dir.path(), 6);
        let key = Db::key();
        let vk = value_key(&key, "大阪");
        db.put(
            KIND_FACT_DIRECTIVE,
            1,
            &FactDirectiveBody {
                op: "forget".into(),
                subject: "self".into(),
                attribute: "residence".into(),
                vk: Some(vk.to_hex()),
                norm: Some(NORM_V1),
                fact: None,
                target: None,
                scope: None,
                marks: vec![block_mark(&key, "self", "residence", &vk).to_hex()],
                vks: vec![vk.to_hex()],
                turns: Vec::new(),
                redactions: Vec::new(),
                hidden: Vec::new(),
                history: false,
                redact: false,
                origin: "user".into(),
            },
        );
    }
    let backups = pre_op_backups(dir.path());
    let err = rebuild(dir.path(), &server(&[four()], true), 2)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::FactsUnavailable(FactsUnavailable::ForgetPending)
        ),
        "{err:?}"
    );
    assert_eq!(pre_op_backups(dir.path()), backups);

    // 記録したモデルと違う。
    let dir = fixture("rebuild-facts-model-mismatch");
    {
        let db = Db::open(dir.path(), 6);
        let bindings = load_bindings(&db.store.read().unwrap()).unwrap();
        let ops = db
            .writes()
            .bind_model(
                &bindings,
                ROLE_CHAT,
                ModelOrigin {
                    model_key: "other".into(),
                    file_name: "other.gguf".into(),
                    model_name: "other".into(),
                    n_params: Some(9),
                    size: Some(9),
                    n_vocab: Some(9),
                    build_info: "b1-test".into(),
                },
            )
            .unwrap();
        db.submit(ops);
    }
    let err = rebuild(dir.path(), &server(&[four()], true), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RebuildError::ModelMismatch { recorded, connected }
            if recorded == "other.gguf" && connected == "chat.gguf"),
        "{err:?}"
    );
    assert_eq!(pre_op_backups(dir.path()), 0);

    // スロットの文脈が抽出に足りない。
    let dir = fixture("rebuild-facts-small-ctx");
    let err = rebuild(dir.path(), &server_with(&[four()], true, 1024), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::FactsUnavailable(FactsUnavailable::ContextTooSmall { n_ctx: 1024 })
        ),
        "{err:?}"
    );
    assert_eq!(pre_op_backups(dir.path()), 0);
}

/// 出力が上限で切れたら、束を半分に分けて送り直す (04 §5.2.1)。
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_answer_is_split_and_sent_again() {
    let dir = TempDir::new("rebuild-facts-truncated");
    prepare(dir.path());
    {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, LIVES);
        db.say(s, PET);
    }
    let cut = r#"{"events": [{"delta": "{\"c\": ["}, {"done": {"finish_reason": "length"}}]}"#;
    let llm = server(
        &[
            cut.to_owned(),
            answer(&[cand(1, "residence", "大阪", LIVES)]),
            answer(&[cand(1, "pet", "猫", PET)]),
        ],
        true,
    );
    let r = rebuild(dir.path(), &llm, 1).await.unwrap();
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        reqs[0].messages[1].content,
        evorag_memory::extraction_input([LIVES, PET])
    );
    assert_eq!(
        reqs[1].messages[1].content,
        evorag_memory::extraction_input([LIVES])
    );
    assert_eq!(
        reqs[2].messages[1].content,
        evorag_memory::extraction_input([PET])
    );
    let f = r.facts.unwrap();
    assert_eq!((f.facts, f.lost()), (2, 0), "{f:?}");
}

/// 新しい版 (`v`) の事実があれば、今の版は事実を作れない (03 §4.2 の規則 6) ので、退避の前に止め、新しい版の事実も
/// 前の事実も残す。
#[tokio::test(flavor = "multi_thread")]
async fn newer_version_facts_stop_the_rebuild_and_are_kept() {
    let dir = TempDir::new("rebuild-facts-newer");
    prepare(dir.path());
    let t = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, LIVES)
    };
    rebuild(
        dir.path(),
        &server(&[answer(&[cand(1, "residence", "大阪", LIVES)])], true),
        1,
    )
    .await
    .unwrap();
    let before = fact_records(dir.path());
    // 新しい版の evorag が書いた事実 (事実の `v` を 2 とする種類の一覧で書く)。
    let newer = {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_658_000_000)));
        let ids = Arc::new(UuidV7Gen::with_seed(clock.clone(), 9));
        let mut kinds = evorag_memory::kinds();
        for k in &mut kinds {
            if k.name == KIND_FACT {
                k.current_v = 2;
            }
        }
        let (store, _) = Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.0.6".into(),
            kinds,
            clock: clock.clone(),
            ids: ids.clone(),
            config_file: None,
        })
        .unwrap();
        let store = Arc::new(store);
        let (writer, handle) = Writer::new(store.clone()).unwrap();
        let thread = std::thread::spawn(move || writer.run());
        let id = ids.next_id();
        let mut env = Envelope::new(id, KIND_FACT, 2, clock.now(), "9.0.0");
        env.set_body(&serde_json::json!({
            "subject": "self", "attribute": "pet", "value": "猫", "state": "live",
            "sources": [{"turn": t, "rev": 0, "start": 0, "end": 3}], "future": true
        }))
        .unwrap();
        handle
            .submit_wait(WriteRequest::conversation(vec![WriteOp::Put(env)]))
            .unwrap();
        handle.shutdown();
        thread.join().unwrap();
        id
    };
    prepare(dir.path());
    let backups = pre_op_backups(dir.path());
    let err = rebuild(
        dir.path(),
        &server(&[answer(&[cand(1, "residence", "大阪", LIVES)])], true),
        2,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RebuildError::FactsUnavailable(FactsUnavailable::NewerFacts { count: 1 })
        ),
        "{err:?}"
    );
    assert_eq!(pre_op_backups(dir.path()), backups, "no backup");
    let store = open(dir.path(), 1);
    let r = store.read().unwrap();
    assert!(
        matches!(r.get(KIND_FACT, newer).unwrap(), Some(Read::Ok(e)) if e.v == 2),
        "the newer fact is kept"
    );
    drop(r);
    drop(store);
    let after = fact_records(dir.path());
    assert_eq!(
        after.iter().map(|(e, _)| e.id).collect::<Vec<_>>(),
        before.iter().map(|(e, _)| e.id).collect::<Vec<_>>(),
        "the old facts are kept"
    );
}

/// 忘れるの当て直しで本文が記録と合わないターンは、抽出に送らない (04 §7.2)。
#[tokio::test(flavor = "multi_thread")]
async fn turns_that_do_not_match_a_forget_are_not_sent() {
    let dir = TempDir::new("rebuild-facts-mismatch");
    prepare(dir.path());
    let t2 = {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, LIVES);
        db.say(s, PET)
    };
    rebuild(
        dir.path(),
        &server(
            &[answer(&[
                cand(1, "residence", "大阪", LIVES),
                cand(2, "pet", "猫", PET),
            ])],
            true,
        ),
        1,
    )
    .await
    .unwrap();
    {
        let db = Db::open(dir.path(), 6);
        db.forget_with("pet", "猫", true);
        // 伏せ字にした本文を、記録と合わない本文に書き換える (外から壊れた・古い版が書き戻したなど)。
        let mut e = db.get(KIND_TURN, t2);
        let mut body: evorag_memory::TurnBody = e.body_as().unwrap();
        body.text = "犬を飼っています".into();
        e.set_body(&body).unwrap();
        db.submit(vec![WriteOp::Put(e)]);
    }
    prepare(dir.path());
    let llm = server(&[answer(&[cand(1, "residence", "大阪", LIVES)])], true);
    let r = rebuild(dir.path(), &llm, 2).await.unwrap();
    let f = r.facts.unwrap();
    assert_eq!(f.skipped_for("forget_mismatch"), 1, "{f:?}");
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].messages[1].content,
        evorag_memory::extraction_input([LIVES]),
        "the mismatched turn is not sent"
    );
}

/// 印を解いた (`unblock`) 忘れるの値は、作り直すと再び覚える (04 §7.5・§7.6)。
#[tokio::test(flavor = "multi_thread")]
async fn an_unblocked_value_comes_back() {
    let dir = TempDir::new("rebuild-facts-unblock");
    prepare(dir.path());
    {
        let db = Db::open(dir.path(), 5);
        db.set_key();
        let s = db.session();
        db.say(s, OSAKA);
        db.say(s, SHRIMP);
        db.say(s, HIKING);
        db.say(s, MOVED);
    }
    rebuild(dir.path(), &server(&[four()], true), 1)
        .await
        .unwrap();
    {
        let db = Db::open(dir.path(), 6);
        let forget = db.forget("hobby", "登山");
        db.unblock("hobby", forget);
    }
    prepare(dir.path());
    let r = rebuild(dir.path(), &server(&[four()], true), 2)
        .await
        .unwrap();
    assert_eq!(r.facts.unwrap().blocked, 0);
    let hobby: Vec<(Option<String>, String)> = rows(dir.path())
        .into_iter()
        .filter(|r| r.attribute == "hobby")
        .map(|r| (r.value, r.state))
        .collect();
    assert_eq!(hobby, [(Some("登山".into()), "live".into())]);
}
