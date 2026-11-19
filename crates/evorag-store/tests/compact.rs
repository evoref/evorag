//! 起動時の compact (03 §8) と、その要否の印 `compact.pending` (03 §3.1.1) の試験。

use std::path::Path;
use std::sync::Arc;

use evorag_store::testutil::{TempDir, raw_set_meta};
use evorag_store::{
    CompactStop, Compaction, DB_FILE, Durability, Envelope, KindSpec, OpenOptions, OpenReport,
    Read, Store, WriteOp, WriteRequest, Writer, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};

/// 写しを書くファイル (`evorag.redb.compacting`)。
const COMPACTING: &str = "evorag.redb.compacting";

/// 消すターンの本文 (ほかの本文と重ならない値)。
const GONE_TEXT: &str = "QZXV-compact-gone-turn-body-73119";
const KEPT_TEXT: &str = "QZXV-compact-kept-turn-body-55210";

const TURN: KindSpec = KindSpec {
    journaled: true,
    ..KindSpec::new("turn", 1)
};
const CHUNK: KindSpec = KindSpec::new("chunk", 1);

#[derive(serde::Serialize)]
struct Body {
    text: String,
}

fn open_as(root: &Path, version: &str) -> (Store, OpenReport) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
    Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: version.into(),
        kinds: vec![TURN, CHUNK],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 11)),
        config_file: None,
    })
    .unwrap()
}

fn open(root: &Path) -> (Store, OpenReport) {
    open_as(root, "0.2.0")
}

fn env(store: &Store, kind: &str, text: &str) -> Envelope {
    let mut e = Envelope::new(store.ids().next_id(), kind, 1, store.clock().now(), "");
    e.set_body(&Body { text: text.into() }).unwrap();
    e
}

/// 書き手を動かして依頼を順に書き、ストアを返す。
fn write_all(store: Store, reqs: Vec<WriteRequest>) -> Store {
    let store = Arc::new(store);
    let (writer, h) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for r in reqs {
        h.submit_wait(r).unwrap();
    }
    h.shutdown();
    t.join().unwrap();
    Arc::into_inner(store).expect("the writer has released the store")
}

fn db_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(DB_FILE)).unwrap()
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// ターンを 2 つ書き、片方を利用者の削除で消す。消したターンの ID と残すターンの ID を返す。
fn write_and_delete(root: &Path) -> (Id, Id) {
    let (store, _) = open(root);
    let gone = env(&store, "turn", &GONE_TEXT.repeat(8));
    let kept = env(&store, "turn", KEPT_TEXT);
    let (gone_id, kept_id) = (gone.id, kept.id);
    // 消すターンの後にも書く (消したページがファイルの末尾に来て、切り詰めで消えることのないように)。
    let later: Vec<WriteRequest> = (0..64)
        .map(|i| {
            WriteRequest::conversation(vec![WriteOp::Put(env(
                &store,
                "turn",
                &format!("架空の後のターン {i}").repeat(32),
            ))])
        })
        .collect();
    let mut reqs = vec![WriteRequest::conversation(vec![
        WriteOp::Put(gone),
        WriteOp::Put(kept),
    ])];
    reqs.extend(later);
    let store = write_all(
        store,
        [
            reqs,
            vec![WriteRequest::conversation(vec![WriteOp::Delete {
                kind: "turn".into(),
                id: gone_id,
            }])],
        ]
        .concat(),
    );
    assert!(
        store.compact_pending().unwrap(),
        "deleting a journaled record sets compact.pending in the same commit"
    );
    (gone_id, kept_id)
}

