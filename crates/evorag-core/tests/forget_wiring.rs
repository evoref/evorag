//! 忘れるの当て直し・ジャーナルの書き直し・退避の写し直し・合わないターン・書き出しの確かめ (04 §7.2・§7.7、
//! 03 §4.2 の規則 12・§6・§7.3・§11)。llama-server は記録の再生で置き換える。試験の値は架空のもの。
//!
//! 忘れるの操作は、閉じたデータ根に `evorag-memory` の計画 (`plan_forget`) を直接書くか、試験の口
//! (`Core::forget_now`) か、`/forget` の命令 (`PreviewForget`・`ForgetFact`) で作る。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evorag_core::export_checks::ExportFilter;
use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, PromptLanguage};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{ForgetEnv, ForgetKey, ForgetOptions, KIND_TURN, ModelProbeBody, TurnBody};
use evorag_store::export::{ExportError, ExportOptions, export};
use evorag_store::testutil::TempDir;
use evorag_store::{
    BodyPatch, OpenOptions, Read, RewriteFault, Store, WriteOp, WriteRequest, Writer, WriterHandle,
    meta,
};
use evorag_types::api::{
    Command, Event, FactKey, FactList, FactRow, ForgetOptions as ForgetChoices, ForgetPlanHash,
    ForgetPreview, Role, TurnMarkKind,
};
use evorag_types::{Clock, Id, ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const REPLY: &str = "了解です";
/// 忘れる値 (ほかの本文と重ならない)。
const VALUE: &str = "唐津";
const LIVES: &str = "私は唐津に住んでいます。モンステラの水やりは、どれくらいの頻度がいい？";
const LIVES_FACT: &str = r#"{"c": [{"t": 1, "s": "self", "a": "residence", "v": "唐津",
    "e": "私は唐津に住んでいます", "m": "fact", "tn": "current", "old": ""}]}"#;
const ASK: &str = "前に聞いたモンステラの水やりの頻度、もう一度教えて";
const MARKER: &str = "[伏せ字]";
const T0: i64 = 1_762_650_000_000;
const DAY: i64 = 86_400_000;

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
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0 - 10_000_000)));
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

fn config(root: &Path, seed: u64, clock: Arc<ManualClock>) -> CoreConfig {
    config_as(root, seed, clock, "0.0.6")
}

