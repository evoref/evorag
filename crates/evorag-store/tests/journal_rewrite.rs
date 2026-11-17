//! 削除と忘れるのジャーナルの書き直しと、単調に増える項目の取り込みの試験 (03 §6、04 §7.2、09 §3.5)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use evorag_store::testutil::{self, TempDir};
use evorag_store::{
    BodyPatch, Envelope, JOURNAL_DIR, JournalCut, KindSpec, OpenOptions, Read, RevSpan,
    RewriteFault, Store, WriteOp, WriteRequest, Writer, WriterHandle, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::Serialize;

/// ターンの代わり: ジャーナルに書き、改訂の番号 `rev` を単調に増える項目として宣言する。
const TURNISH: KindSpec = KindSpec {
    journaled: true,
    monotonic: Some("rev"),
    ..KindSpec::new("turnish", 1)
};

/// 事実への指示の代わり: ジャーナルに書く。
const DIRECTIVE: KindSpec = KindSpec {
    journaled: true,
    ..KindSpec::new("directive", 1)
};

#[derive(Serialize)]
struct Turn {
    text: String,
    rev: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
}

#[derive(Serialize)]
struct Directive {
    target: String,
    rev_after: u64,
}

fn open(root: &Path) -> Arc<Store> {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    let (store, _) = Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.2".into(),
        kinds: vec![TURNISH, DIRECTIVE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 7)),
        config_file: None,
    })
    .unwrap();
    Arc::new(store)
}

fn start(store: &Arc<Store>) -> (WriterHandle, std::thread::JoinHandle<()>) {
    let (writer, handle) = Writer::new(store.clone()).unwrap();
    (handle, std::thread::spawn(move || writer.run()))
}

fn stop(h: &WriterHandle, t: std::thread::JoinHandle<()>) {
    h.shutdown();
    t.join().unwrap();
}

fn turn(store: &Store, id: Id, text: &str, rev: u64, reply_to: Option<Id>) -> Envelope {
    let mut e = Envelope::new(id, "turnish", 1, store.clock().now(), "");
    e.set_body(&Turn {
        text: text.into(),
        rev,
        reply_to: reply_to.map(|r| r.to_string()),
    })
    .unwrap();
    e
}

fn directive(store: &Store, target: Id, rev_after: u64) -> Envelope {
    let mut e = Envelope::new(
        store.ids().next_id(),
        "directive",
        1,
        store.clock().now(),
        "",
    );
    e.set_body(&Directive {
        target: target.to_string(),
        rev_after,
    })
    .unwrap();
    e
}

/// 伏せ字にする `Patch` (本文を差し替えて改訂の番号を上げる)。
fn redact(id: Id, text: &str, rev: u64) -> WriteOp {
    WriteOp::Patch {
        kind: "turnish".into(),
        id,
        fields: BodyPatch::new()
            .set("text", &text)
            .unwrap()
            .set("rev", &rev)
            .unwrap(),
        updated_at: None,
    }
}

fn below(id: Id, rev: u64) -> WriteOp {
    WriteOp::RewriteJournal([(id, JournalCut::Below(rev))].into_iter().collect())
}

fn journal_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root.join(JOURNAL_DIR))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// すべてのジャーナルのファイルの行 (書いた順)。
fn journal_lines(root: &Path) -> Vec<String> {
    journal_files(root)
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn seq_of(line: &str) -> u64 {
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["seq"].as_u64().unwrap()
}

fn text_of(store: &Store, id: Id) -> (String, u64) {
    let Some(Read::Ok(e)) = store.read().unwrap().get("turnish", id).unwrap() else {
        panic!("turn {id} is missing");
    };
    let body: serde_json::Value = serde_json::to_value(e.body.clone()).unwrap();
    (
        body["text"].as_str().unwrap().to_owned(),
        e.body_uint("rev").unwrap(),
    )
}

/// 忘れるの依頼は、1 回の追記で、指示の行を先に、伏せ字にしたターンの行をその後の連番で書き、コミットの後に
/// すぐ、伏せ字の前の行をジャーナルから除く。ほかのターンの行 (`reply_to` に ID を持つ) と指示の行は残る。
/// verifies: 03 §6 (書く物・削除と忘れる)、04 §7.2 (ジャーナル)
#[test]
fn a_forget_request_writes_the_directive_first_and_rewrites_the_journal_at_once() {
    let dir = TempDir::new("rewrite-forget");
    let store = open(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    let other = store.ids().next_id();
    h.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(turn(&store, id, "伏せる前の本文", 0, None)),
        WriteOp::Put(turn(&store, other, "別のターン", 0, Some(id))),
    ]))
    .unwrap();
    assert_eq!(store.journal_rev_span(id), Some(RevSpan { min: 0, max: 0 }));

    let out = h
        .submit_wait(WriteRequest::conversation(vec![
            WriteOp::Put(directive(&store, id, 1)),
            redact(id, "[伏せ字]", 1),
            below(id, 1),
        ]))
        .unwrap();
    let report = out.journal_rewrite.expect("rewritten").expect("succeeded");
    assert_eq!((report.files, report.removed), (1, 1));
    assert_eq!(text_of(&store, id), ("[伏せ字]".to_owned(), 1));
    assert_eq!(store.journal_rev_span(id), Some(RevSpan { min: 1, max: 1 }));

    let lines = journal_lines(dir.path());
    assert!(!lines.iter().any(|l| l.contains("伏せる前の本文")));
    assert!(lines.iter().any(|l| l.contains("別のターン")));
    let at = lines
        .iter()
        .position(|l| l.contains("\"directive\""))
        .expect("the directive line");
    assert!(
        lines[at + 1].contains("[伏せ字]"),
        "the redacted turn follows the directive"
    );
    assert_eq!(seq_of(&lines[at]) + 1, seq_of(&lines[at + 1]));
    stop(&h, t);
}

