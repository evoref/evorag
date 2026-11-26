//! v0.4 のデータ根から新しい根への取り込み (`import-v04` の入れ替えの前まで、03 §12.4、FR-219)。
//!
//! 見本 `tests/fixtures/db/0.4.0/root` (v0.4.0 が書いた根、README に中身の一覧) を一時のフォルダに写してから使う
//! (取り込みは古い根に書かないが、ロックのファイルを作るため。更新時刻は Git が保たないので、写した後の値と比べる)。
//! 断る根・壊れた根は、写しの redb を直接書き換えて作る (v0.4.0 の書き手は使わない)。
//!
//! verifies: 09 §3.6 (v0.4 のデータ根からの取り込み)

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_core::import_v04::{ImportError, ImportOptions, ImportReport, Imported, run};
use evorag_memory::{
    DirectiveBody, Exclusion, FactDirectiveBody, ForgetKey, KIND_DIRECTIVE, KIND_FACT_DIRECTIVE,
    V04Ended, kinds_v04, kinds_v05, loose_key, value_key,
};
use evorag_store::import_v04::{NextStep, Refusal, import_dir, rebuild_check};
use evorag_store::inspect::inspect_collect;
use evorag_store::location::Verdict;
use evorag_store::lock::DataRootLock;
use evorag_store::testutil::{
    TempDir, raw_corrupt_record, raw_put_envelope, raw_put_link, raw_put_lookup, raw_remove_meta,
    raw_set_meta,
};
use evorag_store::{
    BodyPatch, DB_FILE, Envelope, JOURNAL_DIR, OpenError, OpenOptions, Store, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use sha2::{Digest, Sha256};

/// 取り込みの時刻 (見本を作った日の後)。
const NOW: i64 = 1_795_000_000_000;

/// 見本の、伏せ字の記録と本文の合わないターン (README の「除くもの」の (b))。
const MISMATCHED_TURN: &str = "01a1dc98-bdc0-7000-ad0d-723fd1859e5b";
/// 見本の、セッションの無いターン (README の「除くもの」の (a))。
const ORPHAN_TURN: &str = "01a1dd27-9040-7000-bdd7-32262feb6e95";
/// 見本の、伏せ字にして忘れた「かに」の出典のターン。
const REDACTED_TURN: &str = "01a1dc93-3f80-7000-a70c-d6df5d49ce30";

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
        before_verify: None,
    }
}

fn store_options(root: &Path) -> OpenOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(NOW)));
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        kinds: kinds_v05(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
        config_file: None,
    }
}