/// 利用者の削除は同じコミットで `compact.pending` を立て、次の起動の compact で印が消え、消したターンの本文が DB の
/// ファイルのバイト列から無くなる。残す記録はそのまま読める。
/// verifies: 03 §8 (compact)、03 §3.1.1 (`compact.pending`)、AC-FR-307-1 (再起動 (compact) の後の DB のファイル)
#[test]
fn a_delete_is_compacted_away_at_the_next_open() {
    let dir = TempDir::new("compact-delete");
    let (gone, kept) = write_and_delete(dir.path());
    assert!(
        contains(&db_bytes(dir.path()), GONE_TEXT),
        "before the compact, the freed page still holds the deleted text (the test is meaningful)"
    );

    let (store, mut report) = open(dir.path());
    assert!(
        store.compact_pending().unwrap(),
        "the flag survives a restart"
    );
    let store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert!(
        matches!(report.compaction, Compaction::Done { .. }),
        "{:?}",
        report.compaction
    );
    assert!(!store.compact_pending().unwrap(), "the flag is cleared");
    let r = store.read().unwrap();
    assert_eq!(r.get("turn", gone).unwrap(), None);
    assert!(matches!(r.get("turn", kept).unwrap(), Some(Read::Ok(_))));
    drop(r);
    // 書き込みも続けられる (置き換えたファイルを開き直している)。
    let more = env(&store, "turn", "架空の続き");
    let store = write_all(
        store,
        vec![WriteRequest::conversation(vec![WriteOp::Put(more)])],
    );
    assert!(!dir.path().join(COMPACTING).exists());
    drop(store);

    let bytes = db_bytes(dir.path());
    assert!(!contains(&bytes, GONE_TEXT), "the deleted text is gone");
    assert!(contains(&bytes, KEPT_TEXT), "live records are kept");

    // 印が無ければ何もしない。
    let (store, mut report) = open(dir.path());
    let _store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert_eq!(report.compaction, Compaction::NotPending);
}

/// 作り直せる派生の記録 (ジャーナルに書かない種類) の削除では印を立てない。呼び手は `SetCompactPending` で立てる。
/// verifies: 03 §3.1.1 (`compact.pending`)
#[test]
fn derived_deletes_do_not_set_the_flag_but_the_explicit_op_does() {
    let dir = TempDir::new("compact-derived");
    let (store, _) = open(dir.path());
    let chunk = env(&store, "chunk", "架空のチャンク");
    let id = chunk.id;
    let store = write_all(
        store,
        vec![
            WriteRequest::background(vec![WriteOp::Put(chunk)]),
            WriteRequest::background(vec![WriteOp::Delete {
                kind: "chunk".into(),
                id,
            }]),
        ],
    );
    assert!(!store.compact_pending().unwrap());
    drop(store);
    let (store, _) = open(dir.path());
    let store = write_all(
        store,
        vec![WriteRequest::conversation(vec![WriteOp::SetCompactPending])],
    );
    assert!(store.compact_pending().unwrap());
}

/// 空きが足りなければ compact せずに起動を続け、印を残す。空きができた次の起動で compact する。
/// verifies: 03 §8 (compact の前の空きの検査)、04 §7.8
#[test]
fn a_compact_is_skipped_when_space_is_short_and_retried_later() {
    let dir = TempDir::new("compact-low-space");
    write_and_delete(dir.path());
    let before = db_bytes(dir.path());

    let (store, mut report) = open(dir.path());
    let store = store
        .compact_if_pending(&mut report, |_: &Path| Some(1024))
        .unwrap();
    match &report.compaction {
        Compaction::LowSpace { free, need } => {
            assert_eq!(*free, 1024);
            assert!(*need > before.len() as u64, "need covers the copy");
        }
        other => panic!("expected LowSpace, got {other:?}"),
    }
    assert!(store.compact_pending().unwrap(), "the flag is kept");
    assert!(!dir.path().join(COMPACTING).exists(), "nothing is written");
    drop(store);

    let (store, mut report) = open(dir.path());
    let store = store
        .compact_if_pending(&mut report, |_: &Path| Some(u64::MAX))
        .unwrap();
    assert!(matches!(report.compaction, Compaction::Done { .. }));
    assert!(!store.compact_pending().unwrap());
    drop(store);
    assert!(!contains(&db_bytes(dir.path()), GONE_TEXT));
}

/// compact が置き換えの前に止まった (写しが残った) ときは、次に開くときに写しを捨て、印が残っているので compact を
/// やり直す。壊れた写し (書きかけ) が残っていても開ける。
/// verifies: 03 §8 (compact の途中の強制終了)、09 (compact の途中で `compact.pending` が残り、次の起動でやり直す)
#[test]
fn a_compact_stopped_before_the_swap_is_redone_at_the_next_open() {
    let dir = TempDir::new("compact-crash");
    let (_, kept) = write_and_delete(dir.path());

    let (store, mut report) = open(dir.path());
    let store = store
        .compact_with(&mut report, |_: &Path| None, CompactStop::BeforeSwap)
        .unwrap();
    assert!(dir.path().join(COMPACTING).exists(), "the copy is left");
    assert!(
        store.compact_pending().unwrap(),
        "the original keeps the flag"
    );
    drop(store);

    let (store, mut report) = open(dir.path());
    assert!(
        !dir.path().join(COMPACTING).exists(),
        "the leftover copy is removed at open"
    );
    let store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert!(matches!(report.compaction, Compaction::Done { .. }));
    assert!(matches!(
        store.read().unwrap().get("turn", kept).unwrap(),
        Some(Read::Ok(_))
    ));
    drop(store);
    assert!(!contains(&db_bytes(dir.path()), GONE_TEXT));

    // 書きかけの写し (redb として読めないもの) が残っていても、開くときに捨てる。
    std::fs::write(dir.path().join(COMPACTING), b"partial").unwrap();
    let (store, _) = open(dir.path());
    assert!(!dir.path().join(COMPACTING).exists());
    assert!(!store.compact_pending().unwrap());
}

