//! v0.5 の根の種類の golden fixture と、本体の読み方 (03 §4.2 の規則 4・7・11、04 §5.2・§5.2.1・§7.5)。
//!
//! 見本は `tests/fixtures/<kind>/v1-key.bin` と `v1.value` (今の形式。v0.8 の凍結までは形式を変えたら作り直してよい、
//! 03 §4.1)。台帳 (`proposal`) は v0.5.0 から「項目を足すだけ」なので、v0.5.0 の見本 `v0.5.0-key.bin` と
//! `v0.5.0.value` を残し、作り直さない (後の版でも読めることを試験する、03 §4.2 の規則 4)。
//!
//! 作り直すときは `EVORAG_BLESS=1 cargo nextest run -p evorag-memory --test golden_kinds_v05` を流す (v0.5.0 の見本は、
//! 無いときだけ作る)。

use std::path::PathBuf;

use evorag_memory::*;
use evorag_store::key::KeyBuf;
use evorag_store::{Envelope, envelope_to_json};
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

fn bless() -> bool {
    std::env::var_os("EVORAG_BLESS").is_some_and(|v| v == "1")
}

fn key_of(kind: &str, rid: Id) -> Vec<u8> {
    let mut k = KeyBuf::new();
    k.str(kind).id(rid);
    k.into_bytes()
}

fn envelope<T: Serialize>(kind: &str, rid: Id, body: &T) -> Envelope {
    let mut e = Envelope::new(
        rid,
        kind,
        1,
        Timestamp::from_millis(1_764_115_200_000),
        "0.5.0",
    );
    e.set_body(body).unwrap();
    e
}

/// 見本 `<name>-key.bin`・`<name>.value` と同じに書け、見本から同じ本体が読めること。`frozen` の見本は作り直さない。
fn check<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    kind: &str,
    name: &str,
    rid: Id,
    body: &T,
    frozen: bool,
) {
    let e = envelope(kind, rid, body);
    let key = key_of(kind, rid);
    let value = e.encode(&key);
    let dir = fixtures(kind);
    let (key_file, value_file) = (
        dir.join(format!("{name}-key.bin")),
        dir.join(format!("{name}.value")),
    );
    if bless() && !(frozen && value_file.exists()) {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&key_file, &key).unwrap();
        std::fs::write(&value_file, &value).unwrap();
        return;
    }
    let stored_key = std::fs::read(&key_file).expect("fixture; run with EVORAG_BLESS=1");
    let stored = std::fs::read(&value_file).expect("fixture; run with EVORAG_BLESS=1");
    assert!(stored_key == key, "{kind}/{name}: key encoding changed");
    let back = Envelope::decode(&stored_key, &stored).unwrap();
    assert_eq!(back.body_as::<T>().unwrap(), *body, "{kind}/{name}");
    if !frozen {
        assert!(
            stored == value,
            "{kind}/{name}: stored form changed (03 §4)"
        );
    }
}

fn t(n: u8) -> Id {
    id(&format!("019ac1a0-0000-7000-8000-0000000000{n:02x}"))
}

/// 忘れるの指示 (どの項目も埋める)。
fn forget_directive() -> DirectiveBody {
    DirectiveBody {
        entity: Some("self".into()),
        aliases: vec!["わたし".into()],
        aspect: Some("residence".into()),
        vks: vec!["ab".repeat(32), "cd".repeat(32)],
        at: Some(TurnSpan {
            turn: t(2),
            rev: 0,
            start: 6,
            end: 15,
        }),
        unit: Some(t(30)),
        turns: vec![t(2)],
        redactions: vec![DirectiveRedaction {
            turn: t(2),
            rev_before: 0,
            rev_after: 1,
            start: 6,
            end: 15,
            hash: "ef".repeat(32),
        }],
        hidden: vec![TurnSpan {
            turn: t(4),
            rev: 0,
            start: 0,
            end: 9,
        }],
        history: true,
        redact: true,
        ..DirectiveBody::new(DirectiveOp::Forget)
    }
}

