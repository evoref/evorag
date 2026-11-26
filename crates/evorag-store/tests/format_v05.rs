//! v0.5 の新しいデータ根の試験: 形式の版 (03 §3.1.1・§12.4)、ジャーナルの組の行 (03 §6)、中身から作る ID
//! (03 §3.3)、退避と書き出しの範囲 (03 §7.1・§11)。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_store::export::{ExportOptions, NoChecks, export};
use evorag_store::key::OwnedPart;
use evorag_store::migrate::FormatPlan;
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_store::{
    DB_FILE, Envelope, FORMAT_V05, JOURNAL_DIR, KindSpec, LookupRange, OpenError, OpenOptions,
    OpenReport, Read, RecordIds, Store, WriteOp, WriteRequest, Writer, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::Serialize;

/// 会話の追記に倣う、ジャーナルに書く種類。
const SAMPLE: KindSpec = KindSpec {
    journaled: true,
    create_when_newer_present: true,
    ..KindSpec::new("sample", 1)
};
/// 中身から ID を作る派生の種類 (v0.5 の単位に倣う)。
const UNIT: KindSpec = KindSpec {
    ids: RecordIds::Content,
    ..KindSpec::new("unit", 1)
};
/// 作り直せないが、ジャーナルに書かない派生の種類 (v0.5 の台帳に倣う)。
const LEDGER: KindSpec = KindSpec::new("proposal", 1);

const T0: i64 = 1_767_614_400_000;

#[derive(Serialize)]
struct Body {
    text: String,
}

fn options(root: &Path, kinds: Vec<KindSpec>, seed: u64) -> OpenOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(T0)));
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.5.0".into(),
        kinds,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    }
}

fn open_v05(root: &Path) -> (Arc<Store>, OpenReport) {
    let (store, report) = Store::open_with(
        options(root, vec![SAMPLE, UNIT, LEDGER], 5),
        FormatPlan::v05(),
    )
    .unwrap();
    (Arc::new(store), report)
}

fn record(store: &Store, kind: &str, id: Id, text: &str) -> Envelope {
    let mut e = Envelope::new(id, kind, 1, store.clock().now(), "");
    e.set_body(&Body { text: text.into() }).unwrap();
    e
}

/// 書き手で依頼を 1 つずつ書く。
fn write_all(store: &Arc<Store>, reqs: Vec<WriteRequest>) {
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for req in reqs {
        handle.submit_wait(req).unwrap();
    }
    handle.shutdown();
    t.join().unwrap();
}

fn journal_file(root: &Path) -> PathBuf {
    let dir = root.join(JOURNAL_DIR);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "{files:?}");
    files.remove(0)
}

