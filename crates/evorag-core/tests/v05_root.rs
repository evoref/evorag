//! 製品の経路は v0.5 の根だけを開く (03 §12.4、08 §2.6 の M5.1 の「ステップの分け方」)。
//!
//! - 新しいデータ根は v0.5 の形式 (`0.5`) で作る。
//! - v0.4 の根 (形式の版 `0.3`) は在り場所で移行せず、何も書かずに `NeedsImport` で断る。取り込んだ後は、新しい根の
//!   ロックを持ったまま新しい根で続ける (TUI の起動の「y」の経路)。
//! - v0.5 の根では、旧エンジンの裏の処理 (ノート・抽出・訂正の検証・事実の表) を動かさず、`fact`・`note`・
//!   `correction`・`fact_directive` を書かない。事実への操作は断る。会話と想起の索引 (チャンク) は v0.4 のまま動く。
//! - 取り込んだ伏せ字にしない忘れるの `[非表示]` の範囲と private の値は、v0.5 の根でも想起で伏せ続ける。
//! - 正しく閉じていない・DB の無い・壊れた v0.4 の根は、修復も作り直しもせずに断る。v0.5 の根はジャーナルに印を書く。
//!
//! 見本 `tests/fixtures/db/0.4.0/root` を一時のフォルダに写して使う。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evorag_core::import_v04::{Finished, ImportOptions, import_and_swap};
use evorag_core::{Core, CoreConfig, PromptLanguage, RootFormat, StartError};
use evorag_llm::Generation;
use evorag_llm::replay::ReplayClient;
use evorag_memory::{
    KIND_CHUNK, KIND_CORRECTION, KIND_FACT, KIND_FACT_DIRECTIVE, KIND_NOTE, KIND_TURN, TurnBody,
    TurnForgets, redaction_marker, turn_rev,
};
use evorag_store::location::Verdict;
use evorag_store::testutil::TempDir;
use evorag_store::{OpenError, OpenOptions, Read, Store, meta};
use evorag_types::api::{
    Command, Event, FactKey, FactStatus, FactSubject, FactTense, FactsUnavailable, ForgetOptions,
    PrivateScope,
};
use evorag_types::{Id, ManualClock, MessageKey, Timestamp, UuidV7Gen};
use tokio::sync::mpsc::UnboundedReceiver;

const NOW: i64 = 1_795_000_000_000;

/// 伏せ字にしないで忘れた「将棋」の出典のターン (見本の README の「伏せ字にしない忘れる」)。
const HIDDEN_TURN: &str = "01a1dc95-1440-7000-aa3f-7fb9fcd4241d";

fn fixture_root() -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/db/0.4.0/root")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for e in std::fs::read_dir(from).expect("read the fixture") {
        let e = e.expect("an entry");
        let target = to.join(e.file_name());
        if e.file_type().expect("a file type").is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy a file");
        }
    }
}

/// 見本を `<一時>/evorag` に写す。
fn copied_fixture(label: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new(label);
    let root = tmp.path().join("evorag");
    copy_dir(&fixture_root(), &root);
    (tmp, root)
}

/// データ根のファイルの (大きさ・中身)。ロックのファイルは除く (03 §12.4 の「読み方」)。
fn files(root: &Path) -> BTreeMap<String, (u64, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).expect("read a directory") {
            let path = e.expect("an entry").path();
            if path.is_dir() {
                walk(base, &path, out);
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .expect("under the base")
                .to_string_lossy()
                .replace('\\', "/");
            if ["lock", "lock.pid", "lock.pid.tmp"].contains(&rel.as_str()) {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            out.insert(rel, (bytes.len() as u64, bytes));
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// 見本の根の chat の記録と同じモデルの llama-server (応答を `replies` 回返す)。
fn cassette(replies: usize) -> ReplayClient {
    let chat = r#"{"events": [{"delta": "了解です"}, {"done": {}}]}"#;
    ReplayClient::from_json(&format!(
        r#"{{
          "props": {{"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                     "default_generation_settings": {{"n_ctx": 8192}}}},
          "models": {{"data": [{{"id": "test", "meta": {{"n_params": 1, "size": 2, "n_vocab": 3}}}}]}},
          "chats": [{}]
        }}"#,
        vec![chat; replies].join(", ")
    ))
    .expect("a cassette")
}

fn config(root: &Path, root_format: RootFormat) -> CoreConfig {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    CoreConfig {
        data_root: root.to_path_buf(),
        root_format,
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
        aux_request_extra: Default::default(),
        probe_models: false,
        sleep: evorag_core::SleepConfig::immediate(),
        memory_enabled: true,
        precompute: false,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 11)),
    }
}

/// 終了の後に、v0.5 の根を開く。
fn open_v05(root: &Path) -> Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let (kinds, plan) = evorag_memory::v05_root();
    Store::open_with(
        OpenOptions {
            data_root: root.to_path_buf(),
            evorag_version: "0.5.0".into(),
            kinds,
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 12)),
            config_file: None,
        },
        plan,
    )
    .expect("open the v0.5 root")
    .0
}

