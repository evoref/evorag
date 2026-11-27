//! 台帳の掃除の試験 (04 §7.2)。

use super::*;
use crate::apply::ValueKeys;
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveRedaction};
use crate::ledger::{
    CALL_SEGMENT, CorrInput, LedgerCorrectionOut, LedgerLayer, LedgerMood, LedgerRef, LedgerTense,
    LedgerUnitOut, LedgerVerdict,
};
use crate::value_key::{normalize_loose, normalize_v1};
use serde::Serialize;

const K: ForgetKey = ForgetKey::from_bytes_const([9; 32]);
const M: &str = "[伏せ字]";

fn id(n: u64) -> Id {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    Id::from_bytes(b)
}

fn unit(s: u32, entity: &str, aspect: &str, value: &str, old: &str, when: &str) -> LedgerUnitOut {
    LedgerUnitOut {
        sentence: s,
        layer: LedgerLayer::World,
        entity: entity.to_owned(),
        aspect: aspect.to_owned(),
        value: value.to_owned(),
        mood: LedgerMood::Fact,
        tense: LedgerTense::Current,
        old: old.to_owned(),
        when: when.to_owned(),
    }
}

/// 1 つのターン `turn` の本文 `text` を、文ごとに (`。` で) 切って `sentences` にした台帳。
fn ledger(turn: Id, text: &str, keys: &[&str], units: &[LedgerUnitOut]) -> LedgerBody {
    let mut b = LedgerBody {
        call: CALL_SEGMENT.to_owned(),
        status: "complete".to_owned(),
        ..LedgerBody::default()
    };
    let mut start = 0;
    for (i, part) in text.split_inclusive('。').enumerate() {
        b.sentences.push(SentenceSpan::new(
            u32::try_from(i + 1).expect("fits"),
            turn,
            0,
            to_u32(start),
            to_u32(start + part.len()),
        ));
        start += part.len();
    }
    b.inputs.input_hash = Some("ab".repeat(32));
    let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
    b.output
        .set_section(LedgerOutput::KEYS, &keys)
        .expect("keys");
    b.output
        .set_section(LedgerOutput::UNITS, &units)
        .expect("units");
    b
}

fn forget(value: &str, turns: &[Id]) -> DirectiveBody {
    let mut b = DirectiveBody::new(DirectiveOp::Forget);
    b.entity = Some("self".to_owned());
    b.aspect = Some("residence".to_owned());
    b.vks = ValueKeys::of(&K, value).to_hex();
    b.turns = turns.to_vec();
    b
}

/// `b` に、ターン `turn` の本文 `text` (改訂の番号 `rev`) の `needle` のすべてを伏せ字にする記述を足す。
fn redact(
    mut b: DirectiveBody,
    turn: Id,
    text: &str,
    rev: u32,
    needle: &str,
) -> (DirectiveBody, String) {
    let ranges: Vec<Range<usize>> = text
        .match_indices(needle)
        .map(|(i, m)| i..i + m.len())
        .collect();
    let (r, after) = Redaction::plan(text, rev, ranges, M).expect("plan");
    b.redact = true;
    b.redactions
        .extend(r.ranges().iter().map(|x| DirectiveRedaction {
            turn,
            rev_before: rev,
            rev_after: rev + 1,
            start: to_u32(x.start),
            end: to_u32(x.end),
            hash: r.hash().to_string(),
        }));
    (b, after)
}

fn set(ds: Vec<(u64, DirectiveBody)>) -> DirectiveSet {
    DirectiveSet::new(ds.into_iter().map(|(n, b)| (id(n), b)), M, normalize_v1)
}

fn texts<'a>(list: &[(Id, u32, &'a str)]) -> TurnTexts<'a> {
    list.iter()
        .map(|&(turn, rev, text)| RevText { turn, rev, text })
        .collect()
}

