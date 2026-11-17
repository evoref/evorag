//! 忘れるの退避の写し直し (03 §7.3 の `forget_backups`) と、restore の引き継ぎ (03 §10) の試験。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use ciborium::Value;
use evorag_store::forget::{Redaction, TextHash};
use evorag_store::key::{KeyBuf, OwnedPart, Part};
use evorag_store::meta;
use evorag_store::recopy::{ForgetRecopy, LookupProbe, RecopyCount, RecopyView, TurnInCopy};
use evorag_store::restore::{list_backups, restore};
use evorag_store::testutil::TempDir;
use evorag_store::{DB_FILE, Durability, Envelope, KIND_QUARANTINE, KindSpec, OpenOptions, Store};
use evorag_types::{Clock, Id, ManualClock, Timestamp, UuidV7Gen};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};

type Raw = TableDefinition<'static, &'static [u8], &'static [u8]>;
const META: Raw = TableDefinition::new("meta");
const RECORDS: Raw = TableDefinition::new("records");
const LINKS: Raw = TableDefinition::new("links");
const LOOKUP: Raw = TableDefinition::new("lookup");
const BLOBS: Raw = TableDefinition::new("blobs");

const M: &str = "[redacted]";

fn kinds() -> Vec<KindSpec> {
    ["turn", "chunk", "fact", "job", "sample"]
        .into_iter()
        .map(|k| KindSpec::new(k, 1))
        .collect()
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(Timestamp::from_millis(1_767_614_400_000)))
}

fn open(root: &Path, clock: Arc<ManualClock>) -> Store {
    Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.3.0".into(),
        kinds: kinds(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap()
    .0
}

fn map(fields: Vec<(&str, Value)>) -> Value {
    Value::Map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

fn text_of(env: &Envelope) -> Option<String> {
    let Value::Map(m) = &env.body else {
        return None;
    };
    m.iter()
        .find(|(k, _)| k.as_text() == Some("text"))
        .and_then(|(_, v)| v.as_text().map(str::to_owned))
}

fn field_id(env: &Envelope, name: &str) -> Option<Id> {
    let Value::Map(m) = &env.body else {
        return None;
    };
    m.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .and_then(|(_, v)| v.as_text())
        .and_then(|s| s.parse().ok())
}

fn record_key(kind: &str, id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind).id(id);
    k.into_bytes()
}

/// 1 つの表のすべての行。
fn rows(file: &Path, table: Raw) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let db = redb::Database::open(file).unwrap();
    let rtx = db.begin_read().unwrap();
    let t = rtx.open_table(table).unwrap();
    t.iter()
        .unwrap()
        .map(|i| {
            let (k, v) = i.unwrap();
            (k.value().to_vec(), v.value().to_vec())
        })
        .collect()
}

fn mentions(key: &[u8], id: Id) -> bool {
    key.windows(16).any(|w| w == id.as_bytes())
}

/// 試験の記録の ID。
struct Ids {
    session: Id,
    secret_turn: Id,
    other_turn: Id,
    stale_chunk: Id,
    other_chunk: Id,
    fact: Id,
    other_fact: Id,
    removed: Id,
    quarantined_turn: Id,
    other_quarantine: Id,
    reply: Id,
    signal: Id,
}

const SECRET_TEXT: &str = "my locker code is 4242, remember";