fn journal_lines(root: &Path) -> Vec<String> {
    std::fs::read_to_string(journal_file(root))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_journal_lines(root: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(journal_file(root), text).unwrap();
}

fn has(store: &Store, kind: &str, id: Id) -> bool {
    matches!(
        store.read().unwrap().get(kind, id).unwrap(),
        Some(Read::Ok(_))
    )
}

fn meta_of(store: &Store, key: &str) -> Option<String> {
    store.meta(key).unwrap()
}

/// verifies: 03 §3.1.1 (`format.version`・`min_read`・`min_write` を `0.5` にする), 03 §12.4 (v0.4 以前の版は新しい根を読むだけでも開かない)
#[test]
fn a_new_v05_root_is_written_in_format_0_5_and_older_versions_cannot_open_it() {
    let dir = TempDir::new("v05-new-root");
    let (store, report) = open_v05(dir.path());
    assert!(report.created);
    assert!(store.is_v05_root());
    assert_eq!(store.format_version(), FORMAT_V05);
    for key in [
        meta::FORMAT_VERSION,
        meta::FORMAT_MIN_READ,
        meta::FORMAT_MIN_WRITE,
    ] {
        assert_eq!(meta_of(&store, key).as_deref(), Some("0.5"), "{key}");
    }
    drop(store);
    // v0.4 の形式 (0.3) を書く版は、読むだけでも開けない。
    let old = Store::open(options(dir.path(), vec![SAMPLE], 6));
    assert!(
        matches!(old, Err(OpenError::TooNew { ref min_read, .. }) if min_read == "0.5"),
        "{:?}",
        old.err()
    );
    let (store, report) = open_v05(dir.path());
    assert!(!report.created && report.read_only.is_none());
    assert!(store.is_v05_root());
}

/// verifies: 03 §12.1 (v0.5 は在り場所の移行を持たない), 03 §12.4 (v0.4 の根は `import-v04` で取り込む。古い根に書かない)
#[test]
fn a_v04_root_is_not_migrated_in_place_by_the_v05_format() {
    let dir = TempDir::new("v05-old-root");
    let id = {
        let (store, _) = Store::open(options(dir.path(), vec![SAMPLE], 7)).unwrap();
        let store = Arc::new(store);
        let id = store.ids().next_id();
        write_all(
            &store,
            vec![WriteRequest::conversation(vec![WriteOp::Put(record(
                &store,
                "sample",
                id,
                "古い根",
            ))])],
        );
        id
    };
    let v05 = Store::open_with(options(dir.path(), vec![SAMPLE], 8), FormatPlan::v05());
    assert!(
        matches!(v05, Err(OpenError::NeedsImport { ref format }) if format == "0.3"),
        "{:?}",
        v05.err()
    );
    // 移行しない (移行の前の退避も作らない)。
    assert!(!dir.path().join("backups").join("pre-op").exists());
    let (store, report) = Store::open(options(dir.path(), vec![SAMPLE], 9)).unwrap();
    assert!(report.read_only.is_none() && report.migrated_from.is_none());
    assert!(has(&store, "sample", id));
    assert_eq!(
        meta_of(&store, meta::FORMAT_VERSION).as_deref(),
        Some("0.3")
    );
}

/// verifies: 03 §6 (組の行は同じ `grp` を持ち、`sum` の前に置く。1 回の追記の行だけを組にする)
#[test]
fn the_lines_of_one_operation_carry_the_same_group() {
    let dir = TempDir::new("v05-group-lines");
    let (store, _) = open_v05(dir.path());
    let ids: Vec<Id> = (0..4).map(|_| store.ids().next_id()).collect();
    let put = |i: usize| WriteOp::Put(record(&store, "sample", ids[i], "組"));
    write_all(
        &store,
        vec![
            WriteRequest::conversation(vec![put(0)]),
            WriteRequest::conversation(vec![put(1), put(2), put(3), WriteOp::GroupJournalLines]),
        ],
    );
    let lines = journal_lines(dir.path());
    assert_eq!(lines.len(), 4);
    assert!(!lines[0].contains("\"grp\""), "{}", lines[0]);
    for line in &lines[1..] {
        assert!(
            line.contains(",\"grp\":{\"id\":2,\"n\":3},\"sum\":\""),
            "{line}"
        );
    }
}

/// 末尾の組の行がそろわない (書きかけの追記の途中で止まった) ときは、組を当てずに捨てる。削除の行も当てない。
/// verifies: AC-FR-318-4 (組の行がそろわない追記は当たらない), 03 §6 (ファイルの末尾で組がそろわない組は捨てる)
#[test]
fn a_torn_group_at_the_end_of_the_journal_is_dropped_and_not_applied() {
    let dir = TempDir::new("v05-torn-group");
    let (store, _) = open_v05(dir.path());
    let ids: Vec<Id> = (0..3).map(|_| store.ids().next_id()).collect();
    let (kept, a, b) = (ids[0], ids[1], ids[2]);
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![WriteOp::Put(record(
            &store, "sample", kept, "残る",
        ))])],
    );
    drop(store);
    // 組を書く前の DB (組の追記のコミットの前に止まったときの DB)。
    let db_before = std::fs::read(dir.path().join(DB_FILE)).unwrap();
    let journal_before = std::fs::read(journal_file(dir.path())).unwrap();
    let (store, _) = open_v05(dir.path());
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![
            WriteOp::Put(record(&store, "sample", a, "組の 1 行目")),
            WriteOp::Put(record(&store, "sample", b, "組の 2 行目")),
            WriteOp::Delete {
                kind: "sample".into(),
                id: kept,
            },
            WriteOp::GroupJournalLines,
        ])],
    );
    drop(store);
    // 最後の行 (削除の行) を書く前に止まった: 組の 2 行だけがジャーナルにあり、DB にはコミットしていない (削除の
    // 書き直しもしていない)。
    let group: Vec<String> = journal_lines(dir.path())
        .into_iter()
        .filter(|l| l.contains("\"grp\""))
        .collect();
    assert_eq!(group.len(), 3);
    let mut torn = journal_before.clone();
    for line in &group[..2] {
        torn.extend_from_slice(line.as_bytes());
        torn.push(b'\n');
    }
    std::fs::write(journal_file(dir.path()), &torn).unwrap();
    std::fs::write(dir.path().join(DB_FILE), &db_before).unwrap();

    let summary = evorag_store::inspect::inspect(dir.path(), vec![SAMPLE, UNIT, LEDGER], None)
        .map(|r| r.journal)
        .unwrap();
    assert_eq!(
        (
            summary.torn_groups,
            summary.incomplete_groups,
            summary.pending
        ),
        (1, 0, 0)
    );
    let (store, report) = open_v05(dir.path());
    assert_eq!(report.journal_torn_groups, 1);
    assert_eq!(report.journal_incomplete_groups, 0);
    assert_eq!(report.journal_replayed, 0);
    assert!(report.journal_deletes.is_empty());
    assert!(
        has(&store, "sample", kept),
        "the torn delete is not applied"
    );
    assert!(!has(&store, "sample", a) && !has(&store, "sample", b));
    // 組の行はファイルから切り詰めた。
    assert!(std::fs::read(journal_file(dir.path())).unwrap() == journal_before);
    // 削除の行を当てていないので、残った記録を書き直せる。操作をやり直せば当たる。
    write_all(
        &store,
        vec![
            WriteRequest::conversation(vec![WriteOp::Put(record(
                &store,
                "sample",
                kept,
                "書き直し",
            ))]),
            WriteRequest::conversation(vec![
                WriteOp::Put(record(&store, "sample", a, "やり直し")),
                WriteOp::Delete {
                    kind: "sample".into(),
                    id: kept,
                },
                WriteOp::GroupJournalLines,
            ]),
        ],
    );
    drop(store);
    let (store, report) = open_v05(dir.path());
    assert_eq!(report.journal_torn_groups, 0);
    assert!(has(&store, "sample", a) && !has(&store, "sample", kept));
}

