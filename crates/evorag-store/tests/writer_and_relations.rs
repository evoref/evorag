//! 関係・副索引・大きなデータと、書き手の試験 (02 §5、03 §3.1、§3.4)。

use std::path::Path;
use std::sync::{Arc, Mutex};

use evorag_store::key::{OwnedPart, Part};
use evorag_store::testutil::TempDir;
use evorag_store::{
    Envelope, KindSpec, LookupRange, OpenOptions, Read, Store, StoreError, WriteError, WriteOp,
    WriteRequest, Writer,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::Serialize;

const SAMPLE: KindSpec = KindSpec::new("sample", 1);

#[derive(Serialize)]
struct Body {
    text: String,
}

fn open(root: &Path) -> Arc<Store> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.2".into(),
        kinds: vec![SAMPLE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
    })
    .unwrap();
    Arc::new(store)
}

fn record(store: &Store, text: &str) -> Envelope {
    let mut e = Envelope::new(store.ids().next_id(), "sample", 1, store.clock().now(), "");
    e.set_body(&Body { text: text.into() }).unwrap();
    e
}

fn ids(store: &Store, n: usize) -> Vec<Id> {
    (0..n).map(|_| store.ids().next_id()).collect()
}

#[test]
fn links_are_readable_from_both_ends_and_removed_together() {
    let dir = TempDir::new("links");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let v = ids(&store, 4);
    let (session, t1, t2, other) = (v[0], v[1], v[2], v[3]);
    let mut tx = store
        .write(&tok, evorag_store::Durability::Immediate)
        .unwrap();
    tx.put_link(t1, "in_session", session, b"a").unwrap();
    tx.put_link(t2, "in_session", session, b"b").unwrap();
    tx.put_link(t1, "replies_to", other, b"").unwrap();
    assert!(matches!(
        tx.put_link(t1, "~bad", session, b""),
        Err(StoreError::BadRelation(_))
    ));
    tx.commit().unwrap();

    let r = store.read().unwrap();
    assert_eq!(
        r.links_to(session, "in_session").unwrap(),
        vec![(t1, b"a".to_vec()), (t2, b"b".to_vec())]
    );
    assert_eq!(
        r.links_from(t1, "in_session").unwrap(),
        vec![(session, b"a".to_vec())]
    );
    drop(r);

    // t1 の関係を全部消すと、相手の側の逆引きも消える。
    let mut tx = store
        .write(&tok, evorag_store::Durability::Immediate)
        .unwrap();
    assert_eq!(tx.delete_links_of(t1).unwrap(), 2);
    tx.commit().unwrap();
    let r = store.read().unwrap();
    assert_eq!(
        r.links_to(session, "in_session").unwrap(),
        vec![(t2, b"b".to_vec())]
    );
    assert!(r.links_to(other, "replies_to").unwrap().is_empty());
}

#[test]
fn lookups_scan_exact_prefix_and_ranges_in_order() {
    let dir = TempDir::new("lookup");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let v = ids(&store, 5);
    let mut tx = store
        .write(&tok, evorag_store::Durability::Immediate)
        .unwrap();
    for (i, id) in v.iter().enumerate() {
        tx.put_lookup(
            "session",
            "updated_at",
            &Part::Int(1_000 + i as i64 * 10),
            *id,
        )
        .unwrap();
    }
    for (name, id) in [("ab", v[0]), ("abc", v[1]), ("b", v[2]), ("aa", v[3])] {
        tx.put_lookup("fact", "attr", &Part::Str(name), id).unwrap();
    }
    tx.commit().unwrap();

    let r = store.read().unwrap();
    // 更新時刻の新しい順に 2 件 (セッションの一覧)。
    let newest = r
        .lookup(
            "session",
            "updated_at",
            &LookupRange::Between {
                from: None,
                to: None,
            },
            true,
            Some(2),
        )
        .unwrap();
    assert_eq!(
        newest.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![v[4], v[3]]
    );
    assert_eq!(newest[0].value, OwnedPart::Int(1_040));

    let mid = r
        .lookup(
            "session",
            "updated_at",
            &LookupRange::Between {
                from: Some(Part::Int(1_010)),
                to: Some(Part::Int(1_030)),
            },
            false,
            None,
        )
        .unwrap();
    assert_eq!(
        mid.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![v[1], v[2]]
    );

    let ab = r
        .lookup("fact", "attr", &LookupRange::StrPrefix("ab"), false, None)
        .unwrap();
    assert_eq!(
        ab.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![v[0], v[1]]
    );
    let exact = r
        .lookup(
            "fact",
            "attr",
            &LookupRange::Eq(Part::Str("ab")),
            false,
            None,
        )
        .unwrap();
    assert_eq!(exact.iter().map(|e| e.id).collect::<Vec<_>>(), vec![v[0]]);
    // ほかの field・kind は混ざらない。
    let all_attr = r
        .lookup(
            "fact",
            "attr",
            &LookupRange::Between {
                from: None,
                to: None,
            },
            false,
            None,
        )
        .unwrap();
    assert_eq!(all_attr.len(), 4);
}

