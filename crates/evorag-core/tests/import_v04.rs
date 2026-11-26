//! v0.4 のデータ根から新しい根への取り込み (`import-v04` の入れ替えの前まで、03 §12.4、FR-219)。
//!
//! 見本 `tests/fixtures/db/0.4.0/root` (v0.4.0 が書いた根、README に中身の一覧) を一時のフォルダに写してから使う
//! (取り込みは古い根に書かないが、ロックのファイルを作るため。更新時刻は Git が保たないので、写した後の値と比べる)。
//!
//! verifies: 09 §3.6 (v0.4 のデータ根からの取り込み)

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_core::import_v04::{ImportError, ImportOptions, Imported, run};
use evorag_memory::{
    DirectiveBody, Exclusion, ForgetKey, KIND_DIRECTIVE, KIND_FACT_DIRECTIVE, V04Ended, kinds_v04,
    kinds_v05, loose_key, value_key,
};
use evorag_store::import_v04::{NextStep, Refusal, rebuild_check};
use evorag_store::inspect::inspect_collect;
use evorag_store::location::Verdict;
use evorag_store::lock::DataRootLock;
use evorag_store::testutil::{TempDir, raw_remove_meta, raw_set_meta};
use evorag_store::{DB_FILE, Envelope, JOURNAL_DIR, OpenError, OpenOptions, Store, meta};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use sha2::{Digest, Sha256};

/// 取り込みの時刻 (見本を作った日の後)。
const NOW: i64 = 1_795_000_000_000;

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

/// 見本を `<一時のフォルダ>/evorag` に写す。
fn copied_fixture(label: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new(label);
    let root = tmp.path().join("evorag");
    copy_dir(&fixture_root(), &root);
    (tmp, root)
}

fn options(root: &Path) -> ImportOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    ImportOptions {
        old_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        location: |_| Verdict::Ok,
        allow_unsafe_location: false,
    }
}

