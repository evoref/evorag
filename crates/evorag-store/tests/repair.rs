//! `evorag repair` のストアの層 (03 §10): ジャーナルにあって DB に無い記録と `meta` のキーを戻し (単調に増える項目を
//! 下げる行は当てない)、関係と副索引を記録から作り直す。

use std::path::Path;
use std::sync::Arc;

use evorag_store::key::OwnedPart;
use evorag_store::testutil::{self, TempDir};
use evorag_store::{
    BodyPatch, Envelope, KindSpec, LookupRange, OpenOptions, Read, Repaired, Store, StoreError,
    WriteOp, WriteRequest, Writer, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::{Deserialize, Serialize};

const OF: &str = "of";
/// `derive_turnish` が作る副索引の項目 (改訂の番号)。
const REV: &str = "rev";

/// ターンの代わり: ジャーナルに書き、改訂の番号 `rev` を単調に増える項目として宣言し、`parent` への関係と、`rev`
/// の副索引を本体から作る。
const TURNISH: KindSpec = KindSpec {
    journaled: true,
    monotonic: Some("rev"),
    derive: Some(derive_turnish),
    derived_rels: &[OF],
    derived_lookups: &[REV],
    ..KindSpec::new("turnish", 1)
};

/// ジャーナルに書かない派生の記録の代わり (壊れても、ジャーナルから戻らない)。
const DERIVED: KindSpec = KindSpec {
    derive: Some(derive_turnish),
    derived_rels: &[OF],
    derived_lookups: &[REV],
    ..KindSpec::new("derivedish", 1)
};

/// 忘れるの指示の代わり (`derive` を持たない)。
const FORGETISH: KindSpec = KindSpec {
    journaled: true,
    ..KindSpec::new("forgetish", 1)
};

#[derive(Serialize, Deserialize)]
struct Turn {
    text: String,
    rev: u64,
    parent: Id,
}

fn derive_turnish(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<Turn>() {
        Ok(b) => vec![
            WriteOp::PutLink {
                from: e.id,
                rel: OF.into(),
                to: b.parent,
                attrs: Vec::new(),
            },
            WriteOp::PutLookup {
                kind: e.kind.clone(),
                field: REV.into(),
                value: OwnedPart::Int(i64::try_from(b.rev).expect("small rev")),
                id: e.id,
            },
        ],
        Err(_) => Vec::new(),
    }
}

fn is_forgetish(e: &Envelope) -> bool {
    e.kind == FORGETISH.name
}

fn open_with(root: &Path, kinds: Vec<KindSpec>) -> Arc<Store> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.3.0".into(),
        kinds,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap();
    Arc::new(store)
}

fn open(root: &Path) -> Arc<Store> {
    open_with(root, vec![TURNISH, DERIVED, FORGETISH])
}

fn repair(store: &Store) -> Repaired {
    store.repair(is_forgetish).unwrap()
}

fn envelope_v(store: &Store, kind: &str, v: u32, id: Id, body: &impl Serialize) -> Envelope {
    let mut e = Envelope::new(id, kind, v, store.clock().now(), "");
    e.set_body(body).unwrap();
    e
}

fn turn(store: &Store, id: Id, text: &str, rev: u64, parent: Id) -> Envelope {
    envelope_v(
        store,
        "turnish",
        1,
        id,
        &Turn {
            text: text.into(),
            rev,
            parent,
        },
    )
}

fn link(from: Id, rel: &str, to: Id) -> WriteOp {
    WriteOp::PutLink {
        from,
        rel: rel.into(),
        to,
        attrs: Vec::new(),
    }
}

/// 書き手を通して書く (ジャーナルにも載る)。
fn write(store: &Arc<Store>, batches: Vec<Vec<WriteOp>>) {
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for ops in batches {
        handle.submit_wait(WriteRequest::conversation(ops)).unwrap();
    }
    handle.shutdown();
    t.join().unwrap();
}

fn read(store: &Store, id: Id) -> Option<Read> {
    store.read().unwrap().get("turnish", id).unwrap()
}

fn links_of(store: &Store, id: Id, rel: &str) -> Vec<Id> {
    store
        .read()
        .unwrap()
        .links_from(id, rel)
        .unwrap()
        .into_iter()
        .map(|(to, _)| to)
        .collect()
}

fn lookups_of(store: &Store, kind: &str, field: &str, id: Id) -> usize {
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
        .iter()
        .filter(|e| e.id == id)
        .count()
}

