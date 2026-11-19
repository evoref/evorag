//! 埋め込みとベクトルの索引 (05 §2.2、04 §4 の手順 1、FR-410)。llama-server は記録の再生で置き換える。
//! 再生の埋め込みは、文字の bi-gram から作った長さ 1 のベクトル (決定的)。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::power::Power;
use evorag_core::rebind::DataRootModels;
use evorag_core::{Core, CoreConfig, OnBattery, PromptLanguage, SleepConfig};
use evorag_llm::replay::ReplayClient;
use evorag_llm::{Generation, LlmClient};
use evorag_memory::{EmbeddingBody, ModelOrigin};
use evorag_store::Read;
use evorag_store::testutil::TempDir;
use evorag_types::api::{Command, Event};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// `size` を変えると別の埋め込みのモデルになる。
fn embedder(size: u64) -> Arc<ReplayClient> {
    Arc::new(
        ReplayClient::from_json(&format!(
            r#"{{
              "props": {{"build_info": "b1-test", "model_path": "/m/emb-{size}.gguf", "total_slots": 4,
                         "default_generation_settings": {{"n_ctx": 8192}}}},
              "models": {{"data": [{{"id": "emb", "meta": {{"n_params": 5, "size": {size}, "n_vocab": 7}}}}]}},
              "embed": {{"dim": 8}}
            }}"#
        ))
        .unwrap(),
    )
}

fn key(size: u64) -> String {
    let meta = evorag_llm::ModelMeta {
        n_params: Some(5),
        size: Some(size),
        n_vocab: Some(7),
        n_ctx_train: None,
    };
    evorag_llm::model_key(&format!("/m/emb-{size}.gguf"), &meta, "emb")
}

fn origin(size: u64) -> ModelOrigin {
    ModelOrigin {
        model_key: key(size),
        file_name: format!("emb-{size}.gguf"),
        model_name: "emb".into(),
        n_params: Some(5),
        size: Some(size),
        n_vocab: Some(7),
        build_info: "b1-test".into(),
    }
}

fn on_ac() -> Power {
    Power::Ac
}

fn on_battery() -> Power {
    Power::Battery
}

fn config(
    root: &Path,
    seed: u64,
    emb: Arc<ReplayClient>,
    sleep: SleepConfig,
    power: fn() -> Power,
) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    CoreConfig {
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
        probe: evorag_core::Probe {
            power,
            ..evorag_core::Probe::system()
        },
        model_name: "test".into(),
        ignored_config: Vec::new(),
        embed: Some(emb as Arc<dyn LlmClient>),
        rerank: None,
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep,
        memory_enabled: true,
        precompute: true,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
    }
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        if pred(&ev) {
            return;
        }
    }
}

/// セッションを 1 つ作り、発話を送る。
async fn talk(core: &Core, rx: &mut UnboundedReceiver<Event>, texts: &[&str]) {
    core.command(Command::NewSession {
        title: "試験".into(),
    });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    for t in texts {
        core.command(Command::Send { text: (*t).into() });
        until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
        core.settle().await;
    }
}