fn config_as(root: &Path, seed: u64, clock: Arc<ManualClock>, version: &str) -> CoreConfig {
    CoreConfig {
        data_root: root.to_path_buf(),
        config_file: None,
        evorag_version: version.into(),
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

fn clock_at(ms: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(Timestamp::from_millis(ms)))
}

async fn start(
    root: &Path,
    llm: &ReplayClient,
    seed: u64,
    clock: Arc<ManualClock>,
) -> (Core, UnboundedReceiver<Event>, evorag_core::StartReport) {
    let (core, rx, report) = Core::start(config(root, seed, clock), Arc::new(llm.clone()))
        .await
        .unwrap();
    core.settle().await;
    settle(&core).await;
    (core, rx, report)
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

/// 裏の処理を止めたまま発話し、応答を待つ。応答のターンを返す。
async fn ask(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    text: &str,
) -> evorag_types::api::TurnView {
    core.hold(true);
    core.command(Command::Send { text: text.into() });
    let ev = until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
    core.settle_precompute().await;
    let Some(Event::ResponseFinished { turn, .. }) = ev.last() else {
        panic!()
    };
    turn.clone()
}

/// 発話して応答を待ち、裏の処理 (抽出・統合・事実の表) を終えるまで待つ。発話のターンの ID を返す。
async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) -> Id {
    let turn = ask(core, rx, text).await;
    core.hold(false);
    settle(core).await;
    turn.reply_to.expect("an answer to the utterance")
}

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>) {
    core.command(Command::NewSession { title: "s".into() });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

async fn facts(core: &Core, rx: &mut UnboundedReceiver<Event>) -> FactList {
    core.command(Command::ShowFacts);
    let ev = until(rx, |e| matches!(e, Event::FactsInfo { .. })).await;
    match ev.into_iter().last() {
        Some(Event::FactsInfo { list }) => list,
        other => panic!("{other:?}"),
    }
}

fn value_key_of(list: &FactList, value: &str) -> Option<FactKey> {
    list.groups
        .iter()
        .flat_map(|g| g.rows.iter())
        .find_map(|r| match r {
            FactRow::Value { key, value: v, .. } if v == value => Some(key.clone()),
            _ => None,
        })
}

/// 事実 (唐津に住んでいる) のある会話を作って閉じる。発話のターンと、事実の一覧の行の参照を返す。
async fn conversation(root: &Path, clock: Arc<ManualClock>) -> (Id, FactKey) {
    prepare(root);
    let llm = chat(&[reply(REPLY), reply(LIVES_FACT)]);
    let (core, mut rx, _) = start(root, &llm, 7, clock).await;
    new_session(&core, &mut rx).await;
    let turn = say(&core, &mut rx, LIVES).await;
    let list = facts(&core, &mut rx).await;
    let key = value_key_of(&list, VALUE).expect("the fact was extracted");
    core.shutdown().await;
    (turn, key)
}

/// 閉じたデータ根を、試験の手で直接開く。
struct Closed {
    store: Arc<Store>,
    handle: WriterHandle,
    thread: Option<std::thread::JoinHandle<()>>,
    ids: UuidV7Gen,
    clock: Arc<ManualClock>,
}

impl Closed {
    fn open(root: &Path) -> Self {
        Self::open_as(root, "0.0.6")
    }

    /// 版 `version` の evorag として開く。
    fn open_as(root: &Path, version: &str) -> Self {
        let clock = clock_at(T0 + 3_600_000);
        let (store, _) = Store::open(OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: version.into(),
            kinds: evorag_memory::kinds(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock.clone(), 41)),
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
            ids: UuidV7Gen::with_seed(clock.clone(), 42),
            clock,
        }
    }

    fn write(&self, ops: Vec<WriteOp>) -> evorag_store::WriteOutcome {
        self.handle
            .submit_wait(WriteRequest::conversation(ops))
            .unwrap()
    }

    fn recheck_flag(&self) -> Option<String> {
        self.store.meta(meta::FORGET_RECHECK).unwrap()
    }

    fn key(&self) -> ForgetKey {
        let hex = self.store.meta(meta::FORGET_KEY).unwrap().expect("a key");
        ForgetKey::from_hex(&hex).unwrap()
    }

    /// 既定の選択で、忘れるの計画を作る。
    fn plan(&self, target: &FactKey) -> evorag_memory::ForgetPlan {
        let key = self.key();
        let env = ForgetEnv {
            key: &key,
            attributes: evorag_memory::attributes(),
            marker: MARKER,
            hidden_marker: "[非表示]",
            ids: &self.ids,
            now: self.clock.now(),
        };
        let tx = self.store.read().unwrap();
        evorag_memory::plan_forget(&tx, &env, target, ForgetOptions::default())
            .unwrap()
            .expect("the forget is planned")
    }

    fn turn(&self, id: Id) -> (TurnBody, u64) {
        let tx = self.store.read().unwrap();
        match tx.get(KIND_TURN, id).unwrap() {
            Some(Read::Ok(e)) => (e.body_as().unwrap(), e.body_uint("rev").unwrap_or(0)),
            other => panic!("{other:?}"),
        }
    }
}

impl Drop for Closed {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// フォルダの下のファイルのうち、`needle` のバイト列を含むもの。
fn files_with(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_with(&p, needle));
        } else if let Ok(b) = std::fs::read(&p)
            && b.windows(needle.len()).any(|w| w == needle.as_bytes())
        {
            out.push(p);
        }
    }
    out
}