fn id(s: &str) -> Id {
    s.parse().expect("an id")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_root_of(old: &Path) -> PathBuf {
    old.with_file_name("evorag.v05-import")
}

/// データ根のすべてのファイルの (大きさ・更新時刻・SHA-256)。ロックのファイルは除く (取り込みが取るロックで作られる、
/// 03 §12.4 の「読み方」)。
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

fn one(root: &Path, kinds: Vec<evorag_store::KindSpec>, kind: &str, want: Id) -> Envelope {
    collect(root, kinds, kind)
        .into_iter()
        .find(|e| e.id == want)
        .unwrap_or_else(|| panic!("{kind}/{want} is in the root"))
}

fn meta_of(root: &Path) -> BTreeMap<String, String> {
    inspect_collect(root, kinds_v05(), None, &[])
        .expect("inspect")
        .meta
}

/// 取り込みを行い、持っている 2 つのロックを外して結果を返す (試験が両方の根を読み直すため)。
fn import(root: &Path) -> (PathBuf, ImportReport, PathBuf) {
    let Imported {
        new_root,
        renamed_old_root,
        report,
        old_root_lock,
        new_root_lock,
        ..
    } = run(&options(root)).unwrap_or_else(|e| panic!("import failed: {e}"));
    drop((old_root_lock, new_root_lock));
    (new_root, report, renamed_old_root)
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

/// 新しい根の DB とジャーナルのすべてのバイト列。
fn root_bytes(root: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(root.join(DB_FILE)).expect("read the db");
    for e in std::fs::read_dir(root.join(JOURNAL_DIR)).expect("the journal") {
        bytes.extend(std::fs::read(e.expect("an entry").path()).expect("read the journal"));
    }
    bytes
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// 封筒 `env` の本体に差し替えを当てて、閉じた根に直接書く。
fn patch_record(root: &Path, mut env: Envelope, patch: BodyPatch) {
    env.apply_patch(&patch).expect("a map body");
    raw_put_envelope(root, &env);
}

/// 見本から取り込むと、照合が通った新しい根ができ、古い根は変わらない。
///
/// v0.4.0 の配布物そのもので新しい根を開くことは、この試験では確かめていない (この workspace に v0.4.0 の
/// バイナリは無い)。代わりに、新しい根の `format.min_read` が `0.5` で、v0.4 と同じ形式の版 (`0.3`) の開き方が
/// `TooNew` で断ることを確かめる。
///
/// verifies: AC-FR-219-1
#[test]
fn the_v0_4_fixture_imports_into_a_verified_new_root() {
    let (_tmp, old) = copied_fixture("import-ok");
    let before = files(&old);
    let imported = run(&options(&old)).unwrap_or_else(|e| panic!("import failed: {e}"));
    let new = new_root_of(&old);
    assert_eq!(imported.new_root, new);
    // 2 つの根のロックを、取り込みの後も持っている (入れ替えの直前まで持つ、03 §9)。
    assert!(
        DataRootLock::acquire(&old).is_err(),
        "the old root is locked"
    );
    assert!(
        DataRootLock::acquire(&new).is_err(),
        "the new root is locked"
    );
    drop((imported.old_root_lock, imported.new_root_lock));
    let report = imported.report;
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
    assert_eq!(files(&old), before, "the old data root is untouched");

    // 種類ごとの件数と (ID ‖ 本体の CBOR) の SHA-256 が、古い根の取り込んだ記録と一致する。
    let excluded: BTreeSet<Id> = [id(ORPHAN_TURN), id(MISMATCHED_TURN)].into();
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
        let reported = &report.kinds[kind];
        assert_eq!(
            (reported.count, reported.sha256.clone()),
            want,
            "{kind} report"
        );
        // 本体のバイト列・形式の版・作成と更新の時刻・知らない項目を変えずに写す。
        for (o, n) in old_envs.iter().zip(&new_envs) {
            assert_eq!(o.id, n.id);
            assert_eq!(o.v, n.v, "{kind} {} v", o.id);
            assert_eq!(o.body, n.body, "{kind} {} body", o.id);
            assert_eq!((o.created_at, o.updated_at), (n.created_at, n.updated_at));
            assert_eq!(o.unknown, n.unknown);
        }
    }

    // 指示の件数と元の ID の集合が `fact_directive` と一致し、照合の結果にハッシュが残る。
    let fds: BTreeSet<Id> = collect(&old, kinds_v04(), KIND_FACT_DIRECTIVE)
        .iter()
        .map(|e| e.id)
        .collect();
    let directives = collect(&new, kinds_v05(), KIND_DIRECTIVE);
    let ids: BTreeSet<Id> = directives.iter().map(|e| e.id).collect();
    assert_eq!(fds.len(), 10);
    assert_eq!(ids, fds);
    assert_eq!(report.directives.count, 10);
    assert_eq!(
        (report.directives.count, report.directives.sha256.clone()),
        body_digest(&directives)
    );

    // 新しい根の形式の版は 0.5 で、v0.4 と同じ形式 (0.3) の開き方は読むだけでも開かない。
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
    let json: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_REPORT]).expect("import.report is JSON");
    assert_eq!(json["kinds"]["turn"]["count"], 24);
    assert_eq!(json["directive"]["count"], 10);
    assert!(json["directive"].get("ended_added").is_none());
    let mut v04 = store_options(&new);
    v04.evorag_version = "0.4.0".into();
    v04.kinds = kinds_v04();
    let as_v04 = Store::open(v04);
    assert!(
        matches!(as_v04, Err(OpenError::TooNew { .. })),
        "the 0.3 format refuses to open the new root: {:?}",
        as_v04.err()
    );

    // 新しい根のジャーナルから DB を作り直した結果が、DB と一致する。
    let (_, plan) = evorag_memory::v05_root();
    let (store, _) = Store::open_with(store_options(&new), plan.clone()).expect("reopen");
    let scratch = TempDir::new("import-rebuild");
    let diff = rebuild_check(&store, store_options(&scratch.path().join("rebuilt")), plan)
        .expect("rebuild from the journal");
    assert!(diff.is_empty(), "{diff:?}");
}