/// 伏せ字にするターンとその派生の記録、関係の無い記録を書く。
fn seed(store: &Store) -> Ids {
    let tok = store.take_write_token().unwrap();
    let mut tx = store.write(&tok, Durability::Immediate).unwrap();
    let now = store.clock().now();
    let next = |kind: &str, body: Value| {
        let mut e = Envelope::new(store.ids().next_id(), kind, 1, now, "");
        e.body = body;
        e
    };
    let mut session = next("sample", map(vec![("title", "s".into())]));
    let mut secret = next(
        "turn",
        map(vec![("text", SECRET_TEXT.into()), ("role", "user".into())]),
    );
    let mut other = next(
        "turn",
        map(vec![("text", "nothing to see".into()), ("rev", 0.into())]),
    );
    let chunk = |turn: Id| {
        map(vec![
            ("turn", turn.to_string().into()),
            ("src_rev", 0.into()),
        ])
    };
    let mut stale_chunk = next("chunk", chunk(secret.id));
    let mut other_chunk = next("chunk", chunk(other.id));
    let mut fact = next("fact", map(vec![("vk", "aa".into())]));
    let mut other_fact = next("fact", map(vec![("vk", "bb".into())]));
    let mut removed = next("sample", map(vec![("n", 1.into())]));
    let quarantine = |id: Id, raw: &str| {
        map(vec![
            ("kind", "turn".into()),
            ("id", id.to_string().into()),
            ("raw", Value::Bytes(raw.as_bytes().to_vec())),
        ])
    };
    let mut q_secret = next(KIND_QUARANTINE, quarantine(secret.id, SECRET_TEXT));
    let mut q_other = next(KIND_QUARANTINE, quarantine(other.id, "nothing"));
    let mut reply = next("turn", map(vec![("text", "noted".into())]));
    let mut signal = next("sample", map(vec![("kind", "bad".into())]));
    for e in [
        &mut reply,
        &mut signal,
        &mut session,
        &mut secret,
        &mut other,
        &mut stale_chunk,
        &mut other_chunk,
        &mut fact,
        &mut other_fact,
        &mut removed,
        &mut q_secret,
        &mut q_other,
    ] {
        tx.put(e).unwrap();
    }
    tx.put_link(secret.id, "in_session", session.id, &[])
        .unwrap();
    tx.put_link(other.id, "in_session", session.id, &[])
        .unwrap();
    tx.put_link(removed.id, "about", other.id, &[]).unwrap();
    tx.put_link(reply.id, "reply_to", secret.id, &[]).unwrap();
    tx.put_link(signal.id, "target", secret.id, &[]).unwrap();
    tx.put_lookup("sample", "target", &Part::Id(secret.id), signal.id)
        .unwrap();
    tx.put_lookup("chunk", "turn", &Part::Id(secret.id), stale_chunk.id)
        .unwrap();
    tx.put_lookup("chunk", "turn", &Part::Id(other.id), other_chunk.id)
        .unwrap();
    tx.put_lookup("fact", "vk", &Part::Str("aa"), fact.id)
        .unwrap();
    tx.put_lookup("fact", "vk", &Part::Str("bb"), other_fact.id)
        .unwrap();
    tx.put_blob(stale_chunk.id, 0, b"vector-of-secret").unwrap();
    tx.put_blob(other_chunk.id, 0, b"vector-of-other").unwrap();
    tx.put_blob(removed.id, 0, b"removed-blob").unwrap();
    tx.commit().unwrap();
    Ids {
        session: session.id,
        secret_turn: secret.id,
        other_turn: other.id,
        stale_chunk: stale_chunk.id,
        other_chunk: other_chunk.id,
        fact: fact.id,
        other_fact: other_fact.id,
        removed: removed.id,
        quarantined_turn: q_secret.id,
        other_quarantine: q_other.id,
        reply: reply.id,
        signal: signal.id,
    }
}

fn secret_redaction() -> (Redaction, String) {
    let s = SECRET_TEXT.find("4242").unwrap();
    Redaction::plan(SECRET_TEXT, 0, std::iter::once(s..s + 4), M).unwrap()
}

