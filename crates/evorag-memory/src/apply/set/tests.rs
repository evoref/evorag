//! 指示の集合と当て方の試験 (04 §7.4・§7.5・§7.6、§5.2 の関門 8、§6)。

use super::*;
use crate::directive::DirectiveRedaction;
use crate::value_key::aspect_key;
use evorag_store::forget::Redaction;

const K: ForgetKey = ForgetKey::from_bytes_const([3; 32]);
const M: &str = "[伏せ字]";

/// 主語を明示する語 (試験の規則の版 1)。
const SELF_V1: &[&str] = &["私", "me"];
/// 規則の版を上げて「あたし」を足したもの。
const SELF_V2: &[&str] = &["私", "me", "あたし"];

fn resolver(words: &'static [&'static str]) -> impl Fn(&str) -> String {
    move |raw: &str| {
        let n = normalize_v1(raw);
        if words.contains(&n.as_str()) {
            "self".to_owned()
        } else {
            n
        }
    }
}

/// 時刻の順に並ぶ ID (バイト列の順で比べられればよい)。
fn id(n: u64) -> Id {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    Id::from_bytes(b)
}

fn mac(aspect: &str) -> String {
    aspect_key(&K, aspect).to_hex()
}

fn body(op: DirectiveOp, entity: &str, aspect: ProbeAspect<'_>, value: &str) -> DirectiveBody {
    let mut b = DirectiveBody::new(op);
    b.entity = Some(entity.to_owned());
    match aspect {
        ProbeAspect::Dict { name, .. } => b.aspect = Some(name.to_owned()),
        ProbeAspect::Free { mac } => b.aspect_mac = Some(mac.to_owned()),
    }
    b.vks = ValueKeys::of(&K, value).to_hex();
    b
}

fn forget(entity: &str, aspect: ProbeAspect<'_>, value: &str) -> DirectiveBody {
    body(DirectiveOp::Forget, entity, aspect, value)
}

fn cancel(op: DirectiveOp, target: Id) -> DirectiveBody {
    let mut b = DirectiveBody::new(op);
    b.target = Some(target);
    b
}

fn dict(name: &str) -> ProbeAspect<'_> {
    ProbeAspect::Dict {
        name,
        was_free: None,
    }
}

fn probe<'a>(entity: &str, aspect: ProbeAspect<'a>, value: &str, turn: u64) -> Probe<'a> {
    Probe::new(
        ResolvedEntity::new(entity, resolver(SELF_V1)),
        aspect,
        ValueKeys::of(&K, value),
        id(turn),
    )
}

fn set(ds: Vec<(u64, DirectiveBody)>) -> DirectiveSet {
    DirectiveSet::new(
        ds.into_iter().map(|(n, b)| (id(n), b)),
        M,
        resolver(SELF_V1),
    )
}

/// verifies: 04 §7.5 (当て方の述語。値の鍵か照合の鍵・entity の同値類・辞書の属性どうしは属性も比べる)、§5.2 の関門 8
#[test]
fn the_forget_predicate_compares_keys_entity_and_dictionary_attribute() {
    let s = set(vec![(10, forget("self", dict("residence"), "7月22日"))]);
    let hit = |p: &Probe<'_>| s.forgotten_by(p);
    assert_eq!(
        hit(&probe("self", dict("residence"), "7月22日", 1)),
        Some(id(10))
    );
    assert_eq!(
        hit(&probe("self", dict("residence"), "7 月 22 日", 1)),
        Some(id(10))
    );
    assert_eq!(
        hit(&probe("self", dict("residence"), "７月２２日", 20)),
        Some(id(10))
    );
    // 主語を「私」と書いた候補も、解くと self なので当たる。
    assert_eq!(
        hit(&probe("私", dict("residence"), "7月22日", 1)),
        Some(id(10))
    );
    // 別の辞書の属性の同じ値には当たらない。
    assert_eq!(hit(&probe("self", dict("birthday"), "7月22日", 1)), None);
    // 自由記述の属性の相手には、辞書の属性の指示も属性を見ずに当てる。
    let m = mac("記念日");
    assert_eq!(
        hit(&probe("self", ProbeAspect::Free { mac: &m }, "7月22日", 1)),
        Some(id(10))
    );
    assert_eq!(hit(&probe("妻", dict("residence"), "7月22日", 1)), None);
    assert_eq!(hit(&probe("self", dict("residence"), "7月23日", 1)), None);
}