fn scrub(
    l: &mut LedgerBody,
    set: &DirectiveSet,
    first: Option<Id>,
    texts: &TurnTexts<'_>,
) -> Scrubbed {
    scrub_ledger(
        l,
        &ScrubContext {
            key: &K,
            directives: set,
            first_turn: first,
            texts,
        },
    )
    .expect("scrub")
}

fn values(l: &LedgerBody) -> Vec<String> {
    l.output
        .units()
        .items
        .into_iter()
        .map(|u| format!("{}={}", u.aspect, u.value))
        .collect()
}

fn keys(l: &LedgerBody) -> Vec<String> {
    l.output.keys().items
}

/// verifies: 04 §7.2 (台帳の掃除の 2・4: v・old・when・a・e の語句か部分文字列の鍵で除き、entity と属性は問わない。除いた台帳の keys はすべて除く)
#[test]
fn substrings_of_any_field_remove_a_candidate() {
    let t = id(1);
    let text = "霧ヶ丘に住んでいて、柴犬を飼っています。自治会の会計係です。大家さんは猫が好きです。日記をつけています。";
    let mut l = ledger(
        t,
        text,
        &["暮らし", "ペットとの散歩"],
        &[
            unit(1, "self", "pet", "柴犬", "", ""),
            unit(2, "self", "霧ヶ丘の自治会の役", "会計係", "", ""),
            unit(3, "霧ヶ丘の大家さん", "likes", "猫", "", ""),
            unit(4, "self", "続けていること", "日記", "", "霧ヶ丘に越した日"),
            unit(1, "妻", "hometown", "白樺台", "霧ヶ丘", ""),
        ],
    );
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    let r = scrub(&mut l, &s, Some(t), &texts(&[(t, 0, text)]));
    assert_eq!(values(&l), ["pet=柴犬"]);
    assert!(keys(&l).is_empty(), "1〜3 で除いた台帳の keys はすべて除く");
    assert_eq!((r.units, r.keys, r.removed()), (4, 2, 6));
    assert_eq!(l.scrubbed, 6);
    assert_eq!(l.inputs.input_hash, None);
    let mut removed: Vec<ValueKey> = ["暮らし", "ペットとの散歩"]
        .map(|k| value_key(&K, k))
        .into();
    removed.sort();
    assert_eq!(r.removed_keys, removed, "除いた鍵の値の鍵を返す");
}

/// verifies: 04 §7.2 (掃除の 1: 伏せ字の後の本文に写した根拠の文に v か old が無い候補を除き、ほかの値は残す。伏せ字の前の本文があってもなくても同じ結果)
#[test]
fn rule_1_does_not_depend_on_which_text_is_at_hand() {
    let t = id(1);
    let text = "鳴瀬台に住んで、瀬台公園と柴犬の話をする。";
    let l = ledger(
        t,
        text,
        &[],
        &[
            unit(1, "self", "residence", "瀬台", "", ""),
            unit(1, "self", "residence", "鳴瀬", "", ""),
            unit(1, "self", "pet", "柴犬", "", ""),
        ],
    );
    let (f, after) = redact(forget("鳴瀬台", &[t]), t, text, 0, "鳴瀬台");
    let s = set(vec![(10, f)]);
    let run = |tx: &TurnTexts<'_>| {
        let mut x = l.clone();
        scrub(&mut x, &s, Some(t), tx);
        x
    };
    // 伏せ字と同じトランザクション (前の本文だけ)、前と後の本文、当て直し (後の本文だけ)。
    let pre = run(&texts(&[(t, 0, text)]));
    let both = run(&texts(&[(t, 0, text), (t, 1, &after)]));
    let post = run(&texts(&[(t, 1, &after)]));
    assert_eq!(pre, both);
    assert_eq!(pre, post);
    // 「瀬台」は後の本文 (瀬台公園) にも残るので残り、「鳴瀬」は残らないので除く。
    assert_eq!(values(&pre), ["residence=瀬台", "pet=柴犬"]);
    assert_eq!(pre.sentences[0].rev, 1);
    // 本文を作れないとき (渡したが読めない) は、伏せ字に掛かる根拠の文の候補を除く。
    let mut none = TurnTexts::new();
    none.unreadable(t);
    assert!(values(&run(&none)).is_empty());
}