fn id(s: &str) -> Id {
    s.parse().expect("an id")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// データ根のすべてのファイルの (大きさ・更新時刻・SHA-256)。ロックのファイルは除く (取り込みが取るロックで作られる)。
fn files(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime, String)> {
    fn walk(
        base: &Path,
        dir: &Path,
        out: &mut BTreeMap<String, (u64, std::time::SystemTime, String)>,
    ) {
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
            let m = std::fs::metadata(&path).expect("metadata");
            let bytes = std::fs::read(&path).expect("read a file");
            out.insert(
                rel,
                (
                    m.len(),
                    m.modified().expect("a modified time"),
                    hex(&Sha256::digest(&bytes)),
                ),
            );
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// 種類 `kind` の読める記録 (読むだけで開く)。
fn collect(root: &Path, kinds: Vec<evorag_store::KindSpec>, kind: &str) -> Vec<Envelope> {
    let mut got = inspect_collect(root, kinds, None, &[kind]).expect("inspect");
    got.collected.remove(kind).unwrap_or_default()
}

fn meta_of(root: &Path) -> BTreeMap<String, String> {
    inspect_collect(root, kinds_v05(), None, &[])
        .expect("inspect")
        .meta
}

fn import(root: &Path) -> Imported {
    run(options(root)).unwrap_or_else(|e| panic!("import failed: {e}"))
}

/// verifies: AC-FR-219-1
#[test]
fn the_v0_4_fixture_imports_into_a_verified_new_root() {
    let (_tmp, old) = copied_fixture("import-ok");
    let before = files(&old);
    let imported = import(&old);
    let new = old.with_file_name("evorag.v05-import");
    assert_eq!(imported.new_root, new);
    assert!(new.join(DB_FILE).is_file());
    assert!(
        !new.join("import-check").exists(),
        "the rebuild check folder is removed"
    );
    assert!(
        imported
            .renamed_old_root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("evorag-v0.4-")),
        "{:?}",
        imported.renamed_old_root
    );

    // 古い根のすべてのファイルが変わらない (大きさ・更新時刻・SHA-256)。
    drop(imported.old_root_lock);
    assert_eq!(files(&old), before, "the old data root is untouched");

    // 種類ごとの件数と (ID ‖ 本体の CBOR) の SHA-256 が、古い根の取り込んだ記録と一致する。
    let excluded: BTreeSet<Id> = [
        id("01a1dd27-9040-7000-bdd7-32262feb6e95"),
        id("01a1dc98-bdc0-7000-ad0d-723fd1859e5b"),
    ]
    .into();
    let want_counts = [
        ("session", 5),
        ("turn", 24),
        ("signal", 1),
        ("recall_feedback", 2),
        ("setting", 2),
        ("model_binding", 2),
        ("threshold", 1),
    ];
    for (kind, n) in want_counts {
        let old_envs: Vec<Envelope> = collect(&old, kinds_v04(), kind)
            .into_iter()
            .filter(|e| !excluded.contains(&e.id))
            .collect();
        let new_envs = collect(&new, kinds_v05(), kind);
        let want = body_digest(&old_envs);
        let got = body_digest(&new_envs);
        assert_eq!(got.0, n, "{kind} count");
        assert_eq!(got, want, "{kind} digest");
        let reported = &imported.report.kinds[kind];
        assert_eq!(
            (reported.count, reported.sha256.clone()),
            want,
            "{kind} report"
        );
        // 本体のバイト列・作成と更新の時刻・知らない項目を変えずに写す。
        for (o, n) in old_envs.iter().zip(&new_envs) {
            assert_eq!(o.id, n.id);
            assert_eq!(o.body, n.body, "{kind} {} body", o.id);
            assert_eq!((o.created_at, o.updated_at), (n.created_at, n.updated_at));
            assert_eq!(o.unknown, n.unknown);
        }
    }

    // 指示の件数と元の ID の集合が `fact_directive` と一致する。
    let fds: BTreeSet<Id> = collect(&old, kinds_v04(), KIND_FACT_DIRECTIVE)
        .iter()
        .map(|e| e.id)
        .collect();
    let directives = collect(&new, kinds_v05(), KIND_DIRECTIVE);
    let ids: BTreeSet<Id> = directives.iter().map(|e| e.id).collect();
    assert_eq!(fds.len(), 10);
    assert_eq!(ids, fds);
    assert_eq!(imported.report.directives, 10);
    for d in &directives {
        let b: DirectiveBody = d.body_as().expect("a directive body");
        assert!(b.imported, "{} is marked imported", d.id);
    }

    // 新しい根の形式の版は 0.5 で、v0.4 以前の版は読むだけでも開かない。
    let m = meta_of(&new);
    for k in [
        meta::FORMAT_VERSION,
        meta::FORMAT_MIN_READ,
        meta::FORMAT_MIN_WRITE,
    ] {
        assert_eq!(m.get(k).map(String::as_str), Some("0.5"), "{k}");
    }
    let om = meta_of(&old);
    assert_ne!(m.get(meta::DATA_ROOT_ID), om.get(meta::DATA_ROOT_ID));
    assert_eq!(m.get(meta::PROMPT_LANGUAGE), om.get(meta::PROMPT_LANGUAGE));
    assert_eq!(m.get(meta::FORGET_KEY), om.get(meta::FORGET_KEY));
    assert_eq!(
        m.get(meta::IMPORT_OLD_ROOT).map(String::as_str),
        Some(meta::IMPORT_OLD_ROOT_KEPT)
    );
    let source: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_SOURCE]).expect("import.source is JSON");
    assert_eq!(source["journal_last_seq"], 109);
    assert_eq!(
        source["data_root_id"].as_str(),
        om.get(meta::DATA_ROOT_ID).map(String::as_str)
    );
    assert_eq!(
        source["path"].as_str(),
        Some(imported.renamed_old_root.to_string_lossy().as_ref())
    );
    let report: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_REPORT]).expect("import.report is JSON");
    assert_eq!(report["kinds"]["turn"]["count"], 24);
    assert_eq!(report["directive"]["count"], 10);
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    let as_v04 = Store::open(OpenOptions {
        data_root: new.clone(),
        evorag_version: "0.4.0".into(),
        kinds: kinds_v04(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock.clone())),
        config_file: None,
    });
    assert!(
        matches!(as_v04, Err(OpenError::TooNew { .. })),
        "the 0.3 format refuses to open the new root: {:?}",
        as_v04.err()
    );

    // 新しい根のジャーナルから DB を作り直した結果が、DB と一致する。
    let (_, plan) = evorag_memory::v05_root();
    let (store, _) = Store::open_with(
        OpenOptions {
            data_root: new.clone(),
            evorag_version: "0.5.0".into(),
            kinds: kinds_v05(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock.clone())),
            config_file: None,
        },
        plan.clone(),
    )
    .expect("reopen the new root");
    let scratch = TempDir::new("import-rebuild");
    let diff = rebuild_check(
        &store,
        OpenOptions {
            data_root: scratch.path().join("rebuilt"),
            evorag_version: "0.5.0".into(),
            kinds: kinds_v05(),
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::new(clock)),
            config_file: None,
        },
        plan,
    )
    .expect("rebuild from the journal");
    assert!(diff.is_empty(), "{diff:?}");
}