fn store(root: &Path) -> evorag_store::Store {
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

fn count(root: &Path, kind: &str) -> usize {
    let s = store(root);
    let r = s.read().unwrap();
    let mut n = 0;
    r.for_each(kind, |x| {
        n += usize::from(matches!(x, Read::Ok(_)));
        true
    })
    .unwrap();
    n
}

fn embeddings(root: &Path) -> Vec<EmbeddingBody> {
    let s = store(root);
    let r = s.read().unwrap();
    evorag_memory::embeddings_of_model(&r, None)
        .unwrap()
        .into_iter()
        .map(|(_, b, v)| {
            assert_eq!(v.len(), b.dim as usize);
            b
        })
        .collect()
}

fn vec_dir(root: &Path, size: u64) -> std::path::PathBuf {
    root.join("index")
        .join("memory")
        .join(format!("vec-{}", key(size)))
}

const TALK: [&str; 3] = [
    "モンステラの水やりは、どれくらいの頻度がいい？",
    "車のオイル交換の頻度は？",
    "京都で紅葉を見るならどこ？",
];

/// verifies: AC-FR-301-1 (sleep-time の手順: 埋め込み), AC-FR-410-1 (索引を失っても DB から作り直す)
#[tokio::test(flavor = "multi_thread")]
async fn chunks_are_embedded_in_the_background_and_the_index_is_rebuilt_from_the_database() {
    let dir = TempDir::new("embed");
    let emb = embedder(2);
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 1, emb.clone(), SleepConfig::immediate(), on_ac),
        Arc::new(chat(3)),
    )
    .await
    .unwrap();
    talk(&core, &mut rx, &TALK).await;
    core.settle_background().await;
    core.shutdown().await;
    let chunks = count(dir.path(), evorag_memory::KIND_CHUNK);
    let got = embeddings(dir.path());
    // チャンクごとにちょうど 1 つ (区切りの作り直しと重なって残った、もう無いチャンクの埋め込みは、
    // 次の起動の突き合わせで消える)。
    let targets: std::collections::HashSet<_> = got.iter().map(|b| b.target).collect();
    assert_eq!(targets.len(), got.len(), "no chunk is embedded twice");
    assert!(got.len() >= chunks);
    assert!(got.iter().all(|b| b.model_key == key(2) && b.dim == 8));
    assert!(vec_dir(dir.path(), 2).join("CURRENT").is_file());
    let calls = emb.aux_calls().0;

    // ベクトルの索引を失っても、DB の記録から作り直す (埋め込み直さない)。
    std::fs::remove_dir_all(vec_dir(dir.path(), 2)).unwrap();
    let again = embedder(2);
    let (core, _, _) = Core::start(
        config(
            dir.path(),
            2,
            again.clone(),
            SleepConfig::immediate(),
            on_ac,
        ),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;
    assert_eq!(again.aux_calls().0, 0, "no re-embedding (was {calls})");
    assert_eq!(
        embeddings(dir.path()).len(),
        chunks,
        "every chunk is embedded once"
    );
    assert!(vec_dir(dir.path(), 2).join("CURRENT").is_file());
}

/// verifies: AC-FR-410-1 (setup で切り替えると全件を新しいモデルで作り直し、前のモデルの索引が 1 世代残る)
#[tokio::test(flavor = "multi_thread")]
async fn switching_the_embedding_model_rebuilds_and_keeps_one_previous_generation() {
    let dir = TempDir::new("embed-switch");
    let start = |size: u64, seed: u64, replies: usize| {
        let root = dir.path().to_path_buf();
        async move {
            Core::start(
                config(&root, seed, embedder(size), SleepConfig::immediate(), on_ac),
                Arc::new(chat(replies)),
            )
            .await
            .unwrap()
        }
    };
    let bind = |size: u64, seed: u64| {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
        let m = DataRootModels::open(
            dir.path(),
            "0.0.6",
            clock.clone(),
            Arc::new(UuidV7Gen::with_seed(clock, seed)),
        )
        .unwrap();
        m.bind("embed", origin(size)).unwrap();
    };
    let (core, mut rx, _) = start(2, 1, 2).await;
    talk(&core, &mut rx, &TALK[..2]).await;
    core.settle_background().await;
    core.shutdown().await;
    let n = embeddings(dir.path()).len();

    for (size, seed) in [(5u64, 3u64), (9, 5)] {
        bind(size, seed);
        let (core, _, _) = start(size, seed + 1, 0).await;
        core.settle().await;
        core.settle_background().await;
        core.shutdown().await;
        let of_new = embeddings(dir.path())
            .into_iter()
            .filter(|b| b.model_key == key(size))
            .count();
        assert_eq!(of_new, n, "all chunks re-embedded with model {size}");
    }
    // 今 (9) と前 (5) の世代だけが残る。
    assert!(vec_dir(dir.path(), 9).is_dir() && vec_dir(dir.path(), 5).is_dir());
    assert!(!vec_dir(dir.path(), 2).exists());
    assert!(embeddings(dir.path()).iter().all(|b| b.model_key != key(2)));
}

