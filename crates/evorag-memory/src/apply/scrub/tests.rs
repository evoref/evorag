//! 台帳の掃除の試験 (04 §7.2)。

use super::*;
use crate::apply::ValueKeys;
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveRedaction};
use crate::ledger::{
    CorrInput, LedgerCorrectionOut, LedgerLayer, LedgerMood, LedgerTense, LedgerUnitOut,
    LedgerVerdict,
};
use evorag_store::forget::{ForgetOutcome, Redaction, apply_forget};

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
        call: crate::ledger::CALL_SEGMENT.to_owned(),
        status: "complete".to_owned(),
        ..LedgerBody::default()
    };
    let mut start = 0;
    for (i, part) in text.split_inclusive('。').enumerate() {
        b.sentences.push(SentenceSpan {
            s: u32::try_from(i + 1).expect("fits"),
            turn,
            rev: 0,
            start: to_u32(start),
            end: to_u32(start + part.len()),
        });
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
    DirectiveSet::new(ds.into_iter().map(|(n, b)| (id(n), b)), M)
}

fn scrub(
    l: &mut LedgerBody,
    set: &DirectiveSet,
    first: Option<Id>,
    texts: &[RevText<'_>],
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
    // 別のターンの忘れる (このターンは対象のターンではない)。
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    let texts = [RevText {
        turn: t,
        rev: 0,
        text,
    }];
    let r = scrub(&mut l, &s, Some(t), &texts);
    assert_eq!(values(&l), ["pet=柴犬"]);
    assert!(keys(&l).is_empty(), "1〜3 で除いた台帳の keys はすべて除く");
    assert_eq!((r.units, r.keys, r.removed()), (4, 2, 6));
    assert_eq!(l.scrubbed, 6);
    assert_eq!(l.inputs.input_hash, None);
}

/// verifies: 04 §7.2 (台帳の掃除の 1: v か old の一致が伏せ字に掛かる候補を除き、根拠の文に掛かるだけの候補は残す。位置を写し直す)
#[test]
fn redaction_overlap_removes_only_the_values_it_covers() {
    let t = id(1);
    let text = "鳴瀬台に住んで、柴犬を飼う。";
    let l = ledger(
        t,
        text,
        &[],
        &[
            // 値が伏せ字の一部に掛かる (鍵では当たらない) 写し。
            unit(1, "self", "residence", "瀬台", "", ""),
            unit(1, "self", "pet", "柴犬", "", ""),
        ],
    );
    let (f, after) = redact(forget("鳴瀬台", &[t]), t, text, 0, "鳴瀬台");
    let s = set(vec![(10, f)]);
    // 伏せ字の前の本文がある (伏せ字と同じトランザクション)。
    let pre = [
        RevText {
            turn: t,
            rev: 0,
            text,
        },
        RevText {
            turn: t,
            rev: 1,
            text: &after,
        },
    ];
    let mut a = l.clone();
    scrub(&mut a, &s, Some(t), &pre);
    assert_eq!(values(&a), ["pet=柴犬"]);
    let sent = a.sentences[0];
    assert_eq!(sent.rev, 1);
    assert_eq!(
        &after[to_usize(sent.start)..to_usize(sent.end)],
        after.as_str()
    );
    // 伏せ字の後の本文しか無い (当て直し) ときも同じ結果。
    let post = [RevText {
        turn: t,
        rev: 1,
        text: &after,
    }];
    let mut b = l.clone();
    scrub(&mut b, &s, Some(t), &post);
    assert_eq!(b, a);
    // 本文がまったく無いときは、伏せ字に掛かる根拠の文の候補を除く (安全の側)。
    let mut c = l;
    scrub(&mut c, &s, Some(t), &[]);
    assert!(values(&c).is_empty());
}

/// verifies: 04 §7.2 (台帳の掃除の 3: 候補の発話が対象のターンにあるか、old・new の範囲の文字列が vks に当たる訂正を、判定と表の同じ id の組で除く)
#[test]
fn corrections_are_removed_by_turn_or_by_range_strings() {
    let (a1, a2, a3) = (id(1), id(2), id(3));
    let t1 = "出身は白浜じゃなくて霧ヶ浦です。";
    let t2 = "麺は、うどんじゃなくてそばです。";
    let t3 = "前は港町じゃなくて山町です。";
    let mut l = ledger(a1, t1, &[], &[]);
    for (s, turn, t) in [(2, a2, t2), (3, a3, t3)] {
        l.sentences.push(SentenceSpan {
            s,
            turn,
            rev: 0,
            start: 0,
            end: to_u32(t.len()),
        });
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
    };
    l.inputs.corr = vec![
        corr(1, a1, t1, "霧ヶ浦", "白浜"),
        corr(2, a2, t2, "そば", "うどん"),
        corr(3, a3, t3, "山町", "港町"),
    ];
    let verdicts: Vec<LedgerCorrectionOut> = (1..=3)
        .map(|id| LedgerCorrectionOut {
            id,
            verdict: LedgerVerdict::Accept,
        })
        .collect();
    l.output
        .set_section(LedgerOutput::CORRECTIONS, &verdicts)
        .expect("corrections");
    // 「霧ヶ浦」の根拠は a1 (対象のターン)。「港町」は a3 の old の範囲の文字列で当たる別の忘れる。
    let mut other = forget("港町", &[id(98)]);
    other.aspect = Some("hometown".into());
    let s = set(vec![(10, forget("霧ヶ浦", &[a1])), (11, other)]);
    let texts = [
        RevText {
            turn: a1,
            rev: 0,
            text: t1,
        },
        RevText {
            turn: a2,
            rev: 0,
            text: t2,
        },
        RevText {
            turn: a3,
            rev: 0,
            text: t3,
        },
    ];
    let r = scrub(&mut l, &s, Some(a1), &texts);
    let ids: Vec<u32> = l.output.corrections().items.iter().map(|c| c.id).collect();
    let corr: Vec<u32> = l.inputs.corr.iter().map(|c| c.id).collect();
    assert_eq!(ids, [2]);
    assert_eq!(corr, [2]);
    assert_eq!(
        (r.corrections, r.corr, r.removed()),
        (2, 2, 2),
        "corr は scrubbed に数えない"
    );
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
        let texts = [RevText { turn, rev: 0, text }];
        let r = scrub(&mut l, &s, Some(turn), &texts);
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
    let texts = [RevText {
        turn: t,
        rev: 0,
        text,
    }];
    let mut l = ledger(t, text, &["霧ヶ丘の朝市", "週末の過ごし方"], &[]);
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    let r = scrub(&mut l, &s, Some(t), &texts);
    assert_eq!(keys(&l), ["週末の過ごし方"]);
    assert_eq!((r.keys, l.scrubbed), (1, 1));
    // 対象のターンを含む台帳は、鍵が当たらなくてもすべて除き、input_hash を null にする。
    let mut l = ledger(t, text, &["週末の過ごし方"], &[]);
    let s = set(vec![(10, forget("霧ヶ丘", &[t]))]);
    scrub(&mut l, &s, Some(t), &texts);
    assert!(keys(&l).is_empty());
    assert_eq!(l.inputs.input_hash, None);
    // 要素の無い台帳でも、対象のターンを含めば input_hash を null にする。
    let mut l = ledger(t, text, &[], &[]);
    let r = scrub(&mut l, &s, Some(t), &texts);
    assert!(r.hash_cleared);
    assert_eq!(l.inputs.input_hash, None);
    // 当たるものが無い台帳は、バイト列まで変えない。
    let mut l = ledger(
        t,
        text,
        &["週末の過ごし方"],
        &[unit(1, "self", "plan", "朝市", "", "")],
    );
    let before = l.clone();
    let s = set(vec![(10, forget("霧ヶ丘", &[id(99)]))]);
    assert!(!scrub(&mut l, &s, Some(t), &texts).changed());
    assert_eq!(l, before);
    // 忘れるの指示が無ければ何もしない (private は台帳を掃除しない)。
    let mut private = forget("朝市", &[]);
    private.op = "private".to_owned();
    assert!(!scrub(&mut l, &set(vec![(10, private)]), Some(t), &texts).changed());
    assert_eq!(l, before);
}

#[derive(serde::Serialize)]
struct Extra<'a> {
    s: u32,
    a: &'a str,
    v: &'a str,
    note: &'a str,
}

