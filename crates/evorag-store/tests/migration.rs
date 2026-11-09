//! 移行の枠組みの試験 (03 §7.2、§12.1)。試験用の形式の版 `0.2`・`0.3` と移行の関数を使う。
//!
//! verifies: AC-FR-214-1

use std::path::Path;
use std::sync::Arc;

use ciborium::Value;
use evorag_store::migrate::{FormatPlan, Migration, MigrationCtx, Table};
use evorag_store::testutil::TempDir;
use evorag_store::{
    DB_FILE, Durability, Envelope, KindSpec, OpenOptions, OpenReport, Read, ReadOnlyReason, Store,
    meta,
};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use redb::{ReadableDatabase, ReadableTableMetadata};
use serde::Serialize;

const SAMPLE: KindSpec = KindSpec::new("sample", 1);

#[derive(Serialize)]
struct Body {
    text: String,
}

fn opts(root: &Path) -> OpenOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.2".into(),
        kinds: vec![SAMPLE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 3)),
    }
}

fn plan(version: &str, migrations: Vec<Migration>) -> FormatPlan {
    FormatPlan {
        version: version.into(),
        min_read: version.into(),
        min_write: version.into(),
        migrations,
    }
}

fn copy_all(ctx: &mut MigrationCtx<'_>) -> Result<(), String> {
    for t in Table::ALL {
        ctx.copy_table(t)?;
    }
    Ok(())
}

/// 0.1 → 0.2: すべてを写し、sample の本体に項目を足す。
fn add_field(ctx: &mut MigrationCtx<'_>) -> Result<(), String> {
    copy_all(ctx)?;
    let mut rewritten = Vec::new();
    ctx.for_each(Table::Records, |k, v| {
        let mut e = Envelope::decode(k, v).map_err(|e| e.to_string())?;
        if e.kind == "sample"
            && let Value::Map(m) = &mut e.body
        {
            m.push(("migrated".into(), Value::Bool(true)));
            rewritten.push((k.to_vec(), e.encode(k)));
        }
        Ok(())
    })?;
    for (k, v) in rewritten {
        ctx.put(Table::Records, &k, &v)?;
    }
    Ok(())
}

fn fail(_: &mut MigrationCtx<'_>) -> Result<(), String> {
    Err("refusing on purpose".into())
}

/// 記録を 1 件壊す移行 (突き合わせで止まること)。
fn corrupt_one(ctx: &mut MigrationCtx<'_>) -> Result<(), String> {
    copy_all(ctx)?;
    let mut first = None;
    ctx.for_each(Table::Records, |k, v| {
        if first.is_none() {
            let mut v = v.to_vec();
            let last = v.len() - 1;
            v[last] ^= 1;
            first = Some((k.to_vec(), v));
        }
        Ok(())
    })?;
    let (k, v) = first.expect("a record");
    ctx.put(Table::Records, &k, &v)
}

const TO_02: Migration = Migration {
    from: "0.1",
    to: "0.2",
    run: add_field,
};
const TO_03: Migration = Migration {
    from: "0.2",
    to: "0.3",
    run: copy_all,
};

/// 今の形式 (0.1) の DB に、記録を 3 件書く。
fn seed(root: &Path) -> Vec<evorag_types::Id> {
    let (store, _) = Store::open(opts(root)).unwrap();
    let tok = store.take_write_token().unwrap();
    let mut ids = Vec::new();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    for text in ["a", "b", "c"] {
        let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
        e.set_body(&Body { text: text.into() }).unwrap();
        tx.put(&mut e).unwrap();
        ids.push(e.id);
    }
    tx.commit().unwrap();
    ids
}

/// 閉じた DB のファイルの `meta` と件数を、redb で直接読む。
fn peek(db_file: &Path) -> (String, u64) {
    let db = redb::Database::open(db_file).unwrap();
    let rtx = db.begin_read().unwrap();
    let m = rtx
        .open_table(redb::TableDefinition::<&[u8], &[u8]>::new("meta"))
        .unwrap();
    let format = String::from_utf8(
        m.get(b"format.version".as_slice())
            .unwrap()
            .unwrap()
            .value()
            .to_vec(),
    )
    .unwrap();
    let n = rtx
        .open_table(redb::TableDefinition::<&[u8], &[u8]>::new("records"))
        .unwrap()
        .len()
        .unwrap();
    (format, n)
}

fn no_leftovers(root: &Path) {
    for f in ["evorag.redb.migrating", "evorag.redb.pre-migration"] {
        assert!(!root.join(f).exists(), "{f} is left behind");
    }
}

fn backup_of(report: &OpenReport) -> std::path::PathBuf {
    report.pre_op_backup.clone().expect("a pre-op backup")
}