/// 除くもの (a)・(b) が照合の結果に残り、ターンは 24 件・指示は 10 件になる。
///
/// verifies: AC-FR-219-1
#[test]
fn the_fixture_exclusions_are_reported() {
    let (_tmp, old) = copied_fixture("import-excluded");
    let (new, r, _) = import(&old);
    let by: BTreeMap<Id, Exclusion> = r.excluded.iter().map(|e| (e.id, e.reason)).collect();
    assert_eq!(
        by,
        BTreeMap::from([
            (id(ORPHAN_TURN), Exclusion::TurnWithoutSession),
            (id(MISMATCHED_TURN), Exclusion::RedactionMismatch),
        ])
    );
    assert_eq!(r.excluded_count(Exclusion::TargetNotCopied), 0);
    assert_eq!(r.kinds["turn"].count, 24);
    assert_eq!(r.directives.count, 10);
    let m = meta_of(&new);
    let report: serde_json::Value =
        serde_json::from_str(&m[meta::IMPORT_REPORT]).expect("import.report is JSON");
    assert_eq!(report["excluded_counts"]["turn_without_session"], 1);
    assert_eq!(report["excluded_counts"]["redaction_mismatch"], 1);
}

/// 写した指示の形を項目ごとに確かめる (03 §12.4 の「指示の写し方」、04 §7.5)。
///
/// verifies: AC-FR-219-1
#[test]
fn imported_directives_are_mapped_field_by_field() {
    let (_tmp, old) = copied_fixture("import-directives");
    let (new, report, _) = import(&old);
    let olds: BTreeMap<Id, (Envelope, FactDirectiveBody)> =
        collect(&old, kinds_v04(), KIND_FACT_DIRECTIVE)
            .into_iter()
            .map(|e| {
                let b = e.body_as().expect("a v0.4 directive");
                (e.id, (e, b))
            })
            .collect();
    let news: BTreeMap<Id, (Envelope, DirectiveBody)> = collect(&new, kinds_v05(), KIND_DIRECTIVE)
        .into_iter()
        .map(|e| {
            let b = e.body_as().expect("a directive");
            (e.id, (e, b))
        })
        .collect();
    for (i, (oe, o)) in &olds {
        let (ne, n) = &news[i];
        assert_eq!(
            (oe.created_at, oe.updated_at),
            (ne.created_at, ne.updated_at),
            "{i}"
        );
        assert!(n.imported, "{i}");
        assert_eq!(n.op, o.op, "{i}");
        assert_eq!(n.target, o.target, "{i}");
        assert_eq!(n.origin, o.origin, "{i}");
        assert_eq!(n.marks, o.marks, "{i}");
        assert_eq!(n.turns, o.turns, "{i}");
        assert_eq!(n.history, o.history, "{i}");
        assert_eq!(n.redact, o.redact, "{i}");
        assert_eq!(n.norm, o.norm, "{i}");
        let redactions: Vec<_> = o
            .redactions
            .iter()
            .map(|r| {
                (
                    r.turn,
                    r.rev_before,
                    r.rev_after,
                    r.start,
                    r.end,
                    r.hash.clone(),
                )
            })
            .collect();
        let got: Vec<_> = n
            .redactions
            .iter()
            .map(|r| {
                (
                    r.turn,
                    r.rev_before,
                    r.rev_after,
                    r.start,
                    r.end,
                    r.hash.clone(),
                )
            })
            .collect();
        assert_eq!(got, redactions, "{i} redactions");
        let hidden: Vec<_> = o
            .hidden
            .iter()
            .map(|h| (h.turn, h.rev, h.start, h.end))
            .collect();
        let got: Vec<_> = n
            .hidden
            .iter()
            .map(|h| (h.turn, h.rev, h.start, h.end))
            .collect();
        assert_eq!(got, hidden, "{i} hidden");
        match o.op.as_str() {
            "forget" => {
                assert_eq!(n.vks, o.vks, "{i}");
                assert_eq!(n.entity.as_deref(), Some(o.subject.as_str()));
                assert_eq!(n.aspect.as_deref(), Some(o.attribute.as_str()));
                assert_eq!(n.scope, None, "{i}");
            }
            "private" | "accept" | "revert" => {
                assert_eq!(n.vks, o.vk.iter().cloned().collect::<Vec<_>>(), "{i}");
                assert_eq!(n.vks.len(), 1, "{i}");
                assert!(n.at.is_some(), "{i} has a position");
                assert_eq!(n.entity.as_deref(), Some(o.subject.as_str()));
                assert_eq!(n.aspect.as_deref(), Some(o.attribute.as_str()));
                let want_scope = if o.op == "private" {
                    o.scope.clone()
                } else {
                    None
                };
                assert_eq!(n.scope, want_scope, "{i}");
            }
            // 解く指示は、v0.5 の形に合わせて entity と属性を持たない。
            _ => {
                assert!(n.vks.is_empty(), "{i}");
                assert_eq!(n.entity, None, "{i}");
                assert_eq!(n.aspect, None, "{i}");
                assert_eq!(n.at, None, "{i}");
            }
        }
    }
    // 伏せ字の記録と `[非表示]` の範囲が見本にある (比べが空振りしていない)。
    assert!(olds.values().any(|(_, o)| !o.redactions.is_empty()));
    assert!(olds.values().any(|(_, o)| !o.hidden.is_empty()));
    // えびの private: 事実 01a1dc91-6ac0-7004 の出典 (ターン 01a1dc91-6ac0-7000、rev 0、9..36)。
    let (_, shrimp) = &news[&id("01a1dccd-d780-7000-8747-d9fb9bc44a54")];
    let at = shrimp.at.expect("a position");
    assert_eq!(at.turn, id("01a1dc91-6ac0-7000-81df-da7a5eb236f8"));
    assert_eq!((at.rev, at.start, at.end), (0, 9, 36));
    assert_eq!(shrimp.scope.as_deref(), Some("value_only"));
    assert_eq!(report.directives_without_at, 0);
}