/// verifies: 04 §7.5 (自由記述の属性の指示は属性を見ない)、§7.6
#[test]
fn a_free_attribute_forget_ignores_the_attribute() {
    let m = mac("よく行く店");
    let s = set(vec![(
        10,
        forget("self", ProbeAspect::Free { mac: &m }, "喫茶ミモザ"),
    )]);
    let other = mac("行きつけの喫茶店");
    let free = probe("self", ProbeAspect::Free { mac: &other }, "喫茶ミモザ", 1);
    assert!(s.forgotten_by(&free).is_some());
    assert!(
        s.forgotten_by(&probe("self", dict("likes"), "喫茶ミモザ", 1))
            .is_some()
    );
    let longer = probe("self", dict("likes"), "喫茶ミモザのナポリタン", 1);
    assert!(s.forgotten_by(&longer).is_none());
}

/// verifies: 04 §7.6 (entity の同値類: 正規化 v1 か照合の形で同じ呼び名と別名。主語を明示する語は今の規則で self とみなし、規則の版を上げても当たる相手が縮まない)
#[test]
fn the_entity_class_uses_the_current_rules_and_never_shrinks() {
    let mut b = forget("田中 さん", dict("hobby"), "将棋");
    b.aliases = vec!["課長".to_owned()];
    let s = set(vec![(10, b)]);
    assert!(
        s.forgotten_by(&probe("田中さん", dict("hobby"), "将棋", 1))
            .is_some()
    );
    assert!(
        s.forgotten_by(&probe("課長", dict("hobby"), "将棋", 1))
            .is_some()
    );
    assert!(
        s.forgotten_by(&probe("ＴＡＮＡＫＡ", dict("hobby"), "将棋", 1))
            .is_none()
    );
    // 規則の版 1 のときに「あたし」を主語にして書いた忘れる・private (entity は「あたし」のまま) と、self の忘れる。
    let old_forget = forget("あたし", dict("hobby"), "囲碁");
    let mut old_private = body(DirectiveOp::Private, "あたし", dict("dietary"), "えび");
    old_private.scope = Some("value_only".to_owned());
    let self_forget = forget("self", dict("hobby"), "将棋");
    let ds = vec![(10, old_forget), (11, old_private), (12, self_forget)];
    // 規則の版を上げて「あたし」を主語を明示する語に足した後の新しい候補は、self に解かれる。
    let v2 = DirectiveSet::new(
        ds.into_iter().map(|(n, b)| (id(n), b)),
        M,
        resolver(SELF_V2),
    );
    let p = |aspect: &'static str, value: &str| {
        Probe::new(
            ResolvedEntity::new("あたし", resolver(SELF_V2)),
            dict(aspect),
            ValueKeys::of(&K, value),
            id(30),
        )
    };
    assert_eq!(p("hobby", "囲碁").entity().as_str(), "self");
    assert_eq!(v2.forgotten_by(&p("hobby", "囲碁")), Some(id(10)));
    assert_eq!(v2.forgotten_by(&p("hobby", "将棋")), Some(id(12)));
    let unit = p("dietary", "えび");
    assert_eq!(
        v2.private_hit(&UnitProbe {
            probe: &unit,
            anchors: &[],
        }),
        Some((id(11), PrivateHit::Value))
    );
    // 書いたときの形でも当たり続ける。
    let raw = Probe::new(
        ResolvedEntity::new("あたし", resolver(SELF_V1)),
        dict("hobby"),
        ValueKeys::of(&K, "囲碁"),
        id(30),
    );
    assert_eq!(v2.forgotten_by(&raw), Some(id(10)));
}

