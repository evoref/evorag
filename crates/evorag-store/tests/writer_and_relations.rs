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
        config_file: None,
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

/// ジャーナルに書く種類 (会話と明示の操作の代わり)。
const NOTED: KindSpec = KindSpec {
    journaled: true,
    ..KindSpec::new("noted", 1)
};

#[derive(Serialize)]
struct Turnish {
    text: String,
    state: String,
    rev: u32,
}

fn open_noted(root: &Path) -> Arc<Store> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.2".into(),
        kinds: vec![SAMPLE, NOTED],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap();
    Arc::new(store)
}

fn noted(store: &Store, id: Id, body: &Turnish) -> Envelope {
    let mut e = Envelope::new(id, "noted", 1, store.clock().now(), "");
    e.set_body(body).unwrap();
    e
}

fn start(store: &Arc<Store>) -> (evorag_store::WriterHandle, std::thread::JoinHandle<()>) {
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    (handle, std::thread::spawn(move || writer.run()))
}

/// 消した記録の古い封筒を `put` し直しても蘇らない。このプロセスで消したものも、ジャーナルの削除の行で
/// 消したもの (開き直した後) も断る。
/// verifies: 04 §7.7 の 3 (書き戻させない)
#[test]
fn a_deleted_record_is_not_put_back() {
    let dir = TempDir::new("writer-deleted");
    let store = open_noted(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    let stale = noted(
        &store,
        id,
        &Turnish {
            text: "架空の発話".into(),
            state: "pending".into(),
            rev: 0,
        },
    );
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(
        stale.clone(),
    )]))
    .unwrap();
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Delete {
        kind: "noted".into(),
        id,
    }]))
    .unwrap();
    // 消す前に読んだ封筒を書き戻す (応答の経路が古い発話を「確定」にするなど)。
    let other = record(&store, "ほかの記録");
    let r = h.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(other.clone()),
        WriteOp::Put(stale.clone()),
    ]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Deleted { id: got, .. })) if got == id),
        "{r:?}"
    );
    let rtx = store.read().unwrap();
    assert_eq!(rtx.get("noted", id).unwrap(), None);
    assert_eq!(rtx.get("sample", other.id).unwrap(), None, "all or nothing");
    drop(rtx);
    h.shutdown();
    t.join().unwrap();
    drop(store);

    // 開き直しても、ジャーナルの削除の行から断る。
    let store = open_noted(dir.path());
    let (h, t) = start(&store);
    let r = h.submit_wait(WriteRequest::background(vec![WriteOp::Put(stale)]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Deleted { .. }))),
        "{r:?}"
    );
    assert_eq!(store.read().unwrap().get("noted", id).unwrap(), None);
    h.shutdown();
    t.join().unwrap();
}

/// 差し替え (`Patch`) は書き手のトランザクションの中で読み直すので、その間の伏せ字 (本文と改訂の番号) を
/// 上書きしない。ジャーナルには差し替えた後の封筒が残る。無い記録への差し替えは依頼ごと断る。
/// verifies: 04 §7.7 の 3 (ターンは読み直してから更新する)
#[test]
fn a_patch_rereads_the_record_and_keeps_concurrent_changes() {
    let dir = TempDir::new("writer-patch");
    let store = open_noted(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    let read_before = noted(
        &store,
        id,
        &Turnish {
            text: "架空の秘密の値".into(),
            state: "pending".into(),
            rev: 0,
        },
    );
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(read_before)]))
        .unwrap();
    // 応答を作っている間に伏せ字になった。
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(noted(
        &store,
        id,
        &Turnish {
            text: "架空の[伏せ字]".into(),
            state: "pending".into(),
            rev: 1,
        },
    ))]))
    .unwrap();
    // 応答の経路は、前に読んだ発話の状態だけを差し替える。
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Patch {
        kind: "noted".into(),
        id,
        fields: evorag_store::BodyPatch::new()
            .set("state", &"confirmed")
            .unwrap(),
        updated_at: Some(Timestamp::from_millis(1_762_650_001_000)),
    }]))
    .unwrap();
    let check = |store: &Store| {
        let Some(Read::Ok(e)) = store.read().unwrap().get("noted", id).unwrap() else {
            panic!("the record is gone");
        };
        let v: serde_json::Value = e.body_as().unwrap();
        assert_eq!(v["text"], "架空の[伏せ字]");
        assert_eq!(v["rev"], 1);
        assert_eq!(v["state"], "confirmed");
        assert_eq!(e.updated_at, Timestamp::from_millis(1_762_650_001_000));
    };
    check(&store);
    // 無い記録への差し替えは、依頼の全体を断る。
    let other = record(&store, "ほかの記録");
    let r = h.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(other.clone()),
        WriteOp::Patch {
            kind: "noted".into(),
            id: store.ids().next_id(),
            fields: evorag_store::BodyPatch::new(),
            updated_at: None,
        },
    ]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Stale { .. }))),
        "{r:?}"
    );
    assert_eq!(store.read().unwrap().get("sample", other.id).unwrap(), None);
    h.shutdown();
    t.join().unwrap();
    drop(store);

    // DB を失っても、ジャーナルから差し替えた後の封筒が戻る。
    std::fs::remove_file(dir.path().join(evorag_store::DB_FILE)).unwrap();
    let store = open_noted(dir.path());
    check(&store);
}

