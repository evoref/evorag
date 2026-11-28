//! 導出の決定性の性質の試験 (09 §3.5.1 の「導出の決定性 (プロパティの試験)」、04 §5.3 の「全件の導出を定義に
//! する」「到着の順に依らない」「済んだ候補」「関門の結果の覚え」)。
//!
//! 種から作った世界 (複数のセッションの区切りと台帳。単値・多値の辞書の属性、自由記述の属性、出来事、時制と叙法の
//! 違い、訂正の判定、他人の主語、長い一度の入力、関門で落ちる候補、指示 `forget`・`end`・`private`・`private_off`・
//! `accept`・`revert`・`unblock`) を、公開の API (`derive::derive`・`derive::fingerprint`) で導き、次を確かめる。
//!
//! - (a) 同じ入力 (別々に作り直した世界) から、単位・数えた理由・指紋がバイト列まで同じ。
//! - (b) 台帳を渡す順・指示を渡す順・台帳の中の候補と判定の順・台帳の ID (書かれた順) を乱しても、単位と数えた理由が
//!   同じ。台帳の成分を渡す順を乱しても指紋が同じ。
//! - (c) 同じ候補を台帳の中で 2 回渡しても、単位は 1 回のときと同じ (済んだ候補)。
//! - (d) 覚え (`GateCache`) が空でも、同じ入力で温めても、ほかの入力 (台帳の一部・別の指示・乱した順) で温めても、
//!   単位と数えた理由が同じ。
//! - (e) 当て終えた指示 (`forget`・`end`・`private`・`accept`・`revert`) を、その直後にもう一度書いても単位が同じ。
//!
//! 件数は既定で 64 (debug の構成で 1 件の導出に数十 ms かかるため。`EVORAG_PROPERTY_CASES` で変える)。種は 1 から (`EVORAG_PROPERTY_SEED` で始めの種を変える)。
//! 落ちたときは種を出すので、`EVORAG_PROPERTY_SEED=<種> EVORAG_PROPERTY_CASES=1` で 1 件だけ流し直せる。

mod derive_support;

use std::collections::BTreeSet;

use derive_support::{
    Cand, Rng, World, counts_bytes, id_at, just_after, key, summary, unit, units_bytes, with,
};
use evorag_memory::b3::{Reason, Rules};
use evorag_memory::derive::{CorrectionReason, Derived, FoldNote, GateCache};
use evorag_memory::{
    DirectiveBody, DirectiveOp, PrivateScope, TurnSpan, UnitLayer, UnitState, ValueKeys, aspect_key,
};
use evorag_types::Id;
use serde_json::json;

const PLACES: [&str; 5] = ["港南台", "汐見坂", "霧笛町", "北町", "南浜"];
const JOBS: [&str; 3] = ["司書", "庭師", "測量士"];
const HOBBIES: [&str; 4] = ["将棋", "囲碁", "登山", "陶芸"];
const PETS: [&str; 3] = ["インコ", "金魚", "亀"];
const COLORS: [&str; 3] = ["藍色", "若草色", "朱色"];
const ACTS: [&str; 3] = ["バーベキュー", "花火大会", "潮干狩り"];
const WHENS: [&str; 4] = ["昨日", "先週の土曜日", "3月3日", "この前"];
const CHAT: [&str; 3] = [
    "今日は少し肌寒いですね。",
    "近所の公園まで散歩に出かけました。",
    "夕方から雨が降りそうです。",
];
const OTHER: &str = "田中さん";
const FREE_COLOR: &str = "好きな色";