#[test]
fn blobs_are_stored_by_part_and_removed_per_record() {
    let dir = TempDir::new("blobs");
    let store = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let v = ids(&store, 2);
    let mut tx = store
        .write(&tok, evorag_store::Durability::Immediate)
        .unwrap();
    tx.put_blob(v[0], 0, &[1, 2, 3]).unwrap();
    tx.put_blob(v[0], 1, &[4]).unwrap();
    tx.put_blob(v[1], 0, &[9]).unwrap();
    assert_eq!(tx.delete_blobs(v[0]).unwrap(), 2);
    tx.commit().unwrap();
    let r = store.read().unwrap();
    assert_eq!(r.blob(v[0], 0).unwrap(), None);
    assert_eq!(r.blob(v[1], 0).unwrap(), Some(vec![9]));
}

#[test]
fn a_request_is_written_all_or_nothing() {
    let dir = TempDir::new("writer-atomic");
    let store = open(dir.path());
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());

    let good = record(&store, "good");
    let mut bad = record(&store, "bad");
    bad.kind = "nope".into();
    let r = handle.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(good.clone()),
        WriteOp::Put(bad),
    ]));
    assert!(matches!(
        r,
        Err(WriteError::Store(StoreError::UnknownKind(_)))
    ));
    assert_eq!(store.read().unwrap().get("sample", good.id).unwrap(), None);

    let out = handle
        .submit_wait(WriteRequest::conversation(vec![
            WriteOp::Put(good.clone()),
            WriteOp::PutLookup {
                kind: "sample".into(),
                field: "text".into(),
                value: OwnedPart::Str("good".into()),
                id: good.id,
            },
        ]))
        .unwrap();
    assert_eq!(out.put, vec![good.id]);
    let r = store.read().unwrap();
    assert!(matches!(
        r.get("sample", good.id).unwrap(),
        Some(Read::Ok(_))
    ));
    assert_eq!(
        r.lookup(
            "sample",
            "text",
            &LookupRange::Eq(Part::Str("good")),
            false,
            None
        )
        .unwrap()
        .len(),
        1
    );
    drop(r);
    handle.shutdown();
    t.join().unwrap();
}

#[test]
fn conversation_requests_are_written_before_background_ones() {
    let dir = TempDir::new("writer-priority");
    let store = open(dir.path());
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    // 書き手を回す前に積んでおく。
    for (name, req) in [
        (
            "bg1",
            WriteRequest::background(vec![WriteOp::Put(record(&store, "bg1"))]),
        ),
        (
            "conv1",
            WriteRequest::conversation(vec![WriteOp::Put(record(&store, "c1"))]),
        ),
        (
            "bg2",
            WriteRequest::background(vec![WriteOp::Put(record(&store, "bg2"))]),
        ),
        (
            "conv2",
            WriteRequest::conversation(vec![WriteOp::Put(record(&store, "c2"))]),
        ),
    ] {
        let order = order.clone();
        handle
            .submit(
                req,
                Box::new(move |r| {
                    assert!(r.is_ok());
                    order.lock().unwrap().push(name);
                }),
            )
            .unwrap();
    }
    let t = std::thread::spawn(move || writer.run());
    // 全部書けるまで待つ (最後に積む裏の処理の依頼は、ほかのすべての後に処理される)。
    handle
        .submit_wait(WriteRequest::background(Vec::new()))
        .unwrap();
    handle.shutdown();
    t.join().unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["conv1", "conv2", "bg1", "bg2"]);
}

#[test]
fn shutdown_writes_pending_conversation_and_drops_background_work() {
    let dir = TempDir::new("writer-shutdown");
    let store = open(dir.path());
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let conv = record(&store, "keep");
    let bg = record(&store, "drop");
    let results = Arc::new(Mutex::new(Vec::new()));
    for (name, req) in [
        (
            "bg",
            WriteRequest::background(vec![WriteOp::Put(bg.clone())]),
        ),
        (
            "conv",
            WriteRequest::conversation(vec![WriteOp::Put(conv.clone())]),
        ),
    ] {
        let results = results.clone();
        handle
            .submit(
                req,
                Box::new(move |r| results.lock().unwrap().push((name, r.is_ok()))),
            )
            .unwrap();
    }
    handle.shutdown();
    assert!(matches!(
        handle.submit_wait(WriteRequest::conversation(Vec::new())),
        Err(WriteError::ShuttingDown)
    ));
    writer.run();

    let mut got = results.lock().unwrap().clone();
    got.sort();
    assert_eq!(got, vec![("bg", false), ("conv", true)]);
    let r = store.read().unwrap();
    assert!(matches!(
        r.get("sample", conv.id).unwrap(),
        Some(Read::Ok(_))
    ));
    assert_eq!(r.get("sample", bg.id).unwrap(), None);
}

#[test]
fn there_is_only_one_writer_per_store() {
    let dir = TempDir::new("writer-one");
    let store = open(dir.path());
    let _w = Writer::new(store.clone()).unwrap();
    assert!(matches!(
        Writer::new(store.clone()),
        Err(StoreError::TokenTaken)
    ));
    assert!(store.take_write_token().is_none());
}
