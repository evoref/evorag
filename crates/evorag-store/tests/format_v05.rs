//! v0.5 の新しいデータ根の試験: 形式の版 (03 §3.1.1・§12.4)、ジャーナルの組の行 (03 §6)、中身から作る ID
//! (03 §3.3)、退避と書き出しの範囲 (03 §7.1・§11)。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_store::export::{ExportOptions, NoChecks, export};
use evorag_store::key::OwnedPart;
use evorag_store::migrate::FormatPlan;
use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_store::{
    DB_FILE, Envelope, FORMAT_V05, JOURNAL_DIR, JournalCut, KindSpec, LookupRange, OpenError,
    OpenOptions, OpenReport, Read, RecordIds, Store, StoreError, WriteError, WriteOp, WriteRequest,
    Writer, meta,
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
/// 改訂の番号 `rev` を持つ、ジャーナルに書く種類 (ターンに倣う)。
const TURN: KindSpec = KindSpec {
    journaled: true,
    create_when_newer_present: true,
    monotonic: Some("rev"),
    ..KindSpec::new("turn", 1)
};

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
        options(root, vec![SAMPLE, UNIT, LEDGER, TURN], 5),
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

/// v0.5 の根の印 (作るときに最初に書く `format.version` の `meta` の行、03 §6) か。
fn is_root_mark(line: &str) -> bool {
    line.contains("\"op\":\"meta\"") && line.contains("\"key\":\"format.version\"")
}

/// ジャーナルの行。v0.5 の根の印の行は除く (試験が書いた行だけを見る)。
fn journal_lines(root: &Path) -> Vec<String> {
    std::fs::read_to_string(journal_file(root))
        .unwrap()
        .lines()
        .filter(|l| !is_root_mark(l))
        .map(str::to_owned)
        .collect()
}

/// ジャーナルを `lines` で書き直す。今のファイルにある v0.5 の根の印の行は、先頭に残す。
fn write_journal_lines(root: &Path, lines: &[String]) {
    let file = journal_file(root);
    let marks: Vec<String> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .filter(|l| is_root_mark(l))
        .map(str::to_owned)
        .collect();
    let mut text = marks
        .iter()
        .chain(lines)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    std::fs::write(file, text).unwrap();
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
    // `ids` は組の最初の行だけが持つ (ジャーナルを組の行の数の 2 乗で太らせない)。組の ID は最初の行の連番 (連番 1
    // は、v0.5 の根を作るときに書く印の行、03 §6)。
    assert!(
        lines[1].contains(&format!(
            ",\"grp\":{{\"id\":3,\"n\":3,\"ids\":[\"{}\",\"{}\",\"{}\"]}},\"sum\":\"",
            ids[1], ids[2], ids[3]
        )),
        "{}",
        lines[1]
    );
    for line in &lines[2..] {
        assert!(
            line.contains(",\"grp\":{\"id\":3,\"n\":3},\"sum\":\""),
            "{line}"
        );
    }
}

/// v0.5 でない根では、`GroupJournalLines` があっても組にしない (03 §6)。
/// verifies: 03 §6 (組の行は v0.5 の根だけで書く)
#[test]
fn lines_are_not_grouped_outside_a_v05_root() {
    let dir = TempDir::new("v05-group-old-root");
    let (store, _) = Store::open(options(dir.path(), vec![SAMPLE], 12)).unwrap();
    let store = Arc::new(store);
    let ids: Vec<Id> = (0..2).map(|_| store.ids().next_id()).collect();
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![
            WriteOp::Put(record(&store, "sample", ids[0], "1")),
            WriteOp::Put(record(&store, "sample", ids[1], "2")),
            WriteOp::GroupJournalLines,
        ])],
    );
    assert!(
        journal_lines(dir.path())
            .iter()
            .all(|l| !l.contains("\"grp\""))
    );
}

/// 末尾の組の行がそろわない (書きかけの追記の途中で止まった) ときは、組を当てずに捨てる。削除の行も当てない。
/// verifies: 03 §6 (書き手が使っていた DB の、`journal.last_seq` より後の末尾の書きかけの組は捨てる)
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

