//! 書き出し (03 §11): AC-FR-209-1 (各行が封筒の形で、すべての記録が 1 回ずつ含まれる)。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use evorag_store::export::{
    EXPORT_FORMAT, ExportChecks, ExportError, ExportOptions, ExportView, NoChecks, export,
};
use evorag_store::testutil::{TempDir, raw_corrupt_record, raw_set_meta};
use evorag_store::{
    Durability, Envelope, KindSpec, OpenOptions, Read, Store, WriteOp, WriteRequest, Writer,
    envelope_from_json, meta,
};
use evorag_types::{Id, ManualClock, Timestamp, UuidV7Gen};
use serde::{Deserialize, Serialize};

const NOTE: KindSpec = KindSpec::new("note", 1);
const MARK: KindSpec = KindSpec::new("mark", 1);

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Body {
    text: String,
    n: f64,
    raw: serde_bytes_like::Bytes,
}

mod serde_bytes_like {
    /// CBOR のバイト列 (JSON にそのまま写せない値) を作るための型。
    #[derive(Debug, PartialEq)]
    pub struct Bytes(pub Vec<u8>);
    impl serde::Serialize for Bytes {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(&self.0)
        }
    }
    impl<'de> serde::Deserialize<'de> for Bytes {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl serde::de::Visitor<'_> for V {
                type Value = Bytes;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("bytes")
                }
                fn visit_bytes<E>(self, v: &[u8]) -> Result<Bytes, E> {
                    Ok(Bytes(v.to_vec()))
                }
            }
            d.deserialize_bytes(V)
        }
    }
}

fn open(root: &Path) -> Store {
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
    Store::open(OpenOptions {
        data_root: root.to_path_buf(),
        evorag_version: "0.0.6".into(),
        kinds: vec![MARK, NOTE],
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::with_seed(clock, 1)),
        config_file: None,
    })
    .unwrap()
    .0
}

/// `NoChecks` で `exclude` を除いて書き出す。
fn export_plain(
    root: &Path,
    out: &Path,
    now_ms: i64,
    exclude: &[&str],
) -> Result<evorag_store::export::ExportReport, ExportError> {
    export(
        root,
        out,
        &ExportOptions {
            evorag_version: "0.0.6",
            now_ms,
            exclude,
            checks: &NoChecks,
        },
    )
}

fn out_dir(root: &Path) -> std::path::PathBuf {
    let out = root.join("..").join(format!(
        "{}-out",
        root.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&out);
    out
}

fn write_records(store: &Store) -> Vec<Envelope> {
    let tok = store.take_write_token().unwrap();
    let mut out = Vec::new();
    for (i, kind) in [NOTE, NOTE, MARK, NOTE].iter().enumerate() {
        let mut e = Envelope::new(store.ids().next_id(), kind.name, 1, store.clock().now(), "");
        e.set_body(&Body {
            text: format!("記録 {i}"),
            n: 0.1 + i as f64,
            raw: serde_bytes_like::Bytes(vec![0, 255, i as u8]),
        })
        .unwrap();
        let mut tx = store.write(&tok, Durability::Immediate).unwrap();
        tx.put(&mut e).unwrap();
        tx.commit().unwrap();
        out.push(e);
    }
    out
}

/// verifies: AC-FR-209-1, AC-NFR-503-1 (書き出し)
#[test]
fn every_record_is_written_once_as_an_envelope() {
    let dir = TempDir::new("export");
    let store = open(dir.path());
    let written = write_records(&store);
    let broken: Id = written[3].id;
    drop(store);
    // 壊れた記録は飛ばして数える (quarantine と同じく書き出さない)。
    raw_corrupt_record(dir.path(), "note", broken);

    let out = dir.path().join("..").join(format!(
        "{}-out",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&out);
    let report = export_plain(dir.path(), &out, 1, &[]).unwrap();
    assert_eq!(report.kinds.get("note"), Some(&2));
    assert_eq!(report.kinds.get("mark"), Some(&1));
    assert_eq!(report.skipped_corrupt, 1);
    assert_eq!(report.files.len(), 2);

    let mut seen = BTreeSet::new();
    for kind in ["note", "mark"] {
        let text = std::fs::read_to_string(out.join(format!("{kind}.jsonl"))).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["format"], EXPORT_FORMAT);
        assert_eq!(header["version"], 1);
        assert_eq!(header["kind"], kind);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["kind"], kind);
            let back = envelope_from_json(&v).unwrap();
            let orig = written.iter().find(|e| e.id == back.id).unwrap();
            assert_eq!(
                back.body, orig.body,
                "the body round-trips (including bytes)"
            );
            assert!(seen.insert(back.id), "each record once");
        }
    }
    assert_eq!(seen.len(), 3);
    assert!(!seen.contains(&broken));

    // 書き出しは所有者だけ (AC-NFR-503-1)。
    let private = |p: &std::path::Path| {
        !evorag_store::fsutil::acl_applicable(p).unwrap()
            || evorag_store::fsutil::is_private(p).unwrap()
    };
    assert!(private(&out), "the export folder is owner only");
    assert!(private(&out.join("note.jsonl")), "owner only");

    // 中身のあるフォルダには書かない。
    assert!(matches!(
        export_plain(dir.path(), &out, 2, &[]),
        Err(ExportError::NotEmpty(_))
    ));
    let _ = std::fs::remove_dir_all(&out);

    // 使っているプロセスがあれば断る。
    let store = open(dir.path());
    assert!(matches!(
        export_plain(dir.path(), &out, 3, &[]),
        Err(ExportError::Open(_))
    ));
    drop(store);
}