/// 削除の依頼は、コミットの後にすぐ、その ID の `put` の行をジャーナルから除き、結果を返す。
/// verifies: 03 §6 (削除と忘れる)
#[test]
fn a_delete_rewrites_the_journal_and_reports_it() {
    let dir = TempDir::new("rewrite-delete");
    let store = open(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    let other = store.ids().next_id();
    h.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(turn(&store, id, "消す本文", 0, None)),
        WriteOp::Put(turn(&store, other, "残る返事", 0, Some(id))),
    ]))
    .unwrap();
    let out = h
        .submit_wait(WriteRequest::conversation(vec![WriteOp::Delete {
            kind: "turnish".into(),
            id,
        }]))
        .unwrap();
    let report = out.journal_rewrite.expect("rewritten").expect("succeeded");
    assert_eq!(report.removed, 1);
    let lines = journal_lines(dir.path());
    assert!(!lines.iter().any(|l| l.contains("消す本文")));
    assert!(lines.iter().any(|l| l.contains("残る返事")));
    assert!(lines.iter().any(|l| l.contains("\"delete\"")));
    assert_eq!(store.journal_rev_span(id), None);
    stop(&h, t);
}

/// 入れ替えに失敗しても依頼は通り (コミットは済んでいる)、試し直せる失敗を返す。書き直しだけの依頼で
/// 試し直すと済み、DB には何も書かない。
/// verifies: 03 §6 (失敗・追記との直列)、04 §7.2 (sleep-time に試し直す)
#[test]
fn a_failed_swap_is_reported_and_a_rewrite_only_request_finishes_it() {
    let dir = TempDir::new("rewrite-retry");
    let store = open(dir.path());
    let (h, t) = start(&store);
    let id = store.ids().next_id();
    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(turn(
        &store,
        id,
        "伏せる前の本文",
        0,
        None,
    ))]))
    .unwrap();
    store.inject_journal_rewrite_fault(RewriteFault::FailSwap { nth: 0 });
    let out = h
        .submit_wait(WriteRequest::conversation(vec![
            WriteOp::Put(directive(&store, id, 1)),
            redact(id, "[伏せ字]", 1),
            below(id, 1),
        ]))
        .unwrap();
    let err = out.journal_rewrite.expect("tried").unwrap_err();
    assert!(err.is_swap(), "{err:?}");
    assert_eq!(text_of(&store, id), ("[伏せ字]".to_owned(), 1));
    assert!(
        journal_lines(dir.path())
            .iter()
            .any(|l| l.contains("伏せる前の本文"))
    );
    assert_eq!(store.journal_rev_span(id), Some(RevSpan { min: 0, max: 1 }));

    let seq_before = store.meta(meta::JOURNAL_LAST_SEQ).unwrap();
    let out = h
        .submit_wait(WriteRequest::background(vec![below(id, 1)]))
        .unwrap();
    assert_eq!(out.journal_rewrite.unwrap().unwrap().removed, 1);
    assert_eq!(store.meta(meta::JOURNAL_LAST_SEQ).unwrap(), seq_before);
    assert!(
        !journal_lines(dir.path())
            .iter()
            .any(|l| l.contains("伏せる前の本文"))
    );
    stop(&h, t);
}