/// 行の欠けた組は、読める行を当てて数える (DB を失って作り直すとき)。
/// verifies: 03 §6 (組がそろわないときは読める行を当て、欠けた組を数えて `doctor` が報告する)
#[test]
fn an_incomplete_group_before_the_end_applies_its_readable_lines() {
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
    assert!(has(&store, "sample", ids[0]) && has(&store, "sample", ids[2]));
    assert!(!has(&store, "sample", ids[1]));
    // 欠けた組の記録を、当て直しの印に残す。
    assert_eq!(store.partial_groups().unwrap().ids, [ids[0], ids[2]]);
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

/// verifies: 03 §7.1 (v0.5 の根の退避に単位と `memory.derive_fingerprint` を写さない), 03 §11 (単位を書き出さない)
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

#[derive(Serialize)]
struct TurnBody {
    text: String,
    rev: u32,
}

fn turn(store: &Store, id: Id, text: &str, rev: u32) -> Envelope {
    let mut e = Envelope::new(id, "turn", 1, store.clock().now(), "");
    e.set_body(&TurnBody {
        text: text.into(),
        rev,
    })
    .unwrap();
    e
}

fn rev_of(store: &Store, id: Id) -> Option<u64> {
    match store.read().unwrap().get("turn", id).unwrap() {
        Some(Read::Ok(e)) => e.body_uint("rev"),
        _ => None,
    }
}

/// ジャーナルの、`needle` を含む行のチェックサムを壊す。
fn corrupt_line(root: &Path, needle: &str) {
    let lines: Vec<String> = journal_lines(root)
        .into_iter()
        .map(|l| {
            if !l.contains(needle) {
                return l;
            }
            // `sum` の最後の桁を変える (行の形は保つ)。
            let at = l.len() - 3;
            let digit = if &l[at..=at] == "0" { "1" } else { "0" };
            format!("{}{digit}{}", &l[..at], &l[at + 1..])
        })
        .collect();
    write_journal_lines(root, &lines);
}

/// 忘れるの組の後に起きたこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// 同じターンをもう一度忘れた: 後の組の書き直しで、前の組の伏せ字のターンの行を除いた。
    ForgotAgain,
    /// ターンを消した: 削除の書き直しで、前の組の伏せ字のターンの行を除いた。
    Deleted,
    /// 理由なく、前の組の伏せ字のターンの行が失われた。
    Lost,
}

/// ターンを書いて退避を取り、忘れるの組 (指示 → 伏せ字にしたターン) を書き、`after` のことを起こす。退避のフォルダ、
/// 前の組の指示、ターンの ID を返す。
fn forget_then(root: &Path, after: After) -> (PathBuf, Id, Id) {
    let (store, _) = open_v05(root);
    let t = store.ids().next_id();
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![WriteOp::Put(turn(
            &store,
            t,
            "見本の町に住んでいます",
            0,
        ))])],
    );
    drop(store);
    let (store, _) = open_v05(root);
    let backup = store.daily_backup(None).unwrap().unwrap();
    let directive = store.ids().next_id();
    let mut reqs = vec![WriteRequest::conversation(vec![
        WriteOp::Put(record(&store, "sample", directive, "1 回目の忘れる")),
        WriteOp::Put(turn(&store, t, "[伏せ字]に住んでいます", 1)),
        WriteOp::GroupJournalLines,
        WriteOp::RewriteJournal([(t, JournalCut::Below(1))].into()),
    ])];
    match after {
        After::ForgotAgain => {
            let second = store.ids().next_id();
            reqs.push(WriteRequest::conversation(vec![
                WriteOp::Put(record(&store, "sample", second, "2 回目の忘れる")),
                WriteOp::Put(turn(&store, t, "[伏せ字]に[伏せ字]", 2)),
                WriteOp::GroupJournalLines,
                WriteOp::RewriteJournal([(t, JournalCut::Below(2))].into()),
            ]));
        }
        After::Deleted => reqs.push(WriteRequest::conversation(vec![WriteOp::Delete {
            kind: "turn".into(),
            id: t,
        }])),
        After::Lost => {}
    }
    write_all(&store, reqs);
    drop(store);
    // どの場合も、前の組の伏せ字のターンの行はジャーナルに無い。
    let mut lines = journal_lines(root);
    let first_group = lines
        .iter()
        .position(|l| l.contains(&directive.to_string()))
        .unwrap();
    let turn_line = first_group + 1;
    if after == After::Lost {
        assert!(lines[turn_line].contains(&t.to_string()));
        lines.remove(turn_line);
        // 理由なく失われた後にも、ほかの行が続く (末尾ではない)。
        write_journal_lines(root, &lines);
        let (store, _) = open_v05(root);
        write_all(
            &store,
            vec![WriteRequest::conversation(vec![WriteOp::Put(record(
                &store,
                "sample",
                store.ids().next_id(),
                "後の行",
            ))])],
        );
    } else {
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("\"rev\":1") && l.contains(&t.to_string())),
            "the redacted line of the first group was rewritten away"
        );
    }
    (backup, directive, t)
}