fn count(store: &Store, kind: &str) -> usize {
    let r = store.read().expect("read");
    let mut n = 0;
    r.for_each(kind, |_| {
        n += 1;
        true
    })
    .expect("for each");
    n
}

async fn until(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        if pred(&ev) {
            return ev;
        }
    }
}

async fn new_session(core: &Core, rx: &mut UnboundedReceiver<Event>, title: &str) {
    core.command(Command::NewSession {
        title: title.into(),
    });
    until(rx, |e| matches!(e, Event::SessionOpened { .. })).await;
}

async fn say(core: &Core, rx: &mut UnboundedReceiver<Event>, text: &str) {
    core.command(Command::Send { text: text.into() });
    until(rx, |e| matches!(e, Event::ResponseFinished { .. })).await;
    core.settle().await;
}

/// 事実への操作の相手 (v0.5 の根には事実が無いので、どの行でもよい)。
fn some_fact() -> FactKey {
    FactKey {
        subject: FactSubject::User,
        attribute: "hobby".into(),
        vk: "0".repeat(64),
        status: FactStatus::Live,
        tense: FactTense::Current,
        fact: None,
    }
}

/// 新しいデータ根は v0.5 の形式で作る (`format.version`・`min_read`・`min_write` が `0.5`)。忘れるの鍵も作る。
///
/// verifies: AC-FR-219-1 (新しい根の形式の版は `0.5`)
#[tokio::test(flavor = "multi_thread")]
async fn a_new_data_root_is_created_in_the_v05_format() {
    let dir = TempDir::new("v05-new-root");
    let root = dir.path().join("data");
    let (core, _rx, _) = Core::start(config(&root, RootFormat::V05), Arc::new(cassette(0)))
        .await
        .expect("start");
    core.settle().await;
    core.shutdown().await;
    let m = evorag_store::inspect::inspect(&root, evorag_memory::kinds_v05(), None)
        .expect("inspect")
        .meta;
    for key in [
        meta::FORMAT_VERSION,
        meta::FORMAT_MIN_READ,
        meta::FORMAT_MIN_WRITE,
    ] {
        assert_eq!(m.get(key).map(String::as_str), Some("0.5"), "{key}");
    }
    assert!(m.contains_key(meta::FORGET_KEY), "{m:?}");
    assert!(open_v05(&root).is_v05_root());
}

/// v0.4 の根は、在り場所で移行せずに `NeedsImport` で断り、何も書かない (ロックのファイルを除く)。
#[tokio::test(flavor = "multi_thread")]
async fn a_v04_root_is_refused_without_writing_anything() {
    let (_tmp, root) = copied_fixture("v05-refuse-v04");
    let before = files(&root);
    let started = Core::start(config(&root, RootFormat::V05), Arc::new(cassette(0))).await;
    match started {
        Err(StartError::Store(OpenError::NeedsImport { format })) => assert_eq!(format, "0.3"),
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("a v0.4 root must not be opened"),
    }
    assert_eq!(before, files(&root), "the v0.4 root is unchanged");
    assert!(
        !root.with_file_name("evorag.v05-import").exists(),
        "no new root is made"
    );
}