/// verifies: 04 §7.2 (本文の渡し忘れは誤りにし、台帳を変えない)
#[test]
fn a_turn_not_given_is_an_error_and_leaves_the_ledger() {
    let t = id(1);
    let text = "港町に住んでいます。";
    let mut l = ledger(
        t,
        text,
        &["港町の暮らし"],
        &[unit(1, "self", "pet", "柴犬", "", "")],
    );
    let (f, _) = redact(forget("港町", &[t]), t, text, 0, "港町");
    let before = l.clone();
    let err = scrub_ledger(
        &mut l,
        &ScrubContext {
            key: &K,
            directives: &set(vec![(10, f)]),
            first_turn: Some(t),
            texts: &TurnTexts::new(),
        },
    );
    assert!(
        matches!(err, Err(ScrubError::TurnNotGiven(x)) if x == t),
        "{err:?}"
    );
    assert_eq!(l, before);
}

/// verifies: 04 §7.2 (台帳の掃除の 3: 候補の発話が対象のターンにあるか、old・new の範囲の文字列が vks に当たる訂正を、判定と表の同じ id の組で除く。corr だけを除いても input_hash を null にする)
#[test]
fn corrections_are_removed_by_turn_or_by_range_strings() {
    let (a1, a2, a3) = (id(1), id(2), id(3));
    let t1 = "出身は白浜じゃなくて霧ヶ浦です。";
    let t2 = "麺は、うどんじゃなくてそばです。";
    let t3 = "前は港町じゃなくて山町です。";
    let mut l = ledger(a1, t1, &[], &[]);
    for (s, turn, t) in [(2, a2, t2), (3, a3, t3)] {
        l.sentences
            .push(SentenceSpan::new(s, turn, 0, 0, to_u32(t.len())));
    }
    let range = |t: &str, n: &str| {
        let s = t.find(n).expect("in text");
        ByteRange {
            start: to_u32(s),
            end: to_u32(s + n.len()),
        }
    };
    let corr = |id: u32, turn: Id, t: &str, new: &str, old: &str| CorrInput {
        id,
        turn,
        rev: 0,
        pattern: "p".into(),
        new: range(t, new),
        old: Some(range(t, old)),
        extra: UnknownFields::new(),
    };
    l.inputs.corr = vec![
        corr(1, a1, t1, "霧ヶ浦", "白浜"),
        corr(2, a2, t2, "そば", "うどん"),
        corr(3, a3, t3, "山町", "港町"),
    ];
    let verdicts: Vec<LedgerCorrectionOut> = (1..=2)
        .map(|id| LedgerCorrectionOut {
            id,
            verdict: LedgerVerdict::Accept,
        })
        .collect();
    l.output
        .set_section(LedgerOutput::CORRECTIONS, &verdicts)
        .expect("corrections");
    let mut other = forget("港町", &[id(98)]);
    other.aspect = Some("hometown".into());
    let s = set(vec![(10, forget("霧ヶ浦", &[a1])), (11, other)]);
    let tx = texts(&[(a1, 0, t1), (a2, 0, t2), (a3, 0, t3)]);
    let r = scrub(&mut l, &s, Some(a1), &tx);
    let ids: Vec<u32> = l.output.corrections().items.iter().map(|c| c.id).collect();
    let corr_ids: Vec<u32> = l.inputs.corr.iter().map(|c| c.id).collect();
    assert_eq!(ids, [2]);
    assert_eq!(corr_ids, [2]);
    assert_eq!(
        (r.corrections, r.corr, r.removed()),
        (1, 2, 1),
        "corr は scrubbed に数えない"
    );
    // corr だけを除いた台帳 (判定の無い候補 3) も input_hash を null にする。
    let mut only = ledger(a3, t3, &[], &[]);
    only.inputs.corr = vec![corr(3, a3, t3, "山町", "港町")];
    let s = set(vec![(11, forget("港町", &[id(98)]))]);
    let r = scrub(&mut only, &s, None, &texts(&[(a3, 0, t3)]));
    assert_eq!((r.corr, r.removed()), (1, 0));
    assert_eq!(only.inputs.input_hash, None);
}