/// 開くときにジャーナルの削除の行で記録を消したら、同じコミットで印を立てる。
/// verifies: 03 §3.1.1 (`compact.pending`)、03 §6 (開くときの削除)
#[test]
fn deletes_applied_at_open_set_the_flag() {
    let dir = TempDir::new("compact-open-delete");
    let (gone, _) = write_and_delete(dir.path());
    // 古い版が、消した記録を DB に書き戻した (ジャーナルには削除の行がある)。印も無い。
    let (store, _) = open(dir.path());
    let tok = store.take_write_token().unwrap();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    let mut back = env(&store, "turn", "架空の書き戻し");
    back.id = gone;
    tx.put(&mut back).unwrap();
    tx.clear_compact_pending().unwrap();
    tx.commit().unwrap();
    assert!(!store.compact_pending().unwrap());
    drop(store);

    let (store, report) = open(dir.path());
    assert_eq!(report.journal_deletes.len(), 1);
    assert!(store.compact_pending().unwrap());
}

/// `last_writer` が 0.3.0 より古いデータ根を v0.3 が書き込みで開くと、印を立てる。同じ版で開き直しても立てない。
/// verifies: 03 §3.1.1 (`compact.pending` の「`last_writer` が 0.3.0 より古い」)
#[test]
fn opening_data_from_an_older_writer_sets_the_flag() {
    let dir = TempDir::new("compact-old-writer");
    let (store, _) = open_as(dir.path(), "0.2.0");
    assert!(!store.compact_pending().unwrap());
    drop(store);
    let (store, _) = open_as(dir.path(), "0.2.0");
    assert!(!store.compact_pending().unwrap(), "same version: no flag");
    drop(store);

    let (store, mut report) = open_as(dir.path(), "0.3.0");
    assert!(store.compact_pending().unwrap());
    let store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert!(!store.compact_pending().unwrap());
    // compact は `meta` のほかの値を変えない。
    assert_eq!(
        store.meta(meta::LAST_WRITER).unwrap().as_deref(),
        Some("0.3.0")
    );
    drop(store);
    let (store, _) = open_as(dir.path(), "0.3.0");
    assert!(!store.compact_pending().unwrap(), "only once");
    drop(store);

    // 無い・知らない値は「要らない」(compact しない)。
    raw_set_meta(dir.path(), meta::COMPACT_PENDING, "yes");
    let (store, mut report) = open_as(dir.path(), "0.3.0");
    let _store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert_eq!(report.compaction, Compaction::NotPending);
}

/// ジャーナルの取り込みで、改訂の番号を上げた (伏せ字にした) ターンを当てて書き換えたら、同じコミットで印を立てる
/// (伏せ字の前の本文がファイルの空きに残るため)。番号を上げない取り込みでは立てない。
/// verifies: 03 §3.1.1 (`compact.pending`)、03 §6 (取り込み)
#[test]
fn importing_a_redacted_turn_sets_the_flag() {
    const REVISED: KindSpec = KindSpec {
        journaled: true,
        monotonic: Some("rev"),
        ..KindSpec::new("turn", 1)
    };
    #[derive(serde::Serialize)]
    struct Revised {
        text: String,
        rev: u64,
    }
    let dir = TempDir::new("compact-import-redacted");
    let open_revised = || {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.2.0".into(),
            kinds: vec![REVISED],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 13)),
            config_file: None,
        })
        .unwrap()
        .0
    };
    let store = open_revised();
    let mut before = Envelope::new(store.ids().next_id(), "turn", 1, store.clock().now(), "");
    before
        .set_body(&Revised {
            text: GONE_TEXT.into(),
            rev: 0,
        })
        .unwrap();
    let mut after = before.clone();
    after
        .set_body(&Revised {
            text: "[非表示]".into(),
            rev: 1,
        })
        .unwrap();
    let store = write_all(
        store,
        vec![
            WriteRequest::conversation(vec![WriteOp::Put(before.clone())]),
            WriteRequest::conversation(vec![WriteOp::Put(after)]),
        ],
    );
    drop(store);
    // 伏せ字の前の DB に戻った (退避から戻した) 状態: 2 行目はまだ取り込んでいない。印も無い。
    let store = open_revised();
    let tok = store.take_write_token().unwrap();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    tx.put(&mut before).unwrap();
    tx.clear_compact_pending().unwrap();
    tx.commit().unwrap();
    assert!(!store.compact_pending().unwrap());
    drop(store);
    raw_set_meta(dir.path(), meta::JOURNAL_LAST_SEQ, "1");

    let store = open_revised();
    assert!(store.compact_pending().unwrap());
}

