//! ID をプロセスをまたいでも単調にする (03 §3.3)。時計が戻ったまま開き直しても、新しい ID は
//! 記録した ID より大きい。

use std::path::Path;
use std::sync::Arc;

use evorag_store::testutil::TempDir;
use evorag_store::{Durability, Envelope, KindSpec, OpenOptions, Store};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::Serialize;

const SAMPLE: KindSpec = KindSpec::new("sample", 1);

#[derive(Serialize)]
struct Body {
    text: String,
}

fn open_at(root: &Path, ms: i64) -> Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(ms)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.3.0".into(),
        kinds: vec![SAMPLE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap();
    store
}

fn write_one(store: &Store) -> Id {
    let tok = store.take_write_token().unwrap();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
    e.set_body(&Body { text: "x".into() }).unwrap();
    tx.put(&mut e).unwrap();
    tx.commit().unwrap();
    e.id
}

#[test]
fn ids_stay_above_recorded_ones_when_the_clock_went_back() {
    let dir = TempDir::new("id-floor");
    let old = {
        let store = open_at(dir.path(), 1_762_650_000_000);
        write_one(&store)
    };
    // 1 時間戻った時計で開き直す。
    let store = open_at(dir.path(), 1_762_646_400_000);
    assert_eq!(store.max_record_id().unwrap(), Some(old));
    let new = store.ids().next_id();
    assert!(new > old, "{new} must be greater than {old}");
}