/// verifies: 04 §7.2 (unblock で印を解いた指示は、根拠のターンの ID が unblock より前の要素にだけ当て続ける。keys は区切りの最初のターンで比べる)
#[test]
fn unblocked_forgets_apply_only_to_earlier_evidence() {
    let mut unblock = DirectiveBody::new(DirectiveOp::Unblock);
    unblock.target = Some(id(10));
    let s = set(vec![(10, forget("ハッカ", &[id(1)])), (20, unblock)]);
    let text = "愛犬の名前はハッカです。";
    for (turn, kept) in [(id(15), false), (id(25), true)] {
        let mut l = ledger(
            turn,
            text,
            &["愛犬の名前", "ハッカ"],
            &[unit(1, "愛犬", "name", "ハッカ", "", "")],
        );
        let before = l.clone();
        let r = scrub(&mut l, &s, Some(turn), &texts(&[(turn, 0, text)]));
        if kept {
            assert_eq!(l, before, "unblock の後の発話の台帳は変えない");
            assert!(!r.changed());
        } else {
            assert!(values(&l).is_empty());
            assert!(keys(&l).is_empty());
            assert_eq!(l.inputs.input_hash, None);
        }
    }
}

/// verifies: 04 §7.2 (keys は語句か部分文字列で除き、対象のターンを含む台帳の keys はすべて除く。当たらない台帳は変えない)
#[test]
fn keys_are_removed_by_substrings_or_entirely_for_target_turns() {
    let t = id(1);
    let text = "週末は朝市に行く予定です。";
    let tx = texts(&[(t, 0, text)]);
    let mut l = ledger(t, text, &["霧ヶ丘の朝市", "週末の過ごし方"], &[]);
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    let r = scrub(&mut l, &s, Some(t), &tx);
    assert_eq!(keys(&l), ["週末の過ごし方"]);
    assert_eq!((r.keys, l.scrubbed), (1, 1));
    let mut l = ledger(t, text, &["週末の過ごし方"], &[]);
    let s = set(vec![(10, forget("霧ヶ丘", &[t]))]);
    scrub(&mut l, &s, Some(t), &tx);
    assert!(keys(&l).is_empty());
    assert_eq!(l.inputs.input_hash, None);
    let mut l = ledger(t, text, &[], &[]);
    let r = scrub(&mut l, &s, Some(t), &tx);
    assert!(r.hash_cleared);
    let mut l = ledger(
        t,
        text,
        &["週末の過ごし方"],
        &[unit(1, "self", "plan", "朝市", "", "")],
    );
    let before = l.clone();
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    assert!(!scrub(&mut l, &s, Some(t), &tx).changed());
    assert_eq!(l, before);
    let mut private = forget("朝市", &[]);
    private.op = "private".to_owned();
    assert!(!scrub(&mut l, &set(vec![(10, private)]), Some(t), &tx).changed());
    assert_eq!(l, before);
}

#[derive(Serialize)]
struct Extra<'a> {
    s: u32,
    a: &'a str,
    v: &'a str,
    note: &'a str,
}

#[derive(Serialize)]
struct Broken<'a> {
    v: &'a str,
}

/// CBOR の tag を付けた値 (ciborium の tag の書き方)。
struct Tagged<T>(u64, T);

impl<T: Serialize> Serialize for Tagged<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTupleVariant;
        let mut t = s.serialize_tuple_variant("@@TAG@@", 0, "@@TAGGED@@", 2)?;
        t.serialize_field(&self.0)?;
        t.serialize_field(&self.1)?;
        t.end()
    }
}