/// 呼び手 (メモリの層) の判定の代わり: 伏せ字にしたターンを出典とし、出典の改訂の番号が写しと合わないチャンク、
/// 値の鍵が `aa` の事実を除く。
fn drop_rule(env: &Envelope, view: &RecopyView<'_>) -> bool {
    match env.kind.as_str() {
        "chunk" => {
            let Some(turn) = field_id(env, "turn") else {
                return false;
            };
            let src_rev = env.body_uint("src_rev").unwrap_or(0);
            match view.turn(turn) {
                Some(TurnInCopy::Kept { rev }) => u64::from(rev) != src_rev,
                Some(TurnInCopy::Dropped) => true,
                None => false,
            }
        }
        "fact" => {
            let Value::Map(m) = &env.body else {
                return false;
            };
            m.iter()
                .any(|(k, v)| k.as_text() == Some("vk") && v.as_text() == Some("aa"))
        }
        _ => false,
    }
}

fn plan<'a>(
    ids: &Ids,
    redaction: Redaction,
    drop: &'a dyn Fn(&Envelope, &RecopyView<'_>) -> bool,
) -> ForgetRecopy<'a> {
    ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::from([ids.removed]),
        turns: BTreeMap::from([(ids.secret_turn, vec![redaction])]),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: vec![LookupProbe {
            kind: "fact".into(),
            field: "vk".into(),
            value: OwnedPart::Str("aa".into()),
        }],
        drop,
    }
}

/// verifies: 03 §7.3 (忘れるの写し直しの写し方)、04 §7.2 (DB の層: 退避の写し)
#[test]
fn a_backup_is_rewritten_with_the_turn_redacted_and_targets_dropped() {
    let dir = TempDir::new("forget-recopy");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    let file = daily.join(DB_FILE);
    let before_records = rows(&file, RECORDS);
    let before_links = rows(&file, LINKS);
    let (redaction, redacted) = secret_redaction();

    let rule = drop_rule;
    let p = plan(&ids, redaction, &rule);
    let n = store.forget_backups(None, &p, &|| false).unwrap();
    // 日次と、その週の最初の退避の週次。
    assert_eq!(
        n,
        RecopyCount {
            rewritten: 2,
            skipped: 0
        }
    );

    for b in list_backups(dir.path(), None) {
        let file = b.dir.join(DB_FILE);
        let records = rows(&file, RECORDS);
        // すべての記録のチェックサムが合う。
        for (k, v) in &records {
            Envelope::decode(k, v).unwrap_or_else(|e| panic!("{}: {e}", b.dir.display()));
        }
        // ターンは伏せ字になり、改訂の番号が上がり、ほかの項目は残る。
        let turn_key = record_key("turn", ids.secret_turn);
        let turn = Envelope::decode(&turn_key, &records[&turn_key]).unwrap();
        assert_eq!(text_of(&turn).as_deref(), Some(redacted.as_str()));
        assert_eq!(turn.body_uint("rev"), Some(1));
        assert_eq!(TextHash::of(&redacted), secret_redaction().0.hash());
        // 除く記録とその派生・隔離の記録が無い。
        for (kind, id) in [
            ("chunk", ids.stale_chunk),
            ("fact", ids.fact),
            ("sample", ids.removed),
            (KIND_QUARANTINE, ids.quarantined_turn),
        ] {
            assert!(!records.contains_key(&record_key(kind, id)), "{kind} {id}");
        }
        for t in [LINKS, LOOKUP, BLOBS] {
            for k in rows(&file, t).keys() {
                for id in [ids.stale_chunk, ids.fact, ids.removed] {
                    assert!(!mentions(k, id), "a row of a dropped record is left: {id}");
                }
            }
        }
        // 関係の無い記録は、バイト列のまま残る。
        for (kind, id) in [
            ("sample", ids.session),
            ("turn", ids.other_turn),
            ("chunk", ids.other_chunk),
            ("fact", ids.other_fact),
            (KIND_QUARANTINE, ids.other_quarantine),
        ] {
            let k = record_key(kind, id);
            assert_eq!(records.get(&k), before_records.get(&k), "{kind} {id}");
        }
        // 残る記録の関係は残る (伏せ字にしたターン → セッションを含む)。
        let links = rows(&file, LINKS);
        let kept: Vec<_> = before_links
            .keys()
            .filter(|k| !mentions(k, ids.removed))
            .collect();
        assert_eq!(links.len(), kept.len());
        assert!(kept.iter().all(|k| links.contains_key(*k)));
        let blobs = rows(&file, BLOBS);
        assert!(blobs.values().any(|v| v == b"vector-of-other"));
        assert!(!blobs.values().any(|v| v == b"vector-of-secret"));
        // 忘れた本文はどの表にも残らない。
        for t in [META, RECORDS, LINKS, LOOKUP, BLOBS] {
            for (k, v) in rows(&file, t) {
                assert!(
                    !v.windows(4).any(|w| w == b"4242") && !k.windows(4).any(|w| w == b"4242"),
                    "the forgotten value is left in {}",
                    b.dir.display()
                );
            }
        }
    }

    // もう一度当てても変わらない (対象はすでに無い・当たっている)。
    let again = store.forget_backups(None, &p, &|| false).unwrap();
    assert_eq!(
        again,
        RecopyCount {
            rewritten: 0,
            skipped: 2
        }
    );
}