/// 台帳 (v0.5.0 の見本と同じ中身)。
fn sample_ledger() -> LedgerBody {
    let mut output = LedgerOutput::default();
    output
        .set_section(LedgerOutput::KEYS, &vec!["住まい", "引っ越し"])
        .unwrap();
    output
        .set_section(
            LedgerOutput::UNITS,
            &vec![
                LedgerUnitOut {
                    sentence: 1,
                    layer: LedgerLayer::World,
                    entity: "self".into(),
                    aspect: "residence".into(),
                    value: "見本の町".into(),
                    mood: LedgerMood::Fact,
                    tense: LedgerTense::Current,
                    old: "見本の村".into(),
                    when: "先月".into(),
                },
                LedgerUnitOut {
                    sentence: 2,
                    layer: LedgerLayer::Episode,
                    entity: "妻".into(),
                    aspect: "旅行".into(),
                    value: "見本の島".into(),
                    mood: LedgerMood::Fact,
                    tense: LedgerTense::Planned,
                    old: String::new(),
                    when: "来月".into(),
                },
            ],
        )
        .unwrap();
    output
        .set_section(
            LedgerOutput::CORRECTIONS,
            &vec![LedgerCorrectionOut {
                id: 1,
                verdict: LedgerVerdict::Accept,
            }],
        )
        .unwrap();
    LedgerBody {
        call: CALL_SEGMENT.into(),
        segment: Some(segment_id(t(2))),
        schema: LEDGER_SCHEMA,
        variant: "b2-ja-1".into(),
        model_key: format!("{:032x}", 0x0123_4567_89ab_cdef_u128),
        sentences: vec![
            SentenceSpan {
                s: 1,
                turn: t(2),
                rev: 0,
                start: 0,
                end: 30,
                extra: Default::default(),
            },
            SentenceSpan {
                s: 2,
                turn: t(4),
                rev: 1,
                start: 0,
                end: 27,
                extra: Default::default(),
            },
        ],
        inputs: LedgerInputs {
            prefilter: 1,
            splitter: 1,
            correction: 1,
            forgets_seen: Some(t(20)),
            input_hash: Some("12".repeat(32)),
            role_fallback: false,
            role_model_key: Some(format!("{:032x}", 0x0fed_cba9_8765_4321_u128)),
            through: Some(Through {
                turn: t(4),
                pairs: 2,
            }),
            corr: vec![CorrInput {
                id: 1,
                turn: t(4),
                rev: 1,
                pattern: "{old}じゃなくて{new}".into(),
                new: ByteRange { start: 18, end: 27 },
                old: Some(ByteRange { start: 0, end: 9 }),
                extra: Default::default(),
            }],
            refs: vec![LedgerRef::new(t(3), 0)],
            extra: Default::default(),
        },
        output,
        status: "complete".into(),
        attempts: 1,
        tokens: LedgerTokens {
            input: 812,
            output: 96,
            ms: 4_250,
        },
        scrubbed: 0,
        extra: Default::default(),
    }
}

/// verifies: AC-NFR-304-1 (指示の今の形式の見本)
#[test]
fn directive_v1() {
    check(KIND_DIRECTIVE, "v1", t(21), &forget_directive(), false);
}

/// verifies: AC-NFR-304-1 (台帳の今の形式の見本)
#[test]
fn proposal_v1() {
    check(KIND_PROPOSAL, "v1", t(40), &sample_ledger(), false);
}