/// (ID ‖ 本体の CBOR) を ID の順に並べた SHA-256 (03 §12.4 の照合の (1) と同じ定め。試験の側で独りで求める)。
fn body_digest(envs: &[Envelope]) -> (u64, String) {
    let mut sorted: Vec<&Envelope> = envs.iter().collect();
    sorted.sort_by_key(|e| e.id);
    let mut h = Sha256::new();
    for e in &sorted {
        h.update(e.id.as_bytes());
        h.update(e.body_cbor());
    }
    (sorted.len() as u64, hex(&h.finalize()))
}

/// 除くもの (a)・(b) が照合の結果に残り、ターンは 24 件・指示は 10 件になる。
///
/// verifies: AC-FR-219-1
#[test]
fn the_fixture_exclusions_are_reported() {
    let (_tmp, old) = copied_fixture("import-excluded");
    let imported = import(&old);
    let r = &imported.report;
    let by: BTreeMap<Id, Exclusion> = r.excluded.iter().map(|e| (e.id, e.reason)).collect();
    assert_eq!(
        by,
        BTreeMap::from([
            (
                id("01a1dd27-9040-7000-bdd7-32262feb6e95"),
                Exclusion::TurnWithoutSession
            ),
            (
                id("01a1dc98-bdc0-7000-ad0d-723fd1859e5b"),
                Exclusion::RedactionMismatch
            ),
        ])
    );
    assert_eq!(r.excluded_count(Exclusion::TargetNotCopied), 0);
    assert_eq!(r.kinds["turn"].count, 24);
    assert_eq!(r.directives, 10);
    let m = meta_of(&imported.new_root);
    let report: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_REPORT]).expect("import.report is JSON");
    assert_eq!(report["excluded_counts"]["turn_without_session"], 1);
    assert_eq!(report["excluded_counts"]["redaction_mismatch"], 1);
}

/// 写した指示の形 (03 §12.4 の「指示の写し方」、04 §7.5): 主語は entity、属性は辞書の属性、`private`・`accept`・
/// `revert` は `vk` を `vks` の 1 要素にし、v0.4 の事実の出典の範囲を `at` に入れる。
///
/// verifies: AC-FR-219-1
#[test]
fn imported_directives_keep_their_keys_and_get_the_fact_source_as_their_position() {
    let (_tmp, old) = copied_fixture("import-directives");
    let imported = import(&old);
    // 古い根を読み直すので、取り込みが持つ古い根のロックを外す。
    drop(imported.old_root_lock);
    let olds: BTreeMap<Id, evorag_memory::FactDirectiveBody> =
        collect(&old, kinds_v04(), KIND_FACT_DIRECTIVE)
            .into_iter()
            .map(|e| (e.id, e.body_as().expect("a v0.4 directive")))
            .collect();
    let news: BTreeMap<Id, DirectiveBody> =
        collect(&imported.new_root, kinds_v05(), KIND_DIRECTIVE)
            .into_iter()
            .map(|e| (e.id, e.body_as().expect("a directive")))
            .collect();
    for (i, o) in &olds {
        let n = &news[i];
        assert_eq!(n.op, o.op, "{i}");
        assert_eq!(n.target, o.target, "{i}");
        assert_eq!(n.origin, o.origin, "{i}");
        assert_eq!(n.marks, o.marks, "{i}");
        assert_eq!(n.turns, o.turns, "{i}");
        assert_eq!(n.history, o.history, "{i}");
        assert_eq!(n.redact, o.redact, "{i}");
        assert_eq!(n.norm, o.norm, "{i}");
        assert_eq!(n.redactions.len(), o.redactions.len(), "{i}");
        assert_eq!(n.hidden.len(), o.hidden.len(), "{i}");
        match o.op.as_str() {
            "forget" => {
                assert_eq!(n.vks, o.vks, "{i}");
                assert_eq!(n.entity.as_deref(), Some(o.subject.as_str()));
                assert_eq!(n.aspect.as_deref(), Some(o.attribute.as_str()));
            }
            "private" | "accept" | "revert" => {
                assert_eq!(n.vks, o.vk.iter().cloned().collect::<Vec<_>>(), "{i}");
                assert_eq!(n.vks.len(), 1, "{i}");
                assert!(n.at.is_some(), "{i} has a position");
                assert_eq!(n.aspect.as_deref(), Some(o.attribute.as_str()));
            }
            _ => {
                assert!(n.vks.is_empty(), "{i}");
                assert_eq!(n.entity, None, "{i}");
            }
        }
    }
    // えびの private: 事実 01a1dc91-6ac0-7004 の出典 (ターン 01a1dc91-6ac0-7000、rev 0、9..36)。
    let shrimp = &news[&id("01a1dccd-d780-7000-8747-d9fb9bc44a54")];
    let at = shrimp.at.expect("a position");
    assert_eq!(at.turn, id("01a1dc91-6ac0-7000-81df-da7a5eb236f8"));
    assert_eq!((at.rev, at.start, at.end), (0, 9, 36));
    assert_eq!(shrimp.scope.as_deref(), Some("value_only"));
    assert_eq!(imported.report.directives_without_at, 0);
}