fn cases() -> u64 {
    std::env::var("EVORAG_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

fn first_seed() -> u64 {
    std::env::var("EVORAG_PROPERTY_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

fn seeds() -> impl Iterator<Item = u64> {
    let start = first_seed();
    start..start + cases()
}

/// 言った値 (指示の相手に使う)。
#[derive(Debug, Clone)]
struct Said {
    entity: &'static str,
    /// 辞書の属性の名前か、自由記述の属性の名前。
    aspect: &'static str,
    free: bool,
    value: String,
    turn: Id,
    /// 発話の本文の中の値の範囲。
    span: (usize, usize),
}

/// 1 つの文の型: 本文と、その文の候補 (wire の形)・言った値。
struct Piece {
    text: String,
    units: Vec<serde_json::Value>,
    said: Vec<(&'static str, &'static str, bool, String)>,
}

fn piece(r: &mut Rng) -> Piece {
    let p = (*r.pick(&PLACES)).to_owned();
    let q = (*r.pick(&PLACES)).to_owned();
    let h = (*r.pick(&HOBBIES)).to_owned();
    let said = |e, a, f, v: &str| vec![(e, a, f, v.to_owned())];
    let res = "residence";
    match r.below(20) {
        0 => Piece {
            text: format!("私は{p}に住んでいます。"),
            units: vec![unit(res, &p)],
            said: said("self", res, false, &p),
        },
        1 => Piece {
            text: format!("{p}に住んでるよ。"),
            units: vec![unit(res, &p)],
            said: said("self", res, false, &p),
        },
        2 => Piece {
            text: format!("{p}に引っ越しました。"),
            units: vec![unit(res, &p)],
            said: said("self", res, false, &p),
        },
        3 => Piece {
            text: format!("以前は{p}に住んでいました。"),
            units: vec![with(unit(res, &p), "tn", "past")],
            said: said("self", res, false, &p),
        },
        4 => Piece {
            text: format!("もう{p}には住んでいません。"),
            units: vec![with(unit(res, &p), "m", "negated")],
            said: said("self", res, false, &p),
        },
        5 => Piece {
            text: format!("来月{p}に引っ越す予定です。"),
            units: vec![with(unit(res, &p), "tn", "planned")],
            said: said("self", res, false, &p),
        },
        6 => Piece {
            text: format!("{p}に住んでいるのでしょうか？"),
            units: vec![with(unit(res, &p), "m", "question")],
            said: Vec::new(),
        },
        7 => Piece {
            text: format!("{p}に住んでいたらいいのに。"),
            units: vec![with(unit(res, &p), "m", "hypothetical")],
            said: Vec::new(),
        },
        8 => {
            let j = *r.pick(&JOBS);
            Piece {
                text: format!("私の仕事は{j}です。"),
                units: vec![unit("occupation", j)],
                said: said("self", "occupation", false, j),
            }
        }
        9 => Piece {
            text: format!("趣味は{h}です。"),
            units: vec![unit("hobby", &h)],
            said: said("self", "hobby", false, &h),
        },
        10 => {
            let h0 = *r.pick(&HOBBIES);
            Piece {
                text: format!("趣味は{h0}から{h}に変わりました。"),
                units: vec![with(unit("hobby", &h), "old", h0)],
                said: said("self", "hobby", false, &h),
            }
        }
        11 => {
            let x = *r.pick(&PETS);
            Piece {
                text: format!("{x}を飼っています。"),
                units: vec![unit("pet", x)],
                said: said("self", "pet", false, x),
            }
        }
        12 => {
            let c = *r.pick(&COLORS);
            Piece {
                text: format!("好きな色は{c}です。"),
                units: vec![unit(FREE_COLOR, c)],
                said: said("self", FREE_COLOR, true, c),
            }
        }
        13 => {
            let c = *r.pick(&COLORS);
            Piece {
                text: format!("昔好きだった色は{c}です。"),
                units: vec![with(unit(FREE_COLOR, c), "tn", "past")],
                said: said("self", FREE_COLOR, true, c),
            }
        }
        14 => {
            let w = *r.pick(&WHENS);
            let a = *r.pick(&ACTS);
            Piece {
                text: format!("{w}、海辺で{a}をしました。"),
                units: vec![with(with(unit("したこと", a), "l", "episode"), "when", w)],
                said: Vec::new(),
            }
        }
        15 => {
            // 訂正の字句の型と、同じ穴の端の 2 つの候補。
            let mut units = vec![unit(res, &q)];
            if r.chance(1, 2) {
                units.push(unit(res, &format!("{q}駅")));
            }
            Piece {
                text: format!("いや、{p}じゃなくて{q}駅の近くに住んでいます。"),
                units,
                said: said("self", res, false, &q),
            }
        }
        16 => Piece {
            text: format!("{OTHER}は{p}に住んでいます。"),
            units: vec![with(unit(res, &p), "e", OTHER)],
            said: said(OTHER, res, false, &p),
        },
        17 => Piece {
            text: format!("{p}に住んでいて、趣味は{h}です。"),
            units: vec![unit(res, &p), unit("hobby", &h)],
            said: [
                said("self", res, false, &p),
                said("self", "hobby", false, &h),
            ]
            .concat(),
        },
        18 => Piece {
            // 値が根拠の文に無い候補 (関門 2 で落ちる)。
            text: format!("私は{p}に住んでいます。"),
            units: vec![unit(res, &format!("{q}丘"))],
            said: Vec::new(),
        },
        _ => Piece {
            // 言い換えの多い単値の属性の言い直し (言い直しの数の条件)。
            text: format!("やっぱり{p}に住んでいます。"),
            units: vec![unit(res, &p)],
            said: said("self", res, false, &p),
        },
    }
}

fn vks(value: &str) -> Vec<String> {
    ValueKeys::of(&key(), value).to_hex()
}

/// 言った値 `s` を相手にする指示 (`op`)。
fn directive_for(rules: &Rules<'_>, op: DirectiveOp, s: &Said) -> DirectiveBody {
    let mut b = DirectiveBody::new(op);
    b.entity = Some(rules.resolve_entity(s.entity));
    if s.free {
        b.aspect_mac = Some(aspect_key(&key(), s.aspect).to_hex());
    } else {
        b.aspect = Some(s.aspect.to_owned());
    }
    b.vks = vks(&s.value);
    b.norm = Some(1);
    b
}

/// 種 `seed` の世界。
fn random_world(rules: &Rules<'_>, seed: u64) -> World {
    let mut r = Rng::new(seed);
    let mut w = World::default();
    let sessions = 1 + r.below(3);
    let segments = 2 + r.below(5);
    let mut said: Vec<Said> = Vec::new();
    let mut forgets: Vec<Id> = Vec::new();
    let mut privates: Vec<Id> = Vec::new();
    for _ in 0..segments {
        let session = u16::try_from(r.below(sessions)).expect("fits");
        let tz = *r.pick(&[Some(540), Some(540), None, Some(-300)]);
        let pairs = 1 + r.below(3);
        let mut turns = Vec::new();
        let mut cands = Vec::new();
        for k in 0..pairs {
            let mut text = String::new();
            let mut local: Vec<(usize, Piece)> = Vec::new();
            if r.chance(1, 3) {
                text.push_str(r.pick(&CHAT));
            }
            for _ in 0..1 + r.below(2) {
                let p = piece(&mut r);
                local.push((text.len(), p));
                text.push_str(&local.last().expect("pushed").1.text);
            }
            if r.chance(1, 4) {
                text.push_str(r.pick(&CHAT));
            }
            let long = r.chance(1, 10);
            let t = w.say(session, &text, tz, long);
            turns.push(t);
            for (at, p) in local {
                for u in p.units {
                    cands.push(Cand {
                        turn: k,
                        at,
                        unit: u,
                    });
                }
                for (entity, aspect, free, value) in p.said {
                    let start = at + p.text.find(value.as_str()).expect("the value is said");
                    said.push(Said {
                        entity,
                        aspect,
                        free,
                        span: (start, start + value.len()),
                        value,
                        turn: t,
                    });
                }
            }
            if r.chance(2, 3) {
                let p = r.pick(&PLACES);
                let answer = if r.chance(1, 2) {
                    format!("{p}はいいところですね。")
                } else {
                    "なるほど、よく分かりました。".to_owned()
                };
                w.answer(session, &answer);
            }
        }
        r.shuffle(&mut cands);
        let mut vr = r.clone();
        w.ledger(&turns, &cands, |_| match vr.below(5) {
            0 | 1 => Some("accept"),
            2 => Some("reject"),
            3 => Some("unclear"),
            _ => None,
        });
        r.next();
        if r.chance(1, 8) {
            // `corr` に無い判定 (`unknown_id`)。
            let l = w.ledgers.last_mut().expect("a ledger");
            l.verdicts.push(json!({"id": 99, "verdict": "accept"}));
            l.rewrite();
        }
        // 指示 (区切りの後、0〜2 件)。
        for _ in 0..r.below(3) {
            if said.is_empty() {
                break;
            }
            let s = r.pick(&said).clone();
            match r.below(9) {
                0 => {
                    let b = directive_for(rules, DirectiveOp::Forget, &s);
                    forgets.push(w.directive(b));
                }
                1 | 2 => {
                    let mut b = directive_for(rules, DirectiveOp::End, &s);
                    b.entity_raw = Some(s.entity.to_owned());
                    b.turn = Some(s.turn);
                    w.directive(b);
                }
                3 => {
                    let mut b = directive_for(rules, DirectiveOp::Private, &s);
                    let scope = *r.pick(&[PrivateScope::ValueOnly, PrivateScope::Successors]);
                    b.scope = Some(scope.name().to_owned());
                    privates.push(w.directive(b));
                }
                4..=6 => {
                    let op = *r.pick(&[DirectiveOp::Accept, DirectiveOp::Revert]);
                    let mut b = directive_for(rules, op, &s);
                    if r.chance(1, 2) {
                        b.at = Some(TurnSpan {
                            turn: s.turn,
                            rev: 0,
                            start: u32::try_from(s.span.0).expect("fits"),
                            end: u32::try_from(s.span.1).expect("fits"),
                        });
                    }
                    w.directive(b);
                }
                7 if !privates.is_empty() => {
                    let mut b = DirectiveBody::new(DirectiveOp::PrivateOff);
                    b.target = Some(*r.pick(&privates));
                    w.directive(b);
                }
                8 if !forgets.is_empty() => {
                    let mut b = DirectiveBody::new(DirectiveOp::Unblock);
                    b.target = Some(*r.pick(&forgets));
                    w.directive(b);
                }
                _ => {}
            }
        }
    }
    w
}

/// 単位と数えた理由が同じこと (覚えの当たりは比べない)。
#[track_caller]
fn assert_same(got: &Derived, want: &Derived, what: &str, seed: u64) {
    assert!(
        units_bytes(got) == units_bytes(want),
        "{what}: units differ (EVORAG_PROPERTY_SEED={seed} EVORAG_PROPERTY_CASES=1)\n got: {:#?}\nwant: {:#?}",
        summary(got),
        summary(want)
    );
    assert!(
        counts_bytes(got) == counts_bytes(want),
        "{what}: counts differ (EVORAG_PROPERTY_SEED={seed} EVORAG_PROPERTY_CASES=1)\n got: {:?}\nwant: {:?}",
        got.counts,
        want.counts
    );
}

#[track_caller]
fn assert_same_units(got: &Derived, want: &Derived, what: &str, seed: u64) {
    assert!(
        units_bytes(got) == units_bytes(want),
        "{what}: units differ (EVORAG_PROPERTY_SEED={seed} EVORAG_PROPERTY_CASES=1)\n got: {:#?}\nwant: {:#?}",
        summary(got),
        summary(want)
    );
}

fn cold(rules: &Rules<'_>, w: &World) -> Derived {
    w.derive_with(rules, &mut GateCache::new())
}

/// 作った世界が、性質の試験の相手として広く当たっていること (関門を通る候補・訂正・指示・状態の別)。
#[test]
fn the_generator_covers_the_rules() {
    let rules = Rules::bundled();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for seed in seeds() {
        let d = cold(&rules, &random_world(&rules, seed));
        let c = &d.counts;
        let flags = [
            ("applied", c.correction(CorrectionReason::Applied) > 0),
            ("ended", c.folded(FoldNote::Ended) > 0),
            ("adopted", c.folded(FoldNote::Adopted) > 0),
            ("replaced", c.folded(FoldNote::Replaced) > 0),
            ("forgotten", c.gate(Reason::Forgotten) > 0),
            (
                "value_not_in_evidence",
                c.gate(Reason::ValueNotInEvidence) > 0,
            ),
            ("unknown_id", c.correction(CorrectionReason::UnknownId) > 0),
            ("private", d.units.iter().any(|u| u.body.private)),
            ("free", d.units.iter().any(|u| u.body.free)),
            (
                "episode",
                d.units.iter().any(|u| u.body.layer == UnitLayer::Episode),
            ),
            (
                "contested",
                d.units
                    .iter()
                    .any(|u| u.body.state == Some(UnitState::Contested)),
            ),
            (
                "superseded",
                d.units
                    .iter()
                    .any(|u| u.body.state == Some(UnitState::Superseded)),
            ),
            (
                "other_entity",
                d.units.iter().any(|u| u.body.entity != "self"),
            ),
        ];
        seen.extend(flags.iter().filter(|(_, b)| *b).map(|(n, _)| *n));
    }
    // 件数を減らして流したときは、すべてに当たらなくてもよい。
    if cases() >= 50 {
        let want: BTreeSet<&str> = [
            "applied",
            "ended",
            "adopted",
            "replaced",
            "forgotten",
            "value_not_in_evidence",
            "unknown_id",
            "private",
            "free",
            "episode",
            "contested",
            "superseded",
            "other_entity",
        ]
        .into();
        let missing: Vec<_> = want.difference(&seen).collect();
        assert!(missing.is_empty(), "the generator never hits {missing:?}");
    }
}

/// (a) 同じ入力から同じ結果 (別々に作り直した世界で、単位・数えた理由・指紋がバイト列まで同じ)。
///
/// verifies: AC-FR-315-1
#[test]
fn the_same_inputs_give_the_same_bytes() {
    let rules = Rules::bundled();
    for seed in seeds() {
        let a = random_world(&rules, seed);
        let b = random_world(&rules, seed);
        let da = cold(&rules, &a);
        let db = cold(&rules, &b);
        assert_same(&db, &da, "rebuilt world", seed);
        let order: Vec<usize> = (0..a.ledgers.len()).collect();
        assert_eq!(
            a.fingerprint_in(&rules, &order).as_bytes(),
            b.fingerprint_in(&rules, &order).as_bytes(),
            "fingerprint (EVORAG_PROPERTY_SEED={seed})"
        );
        // 同じ世界で 2 回目。
        assert_same(&cold(&rules, &a), &da, "second derivation", seed);
    }
}

/// (b) 読む順に依らない: 台帳を渡す順・指示を渡す順・台帳の中の候補と判定の順・台帳の ID。
///
/// verifies: AC-FR-315-1
#[test]
fn the_result_does_not_depend_on_the_reading_order() {
    let rules = Rules::bundled();
    for seed in seeds() {
        let w = random_world(&rules, seed);
        let base = cold(&rules, &w);
        let identity: Vec<usize> = (0..w.ledgers.len()).collect();
        let fp = w.fingerprint_in(&rules, &identity);
        let mut r = Rng::new(seed ^ 0x5EED_0F0D);
        for round in 0..2 {
            let mut order = identity.clone();
            r.shuffle(&mut order);
            let mut directives = w.directives.clone();
            r.shuffle(&mut directives);
            let got = w.derive_in(&rules, &mut GateCache::new(), &order, &directives);
            assert_same(
                &got,
                &base,
                &format!("round {round}: order {order:?}"),
                seed,
            );
            assert_eq!(
                w.fingerprint_in(&rules, &order),
                fp,
                "fingerprint, order {order:?} (EVORAG_PROPERTY_SEED={seed})"
            );

            // 台帳の中の候補と判定の順 (本体が変わるので、指紋は比べない)。
            let mut inner = w.clone();
            for l in &mut inner.ledgers {
                r.shuffle(&mut l.units);
                r.shuffle(&mut l.verdicts);
                l.rewrite();
            }
            let got = inner.derive_in(&rules, &mut GateCache::new(), &order, &directives);
            assert_same(&got, &base, &format!("round {round}: inner order"), seed);

            // 台帳の ID (書かれた順) を替える。ターンと指示の ID は変えない。
            let mut reid = w.clone();
            let mut used: BTreeSet<Id> = BTreeSet::new();
            for l in &mut reid.ledgers {
                loop {
                    let ms =
                        derive_support::T0 + i64::try_from(r.below(1 << 30)).expect("fits") * 7;
                    let id = id_at(ms, u16::try_from(r.below(0x7FFF)).expect("fits"));
                    if used.insert(id) {
                        l.id = id;
                        break;
                    }
                }
            }
            let got = reid.derive_with(&rules, &mut GateCache::new());
            assert_same(&got, &base, &format!("round {round}: ledger ids"), seed);
        }
    }
}

/// (c) 済んだ候補: 同じ候補を台帳の中で 2 回渡しても、単位は 1 回のときと同じ。
///
/// verifies: AC-FR-315-1
#[test]
fn the_same_candidate_twice_is_the_same_as_once() {
    let rules = Rules::bundled();
    for seed in seeds() {
        let w = random_world(&rules, seed);
        let base = cold(&rules, &w);
        let mut r = Rng::new(seed ^ 0xD0D0);
        let mut twice = w.clone();
        let mut copied = 0usize;
        for l in &mut twice.ledgers {
            let n = l.units.len();
            for i in 0..n {
                if r.chance(1, 2) {
                    let u = l.units[i].clone();
                    let at = r.below(l.units.len() + 1);
                    l.units.insert(at, u);
                    copied += 1;
                }
            }
            l.rewrite();
        }
        let got = cold(&rules, &twice);
        assert_same_units(&got, &base, &format!("{copied} copies"), seed);
        // 全部を 2 回 (同じ順で並べて) でも同じ。
        let mut all = w.clone();
        for l in &mut all.ledgers {
            let copy = l.units.clone();
            l.units.extend(copy);
            l.rewrite();
        }
        assert_same_units(&cold(&rules, &all), &base, "every candidate twice", seed);
    }
}

/// (d) 覚えの有る無しで結果が同じ (同じ入力で温めた覚え・ほかの入力で温めた覚え)。
///
/// verifies: AC-FR-315-1
#[test]
fn a_warm_cache_gives_the_same_result() {
    let rules = Rules::bundled();
    for seed in seeds() {
        let w = random_world(&rules, seed);
        let base = cold(&rules, &w);
        let lookups: usize = w.ledgers.iter().map(|l| l.units.len()).sum();
        assert_eq!(
            base.cache.hits + base.cache.misses,
            lookups,
            "lookups (EVORAG_PROPERTY_SEED={seed})"
        );

        // 同じ入力で温めた覚え。
        let mut cache = GateCache::new();
        let _ = w.derive_with(&rules, &mut cache);
        let warm = w.derive_with(&rules, &mut cache);
        assert_eq!(warm.cache.misses, 0, "EVORAG_PROPERTY_SEED={seed}");
        assert_same(&warm, &base, "warm on the same inputs", seed);
        assert!(cache.len() <= lookups, "EVORAG_PROPERTY_SEED={seed}");

        // ほかの入力で温めた覚え: 台帳の前半と指示の一部、乱した順、指示を除いた世界。
        let mut r = Rng::new(seed ^ 0xCAC4E);
        let mut cache = GateCache::new();
        let mut part = w.clone();
        part.ledgers.truncate(w.ledgers.len() / 2);
        part.directives.retain(|_| r.chance(1, 2));
        let _ = part.derive_with(&rules, &mut cache);
        assert_same(
            &w.derive_with(&rules, &mut cache),
            &base,
            "warm on a part",
            seed,
        );
        let mut order: Vec<usize> = (0..w.ledgers.len()).collect();
        r.shuffle(&mut order);
        let mut bare = w.clone();
        bare.directives.clear();
        let _ = bare.derive_in(&rules, &mut cache, &order, &[]);
        assert_same(
            &w.derive_with(&rules, &mut cache),
            &base,
            "warm without directives",
            seed,
        );
        // 温めた覚えで導いた、指示の無い世界も、覚えずに導いたものと同じ。
        let mut fresh = GateCache::new();
        let bare_cold = bare.derive_with(&rules, &mut fresh);
        assert_same(
            &bare.derive_with(&rules, &mut cache),
            &bare_cold,
            "bare world, warm",
            seed,
        );

        // 台帳を 1 つ替えた後 (候補の一部を落とす): 覚えを使っても、覚えずに導いたものと同じ。
        if !w.ledgers.is_empty() {
            let mut changed = w.clone();
            let i = r.below(changed.ledgers.len());
            let l = &mut changed.ledgers[i];
            l.units.retain(|_| r.chance(1, 2));
            l.rewrite();
            let mut cache = GateCache::new();
            let _ = w.derive_with(&rules, &mut cache);
            let warm = changed.derive_with(&rules, &mut cache);
            assert_same(&warm, &cold(&rules, &changed), "one ledger changed", seed);
        }
    }
}

/// (e) 当て終えた指示をすぐ後にもう一度書いても、単位が同じ (忘れる・`end`・private・採る・戻す)。
///
/// verifies: AC-FR-315-1
#[test]
fn a_directive_written_twice_is_the_same_as_once() {
    let rules = Rules::bundled();
    for seed in seeds() {
        let w = random_world(&rules, seed);
        let base = cold(&rules, &w);
        // 解く指示 (`private_off`・`unblock`) は相手の ID を 1 つだけ指すので、解かれた指示は 2 回書くと解かれない方が
        // 残る (1 回と同じにならないのが正しい)。解かれていない指示だけを 2 回書く。
        let cancelled: BTreeSet<Id> = w.directives.iter().filter_map(|(_, b)| b.target).collect();
        let mut twice = w.clone();
        for (id, b) in &w.directives {
            if cancelled.contains(id) {
                continue;
            }
            let repeatable = matches!(
                b.op(),
                DirectiveOp::Forget
                    | DirectiveOp::End
                    | DirectiveOp::Private
                    | DirectiveOp::Accept
                    | DirectiveOp::Revert
            );
            if repeatable {
                twice.directives.push((just_after(*id), b.clone()));
            }
        }
        if twice.directives.len() == w.directives.len() {
            continue;
        }
        assert_same_units(&cold(&rules, &twice), &base, "directives twice", seed);
    }
}
