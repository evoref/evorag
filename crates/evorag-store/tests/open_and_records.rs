//! データ根を開く・記録を読み書きする試験 (03 §3、§4.2、§8、§9)。

use std::path::Path;
use std::sync::Arc;

use evorag_store::key::KeyBuf;
use evorag_store::lock::LockError;
use evorag_store::testutil::TempDir;
use evorag_store::{
    DB_FILE, Durability, Envelope, KIND_QUARANTINE, KindSpec, OpenError, OpenOptions, Read,
    ReadOnlyReason, Store, StoreError, WriteToken, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

const SAMPLE: KindSpec = KindSpec::new("sample", 1);
const APPEND: KindSpec = KindSpec {
    name: "append",
    current_v: 1,
    create_when_newer_present: true,
};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Body {
    text: String,
}

fn opts(root: &Path, version: &str) -> OpenOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: version.to_owned(),
        kinds: vec![SAMPLE, APPEND],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
    }
}

fn open(root: &Path) -> Store {
    Store::open(opts(root, "0.0.2")).unwrap().0
}

fn new_record(store: &Store, kind: &KindSpec, text: &str) -> Envelope {
    let mut e = Envelope::new(
        store.ids().next_id(),
        kind.name,
        kind.current_v,
        store.clock().now(),
        "",
    );
    e.set_body(&Body { text: text.into() }).unwrap();
    e
}

fn put(store: &Store, tok: &WriteToken, e: &mut Envelope) {
    let mut tx = store.write(tok, Durability::Immediate).unwrap();
    tx.put(e).unwrap();
    tx.commit().unwrap();
}

fn record_key(kind: &str, id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind).id(id);
    k.into_bytes()
}

/// 閉じた DB を redb で直接いじる (試験だけ)。
fn with_raw_db(root: &Path, f: impl FnOnce(&redb::WriteTransaction)) {
    let db = redb::Database::create(root.join(DB_FILE)).unwrap();
    let tx = db.begin_write().unwrap();
    f(&tx);
    tx.commit().unwrap();
}

const RAW_META: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("meta");
const RAW_RECORDS: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("records");

fn set_meta(root: &Path, pairs: &[(&str, &str)]) {
    with_raw_db(root, |tx| {
        let mut m = tx.open_table(RAW_META).unwrap();
        for (k, v) in pairs {
            m.insert(k.as_bytes(), v.as_bytes()).unwrap();
        }
    });
}

#[test]
fn creates_and_reopens_with_the_same_meta() {
    let dir = TempDir::new("store-create");
    let (store, report) = Store::open(opts(dir.path(), "0.0.2")).unwrap();
    let tok = store.take_write_token().unwrap();
    assert!(report.created);
    let root_id = store.meta(meta::DATA_ROOT_ID).unwrap().unwrap();
    assert_eq!(
        store.meta(meta::FORMAT_VERSION).unwrap().as_deref(),
        Some("0.1")
    );
    assert_eq!(
        store.meta(meta::MIGRATION_STATE).unwrap().as_deref(),
        Some("none")
    );
    let mut e = new_record(&store, &SAMPLE, "こんにちは");
    put(&store, &tok, &mut e);
    drop(store);

    let (store, report) = Store::open(opts(dir.path(), "0.0.3")).unwrap();
    assert!(!report.created);
    assert_eq!(store.meta(meta::DATA_ROOT_ID).unwrap(), Some(root_id));
    assert_eq!(
        store.meta(meta::CREATED_BY).unwrap().as_deref(),
        Some("0.0.2")
    );
    assert_eq!(
        store.meta(meta::LAST_WRITER).unwrap().as_deref(),
        Some("0.0.3")
    );
    assert_eq!(
        store.meta(meta::MAX_WRITER).unwrap().as_deref(),
        Some("0.0.3")
    );
    assert!(store.meta("writer.0.0.2").unwrap().is_some());
    assert!(store.meta("writer.0.0.3").unwrap().is_some());
    let rtx = store.read().unwrap();
    let Some(Read::Ok(back)) = rtx.get("sample", e.id).unwrap() else {
        panic!("record not readable")
    };
    assert_eq!(back.body_as::<Body>().unwrap().text, "こんにちは");
    assert_eq!(back.writer, "0.0.2");
}

/// verifies: AC-FR-203-1 (ストアの層。終了コード 2 は bin で判定する)
#[test]
fn a_second_open_of_the_same_data_root_is_refused_until_the_first_closes() {
    let dir = TempDir::new("store-lock");
    let first = open(dir.path());
    match Store::open(opts(dir.path(), "0.0.2")) {
        Err(OpenError::Lock(LockError::InUse { pid })) => {
            assert_eq!(pid, Some(std::process::id()));
        }
        Err(e) => panic!("expected InUse, got {e}"),
        Ok(_) => panic!("expected InUse, got a store"),
    }
    drop(first);
    open(dir.path());
}

