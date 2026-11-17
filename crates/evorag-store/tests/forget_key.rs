//! `memory.forget_key` の探し方 (03 §3.1.1)。
//!
//! v0.2.x は、この `meta` の行を取り込まずに `journal.last_seq` だけを進める。そのため v0.3 は、
//! ジャーナルのすべての `meta` の行から鍵を探して入れ、食い違えば書き換えずに知らせる。
//!
//! verifies: 03 §3.1.1 (`memory.forget_key` の探し方とフェイルクローズ)

use std::path::Path;
use std::sync::Arc;

use evorag_store::testutil::{TempDir, raw_remove_meta, raw_set_meta};
use evorag_store::{OpenOptions, OpenReport, Store, WriteOp, WriteRequest, Writer, meta};
use evorag_types::{ManualClock, Timestamp, UuidV7Gen};

const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn open(root: &Path) -> (Arc<Store>, OpenReport) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, report) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.3.0".into(),
        kinds: Vec::new(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap();
    (Arc::new(store), report)
}

/// 書き手を通して鍵を書く (ジャーナルにも `meta` の行が載る)。
fn write_key(root: &Path, key: &str) {
    let (store, _) = open(root);
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    handle
        .submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaOnce {
            key: meta::FORGET_KEY.into(),
            value: key.into(),
        }]))
        .unwrap();
    handle.shutdown();
    t.join().unwrap();
}

#[test]
fn a_key_line_skipped_by_an_old_version_is_recovered_from_the_journal() {
    let dir = TempDir::new("forget-key-recover");
    write_key(dir.path(), K1);
    // v0.2.x で開いた後の状態: 鍵の行は取り込まれず、連番だけが進んでいる。
    raw_remove_meta(dir.path(), meta::FORGET_KEY);

    let (store, report) = open(dir.path());
    // 連番での取り込みではなく、全行から探す経路で入った。
    assert_eq!(report.journal_replayed, 0);
    assert!(report.forget_key_recovered);
    assert!(!report.forget_key_conflict);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap().as_deref(), Some(K1));
    drop(store);

    // 入れた後は、開き直しても何もしない。
    let (store, report) = open(dir.path());
    assert!(!report.forget_key_recovered);
    assert!(!report.forget_key_conflict);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap().as_deref(), Some(K1));
}

#[test]
fn a_different_key_in_the_database_is_reported_and_not_overwritten() {
    let dir = TempDir::new("forget-key-conflict-db");
    write_key(dir.path(), K1);
    raw_set_meta(dir.path(), meta::FORGET_KEY, K2);

    let (store, report) = open(dir.path());
    assert!(report.forget_key_conflict);
    assert!(!report.forget_key_recovered);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap().as_deref(), Some(K2));
}

#[test]
fn while_keys_disagree_no_new_key_can_be_written() {
    let dir = TempDir::new("forget-key-enforced");
    write_key(dir.path(), K1);
    raw_set_meta(dir.path(), meta::FORGET_KEY, K2);
    let (store, report) = open(dir.path());
    assert!(report.forget_key_conflict);
    assert!(store.forget_key_conflict());
    // 呼び出し側が旗を見落として作り直そうとしても、書けない。
    raw_attempt_to_write(&store, &[K2]);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap().as_deref(), Some(K2));
}

#[test]
fn the_same_key_written_twice_is_not_a_conflict() {
    let dir = TempDir::new("forget-key-twice");
    write_key(dir.path(), K1);
    write_key(dir.path(), K1);
    let (store, report) = open(dir.path());
    assert!(!report.forget_key_conflict);
    assert!(!store.forget_key_conflict());
}

#[test]
fn a_badly_shaped_key_is_refused_and_reported() {
    let dir = TempDir::new("forget-key-shape");
    let (store, _) = open(dir.path());
    // 大文字・短いものは書けない。
    raw_attempt_to_write(&store, &[&"AB".repeat(32), "abc"]);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap(), None);
    drop(store);
    raw_set_meta(dir.path(), meta::FORGET_KEY, "not-a-key");
    let (store, report) = open(dir.path());
    assert!(report.forget_key_conflict);
    assert!(store.forget_key_conflict());
}

#[test]
fn a_read_only_open_reports_a_key_only_in_the_journal_without_writing() {
    let dir = TempDir::new("forget-key-read-only");
    write_key(dir.path(), K1);
    raw_remove_meta(dir.path(), meta::FORGET_KEY);
    // 新しい版が書いた形式として、読むだけで開かせる。
    raw_set_meta(dir.path(), meta::FORMAT_MIN_WRITE, "9.9");
    let (store, report) = open(dir.path());
    assert!(report.read_only.is_some());
    assert!(report.forget_key_pending);
    assert!(!report.forget_key_recovered);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap(), None);
}

/// 書き手を通して鍵を書こうとし、断られることを確かめる。
fn raw_attempt_to_write(store: &Arc<Store>, keys: &[&str]) {
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for key in keys {
        let r = handle.submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaOnce {
            key: meta::FORGET_KEY.into(),
            value: (*key).into(),
        }]));
        assert!(r.is_err(), "writing the key `{key}` must be refused");
    }
    handle.shutdown();
    t.join().unwrap();
}

#[test]
fn no_key_anywhere_is_left_alone() {
    let dir = TempDir::new("forget-key-none");
    let (store, report) = open(dir.path());
    assert!(!report.forget_key_recovered);
    assert!(!report.forget_key_conflict);
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap(), None);
}

#[test]
fn the_key_can_be_written_only_once() {
    let dir = TempDir::new("forget-key-once");
    write_key(dir.path(), K1);
    let (store, _) = open(dir.path());
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    let r = handle.submit_wait(WriteRequest::conversation(vec![WriteOp::SetMetaOnce {
        key: meta::FORGET_KEY.into(),
        value: K2.into(),
    }]));
    assert!(r.is_err(), "a second key must be refused");
    handle.shutdown();
    t.join().unwrap();
    assert_eq!(store.meta(meta::FORGET_KEY).unwrap().as_deref(), Some(K1));
}