/// verifies: AC-FR-211-1 (ストアの層: 失った記録と壊れた記録をジャーナルから戻し、改訂の番号を下げる行を当てず、
/// 記録の無い ID の関係を消す)
#[test]
fn missing_and_broken_records_come_back_from_the_journal_without_lowering_the_revision() {
    let dir = TempDir::new("repair-store");
    let root = dir.path();
    let store = open(root);
    let parent = store.ids().next_id();
    let (a, b, ghost) = (
        store.ids().next_id(),
        store.ids().next_id(),
        store.ids().next_id(),
    );
    let before = turn(&store, a, "前の本文", 0, parent);
    write(
        &store,
        vec![
            vec![WriteOp::Put(before.clone()), link(a, OF, parent)],
            vec![WriteOp::Put(turn(&store, b, "別の本文", 0, parent))],
            // 伏せ字 (改訂の番号を上げる)。
            vec![WriteOp::Patch {
                kind: "turnish".into(),
                id: a,
                fields: BodyPatch::new()
                    .set("text", &"[伏せ字]")
                    .unwrap()
                    .set("rev", &1u64)
                    .unwrap(),
                updated_at: None,
            }],
            // 古い版が伏せ字の前の写しを書き戻した (ジャーナルの後ろに、改訂の番号の小さい行)。
            vec![WriteOp::Put(before)],
            // 記録の無い ID からの関係 (作り直すと消える)。
            vec![link(ghost, OF, parent)],
        ],
    );
    drop(store);
    // DB から a を失い、b が壊れる。b の関係は書いていない (repair が本体から作る)。
    testutil::raw_remove_record(root, "turnish", a);
    testutil::raw_corrupt_record(root, "turnish", b);

    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.restored.get("turnish"), Some(&2), "{r:?}");
    assert_eq!(r.restored_total(), 2);
    assert_eq!(r.quarantined, 1, "{r:?}");
    assert_eq!(r.lower_rev_skipped, 1, "{r:?}");
    // b の関係を書き足し (順と逆の 2 行。a の関係は DB に残っていた)、記録の無い ID の関係を消した。
    assert_eq!(r.links_added, 2, "{r:?}");
    assert_eq!(r.links_removed, 2, "{r:?}");
    match read(&store, a) {
        Some(Read::Ok(e)) => {
            let body: Turn = e.body_as().unwrap();
            assert_eq!((body.text.as_str(), body.rev), ("[伏せ字]", 1));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(read(&store, b), Some(Read::Ok(_))));
    assert_eq!(links_of(&store, a, OF), [parent]);
    assert_eq!(links_of(&store, b, OF), [parent]);
    assert!(links_of(&store, ghost, OF).is_empty());
    assert_eq!(lookups_of(&store, "turnish", REV, a), 1);
    // 忘れるの指示が無いので、記録を戻しても `forget.recheck` は立てない。
    assert!(!r.forget_recheck, "{r:?}");
    assert_eq!(store.meta(meta::FORGET_RECHECK).unwrap(), None);

    // 2 回目は何も変えない。
    let again = repair(&store);
    assert_eq!(again.restored_total(), 0, "{again:?}");
    assert_eq!(
        (
            again.links_added,
            again.links_removed,
            again.lookups_added,
            again.lookups_removed
        ),
        (0, 0, 0, 0),
        "{again:?}"
    );
}

/// 削除の行のある ID の記録は、DB に残っていても戻さず、repair の後には DB からも消えている。開くときに消して
/// まだ整理の job にしていない ID (`cleanup.pending_deletes`) も戻さない。
///
/// verifies: AC-FR-211-1 (削除の行と pending_deletes を消したとみなす)
#[test]
fn deleted_ids_are_not_restored_and_stale_records_are_removed() {
    let dir = TempDir::new("repair-deleted");
    let root = dir.path();
    let store = open(root);
    let parent = store.ids().next_id();
    let (x, y) = (store.ids().next_id(), store.ids().next_id());
    write(
        &store,
        vec![
            vec![WriteOp::Put(turn(&store, x, "消す", 0, parent))],
            vec![WriteOp::Put(turn(&store, y, "失う", 0, parent))],
        ],
    );
    drop(store);
    // x を消す前の DB を取っておく。
    let db = root.join(evorag_store::DB_FILE);
    let saved = root.join("saved.redb");
    std::fs::copy(&db, &saved).unwrap();
    let store = open(root);
    write(
        &store,
        vec![vec![WriteOp::Delete {
            kind: "turnish".into(),
            id: x,
        }]],
    );
    let last_seq = store.meta(meta::JOURNAL_LAST_SEQ).unwrap().unwrap();
    drop(store);
    // 古い版が x を書き戻した DB (ジャーナルは取り込み済み)。y は、開くときに消して整理の job にしていない ID。
    std::fs::copy(&saved, &db).unwrap();
    testutil::raw_set_meta(root, meta::JOURNAL_LAST_SEQ, &last_seq);
    testutil::raw_remove_record(root, "turnish", y);
    testutil::raw_set_meta(root, meta::PENDING_DELETES, &y.to_string());

    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.restored_total(), 0, "{r:?}");
    assert!(read(&store, x).is_none(), "the deleted record is gone");
    assert!(
        read(&store, y).is_none(),
        "a pending delete is not restored"
    );
    assert!(links_of(&store, x, OF).is_empty());
}