/// 派生の記録を書く前提 (`Expect`): 出典が残っていて改訂の番号が読んだときと合わなければ、依頼ごと断る。
/// verifies: 04 §7.7 の 4 (派生物は出典を確かめてから書く)
#[test]
fn an_expectation_on_the_source_guards_the_whole_request() {
    let dir = TempDir::new("writer-expect");
    let store = open_noted(dir.path());
    let (h, t) = start(&store);
    let source = store.ids().next_id();
    // `rev` の無い本体は 0 として読む (03 §3.3)。
    let mut e = Envelope::new(source, "noted", 1, store.clock().now(), "");
    e.set_body(&Body {
        text: "架空の出典".into(),
    })
    .unwrap();
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(e)]))
        .unwrap();
    let expect = |rev: u64| WriteOp::Expect {
        kind: "noted".into(),
        id: source,
        field: Some(("rev".into(), rev)),
    };
    let first = record(&store, "派生 1");
    h.submit_wait(WriteRequest::background(vec![
        expect(0),
        WriteOp::Put(first.clone()),
    ]))
    .unwrap();
    // 番号が変わった (伏せ字) 後は、前の番号で作った派生の記録を書かない。
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(noted(
        &store,
        source,
        &Turnish {
            text: "架空の[伏せ字]".into(),
            state: "confirmed".into(),
            rev: 1,
        },
    ))]))
    .unwrap();
    let second = record(&store, "派生 2");
    let r = h.submit_wait(WriteRequest::background(vec![
        expect(0),
        WriteOp::Put(second.clone()),
    ]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Stale { .. }))),
        "{r:?}"
    );
    // 出典が消えた後も書かない。
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Delete {
        kind: "noted".into(),
        id: source,
    }]))
    .unwrap();
    let r = h.submit_wait(WriteRequest::background(vec![
        WriteOp::Expect {
            kind: "noted".into(),
            id: source,
            field: None,
        },
        WriteOp::Put(second.clone()),
    ]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Stale { .. }))),
        "{r:?}"
    );
    let rtx = store.read().unwrap();
    assert!(matches!(
        rtx.get("sample", first.id).unwrap(),
        Some(Read::Ok(_))
    ));
    assert_eq!(rtx.get("sample", second.id).unwrap(), None);
    drop(rtx);
    h.shutdown();
    t.join().unwrap();
}

/// 前に消した ID は、同じ依頼の中で消してから作り直しても蘇らない。
/// verifies: 04 §7.7 の 3 (書き戻させない)
#[test]
fn a_delete_then_put_in_one_request_does_not_resurrect_a_deleted_id() {
    let dir = TempDir::new("writer-delete-put");
    let store = open_noted(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    let env = noted(
        &store,
        id,
        &Turnish {
            text: "架空の発話".into(),
            state: "confirmed".into(),
            rev: 0,
        },
    );
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(env.clone())]))
        .unwrap();
    let delete = || WriteOp::Delete {
        kind: "noted".into(),
        id,
    };
    h.submit_wait(WriteRequest::conversation(vec![delete()]))
        .unwrap();
    let r = h.submit_wait(WriteRequest::conversation(vec![
        delete(),
        WriteOp::Put(env),
    ]));
    assert!(
        matches!(r, Err(WriteError::Store(StoreError::Deleted { .. }))),
        "{r:?}"
    );
    assert_eq!(store.read().unwrap().get("noted", id).unwrap(), None);
    h.shutdown();
    t.join().unwrap();
}