/// 台帳の v0.5.0 の見本を、今の読み手で読める (作り直さない見本。中身の期待はここに固定する)。
/// verifies: AC-NFR-304-3 (台帳の v0.5.0 の見本を読める)
#[test]
fn proposal_v0_5_0_sample_stays_readable() {
    check(KIND_PROPOSAL, "v0.5.0", t(40), &sample_ledger(), true);
    if bless() {
        return;
    }
    let dir = fixtures(KIND_PROPOSAL);
    let back = Envelope::decode(
        &std::fs::read(dir.join("v0.5.0-key.bin")).unwrap(),
        &std::fs::read(dir.join("v0.5.0.value")).unwrap(),
    )
    .unwrap();
    assert_eq!(back.id, t(40));
    let l: LedgerBody = back.body_as().unwrap();
    assert!(l.is_segment_call());
    assert_eq!(l.status(), LedgerStatus::Complete);
    assert_eq!(l.segment, Some(segment_id(t(2))));
    let keys = l.output.keys();
    assert_eq!(
        (keys.items.as_slice(), keys.mismatched),
        (&["住まい".to_owned(), "引っ越し".to_owned()][..], 0)
    );
    let units = l.output.units();
    assert_eq!(units.mismatched, 0);
    assert_eq!(
        units.items[0],
        LedgerUnit {
            sentence: 1,
            layer: LedgerLayer::World,
            entity: "self".into(),
            aspect: "residence".into(),
            value: "見本の町".into(),
            mood: LedgerMood::Fact,
            tense: LedgerTense::Current,
            old: "見本の村".into(),
            when: "先月".into(),
        }
    );
    assert_eq!(
        (
            units.items[1].layer,
            units.items[1].tense,
            units.items[1].entity.as_str()
        ),
        (LedgerLayer::Episode, LedgerTense::Planned, "妻")
    );
    assert_eq!(
        l.output.corrections().items,
        [LedgerCorrection {
            id: 1,
            verdict: LedgerVerdict::Accept
        }]
    );
    assert!(l.output.invalid().is_empty());
    assert_eq!(l.inputs.corr[0].old, Some(ByteRange { start: 0, end: 9 }));
    assert_eq!(
        l.sources().into_iter().collect::<Vec<_>>(),
        [
            SrcRev { turn: t(2), rev: 0 },
            SrcRev { turn: t(3), rev: 0 },
            SrcRev { turn: t(4), rev: 1 },
        ]
    );
}

/// 本体の map に項目を足す (新しい版が書いた形をまねる)。
fn add_field(e: &mut Envelope, name: &str, value: &serde_json::Value) {
    let ciborium_value = evorag_store::RawValue::from_serialize(value).unwrap();
    let mut patch = evorag_store::BodyPatch::new();
    patch = patch.set(name, &ciborium_value).unwrap();
    e.apply_patch(&patch).unwrap();
}