#[test]
fn an_older_database_is_backed_up_then_migrated() {
    let dir = TempDir::new("migrate-ok");
    let ids = seed(dir.path());

    let (store, report) = Store::open_with(opts(dir.path()), plan("0.2", vec![TO_02])).unwrap();
    assert_eq!(report.migrated_from.as_deref(), Some("0.1"));
    assert_eq!(report.read_only, None);
    assert_eq!(
        store.meta(meta::FORMAT_VERSION).unwrap().as_deref(),
        Some("0.2")
    );
    assert_eq!(
        store.meta(meta::MIGRATION_STATE).unwrap().as_deref(),
        Some("none")
    );
    for id in &ids {
        let Some(Read::Ok(e)) = store.read().unwrap().get("sample", *id).unwrap() else {
            panic!("record lost in migration")
        };
        let Value::Map(m) = &e.body else { panic!() };
        assert!(m.iter().any(|(k, _)| k.as_text() == Some("migrated")));
    }
    no_leftovers(dir.path());

    // 退避は移行の前の形のまま。
    let backup = backup_of(&report);
    assert!(backup.starts_with(dir.path().join("backups").join("pre-op")));
    assert!(
        backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-migrate-v0.1")
    );
    drop(store);
    assert_eq!(peek(&backup.join(DB_FILE)), ("0.1".into(), 3));
}

#[test]
fn a_failed_migration_keeps_the_original_and_opens_read_only() {
    let dir = TempDir::new("migrate-fail");
    let ids = seed(dir.path());
    let failing = Migration {
        from: "0.1",
        to: "0.2",
        run: fail,
    };

    let (store, report) = Store::open_with(opts(dir.path()), plan("0.2", vec![failing])).unwrap();
    let Some(ReadOnlyReason::MigrationFailed { reason }) = &report.read_only else {
        panic!("expected a read-only open, got {report:?}")
    };
    assert!(reason.contains("refusing on purpose"), "{reason}");
    assert!(
        store
            .write(&store.take_write_token().unwrap(), Durability::Immediate)
            .is_err()
    );
    assert_eq!(
        store.meta(meta::FORMAT_VERSION).unwrap().as_deref(),
        Some("0.1")
    );
    assert!(matches!(
        store.read().unwrap().get("sample", ids[0]).unwrap(),
        Some(Read::Ok(_))
    ));
    no_leftovers(dir.path());
    // 移行の前に退避は作られている。
    let pre = dir.path().join("backups").join("pre-op");
    assert_eq!(std::fs::read_dir(pre).unwrap().count(), 1);
}

#[test]
fn a_migration_that_corrupts_records_is_rejected_by_the_check() {
    let dir = TempDir::new("migrate-corrupt");
    seed(dir.path());
    let bad = Migration {
        from: "0.1",
        to: "0.2",
        run: corrupt_one,
    };
    let (store, report) = Store::open_with(opts(dir.path()), plan("0.2", vec![bad])).unwrap();
    assert!(matches!(
        report.read_only,
        Some(ReadOnlyReason::MigrationFailed { .. })
    ));
    assert_eq!(store.census().unwrap().known["sample"].ok, 3);
    no_leftovers(dir.path());
}

#[test]
fn migrations_are_applied_in_order_across_several_versions() {
    let dir = TempDir::new("migrate-chain");
    seed(dir.path());
    let (store, report) =
        Store::open_with(opts(dir.path()), plan("0.3", vec![TO_03, TO_02])).unwrap();
    assert_eq!(report.migrated_from.as_deref(), Some("0.1"));
    assert_eq!(
        store.meta(meta::FORMAT_VERSION).unwrap().as_deref(),
        Some("0.3")
    );
    assert_eq!(store.census().unwrap().known["sample"].ok, 3);
    no_leftovers(dir.path());
}

#[test]
fn leftovers_of_an_interrupted_migration_are_cleaned_up() {
    // 入れ替えの前に止まった: 移行先の書きかけが残っている。
    let dir = TempDir::new("migrate-crash-before-swap");
    seed(dir.path());
    std::fs::write(dir.path().join("evorag.redb.migrating"), b"half written").unwrap();
    let (store, _) = Store::open(opts(dir.path())).unwrap();
    assert_eq!(store.census().unwrap().known["sample"].ok, 3);
    drop(store);
    no_leftovers(dir.path());

    // 元を退けた後、移行先を置く前に止まった (移行先は書き終えていない): 元に戻す。
    let dir = TempDir::new("migrate-crash-mid-swap");
    seed(dir.path());
    std::fs::rename(
        dir.path().join(DB_FILE),
        dir.path().join("evorag.redb.pre-migration"),
    )
    .unwrap();
    std::fs::write(dir.path().join("evorag.redb.migrating"), b"half written").unwrap();
    let (store, _) = Store::open(opts(dir.path())).unwrap();
    assert_eq!(store.census().unwrap().known["sample"].ok, 3);
    drop(store);
    no_leftovers(dir.path());

    // 移行先を書き終えた後、置く前に止まった: 移行先を置く。
    let dir = TempDir::new("migrate-crash-done");
    seed(dir.path());
    std::fs::rename(
        dir.path().join(DB_FILE),
        dir.path().join("evorag.redb.migrating"),
    )
    .unwrap();
    let (store, report) = Store::open(opts(dir.path())).unwrap();
    assert!(!report.created);
    assert_eq!(store.census().unwrap().known["sample"].ok, 3);
    drop(store);
    no_leftovers(dir.path());
}