#[derive(serde::Serialize)]
struct Broken<'a> {
    v: &'a str,
}

/// verifies: 04 §7.2 (読むときの型に合わない要素・知らない項目・知らない節にも、忘れた値の写しを残さない)
#[test]
fn unknown_fields_unreadable_elements_and_unknown_sections_are_scanned() {
    let t = id(1);
    let text = "柴犬を飼っています。";
    let mut l = ledger(t, text, &[], &[]);
    // 知らない項目に値の写しがある要素、`s` を欠いて読めない要素、知らない節の要素。
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
    ];
    l.output
        .set_section(LedgerOutput::UNITS, &units)
        .expect("units");
    l.output
        .set_section("topics", &vec!["鳴瀬台の暮らし", "犬"])
        .expect("topics");
    let s = set(vec![(10, forget("鳴瀬台", &[id(99)]))]);
    let texts = [RevText {
        turn: t,
        rev: 0,
        text,
    }];
    let r = scrub(&mut l, &s, Some(t), &texts);
    assert_eq!(values(&l), ["pet=柴犬"]);
    assert_eq!(l.output.units().mismatched, 0);
    let topics: Vec<String> = l
        .output
        .raw_section("topics")
        .expect("kept")
        .to()
        .expect("strings");
    assert_eq!(topics, ["犬"]);
    assert_eq!((r.units, r.other, r.removed()), (2, 1, 3));
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
        directives.push((10 + u64::try_from(n).expect("fits"), f));
    }
    Case {
        turn,
        ledger,
        directives,
        texts,
    }
}