/// 写しを残して止めた (置き換えの前の強制終了) 後に、`evorag.redb` の状態を `damage` で変える。
fn left_copy_then(damage: fn(&Path)) -> (TempDir, Id) {
    let dir = TempDir::new("compact-left-copy");
    let (_, kept) = write_and_delete(dir.path());
    let (store, mut report) = open(dir.path());
    drop(
        store
            .compact_with(&mut report, |_: &Path| None, CompactStop::BeforeSwap)
            .unwrap(),
    );
    assert!(dir.path().join(COMPACTING).exists());
    damage(dir.path());
    (dir, kept)
}

fn remove_main(p: &Path) {
    std::fs::remove_file(p.join(DB_FILE)).unwrap();
}

fn empty_main(p: &Path) {
    std::fs::write(p.join(DB_FILE), b"").unwrap();
}

fn keep_main(_: &Path) {}

/// 原子的でない rename の途中で `evorag.redb` が無くなった・空になったときは、開くときに完全な写しを戻す (空の
/// DB を作らない)。どちらも完全なら写しを捨てる。
/// verifies: 03 §8 (compact の途中の強制終了、原子的でない rename)
#[test]
fn a_complete_copy_is_put_back_when_the_main_file_is_missing_or_empty() {
    for (label, damage) in [("missing", remove_main as fn(&Path)), ("empty", empty_main)] {
        let (dir, kept) = left_copy_then(damage);
        let (store, report) = open(dir.path());
        assert!(!report.created, "{label}: no fresh database");
        assert!(!dir.path().join(COMPACTING).exists(), "{label}");
        assert!(
            matches!(
                store.read().unwrap().get("turn", kept).unwrap(),
                Some(Read::Ok(_))
            ),
            "{label}: the copy holds the records"
        );
        assert!(
            !store.compact_pending().unwrap(),
            "{label}: the copy is the compacted file"
        );
        drop(store);
        assert!(!contains(&db_bytes(dir.path()), GONE_TEXT), "{label}");
    }

    // 両方が完全なら、写しを捨てて本体を使う (印は残り、compact をやり直す)。
    let (dir, _) = left_copy_then(keep_main);
    let (store, _) = open(dir.path());
    assert!(!dir.path().join(COMPACTING).exists());
    assert!(store.compact_pending().unwrap());
}

/// 本体が無く、写しも完全でなければ、新しい DB を作らずに開くのを止め、写しを残す。本体が空でも同じ。
/// verifies: 03 §8 (compact の途中の強制終了)
#[test]
fn no_fresh_database_is_created_beside_a_leftover_copy() {
    for keep_empty_main in [false, true] {
        let dir = TempDir::new("compact-no-fresh");
        write_and_delete(dir.path());
        remove_main(dir.path());
        if keep_empty_main {
            empty_main(dir.path());
        }
        std::fs::write(dir.path().join(COMPACTING), b"partial").unwrap();
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        let r = Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.2.0".into(),
            kinds: vec![TURN, CHUNK],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 11)),
            config_file: None,
        });
        assert!(
            matches!(r, Err(evorag_store::OpenError::Unreadable(_))),
            "{keep_empty_main}"
        );
        assert!(dir.path().join(COMPACTING).exists(), "the copy is kept");
        assert!(
            !dir.path().join("broken").exists(),
            "the main file is not moved aside"
        );
    }
}