/// 書き出したファイルの記録の ID (先頭の形式の行を除く)。
fn exported_ids(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["id"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn export_opts(checks: &ExportFilter) -> ExportOptions<'_> {
    ExportOptions {
        evorag_version: "0.0.6",
        now_ms: T0,
        exclude: &[
            evorag_memory::KIND_MODEL_PROBE,
            evorag_memory::KIND_CHUNK,
            evorag_memory::KIND_EMBEDDING,
            evorag_memory::KIND_JOB,
        ],
        checks,
    }
}

/// 忘れるの操作の行 (指示と伏せ字にしたターン) をジャーナルに書いた後、DB のコミットの前に止まったときは、次の
/// 起動で当て直す: 事実を取り消し、伏せ字の前の本文から作った派生の記録を消し、job と `compact.pending` を書く。
/// その次の起動 (compact) の後は、DB のファイルとジャーナルに値が残らない。
///
/// verifies: 04 §7.2 (起動時の当て直し)、AC-FR-307-2
#[tokio::test(flavor = "multi_thread")]
async fn a_forget_cut_short_before_its_commit_is_replayed_at_the_next_start() {
    let dir = TempDir::new("forget-crash");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    // 忘れるの前の DB を取っておき、操作を書いた後に戻す (ジャーナルの行だけが先に書けた状態をまねる)。
    let db = root.join(evorag_store::DB_FILE);
    let saved = root.join("saved.redb");
    std::fs::copy(&db, &saved).unwrap();
    {
        let c = Closed::open(root);
        let plan = c.plan(&key);
        assert_eq!(plan.redacted_turns, [turn]);
        c.write(plan.ops);
    }
    std::fs::copy(&saved, &db).unwrap();
    std::fs::remove_file(&saved).unwrap();

    let llm = chat(&[]);
    let (core, mut rx, report) = start(root, &llm, 8, clock_at(T0 + 2 * 3_600_000)).await;
    assert!(report.store.journal_replayed > 0);
    assert!(report.forget.mismatched.is_empty(), "{:?}", report.forget);
    assert!(!report.forget.journal_pending);
    let list = facts(&core, &mut rx).await;
    assert!(value_key_of(&list, VALUE).is_none(), "{list:?}");
    assert_eq!(
        core.pending_cleanup_jobs(),
        0,
        "the jobs were done at startup"
    );
    core.shutdown().await;

    {
        let c = Closed::open(root);
        let (body, rev) = c.turn(turn);
        assert_eq!(rev, 1);
        assert!(body.text.contains(MARKER) && !body.text.contains(VALUE));
        let tx = c.store.read().unwrap();
        let live = evorag_memory::load_facts(&tx).unwrap().set;
        assert!(
            live.facts.iter().all(|f| f.value.as_deref() != Some(VALUE)),
            "{live:?}"
        );
        // 伏せ字の前の本文から作ったチャンクは残らない (作り直したものは伏せ字の後の番号を持つ)。
        for (_, ch) in evorag_memory::chunks_of(&tx, turn).unwrap() {
            assert_eq!(ch.src_rev(), 1);
        }
        assert_eq!(
            c.store.meta(meta::COMPACT_PENDING).unwrap().as_deref(),
            Some("1")
        );
    }
    // 次の起動で compact する。DB のファイル・ジャーナル・退避に値が残らない。
    let (core, _rx, _) = start(root, &chat(&[]), 9, clock_at(T0 + 3 * 3_600_000)).await;
    core.shutdown().await;
    for sub in [
        root.join(evorag_store::DB_FILE),
        root.join(evorag_store::JOURNAL_DIR),
        root.join("backups"),
    ] {
        let hits = if sub.is_dir() {
            files_with(&sub, VALUE)
        } else {
            files_with(sub.parent().unwrap(), VALUE)
                .into_iter()
                .filter(|p| *p == sub)
                .collect()
        };
        assert!(hits.is_empty(), "{hits:?}");
    }
}

/// 古い退避から戻した後の起動で、ジャーナルの忘れるを当て直し、すべての忘れるの指示の `forget_backups` の job を
/// 作り直して、退避からも値を除く。
///
/// verifies: 03 §4.2 の規則 12 (restore の後)、03 §10、04 §7.2、AC-FR-307-3
#[tokio::test(flavor = "multi_thread")]
async fn restoring_an_older_backup_replays_forgets_and_rewrites_the_backups() {
    let dir = TempDir::new("forget-restore");
    let root = dir.path();
    let clock = clock_at(T0);
    let (_, key) = conversation(root, clock.clone()).await;
    // 次の日の退避 (値を含む) を作る。
    {
        let c = Closed::open(root);
        c.clock.advance(DAY);
        c.store.daily_backup(None).unwrap().expect("a daily backup");
    }
    let backup = evorag_store::restore::list_backups(root, None)
        .into_iter()
        .find(|b| !files_with(&b.dir, VALUE).is_empty())
        .expect("a backup holding the value")
        .dir;

    // 忘れる (退避の写し直しは、まとめて待つ間に終了するので job に残る)。
    let llm = chat(&[]);
    let (core, _rx, _) = start(root, &llm, 8, clock_at(T0 + DAY + 1_000)).await;
    core.forget_now(key, ForgetOptions::default())
        .await
        .unwrap();
    core.shutdown().await;

    evorag_store::restore::restore(
        root,
        &backup,
        T0 + DAY + 2_000,
        None,
        &evorag_memory::carried_on_restore,
    )
    .unwrap();
    // 戻した DB に、起動時に退避の写し直しの job を作り直す印が立つ (03 §10)。
    assert_eq!(Closed::open(root).recheck_flag().as_deref(), Some("1"));

    let (core, mut rx, _) = start(root, &chat(&[]), 9, clock_at(T0 + DAY + 3_000)).await;
    let list = facts(&core, &mut rx).await;
    assert!(value_key_of(&list, VALUE).is_none(), "{list:?}");
    assert_eq!(core.pending_cleanup_jobs(), 0);
    core.shutdown().await;
    let hits = files_with(&root.join("backups"), VALUE);
    assert!(hits.is_empty(), "{hits:?}");
    // job を書いた依頼で、印を消した。
    assert_eq!(Closed::open(root).recheck_flag(), None);
}

/// 前回書いた版が 0.3.0 より古いデータ根を開くと、`last_writer` を書き換えるのと同じコミットで印が立つ。job を
/// 書く前に止まっても (コアを起動しないまま)、次の起動で、すべての忘れるの指示の `forget_backups` の job を作り
/// 直して退避から値を除き、印を消す。印は compact の写しにも残る。
///
/// verifies: 03 §4.2 の規則 12 の (1)、03 §7.3、04 §7.2
#[tokio::test(flavor = "multi_thread")]
async fn an_old_writer_flag_survives_a_stop_before_the_jobs_are_written() {
    let dir = TempDir::new("forget-old-writer");
    let root = dir.path();
    let (_, key) = conversation(root, clock_at(T0)).await;
    {
        let c = Closed::open(root);
        c.clock.advance(DAY);
        c.store.daily_backup(None).unwrap().expect("a daily backup");
        // 忘れる。退避の写し直しの job は書かない (古い版が開いた間に作った退避が残っている状態をまねる)。
        let plan = c.plan(&key);
        let ops: Vec<WriteOp> = plan
            .ops
            .into_iter()
            .filter(|op| {
                !matches!(op, WriteOp::Put(e)
                    if e.kind == evorag_memory::KIND_JOB
                        && e.body_as::<evorag_memory::JobBody>()
                            .is_ok_and(|b| b.task == evorag_memory::JOB_FORGET_BACKUPS))
            })
            .collect();
        c.write(ops);
        assert_eq!(c.recheck_flag(), None);
    }
    assert!(!files_with(&root.join("backups"), VALUE).is_empty());
    // v0.3 が開いて印を立て、job を書く前に止まる。
    assert_eq!(
        Closed::open_as(root, "0.3.0").recheck_flag().as_deref(),
        Some("1")
    );
    let (core, _rx, _) = Core::start(
        config_as(root, 9, clock_at(T0 + DAY + 1_000), "0.3.0"),
        Arc::new(chat(&[])),
    )
    .await
    .unwrap();
    core.settle().await;
    settle(&core).await;
    assert_eq!(core.pending_cleanup_jobs(), 0);
    core.shutdown().await;
    let hits = files_with(&root.join("backups"), VALUE);
    assert!(hits.is_empty(), "{hits:?}");
    assert_eq!(Closed::open_as(root, "0.3.0").recheck_flag(), None);
}

/// `forget_backups` の job は、操作のコミットの直後に、日次・週次・移行と作り直しの前の退避のすべてを、伏せ字に
/// して写し直す。済んだら job を消す。
///
/// verifies: 03 §7.3 (`forget_backups`)、AC-FR-307-1 (退避)
#[tokio::test(flavor = "multi_thread")]
async fn the_backup_job_rewrites_daily_weekly_and_pre_op_backups() {
    let dir = TempDir::new("forget-backups");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    {
        let c = Closed::open(root);
        c.clock.advance(DAY);
        c.store.daily_backup(None).unwrap().expect("a daily backup");
        c.store.pre_op_backup("test", &[]).unwrap();
    }
    for sub in ["daily", "weekly", "pre-op"] {
        assert!(
            !files_with(&root.join("backups").join(sub), VALUE).is_empty(),
            "{sub} holds the value before the forget"
        );
    }
    let (core, _rx, _) = start(root, &chat(&[]), 8, clock_at(T0 + DAY + 1_000)).await;
    core.forget_now(key, ForgetOptions::default())
        .await
        .unwrap();
    // 待たずに始まった写し直しを待つ。
    core.settle().await;
    assert_eq!(core.pending_cleanup_jobs(), 0);
    core.shutdown().await;
    let hits = files_with(&root.join("backups"), VALUE);
    assert!(hits.is_empty(), "{hits:?}");
    // 写しの中のターンは、伏せ字の後の本文で残る (ID の文字列ではなくバイト列の ID を探す)。
    let pre_op = &root.join("backups").join("pre-op");
    assert!(!files_with(pre_op, MARKER).is_empty());
    let _ = turn;
}

/// 本文が忘れるの指示と合わないターン (値を含む) は、会話の履歴に送らず、画面には本文を出さずに「確かめられ
/// ない」とし、チャンクを埋め込まない。
///
/// verifies: 04 §7.2 (合わないターン)
#[tokio::test(flavor = "multi_thread")]
async fn a_mismatched_turn_is_withheld_from_the_history_the_view_and_embeddings() {
    let dir = TempDir::new("forget-withheld");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    let session = {
        let c = Closed::open(root);
        let plan = c.plan(&key);
        c.write(plan.ops);
        // 伏せ字の後の番号のまま、値を含む本文に戻す (合わないターン)。
        c.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: turn,
            fields: BodyPatch::new().set("text", &LIVES).unwrap(),
            updated_at: None,
        }]);
        c.turn(turn).0.session
    };
    let llm = chat(&[reply(REPLY)]);
    let mut cfg = config(root, 9, clock_at(T0 + 2 * 3_600_000));
    let emb = embedder();
    cfg.embed = Some(emb.clone() as Arc<dyn evorag_llm::LlmClient>);
    let (core, mut rx, report) = Core::start(cfg, Arc::new(llm.clone())).await.unwrap();
    core.settle().await;
    settle(&core).await;
    assert_eq!(report.forget.mismatched, [turn]);
    // 画面: 本文を出さずに「確かめられない」とする (04 §7.2)。
    let view = core.turns(session);
    let shown = view
        .iter()
        .find(|t| t.id == turn)
        .expect("the turn is listed");
    assert!(shown.unverified && shown.text.is_empty(), "{shown:?}");
    core.command(Command::OpenSession { id: session });
    let ev = until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    let Some(Event::SessionOpened { turns, .. }) = ev.last() else {
        panic!()
    };
    assert!(turns.iter().all(|t| !t.text.contains(VALUE)), "{turns:?}");
    assert!(turns.iter().any(|t| t.id == turn && t.unverified));
    // 会話の履歴にも、先に温める要求にも、値を送らない。
    ask(&core, &mut rx, "うん").await;
    core.hold(false);
    settle(&core).await;
    let sent: Vec<String> = llm
        .requests()
        .iter()
        .chain(llm.warm_requests().iter())
        .flat_map(|r| r.messages.iter().map(|m| m.content.clone()))
        .collect();
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|m| !m.contains(VALUE)), "{sent:?}");
    assert!(emb.aux_calls().0 > 0, "other chunks are embedded");
    core.shutdown().await;
    // 合わないターンのチャンクは埋め込まない。
    let c = Closed::open(root);
    let tx = c.store.read().unwrap();
    let chunks = evorag_memory::chunks_of(&tx, turn).unwrap();
    assert!(!chunks.is_empty());
    for (id, _) in chunks {
        assert!(
            tx.links_to(id, evorag_memory::REL_EMBEDDING_OF)
                .unwrap()
                .is_empty(),
            "a chunk of the mismatched turn was embedded"
        );
    }
}