#[derive(Serialize)]
struct WithTag<'a> {
    s: u32,
    a: &'a str,
    v: &'a str,
    note: Tagged<&'a str>,
}

/// verifies: 04 §7.2 (読むときの型に合わない要素・知らない項目・tag の付いた値・知らない節の値と項目の名前にも、忘れた値の写しを残さない)
#[test]
fn unknown_fields_unreadable_elements_tags_and_unknown_sections_are_scanned() {
    let t = id(1);
    let text = "柴犬を飼っています。";
    let mut l = ledger(t, text, &[], &[]);
    let units = vec![
        RawValue::from_serialize(&unit(1, "self", "pet", "柴犬", "", "")).expect("raw"),
        RawValue::from_serialize(&Extra {
            s: 1,
            a: "pet",
            v: "三毛猫",
            note: "鳴瀬台の家で",
        })
        .expect("raw"),
        RawValue::from_serialize(&Broken { v: "鳴瀬台" }).expect("raw"),
        // 知らない項目に tag の付いた値があっても、型の項目 `v` を見落とさない。
        RawValue::from_serialize(&WithTag {
            s: 1,
            a: "pet",
            v: "鳴瀬台の犬",
            note: Tagged(32, "x"),
        })
        .expect("raw"),
        RawValue::from_serialize(&WithTag {
            s: 1,
            a: "pet",
            v: "秋田犬",
            note: Tagged(32, "鳴瀬台"),
        })
        .expect("raw"),
    ];
    l.output
        .set_section(LedgerOutput::UNITS, &units)
        .expect("units");
    l.output
        .set_section("topics", &vec!["鳴瀬台の暮らし", "犬"])
        .expect("topics");
    let named: BTreeMap<&str, u32> = [("鳴瀬台", 1), ("ok", 2)].into();
    l.output
        .set_section("named", &vec![named.clone(), [("ok", 3)].into()])
        .expect("named");
    let s = set(vec![(10, forget("鳴瀬台", &[id(99)]))]);
    let r = scrub(&mut l, &s, Some(t), &texts(&[(t, 0, text)]));
    assert_eq!(values(&l), ["pet=柴犬"]);
    assert_eq!(l.output.units().mismatched, 0);
    let topics: Vec<String> = l
        .output
        .raw_section("topics")
        .expect("kept")
        .to()
        .expect("strings");
    assert_eq!(topics, ["犬"]);
    let named: Vec<BTreeMap<String, u32>> = l
        .output
        .raw_section("named")
        .expect("kept")
        .to()
        .expect("maps");
    assert_eq!(named, [BTreeMap::from([("ok".to_owned(), 3)])]);
    assert_eq!((r.units, r.other, r.removed()), (4, 2, 6));
}