/// 末尾でない場所で行の欠けた組は、当てずに数える (取り込み直すとき、ほかの行は当てる)。
/// verifies: 03 §6 (末尾でない場所の欠けた組は、当てずに数えて `doctor` が報告する)
#[test]
fn an_incomplete_group_before_the_end_is_counted_and_not_applied() {
    let dir = TempDir::new("v05-incomplete-group");
    let (store, _) = open_v05(dir.path());
    let ids: Vec<Id> = (0..4).map(|_| store.ids().next_id()).collect();
    let put = |i: usize, text: &str| WriteOp::Put(record(&store, "sample", ids[i], text));
    write_all(
        &store,
        vec![
            WriteRequest::conversation(vec![
                put(0, "組の 1"),
                put(1, "組の 2"),
                put(2, "組の 3"),
                WriteOp::GroupJournalLines,
            ]),
            WriteRequest::conversation(vec![put(3, "後の行")]),
        ],
    );
    drop(store);
    // 組の真ん中の行が失われ (壊れた行として除かれた)、DB も失って、ジャーナルから作り直す。
    let mut lines = journal_lines(dir.path());
    lines.remove(1);
    write_journal_lines(dir.path(), &lines);
    std::fs::remove_file(dir.path().join(DB_FILE)).unwrap();

    let (store, report) = open_v05(dir.path());
    assert_eq!(report.journal_incomplete_groups, 1);
    assert_eq!(report.journal_torn_groups, 0);
    assert!(has(&store, "sample", ids[3]));
    for &id in &ids[..3] {
        assert!(!has(&store, "sample", id), "{id}");
    }
    // 欠けた組の行は消さずに残す。
    assert_eq!(journal_lines(dir.path()).len(), 3);
    drop(store);
    let summary = evorag_store::inspect::inspect(dir.path(), vec![SAMPLE, UNIT, LEDGER], None)
        .map(|r| r.journal)
        .unwrap();
    assert_eq!((summary.incomplete_groups, summary.pending), (1, 0));
}

/// verifies: 03 §3.3 (中身から ID を作る種類は `IdGen` の下限に入れない)
#[test]
fn content_ids_do_not_raise_the_id_floor() {
    let dir = TempDir::new("v05-content-ids");
    let (store, _) = open_v05(dir.path());
    let sample = store.ids().next_id();
    let unit = Id::from_bytes([0xfe; 16]);
    write_all(
        &store,
        vec![WriteRequest::background(vec![
            WriteOp::Put(record(&store, "sample", sample, "時刻の順")),
            WriteOp::Put(record(&store, "unit", unit, "中身から")),
        ])],
    );
    drop(store);
    let (store, _) = open_v05(dir.path());
    assert!(has(&store, "unit", unit));
    assert_eq!(store.max_record_id().unwrap(), Some(sample));
    let next = store.ids().next_id();
    assert!(next > sample && next < unit);
    assert_eq!(next.timestamp(), Timestamp::from_millis(T0));
}