/// 作り直せる記録 (知っている種類・今の版・読める) の関係と副索引は、`derive` が作りうる名前の行だけを作り直す:
/// 失った関係と副索引は足し、本体と合わない宣言の名前の行は消し、宣言の無い名前の行 (新しい版が書いたもの) は残す。
///
/// verifies: AC-FR-211-1 (失った関係の作り直しと、知らない関係・副索引を残すこと、03 §4.2 の規則 6・11)
#[test]
fn intact_records_get_their_relations_back_and_unknown_names_are_kept() {
    let dir = TempDir::new("repair-relations");
    let root = dir.path();
    let store = open(root);
    let (parent, other) = (store.ids().next_id(), store.ids().next_id());
    let (t1, t2) = (store.ids().next_id(), store.ids().next_id());
    write(
        &store,
        vec![
            vec![
                WriteOp::Put(turn(&store, t1, "一", 0, parent)),
                link(t1, OF, parent),
            ],
            vec![
                WriteOp::Put(turn(&store, t2, "二", 0, parent)),
                link(t2, OF, parent),
                // 本体と合わない宣言の名前の関係 (作り直すと消える)。
                link(t2, OF, other),
                // 新しい版が足した関係と副索引の項目 (残す)。
                link(t2, "future_rel", other),
                WriteOp::PutLookup {
                    kind: "turnish".into(),
                    field: "future_field".into(),
                    value: OwnedPart::Int(7),
                    id: t2,
                },
            ],
        ],
    );
    drop(store);
    assert_eq!(testutil::raw_remove_links(root, t1, OF), 2);
    assert!(testutil::raw_remove_lookups(root, "turnish") > 0);

    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.restored_total(), 0, "{r:?}");
    // t1 の関係 (順と逆) を足し、t2 の合わない関係 (順と逆) を消した。
    assert_eq!((r.links_added, r.links_removed), (2, 2), "{r:?}");
    assert_eq!(links_of(&store, t1, OF), [parent]);
    assert_eq!(links_of(&store, t2, OF), [parent]);
    assert_eq!(links_of(&store, t2, "future_rel"), [other]);
    assert_eq!(lookups_of(&store, "turnish", REV, t1), 1);
    assert_eq!(lookups_of(&store, "turnish", REV, t2), 1);
    // 消した副索引の中の知らない項目は戻らないが、残っていれば消さない。
    assert_eq!(lookups_of(&store, "turnish", "future_field", t2), 0);
    drop(store);

    let store = open(root);
    write(
        &store,
        vec![vec![WriteOp::PutLookup {
            kind: "turnish".into(),
            field: "future_field".into(),
            value: OwnedPart::Int(7),
            id: t2,
        }]],
    );
    drop(store);
    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.lookups_removed, 0, "{r:?}");
    assert_eq!(lookups_of(&store, "turnish", "future_field", t2), 1);
}