/// 置き換えに失敗しても本体が残っていれば、写しを捨てて元のまま動く (印は残る)。本体が消えた後に失敗したら
/// (原子的でない rename)、写しを残して開くのを止め、次に開くときに写しを戻す。
/// verifies: 03 §8 (compact の置き換えの失敗)
#[test]
fn a_failed_swap_keeps_the_data() {
    let dir = TempDir::new("compact-swap-fails");
    let (_, kept) = write_and_delete(dir.path());
    let (store, mut report) = open(dir.path());
    let store = store
        .compact_with(&mut report, |_: &Path| None, CompactStop::SwapFails)
        .unwrap();
    assert!(matches!(report.compaction, Compaction::Failed { .. }));
    assert!(!dir.path().join(COMPACTING).exists());
    assert!(store.compact_pending().unwrap());
    assert!(matches!(
        store.read().unwrap().get("turn", kept).unwrap(),
        Some(Read::Ok(_))
    ));
    drop(store);

    let (store, mut report) = open(dir.path());
    let r = store.compact_with(
        &mut report,
        |_: &Path| None,
        CompactStop::SwapFailsAfterRemovingMain,
    );
    assert!(matches!(r, Err(evorag_store::OpenError::Unreadable(_))));
    drop(r);
    assert!(dir.path().join(COMPACTING).exists(), "the copy is kept");
    let (store, _) = open(dir.path());
    assert!(matches!(
        store.read().unwrap().get("turn", kept).unwrap(),
        Some(Read::Ok(_))
    ));
    assert!(!store.compact_pending().unwrap());
}

/// 知らない表 (新しい版が足した表) があれば、写すと失うので compact しない (03 §4.2 の規則 6)。
/// verifies: 03 §8 (compact)、03 §4.2 の規則 6
#[test]
fn unknown_tables_are_not_compacted_away() {
    let dir = TempDir::new("compact-unknown-table");
    write_and_delete(dir.path());
    {
        let db = redb::Database::create(dir.path().join(DB_FILE)).unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(redb::TableDefinition::<&str, &str>::new("future"))
            .unwrap()
            .insert("k", "v")
            .unwrap();
        tx.commit().unwrap();
    }
    let (store, mut report) = open(dir.path());
    let store = store
        .compact_if_pending(&mut report, |_: &Path| None)
        .unwrap();
    assert!(
        matches!(&report.compaction, Compaction::Failed { reason } if reason.contains("future")),
        "{:?}",
        report.compaction
    );
    assert!(store.compact_pending().unwrap());
}

/// 書き手は、`Put` と `Patch` で改訂の番号を上げた (伏せ字にした) 依頼に印を自動で立てる。上げない書き換えでは
/// 立てない。
/// verifies: 03 §3.1.1 (`compact.pending`)
#[test]
fn the_writer_flags_rewrites_that_raise_the_revision() {
    const REVISED: KindSpec = KindSpec {
        journaled: true,
        monotonic: Some("rev"),
        ..KindSpec::new("turn", 1)
    };
    #[derive(serde::Serialize)]
    struct Revised {
        text: String,
        rev: u64,
    }
    let dir = TempDir::new("compact-writer-rev");
    let open_revised = || {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)));
        Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.2.0".into(),
            kinds: vec![REVISED],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 17)),
            config_file: None,
        })
        .unwrap()
        .0
    };
    let store = open_revised();
    let mut turn = Envelope::new(store.ids().next_id(), "turn", 1, store.clock().now(), "");
    turn.set_body(&Revised {
        text: GONE_TEXT.into(),
        rev: 0,
    })
    .unwrap();
    let mut same = turn.clone();
    same.set_body(&Revised {
        text: "架空の書き換え".into(),
        rev: 0,
    })
    .unwrap();
    let mut raised = turn.clone();
    raised
        .set_body(&Revised {
            text: "[非表示]".into(),
            rev: 1,
        })
        .unwrap();
    let id = turn.id;
    let store = write_all(
        store,
        vec![
            WriteRequest::conversation(vec![WriteOp::Put(turn)]),
            WriteRequest::conversation(vec![WriteOp::Put(same)]),
        ],
    );
    assert!(!store.compact_pending().unwrap(), "same revision: no flag");
    drop(store);
    let store = write_all(
        open_revised(),
        vec![WriteRequest::conversation(vec![WriteOp::Put(raised)])],
    );
    assert!(store.compact_pending().unwrap(), "Put raising the revision");
    drop(store);

    // Patch で上げる (忘れるの伏せ字の形)。
    raw_set_meta(dir.path(), meta::COMPACT_PENDING, "0");
    let fields = evorag_store::BodyPatch::new().set("rev", &2u64).unwrap();
    let store = write_all(
        open_revised(),
        vec![WriteRequest::conversation(vec![WriteOp::Patch {
            kind: "turn".into(),
            id,
            fields,
            updated_at: None,
        }])],
    );
    assert!(
        store.compact_pending().unwrap(),
        "Patch raising the revision"
    );
}
