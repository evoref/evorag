//! 形式の版 0.1 → 0.2 の移行 (03 §12.1)。0.2 は 0.1 に種類と任意の項目を足しただけなので、記録を
//! 書き換えずに写し、`min_read`・`min_write` を 0.1 のままにする (v0.1.2 に戻しても読み書きできる)。
//!
//! verifies: AC-NFR-307-1 (ストアの層)

use std::path::Path;
use std::sync::Arc;

use evorag_store::migrate::FormatPlan;
use evorag_store::testutil::TempDir;
use evorag_store::{Durability, Envelope, KindSpec, OpenOptions, Read, Store, meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};
use serde::{Deserialize, Serialize};

const SAMPLE: KindSpec = KindSpec::new("sample", 1);

#[derive(Serialize, Deserialize)]
struct Body {
    text: String,
}

fn opts(root: &Path, version: &str, seed: u64) -> OpenOptions {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: version.into(),
        kinds: vec![SAMPLE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, seed)),
        config_file: None,
    }
}

/// v0.1 の evorag (形式の版 0.1 だけを知る) のふりをする。
fn v0_1() -> FormatPlan {
    FormatPlan {
        version: "0.1".into(),
        min_read: "0.1".into(),
        min_write: "0.1".into(),
        migrations: Vec::new(),
    }
}

fn write(store: &Store, text: &str) -> evorag_types::Id {
    let tok = store.take_write_token().unwrap();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
    e.set_body(&Body { text: text.into() }).unwrap();
    tx.put(&mut e).unwrap();
    tx.commit().unwrap();
    e.id
}

fn text(store: &Store, id: evorag_types::Id) -> String {
    match store.read().unwrap().get("sample", id).unwrap() {
        Some(Read::Ok(e)) => e.body_as::<Body>().unwrap().text,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_v0_1_database_is_backed_up_and_moved_to_0_2_and_v0_1_can_still_write_to_it() {
    let dir = TempDir::new("format-0-2");
    let first = {
        let (store, report) = Store::open_with(opts(dir.path(), "0.1.2", 1), v0_1()).unwrap();
        assert!(report.created);
        write(&store, "v0.1 で書いた")
    };

    // v0.2 で開く: 退避してから 0.2 に移す。記録はそのまま。
    let second = {
        let (store, report) = Store::open(opts(dir.path(), "0.2.0", 2)).unwrap();
        assert_eq!(report.migrated_from.as_deref(), Some("0.1"));
        let backup = report.pre_op_backup.expect("a backup before migrating");
        assert!(backup.join(evorag_store::DB_FILE).exists());
        assert_eq!(
            store.meta(meta::FORMAT_VERSION).unwrap().as_deref(),
            Some("0.2")
        );
        assert_eq!(
            store.meta(meta::FORMAT_MIN_READ).unwrap().as_deref(),
            Some("0.1")
        );
        assert_eq!(
            store.meta(meta::FORMAT_MIN_WRITE).unwrap().as_deref(),
            Some("0.1")
        );
        assert_eq!(text(&store, first), "v0.1 で書いた");
        write(&store, "v0.2 で書いた")
    };

    // v0.1.2 に戻す: 読むだけにならず、書ける (形式の版は 0.2 のまま)。
    let third = {
        let (store, report) = Store::open_with(opts(dir.path(), "0.1.2", 3), v0_1()).unwrap();
        assert_eq!(report.read_only, None);
        assert_eq!(report.migrated_from, None);
        assert_eq!(text(&store, second), "v0.2 で書いた");
        write(&store, "v0.1.2 に戻して書いた")
    };

    // もう一度 v0.2 で開く: 移行は起きず、3 件とも読める。
    let (store, report) = Store::open(opts(dir.path(), "0.2.0", 4)).unwrap();
    assert_eq!(report.migrated_from, None);
    for id in [first, second, third] {
        text(&store, id);
    }
}