/// 対象を含まないことを、ID の最大か記録の表で確かめられた退避は飛ばす。
/// verifies: 03 §7.3 (飛ばす退避)
#[test]
fn backups_proven_to_hold_no_target_are_skipped() {
    let dir = TempDir::new("forget-recopy-skip");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    let file = daily.join(DB_FILE);
    // 退避の記録の ID の最大 (最後に書いた記録)。
    let max = rows(&file, RECORDS)
        .keys()
        .map(|k| Id::from_bytes(k[k.len() - 16..].try_into().unwrap()))
        .max()
        .unwrap();
    assert_eq!(max, ids.signal);

    // 退避の後にできた記録だけが対象: 記録の表から求めた ID の最大より大きいので、中を走査せずに飛ばす。
    let later = store.ids().next_id();
    assert!(later > max);
    let before: Vec<_> = [META, RECORDS, LINKS, LOOKUP, BLOBS]
        .into_iter()
        .map(|t| rows(&file, t))
        .collect();
    let never = |_: &Envelope, _: &RecopyView<'_>| -> bool { panic!("not scanned") };
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::from([later]),
        turns: BTreeMap::new(),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: Vec::new(),
        drop: &never,
    };
    let n = store.forget_backups(None, &p, &|| false).unwrap();
    assert_eq!(n.rewritten, 0);
    assert_eq!(n.skipped, 2);
    assert_eq!(
        [META, RECORDS, LINKS, LOOKUP, BLOBS]
            .into_iter()
            .map(|t| rows(&file, t))
            .collect::<Vec<_>>(),
        before,
        "a skipped backup is not touched"
    );

    // ID の最大より小さい対象も、記録の表と副索引で無いと確かめて飛ばす。
    let unknown = Id::from_bytes([0x01; 16]);
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::from([unknown]),
        turns: BTreeMap::new(),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: vec![LookupProbe {
            kind: "fact".into(),
            field: "vk".into(),
            value: OwnedPart::Str("zz".into()),
        }],
        drop: &never,
    };
    assert_eq!(
        store.forget_backups(None, &p, &|| false).unwrap().rewritten,
        0
    );

    // 副索引に当たる記録があれば写し直す。
    let rule = drop_rule;
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::new(),
        turns: BTreeMap::new(),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: vec![LookupProbe {
            kind: "fact".into(),
            field: "vk".into(),
            value: OwnedPart::Str("aa".into()),
        }],
        drop: &rule,
    };
    assert_eq!(
        store.forget_backups(None, &p, &|| false).unwrap().rewritten,
        2
    );
    assert!(!rows(&file, RECORDS).contains_key(&record_key("fact", ids.fact)));
}