/// 写しの中の、副索引の全件 (種類と項目ごと)。
fn lookups(store: &Store, kind: &str, field: &str) -> Vec<Id> {
    store
        .read()
        .unwrap()
        .lookup(
            kind,
            field,
            &LookupRange::Between {
                from: None,
                to: None,
            },
            false,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// 退避の DB を別のデータ根に置いて開く。
fn open_copy(backup_db: &Path) -> (TempDir, Arc<Store>) {
    let other = TempDir::new("v05-backup-copy");
    std::fs::copy(backup_db, other.path().join(DB_FILE)).unwrap();
    let (store, _) = open_v05(other.path());
    (other, store)
}

/// verifies: 03 §7.1 (v0.5 の根の退避に単位と `memory.derive_fingerprint` を写さない), 03 §11 (単位を書き出さない), AC-FR-313-2 (退避と書き出しに台帳が含まれる)
#[test]
fn units_and_the_derive_fingerprint_are_left_out_of_backups_and_exports() {
    let dir = TempDir::new("v05-backup-scope");
    let (store, _) = open_v05(dir.path());
    let (turn, ledger) = (store.ids().next_id(), store.ids().next_id());
    let unit = Id::from_bytes([0x5a; 16]);
    write_all(
        &store,
        vec![
            WriteRequest::conversation(vec![WriteOp::Put(record(&store, "sample", turn, "根拠"))]),
            WriteRequest::background_durable(vec![
                WriteOp::Put(record(&store, "proposal", ledger, "台帳")),
                WriteOp::PutLink {
                    from: ledger,
                    rel: "ledger_src".into(),
                    to: turn,
                    attrs: Vec::new(),
                },
                WriteOp::PutLookup {
                    kind: "proposal".into(),
                    field: "segment".into(),
                    value: OwnedPart::Id(turn),
                    id: ledger,
                },
            ]),
            WriteRequest::background(vec![
                WriteOp::Put(record(&store, "unit", unit, "単位")),
                WriteOp::PutLink {
                    from: unit,
                    rel: "unit_anchor".into(),
                    to: turn,
                    attrs: Vec::new(),
                },
                WriteOp::PutLookup {
                    kind: "unit".into(),
                    field: "vk".into(),
                    value: OwnedPart::Str("ab".repeat(32)),
                    id: unit,
                },
                WriteOp::PutBlob {
                    id: unit,
                    part: 0,
                    bytes: vec![1, 2, 3],
                },
            ]),
        ],
    );
    drop(store);
    raw_set_meta(dir.path(), meta::DERIVE_FINGERPRINT, "fp");
    let (store, _) = open_v05(dir.path());
    assert!(has(&store, "unit", unit));
    assert_eq!(
        meta_of(&store, meta::DERIVE_FINGERPRINT).as_deref(),
        Some("fp")
    );
    let daily = store.daily_backup(None).unwrap().unwrap().join(DB_FILE);
    let pre_op = store.pre_op_backup("rebuild", &[]).unwrap().join(DB_FILE);
    for backup in [daily, pre_op] {
        let (_dir, copy) = open_copy(&backup);
        let rtx = copy.read().unwrap();
        assert!(rtx.get("unit", unit).unwrap().is_none(), "{backup:?}");
        assert!(rtx.links_from(unit, "unit_anchor").unwrap().is_empty());
        assert!(rtx.links_to(turn, "unit_anchor").unwrap().is_empty());
        assert!(rtx.blob(unit, 0).unwrap().is_none());
        drop(rtx);
        assert!(lookups(&copy, "unit", "vk").is_empty());
        assert_eq!(meta_of(&copy, meta::DERIVE_FINGERPRINT), None);
        // 台帳と、その関係・副索引は写す。
        assert!(has(&copy, "proposal", ledger));
        assert_eq!(
            copy.read()
                .unwrap()
                .links_from(ledger, "ledger_src")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(lookups(&copy, "proposal", "segment"), [ledger]);
        assert!(has(&copy, "sample", turn));
    }
    drop(store);

    let out = dir.path().with_extension("export");
    let _ = std::fs::remove_dir_all(&out);
    let report = export(
        dir.path(),
        &out,
        &ExportOptions {
            evorag_version: "0.5.0",
            now_ms: T0,
            exclude: &[],
            checks: &NoChecks,
        },
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&out);
    assert_eq!(report.kinds.get("unit"), None);
    assert_eq!(report.kinds.get("proposal"), Some(&1));
    assert_eq!(report.kinds.get("sample"), Some(&1));
}