/// 書き直しで正しく除いた行の欠けた組は、そろった組として当てる。理由なく欠けた組も、読める行 (忘れるの指示) を当てて
/// 当て直しの印を残す (restore の経路)。
/// verifies: 03 §6 (組の行の欠けを、後の削除の行か同じ ID の後の `put` の行で、正しく除いたものと見分ける。そろわない組の読める行を当てる)
#[test]
fn a_group_whose_line_was_rewritten_away_is_still_applied_on_restore() {
    for after in [After::ForgotAgain, After::Deleted, After::Lost] {
        let dir = TempDir::new("v05-group-restore");
        let (backup, directive, t) = forget_then(dir.path(), after);
        evorag_store::restore::restore(dir.path(), &backup, T0, None, &|_| false).unwrap();
        let (store, report) = open_v05(dir.path());
        assert!(has(&store, "sample", directive), "{after:?}");
        assert_eq!(
            report.journal_incomplete_groups,
            usize::from(after == After::Lost),
            "{after:?}"
        );
        let want = match after {
            After::ForgotAgain => Some(2),
            After::Deleted => None,
            // 伏せ字のターンの行は失われたので、退避のときの本文のまま。忘れるの指示は戻ったので、当て直し (restore
            // の後の `forget.recheck`) の対象になり、行の欠けた組の印にも残る。
            After::Lost => Some(0),
        };
        assert_eq!(rev_of(&store, t), want, "{after:?}");
        if after == After::Lost {
            assert_eq!(store.partial_groups().unwrap().ids, [directive]);
            assert_eq!(
                store.meta(meta::FORGET_RECHECK).unwrap().as_deref(),
                Some(meta::FORGET_RECHECK_SET)
            );
        }
        // 開くときの取り込みで、末尾が分からない印は消える。
        assert_eq!(meta_of(&store, meta::JOURNAL_TAIL_UNKNOWN), None);
    }
}

/// 同じ判定を、ジャーナルから記録を戻す `repair` の経路でも当てる。
/// verifies: 03 §6 (組の行の欠けの判定は `repair` の読み方でも同じ), 03 §10 (`repair` はジャーナルから失った記録を戻す)
#[test]
fn a_group_whose_line_was_rewritten_away_is_still_applied_by_repair() {
    for after in [After::ForgotAgain, After::Deleted, After::Lost] {
        let dir = TempDir::new("v05-group-repair");
        let (_, directive, _) = forget_then(dir.path(), after);
        evorag_store::testutil::raw_remove_record(dir.path(), "sample", directive);
        let (store, _) = open_v05(dir.path());
        assert!(!has(&store, "sample", directive));
        let repaired = store.repair(|_| false).unwrap();
        assert!(has(&store, "sample", directive), "{after:?}");
        assert_eq!(
            repaired.journal_incomplete_groups,
            usize::from(after == After::Lost),
            "{after:?}"
        );
        // 作り直した DB は、末尾の組をコミットしたか分からない。
        assert_eq!(
            meta_of(&store, meta::JOURNAL_TAIL_UNKNOWN).as_deref(),
            Some("1")
        );
    }
}

/// 組の壊れ方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// コミットした末尾の組の、最後の行 (伏せ字のターン) が壊れた。
    LastLineCorrupt,
    /// 組の途中の行だけが欠けた。
    MiddleLineMissing,
    /// 組の最後の行が無く、その後ろにチェックサムの合わない行がある。
    BadLineAfterTheGroup,
    /// 削除の組の、`delete` の後の行が壊れた。
    DeleteGroupLineCorrupt,
}

/// DB を戻す経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// 組の前に取った退避から戻す。
    Restore,
    /// DB を失い、ジャーナルだけから作り直す。
    LostDatabase,
    /// 組を書く前の書き手の DB のまま開く (組の追記のコミットの前に止まったときの DB)。
    WriterDatabase,
}