/// 知らない項目 (最上位・`output` の知らない節・節の要素の知らない項目) を足した台帳を読んで書き戻すと、知らない
/// 項目が残る。
/// verifies: AC-NFR-304-3 (知らない項目を足した台帳を読んで書き戻すと、知らない項目が残る), 03 §4.2 の規則 7
#[test]
fn unknown_fields_of_a_ledger_survive_a_rewrite() {
    let mut ledger = sample_ledger();
    let mut units = serde_json::to_value(
        ledger
            .output
            .units()
            .items
            .iter()
            .map(|u| {
                serde_json::json!({
                    "s": u.sentence, "l": "world", "e": u.entity, "a": u.aspect, "v": u.value,
                    "m": "fact", "tn": "current", "old": u.old, "when": u.when,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    units[0]["s2"] = serde_json::json!(3);
    ledger
        .output
        .set_section(LedgerOutput::UNITS, &units)
        .unwrap();
    ledger
        .output
        .set_section("topics", &serde_json::json!([{"title": "住まい"}]))
        .unwrap();
    let mut e = envelope(KIND_PROPOSAL, t(41), &ledger);
    add_field(&mut e, "future", &serde_json::json!({"weight": 2}));
    let key = key_of(KIND_PROPOSAL, t(41));
    let mut back = Envelope::decode(&key, &e.encode(&key)).unwrap();

    // 今の版が読み (知らない項目は型に無い)、掃除の数を変えて書き戻す。
    let mut read: LedgerBody = back.body_as().unwrap();
    assert_eq!(read.output.units().items.len(), 2);
    assert!(read.output.has("topics"));
    read.scrubbed = 1;
    back.set_body(&read).unwrap();
    let json = envelope_to_json(&back);
    let body = &json["body"];
    assert_eq!(body["future"], serde_json::json!({"weight": 2}));
    assert_eq!(body["scrubbed"], serde_json::json!(1));
    assert_eq!(body["output"]["topics"][0]["title"], "住まい");
    assert_eq!(body["output"]["units"][0]["s2"], 3);
    assert_eq!(body["output"]["keys"][1], "引っ越し");
}

/// 知らない列挙の値を持つ要素は、その要素だけを型が合わないとして数え、台帳の全体は読める。`s`・`a`・`v` を欠く要素も
/// 捨て、型の違う項目を持つ要素も捨てる。ほかの項目は既定値で読む (`l` は `world`、`e` は `self`、`m` は `fact`、`tn` は
/// `current`)。
/// verifies: AC-NFR-304-3 (知らない列挙の値の要素は、その要素だけを数えて導出に使わない), 04 §5.2 (読むときの型)
#[test]
fn elements_with_unknown_values_are_counted_and_the_ledger_stays_readable() {
    let mut ledger = sample_ledger();
    ledger
        .output
        .set_section(
            LedgerOutput::UNITS,
            &serde_json::json!([
                {"s": 1, "a": "residence", "v": "見本の町"},
                {"s": 1, "l": "world", "e": "self", "a": "hobby", "v": "将棋", "m": "sarcastic",
                 "tn": "current", "old": "", "when": ""},
                {"s": 2, "l": "dream", "a": "hobby", "v": "囲碁"},
                {"a": "hobby", "v": "番号なし"},
                {"s": 3, "v": "属性なし"},
                {"s": 2, "a": "hobby", "v": "読書", "tn": "past", "e": 7},
            ]),
        )
        .unwrap();
    ledger
        .output
        .set_section(
            LedgerOutput::KEYS,
            &serde_json::json!(["住まい", 3, {"k": "x"}]),
        )
        .unwrap();
    ledger
        .output
        .set_section(
            LedgerOutput::CORRECTIONS,
            &serde_json::json!([{"id": 1, "verdict": "maybe"}, {"id": 2, "verdict": "reject"}, {"verdict": "accept"}]),
        )
        .unwrap();
    ledger.output.set_invalid("hints", "not an array").unwrap();
    ledger.status = "paused".into();
    let e = envelope(KIND_PROPOSAL, t(42), &ledger);
    let key = key_of(KIND_PROPOSAL, t(42));
    let back = Envelope::decode(&key, &e.encode(&key)).unwrap();
    let read: LedgerBody = back
        .body_as()
        .expect("the ledger as a whole stays readable");
    assert_eq!(read.status(), LedgerStatus::Unknown);
    let units = read.output.units();
    assert_eq!(units.mismatched, 5, "{:?}", units.items);
    assert_eq!(
        units.items,
        [LedgerUnit {
            sentence: 1,
            layer: LedgerLayer::World,
            entity: "self".into(),
            aspect: "residence".into(),
            value: "見本の町".into(),
            mood: LedgerMood::Fact,
            tense: LedgerTense::Current,
            old: String::new(),
            when: String::new(),
        }]
    );
    let keys = read.output.keys();
    assert_eq!(
        (keys.items, keys.mismatched),
        (vec!["住まい".to_owned()], 2)
    );
    let corrections = read.output.corrections();
    assert_eq!(corrections.mismatched, 2);
    assert_eq!(
        corrections.items,
        [LedgerCorrection {
            id: 2,
            verdict: LedgerVerdict::Reject
        }]
    );
    assert_eq!(
        read.output.invalid().get("hints").map(String::as_str),
        Some("not an array")
    );
    assert!(!read.output.has("hints"));
    // 合った節を入れ直すと、その節の理由は消える。
    let mut fixed = read.output.clone();
    fixed.set_section("hints", &serde_json::json!([])).unwrap();
    assert!(fixed.has("hints") && fixed.invalid().is_empty());
}

/// 知らない `op`・`origin`・`scope` の指示も読め、知らない `op` は [`DirectiveOp::Unknown`] になる。知らない項目は残る。
/// verifies: 04 §7.5 (知らない `op` の読み方), 03 §4.2 の規則 7・11
#[test]
fn a_directive_with_unknown_values_is_read_and_keeps_unknown_fields() {
    let mut d = DirectiveBody::new(DirectiveOp::Private);
    d.op = "pin".into();
    d.origin = "learned".into();
    d.scope = Some("forever".into());
    let mut e = envelope(KIND_DIRECTIVE, t(43), &d);
    add_field(&mut e, "pin_rank", &serde_json::json!(1));
    let key = key_of(KIND_DIRECTIVE, t(43));
    let mut back = Envelope::decode(&key, &e.encode(&key)).unwrap();
    let read: DirectiveBody = back.body_as().unwrap();
    assert_eq!(read.op(), DirectiveOp::Unknown);
    assert_eq!(read.origin(), DirectiveOrigin::User);
    assert_eq!(read.scope(), None);
    back.set_body(&read).unwrap();
    assert_eq!(envelope_to_json(&back)["body"]["pin_rank"], 1);

    let mut private = DirectiveBody::new(DirectiveOp::Private);
    private.scope = Some("forever".into());
    private.origin = "import".into();
    assert_eq!(private.scope(), Some(PrivateScope::Successors));
    assert_eq!(private.origin(), DirectiveOrigin::Import);
    private.scope = Some(PrivateScope::ValueOnly.name().into());
    assert_eq!(private.scope(), Some(PrivateScope::ValueOnly));
    for op in [
        DirectiveOp::Forget,
        DirectiveOp::End,
        DirectiveOp::Private,
        DirectiveOp::PrivateOff,
        DirectiveOp::Accept,
        DirectiveOp::Revert,
        DirectiveOp::Unblock,
    ] {
        assert_eq!(DirectiveOp::of(op.name().unwrap()), op);
    }
}

/// 区切りの ID は最初の往復の発話のターンだけで決まり、ターンの ID と違う (04 §2 の「区切りの ID」)。
/// verifies: 04 §2 (区切りの ID は SHA-256 ("evorag.seg.1" ‖ 0x00 ‖ ターンの ID) の頭の 16 バイト)
#[test]
fn a_segment_id_is_derived_from_its_first_turn() {
    use sha2::{Digest, Sha256};
    let first = t(2);
    let mut h = Sha256::new();
    h.update(b"evorag.seg.1\0");
    h.update(first.as_bytes());
    let want: [u8; 16] = h.finalize()[..16].try_into().unwrap();
    assert_eq!(segment_id(first), Id::from_bytes(want));
    assert_ne!(segment_id(first), first);
    let body = SegmentBody {
        session: t(1),
        pairs: vec![first, t(4)],
        closed: true,
        chars: 42,
    };
    assert_eq!(body.id(), Some(segment_id(first)));
    let e = envelope(KIND_SEGMENT, segment_id(first), &body);
    let key = key_of(KIND_SEGMENT, segment_id(first));
    let back = Envelope::decode(&key, &e.encode(&key)).unwrap();
    assert_eq!(back.body_as::<SegmentBody>().unwrap(), body);
}

/// 単位の本体を書いて読める (04 §5.1 の「本体の形」)。
/// verifies: 04 §5.1 (単位の本体の形)
#[test]
fn a_unit_body_round_trips() {
    let when = UnitWhen {
        from: Some("2026-01".into()),
        to: None,
        basis: WhenBasis::Stated,
    };
    let body = UnitBody {
        layer: UnitLayer::World,
        entity: "self".into(),
        aspect: "residence".into(),
        free: false,
        value: "見本の町".into(),
        vk: "ab".repeat(32),
        norm: NORM_V1,
        when: Some(when.clone()),
        tense: UnitTense::Current,
        state: Some(UnitState::Superseded),
        against: None,
        by: Some(t(31)),
        state_at: Some(1_764_115_200_000),
        private: false,
        anchors: vec![Anchor {
            turn: t(2),
            rev: 0,
            start: 6,
            end: 18,
            s_start: 0,
            s_end: 30,
            kind: AnchorKind::Said,
            when: Some(when),
        }],
    };
    let e = envelope(KIND_UNIT, t(30), &body);
    let key = key_of(KIND_UNIT, t(30));
    let back = Envelope::decode(&key, &e.encode(&key)).unwrap();
    assert_eq!(back.body_as::<UnitBody>().unwrap(), body);
    // 知らない状態の単位は型として読めない (導き直す、04 §5.1 の「知らない値」)。
    let mut odd = back.clone();
    add_field(&mut odd, "state", &serde_json::json!("frozen"));
    assert!(odd.body_as::<UnitBody>().is_err());
}

/// 台帳は先に頭だけを読み、`segment` 以外の呼び出しなら本体を読まない。項目を欠く台帳は既定値で読み (有効でない側)、
/// 型として読めない台帳も読めない記録にしない。
/// verifies: 04 §5.2.1 (台帳の読み方), 03 §3.3 (`call` が `segment` 以外の台帳は読み飛ばして残す)
#[test]
fn a_ledger_is_read_head_first_and_missing_fields_read_as_invalid() {
    let key = key_of(KIND_PROPOSAL, t(44));
    let read = |body: &serde_json::Value| {
        let e = envelope(KIND_PROPOSAL, t(44), body);
        read_ledger(&Envelope::decode(&key, &e.encode(&key)).unwrap())
    };
    // ほかの呼び出し (v0.7 の見出しなど): 本体の形が違っても、頭だけで止まる。
    let other = read(&serde_json::json!({"call": "topic_title", "sentences": "not a list"}));
    assert!(matches!(other, Some(LedgerRead::OtherCall(ref h)) if h.call == "topic_title"));
    // 項目を欠く区切りの台帳: 既定値で読む (規則の版 0・状態は知らない値)。
    let Some(LedgerRead::Segment(bare)) = read(&serde_json::json!({"call": "segment"})) else {
        panic!("a segment ledger with missing fields is still read");
    };
    assert_eq!(bare.status(), LedgerStatus::Unknown);
    assert_eq!((bare.schema, bare.inputs.prefilter), (0, 0));
    assert!(bare.sentences.is_empty() && bare.output.sections().next().is_none());
    // 本体を型として読めない区切りの台帳: 有効でない台帳として扱う (消さない)。
    let broken = read(
        &serde_json::json!({"call": "segment", "status": "complete", "sentences": [{"s": 1}]}),
    );
    assert!(
        matches!(broken, Some(LedgerRead::Unreadable(ref h)) if h.status() == LedgerStatus::Complete)
    );
    // 今の見本は、区切りの台帳として読める。
    let Some(LedgerRead::Segment(full)) = read(&serde_json::to_value(sample_ledger()).unwrap())
    else {
        panic!("the sample is a segment ledger");
    };
    assert_eq!(*full, sample_ledger());
}

/// `output` の節の名前 `invalid` は予約で、節として書けない。合わなかった理由が読めない形なら、上書きしない。
/// verifies: 04 §5.2.1 (`output` の `invalid` は予約の名前)
#[test]
fn the_reserved_section_name_is_refused() {
    let mut output = LedgerOutput::default();
    assert!(matches!(
        output.set_section(LedgerOutput::INVALID, &serde_json::json!({})),
        Err(LedgerOutputError::Reserved(_))
    ));
    assert!(matches!(
        output.set_invalid(LedgerOutput::INVALID, "x"),
        Err(LedgerOutputError::Reserved(_))
    ));
    // 読めない形の `invalid` (新しい版が書いた形など) は書き換えない。
    let mut ledger = sample_ledger();
    let mut e = envelope(KIND_PROPOSAL, t(45), &ledger);
    let mut body = envelope_to_json(&e)["body"].clone();
    body["output"]["invalid"] = serde_json::json!(["not", "a", "map"]);
    e.set_body(&body).unwrap();
    ledger = e.body_as().unwrap();
    assert!(matches!(
        ledger.output.set_section(LedgerOutput::KEYS, &vec!["鍵"]),
        Err(LedgerOutputError::UnreadableInvalid)
    ));
    assert!(matches!(
        ledger.output.set_invalid(LedgerOutput::UNITS, "x"),
        Err(LedgerOutputError::UnreadableInvalid)
    ));
}