/// 埋め込みの llama-server。
fn embedder() -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(
            r#"{
              "props": {"build_info": "b1-test", "model_path": "/m/emb.gguf", "total_slots": 4,
                        "default_generation_settings": {"n_ctx": 8192}},
              "models": {"data": [{"id": "emb", "meta": {"n_params": 5, "size": 6, "n_vocab": 7}}]},
              "embed": {"dim": 8}
            }"#,
        )
        .unwrap(),
    )
}

/// ジャーナルの書き直しの入れ替えに失敗したら、状態に示し、sleep-time に試し直す。
///
/// verifies: 03 §6 (書き直しの失敗)、04 §7.2、04 §7.8
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_journal_swap_is_retried_in_sleep_time() {
    let dir = TempDir::new("forget-swap");
    let root = dir.path();
    let (_, key) = conversation(root, clock_at(T0)).await;
    let (core, _rx, _) = start(root, &chat(&[]), 8, clock_at(T0 + 1_000)).await;
    core.hold(true);
    core.inject_journal_rewrite_fault(RewriteFault::FailSwap { nth: 0 });
    core.forget_now(key, ForgetOptions::default())
        .await
        .unwrap();
    assert!(core.forget_status().journal_pending);
    let journal = root.join(evorag_store::JOURNAL_DIR);
    assert!(
        !files_with(&journal, VALUE).is_empty(),
        "the line from before the redaction is still there"
    );
    core.hold(false);
    core.retry_background_now();
    settle(&core).await;
    assert!(!core.forget_status().journal_pending);
    let hits = files_with(&journal, VALUE);
    assert!(hits.is_empty(), "{hits:?}");
    core.shutdown().await;
}