struct Damaged {
    /// 組の前に取った退避。
    backup: PathBuf,
    /// 組を書く前の DB のバイト列。
    db_before: Vec<u8>,
    /// 忘れるの組の指示 (と、途中の行の指示)。削除の組では消す記録と、組の 2 行目の記録。
    first: Id,
    second: Id,
    turn: Id,
}

fn damaged(root: &Path, damage: Damage) -> Damaged {
    let (store, _) = open_v05(root);
    let (t, x) = (store.ids().next_id(), store.ids().next_id());
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![
            WriteOp::Put(turn(&store, t, "見本の町に住んでいます", 0)),
            WriteOp::Put(record(&store, "sample", x, "消す記録")),
        ])],
    );
    drop(store);
    let db_before = std::fs::read(root.join(DB_FILE)).unwrap();
    let (store, _) = open_v05(root);
    let backup = store.daily_backup(None).unwrap().unwrap();
    let (first, second) = (store.ids().next_id(), store.ids().next_id());
    let req = if damage == Damage::DeleteGroupLineCorrupt {
        WriteRequest::conversation(vec![
            WriteOp::Delete {
                kind: "sample".into(),
                id: x,
            },
            WriteOp::Put(record(&store, "sample", second, "削除で足す指示")),
            WriteOp::GroupJournalLines,
        ])
    } else {
        WriteRequest::conversation(vec![
            WriteOp::Put(record(&store, "sample", first, "忘れるの指示")),
            WriteOp::Put(record(&store, "sample", second, "end の指示")),
            WriteOp::Put(turn(&store, t, "[伏せ字]に住んでいます", 1)),
            WriteOp::GroupJournalLines,
            WriteOp::RewriteJournal([(t, JournalCut::Below(1))].into()),
        ])
    };
    write_all(&store, vec![req]);
    drop(store);
    let lines = journal_lines(root);
    match damage {
        Damage::LastLineCorrupt => corrupt_line(root, "[伏せ字]に住んでいます"),
        Damage::MiddleLineMissing => {
            let kept: Vec<String> = lines
                .into_iter()
                .filter(|l| !l.contains(&format!("\"id\":\"{second}\"")))
                .collect();
            write_journal_lines(root, &kept);
        }
        Damage::BadLineAfterTheGroup => {
            let mut kept: Vec<String> = lines
                .into_iter()
                .filter(|l| !l.contains("[伏せ字]に住んでいます"))
                .collect();
            kept.push("{\"seq\":99,\"op\":\"put\",\"env\":{},\"sum\":\"0000000000000000\"}".into());
            write_journal_lines(root, &kept);
        }
        Damage::DeleteGroupLineCorrupt => corrupt_line(root, &format!("\"id\":\"{second}\"")),
    }
    Damaged {
        backup,
        db_before,
        first: if damage == Damage::DeleteGroupLineCorrupt {
            x
        } else {
            first
        },
        second,
        turn: t,
    }
}