/// verifies: 04 §7.5 (unblock は根拠のターンの ID で決める。最も早い unblock。解く相手の種類が違う・相手より ID が小さい解く指示は当てない)
#[test]
fn unblock_releases_only_later_evidence() {
    let s = set(vec![
        (10, forget("self", dict("name"), "ハッカ")),
        (20, cancel(DirectiveOp::Unblock, id(10))),
    ]);
    assert_eq!(s.forgets()[0].unblocked(), Some(id(20)));
    assert!(
        s.forgotten_by(&probe("self", dict("name"), "ハッカ", 15))
            .is_some()
    );
    assert!(
        s.forgotten_by(&probe("self", dict("name"), "ハッカ", 25))
            .is_none()
    );
    let s = set(vec![
        (10, forget("self", dict("name"), "ハッカ")),
        (30, cancel(DirectiveOp::Unblock, id(10))),
        (20, cancel(DirectiveOp::Unblock, id(10))),
    ]);
    assert_eq!(s.forgets()[0].unblocked(), Some(id(20)));
    let s = set(vec![
        (10, forget("self", dict("name"), "ハッカ")),
        (
            11,
            body(DirectiveOp::Private, "self", dict("name"), "ハッカ"),
        ),
        (20, cancel(DirectiveOp::PrivateOff, id(10))),
        (21, cancel(DirectiveOp::Unblock, id(11))),
        (22, cancel(DirectiveOp::Unblock, id(99))),
        // 時計が戻って、相手より ID の小さい解く指示。
        (5, cancel(DirectiveOp::Unblock, id(10))),
        (6, cancel(DirectiveOp::PrivateOff, id(11))),
    ]);
    assert_eq!(s.forgets()[0].unblocked(), None);
    assert_eq!(s.privates().len(), 1);
    assert_eq!(s.counts().unmatched_cancels, 5);
    // 忘れる自身の根拠 (unblock より前のターン) は解けない。
    assert!(
        s.forgotten_by(&probe("self", dict("name"), "ハッカ", 8))
            .is_some()
    );
}

fn private(value: &str, scope: PrivateScope, at: Option<TurnSpan>) -> DirectiveBody {
    let mut b = body(DirectiveOp::Private, "self", dict("dietary"), value);
    b.scope = Some(scope.name().to_owned());
    b.at = at;
    b
}

fn span(turn: u64, rev: u32, start: u32, end: u32) -> TurnSpan {
    TurnSpan {
        turn: id(turn),
        rev,
        start,
        end,
    }
}

/// verifies: 04 §7.5 (private は vks か根拠の位置で当たり、「後継まで」は後に言われた同じ (entity, 辞書の属性) の値にも当たる。private_off で解く)
#[test]
fn private_hits_by_value_position_and_successor() {
    let s = set(vec![(
        10,
        private("くるみ", PrivateScope::Successors, Some(span(1, 0, 0, 9))),
    )]);
    let unit = |value: &str, turn: u64, anchors: &[TurnSpan]| {
        let p = probe("self", dict("dietary"), value, turn);
        s.private_hit(&UnitProbe { probe: &p, anchors })
    };
    assert_eq!(
        unit("くるみ", 1, &[span(1, 0, 0, 9)]),
        Some((id(10), PrivateHit::Value))
    );
    assert_eq!(
        unit("胡桃", 1, &[span(1, 0, 0, 9)]),
        Some((id(10), PrivateHit::Position))
    );
    assert_eq!(unit("胡桃", 1, &[span(1, 0, 0, 6)]), None);
    assert_eq!(
        unit("えび", 30, &[span(30, 0, 0, 6)]),
        Some((id(10), PrivateHit::Successor))
    );
    assert_eq!(unit("えび", 5, &[span(5, 0, 0, 6)]), None);
    let p = &s.privates()[0];
    assert_eq!(
        (p.imported(), p.entity(), p.aspect()),
        (
            false,
            Some("self"),
            &DirectiveAspect::Dict("dietary".to_owned())
        )
    );
    let only = set(vec![(10, private("くるみ", PrivateScope::ValueOnly, None))]);
    let p = probe("self", dict("dietary"), "えび", 30);
    assert_eq!(
        only.private_hit(&UnitProbe {
            probe: &p,
            anchors: &[]
        }),
        None
    );
    let off = set(vec![
        (10, private("くるみ", PrivateScope::Successors, None)),
        (20, cancel(DirectiveOp::PrivateOff, id(10))),
    ]);
    assert!(off.privates().is_empty());
}