/// 書き直しの依頼と会話の追記が入り混じっても、追記した行は失われない (書き直しは書き手の依頼として、追記と
/// 直列に行う)。
/// verifies: 03 §6 (追記との直列)
#[test]
fn appends_interleaved_with_rewrites_are_not_lost() {
    let dir = TempDir::new("rewrite-serial");
    let store = open(dir.path());
    let (h, t) = start(&store);
    let target = store.ids().next_id();
    h.submit_wait(WriteRequest::conversation(vec![
        WriteOp::Put(turn(&store, target, "伏せる前の本文", 0, None)),
        WriteOp::Put(turn(&store, target, "[伏せ字]", 1, None)),
    ]))
    .unwrap();
    let ids: Vec<Id> = (0..60).map(|_| store.ids().next_id()).collect();
    std::thread::scope(|s| {
        for chunk in ids.chunks(20) {
            let h = h.clone();
            let store = &store;
            s.spawn(move || {
                for &id in chunk {
                    h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(turn(
                        store, id, "追記", 0, None,
                    ))]))
                    .unwrap();
                }
            });
        }
        let h = h.clone();
        s.spawn(move || {
            for _ in 0..20 {
                let cuts: BTreeMap<Id, JournalCut> =
                    [(target, JournalCut::Below(1))].into_iter().collect();
                h.submit_wait(WriteRequest::background(vec![WriteOp::RewriteJournal(
                    cuts,
                )]))
                .unwrap();
            }
        });
    });
    stop(&h, t);
    let lines = journal_lines(dir.path());
    for id in &ids {
        let needle = format!("\"id\":\"{id}\"");
        assert_eq!(
            lines.iter().filter(|l| l.contains(&needle)).count(),
            1,
            "the line of {id} must survive"
        );
    }
    assert!(!lines.iter().any(|l| l.contains("伏せる前の本文")));
}

/// 取り込みは、DB の記録の改訂の番号を小さくする `put` の行を当てない (伏せ字の前の行で、伏せ字を戻さない)。
/// verifies: 03 §6 (突き合わせ)
#[test]
fn an_imported_line_never_lowers_the_revision() {
    let dir = TempDir::new("rewrite-monotonic");
    let id;
    {
        let store = open(dir.path());
        let (h, t) = start(&store);
        id = store.ids().next_id();
        h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(turn(
            &store,
            id,
            "伏せる前の本文",
            0,
            None,
        ))]))
        .unwrap();
        h.submit_wait(WriteRequest::conversation(vec![redact(id, "[伏せ字]", 1)]))
            .unwrap();
        stop(&h, t);
    }
    // DB は伏せ字の後 (rev 1) のまま、ジャーナルには伏せ字の前の行だけが取り込み待ちで残る状態を作る
    // (古い版で開いた間に連番だけが戻ったなど)。
    for p in journal_files(dir.path()) {
        let text = std::fs::read_to_string(&p).unwrap();
        let kept: String = text
            .split_inclusive('\n')
            .filter(|l| !l.contains("[伏せ字]"))
            .collect();
        std::fs::write(&p, kept).unwrap();
    }
    testutil::raw_set_meta(dir.path(), meta::JOURNAL_LAST_SEQ, "0");
    let store = open(dir.path());
    assert_eq!(text_of(&store, id), ("[伏せ字]".to_owned(), 1));
    assert_eq!(store.journal_rev_span(id), Some(RevSpan { min: 0, max: 0 }));
}

/// 開くときに、残っている `*.jsonl.tmp` を消し、ID ごとの改訂の番号の範囲を集める。
/// verifies: 03 §6 (書き直し方)、04 §7.2 (ジャーナルの層の見つけ方)
#[test]
fn opening_removes_leftover_temporary_files_and_collects_revisions() {
    let dir = TempDir::new("rewrite-open");
    let id;
    {
        let store = open(dir.path());
        let (h, t) = start(&store);
        id = store.ids().next_id();
        store.inject_journal_rewrite_fault(RewriteFault::StopAfterTemp);
        h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(turn(
            &store,
            id,
            "伏せる前の本文",
            0,
            None,
        ))]))
        .unwrap();
        let out = h
            .submit_wait(WriteRequest::conversation(vec![
                WriteOp::Put(directive(&store, id, 1)),
                redact(id, "[伏せ字]", 1),
                below(id, 1),
            ]))
            .unwrap();
        assert!(out.journal_rewrite.unwrap().is_err());
        stop(&h, t);
    }
    assert!(
        journal_files(dir.path())
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".jsonl.tmp"))
    );
    let store = open(dir.path());
    assert!(
        !journal_files(dir.path())
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".jsonl.tmp"))
    );
    // 書き直しが済んでいないことが、集めた範囲から分かる (伏せ字の後の番号 1 に対して min 0)。
    assert_eq!(store.journal_rev_span(id), Some(RevSpan { min: 0, max: 1 }));
}