/// 組の 1 行が壊れた・欠けたジャーナルから DB を戻すと、組の読める行を当てる。組の中の `delete` の行はいつも削除として
/// 扱い、伏せ字の前の本文の行は戻らない。書き手の DB のまま開いたときに、末尾の書きかけと見なして捨てるのは、読める行が
/// 組の頭から続き、後ろに壊れた行が無いときだけ。
/// verifies: 03 §6 (そろわない組の読める行を当てる。組の中の `delete` の行はいつも削除。切り詰めるのは書き手の DB の末尾の書きかけだけ)
#[test]
fn a_damaged_group_applies_its_readable_lines_on_every_way_back() {
    for damage in [
        Damage::LastLineCorrupt,
        Damage::MiddleLineMissing,
        Damage::BadLineAfterTheGroup,
        Damage::DeleteGroupLineCorrupt,
    ] {
        for way in [Way::Restore, Way::LostDatabase, Way::WriterDatabase] {
            // 末尾の最後の行が壊れただけの組は、書き手の DB では書きかけと区別できない (壊れた行は組の後ろの行として
            // 数えるので、捨てない)。
            let dir = TempDir::new("v05-damaged-group");
            let d = damaged(dir.path(), damage);
            match way {
                Way::Restore => {
                    evorag_store::restore::restore(dir.path(), &d.backup, T0, None, &|_| false)
                        .unwrap();
                }
                Way::LostDatabase => std::fs::remove_file(dir.path().join(DB_FILE)).unwrap(),
                Way::WriterDatabase => {
                    std::fs::write(dir.path().join(DB_FILE), &d.db_before).unwrap()
                }
            }
            let lines_before = journal_lines(dir.path()).len();
            let (store, report) = open_v05(dir.path());
            let case = format!("{damage:?} {way:?}");
            assert_eq!(report.journal_torn_groups, 0, "{case}");
            assert_eq!(report.journal_incomplete_groups, 1, "{case}");
            assert_eq!(
                journal_lines(dir.path()).len(),
                lines_before,
                "{case}: not cut"
            );
            match damage {
                Damage::DeleteGroupLineCorrupt => {
                    assert!(
                        !has(&store, "sample", d.first),
                        "{case}: the delete applies"
                    );
                }
                Damage::MiddleLineMissing => {
                    assert!(has(&store, "sample", d.first), "{case}");
                    assert!(!has(&store, "sample", d.second), "{case}");
                    assert_eq!(rev_of(&store, d.turn), Some(1), "{case}");
                }
                Damage::LastLineCorrupt | Damage::BadLineAfterTheGroup => {
                    assert!(has(&store, "sample", d.first), "{case}");
                    assert!(has(&store, "sample", d.second), "{case}");
                    // 伏せ字の前の行は書き直しで除いたので、ジャーナルからは戻らない (退避と書き手の DB には残り、
                    // 戻った忘れるの指示の当て直しの対象になる)。DB を失ったときは、ターンが無い。
                    let want = if way == Way::LostDatabase {
                        None
                    } else {
                        Some(0)
                    };
                    assert_eq!(rev_of(&store, d.turn), want, "{case}");
                    assert!(
                        store.partial_groups().unwrap().ids.contains(&d.first),
                        "{case}"
                    );
                }
            }
        }
    }
}

/// 同じ壊れ方のジャーナルを `repair` で当てても、読める行を戻し、削除した記録は戻さない。
/// verifies: 03 §6 (`repair` もそろわない組の読める行を当てる), 03 §10
#[test]
fn a_damaged_group_applies_its_readable_lines_on_repair() {
    for damage in [
        Damage::LastLineCorrupt,
        Damage::MiddleLineMissing,
        Damage::BadLineAfterTheGroup,
        Damage::DeleteGroupLineCorrupt,
    ] {
        let dir = TempDir::new("v05-damaged-repair");
        let d = damaged(dir.path(), damage);
        if damage != Damage::DeleteGroupLineCorrupt {
            evorag_store::testutil::raw_remove_record(dir.path(), "sample", d.first);
        }
        let (store, _) = open_v05(dir.path());
        let repaired = store.repair(|_| false).unwrap();
        assert_eq!(repaired.journal_incomplete_groups, 1, "{damage:?}");
        assert_eq!(
            has(&store, "sample", d.first),
            damage != Damage::DeleteGroupLineCorrupt,
            "{damage:?}"
        );
    }
}

/// 中身から ID を作る種類は、消してから同じ ID で作り直せる (03 §4.2 の規則 0 の例外)。ほかの種類は作り直せない。
/// verifies: 03 §4.2 の規則 0 (中身から ID を決める派生の種類は、消した ID を使い直す)
#[test]
fn a_content_id_record_can_be_written_again_after_it_was_deleted() {
    let dir = TempDir::new("v05-content-id-again");
    let (store, _) = open_v05(dir.path());
    let unit = Id::from_bytes([0x6c; 16]);
    let sample = store.ids().next_id();
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for (kind, id) in [("unit", unit), ("sample", sample)] {
        handle
            .submit_wait(WriteRequest::conversation(vec![WriteOp::Put(record(
                &store, kind, id, "1 回目",
            ))]))
            .unwrap();
        handle
            .submit_wait(WriteRequest::conversation(vec![WriteOp::Delete {
                kind: kind.into(),
                id,
            }]))
            .unwrap();
    }
    handle
        .submit_wait(WriteRequest::background(vec![WriteOp::Put(record(
            &store,
            "unit",
            unit,
            "導き直し",
        ))]))
        .unwrap();
    let again = handle.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(record(
        &store,
        "sample",
        sample,
        "蘇らせない",
    ))]));
    assert!(matches!(
        again,
        Err(WriteError::Store(StoreError::Deleted { .. }))
    ));
    handle.shutdown();
    t.join().unwrap();
    assert!(has(&store, "unit", unit));
    assert!(!has(&store, "sample", sample));
}