/// 作り直せない記録 (知らない種類・新しい版・本体を読めない・壊れていてジャーナルから戻らない) の関係は残す。
///
/// verifies: AC-FR-211-1 (作り直せない記録の関係を残す、03 §4.2 の規則 6・11)
#[test]
fn relations_of_records_that_cannot_be_rederived_are_kept() {
    let dir = TempDir::new("repair-keep");
    let root = dir.path();
    const ALIEN: KindSpec = KindSpec::new("alien", 1);
    const TURNISH_V2: KindSpec = KindSpec {
        current_v: 2,
        ..TURNISH
    };
    let store = open_with(root, vec![TURNISH, DERIVED, ALIEN]);
    let (parent, other) = (store.ids().next_id(), store.ids().next_id());
    let (alien, unreadable, broken, newer) = (
        store.ids().next_id(),
        store.ids().next_id(),
        store.ids().next_id(),
        store.ids().next_id(),
    );
    write(
        &store,
        vec![
            vec![
                WriteOp::Put(envelope_v(
                    &store,
                    "alien",
                    1,
                    alien,
                    &serde_json::json!({"x": 1}),
                )),
                link(alien, OF, parent),
            ],
            // `parent` の無い本体 (関係に要る項目を読めない)。
            vec![
                WriteOp::Put(envelope_v(
                    &store,
                    "turnish",
                    1,
                    unreadable,
                    &serde_json::json!({"text": "?", "rev": 0}),
                )),
                link(unreadable, OF, other),
            ],
            // ジャーナルに書かない種類 (壊れても戻らない)。
            vec![
                WriteOp::Put(envelope_v(
                    &store,
                    "derivedish",
                    1,
                    broken,
                    &Turn {
                        text: "派生".into(),
                        rev: 0,
                        parent,
                    },
                )),
                link(broken, OF, other),
            ],
        ],
    );
    drop(store);
    let store = open_with(root, vec![TURNISH_V2]);
    write(
        &store,
        vec![vec![
            WriteOp::Put(envelope_v(
                &store,
                "turnish",
                2,
                newer,
                &Turn {
                    text: "新しい版".into(),
                    rev: 0,
                    parent,
                },
            )),
            link(newer, OF, other),
        ]],
    );
    drop(store);
    testutil::raw_corrupt_record(root, "derivedish", broken);

    // この版は `alien` を知らず、`turnish` を v1 までしか知らない。
    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.restored_total(), 0, "{r:?}");
    assert_eq!(r.links_removed, 0, "{r:?}");
    assert_eq!(links_of(&store, alien, OF), [parent]);
    assert_eq!(links_of(&store, unreadable, OF), [other]);
    assert_eq!(links_of(&store, broken, OF), [other]);
    // 新しい版の記録は、前の関係を残したまま、読める分を書き足す。
    let mut newer_links = links_of(&store, newer, OF);
    newer_links.sort();
    let mut want = vec![parent, other];
    want.sort();
    assert_eq!(newer_links, want);
}

/// ジャーナルの `meta` の行は、DB にそのキーが無いときだけ戻す。チェックサムの合わない行は数えて知らせる。
///
/// verifies: AC-FR-211-1 (ジャーナルの meta の行を戻す)
#[test]
fn meta_lines_come_back_and_bad_lines_are_counted() {
    let dir = TempDir::new("repair-meta");
    let root = dir.path();
    let store = open(root);
    let parent = store.ids().next_id();
    let t = store.ids().next_id();
    write(
        &store,
        vec![
            vec![WriteOp::SetMetaOnce {
                key: meta::PROMPT_LANGUAGE.into(),
                value: "en".into(),
            }],
            vec![WriteOp::Put(turn(&store, t, "本文", 0, parent))],
        ],
    );
    drop(store);
    testutil::raw_remove_meta(root, meta::PROMPT_LANGUAGE);
    // チェックサムの合わない行を 1 行足す。
    let journal = root.join(evorag_store::JOURNAL_DIR);
    let last = std::fs::read_dir(&journal)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .max()
        .unwrap();
    let mut bytes = std::fs::read(&last).unwrap();
    bytes.extend_from_slice(b"{\"seq\":999999,\"op\":\"put\",\"sum\":\"00\"}\n");
    std::fs::write(&last, bytes).unwrap();

    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.metas_restored, 1, "{r:?}");
    assert_eq!(r.journal_bad_lines, 1, "{r:?}");
    assert_eq!(
        store.meta(meta::PROMPT_LANGUAGE).unwrap().as_deref(),
        Some("en")
    );
    let again = repair(&store);
    assert_eq!(again.metas_restored, 0, "{again:?}");
}

/// 忘れるの指示が 1 つでもあれば、記録を戻さなくても `forget.recheck` を立てる。
///
/// verifies: AC-FR-211-1 (forget.recheck)
#[test]
fn forget_recheck_is_set_whenever_a_forget_directive_exists() {
    let dir = TempDir::new("repair-recheck");
    let root = dir.path();
    let store = open(root);
    let r = repair(&store);
    assert!(!r.forget_recheck);
    let d = store.ids().next_id();
    write(
        &store,
        vec![vec![WriteOp::Put(envelope_v(
            &store,
            "forgetish",
            1,
            d,
            &serde_json::json!({}),
        ))]],
    );
    drop(store);
    let store = open(root);
    let r = repair(&store);
    assert_eq!(r.restored_total(), 0, "{r:?}");
    assert!(r.forget_recheck, "{r:?}");
    assert_eq!(
        store.meta(meta::FORGET_RECHECK).unwrap().as_deref(),
        Some(meta::FORGET_RECHECK_SET)
    );
}

#[test]
fn repair_is_refused_once_the_writer_has_started() {
    let dir = TempDir::new("repair-writer");
    let store = open(dir.path());
    let (_writer, _handle) = Writer::new(store.clone()).unwrap();
    assert!(matches!(
        store.repair(is_forgetish),
        Err(StoreError::TokenTaken)
    ));
}