/// verifies: AC-FR-301-2 (電池で `reduce` なら、sleep-time の手順は操作の無い時間の 5 倍まで待つ)
#[tokio::test(flavor = "multi_thread")]
async fn on_battery_reduce_delays_the_embedding() {
    let dir = TempDir::new("embed-battery");
    let emb = embedder(2);
    let sleep = SleepConfig {
        enabled: true,
        idle: Duration::from_millis(300),
        on_battery: OnBattery::Reduce,
    };
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 1, emb.clone(), sleep, on_battery),
        Arc::new(chat(1)),
    )
    .await
    .unwrap();
    // 発話を送って応答を待つ間は、裏の処理を止めておく (遅い機械では、応答と後片付けだけで 5 倍の
    // 1.5 秒を超え、時間を測り始める前に埋め込みが始まることがあった)。止めを解くと操作として数える。
    core.hold(true);
    talk(&core, &mut rx, &TALK[..1]).await;
    core.hold(false);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(emb.aux_calls().0, 0, "not before 5 x idle");
    // 5 倍 (1.5 秒) を過ぎたら始まる (見直しは 1 秒ごとなので、締切つきで待つ)。
    for _ in 0..100 {
        if emb.aux_calls().0 > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(emb.aux_calls().0 > 0);
    core.shutdown().await;
}

/// 「拒否」を含む文書を断る埋め込みのサーバ (1 件ずつ送り直すと、それだけが断られる)。
struct Picky(Arc<ReplayClient>);

impl LlmClient for Picky {
    fn health(&self) -> evorag_llm::BoxFuture<'_, Result<(), evorag_llm::LlmError>> {
        self.0.health()
    }
    fn server_info(
        &self,
    ) -> evorag_llm::BoxFuture<'_, Result<evorag_llm::ServerInfo, evorag_llm::LlmError>> {
        self.0.server_info()
    }
    fn count_tokens<'a>(
        &'a self,
        text: &'a str,
    ) -> evorag_llm::BoxFuture<'a, Result<usize, evorag_llm::LlmError>> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        req: evorag_llm::ChatRequest,
    ) -> evorag_llm::BoxFuture<'_, Result<Box<dyn evorag_llm::ChatStream>, evorag_llm::LlmError>>
    {
        self.0.chat(req)
    }
    fn embed<'a>(
        &'a self,
        inputs: &'a [String],
    ) -> evorag_llm::BoxFuture<'a, Result<Vec<Vec<f32>>, evorag_llm::LlmError>> {
        if inputs.iter().any(|t| t.contains("拒否")) {
            return Box::pin(async {
                Err(evorag_llm::LlmError::Http {
                    status: 500,
                    body: "too long".into(),
                })
            });
        }
        self.0.embed(inputs)
    }
}

/// verifies: AC-FR-301-1 (埋め込みのサーバに断られるチャンクは飛ばし、後ろのチャンクを止めない)
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_chunk_does_not_block_the_others() {
    let dir = TempDir::new("embed-refused");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let mut cfg = config(dir.path(), 1, embedder(2), SleepConfig::immediate(), on_ac);
    cfg.embed = Some(Arc::new(Picky(embedder(2))) as Arc<dyn LlmClient>);
    cfg.clock = clock.clone();
    let (core, mut rx, _) = Core::start(cfg, Arc::new(chat(3))).await.unwrap();
    talk(&core, &mut rx, &["拒否される文書です", TALK[0], TALK[1]]).await;
    core.settle_background().await;
    core.shutdown().await;
    let chunks = count(dir.path(), evorag_memory::KIND_CHUNK);
    // 断られた 1 件 (利用者の発話のチャンク) だけが埋め込まれない。
    assert_eq!(embeddings(dir.path()).len(), chunks - 1);
}