/// verifies: 04 §7.5 (取り込んだ指示の位置は範囲が重なれば当たる)、§7.3 (根拠の位置は後の伏せ字で写して比べる)
#[test]
fn positions_follow_later_redactions_and_imported_positions_overlap() {
    let text = "abc secret def くるみ";
    let (r, _) = Redaction::plan(text, 0, std::iter::once(4..10), M).expect("plan");
    let mut f = forget("self", dict("hobby"), "secret");
    f.redactions = vec![DirectiveRedaction {
        turn: id(1),
        rev_before: 0,
        rev_after: 1,
        start: 4,
        end: 10,
        hash: r.hash().to_string(),
    }];
    let kurumi = u32::try_from(text.find("くるみ").expect("in text")).expect("fits");
    let at = span(1, 0, kurumi, kurumi + 9);
    let s = set(vec![
        (5, private("胡桃", PrivateScope::ValueOnly, Some(at))),
        (10, f),
    ]);
    let shift = u32::try_from(M.len()).expect("fits") - 6;
    let after = span(1, 1, kurumi + shift, kurumi + shift + 9);
    let p = probe("self", dict("dietary"), "くるみ", 1);
    assert_eq!(
        s.private_hit(&UnitProbe {
            probe: &p,
            anchors: &[after]
        }),
        Some((id(5), PrivateHit::Position))
    );
    let mut imported = private(
        "胡桃",
        PrivateScope::ValueOnly,
        Some(span(1, 0, kurumi - 2, kurumi + 3)),
    );
    imported.imported = true;
    let s = set(vec![(5, imported)]);
    assert!(s.privates()[0].imported());
    assert_eq!(
        s.private_hit(&UnitProbe {
            probe: &p,
            anchors: &[span(1, 0, kurumi, kurumi + 9)]
        }),
        Some((id(5), PrivateHit::Position))
    );
}

/// verifies: 04 §7.4 (end は entity を今の規則で解き直し、同じ組の vks に当たる値だけを終える。辞書の写しの前の鍵とも比べる)
#[test]
fn end_hits_its_group_and_the_value_keys_only() {
    let free = mac("住まい");
    let mut e = body(
        DirectiveOp::End,
        "わたし",
        ProbeAspect::Free { mac: &free },
        "港町",
    );
    e.entity_raw = Some("私".to_owned());
    e.turn = Some(id(7));
    let s = set(vec![(10, e)]);
    let end = &s.ends()[0];
    assert_eq!(end.position(), id(7));
    let written = probe("self", ProbeAspect::Free { mac: &free }, "港町", 1);
    assert!(end.hits(&written));
    let mapped = probe(
        "self",
        ProbeAspect::Dict {
            name: "residence",
            was_free: Some(&free),
        },
        "港町",
        1,
    );
    assert!(end.hits(&mapped));
    assert!(!end.hits(&probe("self", ProbeAspect::Free { mac: &free }, "山町", 1)));
    assert!(!end.hits(&probe("self", dict("residence"), "港町", 1)));
}

/// verifies: 04 §7.5 (項目を欠く指示: 忘れる・private は止めすぎる側に倒し、end・採る・戻すは当てずに数える)
#[test]
fn incomplete_directives_are_handled_by_kind() {
    let mut f = forget("self", dict("hobby"), "囲碁");
    f.entity = None;
    f.aspect = None;
    let mut p = private("えび", PrivateScope::Successors, None);
    p.aspect = None;
    let mut e = body(DirectiveOp::End, "self", dict("hobby"), "将棋");
    e.aspect = None;
    let mut a = body(DirectiveOp::Accept, "self", dict("hobby"), "将棋");
    a.entity = None;
    let s = set(vec![(10, f), (11, p), (12, e), (13, a)]);
    assert!(
        s.forgotten_by(&probe("妻", dict("event"), "囲碁", 1))
            .is_some()
    );
    // 属性の無い private は、どの辞書の属性の後継にも当たる。
    let later = probe("self", dict("likes"), "からあげ", 30);
    assert_eq!(
        s.private_hit(&UnitProbe {
            probe: &later,
            anchors: &[]
        }),
        Some((id(11), PrivateHit::Successor))
    );
    assert!(s.ends().is_empty());
    assert!(s.adoptions().is_empty());
    assert_eq!(s.counts().incomplete, 2);
}