/// verifies: AC-FR-215-1
#[test]
fn a_database_in_a_newer_format_opens_read_only_and_refuses_writes() {
    let dir = TempDir::new("store-newer");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut e = new_record(&store, &SAMPLE, "a");
    put(&store, &tok, &mut e);
    drop(store);
    set_meta(
        dir.path(),
        &[
            (meta::FORMAT_VERSION, "0.2"),
            (meta::FORMAT_MIN_WRITE, "0.2"),
        ],
    );

    let (store, report) = Store::open(opts(dir.path(), "0.0.2")).unwrap();
    let tok = store.take_write_token().unwrap();
    assert_eq!(
        report.read_only,
        Some(ReadOnlyReason::NewerFormat {
            min_write: "0.2".into()
        })
    );
    assert!(matches!(
        store.write(&tok, Durability::Immediate),
        Err(StoreError::ReadOnly(_))
    ));
    // 読むことはできる。開いたことも記録しない (書かない)。
    assert!(matches!(
        store.read().unwrap().get("sample", e.id).unwrap(),
        Some(Read::Ok(_))
    ));
    assert_eq!(
        store.meta(meta::LAST_WRITER).unwrap().as_deref(),
        Some("0.0.2")
    );
}

#[test]
fn a_newer_format_that_still_allows_older_writers_stays_writable() {
    let dir = TempDir::new("store-newer-compat");
    drop(open(dir.path()));
    set_meta(dir.path(), &[(meta::FORMAT_VERSION, "0.2")]);
    let (store, report) = Store::open(opts(dir.path(), "0.0.2")).unwrap();
    let tok = store.take_write_token().unwrap();
    assert_eq!(report.read_only, None);
    store.write(&tok, Durability::Immediate).unwrap();
}

#[test]
fn a_database_that_needs_a_newer_reader_or_a_migration_is_not_opened() {
    let dir = TempDir::new("store-too-new");
    drop(open(dir.path()));
    set_meta(dir.path(), &[(meta::FORMAT_MIN_READ, "0.5")]);
    assert!(matches!(
        Store::open(opts(dir.path(), "0.0.2")),
        Err(OpenError::TooNew { .. })
    ));

    let dir = TempDir::new("store-old");
    drop(open(dir.path()));
    set_meta(
        dir.path(),
        &[
            (meta::FORMAT_VERSION, "0.0"),
            (meta::FORMAT_MIN_READ, "0.0"),
            (meta::FORMAT_MIN_WRITE, "0.0"),
        ],
    );
    assert!(matches!(
        Store::open(opts(dir.path(), "0.0.2")),
        Err(OpenError::MigrationRequired { .. })
    ));
}

/// verifies: AC-FR-206-1 (= AC-NFR-303-1。`doctor` の報告は doctor のタスクで判定する)
#[test]
fn a_corrupted_record_is_quarantined_and_the_others_stay_readable() {
    let dir = TempDir::new("store-corrupt");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut good = new_record(&store, &SAMPLE, "good");
    let mut bad = new_record(&store, &SAMPLE, "bad");
    put(&store, &tok, &mut good);
    put(&store, &tok, &mut bad);
    drop(store);

    // 本体のバイトを 1 つ書き換える。
    with_raw_db(dir.path(), |tx| {
        let mut t = tx.open_table(RAW_RECORDS).unwrap();
        let k = record_key("sample", bad.id);
        let mut v = t.get(k.as_slice()).unwrap().unwrap().value().to_vec();
        let last = v.len() - 2;
        v[last] ^= 0x20;
        t.insert(k.as_slice(), v.as_slice()).unwrap();
    });

    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut seen = Vec::new();
    store
        .read()
        .unwrap()
        .for_each("sample", |r| {
            seen.push(r);
            true
        })
        .unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .any(|r| matches!(r, Read::Ok(e) if e.id == good.id))
    );
    assert!(
        seen.iter()
            .any(|r| matches!(r, Read::Corrupt { id, .. } if *id == bad.id))
    );
    assert_eq!(store.suspect_count(), 1);

    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    assert_eq!(tx.quarantine_suspects().unwrap(), 1);
    tx.commit().unwrap();

    let census = store.census().unwrap();
    assert_eq!(census.known["sample"].ok, 1);
    assert_eq!(census.known["sample"].corrupt, 0);
    assert_eq!(census.known[KIND_QUARANTINE].ok, 1);
    // 隔離した記録は原文と理由を持つ。
    let mut q = None;
    store
        .read()
        .unwrap()
        .for_each(KIND_QUARANTINE, |r| {
            q = Some(r);
            false
        })
        .unwrap();
    let Some(Read::Ok(q)) = q else {
        panic!("no quarantine record")
    };
    let body: QuarantineBody = q.body_as().unwrap();
    assert_eq!(body.kind, "sample");
    assert_eq!(body.id, bad.id.to_string());
    assert_eq!(body.reason, "checksum mismatch");
}

#[derive(Deserialize)]
struct QuarantineBody {
    kind: String,
    id: String,
    reason: String,
}