/// verifies: 04 §7.2 (台帳の本体と入れ子の型の知らない項目と、invalid の理由も掃除する)、03 §4.2 の規則 7 (知らない項目は書き戻しても残る)
#[test]
fn unknown_fields_of_the_body_and_invalid_reasons_are_scrubbed() {
    let t = id(1);
    let text = "柴犬を飼っています。";
    let mut l = ledger(t, text, &[], &[unit(1, "self", "pet", "柴犬", "", "")]);
    let raw = |v: &str| RawValue::from_serialize(&v).expect("raw");
    l.extra.insert("topic_hint".into(), raw("鳴瀬台の話"));
    l.extra.insert("future".into(), raw("残る"));
    l.inputs.extra.insert("echo".into(), raw("鳴瀬台"));
    l.sentences[0]
        .extra
        .insert("鳴瀬台".into(), raw("項目の名前"));
    l.inputs.refs.push(LedgerRef::new(t, 0));
    l.inputs.refs[0]
        .extra
        .insert("quote".into(), raw("鳴瀬台です"));
    l.output
        .set_invalid("insights", "鳴瀬台 は型に合わない")
        .expect("invalid");
    l.output
        .set_invalid("titles", "not_array")
        .expect("invalid");
    // 知らない項目は、書き戻しても残る (03 §4.2 の規則 7)。
    let back: LedgerBody = RawValue::from_serialize(&l)
        .expect("raw")
        .to()
        .expect("read back");
    assert_eq!(back, l);
    let s = set(vec![(10, forget("鳴瀬台", &[id(99)]))]);
    let r = scrub(&mut l, &s, Some(t), &texts(&[(t, 0, text)]));
    assert_eq!(r.unknown_fields, 5);
    assert_eq!(l.extra.keys().collect::<Vec<_>>(), ["future"]);
    assert!(l.inputs.extra.is_empty());
    assert!(l.sentences[0].extra.is_empty());
    assert!(l.inputs.refs[0].extra.is_empty());
    let reasons = l.output.invalid();
    assert_eq!(reasons["insights"], "scrubbed");
    assert_eq!(reasons["titles"], "not_array");
    assert_eq!(values(&l), ["pet=柴犬"]);
}

/// verifies: 04 §7.2 (型として読めない台帳と知らない呼び出しの台帳は、生のまま走査し、当たれば消す)
#[test]
fn raw_ledgers_are_scanned_whole() {
    #[derive(Serialize)]
    struct Other<'a> {
        call: &'a str,
        title: &'a str,
        units: Vec<Tagged<&'a str>>,
    }
    let env = |title: &str, inner: &str| {
        let mut e = Envelope::new(
            id(50),
            "proposal",
            1,
            evorag_types::Timestamp::from_millis(0),
            "test",
        );
        e.set_body(&Other {
            call: "topic_title",
            title,
            units: vec![Tagged(32, inner)],
        })
        .expect("body");
        e
    };
    let s = set(vec![(10, forget("鳴瀬台", &[]))]);
    assert_eq!(
        scan_raw_ledger(&K, &s, &env("散歩の話", "柴犬")),
        RawLedger::Keep
    );
    assert_eq!(
        scan_raw_ledger(&K, &s, &env("鳴瀬台の話", "柴犬")),
        RawLedger::Delete
    );
    assert_eq!(
        scan_raw_ledger(&K, &s, &env("散歩", "鳴瀬台")),
        RawLedger::Delete
    );
    assert_eq!(
        scan_raw_ledger(&K, &set(vec![]), &env("鳴瀬台", "")),
        RawLedger::Keep
    );
}

/// 決まった種から作る、小さな擬似乱数 (性質を多くの入力で確かめるため)。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).expect("fits") % n
    }
}

const WORDS: [&str; 6] = ["霧ヶ丘", "柴犬", "朝市", "港町", "日記", "将棋"];

struct Case {
    turn: Id,
    ledger: LedgerBody,
    directives: Vec<(u64, DirectiveBody)>,
    /// 改訂の番号ごとの本文 (伏せ字の前と後)。
    texts: Vec<(u32, String)>,
    /// 印を解いていない忘れるの値 (区切りの最初のターンより前に解いた忘れるは除く)。
    blocked: Vec<&'static str>,
}