/// 取り込んだ後 (TUI の起動の確認で y)、新しい根のロックを渡したまま新しい根で続ける。会話はそのまま見え、事実の一覧は
/// 開発中の版であることを示して空。取り込んだ伏せ字にしない忘れるの `[非表示]` の範囲は、v0.5 の根でも当たる。
///
/// verifies: 03 §12.4 の「きっかけ」(y なら同じ手順を行い、終えたら新しい根で続ける)
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_root_is_continued_with_the_lock_handed_over() {
    let (_tmp, old) = copied_fixture("v05-continue");
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let opts = ImportOptions {
        old_root: old.clone(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
        before_verify: None,
        swap_hooks: Default::default(),
    };
    let Finished {
        root,
        renamed_old_root,
        new_root_lock,
        old_root_lock,
        ..
    } = import_and_swap(&opts, &mut |_| true).expect("import");
    drop(old_root_lock);
    assert_eq!(root, old);
    assert!(renamed_old_root.is_dir(), "the old root is kept");

    let (core, mut rx, _) = Core::start_locked(
        config(&root, RootFormat::V05),
        Arc::new(cassette(0)),
        Some(new_root_lock),
    )
    .await
    .expect("start on the new root with the handed-over lock");
    core.settle().await;
    let titles: Vec<String> = core.sessions(50).into_iter().map(|s| s.title).collect();
    assert!(titles.iter().any(|t| t == "暮らしの話"), "{titles:?}");
    core.command(Command::ShowFacts);
    match until(&mut rx, |e| matches!(e, Event::FactsInfo { .. })).await {
        Event::FactsInfo { list } => {
            assert!(list.groups.is_empty(), "{list:?}");
            assert_eq!(list.note, Some(FactsUnavailable::DevelopmentBuild));
        }
        _ => unreachable!(),
    }
    core.shutdown().await;

    let store = open_v05(&root);
    assert_eq!(count(&store, KIND_FACT), 0);
    assert_eq!(count(&store, KIND_FACT_DIRECTIVE), 0);
    let r = store.read().expect("read");
    let forgets = TurnForgets::load(&r).expect("load the forgets");
    let turn: Id = HIDDEN_TURN.parse().expect("an id");
    let Some(Read::Ok(env)) = r.get(KIND_TURN, turn).expect("get") else {
        panic!("the turn is imported");
    };
    let body: TurnBody = env.body_as().expect("a turn");
    let hidden = forgets
        .hidden_ranges(turn, &body.text, turn_rev(&env), redaction_marker("ja"))
        .expect("the turn matches its forget directives");
    let values: Vec<&str> = hidden.iter().map(|r| &body.text[r.clone()]).collect();
    assert_eq!(values, ["将棋"], "{}", body.text);
}

/// v0.5 の根では、旧エンジンの裏の処理を動かさない (補助の呼び出しを送らない)。`fact`・`note`・`correction`・
/// `fact_directive` を書かない (セッションの削除でも)。事実への操作 (private・忘れる) は、開発中の版であることを
/// 示して断る。会話のチャンク (想起の索引) は作る。
#[tokio::test(flavor = "multi_thread")]
async fn the_old_engine_does_not_run_on_a_v05_root() {
    let dir = TempDir::new("v05-no-old-engine");
    let root = dir.path().join("data");
    let llm = cassette(3);
    let (core, mut rx, _) = Core::start(config(&root, RootFormat::V05), Arc::new(llm.clone()))
        .await
        .expect("start");
    core.settle().await;
    new_session(&core, &mut rx, "暮らし").await;
    say(&core, &mut rx, "私は大阪に住んでいて、趣味は将棋です。").await;
    say(&core, &mut rx, "猫を飼っています。").await;
    core.settle_background().await;

    core.command(Command::SetFactPrivate {
        target: some_fact(),
        scope: PrivateScope::Successors,
    });
    match until(&mut rx, |e| matches!(e, Event::FactOpRejected { .. })).await {
        Event::FactOpRejected { error, .. } => {
            assert_eq!(error.key, MessageKey::FactsDevelopmentBuild);
        }
        _ => unreachable!(),
    }
    core.command(Command::PreviewForget {
        target: some_fact(),
        options: ForgetOptions::default(),
    });
    match until(&mut rx, |e| matches!(e, Event::ForgetRejected { .. })).await {
        Event::ForgetRejected { error, .. } => {
            assert_eq!(error.key, MessageKey::FactsDevelopmentBuild);
        }
        _ => unreachable!(),
    }

    new_session(&core, &mut rx, "消す").await;
    say(&core, &mut rx, "これは消すセッションです。").await;
    core.command(Command::DeleteSession);
    until(&mut rx, |e| matches!(e, Event::SessionDeleted { .. })).await;
    core.settle().await;
    core.settle_background().await;
    core.shutdown().await;

    // 送ったのは 3 つの応答の要求だけ (ノート・抽出・訂正の検証の補助の呼び出しは無い)。
    assert_eq!(llm.requests().len(), 3, "only the replies are requested");
    let store = open_v05(&root);
    for kind in [KIND_FACT, KIND_NOTE, KIND_CORRECTION, KIND_FACT_DIRECTIVE] {
        assert_eq!(count(&store, kind), 0, "{kind}");
    }
    assert_eq!(
        count(&store, KIND_TURN),
        4,
        "the deleted session's turns are gone"
    );
    assert!(count(&store, KIND_CHUNK) > 0, "recall chunks are made");
}

/// 見本を写して取り込み、新しい根のロックを返す。
fn imported_fixture(label: &str) -> (TempDir, PathBuf, evorag_store::lock::DataRootLock) {
    let (tmp, old) = copied_fixture(label);
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let opts = ImportOptions {
        old_root: old.clone(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
        before_verify: None,
        swap_hooks: Default::default(),
    };
    let done = import_and_swap(&opts, &mut |_| true).expect("import");
    (tmp, done.root, done.new_root_lock)
}

/// LLM への要求の、今の発話の側 (想起の引用の枠を含む、05 §4.1)。
fn user_side(messages: &[evorag_llm::ChatMessage]) -> String {
    messages
        .iter()
        .filter(|m| m.role == evorag_llm::Role::User)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 正しく閉じていない・DB の無い・空の・壊れた v0.4 の根は、v0.5 の形式で開くときに、修復も作り直しも `broken/` へ
/// 移すこともせずに断る。ロックのファイルを除くすべてのファイルのバイト列が変わらない。
///
/// verifies: 03 §8 (v0.4 のデータ根を v0.5 で開いた)
#[tokio::test(flavor = "multi_thread")]
async fn unclean_or_damaged_v04_roots_are_refused_without_writing_anything() {
    type Damage = fn(&Path);
    let cases: [(&str, Damage, Option<&str>); 4] = [
        (
            "unclean",
            |r| {
                // redb の修復が要る印 (ファイルの頭の god byte、redb 4 の header.rs)。
                let p = r.join("evorag.redb");
                let mut b = std::fs::read(&p).expect("read");
                b[9] |= 2;
                std::fs::write(&p, b).expect("write");
            },
            Some("0.3"),
        ),
        (
            "no-db",
            |r| std::fs::remove_file(r.join("evorag.redb")).expect("remove"),
            None,
        ),
        (
            "empty-db",
            |r| std::fs::write(r.join("evorag.redb"), b"").expect("write"),
            None,
        ),
        (
            "broken-db",
            |r| std::fs::write(r.join("evorag.redb"), b"garbage").expect("write"),
            None,
        ),
    ];
    for (label, damage, want) in cases {
        let (_tmp, root) = copied_fixture(&format!("v05-refuse-{label}"));
        damage(&root);
        let before = files(&root);
        let started = Core::start(config(&root, RootFormat::V05), Arc::new(cassette(0))).await;
        match started {
            Err(StartError::Store(OpenError::NeedsV04Open { format })) => {
                assert_eq!(format.as_deref(), want, "{label}");
            }
            Err(e) => panic!("{label}: unexpected error: {e}"),
            Ok(_) => panic!("{label}: a damaged v0.4 root must not be opened"),
        }
        assert_eq!(before, files(&root), "{label}: the v0.4 root is unchanged");
        assert!(!root.join("broken").exists(), "{label}");
    }
}

/// v0.5 の根は、作るときにジャーナルへ v0.5 の根の印 (`format.version` の `meta` の行) を書く。DB を失っても、
/// ジャーナルから v0.5 の根として作り直せる。
///
/// verifies: 03 §6 (`format.version` の `meta` の行)
#[tokio::test(flavor = "multi_thread")]
async fn a_v05_root_marks_its_journal_and_rebuilds_a_lost_database() {
    let dir = TempDir::new("v05-journal-mark");
    let root = dir.path().join("data");
    let (core, mut rx, _) = Core::start(config(&root, RootFormat::V05), Arc::new(cassette(1)))
        .await
        .expect("start");
    core.settle().await;
    new_session(&core, &mut rx, "印の確かめ").await;
    say(&core, &mut rx, "こんにちは").await;
    core.shutdown().await;
    let journal: String = std::fs::read_dir(root.join("journal"))
        .expect("the journal")
        .map(|e| std::fs::read_to_string(e.expect("an entry").path()).expect("read"))
        .collect();
    let first = journal.lines().next().expect("a line");
    assert!(
        first.contains("\"format.version\"") && first.contains("\"0.5\""),
        "the first line marks the root: {first}"
    );
    assert_eq!(journal.matches("\"format.version\"").count(), 1);

    std::fs::remove_file(root.join("evorag.redb")).expect("lose the database");
    let (core, _rx, report) = Core::start(config(&root, RootFormat::V05), Arc::new(cassette(0)))
        .await
        .expect("rebuild from the journal");
    core.settle().await;
    assert!(report.store.created, "{report:?}");
    let titles: Vec<String> = core.sessions(10).into_iter().map(|s| s.title).collect();
    assert_eq!(titles, ["印の確かめ"]);
    core.shutdown().await;
    assert!(open_v05(&root).is_v05_root());
}

/// 取り込んだ private (dietary の「えび」、`value_only`) と、伏せ字にしない忘れる (hobby の「将棋」) の値は、v0.5 の根の
/// 想起で添える本文 (LLM への要求) に出ない。忘れるの `[非表示]` は、セッションの画面の印にも出る。
///
/// verifies: 04 §7.9 (private と伏せ字にしない忘れるは想起で `[非表示]`、08 §2.6 の M5.1〜M5.3 の扱い)
#[tokio::test(flavor = "multi_thread")]
async fn imported_private_and_forgotten_values_stay_hidden_in_recall_and_on_screen() {
    let (_tmp, root, lock) = imported_fixture("v05-hidden");
    let llm = cassette(2);
    let (core, mut rx, _) = Core::start_locked(
        config(&root, RootFormat::V05),
        Arc::new(llm.clone()),
        Some(lock),
    )
    .await
    .expect("start");
    core.settle().await;

    // 画面: 暮らしの話のセッションの「趣味は将棋です」は、`[非表示]` の印の範囲を持つ。
    core.command(Command::OpenSession {
        id: "01a1dc8a-17c0-7000-a698-4080bab12a02"
            .parse()
            .expect("an id"),
    });
    let Event::SessionOpened { turns, .. } =
        until(&mut rx, |e| matches!(e, Event::SessionOpened { .. })).await
    else {
        unreachable!()
    };
    let hidden_turn: Id = HIDDEN_TURN.parse().expect("an id");
    let t = turns
        .iter()
        .find(|t| t.id == hidden_turn)
        .expect("the turn is shown");
    let marked: Vec<&str> = t
        .marks
        .iter()
        .filter(|m| m.kind == evorag_types::api::TurnMarkKind::Hidden)
        .map(|m| &t.text[m.range.clone()])
        .collect();
    assert_eq!(marked, ["将棋"], "{t:?}");

    // 想起: 別のセッションから問うと、以前の往復を添えるが、値は `[非表示]` になる。
    new_session(&core, &mut rx, "別の話").await;
    say(
        &core,
        &mut rx,
        "実は食べられないんです、と前に言ったのを覚えていますか",
    )
    .await;
    say(
        &core,
        &mut rx,
        "趣味は何です、と前に言ったのを覚えていますか",
    )
    .await;
    core.shutdown().await;

    // 伏せる材料: 取り込んだ private の指示は値の文字列を持たないが、`vks` と根拠の位置で伏せる。
    {
        let store = open_v05(&root);
        let r = store.read().expect("read");
        let masker = evorag_memory::Masker::load(&r, redaction_marker("ja")).expect("a masker");
        let turn: Id = "01a1dc91-6ac0-7000-81df-da7a5eb236f8"
            .parse()
            .expect("an id");
        let Some(Read::Ok(env)) = r.get(KIND_TURN, turn).expect("get") else {
            panic!("the turn is imported");
        };
        let body: TurnBody = env.body_as().expect("a turn");
        assert!(body.text.contains("えび"), "{}", body.text);
        let masked = masker.mask(
            &[evorag_memory::TurnText {
                id: turn,
                rev: turn_rev(&env),
                text: &body.text,
            }],
            "[非表示]",
        );
        assert!(!masked[0].contains("えび"), "{masked:?}");
        assert!(masked[0].contains("[非表示]"), "{masked:?}");
    }
    let sent: Vec<String> = llm
        .requests()
        .iter()
        .map(|r| user_side(&r.messages))
        .collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    // 添えた往復は、伏せた後の本文で入る (private は根拠の位置 (取り込んだ v0.4 の出典の範囲) ごと伏せる)。
    assert!(sent[0].contains("実は、[非表示]んです"), "{}", sent[0]);
    assert!(sent[1].contains("趣味は[非表示]です"), "{}", sent[1]);
    for (i, s) in sent.iter().enumerate() {
        assert!(
            s.contains("<<引用-"),
            "request {i} recalls earlier turns: {s}"
        );
        assert!(s.contains("[非表示]"), "request {i}: {s}");
        assert!(
            !s.contains("えび"),
            "request {i} leaks the private value: {s}"
        );
        assert!(
            !s.contains("将棋"),
            "request {i} leaks the forgotten value: {s}"
        );
    }
}