/// verifies: AC-FR-209-1 (作り直せる種類は書き出さない、03 §11)
#[test]
fn excluded_kinds_are_not_written() {
    let dir = TempDir::new("export-exclude");
    let store = open(dir.path());
    write_records(&store);
    drop(store);
    let out = dir.path().join("..").join(format!(
        "{}-out",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&out);
    let report = export_plain(dir.path(), &out, 1, &["mark"]).unwrap();
    assert_eq!(report.kinds.get("note"), Some(&3));
    assert_eq!(report.kinds.get("mark"), None);
    assert!(!out.join("mark.jsonl").exists());
}

const TURN: KindSpec = KindSpec {
    journaled: true,
    ..KindSpec::new("turn", 1)
};

/// 取り込んでいないジャーナルの行があれば書き出しを断り、出力先に何も作らない。一度開いて取り込めば書き出せる。
/// verifies: 03 §11 (取り込んでいないジャーナルの行があれば断る)
#[test]
fn export_refuses_while_journal_lines_are_not_imported() {
    let dir = TempDir::new("export-pending-journal");
    let open_turns = || {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_762_650_000_000)));
        Store::open(OpenOptions {
            data_root: dir.path().to_path_buf(),
            evorag_version: "0.0.6".into(),
            kinds: vec![TURN],
            clock: clock.clone(),
            ids: Arc::new(UuidV7Gen::with_seed(clock, 3)),
            config_file: None,
        })
        .unwrap()
        .0
    };
    let store = Arc::new(open_turns());
    let (writer, h) = Writer::new(store.clone()).unwrap();
    let t = std::thread::spawn(move || writer.run());
    for i in 0..2 {
        let mut e = Envelope::new(store.ids().next_id(), "turn", 1, store.clock().now(), "");
        e.set_body(&Body {
            text: format!("ターン {i}"),
            n: 0.0,
            raw: serde_bytes_like::Bytes(vec![]),
        })
        .unwrap();
        h.submit_wait(WriteRequest::conversation(vec![WriteOp::Put(e)]))
            .unwrap();
    }
    h.shutdown();
    t.join().unwrap();
    drop(store);
    // DB がジャーナルの行を取り込む前に止まった状態 (取り込んだ連番を戻す)。
    raw_set_meta(dir.path(), meta::JOURNAL_LAST_SEQ, "0");

    let out = out_dir(dir.path());
    assert!(matches!(
        export_plain(dir.path(), &out, 1, &[]),
        Err(ExportError::PendingJournal { lines: 2 })
    ));
    assert!(!out.exists(), "nothing is created when refused");

    // evorag を一度起動すると取り込まれ、書き出せる。
    drop(open_turns());
    let report = export_plain(dir.path(), &out, 2, &[]).unwrap();
    assert_eq!(report.kinds.get("turn"), Some(&2));
}

/// メモリの層の確かめ: 当て直しの済んでいない忘れるがあれば断り、外すと言われた記録は書かずに数える。
/// verifies: 03 §11 (当て直しの済んでいない忘れるがあれば断る、派生の記録の確かめ)
#[test]
fn export_follows_the_checks_of_the_upper_layers() {
    struct Pending;
    impl ExportChecks for Pending {
        fn pending_forget(&self, view: &ExportView<'_>) -> bool {
            let mut n = 0;
            view.for_each("note", |_| {
                n += 1;
                true
            })
            .unwrap();
            n > 0
        }
    }
    struct DropMarks;
    impl ExportChecks for DropMarks {
        fn include(&self, view: &ExportView<'_>, env: &Envelope) -> bool {
            assert!(matches!(
                view.get(&env.kind, env.id).unwrap(),
                Some(Read::Ok(_))
            ));
            env.kind != "mark"
        }
    }
    let dir = TempDir::new("export-checks");
    let store = open(dir.path());
    write_records(&store);
    drop(store);
    let out = out_dir(dir.path());
    let opts = |checks: &'static dyn ExportChecks| ExportOptions {
        evorag_version: "0.0.6",
        now_ms: 1,
        exclude: &[],
        checks,
    };
    assert!(matches!(
        export(dir.path(), &out, &opts(&Pending)),
        Err(ExportError::PendingForget)
    ));
    assert!(!out.exists(), "nothing is created when refused");
    let report = export(dir.path(), &out, &opts(&DropMarks)).unwrap();
    assert_eq!(report.kinds.get("note"), Some(&3));
    assert_eq!(report.kinds.get("mark"), None);
    assert_eq!(report.skipped_by_check, 1);
}