fn random_case(rng: &mut Lcg) -> Case {
    let turn = id(5);
    let text: String = (0..3)
        .map(|_| format!("{}と{}の話です。", WORDS[rng.next(6)], WORDS[rng.next(6)]))
        .collect();
    let units: Vec<LedgerUnitOut> = (0..4)
        .map(|_| {
            let s = u32::try_from(rng.next(3) + 1).expect("fits");
            let w = WORDS[rng.next(6)];
            let old = if rng.next(3) == 0 {
                WORDS[rng.next(6)]
            } else {
                ""
            };
            unit(s, "self", "topic", w, old, "")
        })
        .collect();
    let keys: Vec<String> = (0..2)
        .map(|_| format!("{}の鍵", WORDS[rng.next(6)]))
        .collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ledger = ledger(turn, &text, &key_refs, &units);
    let mut directives = Vec::new();
    let mut texts = vec![(0, text)];
    let mut blocked = Vec::new();
    for n in 0..rng.next(3) + 1 {
        let w = WORDS[rng.next(6)];
        let targets: &[Id] = if rng.next(2) == 0 { &[] } else { &[turn] };
        let mut f = forget(w, targets);
        let (rev, current) = texts.last().cloned().expect("text");
        if rng.next(2) == 0 && current.contains(w) {
            let (g, after) = redact(f, turn, &current, rev, w);
            f = g;
            texts.push((rev + 1, after));
        }
        let fid = 10 + u64::try_from(n).expect("fits");
        directives.push((fid, f));
        // 印を解く: 区切りのターン (5) より前の ID の unblock は不正 (相手より小さい) なので、後の ID で解く。
        // 解いた忘れるは、ターン 5 の要素にはまだ当たる (5 < 解いた ID)。
        if rng.next(3) == 0 {
            let mut u = DirectiveBody::new(DirectiveOp::Unblock);
            u.target = Some(id(fid));
            directives.push((100 + fid, u));
        }
        blocked.push(w);
    }
    Case {
        turn,
        ledger,
        directives,
        texts,
        blocked,
    }
}

/// verifies: 04 §7.2 (掃除は何度当てても同じ。指示を足しながら当てても、まとめて当てても同じ。伏せ字の前の本文の有無に依らない。忘れた値の写しを残さない)
#[test]
fn scrubbing_is_idempotent_incremental_path_independent_and_leaves_no_value() {
    let mut rng = Lcg(0x5eed);
    for case in 0..80 {
        let c = random_case(&mut rng);
        let t = c.turn;
        let all_texts: TurnTexts<'_> = c
            .texts
            .iter()
            .map(|(rev, s)| RevText {
                turn: t,
                rev: *rev,
                text: s,
            })
            .collect();
        let last = c.texts.last().expect("text");
        let post_only = texts(&[(t, last.0, &last.1)]);
        let all = set(c.directives.clone());
        let mut once = c.ledger.clone();
        scrub(&mut once, &all, Some(t), &all_texts);
        let mut twice = once.clone();
        let again = scrub(&mut twice, &all, Some(t), &all_texts);
        assert_eq!(twice, once, "case {case}: idempotent");
        assert!(
            !again.changed(),
            "case {case}: the second pass changes nothing"
        );
        // 指示を 1 つずつ足しながら当てる (書く前の掃除と、後の忘れるの掃除)。
        let mut step = c.ledger.clone();
        for k in 1..=c.directives.len() {
            scrub(
                &mut step,
                &set(c.directives[..k].to_vec()),
                Some(t),
                &all_texts,
            );
        }
        assert_eq!(step, once, "case {case}: incremental == batch");
        // 当て直し (伏せ字の後の本文しか無い) でも同じ台帳になる。
        let mut replay = c.ledger.clone();
        scrub(&mut replay, &all, Some(t), &post_only);
        assert_eq!(replay, once, "case {case}: path independence");
        // 忘れた値が、残った要素の文字列に (正規化 v1 か照合の形で) 含まれない。
        let left: Vec<String> = once
            .output
            .units()
            .items
            .into_iter()
            .flat_map(|u| [u.value, u.old, u.aspect, u.entity, u.when])
            .chain(once.output.keys().items)
            .collect();
        for w in &c.blocked {
            for s in &left {
                assert!(
                    !normalize_v1(s).contains(&normalize_v1(w))
                        && !normalize_loose(s).contains(&normalize_loose(w)),
                    "case {case}: {w} is kept in {s}"
                );
            }
        }
        assert!(
            once.sentences.iter().all(|s| s.rev == last.0),
            "case {case}"
        );
    }
}