/// 本文が忘れるの指示と合わないターンは、本文を変えずに残し、想起と書き出しから除き、状態に示す。
///
/// verifies: 04 §7.2 (合わないターン)、03 §11
#[tokio::test(flavor = "multi_thread")]
async fn a_mismatched_turn_is_kept_out_of_recall_and_export() {
    let dir = TempDir::new("forget-mismatch");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    // 対照: 忘れる前は、その往復を添える。
    {
        let llm = chat(&[reply(REPLY)]);
        let (core, mut rx, _) = start(root, &llm, 8, clock_at(T0 + 1_000)).await;
        new_session(&core, &mut rx).await;
        let answer = ask(&core, &mut rx, ASK).await;
        assert_eq!(answer.sources, 1);
        let sent = llm
            .requests()
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .content
            .clone();
        assert!(sent.contains("どれくらいの頻度"), "{sent}");
        // 対照の問いの往復は消す (後の問いで、こちらを添えないように)。
        core.command(Command::DeleteSession);
        until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
        core.settle().await;
        core.shutdown().await;
    }
    let tampered = {
        let c = Closed::open(root);
        let plan = c.plan(&key);
        c.write(plan.ops);
        // 伏せ字の後の番号のまま、本文を記録と合わないものにする。
        let (body, rev) = c.turn(turn);
        assert_eq!(rev, 1);
        let text = body.text.replace("私は", "僕は");
        c.write(vec![WriteOp::Patch {
            kind: KIND_TURN.into(),
            id: turn,
            fields: BodyPatch::new().set("text", &text).unwrap(),
            updated_at: None,
        }]);
        text
    };

    let llm = chat(&[reply(REPLY)]);
    let (core, mut rx, report) = start(root, &llm, 9, clock_at(T0 + 2 * 3_600_000)).await;
    assert_eq!(report.forget.mismatched, [turn]);
    assert_eq!(core.forget_status().mismatched, [turn]);
    new_session(&core, &mut rx).await;
    let answer = ask(&core, &mut rx, ASK).await;
    assert_eq!(answer.sources, 0);
    let sent = llm
        .requests()
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    assert!(!sent.contains("どれくらいの頻度がいい"), "{sent}");
    core.shutdown().await;
    {
        let c = Closed::open(root);
        // 本文は変えない。
        assert_eq!(c.turn(turn).0.text, tampered);
        let attached = c.turn(answer.id).0.attached;
        assert!(attached.is_empty(), "{attached:?}");
    }
    let out = root.join("export");
    let checks = ExportFilter::new();
    export(root, &out, &export_opts(&checks)).unwrap();
    let ids = exported_ids(&out.join("turn.jsonl"));
    assert!(!ids.is_empty());
    assert!(!ids.contains(&turn.to_string()), "{ids:?}");
    // 状態の行と doctor の数。
    let check = evorag_store::inspect::with_view(root, |v| {
        evorag_core::export_checks::forget_check(v, Some("ja"))
    })
    .unwrap()
    .flatten()
    .unwrap();
    assert_eq!(check.mismatched, 1);
    assert!(!check.pending);
}