/// verifies: AC-FR-309-1, AC-FR-309-2 (会話を消すと、前の世代のベクトルの索引からも、その行が消える)
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_also_cleans_the_previous_generation() {
    let dir = TempDir::new("embed-delete-prev");
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 1, embedder(2), SleepConfig::immediate(), on_ac),
        Arc::new(chat(1)),
    )
    .await
    .unwrap();
    talk(&core, &mut rx, &TALK[..1]).await;
    core.settle_background().await;
    core.shutdown().await;
    // 埋め込みのモデルを切り替える (前の世代として vec-<2> が残る)。
    {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
        let m = DataRootModels::open(
            dir.path(),
            "0.0.6",
            clock.clone(),
            Arc::new(UuidV7Gen::with_seed(clock, 3)),
        )
        .unwrap();
        m.bind("embed", origin(5)).unwrap();
    }
    let (core, _rx, _) = Core::start(
        config(dir.path(), 4, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;
    let prev = evorag_index::vec::VecIndex::open_existing(&vec_dir(dir.path(), 2)).unwrap();
    let rows_before = prev.snapshot().len();
    assert!(rows_before > 0);
    drop(prev);
    // 最初のセッションを開いて消す。
    let first = first_session(dir.path());
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 6, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.command(Command::OpenSession { id: first });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.shutdown().await;
    let prev = evorag_index::vec::VecIndex::open_existing(&vec_dir(dir.path(), 2)).unwrap();
    assert_eq!(
        prev.snapshot().len(),
        0,
        "the previous generation is cleaned too"
    );
}

/// 最初に作ったセッションの ID。
fn first_session(root: &Path) -> evorag_types::Id {
    let s = store(root);
    let r = s.read().unwrap();
    let mut ids = Vec::new();
    r.for_each(evorag_memory::KIND_SESSION, |x| {
        if let Read::Ok(e) = x {
            ids.push(e.id);
        }
        true
    })
    .unwrap();
    ids.sort();
    ids[0]
}

/// verifies: AC-FR-309-1, AC-FR-309-2 (会話を消すと、そのチャンクの埋め込みとベクトルも消える)
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_removes_its_embeddings() {
    let dir = TempDir::new("embed-delete");
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 1, embedder(2), SleepConfig::immediate(), on_ac),
        Arc::new(chat(1)),
    )
    .await
    .unwrap();
    talk(&core, &mut rx, &TALK[..1]).await;
    core.settle_background().await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.shutdown().await;
    assert!(embeddings(dir.path()).is_empty());
    let s = store(dir.path());
    let r = s.read().unwrap();
    let mut blobs = 0;
    r.for_each(evorag_memory::KIND_EMBEDDING, |_| {
        blobs += 1;
        true
    })
    .unwrap();
    assert_eq!(blobs, 0);
}

