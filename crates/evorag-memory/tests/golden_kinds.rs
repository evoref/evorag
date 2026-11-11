//! 会話の層の種類の golden fixture (03 §4.2 の規則 4、NFR-304)。
//!
//! 見本は `tests/fixtures/<kind>/v1-key.bin` と `v1.value`。v0.1〜v0.4 は今の形式の見本だけを持つ
//! (03 §4.1)。作り直すときは `EVORAG_BLESS=1 cargo nextest run -p evorag-memory` を流す。
//!
//! verifies: AC-NFR-304-1

use std::path::PathBuf;

use evorag_memory::*;
use evorag_store::Envelope;
use evorag_store::key::KeyBuf;
use evorag_types::{Id, Timestamp};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn fixtures(kind: &str) -> PathBuf {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/fixtures").join(kind)
}

fn id(s: &str) -> Id {
    s.parse().unwrap()
}

/// 見本と同じに書け、見本から同じ本体が読めること。
fn check<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    kind: &str,
    rid: Id,
    body: T,
) {
    let mut e = Envelope::new(
        rid,
        kind,
        1,
        Timestamp::from_millis(1_762_650_000_000),
        "0.0.3",
    );
    e.set_body(&body).unwrap();
    let mut k = KeyBuf::new();
    k.str(kind).id(rid);
    let key = k.into_bytes();
    let value = e.encode(&key);
    let dir = fixtures(kind);
    if std::env::var_os("EVORAG_BLESS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("v1-key.bin"), &key).unwrap();
        std::fs::write(dir.join("v1.value"), &value).unwrap();
        return;
    }
    let stored_key =
        std::fs::read(dir.join("v1-key.bin")).expect("fixture; run with EVORAG_BLESS=1");
    let stored = std::fs::read(dir.join("v1.value")).expect("fixture; run with EVORAG_BLESS=1");
    assert_eq!(stored_key, key, "{kind}: key encoding changed");
    assert_eq!(stored, value, "{kind}: stored form changed (03 §4)");
    let back = Envelope::decode(&stored_key, &stored).unwrap();
    assert_eq!(back.body_as::<T>().unwrap(), body);
}

#[test]
fn session_v1() {
    check(
        KIND_SESSION,
        id("019a6a2b-0000-7000-8000-000000000001"),
        SessionBody {
            title: "見本のセッション".into(),
            mode: "chat".into(),
            state: SessionState::Active,
            memory: Some(false),
        },
    );
}

#[test]
fn turn_v1() {
    check(
        KIND_TURN,
        id("019a6a2b-0000-7000-8000-000000000002"),
        TurnBody {
            session: id("019a6a2b-0000-7000-8000-000000000001"),
            role: TurnRole::Assistant,
            state: TurnState::Interrupted,
            text: "途中までの応答".into(),
            model_key: Some("0123456789abcdef0123456789abcdef".into()),
            generation: Some(GenerationRecord {
                temperature: 0.7,
                top_p: 0.95,
                max_tokens: Some(512),
                seed: None,
            }),
            attached: vec![id("019a6a2b-0000-7000-8000-000000000009")],
            reply_to: Some(id("019a6a2b-0000-7000-8000-000000000003")),
            error: Some("interrupted".into()),
            recall: Some(RecallRecord {
                decision: RecallDecision::Fire,
                sources: vec![1],
            }),
            obs: Some(TurnObs {
                source_base: Some(3),
                history_from: Some(id("019a6a2b-0000-7000-8000-000000000003")),
                recall_position: Some(RECALL_BEFORE_UTTERANCE.into()),
                answer: [("language".to_owned(), "auto".to_owned())].into(),
            }),
        },
    );
}

#[test]
fn signal_v1() {
    check(
        KIND_SIGNAL,
        id("019a6a2b-0000-7000-8000-000000000004"),
        SignalBody {
            kind: SignalKind::Retry,
            target: id("019a6a2b-0000-7000-8000-000000000002"),
            reason: None,
        },
    );
}

#[test]
fn setting_v1() {
    check(
        KIND_SETTING,
        id("019a6a2b-0000-7000-8000-000000000006"),
        SettingBody {
            name: SETTING_ANSWER_LENGTH.into(),
            value: "short".into(),
        },
    );
}

#[test]
fn job_v1() {
    check(
        KIND_JOB,
        id("019a6a2b-0000-7000-8000-000000000005"),
        JobBody {
            task: JOB_PURGE_BACKUPS.into(),
            ids: vec![id("019a6a2b-0000-7000-8000-000000000001")],
        },
    );
}