/// 外で読んだ記録の本体が、書き手のトランザクションの中で変わっていれば、依頼の全体を断る (04 §2)。
/// verifies: 04 §2 (区切りを書き換えるときは、読んだときの本体と同じであることを確かめる)
#[test]
fn an_expected_body_that_changed_refuses_the_request() {
    let dir = TempDir::new("v05-expect-body");
    let (store, _) = open_v05(dir.path());
    let id = store.ids().next_id();
    let first = record(&store, "sample", id, "読んだとき");
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    let expect = |body: Option<[u8; 32]>| WriteOp::ExpectBody {
        kind: "sample".into(),
        id,
        body,
    };
    // 無いはずの記録が無い。
    handle
        .submit_wait(WriteRequest::conversation(vec![
            expect(None),
            WriteOp::Put(first.clone()),
        ]))
        .unwrap();
    // その間に書き換えられた。
    handle
        .submit_wait(WriteRequest::conversation(vec![WriteOp::Put(record(
            &store,
            "sample",
            id,
            "その間に閉じた",
        ))]))
        .unwrap();
    for op in [expect(Some(first.body_digest())), expect(None)] {
        let stale = handle.submit_wait(WriteRequest::conversation(vec![
            op,
            WriteOp::Put(record(&store, "sample", id, "古い本体から")),
        ]));
        assert!(matches!(
            stale,
            Err(WriteError::Store(StoreError::Stale { .. }))
        ));
    }
    handle.shutdown();
    t.join().unwrap();
    let rtx = store.read().unwrap();
    let Some(Read::Ok(now)) = rtx.get("sample", id).unwrap() else {
        panic!("the record is there");
    };
    assert!(now != first);
}

/// 退避は、元の種類が単位の隔離の記録も写さない (03 §7.1)。
/// verifies: 03 §7.1 (v0.5 の根の退避に単位を写さない。隔離した単位も)
#[test]
fn a_quarantined_unit_is_not_backed_up() {
    let dir = TempDir::new("v05-quarantined-unit");
    let (store, _) = open_v05(dir.path());
    let unit = Id::from_bytes([0x7d; 16]);
    write_all(
        &store,
        vec![WriteRequest::background(vec![WriteOp::Put(record(
            &store,
            "unit",
            unit,
            "壊れる単位",
        ))])],
    );
    drop(store);
    evorag_store::testutil::raw_corrupt_record(dir.path(), "unit", unit);
    let (store, _) = open_v05(dir.path());
    // 読んで壊れていると分かった記録は、次の書き込みの前に隔離される (03 §8)。
    assert!(matches!(
        store.read().unwrap().get("unit", unit).unwrap(),
        Some(Read::Corrupt { .. })
    ));
    write_all(
        &store,
        vec![WriteRequest::conversation(vec![WriteOp::Put(record(
            &store,
            "sample",
            store.ids().next_id(),
            "書き込み",
        ))])],
    );
    let quarantined = store
        .census()
        .unwrap()
        .known
        .get("quarantine")
        .map_or(0, |c| c.ok);
    assert_eq!(quarantined, 1);
    let backup = store.daily_backup(None).unwrap().unwrap().join(DB_FILE);
    let (_dir, copy) = open_copy(&backup);
    let census = copy.census().unwrap();
    assert_eq!(census.known.get("quarantine").map_or(0, |c| c.ok), 0);
    assert_eq!(census.known.get("unit").map_or(0, |c| c.ok + c.corrupt), 0);
}