/// 前の世代 (vec-<2>) と今の世代 (vec-<5>) のベクトルの索引に行がある状態を作る。最初のセッションの ID を返す。
async fn two_generations(root: &Path) -> evorag_types::Id {
    let (core, mut rx, _) = Core::start(
        config(root, 1, embedder(2), SleepConfig::immediate(), on_ac),
        Arc::new(chat(1)),
    )
    .await
    .unwrap();
    talk(&core, &mut rx, &TALK[..1]).await;
    core.settle_background().await;
    core.shutdown().await;
    {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_660_000_000)));
        let m = DataRootModels::open(
            root,
            "0.0.6",
            clock.clone(),
            Arc::new(UuidV7Gen::with_seed(clock, 3)),
        )
        .unwrap();
        m.bind("embed", origin(5)).unwrap();
    }
    let (core, _rx, _) = Core::start(
        config(root, 4, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;
    for size in [2, 5] {
        let ix = evorag_index::vec::VecIndex::open_existing(&vec_dir(root, size)).unwrap();
        assert!(!ix.snapshot().is_empty(), "generation {size} has rows");
    }
    first_session(root)
}

fn rows(root: &Path, size: u64) -> usize {
    evorag_index::vec::VecIndex::open_existing(&vec_dir(root, size))
        .map_or(0, |ix| ix.snapshot().len())
}

fn cleanup_jobs(root: &Path) -> usize {
    let s = store(root);
    let r = s.read().unwrap();
    evorag_memory::pending_purge_jobs(&r).unwrap().len()
}

/// 削除をコミットした直後 (索引から除く前) に落ちても、次の起動で、job から今と前の世代の索引の行を除く。
/// verifies: AC-FR-309-2 (前の世代の索引の分を含む)、04 §7.7 の 2 (除く ID を job に残し、起動時に続ける)
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_the_indexes_are_cleaned_is_finished_on_the_next_start() {
    let dir = TempDir::new("embed-delete-crash");
    let first = two_generations(dir.path()).await;
    // 削除のコミット (job を同じ依頼で書く) だけを行い、索引には触れずに終わる (強制終了をまねる)。
    {
        let store = Arc::new(store(dir.path()));
        let (writer, h) = evorag_store::Writer::new(store.clone()).unwrap();
        let t = std::thread::spawn(move || writer.run());
        let w = evorag_memory::Writes {
            clock: &**store.clock(),
            ids: &**store.ids(),
        };
        let mut ops = {
            let r = store.read().unwrap();
            let s = evorag_memory::load_session(&r, first).unwrap();
            evorag_memory::delete_session(&r, &s.envelope, &w).unwrap()
        };
        let ids = evorag_memory::deleted_ids(&ops);
        ops.push(evorag_memory::purge_job(&w, ids).unwrap().1);
        h.submit_wait(evorag_store::WriteRequest::conversation(ops))
            .unwrap();
        h.shutdown();
        t.join().unwrap();
    }
    assert!(rows(dir.path(), 2) > 0, "the crash left the previous rows");

    let (core, _rx, _) = Core::start(
        config(dir.path(), 6, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;
    assert_eq!(rows(dir.path(), 2), 0, "the previous generation is cleaned");
    assert_eq!(rows(dir.path(), 5), 0, "the current generation is cleaned");
    assert_eq!(cleanup_jobs(dir.path()), 0, "the job is done");
}

/// 整理の job が済むまで、裏の処理を試し直させながら待つ (遅い CI の機械では、1 回の `settle_background` の
/// 間に sleep-time の整理の手順まで回らないことがあるため)。最後に残っていた job の数を返す。
async fn cleanup_finished(core: &Core) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        core.settle_background().await;
        let left = core.pending_cleanup_jobs();
        if left == 0 || std::time::Instant::now() > deadline {
            return left;
        }
        core.retry_background_now();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 索引から除けなかったら、job を残して sleep-time に試し直す (終わるまで job の記録として `doctor` に出る)。
/// verifies: AC-FR-309-2 (前の世代の索引の分を含む)、04 §7.7 の 2 (除けなかった分は job から再試行する)
#[tokio::test(flavor = "multi_thread")]
async fn an_index_removal_that_fails_is_retried_from_the_job() {
    let dir = TempDir::new("embed-delete-retry");
    let first = two_generations(dir.path()).await;
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 6, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.inject_index_removal_failure(usize::MAX);
    core.command(Command::OpenSession { id: first });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.settle_background().await;
    assert_eq!(core.pending_cleanup_jobs(), 1, "the job is kept");

    // 除けるようになったら、sleep-time の手順が job から続けて終える。
    core.inject_index_removal_failure(0);
    core.retry_background_now();
    assert_eq!(cleanup_finished(&core).await, 0, "the job is done");
    core.shutdown().await;
    assert_eq!(rows(dir.path(), 2), 0, "the previous generation is cleaned");
    assert_eq!(rows(dir.path(), 5), 0, "the current generation is cleaned");
}

/// 整理の手順は、利用者の操作で sleep-time の手順が落とされても途中で止まらず、job を残したまま旗を下ろさない。
/// verifies: 04 §7.7 の 2 (除けなかった分は job から再試行する)
#[tokio::test(flavor = "multi_thread")]
async fn a_cleanup_interrupted_by_input_still_finishes() {
    let dir = TempDir::new("embed-cleanup-touch");
    let first = two_generations(dir.path()).await;
    let (core, mut rx, _) = Core::start(
        config(dir.path(), 6, embedder(5), SleepConfig::immediate(), on_ac),
        Arc::new(chat(0)),
    )
    .await
    .unwrap();
    core.settle().await;
    core.inject_index_removal_failure(usize::MAX);
    core.command(Command::OpenSession { id: first });
    until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.settle_background().await;
    assert_eq!(core.pending_cleanup_jobs(), 1);

    // 試し直しを始めさせ、その間も操作を続ける。
    core.inject_index_removal_failure(0);
    core.retry_background_now();
    for _ in 0..200 {
        core.touch();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(cleanup_finished(&core).await, 0, "the job is done");
    core.shutdown().await;
    assert_eq!(rows(dir.path(), 2), 0);
}