/// `import.v04_ended` に、見本の今の値でなかった値 (東京・パン屋) があり、値の文字列は無い。
///
/// verifies: AC-FR-219-4 (一部: `end` を足す材料の表。導出で `end` を足すのは M5.3・M5.4)
#[test]
fn the_ended_table_lists_the_superseded_values_without_their_strings() {
    let (_tmp, old) = copied_fixture("import-ended");
    let (new, _, _) = import(&old);
    let m = meta_of(&new);
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

/// 退行の見張り: 伏せ字にして忘れた値の文字列 (README の一覧) は、見本の古い根にも無いので、新しい根の DB にも
/// ジャーナルにも無い (伏せ字の判定が効くことは、下の書き換えた根の試験で確かめる)。
///
/// verifies: AC-FR-219-4 (一部: 伏せ字にした値が新しい根に入らない)
#[test]
fn redacted_values_never_reach_the_new_root() {
    let (_tmp, old) = copied_fixture("import-redacted");
    let (new, _, _) = import(&old);
    let bytes = root_bytes(&new);
    for s in ["かに", "チョコレート", "5月3日"] {
        assert!(!contains(&bytes, s), "the new root does not hold {s}");
    }
    // 伏せ字にしないで忘れた値の本文は、ターンの本文のまま残る (探し方が効いていることの確かめ)。
    assert!(contains(&bytes, "将棋"));
}

/// 伏せ字の前の本文と改訂の番号に戻したターン (当て直しの前の根) は、何も書かずに断る (伏せ字にして写すと「封筒を
/// そのまま」にならないため、03 §12.4 の (2))。
///
/// verifies: AC-FR-219-2
#[test]
fn a_turn_back_at_its_pre_redaction_text_is_refused() {
    let (_tmp, old) = copied_fixture("import-pre-redaction");
    let turn = id(REDACTED_TURN);
    let forget = collect(&old, kinds_v04(), KIND_FACT_DIRECTIVE)
        .into_iter()
        .filter_map(|e| e.body_as::<FactDirectiveBody>().ok())
        .find(|b| b.redactions.iter().any(|r| r.turn == turn))
        .expect("the forget of the turn");
    let r = forget
        .redactions
        .iter()
        .find(|r| r.turn == turn)
        .expect("a redaction");
    let env = one(&old, kinds_v04(), "turn", turn);
    let after: String = env.body_as::<serde_json::Value>().expect("a body")["text"]
        .as_str()
        .expect("a text")
        .to_owned();
    let marker = "[伏せ字]";
    let start = r.start as usize;
    assert_eq!(&after[start..start + marker.len()], marker);
    let before = format!("{}かに{}", &after[..start], &after[start + marker.len()..]);
    assert_eq!(r.end as usize - start, "かに".len());
    patch_record(
        &old,
        env,
        BodyPatch::new()
            .set("text", &before)
            .and_then(|p| p.set("rev", &r.rev_before))
            .expect("a patch"),
    );
    assert_refused(
        &old,
        |r| matches!(r, Refusal::ForgetPending),
        NextStep::OpenOnceWithV04,
    );
}

/// 伏せ字の記録と合わないターンは除き、その本文 (目印の文字列) は新しい根の DB にもジャーナルにも入らない。
///
/// verifies: AC-FR-219-4 (一部: 合わないターンの本文を写さない)
#[test]
fn a_mismatched_turn_and_its_text_stay_out_of_the_new_root() {
    let (_tmp, old) = copied_fixture("import-mismatch-text");
    let env = one(&old, kinds_v04(), "turn", id(MISMATCHED_TURN));
    patch_record(
        &old,
        env,
        BodyPatch::new()
            .set("text", &"誕生日は目印MISMATCHでした")
            .expect("a patch"),
    );
    assert!(contains(
        &std::fs::read(old.join(DB_FILE)).expect("db"),
        "目印MISMATCH"
    ));
    let (new, report, _) = import(&old);
    assert!(
        report
            .excluded
            .iter()
            .any(|e| e.id == id(MISMATCHED_TURN) && e.reason == Exclusion::RedactionMismatch)
    );
    assert!(!contains(&root_bytes(&new), "目印MISMATCH"));
}

/// 写さない種類 (`fact`・`note`・`chunk`) の中身は、新しい根に出ない (03 §12.4 の「写さないもの」)。
///
/// verifies: AC-FR-219-4 (一部: 写さない種類の値を写さない)
#[test]
fn kinds_that_are_not_copied_never_reach_the_new_root() {
    let (_tmp, old) = copied_fixture("import-not-copied");
    let fact = one(
        &old,
        kinds_v04(),
        "fact",
        id("01a1dc8b-ec80-7004-93fc-d6513d02befe"),
    );
    patch_record(
        &old,
        fact,
        BodyPatch::new().set("value", &"目印FACT").expect("a patch"),
    );
    let note = collect(&old, kinds_v04(), "note")
        .into_iter()
        .next()
        .expect("a note");
    patch_record(
        &old,
        note,
        BodyPatch::new()
            .set("keys", &["目印NOTE"])
            .expect("a patch"),
    );
    let chunk = collect(&old, kinds_v04(), "chunk")
        .into_iter()
        .next()
        .expect("a chunk");
    patch_record(
        &old,
        chunk,
        BodyPatch::new().set("x", &"目印CHUNK").expect("a patch"),
    );
    let old_bytes = std::fs::read(old.join(DB_FILE)).expect("db");
    let (new, _, _) = import(&old);
    let bytes = root_bytes(&new);
    for s in ["目印FACT", "目印NOTE", "目印CHUNK"] {
        assert!(contains(&old_bytes, s), "the old root holds {s}");
        assert!(!contains(&bytes, s), "the new root does not hold {s}");
    }
}

/// 断る根: ロックのファイルのほかは何も書かずに、理由と次にすることを示す。新しい根も残さない。
fn assert_refused(root: &Path, want: fn(&Refusal) -> bool, next: NextStep) -> Refusal {
    let before = files(root);
    let refusal = match run(&options(root)) {
        Err(ImportError::Refused(r)) => {
            assert!(want(&r), "unexpected refusal {r:?}");
            assert_eq!(r.next_step(), next, "{r:?}");
            r
        }
        other => panic!("expected a refusal, got {:?}", other.map(|i| i.new_root)),
    };
    assert_eq!(files(root), before, "nothing is written to the old root");
    let new = import_dir(root).expect("a named root");
    assert!(!new.exists(), "no new root is left");
    refusal
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
    let r = assert_refused(
        &old,
        |r| {
            matches!(
                r,
                Refusal::JournalPending {
                    lines: 1,
                    partial_tail: false,
                    rewrite_leftover: false
                }
            )
        },
        NextStep::OpenOnceWithV04,
    );
    assert!(r.to_string().contains("1 line(s)"), "{r}");
}

fn journal_file(root: &Path) -> PathBuf {
    std::fs::read_dir(root.join(JOURNAL_DIR))
        .expect("the journal")
        .map(|e| e.expect("an entry").path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("a journal file")
}

/// 書きかけの末尾の行と、書き直しの一時のファイルも、v0.4 で一度開いてから取り込む (03 §12.4 の (3))。
///
/// verifies: AC-FR-219-2
#[test]
fn a_journal_with_an_unfinished_tail_or_a_rewrite_leftover_is_refused() {
    let (_tmp, old) = copied_fixture("import-journal-tail");
    let file = journal_file(&old);
    let mut bytes = std::fs::read(&file).expect("read");
    bytes.extend_from_slice(b"{\"seq\":110,\"op\":\"put\"");
    std::fs::write(&file, bytes).expect("write");
    let r = assert_refused(
        &old,
        |r| {
            matches!(
                r,
                Refusal::JournalPending {
                    lines: 0,
                    partial_tail: true,
                    ..
                }
            )
        },
        NextStep::OpenOnceWithV04,
    );
    assert!(!r.to_string().contains("0 line(s)"), "{r}");

    let (_tmp2, old) = copied_fixture("import-journal-tmp");
    let tmp = journal_file(&old).with_extension("jsonl.tmp");
    std::fs::write(&tmp, b"").expect("write");
    assert_refused(
        &old,
        |r| {
            matches!(
                r,
                Refusal::JournalPending {
                    rewrite_leftover: true,
                    ..
                }
            )
        },
        NextStep::OpenOnceWithV04,
    );
}

/// 正しく閉じていない根 (移行の跡・redb の修復が要る DB・ジャーナルにだけある鍵) は、v0.4 で一度開いてから取り込む
/// (03 §12.4 の (6))。
///
/// verifies: AC-FR-219-2
#[test]
fn a_root_not_closed_cleanly_is_refused() {
    let unfinished = |r: &Refusal| matches!(r, Refusal::Unfinished { .. });

    let (_t1, old) = copied_fixture("import-migrating");
    std::fs::write(old.join("evorag.redb.migrating"), b"").expect("write");
    assert_refused(&old, unfinished, NextStep::OpenOnceWithV04);

    // 本体の DB が退けられた移行の途中も、データ根でないとはせずに同じく断る。
    let (_t2, old) = copied_fixture("import-migrating-no-db");
    std::fs::rename(old.join(DB_FILE), old.join("evorag.redb.pre-migration")).expect("rename");
    assert_refused(&old, unfinished, NextStep::OpenOnceWithV04);

    // redb の修復が要る (ファイルの頭の god byte の「修復が要る」の印、redb 4.5 の header.rs)。
    let (_t3, old) = copied_fixture("import-needs-repair");
    let mut db = std::fs::read(old.join(DB_FILE)).expect("read");
    db[9] |= 2;
    std::fs::write(old.join(DB_FILE), db).expect("write");
    assert_refused(&old, unfinished, NextStep::OpenOnceWithV04);

    let (_t4, old) = copied_fixture("import-key-in-journal");
    raw_remove_meta(&old, meta::FORGET_KEY);
    assert_refused(&old, unfinished, NextStep::OpenOnceWithV04);
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
    assert!(matches!(run(&options(&old)), Err(ImportError::Locked(_))));
    assert_eq!(files(&old), before);
    assert!(!new_root_of(&old).exists());
    drop(held);
}

/// 写しの途中で読めない記録が見つかったら、作りかけの新しい根を消して断る (03 §12.4 の (7))。
///
/// verifies: AC-FR-219-2
#[test]
fn an_unreadable_record_found_while_copying_leaves_no_new_root() {
    let (_tmp, old) = copied_fixture("import-corrupt");
    raw_corrupt_record(&old, "turn", id("01a1dcb9-b340-7000-9dc4-43bfb29af542"));
    assert_refused(
        &old,
        |r| matches!(r, Refusal::Unreadable { .. }),
        NextStep::RepairWithV04,
    );
}

/// 照合が合わなければ (ここでは、照合の前に古い根のファイルを変えて (4) を外す)、新しい根を消す。
///
/// verifies: AC-FR-219-1
#[test]
fn a_failed_verification_removes_the_new_root() {
    let (_tmp, old) = copied_fixture("import-mismatch");
    let mut opts = options(&old);
    opts.before_verify = Some(|old, _new| {
        let f = old.join("index/memory/chunk-lex/CURRENT");
        let mut bytes = std::fs::read(&f).expect("read");
        bytes.push(b'\n');
        std::fs::write(&f, bytes).expect("write");
    });
    match run(&opts) {
        Err(ImportError::Mismatch(problems)) => {
            assert!(
                problems.iter().any(|p| p.contains("old data root changed")),
                "{problems:?}"
            );
        }
        other => panic!("expected a mismatch, got {:?}", other.map(|i| i.new_root)),
    }
    assert!(!new_root_of(&old).exists(), "the new root is removed");
}

/// 照合を終えていない作りかけの根 (`import.source` があって `import.report` が無い) は、通常の開き方では開かず、
/// 何も書かない。取り込みの照合の入口では開ける (03 §12.4 の「書き方」)。
#[test]
fn an_unfinished_import_root_is_not_opened() {
    let (_tmp, old) = copied_fixture("import-unfinished");
    let (new, _, _) = import(&old);
    raw_remove_meta(&new, meta::IMPORT_REPORT);
    let before = files(&new);
    let (_, plan) = evorag_memory::v05_root();
    assert!(matches!(
        Store::open_with(store_options(&new), plan.clone()),
        Err(OpenError::UnfinishedImport)
    ));
    assert_eq!(files(&new), before, "nothing is written");
    let lock = DataRootLock::acquire(&new).expect("lock the new root");
    Store::open_for_import(store_options(&new), plan, lock).expect("the import entry opens it");
}

/// 照合の (3) は、ジャーナルを経ずに DB にだけ書いた記録・関係・副索引と、`meta` の値の違いを見つける。
#[test]
fn the_rebuild_check_finds_what_only_the_database_has() {
    let (_tmp, old) = copied_fixture("import-rebuild-diff");
    let (new, _, _) = import(&old);
    let extra = id("01a1dd30-0000-7000-8000-000000000001");
    let target = id("01a1dc8a-17c0-7000-a698-4080bab12a02");
    let mut env = Envelope::new(extra, "session", 1, Timestamp::from_millis(NOW), "0.5.0");
    env.set_body(&serde_json::json!({ "title": "x", "mode": "chat", "state": "active" }))
        .expect("a body");
    raw_put_envelope(&new, &env);
    raw_put_link(&new, target, "in_session", extra);
    raw_put_lookup(&new, "session", "updated_at", "x", extra);
    raw_set_meta(
        &new,
        meta::IMPORT_V04_ENDED,
        "{\"v\":1,\"done\":true,\"values\":[]}",
    );
    let (_, plan) = evorag_memory::v05_root();
    let (store, _) = Store::open_with(store_options(&new), plan.clone()).expect("reopen");
    let scratch = TempDir::new("import-rebuild-diff");
    let diff = rebuild_check(&store, store_options(&scratch.path().join("rebuilt")), plan)
        .expect("rebuild from the journal");
    assert!(diff.records >= 1, "{diff:?}");
    assert!(diff.links >= 1, "{diff:?}");
    assert!(diff.lookups >= 1, "{diff:?}");
    assert_eq!(diff.meta, vec![meta::IMPORT_V04_ENDED.to_owned()]);
}

/// 危ない置き場所は、古い根に触れる前に断る (03 §2.2・§12.4 の (5))。
#[test]
fn an_unsafe_location_is_refused_before_touching_the_root() {
    let (_tmp, old) = copied_fixture("import-unsafe");
    let mut opts = options(&old);
    opts.location = |_| Verdict::Unsafe("a synced folder".into());
    assert!(matches!(run(&opts), Err(ImportError::UnsafeLocation(_))));
    assert!(
        !old.join("lock").exists(),
        "the old root is not even locked"
    );
    assert!(!new_root_of(&old).exists());
}

/// 前の取り込みの新しい根が残っていれば、触れずに断る (片付けは入れ替えの段が行う)。
#[test]
fn a_leftover_import_folder_is_not_overwritten() {
    let (_tmp, old) = copied_fixture("import-leftover");
    let leftover = new_root_of(&old);
    std::fs::create_dir_all(&leftover).expect("make a leftover");
    std::fs::write(leftover.join("marker"), b"x").expect("write");
    assert!(matches!(run(&options(&old)), Err(ImportError::Leftover(_))));
    assert_eq!(std::fs::read(leftover.join("marker")).expect("kept"), b"x");
}

/// 取り込み済みの新しい根を古い根として指したときは、取り込みは要らないと示して断る。
#[test]
fn an_already_imported_root_is_refused() {
    let (_tmp, old) = copied_fixture("import-twice");
    let (new, _, _) = import(&old);
    let r = assert_refused(
        &new,
        |r| matches!(r, Refusal::NewerFormat { .. }),
        NextStep::NothingToImport,
    );
    assert!(r.to_string().contains("0.5"));
}