/// `import.v04_ended` に、見本の今の値でなかった値 (東京・パン屋) があり、値の文字列は無い。
///
/// verifies: AC-FR-219-4 (一部: `end` を足す材料の表。導出で `end` を足すのは M5.3・M5.4)
#[test]
fn the_ended_table_lists_the_superseded_values_without_their_strings() {
    let (_tmp, old) = copied_fixture("import-ended");
    let imported = import(&old);
    let m = meta_of(&imported.new_root);
    let raw = &m[meta::IMPORT_V04_ENDED];
    for s in ["東京", "パン屋", "大阪", "えび", "猫"] {
        assert!(
            !raw.contains(s),
            "import.v04_ended carries no value string ({s})"
        );
    }
    let ended: V04Ended = serde_json::from_str(raw).expect("import.v04_ended is JSON");
    assert!(!ended.done);
    let key = ForgetKey::from_hex(&m[meta::FORGET_KEY]).expect("a forget key");
    let has = |aspect: &str, value: &str, turn: &str| {
        let vk = value_key(&key, value).to_hex();
        let lk = loose_key(&key, value).to_hex();
        ended.values.iter().any(|v| {
            v.aspect == aspect
                && v.vks.contains(&vk)
                && v.vks.contains(&lk)
                && v.turn == Some(id(turn))
        })
    };
    assert!(has(
        "residence",
        "東京",
        "01a1dca3-ba40-7000-93f1-83c986aec07e"
    ));
    assert!(has(
        "occupation",
        "パン屋",
        "01a1dc8f-9600-7000-9d2f-9b2d0b5f15e6"
    ));
    // v0.4 で Live だった値は表に入らない。
    let osaka = value_key(&key, "大阪").to_hex();
    assert!(ended.values.iter().all(|v| !v.vks.contains(&osaka)));
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// 伏せ字にして忘れた値の文字列 (README の一覧) は、新しい根の DB にもジャーナルにも無い。
///
/// verifies: AC-FR-219-4 (一部: 伏せ字にした値が新しい根に入らない)
#[test]
fn redacted_values_never_reach_the_new_root() {
    let (_tmp, old) = copied_fixture("import-redacted");
    let imported = import(&old);
    let mut bytes = std::fs::read(imported.new_root.join(DB_FILE)).expect("read the db");
    for e in std::fs::read_dir(imported.new_root.join(JOURNAL_DIR)).expect("the journal") {
        bytes.extend(std::fs::read(e.expect("an entry").path()).expect("read the journal"));
    }
    for s in ["かに", "チョコレート", "5月3日"] {
        assert!(!contains(&bytes, s), "the new root does not hold {s}");
    }
    // 伏せ字にしないで忘れた値の本文は、ターンの本文のまま残る (探し方が効いていることの確かめ)。
    assert!(contains(&bytes, "将棋"));
}

/// 断る根: 何も書かずに、理由と次にすることを示す。
fn assert_refused(root: &Path, want: fn(&Refusal) -> bool, next: NextStep) {
    let before = files(root);
    match run(options(root)) {
        Err(ImportError::Refused(r)) => {
            assert!(want(&r), "unexpected refusal {r:?}");
            assert_eq!(r.next_step(), next, "{r:?}");
        }
        other => panic!("expected a refusal, got {:?}", other.map(|i| i.new_root)),
    }
    assert_eq!(files(root), before, "nothing is written to the old root");
    assert!(
        !root.with_file_name("evorag.v05-import").exists(),
        "no new root is left"
    );
}

/// verifies: AC-FR-219-2
#[test]
fn a_root_with_a_pending_forget_recheck_is_refused() {
    let (_tmp, old) = copied_fixture("import-recheck");
    raw_set_meta(&old, meta::FORGET_RECHECK, meta::FORGET_RECHECK_SET);
    assert_refused(
        &old,
        |r| matches!(r, Refusal::ForgetPending),
        NextStep::OpenOnceWithV04,
    );
}

/// verifies: AC-FR-219-2
#[test]
fn a_root_with_journal_lines_not_in_the_database_is_refused() {
    let (_tmp, old) = copied_fixture("import-journal");
    raw_set_meta(&old, meta::JOURNAL_LAST_SEQ, "108");
    assert_refused(
        &old,
        |r| matches!(r, Refusal::JournalPending { lines: 1 }),
        NextStep::OpenOnceWithV04,
    );
}

/// verifies: AC-FR-219-2
#[test]
fn a_root_used_only_by_v0_3_is_refused() {
    let (_tmp, old) = copied_fixture("import-v03");
    raw_set_meta(&old, meta::MAX_WRITER, "0.3.0");
    raw_set_meta(&old, meta::LAST_WRITER, "0.3.0");
    raw_remove_meta(&old, "writer.0.4.0");
    assert_refused(
        &old,
        |r| matches!(r, Refusal::NotOpenedByV04 { .. }),
        NextStep::OpenOnceWithV04,
    );
}

/// verifies: AC-FR-219-2
#[test]
fn a_format_0_2_root_is_refused() {
    let (_tmp, old) = copied_fixture("import-v02");
    raw_set_meta(&old, meta::FORMAT_VERSION, "0.2");
    assert_refused(
        &old,
        |r| matches!(r, Refusal::NotOpenedByV04 { .. }),
        NextStep::OpenOnceWithV04,
    );
}

/// 古い根を v0.4 が使っている (ロックが取れない) ときは断り、何も書かない (03 §12.4 の (5))。
///
/// verifies: AC-FR-219-2
#[test]
fn a_root_in_use_is_refused_without_writing() {
    let (_tmp, old) = copied_fixture("import-locked");
    let held = DataRootLock::acquire(&old).expect("hold the old root");
    let before = files(&old);
    assert!(matches!(run(options(&old)), Err(ImportError::Locked(_))));
    assert_eq!(files(&old), before);
    assert!(!old.with_file_name("evorag.v05-import").exists());
    drop(held);
}

/// 危ない置き場所は、古い根に触れる前に断る (03 §2.2・§12.4 の (5))。
#[test]
fn an_unsafe_location_is_refused_before_touching_the_root() {
    let (_tmp, old) = copied_fixture("import-unsafe");
    let mut opts = options(&old);
    opts.location = |_| Verdict::Unsafe("a synced folder".into());
    assert!(matches!(run(opts), Err(ImportError::UnsafeLocation(_))));
    assert!(
        !old.join("lock").exists(),
        "the old root is not even locked"
    );
    assert!(!old.with_file_name("evorag.v05-import").exists());
}

/// 前の取り込みの新しい根が残っていれば、触れずに断る (片付けは入れ替えの段が行う)。
#[test]
fn a_leftover_import_folder_is_not_overwritten() {
    let (_tmp, old) = copied_fixture("import-leftover");
    let leftover = old.with_file_name("evorag.v05-import");
    std::fs::create_dir_all(&leftover).expect("make a leftover");
    std::fs::write(leftover.join("marker"), b"x").expect("write");
    assert!(matches!(run(options(&old)), Err(ImportError::Leftover(_))));
    assert_eq!(std::fs::read(leftover.join("marker")).expect("kept"), b"x");
}