/// 写しのターンが記録と合わなければ、そのターンを写しから除く (restore はジャーナルから伏せ字の後の行を足す)。
/// verifies: 04 §7.2 (DB の層: 退避の写しで合わないターンを除く)
#[test]
fn a_turn_that_does_not_match_is_dropped_from_the_copy() {
    let dir = TempDir::new("forget-recopy-mismatch");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    // 記録した本文と違う本文の伏せ字。
    let (wrong, _) = Redaction::plan(
        "your locker code is 4242, remember",
        0,
        std::iter::once(20..24),
        M,
    )
    .unwrap();
    let rule = drop_rule;
    let p = plan(&ids, wrong, &rule);
    store.forget_backups(None, &p, &|| false).unwrap();
    let records = rows(&daily.join(DB_FILE), RECORDS);
    assert!(!records.contains_key(&record_key("turn", ids.secret_turn)));
    // 除いたターンを出典とする派生の記録と隔離の記録も除く。
    assert!(!records.contains_key(&record_key("chunk", ids.stale_chunk)));
    assert!(!records.contains_key(&record_key(KIND_QUARANTINE, ids.quarantined_turn)));
    assert!(records.contains_key(&record_key("turn", ids.other_turn)));
    // 除いたターンを指す関係と副索引は ID だけなので残す (restore がジャーナルからターンを戻す)。
    let links = rows(&daily.join(DB_FILE), LINKS);
    for (from, to) in [
        (ids.reply, ids.secret_turn),
        (ids.signal, ids.secret_turn),
        (ids.secret_turn, ids.session),
    ] {
        assert!(
            links
                .keys()
                .any(|k| k.starts_with(&id_key(from)) && k.ends_with(&id_key(to))),
            "{from} -> {to}"
        );
    }
    assert!(
        rows(&daily.join(DB_FILE), LOOKUP)
            .keys()
            .any(|k| mentions(k, ids.secret_turn) && mentions(k, ids.signal))
    );
}

fn id_key(id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.id(id);
    k.into_bytes()
}

/// 写しにターンの記録が無く、その隔離の記録だけがある退避も、飛ばさずに隔離の記録を除く。
/// verifies: 03 §7.3 (伏せ字にしたターンの quarantine の記録を消す、飛ばす退避)
#[test]
fn a_quarantined_copy_of_a_turn_is_dropped_even_without_the_turn() {
    let dir = TempDir::new("forget-recopy-quarantine-only");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    evorag_store::testutil::raw_remove_record(&daily, "turn", ids.secret_turn);
    let never = |_: &Envelope, _: &RecopyView<'_>| false;
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::new(),
        turns: BTreeMap::from([(ids.secret_turn, vec![secret_redaction().0])]),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: Vec::new(),
        drop: &never,
    };
    let n = store.forget_backups(None, &p, &|| false).unwrap();
    // 日次は写し直す。週次はターンを持つので、伏せ字にして写し直す。
    assert_eq!(n.rewritten, 2, "{n:?}");
    let records = rows(&daily.join(DB_FILE), RECORDS);
    assert!(!records.contains_key(&record_key(KIND_QUARANTINE, ids.quarantined_turn)));
    assert!(records.contains_key(&record_key(KIND_QUARANTINE, ids.other_quarantine)));
}

/// 読めない記録 (チェックサムが合わない) は、対象の ID を文字列で含むときだけ写しから除く。
/// verifies: 03 §7.3 (写し方)、03 §8 (読めない記録)
#[test]
fn unreadable_records_naming_a_target_are_dropped() {
    let dir = TempDir::new("forget-recopy-corrupt");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    evorag_store::testutil::raw_corrupt_record(&daily, "chunk", ids.stale_chunk);
    evorag_store::testutil::raw_corrupt_record(&daily, "chunk", ids.other_chunk);
    let rule = drop_rule;
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::new(),
        turns: BTreeMap::from([(ids.secret_turn, vec![secret_redaction().0])]),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: Vec::new(),
        drop: &rule,
    };
    store.forget_backups(None, &p, &|| false).unwrap();
    let records = rows(&daily.join(DB_FILE), RECORDS);
    assert!(!records.contains_key(&record_key("chunk", ids.stale_chunk)));
    assert!(records.contains_key(&record_key("chunk", ids.other_chunk)));
    // 除いた記録の blob も除く。読めないまま残した記録の blob は残す。
    let blobs = rows(&daily.join(DB_FILE), BLOBS);
    assert!(!blobs.values().any(|v| v == b"vector-of-secret"));
    assert!(blobs.values().any(|v| v == b"vector-of-other"));
}