/// 当て直しの済んでいない忘れるがあれば、書き出しを断る。一度起動して当て直した後は、伏せ字の後の本文で書き出す。
///
/// verifies: 03 §11 (private と忘れる)、04 §7.2
#[tokio::test(flavor = "multi_thread")]
async fn export_is_refused_until_the_forget_is_replayed() {
    let dir = TempDir::new("forget-export");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    {
        let c = Closed::open(root);
        let plan = c.plan(&key);
        let d = plan.directive;
        // 指示 (とその関係・副索引) だけを書く (伏せ字・取り消しは当て直しに任せる)。
        let ops: Vec<WriteOp> = plan
            .ops
            .into_iter()
            .filter(|op| match op {
                WriteOp::Put(e) => e.id == d,
                WriteOp::PutLink { from, .. } => *from == d,
                WriteOp::PutLookup { id, .. } => *id == d,
                _ => false,
            })
            .collect();
        c.write(ops);
        assert_eq!(c.turn(turn).1, 0);
    }
    let out = root.join("export");
    let checks = ExportFilter::new();
    let err = export(root, &out, &export_opts(&checks)).unwrap_err();
    assert!(matches!(err, ExportError::PendingForget), "{err:?}");
    assert!(!out.exists());

    let (core, _rx, _) = start(root, &chat(&[]), 8, clock_at(T0 + 2 * 3_600_000)).await;
    core.shutdown().await;
    let checks = ExportFilter::new();
    export(root, &out, &export_opts(&checks)).unwrap();
    let turns = std::fs::read_to_string(out.join("turn.jsonl")).unwrap();
    assert!(turns.contains(MARKER), "{turns}");
    let hits = files_with(&out, VALUE);
    assert!(hits.is_empty(), "{hits:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// `/forget` の命令 (04 §7.1〜§7.3)
// ---------------------------------------------------------------------------------------------------------------

/// ノートの鍵の答え (起動の裏の処理が、前の起動の往復のノートを作る)。
const KEYS: &str = r#"{"keys": ["観葉植物の水やり"]}"#;

/// 生成を途中で止めたままにする応答 (応答を生成している間をまねる)。
const HANG: &str = r#"{"events": [{"delta": "考え"}, {"hang": true}]}"#;

/// 開いているセッションを開き直す (前の起動で作ったセッション)。セッションの ID を返す。
async fn reopen(core: &Core, rx: &mut UnboundedReceiver<Event>) -> Id {
    let session = core.sessions(1)[0].id;
    core.command(Command::OpenSession { id: session });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    session
}

async fn preview_of(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    key: &FactKey,
    options: ForgetChoices,
) -> ForgetPreview {
    core.command(Command::PreviewForget {
        target: key.clone(),
        options,
    });
    let ev = until(rx, |e| {
        matches!(
            e,
            Event::ForgetPreviewInfo { .. } | Event::ForgetRejected { .. }
        )
    })
    .await;
    match ev.into_iter().last() {
        Some(Event::ForgetPreviewInfo { preview }) => preview,
        other => panic!("{other:?}"),
    }
}

/// 忘れるの実行の結果 (`FactForgotten` か `ForgetRejected`) までのイベント。
async fn forget_with(
    core: &Core,
    rx: &mut UnboundedReceiver<Event>,
    key: &FactKey,
    options: ForgetChoices,
    plan: ForgetPlanHash,
) -> Vec<Event> {
    core.command(Command::ForgetFact {
        target: key.clone(),
        options,
        plan,
    });
    until(rx, |e| {
        matches!(
            e,
            Event::FactForgotten { .. } | Event::ForgetRejected { .. }
        )
    })
    .await
}

/// `/forget` の見本は記録を変えない。見本の計画のハッシュを添えた実行で忘れ、開いているセッションの伏せたターン
/// を、印の範囲を添えて知らせ、新しい一覧を返す。応答のモデルのすべてのスロットにキャッシュの消去を求め、断られ
/// ても (既定の構成の 501) 続ける。
///
/// verifies: 04 §7.1 (見本と計画)、04 §7.2 (llama-server の行)、04 §7.3 (画面への印の範囲)
#[tokio::test(flavor = "multi_thread")]
async fn the_forget_command_previews_then_forgets_with_the_plan_hash() {
    let dir = TempDir::new("forget-command");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    let llm = chat(&[]);
    let (core, mut rx, _) = start(root, &llm, 8, clock_at(T0 + 3_600_000)).await;
    core.hold(true);
    let session = reopen(&core, &mut rx).await;

    let preview = preview_of(&core, &mut rx, &key, ForgetChoices::default()).await;
    assert_eq!(preview.target, key);
    assert_eq!(preview.samples.len(), 1, "{preview:?}");
    let s = &preview.samples[0];
    assert_eq!((s.turn, s.role), (turn, Role::User));
    assert!(s.before.contains(VALUE) && !s.before.contains('\n'));
    assert!(s.after.contains(MARKER) && !s.after.contains(VALUE));
    let Ok(hash) = preview.plan.clone() else {
        panic!("{preview:?}")
    };
    // 見本は記録を変えない。
    assert!(
        core.turns(session)
            .iter()
            .any(|t| t.id == turn && t.text.contains(VALUE) && t.marks.is_empty())
    );
    assert!(value_key_of(&facts(&core, &mut rx).await, VALUE).is_some());
    assert_eq!(core.pending_cleanup_jobs(), 0);
    assert!(llm.erased_slots().is_empty());

    let ev = forget_with(&core, &mut rx, &key, ForgetChoices::default(), hash).await;
    let shown = ev
        .iter()
        .find_map(|e| match e {
            Event::TurnsRedacted { session: s, turns } if *s == session => Some(turns.clone()),
            _ => None,
        })
        .expect("the open session is told about the redacted turn");
    let t = shown
        .iter()
        .find(|t| t.id == turn)
        .expect("the source turn");
    assert!(!t.text.contains(VALUE) && !t.unverified);
    assert_eq!(t.marks.len(), 1, "{t:?}");
    assert_eq!(t.marks[0].kind, TurnMarkKind::Redacted);
    assert_eq!(&t.text[t.marks[0].range.clone()], MARKER);
    match ev.last() {
        Some(Event::FactForgotten {
            list,
            redacted,
            journal_pending,
            broken,
        }) => {
            assert!(value_key_of(list, VALUE).is_none(), "{list:?}");
            assert_eq!((*redacted, *journal_pending, *broken), (1, false, 0));
        }
        other => panic!("{other:?}"),
    }
    // 照会のターンにも印の範囲を添える。
    let again = core.turns(session);
    let t = again.iter().find(|t| t.id == turn).unwrap();
    assert_eq!(t.marks.len(), 1);
    core.settle().await;
    assert_eq!(llm.erased_slots(), [0, 1], "every slot is asked once");
    core.shutdown().await;
}

/// 見本の後に計画が変わっていたら、実行せずに新しい見本を返す。選択を変えた見本は別の計画になる。応答を生成して
/// いる間は、忘れるを受け付けない。どちらも記録を変えない。
///
/// verifies: 04 §7.1 (見本と計画、受け付けないとき)
#[tokio::test(flavor = "multi_thread")]
async fn an_outdated_plan_or_a_running_reply_stops_the_forget() {
    let dir = TempDir::new("forget-refused");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    let llm = chat(&[reply(KEYS), HANG.to_owned()]);
    let (core, mut rx, _) = start(root, &llm, 8, clock_at(T0 + 3_600_000)).await;
    core.hold(true);
    let session = reopen(&core, &mut rx).await;
    let preview = preview_of(&core, &mut rx, &key, ForgetChoices::default()).await;
    let kept_options = ForgetChoices {
        redact: false,
        ..ForgetChoices::default()
    };
    let kept = preview_of(&core, &mut rx, &key, kept_options).await;
    assert_eq!(kept.options, kept_options);
    assert_ne!(
        kept.plan, preview.plan,
        "the choices are covered by the plan"
    );
    assert!(kept.samples[0].after.contains("[非表示]"), "{kept:?}");

    let stale = ForgetPlanHash::new("0".repeat(64));
    let ev = forget_with(&core, &mut rx, &key, ForgetChoices::default(), stale).await;
    match ev.last() {
        Some(Event::ForgetRejected {
            error,
            preview: Some(fresh),
            list: None,
        }) => {
            assert_eq!(error.key, MessageKey::ForgetOutdated);
            assert_eq!(fresh.plan, preview.plan);
            assert_eq!(fresh.samples, preview.samples);
        }
        other => panic!("{other:?}"),
    }

    core.command(Command::Send {
        text: "続けて".into(),
    });
    until(&mut rx, |e| matches!(e, Event::Token { .. })).await;
    let hash = preview.plan.clone().unwrap();
    let ev = forget_with(&core, &mut rx, &key, ForgetChoices::default(), hash).await;
    match ev.last() {
        Some(Event::ForgetRejected {
            error,
            preview: None,
            list: None,
        }) => assert_eq!(error.key, MessageKey::ForgetBusy),
        other => panic!("{other:?}"),
    }
    core.command(Command::Interrupt);
    until(&mut rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    assert!(value_key_of(&facts(&core, &mut rx).await, VALUE).is_some());
    assert!(
        core.turns(session)
            .iter()
            .any(|t| t.id == turn && t.text.contains(VALUE))
    );
    assert!(llm.erased_slots().is_empty());
    core.shutdown().await;
}

/// `/forget` の命令で伏せ字にして忘れた後は、値が、次のターンの llama-server への要求 (偽の llama-server で記録
/// する)・ジャーナル・索引・退避と、再起動 (compact) の後のデータ根のどのファイルにも出ない。
///
/// verifies: AC-FR-307-1、04 §7.2
#[tokio::test(flavor = "multi_thread")]
async fn after_the_forget_command_the_value_is_gone_from_requests_and_files() {
    let dir = TempDir::new("forget-gone");
    let root = dir.path();
    let (turn, key) = conversation(root, clock_at(T0)).await;
    // 起動の裏の処理 (前の往復のノートの鍵) → 次のターンの応答 → その後の裏の処理 (抽出・ノートの鍵)。
    let llm = chat(&[
        reply(KEYS),
        reply(REPLY),
        reply(r#"{"c": []}"#),
        reply(KEYS),
        reply(KEYS),
    ]);
    let (core, mut rx, _) = start(root, &llm, 8, clock_at(T0 + 3_600_000)).await;
    core.hold(true);
    reopen(&core, &mut rx).await;
    let preview = preview_of(&core, &mut rx, &key, ForgetChoices::default()).await;
    let ev = forget_with(
        &core,
        &mut rx,
        &key,
        ForgetChoices::default(),
        preview.plan.clone().unwrap(),
    )
    .await;
    assert!(
        matches!(ev.last(), Some(Event::FactForgotten { .. })),
        "{ev:?}"
    );
    // 忘れる前の要求 (起動の裏の処理) は数えない。
    let (from, from_warm) = (llm.requests().len(), llm.warm_requests().len());
    core.settle().await;
    for sub in [
        root.join(evorag_store::JOURNAL_DIR),
        root.join("index"),
        root.join("backups"),
    ] {
        let hits = files_with(&sub, VALUE);
        assert!(hits.is_empty(), "{hits:?}");
    }

    say(&core, &mut rx, ASK).await;
    let sent: Vec<String> = llm.requests()[from..]
        .iter()
        .chain(llm.warm_requests()[from_warm..].iter())
        .flat_map(|r| r.messages.iter().map(|m| m.content.clone()))
        .collect();
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|m| !m.contains(VALUE)), "{sent:?}");
    assert!(
        sent.iter().any(|m| m.contains(MARKER)),
        "the redacted utterance stays in the history"
    );
    core.shutdown().await;

    // 次の起動で compact する。データ根のどのファイルにも値が残らない。
    let (core, _rx, _) = start(root, &chat(&[]), 9, clock_at(T0 + 2 * 3_600_000)).await;
    core.shutdown().await;
    let hits = files_with(root, VALUE);
    assert!(hits.is_empty(), "{hits:?}");
    let c = Closed::open(root);
    assert!(c.turn(turn).0.text.contains(MARKER));
}