/// 取り込みの `meta` のキー (03 §3.1.1): `import.source` は一度だけ書き、`import.old_root`・`import.v04_ended` は
/// 上書きしてよく (処分した印は `kept` に戻さない)、どれもジャーナルにも書く。`import.report` はジャーナルに書かない。
/// ジャーナルだけから作り直すと、上書きしてよいキーは連番の最も大きい行の値になる。
#[test]
fn import_meta_keys_are_journaled_and_replayed_latest_first() {
    let dir = TempDir::new("v05-import-meta");
    let root = dir.path().join("root");
    let (store, _) = open_v05(&root);
    let set = |key: &str, value: &str| WriteOp::SetMetaLatest {
        key: key.into(),
        value: value.into(),
    };
    let once = |key: &str, value: &str| WriteOp::SetMetaOnce {
        key: key.into(),
        value: value.into(),
    };
    // 書き手はストアごとに 1 つ (02 §5)。
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for req in [
        WriteRequest::conversation(vec![
            once(meta::IMPORT_SOURCE, "{\"v\":1}"),
            set(meta::IMPORT_OLD_ROOT, meta::IMPORT_OLD_ROOT_KEPT),
            set(meta::IMPORT_V04_ENDED, "{\"done\":false,\"values\":[]}"),
        ]),
        WriteRequest::conversation(vec![set(
            meta::IMPORT_V04_ENDED,
            "{\"done\":true,\"values\":[]}",
        )]),
        WriteRequest::conversation(vec![set(meta::IMPORT_OLD_ROOT, "disposed:1")]),
        WriteRequest::conversation(vec![once(meta::IMPORT_REPORT, "{}")]),
    ] {
        handle.submit_wait(req).unwrap();
    }
    // 処分した印は戻せず、ほかのキーは上書きの口で書けず、一度だけ書くキーは書き換えられない。
    for (op, conflict) in [
        (set(meta::IMPORT_OLD_ROOT, meta::IMPORT_OLD_ROOT_KEPT), true),
        (set(meta::PROMPT_LANGUAGE, "en"), false),
        // 表 (`values`) は変えられない (書き換えてよいのは `done` だけ)。
        (
            set(meta::IMPORT_V04_ENDED, "{\"done\":true,\"values\":[1]}"),
            true,
        ),
        (once(meta::IMPORT_SOURCE, "{\"v\":2}"), true),
    ] {
        match handle.submit_wait(WriteRequest::conversation(vec![op])) {
            Err(WriteError::Store(StoreError::MetaConflict { .. })) if conflict => {}
            Err(WriteError::Store(StoreError::MetaNotWritable(_))) if !conflict => {}
            other => panic!("expected a refusal (conflict {conflict}), got {other:?}"),
        }
    }
    handle.shutdown();
    t.join().unwrap();
    assert_eq!(
        meta_of(&store, meta::IMPORT_OLD_ROOT).as_deref(),
        Some("disposed:1")
    );
    let lines = journal_lines(&root);
    assert_eq!(lines.len(), 5, "{lines:#?}");
    assert!(lines.iter().all(|l| !l.contains(meta::IMPORT_REPORT)));
    let file = journal_file(&root);
    drop(store);

    // ジャーナルだけから作り直す。
    let rebuilt = dir.path().join("rebuilt");
    std::fs::create_dir_all(rebuilt.join(JOURNAL_DIR)).unwrap();
    std::fs::copy(
        &file,
        rebuilt.join(JOURNAL_DIR).join(file.file_name().unwrap()),
    )
    .unwrap();
    // `import.source` があって `import.report` が無い根は、作りかけの取り込みとして通常の開き方では開かない
    // (03 §12.4)。取り込みの照合の入口では開ける。
    let refused = Store::open_with(
        options(&rebuilt, vec![SAMPLE, UNIT, LEDGER, TURN], 5),
        FormatPlan::v05(),
    );
    assert!(matches!(refused, Err(OpenError::UnfinishedImport)));
    let lock = evorag_store::lock::DataRootLock::acquire(&rebuilt).unwrap();
    let (store, _) = Store::open_for_import(
        options(&rebuilt, vec![SAMPLE, UNIT, LEDGER, TURN], 5),
        FormatPlan::v05(),
        lock,
    )
    .unwrap();
    assert_eq!(
        meta_of(&store, meta::IMPORT_SOURCE).as_deref(),
        Some("{\"v\":1}")
    );
    assert_eq!(
        meta_of(&store, meta::IMPORT_OLD_ROOT).as_deref(),
        Some("disposed:1")
    );
    assert_eq!(
        meta_of(&store, meta::IMPORT_V04_ENDED).as_deref(),
        Some("{\"done\":true,\"values\":[]}")
    );
    assert_eq!(meta_of(&store, meta::IMPORT_REPORT), None);
}