/// 開けない退避があっても、ほかの退避は写し直してから失敗を返す (呼び手は job を残す)。
/// verifies: 03 §7.3 (1 つの退避の失敗で止めない)、04 §7.7 の 5
#[test]
fn an_unopenable_backup_fails_after_the_others_are_rewritten() {
    let dir = TempDir::new("forget-recopy-bad");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    let bad = dir.path().join("backups").join("daily").join("20260101");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join(DB_FILE), b"not a database").unwrap();
    let rule = drop_rule;
    let p = plan(&ids, secret_redaction().0, &rule);
    assert!(store.forget_backups(None, &p, &|| false).is_err());
    let records = rows(&daily.join(DB_FILE), RECORDS);
    assert!(!records.contains_key(&record_key("sample", ids.removed)));
    // 止めたら、次の退避に移る前に失敗を返す。
    assert!(store.forget_backups(None, &p, &|| true).is_err());
}

/// restore は、置き換える DB に残っている整理の job と `compact.pending` を、戻した DB に引き継ぐ。
/// verifies: 03 §10 (restore の引き継ぎ)、03 §3.1.1 (`compact.pending`)
#[test]
fn restore_carries_cleanup_jobs_and_compact_pending() {
    let dir = TempDir::new("forget-restore-carry");
    let c = clock();
    let (job, other, old_job) = {
        let store = open(dir.path(), c.clone());
        let tok = store.take_write_token().unwrap();
        let now = store.clock().now();
        let mut old_job = Envelope::new(store.ids().next_id(), "job", 1, now, "");
        old_job.body = map(vec![("task", "purge_indexes".into())]);
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut old_job).unwrap();
        tx.commit().unwrap();
        store.daily_backup(None).unwrap().unwrap();
        assert!(!store.compact_pending().unwrap());

        let mut job = Envelope::new(store.ids().next_id(), "job", 1, now, "");
        job.body = map(vec![
            ("task", "forget_backups".into()),
            ("directive", store.ids().next_id().to_string().into()),
        ]);
        let mut other = Envelope::new(store.ids().next_id(), "sample", 1, now, "");
        other.body = map(vec![("n", 2.into())]);
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut job).unwrap();
        tx.put(&mut other).unwrap();
        tx.set_compact_pending().unwrap();
        tx.commit().unwrap();
        assert!(store.compact_pending().unwrap());
        (job.id, other.id, old_job.id)
    };
    let backup = list_backups(dir.path(), None)
        .into_iter()
        .find(|b| b.dir.parent().is_some_and(|p| p.ends_with("daily")))
        .unwrap();
    let carry = |e: &Envelope| e.kind == "job";
    restore(dir.path(), &backup.dir, c.now().as_millis(), None, &carry).unwrap();

    let store = open(dir.path(), c);
    let r = store.read().unwrap();
    assert!(
        r.get("job", job).unwrap().is_some(),
        "the cleanup job is carried"
    );
    assert!(r.get("job", old_job).unwrap().is_some());
    assert!(
        r.get("sample", other).unwrap().is_none(),
        "other records are not"
    );
    drop(r);
    assert!(store.compact_pending().unwrap());
}