/// verifies: 04 §6 (訂正の old・new の鍵が忘れるの vks にあれば forgotten。entity と属性は問わない)
#[test]
fn corrections_are_forgotten_by_keys_alone() {
    let s = set(vec![
        (10, forget("妻", dict("hometown"), "霧ヶ浦")),
        (20, cancel(DirectiveOp::Unblock, id(10))),
    ]);
    let new = ValueKeys::of(&K, "霧ヶ浦");
    let old = ValueKeys::of(&K, "白浜");
    assert_eq!(s.forgotten_correction(&[new, old], id(15)), Some(id(10)));
    assert_eq!(s.forgotten_correction(&[old], id(15)), None);
    // 印を解いた後の発話の訂正には当てない。
    assert_eq!(s.forgotten_correction(&[new], id(25)), None);
}

/// verifies: 04 §7.5 (読めない vks と、正規化の版の違う指示を数える)
#[test]
fn unreadable_keys_and_other_norms_are_counted() {
    let mut upper = forget("self", dict("hobby"), "将棋");
    upper.vks.push(upper.vks[0].to_uppercase());
    let mut v2 = forget("self", dict("hobby"), "囲碁");
    v2.norm = Some(2);
    let mut unknown = forget("self", dict("hobby"), "碁");
    unknown.op = "pin".to_owned();
    let s = set(vec![(10, upper), (11, v2), (12, unknown)]);
    let c = s.counts();
    assert_eq!((c.unreadable_keys, c.other_norm, c.unknown), (1, 1, 1));
    // 読めた要素では当たる。
    assert!(
        s.forgotten_by(&probe("self", dict("hobby"), "将棋", 1))
            .is_some()
    );
}

/// verifies: 04 §5.3 (候補・採る・戻す・end の列の位置。end はそのターンの候補の後、採る・戻すは指示の ID の位置)
#[test]
fn the_timeline_orders_candidates_adoptions_and_ends() {
    let accept = body(DirectiveOp::Accept, "self", dict("residence"), "東京");
    let mut end_at_turn = body(DirectiveOp::End, "self", dict("residence"), "大阪");
    end_at_turn.turn = Some(id(5));
    let mut end_self = end_at_turn.clone();
    end_self.turn = None;
    let s = set(vec![(30, end_self), (20, accept), (40, end_at_turn)]);
    let mut all: Vec<Slot> = s.timeline().iter().map(|(sl, _)| *sl).collect();
    all.extend([5, 20, 25, 30].map(|n| Slot::candidates(id(n))));
    all.sort();
    let order: Vec<(Id, Phase, bool)> = all
        .iter()
        .map(|s| (s.at, s.phase, s.directive.is_some()))
        .collect();
    assert_eq!(
        order,
        [
            (id(5), Phase::Candidates, false),
            (id(5), Phase::AfterCandidates, true),
            (id(20), Phase::Candidates, false),
            (id(20), Phase::Candidates, true),
            (id(25), Phase::Candidates, false),
            (id(30), Phase::Candidates, false),
            (id(30), Phase::AfterCandidates, true),
        ]
    );
    let a = &s.adoptions()[0];
    let p = probe("self", dict("residence"), "東京", 1);
    assert_eq!(
        a.hits(
            &UnitProbe {
                probe: &p,
                anchors: &[]
            },
            s.redactions()
        ),
        Some(AdoptHit::Value)
    );
    let other = probe("self", dict("hometown"), "東京", 1);
    assert_eq!(
        a.hits(
            &UnitProbe {
                probe: &other,
                anchors: &[]
            },
            s.redactions()
        ),
        None
    );
}

/// verifies: 04 §5.5 (private の vks か、導出で private の印を付けた単位の鍵に、語句か部分文字列が当たる区切りの鍵を除く)
#[test]
fn private_screens_keys_by_substrings() {
    let s = set(vec![(
        10,
        private("くるみ", PrivateScope::Successors, None),
    )]);
    let none = BTreeSet::new();
    let keys = |t: &str| SubstringKeys::of(&K, t);
    assert!(s.screens_key(&keys("くるみの代わり"), &none));
    assert!(!s.screens_key(&keys("食べられないもの"), &none));
    // 後継で private になった単位 (値「えび」) の鍵も除く。
    let ebi = ValueKeys::of(&K, "えび");
    let units: BTreeSet<ValueKey> = [ebi.vk, ebi.loose].into();
    assert!(s.screens_key(&keys("えびの代わり"), &units));
    assert!(!set(vec![]).screens_key(&keys("くるみの代わり"), &none));
}