/// verifies: AC-FR-215-2 (知らない種類の記録を読み飛ばして数え、消さない)
#[test]
fn records_of_unknown_kinds_are_counted_and_kept() {
    let dir = TempDir::new("store-unknown");
    let store = open(dir.path());
    let id = store.ids().next_id();
    drop(store);
    let mut future = Envelope::new(id, "from_the_future", 3, Timestamp::from_millis(1), "9.0.0");
    future.set_body(&Body { text: "x".into() }).unwrap();
    with_raw_db(dir.path(), |tx| {
        let mut t = tx.open_table(RAW_RECORDS).unwrap();
        let k = record_key("from_the_future", id);
        t.insert(k.as_slice(), future.encode(&k).as_slice())
            .unwrap();
    });

    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut e = new_record(&store, &SAMPLE, "a");
    put(&store, &tok, &mut e);
    let mut unknown = new_record(&store, &SAMPLE, "b");
    unknown.kind = "from_the_future".into();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    assert!(matches!(
        tx.put(&mut unknown),
        Err(StoreError::UnknownKind(_))
    ));
    drop(tx);

    let census = store.census().unwrap();
    assert_eq!(census.unknown.get("from_the_future"), Some(&1));
    assert_eq!(census.known["sample"].ok, 1);
}

#[test]
fn records_in_a_newer_v_are_not_updated_but_can_be_deleted() {
    let dir = TempDir::new("store-newer-v");
    let store = open(dir.path());
    let id = store.ids().next_id();
    drop(store);
    let mut newer = Envelope::new(id, "sample", 2, Timestamp::from_millis(1), "9.0.0");
    newer.set_body(&Body { text: "v2".into() }).unwrap();
    with_raw_db(dir.path(), |tx| {
        let mut t = tx.open_table(RAW_RECORDS).unwrap();
        let k = record_key("sample", id);
        t.insert(k.as_slice(), newer.encode(&k).as_slice()).unwrap();
    });

    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut overwrite = new_record(&store, &SAMPLE, "v1");
    overwrite.id = id;
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    assert!(matches!(
        tx.put(&mut overwrite),
        Err(StoreError::NewerRecord { v: 2, .. })
    ));
    assert!(tx.delete("sample", id).unwrap());
    tx.commit().unwrap();
    assert_eq!(store.read().unwrap().get("sample", id).unwrap(), None);
}

#[test]
fn after_a_rollback_kinds_with_newer_records_only_create_when_allowed() {
    let dir = TempDir::new("store-rollback");
    let store = open(dir.path());
    let (a, b) = (store.ids().next_id(), store.ids().next_id());
    drop(store);
    with_raw_db(dir.path(), |tx| {
        let mut t = tx.open_table(RAW_RECORDS).unwrap();
        for (kind, id) in [("sample", a), ("append", b)] {
            let mut e = Envelope::new(id, kind, 2, Timestamp::from_millis(1), "0.9.0");
            e.set_body(&Body { text: "new".into() }).unwrap();
            let k = record_key(kind, id);
            t.insert(k.as_slice(), e.encode(&k).as_slice()).unwrap();
        }
    });
    set_meta(dir.path(), &[(meta::MAX_WRITER, "0.9.0")]);

    let (store, report) = Store::open(opts(dir.path(), "0.0.2")).unwrap();
    let tok = store.take_write_token().unwrap();
    assert_eq!(report.newer_writer_seen.as_deref(), Some("0.9.0"));
    // max_writer は下げない。
    assert_eq!(
        store.meta(meta::MAX_WRITER).unwrap().as_deref(),
        Some("0.9.0")
    );
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    let mut s = new_record(&store, &SAMPLE, "x");
    assert!(matches!(
        tx.put(&mut s),
        Err(StoreError::KindHasNewerRecords(_))
    ));
    let mut ap = new_record(&store, &APPEND, "y");
    tx.put(&mut ap).unwrap();
    tx.commit().unwrap();
}

#[test]
fn background_results_written_without_durability_are_readable_after_a_clean_close() {
    let dir = TempDir::new("store-none");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut e = new_record(&store, &SAMPLE, "bg");
    let mut tx = store.write(&tok, Durability::None).unwrap();
    tx.put(&mut e).unwrap();
    tx.commit().unwrap();
    // 区切りの Immediate のコミット (03 §3.4)。
    store
        .write(&tok, Durability::Immediate)
        .unwrap()
        .commit()
        .unwrap();
    drop(store);
    let store = open(dir.path());
    assert!(matches!(
        store.read().unwrap().get("sample", e.id).unwrap(),
        Some(Read::Ok(_))
    ));
}

#[test]
fn records_must_be_written_at_the_current_v() {
    let dir = TempDir::new("store-wrong-v");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut e = new_record(&store, &SAMPLE, "a");
    e.v = 2;
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    assert!(matches!(
        tx.put(&mut e),
        Err(StoreError::WrongVersion {
            expected: 1,
            got: 2,
            ..
        })
    ));
}
