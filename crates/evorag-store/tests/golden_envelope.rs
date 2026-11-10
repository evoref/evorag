//! 封筒の golden fixture (03 §4.2 の規則 4、NFR-304)。
//!
//! 見本は `tests/fixtures/envelope/` に置く。v0.1〜v0.4 は今の形式の見本だけを持ち、形式を変えたら
//! 作り直してよい (03 §4.1)。作り直すときは `EVORAG_BLESS=1 cargo nextest run -p evorag-store` を流す。
//!
//! verifies: AC-NFR-304-1 (封筒の層), AC-FR-215-2 (封筒と本体の知らない項目が書き戻しで残る)

use std::path::PathBuf;

use ciborium::Value;
use evorag_store::Envelope;
use evorag_store::key::KeyBuf;
use evorag_types::{Id, Timestamp};
use serde::{Deserialize, Serialize};

fn fixtures() -> PathBuf {
    // 実行時の値を先に見る (no-network の CI は、試験を別の場所に写した作業ツリーで走らせる)。
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures/envelope")
}

fn bless() -> bool {
    std::env::var_os("EVORAG_BLESS").is_some_and(|v| v == "1")
}

/// 見本と同じなら通し、`EVORAG_BLESS=1` なら書き直す。
fn check_fixture(name: &str, actual: &[u8]) {
    let path = fixtures().join(name);
    if bless() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing fixture {} ({e}); run with EVORAG_BLESS=1",
            path.display()
        )
    });
    assert_eq!(
        expected, actual,
        "{name} changed: the stored format differs from the fixture (03 §4)"
    );
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct SampleBody {
    text: String,
    note: Option<String>,
    count: u32,
}

fn key(id: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str("sample").id(id);
    k.into_bytes()
}

fn sample() -> Envelope {
    let id: Id = "019a6a2b-3c4d-7e5f-8a6b-7c8d9e0f1a2b".parse().unwrap();
    let mut e = Envelope::new(
        id,
        "sample",
        1,
        Timestamp::from_millis(1_762_650_000_000),
        "0.0.1",
    );
    e.updated_at = Timestamp::from_millis(1_762_650_001_234);
    e.set_body(&SampleBody {
        text: "日本語の本文\0と制御文字".into(),
        note: None,
        count: 3,
    })
    .unwrap();
    e
}

/// 新しい版が書いたつもりの記録 (封筒と本体に、この版の知らない項目がある)。
fn written_by_newer() -> Envelope {
    let mut e = sample();
    if let Value::Map(m) = &mut e.body {
        m.push((
            "future_body_field".into(),
            Value::Array(vec![1.into(), "x".into()]),
        ));
    }
    e.unknown
        .push(("future_envelope_field".into(), Value::Bytes(vec![0, 1, 2])));
    e
}

#[test]
fn current_envelope_matches_its_fixture() {
    let e = sample();
    let k = key(e.id);
    check_fixture("v1-key.bin", &k);
    check_fixture("v1.value", &e.encode(&k));
    if bless() {
        return;
    }
    let stored = std::fs::read(fixtures().join("v1.value")).unwrap();
    let back = Envelope::decode(&k, &stored).unwrap();
    assert_eq!(back, e);
    assert_eq!(
        back.body_as::<SampleBody>().unwrap(),
        SampleBody {
            text: "日本語の本文\0と制御文字".into(),
            note: None,
            count: 3
        }
    );
}

#[test]
fn unknown_fields_in_the_fixture_survive_a_write_back() {
    let e = written_by_newer();
    let k = key(e.id);
    check_fixture("v1-unknown-fields.value", &e.encode(&k));
    if bless() {
        return;
    }
    let stored = std::fs::read(fixtures().join("v1-unknown-fields.value")).unwrap();
    let mut read = Envelope::decode(&k, &stored).unwrap();
    let mut body: SampleBody = read.body_as().unwrap();
    body.count += 1;
    read.set_body(&body).unwrap();
    let back = Envelope::decode(&k, &read.encode(&k)).unwrap();

    assert_eq!(back.unknown, e.unknown);
    let Value::Map(m) = &back.body else {
        panic!("body is not a map")
    };
    assert!(
        m.iter()
            .any(|(k, _)| k.as_text() == Some("future_body_field"))
    );
    assert_eq!(back.body_as::<SampleBody>().unwrap().count, 4);
}