/// verifies: 04 §7.2 (掃除は何度当てても同じ。指示を足しながら当てても、まとめて当てても同じ。忘れた値の写しを残さない)
#[test]
fn scrubbing_is_idempotent_incremental_and_leaves_no_value() {
    let mut rng = Lcg(0x5eed);
    for case in 0..80 {
        let c = random_case(&mut rng);
        let t = c.turn;
        let texts: Vec<RevText<'_>> = c
            .texts
            .iter()
            .map(|(rev, s)| RevText {
                turn: t,
                rev: *rev,
                text: s,
            })
            .collect();
        // 伏せ字が本文と合うこと (試験の材料の確かめ)。
        let all = set(c.directives.clone());
        for w in c.texts.windows(2) {
            let red = all.redactions().redactions(t).expect("valid");
            let step = red.iter().find(|r| r.rev_before() == w[0].0).expect("step");
            assert_eq!(
                apply_forget(&w[0].1, w[0].0, step, M),
                ForgetOutcome::Applied {
                    text: w[1].1.clone(),
                    rev: w[1].0
                },
                "case {case}"
            );
        }
        let mut once = c.ledger.clone();
        scrub(&mut once, &all, Some(t), &texts);
        let mut twice = once.clone();
        let again = scrub(&mut twice, &all, Some(t), &texts);
        assert_eq!(twice, once, "case {case}: idempotent");
        assert!(
            !again.changed(),
            "case {case}: the second pass changes nothing"
        );
        // 指示を 1 つずつ足しながら当てる (書く前の掃除と、後の忘れるの掃除)。
        let mut step = c.ledger.clone();
        for k in 1..=c.directives.len() {
            scrub(&mut step, &set(c.directives[..k].to_vec()), Some(t), &texts);
        }
        assert_eq!(step, once, "case {case}: incremental == batch");
        // 指示を読む順に依らない。
        let mut reversed = c.directives.clone();
        reversed.reverse();
        let mut other_order = c.ledger.clone();
        scrub(&mut other_order, &set(reversed), Some(t), &texts);
        assert_eq!(other_order, once, "case {case}: order of reading");
        // どの忘れた値も、残った要素の文字列に部分文字列として残らない。
        for (_, b) in &c.directives {
            let f_keys: BTreeSet<ValueKey> =
                b.vks.iter().filter_map(|h| ValueKey::from_hex(h)).collect();
            let left: Vec<String> = once
                .output
                .units()
                .items
                .into_iter()
                .flat_map(|u| [u.value, u.old, u.aspect, u.entity, u.when])
                .chain(once.output.keys().items)
                .collect();
            let k = substring_keys_of(&K, left.iter().map(String::as_str));
            assert!(
                f_keys.iter().all(|x| !k.contains(x)),
                "case {case}: no forgotten value is kept"
            );
        }
        // 位置は最後の改訂の番号にそろう。
        let last = c.texts.last().expect("text").0;
        assert!(once.sentences.iter().all(|s| s.rev == last), "case {case}");
    }
}