/// `compact.pending` は書き換えてよく、消せる。知らない値は「要らない」として読む。
/// verifies: 03 §3.1.1 (`compact.pending`)
#[test]
fn compact_pending_can_be_set_cleared_and_unknown_values_read_as_not_needed() {
    let dir = TempDir::new("compact-pending");
    let c = clock();
    {
        let store = open(dir.path(), c.clone());
        let tok = store.take_write_token().unwrap();
        for _ in 0..2 {
            let mut tx = store.write(&tok, Durability::Immediate).unwrap();
            tx.set_compact_pending().unwrap();
            tx.commit().unwrap();
            assert!(store.compact_pending().unwrap());
        }
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.clear_compact_pending().unwrap();
        tx.commit().unwrap();
        assert!(!store.compact_pending().unwrap());
    }
    evorag_store::testutil::raw_set_meta(dir.path(), meta::COMPACT_PENDING, "yes");
    let store = open(dir.path(), c);
    assert!(!store.compact_pending().unwrap());
}

/// 指示の記録が正しくない伏せ字のターンは、写しから記録だけを除く (関係と副索引は残す)。そのターンを出典とする
/// 派生の記録は、写しのターンが無いので古いものとして除く。
/// verifies: 03 §7.3 (伏せ字が合わずに写しから除くターン)
#[test]
fn a_turn_with_a_broken_directive_is_dropped_as_a_record_only() {
    let dir = TempDir::new("forget-recopy-drop-turn");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    let daily = store.daily_backup(None).unwrap().unwrap();
    let file = daily.join(DB_FILE);
    let rule = drop_rule;
    let p = ForgetRecopy {
        scan_from: None,
        remove: BTreeSet::new(),
        turns: BTreeMap::new(),
        drop_turns: BTreeSet::from([ids.secret_turn]),
        marker: M.into(),
        candidates: Vec::new(),
        drop: &rule,
    };
    assert_eq!(
        store.forget_backups(None, &p, &|| false).unwrap().rewritten,
        2
    );
    let records = rows(&file, RECORDS);
    for (kind, id) in [
        ("turn", ids.secret_turn),
        ("chunk", ids.stale_chunk),
        (KIND_QUARANTINE, ids.quarantined_turn),
    ] {
        assert!(!records.contains_key(&record_key(kind, id)), "{kind} {id}");
    }
    assert!(records.contains_key(&record_key("turn", ids.other_turn)));
    // 応答と信号からそのターンへの関係・副索引は残る (restore はジャーナルからターンを戻す)。
    let links = rows(&file, LINKS);
    assert!(
        links
            .keys()
            .any(|k| mentions(k, ids.reply) && mentions(k, ids.secret_turn))
    );
    let lookup = rows(&file, LOOKUP);
    assert!(
        lookup
            .keys()
            .any(|k| mentions(k, ids.signal) && mentions(k, ids.secret_turn))
    );
    for (_, v) in rows(&file, RECORDS) {
        assert!(!v.windows(4).any(|w| w == b"4242"));
    }
}

/// 副索引で引けない記録を除きうるとき (`scan_from`)、記録の ID の最大がその ID 以上の退避は飛ばさずに走査する。
/// verifies: 03 §7.3 (飛ばす退避)
#[test]
fn backups_newer_than_the_directive_are_scanned() {
    let dir = TempDir::new("forget-recopy-scan-from");
    let c = clock();
    let store = open(dir.path(), c.clone());
    let ids = seed(&store);
    store.daily_backup(None).unwrap().unwrap();
    let scanned = std::cell::Cell::new(0usize);
    let count = |_: &Envelope, _: &RecopyView<'_>| -> bool {
        scanned.set(scanned.get() + 1);
        false
    };
    let later = store.ids().next_id();
    let plan = |from: Id| ForgetRecopy {
        scan_from: Some(from),
        remove: BTreeSet::new(),
        turns: BTreeMap::new(),
        drop_turns: BTreeSet::new(),
        marker: M.into(),
        candidates: Vec::new(),
        drop: &count,
    };
    // 退避の記録がすべて指示より前なら飛ばす。
    store.forget_backups(None, &plan(later), &|| false).unwrap();
    assert_eq!(scanned.get(), 0);
    // 指示より後の記録を持つ退避は走査する。
    store
        .forget_backups(None, &plan(ids.session), &|| false)
        .unwrap();
    assert!(scanned.get() > 0);
}
