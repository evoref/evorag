//! 導出の試験: 畳みの規則ごとの試験、決定性の性質の試験 (09 §3.5.1)、`import-v04` の `end`、1 万件の合成データの
//! ベンチ (`#[ignore]`)。

use std::collections::BTreeMap;
use std::ops::Range;

use serde_json::{Value, json};

use super::*;
use crate::apply::{DirectiveSet, ValueKeys};
use crate::b3::{Dictionary, Rules};
use crate::corrections::{CORRECTION_V1, find_holes};
use crate::directive::{DirectiveBody, DirectiveOp, PrivateScope};
use crate::import_v04::{EndedValue, V04Ended};
use crate::ledger::{ByteRange, CALL_SEGMENT, CorrInput, LedgerBody, LedgerOutput, SentenceSpan};
use crate::ledger_select::{Stage, StoredLedger};
use crate::prefilter::{MODE_CHAT, PREFILTER_V1, Screened, prefilter};
use crate::sentences::{SPLITTER_V1, split_v1};
use crate::unit::{AnchorKind, UnitBody, UnitState, UnitTense, WhenBasis};
use crate::value_key::{ForgetKey, aspect_key, normalize_v1};

const KEY: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

/// 2026-04-01T00:00:00Z。
const T0: i64 = 1_775_001_600_000;

/// 時刻 `ms` の UUID v7 の形の ID (`n` で同じ時刻の中を分ける)。
pub(super) fn id_at(ms: i64, n: u16) -> Id {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[7..9].copy_from_slice(&n.to_be_bytes());
    b[9] = 0x80;
    Id::from_bytes(b)
}

struct TurnData {
    session: Id,
    speaker: Speaker,
    text: String,
    tz: Option<i32>,
    long: Vec<Range<usize>>,
}

/// 試験の世界: ターン・台帳・指示。
#[derive(Default)]
struct World {
    turns: BTreeMap<Id, TurnData>,
    ledgers: Vec<(Id, LedgerBody)>,
    /// 台帳ごとの候補 (wire の形。順を乱すときに使う)。
    units: Vec<Vec<Value>>,
    directives: Vec<(Id, DirectiveBody)>,
    clock: i64,
    seq: u16,
}

fn u(s: u32, aspect: &str, value: &str) -> Value {
    json!({"s": s, "a": aspect, "v": value})
}

fn with(mut v: Value, k: &str, x: &str) -> Value {
    v[k] = json!(x);
    v
}

impl World {
    fn next_id(&mut self) -> Id {
        self.clock += 60_000;
        self.seq += 1;
        id_at(T0 + self.clock, self.seq)
    }

    fn say_in(&mut self, session: u8, text: &str) -> Id {
        let id = self.next_id();
        self.turns.insert(
            id,
            TurnData {
                session: id_at(0, u16::from(session)),
                speaker: Speaker::User,
                text: text.to_owned(),
                tz: Some(540),
                long: Vec::new(),
            },
        );
        id
    }

    fn say(&mut self, text: &str) -> Id {
        self.say_in(1, text)
    }

    fn answer(&mut self, text: &str) -> Id {
        let id = self.next_id();
        self.turns.insert(
            id,
            TurnData {
                session: id_at(0, 1),
                speaker: Speaker::Assistant,
                text: text.to_owned(),
                tz: Some(540),
                long: Vec::new(),
            },
        );
        id
    }

    /// 区切り 1 つの台帳 (往復の発話 `turns`、候補 `units`、訂正の判定 `corrections`)。文番号は区切りの中で 1 から。
    fn ledger_with(&mut self, turns: &[Id], units: &[Value], corrections: &[(u32, &str)]) -> Id {
        let mut sentences = Vec::new();
        let mut corr = Vec::new();
        let mut n = 1;
        for t in turns {
            let text = &self.turns[t].text;
            if let Some(h) = find_holes(MODE_CHAT, text) {
                corr.push(CorrInput {
                    id: u32::try_from(corr.len() + 1).expect("fits"),
                    turn: *t,
                    rev: 0,
                    pattern: h.pattern.to_owned(),
                    new: byte_range(&h.new),
                    old: h.old.as_ref().map(byte_range),
                    extra: Default::default(),
                });
            }
            let Screened::Send(f) = prefilter(MODE_CHAT, text) else {
                continue;
            };
            for r in split_v1(f.text()) {
                let stored = f.to_stored(r).expect("one piece");
                sentences.push(SentenceSpan::new(
                    n,
                    *t,
                    0,
                    u32::try_from(stored.start).expect("fits"),
                    u32::try_from(stored.end).expect("fits"),
                ));
                n += 1;
            }
        }
        let mut output = LedgerOutput::default();
        output
            .set_section(LedgerOutput::KEYS, &Vec::<String>::new())
            .expect("keys");
        output
            .set_section(LedgerOutput::UNITS, &units)
            .expect("units");
        let verdicts: Vec<Value> = corrections
            .iter()
            .map(|(id, v)| json!({"id": id, "verdict": v}))
            .collect();
        output
            .set_section(LedgerOutput::CORRECTIONS, &verdicts)
            .expect("corrections");
        let mut body = LedgerBody {
            call: CALL_SEGMENT.to_owned(),
            segment: Some(turns[0]),
            schema: 1,
            variant: "sr2".to_owned(),
            model_key: "m".to_owned(),
            sentences,
            output,
            status: "complete".to_owned(),
            ..LedgerBody::default()
        };
        body.inputs.prefilter = PREFILTER_V1;
        body.inputs.splitter = SPLITTER_V1;
        body.inputs.correction = CORRECTION_V1;
        body.inputs.corr = corr;
        let id = self.next_id();
        self.ledgers.push((id, body));
        self.units.push(units.to_vec());
        id
    }

    fn ledger(&mut self, turns: &[Id], units: &[Value]) -> Id {
        self.ledger_with(turns, units, &[])
    }

    fn directive(&mut self, b: DirectiveBody) -> Id {
        let id = self.next_id();
        self.directives.push((id, b));
        id
    }

    fn directive_set(&self) -> DirectiveSet {
        let rules = Rules::bundled();
        DirectiveSet::new(self.directives.iter().cloned(), "[伏せ字]", |s| {
            rules.resolve_entity(s)
        })
    }

    fn turn_inputs(&self) -> BTreeMap<Id, TurnInput<'_>> {
        self.turns
            .iter()
            .map(|(id, t)| {
                (
                    *id,
                    TurnInput {
                        session: t.session,
                        speaker: t.speaker,
                        rev: 0,
                        text: &t.text,
                        tz_offset: t.tz,
                        long_inputs: &t.long,
                    },
                )
            })
            .collect()
    }

    fn derive_with(&self, cache: &mut GateCache, order: &[usize]) -> Derived {
        let directives = self.directive_set();
        let turns = self.turn_inputs();
        let selected: Vec<Selected<'_>> = order
            .iter()
            .map(|&i| {
                let (id, body) = &self.ledgers[i];
                Selected {
                    ledger: StoredLedger { id: *id, body },
                    stage: Stage::Current,
                }
            })
            .collect();
        let rules = Rules::bundled();
        let inputs = Inputs {
            ledgers: &selected,
            turns: &turns,
            directives: &directives,
            env: Env {
                dictionary: Dictionary::bundled(),
                rules: &rules,
                key: &KEY,
            },
            dates: dates::date_rules(),
        };
        derive(&inputs, cache)
    }

    fn derive(&self) -> Derived {
        let order: Vec<usize> = (0..self.ledgers.len()).collect();
        self.derive_with(&mut GateCache::new(), &order)
    }
}

fn byte_range(r: &Range<usize>) -> ByteRange {
    ByteRange {
        start: u32::try_from(r.start).expect("fits"),
        end: u32::try_from(r.end).expect("fits"),
    }
}

fn find<'d>(d: &'d Derived, aspect: &str, value: &str) -> &'d DerivedUnit {
    d.units
        .iter()
        .find(|u| u.body.aspect == aspect && u.body.value == value)
        .unwrap_or_else(|| panic!("no unit {aspect}={value} in {:#?}", summary(d)))
}

fn summary(d: &Derived) -> Vec<String> {
    d.units
        .iter()
        .map(|u| {
            let b = &u.body;
            format!(
                "{}/{}={} {:?} {:?} p={}",
                b.entity, b.aspect, b.value, b.state, b.tense, b.private
            )
        })
        .collect()
}

fn state(d: &Derived, aspect: &str, value: &str) -> Option<UnitState> {
    find(d, aspect, value).body.state
}

fn vks(value: &str) -> Vec<String> {
    ValueKeys::of(&KEY, value).to_hex()
}

fn dict_directive(op: DirectiveOp, aspect: &str, value: &str) -> DirectiveBody {
    let mut b = DirectiveBody::new(op);
    b.entity = Some("self".to_owned());
    b.aspect = Some(aspect.to_owned());
    b.vks = vks(value);
    b.norm = Some(1);
    b
}

// ---- 単値の辞書の属性 ----

#[test]
fn an_explicit_subject_or_a_change_word_replaces_the_current_value() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("先月、汐見坂に引っ越しました。");
    w.ledger(&[b], &[u(1, "residence", "汐見坂")]);
    let d = w.derive();
    let old = find(&d, "residence", "港南台");
    let new = find(&d, "residence", "汐見坂");
    assert_eq!(old.body.state, Some(UnitState::Superseded));
    assert_eq!(old.body.by, Some(new.id));
    assert_eq!(old.body.state_at, Some(b.timestamp().as_millis()));
    assert_eq!(new.body.state, Some(UnitState::Live));
    assert_eq!(d.units.len(), 2);
}

#[test]
fn a_bare_new_value_is_contested_and_a_repetition_promotes_it() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("霧笛町に住んでるよ。");
    w.ledger(&[b], &[u(1, "residence", "霧笛町")]);
    let d = w.derive();
    let live = find(&d, "residence", "港南台").id;
    let c = find(&d, "residence", "霧笛町");
    assert_eq!(c.body.state, Some(UnitState::Contested));
    assert_eq!(c.body.against, Some(live));
    assert_eq!(d.counts.folded(FoldNote::Contested), 1);

    let e = w.say("霧笛町に住んでるよ。");
    w.ledger(&[e], &[u(1, "residence", "霧笛町")]);
    let d = w.derive();
    let new = find(&d, "residence", "霧笛町");
    assert_eq!(new.body.state, Some(UnitState::Live));
    assert_eq!(find(&d, "residence", "港南台").body.by, Some(new.id));
    assert_eq!(new.body.anchors.len(), 2);
}

#[test]
fn a_superseded_value_said_again_is_the_same_unit() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("私は汐見坂に住んでいます。");
    w.ledger(&[b], &[u(1, "residence", "汐見坂")]);
    let c = w.say("私はまた港南台に住んでいます。");
    w.ledger(&[c], &[u(1, "residence", "港南台")]);
    let d = w.derive();
    assert_eq!(d.units.len(), 2, "{:#?}", summary(&d));
    let back = find(&d, "residence", "港南台");
    assert_eq!(back.body.state, Some(UnitState::Live));
    assert_eq!(back.body.by, None);
    assert_eq!(back.body.anchors.len(), 2);
    assert_eq!(find(&d, "residence", "汐見坂").body.by, Some(back.id));
}

#[test]
fn past_tense_ends_the_current_value_and_negation_needs_a_change_word() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("以前は港南台に住んでいました。");
    w.ledger(&[b], &[with(u(1, "residence", "港南台"), "tn", "past")]);
    let d = w.derive();
    let x = find(&d, "residence", "港南台");
    assert_eq!(x.body.state, Some(UnitState::Superseded));
    assert_eq!(x.body.by, None);
    assert!(
        x.body
            .anchors
            .iter()
            .any(|a| a.kind == AnchorKind::Ended && a.turn == b)
    );
    assert_eq!(
        x.body.tense,
        UnitTense::Current,
        "the record keeps its tense"
    );

    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("港南台には住んでいません。");
    w.ledger(&[b], &[with(u(1, "residence", "港南台"), "m", "negated")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "港南台"), Some(UnitState::Live));
    assert_eq!(d.counts.folded(FoldNote::NegationDropped), 1);

    let c = w.say("もう港南台には住んでいません。");
    w.ledger(&[c], &[with(u(1, "residence", "港南台"), "m", "negated")]);
    let d = w.derive();
    assert_eq!(
        state(&d, "residence", "港南台"),
        Some(UnitState::Superseded)
    );
}

#[test]
fn a_plan_has_its_own_slot_and_becomes_current_when_it_happens() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("来年、汐見坂に引っ越す予定です。");
    w.ledger(&[b], &[with(u(1, "residence", "汐見坂"), "tn", "planned")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "港南台"), Some(UnitState::Live));
    let plan = find(&d, "residence", "汐見坂");
    assert_eq!(
        (plan.body.state, plan.body.tense),
        (Some(UnitState::Live), UnitTense::Planned)
    );

    let c = w.say("汐見坂に引っ越しました。");
    w.ledger(&[c], &[u(1, "residence", "汐見坂")]);
    let d = w.derive();
    let now = find(&d, "residence", "汐見坂");
    assert_eq!(
        (now.body.state, now.body.tense),
        (Some(UnitState::Live), UnitTense::Current)
    );
    assert_eq!(find(&d, "residence", "港南台").body.by, Some(now.id));
    assert_eq!(d.units.len(), 2);
}

#[test]
fn a_long_single_input_does_not_replace_the_current_value() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let text = "私は汐見坂に住んでいます。";
    let b = w.say(text);
    w.turns.get_mut(&b).expect("turn").long = std::iter::once(0..text.len()).collect();
    w.ledger(&[b], &[u(1, "residence", "汐見坂")]);
    let c = w.say("私は汐見坂に住んでいます。");
    w.turns.get_mut(&c).expect("turn").long = std::iter::once(0..text.len()).collect();
    w.ledger(&[c], &[u(1, "residence", "汐見坂")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "港南台"), Some(UnitState::Live));
    assert_eq!(state(&d, "residence", "汐見坂"), Some(UnitState::Contested));
}

// ---- 多値・自由記述の属性・出来事 ----

#[test]
fn a_multi_valued_attribute_replaces_only_the_named_old_value() {
    let mut w = World::default();
    let a = w.say("趣味は将棋です。");
    let b = w.say("趣味は登山です。");
    w.ledger(&[a, b], &[u(1, "hobby", "将棋"), u(2, "hobby", "登山")]);
    let c = w.say("趣味は将棋から囲碁に変えました。");
    w.ledger(&[c], &[with(u(1, "hobby", "囲碁"), "old", "将棋")]);
    let d = w.derive();
    let go = find(&d, "hobby", "囲碁");
    assert_eq!(state(&d, "hobby", "登山"), Some(UnitState::Live));
    assert_eq!(find(&d, "hobby", "将棋").body.by, Some(go.id));
    assert_eq!(go.body.state, Some(UnitState::Live));
}

#[test]
fn free_aspects_are_never_replaced() {
    let mut w = World::default();
    let a = w.say("よく行く店は喫茶ミモザです。");
    w.ledger(&[a], &[u(1, "よく行く店", "喫茶ミモザ")]);
    let b = w.say("よく行く店は書店アカシアです。");
    w.ledger(&[b], &[u(1, "よく行く店", "書店アカシア")]);
    let c = w.say("前の住まいは霧ヶ丘でした。");
    w.ledger(&[c], &[with(u(1, "前の住まい", "霧ヶ丘"), "tn", "past")]);
    let d = w.derive();
    let a = find(&d, "よく行く店", "喫茶ミモザ");
    assert!(a.body.free);
    assert_eq!(a.body.state, Some(UnitState::Live));
    assert_eq!(
        state(&d, "よく行く店", "書店アカシア"),
        Some(UnitState::Live)
    );
    let past = find(&d, "前の住まい", "霧ヶ丘");
    assert_eq!(
        (past.body.state, past.body.by),
        (Some(UnitState::Superseded), None)
    );
}

#[test]
fn episodes_merge_by_date_and_keep_no_state() {
    let mut w = World::default();
    let a = w.say("昨日、海辺の公園でバーベキューをしました。");
    w.ledger(
        &[a],
        &[with(
            with(u(1, "したこと", "バーベキュー"), "l", "episode"),
            "when",
            "昨日",
        )],
    );
    let b = w.say("昨日、海辺の公園でバーベキューをしました。");
    w.ledger(
        &[b],
        &[with(
            with(u(1, "したこと", "バーベキュー"), "l", "episode"),
            "when",
            "昨日",
        )],
    );
    w.clock += 3 * 86_400_000;
    let c = w.say("昨日、海辺の公園でバーベキューをしました。");
    w.ledger(
        &[c],
        &[with(
            with(u(1, "したこと", "バーベキュー"), "l", "episode"),
            "when",
            "昨日",
        )],
    );
    let d = w.derive();
    let eps: Vec<&DerivedUnit> = d
        .units
        .iter()
        .filter(|u| u.body.aspect == "したこと")
        .collect();
    assert_eq!(eps.len(), 2, "{:#?}", summary(&d));
    assert!(eps.iter().all(|u| u.body.state.is_none()));
    let first = eps
        .iter()
        .find(|u| u.body.anchors.len() == 2)
        .expect("the two mentions on the same day merge");
    let when = first.body.when.as_ref().expect("when");
    assert_eq!(when.basis, WhenBasis::Relative);
    assert_eq!(when.from.as_deref(), Some("2026-03-31"));
}

#[test]
fn an_episode_without_a_date_keeps_the_utterance_day() {
    let mut w = World::default();
    let a = w.say("海辺の公園でバーベキューをしました。");
    w.ledger(
        &[a],
        &[with(u(1, "したこと", "バーベキュー"), "l", "episode")],
    );
    let d = w.derive();
    let when = find(&d, "したこと", "バーベキュー")
        .body
        .when
        .clone()
        .expect("when");
    assert_eq!(when.basis, WhenBasis::Utterance);
    assert_eq!(when.from.as_deref(), Some("2026-04-01"));
}

// ---- 済んだ候補・関門の数え方 ----

#[test]
fn the_same_candidate_twice_counts_once() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(
        &[a],
        &[u(1, "residence", "港南台"), u(1, "residence", "港南台")],
    );
    let d = w.derive();
    assert_eq!(find(&d, "residence", "港南台").body.anchors.len(), 1);
    assert_eq!(d.counts.folded(FoldNote::Already), 1);
}

#[test]
fn gate_failures_are_counted_and_do_not_become_units() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(
        &[a],
        &[
            u(9, "residence", "港南台"),
            u(1, "residence", "汐見坂"),
            json!({"s": 1}),
        ],
    );
    let d = w.derive();
    assert!(d.units.is_empty());
    assert_eq!(d.counts.gate(Reason::Evidence), 1);
    assert_eq!(d.counts.gate(Reason::ValueNotInEvidence), 1);
    assert_eq!(d.counts.mismatched, 1);
}

// ---- 指示 ----

#[test]
fn a_forgotten_value_does_not_become_a_unit() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    w.directive(dict_directive(DirectiveOp::Forget, "residence", "港南台"));
    let d = w.derive();
    assert!(d.units.is_empty());
    assert_eq!(d.counts.gate(Reason::Forgotten), 1);
}

#[test]
fn an_end_closes_the_value_and_a_later_mention_becomes_live() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("霧笛町に住んでるよ。");
    w.ledger(&[b], &[u(1, "residence", "霧笛町")]);
    let mut e = dict_directive(DirectiveOp::End, "residence", "港南台");
    e.turn = Some(b);
    e.entity_raw = Some("self".to_owned());
    w.directive(e);
    let d = w.derive();
    let ended = find(&d, "residence", "港南台");
    assert_eq!(
        (ended.body.state, ended.body.by),
        (Some(UnitState::Superseded), None)
    );
    assert_eq!(ended.body.state_at, Some(b.timestamp().as_millis()));
    // 終えた値を指したまま残る矛盾 (04 §5.3 の「今の値を終えたときの矛盾」)。
    let c = find(&d, "residence", "霧笛町");
    assert_eq!(
        (c.body.state, c.body.against),
        (Some(UnitState::Contested), Some(ended.id))
    );

    let f = w.say("霧笛町に住んでるよ。");
    w.ledger(&[f], &[u(1, "residence", "霧笛町")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "霧笛町"), Some(UnitState::Live));
    assert_eq!(
        state(&d, "residence", "港南台"),
        Some(UnitState::Superseded)
    );
}

#[test]
fn accept_and_revert_adopt_a_value_at_the_directive() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let b = w.say("霧笛町に住んでるよ。");
    w.ledger(&[b], &[u(1, "residence", "霧笛町")]);
    w.directive(dict_directive(DirectiveOp::Accept, "residence", "霧笛町"));
    let d = w.derive();
    let new = find(&d, "residence", "霧笛町");
    assert_eq!(new.body.state, Some(UnitState::Live));
    assert_eq!(find(&d, "residence", "港南台").body.by, Some(new.id));
    assert_eq!(d.counts.folded(FoldNote::Adopted), 1);

    w.directive(dict_directive(DirectiveOp::Revert, "residence", "港南台"));
    let d = w.derive();
    assert_eq!(state(&d, "residence", "港南台"), Some(UnitState::Live));
    assert_eq!(
        state(&d, "residence", "霧笛町"),
        Some(UnitState::Superseded)
    );
}

#[test]
fn private_marks_the_value_and_its_successors() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let mut p = dict_directive(DirectiveOp::Private, "residence", "港南台");
    p.scope = Some("value_only".to_owned());
    w.directive(p);
    let d = w.derive();
    let x = find(&d, "residence", "港南台");
    assert!(x.body.private);
    assert_eq!(
        x.private_by.map(|h| h.1),
        Some(crate::apply::PrivateHit::Value)
    );

    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let mut p = dict_directive(DirectiveOp::Private, "residence", "港南台");
    p.scope = Some(PrivateScope::Successors.name().to_owned());
    w.directive(p);
    let b = w.say("私は汐見坂に住んでいます。");
    w.ledger(&[b], &[u(1, "residence", "汐見坂")]);
    let d = w.derive();
    assert!(find(&d, "residence", "港南台").body.private);
    let next = find(&d, "residence", "汐見坂");
    assert!(next.body.private);
    assert_eq!(
        next.private_by.map(|h| h.1),
        Some(crate::apply::PrivateHit::Successor)
    );
}

#[test]
fn an_end_written_for_a_free_aspect_still_hits_after_the_aspect_is_mapped() {
    // 04 §7.5 の「辞書の写しの後の end」: 写す前の自由記述の属性の鍵とも比べる。
    let mut w = World::default();
    let a = w.say("住まいは北町です。");
    w.ledger(&[a], &[u(1, "住まい", "北町")]);
    let b = w.say("住まいは南浜です。");
    w.ledger(&[b], &[u(1, "住まい", "南浜")]);
    let mut e = DirectiveBody::new(DirectiveOp::End);
    e.entity = Some("self".to_owned());
    e.entity_raw = Some("self".to_owned());
    e.aspect_mac = Some(aspect_key(&KEY, &normalize_v1("住まい")).to_hex());
    e.vks = vks("北町");
    e.norm = Some(1);
    e.turn = Some(a);
    w.directive(e);
    let d = w.derive();
    assert_eq!(state(&d, "住まい", "北町"), Some(UnitState::Superseded));
    assert_eq!(state(&d, "住まい", "南浜"), Some(UnitState::Live));
}

// ---- 訂正 ----

#[test]
fn an_accepted_correction_replaces_the_current_value() {
    let run = |verdict: &str| {
        let mut w = World::default();
        let a = w.say("私は大阪に住んでいます。");
        w.ledger(&[a], &[u(1, "residence", "大阪")]);
        w.answer("大阪はいいところですね。");
        let b = w.say("いや、大阪じゃなくて京都に住んでいます。");
        w.ledger_with(&[b], &[u(1, "residence", "京都")], &[(1, verdict)]);
        (w.derive(), b)
    };
    let (d, b) = run("accept");
    let kyoto = find(&d, "residence", "京都");
    let osaka = find(&d, "residence", "大阪");
    assert_eq!(kyoto.body.state, Some(UnitState::Live), "{:#?}", d.counts);
    assert_eq!(osaka.body.by, Some(kyoto.id));
    assert!(
        osaka
            .body
            .anchors
            .iter()
            .any(|a| a.kind == AnchorKind::Corrected && a.turn == b)
    );
    assert_eq!(d.counts.correction(CorrectionReason::Applied), 1);

    let (d, _) = run("reject");
    assert_eq!(state(&d, "residence", "京都"), Some(UnitState::Contested));
    assert_eq!(d.counts.correction(CorrectionReason::NotCorrection), 1);
}

#[test]
fn a_correction_whose_old_value_is_not_current_has_no_effect() {
    let mut w = World::default();
    let a = w.say("私は神戸に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "神戸")]);
    w.answer("神戸はいいところですね。");
    let b = w.say("いや、大阪じゃなくて京都に住んでいます。");
    w.ledger_with(&[b], &[u(1, "residence", "京都")], &[(1, "accept")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "京都"), Some(UnitState::Contested));
    assert_eq!(d.counts.correction(CorrectionReason::OldNotFound), 1);
}

// ---- 決定性の性質の試験 (09 §3.5.1) ----

/// 小さな擬似乱数 (xorshift64*)。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).expect("fits")).expect("fits")
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

const PLACES: [&str; 4] = ["港南台", "汐見坂", "霧笛町", "北町"];
const HOBBIES: [&str; 3] = ["将棋", "囲碁", "登山"];

/// 乱数で作った世界 (住まいの言い直し・趣味・出来事・指示)。
fn random_world(seed: u64) -> World {
    let mut r = Rng(seed | 1);
    let mut w = World::default();
    for _ in 0..12 {
        let n = 1 + r.below(3);
        let mut turns = Vec::new();
        let mut units = Vec::new();
        for k in 0..n {
            let s = u32::try_from(k + 1).expect("fits");
            match r.below(5) {
                0 => {
                    let p = PLACES[r.below(PLACES.len())];
                    turns.push(w.say(&format!("私は{p}に住んでいます。")));
                    units.push(u(s, "residence", p));
                }
                1 => {
                    let p = PLACES[r.below(PLACES.len())];
                    turns.push(w.say(&format!("{p}に住んでるよ。")));
                    units.push(u(s, "residence", p));
                }
                2 => {
                    let p = PLACES[r.below(PLACES.len())];
                    turns.push(w.say(&format!("以前は{p}に住んでいました。")));
                    units.push(with(u(s, "residence", p), "tn", "past"));
                }
                3 => {
                    let h = HOBBIES[r.below(HOBBIES.len())];
                    turns.push(w.say(&format!("趣味は{h}です。")));
                    units.push(u(s, "hobby", h));
                }
                _ => {
                    turns.push(w.say("昨日、海辺の公園でバーベキューをしました。"));
                    units.push(with(
                        with(u(s, "したこと", "バーベキュー"), "l", "episode"),
                        "when",
                        "昨日",
                    ));
                }
            }
        }
        w.ledger(&turns, &units);
        if r.below(4) == 0 {
            let p = PLACES[r.below(PLACES.len())];
            let op = [
                DirectiveOp::Accept,
                DirectiveOp::Revert,
                DirectiveOp::Private,
            ][r.below(3)];
            w.directive(dict_directive(op, "residence", p));
        }
    }
    w
}

/// verifies: AC-FR-315-1 (台帳を読む順・台帳の中の候補の順・指示の順を乱しても、単位の集合と本体が同じ。04 §5.3)
#[test]
fn derivation_does_not_depend_on_the_arrival_order() {
    for seed in 1..=40u64 {
        let w = random_world(seed);
        let base = w.derive();
        let mut r = Rng(seed * 7919);
        for _ in 0..4 {
            let mut order: Vec<usize> = (0..w.ledgers.len()).collect();
            r.shuffle(&mut order);
            let mut shuffled = World {
                turns: BTreeMap::new(),
                ledgers: w.ledgers.clone(),
                directives: w.directives.clone(),
                ..World::default()
            };
            // 台帳の中の候補の順も乱す。
            for ((_, body), units) in shuffled.ledgers.iter_mut().zip(&w.units) {
                let mut units = units.clone();
                r.shuffle(&mut units);
                body.output
                    .set_section(LedgerOutput::UNITS, &units)
                    .expect("units");
            }
            shuffled.turns = w
                .turns
                .iter()
                .map(|(k, t)| {
                    (
                        *k,
                        TurnData {
                            session: t.session,
                            speaker: t.speaker,
                            text: t.text.clone(),
                            tz: t.tz,
                            long: t.long.clone(),
                        },
                    )
                })
                .collect();
            r.shuffle(&mut shuffled.directives);
            let got = shuffled.derive_with(&mut GateCache::new(), &order);
            assert_eq!(got, base, "seed {seed}, order {order:?}");
        }
    }
}

#[test]
fn derivation_is_idempotent_and_the_cache_does_not_change_the_result() {
    for seed in 1..=20u64 {
        let w = random_world(seed);
        let order: Vec<usize> = (0..w.ledgers.len()).collect();
        let mut cache = GateCache::new();
        let cold = w.derive_with(&mut cache, &order);
        assert_eq!(cold.cache.hits + cold.cache.misses, cache_lookups(&w));
        let warm = w.derive_with(&mut cache, &order);
        assert_eq!(warm.cache.misses, 0, "seed {seed}");
        assert_eq!(
            Derived {
                cache: cold.cache,
                ..warm.clone()
            },
            cold,
            "seed {seed}"
        );
        // 覚えはその回に使った鍵だけを残す。
        assert!(cache.len() <= cache_lookups(&w));
    }
}

fn cache_lookups(w: &World) -> usize {
    w.ledgers
        .iter()
        .map(|(_, b)| b.output.units().items.len())
        .sum()
}

/// 関門 1〜7 の覚えの鍵が同じなら、結果は同じ (04 §5.3。鍵に入らない入力 (ほかの台帳・指示の当たり) を変えても、
/// 覚えた結果で導いた単位は覚えずに導いた単位と同じ)。
#[test]
fn cached_results_stay_valid_when_other_inputs_change() {
    for seed in 1..=20u64 {
        let mut w = random_world(seed);
        let order: Vec<usize> = (0..w.ledgers.len()).collect();
        let mut cache = GateCache::new();
        let _ = w.derive_with(&mut cache, &order);
        w.directive(dict_directive(DirectiveOp::Forget, "residence", PLACES[0]));
        let a = w.say("趣味は将棋です。");
        w.ledger(&[a], &[u(1, "hobby", "将棋")]);
        let order: Vec<usize> = (0..w.ledgers.len()).collect();
        let warm = w.derive_with(&mut cache, &order);
        let cold = w.derive_with(&mut GateCache::new(), &order);
        assert_eq!(warm.units, cold.units, "seed {seed}");
        assert_eq!(warm.counts, cold.counts, "seed {seed}");
    }
}

// ---- import-v04 の end ----

#[test]
fn import_ends_close_values_that_were_not_current_in_v04() {
    let mut w = World::default();
    let a = w.say("私は港南台に住んでいます。");
    w.ledger(&[a], &[u(1, "residence", "港南台")]);
    let table = V04Ended {
        v: 1,
        done: false,
        values: vec![EndedValue {
            entity: "self".to_owned(),
            aspect: "residence".to_owned(),
            vks: vks("港南台"),
            turn: Some(a),
        }],
    };
    let rules = Rules::bundled();
    let resolve = |s: &str| rules.resolve_entity(s);
    let d = w.derive();
    let ends = import_ends(&d, &table, &w.directive_set(), &KEY, resolve);
    assert_eq!(ends.len(), 1);
    let e = &ends[0];
    let mut b = DirectiveBody::new(DirectiveOp::End);
    b.entity = Some(e.entity.clone());
    b.entity_raw = Some(e.entity_raw.clone());
    b.aspect = Some(e.aspect.clone());
    b.vks = e.vks.clone();
    b.turn = e.turn;
    b.origin = "import".to_owned();
    b.norm = Some(1);
    w.directive(b);
    let d = w.derive();
    assert_eq!(
        state(&d, "residence", "港南台"),
        Some(UnitState::Superseded)
    );
    assert!(import_ends(&d, &table, &w.directive_set(), &KEY, resolve).is_empty());

    // 取り込みの後に言い直した値は、end の後の候補として Live になり、もう end を足さない。
    let c = w.say("私は港南台に住んでいます。");
    w.ledger(&[c], &[u(1, "residence", "港南台")]);
    let d = w.derive();
    assert_eq!(state(&d, "residence", "港南台"), Some(UnitState::Live));
    assert!(import_ends(&d, &table, &w.directive_set(), &KEY, resolve).is_empty());
}

// ---- 単位の本体 ----

#[test]
fn unit_bodies_are_well_formed() {
    for seed in 1..=10u64 {
        let d = random_world(seed).derive();
        assert!(
            d.units.windows(2).all(|p| p[0].id < p[1].id),
            "sorted by id"
        );
        for x in &d.units {
            let b: &UnitBody = &x.body;
            assert_eq!(b.layer == UnitLayer::World, b.state.is_some());
            assert!(
                b.anchors
                    .windows(2)
                    .all(|p| (p[0].turn, p[0].start) <= (p[1].turn, p[1].start))
            );
            assert_eq!(b.against.is_some(), b.state == Some(UnitState::Contested));
            if b.state != Some(UnitState::Superseded) {
                assert_eq!((b.by, b.state_at), (None, None));
            }
            let id = unit_id(
                b.layer,
                &b.entity,
                &b.aspect,
                b.free,
                &b.vk,
                (b.layer == UnitLayer::Episode)
                    .then(|| b.when.as_ref().and_then(|w| w.from.clone()))
                    .flatten()
                    .as_deref(),
            );
            assert_eq!(id, x.id, "the id follows the content");
        }
        // 単値の属性の今の値は、組ごとに高々 1 つ (予定の枠は別)。
        let live: Vec<_> = d
            .units
            .iter()
            .filter(|x| {
                x.body.aspect == "residence"
                    && x.body.state == Some(UnitState::Live)
                    && x.body.tense == UnitTense::Current
            })
            .collect();
        assert!(live.len() <= 1, "seed {seed}");
    }
}

// ---- 1 万件の合成データのベンチ (S-17 の 6、08 §2.6 の M5.3) ----

/// 1 万件の候補 (5,000 ターン × 2 件、625 の区切り) を、覚えの無い導出と覚えの効いた導出で測る。
/// `cargo test -p evorag-memory --release -- --ignored derive_bench --nocapture` で走らせる。
#[test]
#[ignore = "bench: run with --release --ignored"]
fn derive_bench_10k_units() {
    let mut w = World::default();
    for seg in 0..625 {
        let mut turns = Vec::new();
        let mut units = Vec::new();
        for k in 0..8u32 {
            let i = seg * 8 + k;
            turns.push(w.say_in(
                u8::try_from(seg % 200).expect("fits"),
                &format!("私は町{i}番に住んでいます。趣味は将棋{i}です。"),
            ));
            units.push(u(2 * k + 1, "residence", &format!("町{i}番")));
            units.push(u(2 * k + 2, "hobby", &format!("将棋{i}")));
        }
        w.ledger(&turns, &units);
    }
    let order: Vec<usize> = (0..w.ledgers.len()).collect();
    let mut cache = GateCache::new();
    let t = std::time::Instant::now();
    let cold = w.derive_with(&mut cache, &order);
    let cold_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let warm = w.derive_with(&mut cache, &order);
    let warm_ms = t.elapsed().as_millis();
    assert_eq!(cold.units, warm.units);
    assert_eq!(cold.units.len(), 10_000);
    assert_eq!(warm.cache.misses, 0);
    println!("derive 10k: cold {cold_ms} ms, warm {warm_ms} ms");
}

/// ベンチの小さな形 (常に走らせる。遅い機械でも収まる緩い上限で、畳みが件数の 2 乗にならないことを見る)。
#[test]
fn derive_scales_roughly_linearly() {
    let build = |n: u32| {
        let mut w = World::default();
        for seg in 0..n {
            let mut turns = Vec::new();
            let mut units = Vec::new();
            for k in 0..4u32 {
                let i = seg * 4 + k;
                turns.push(w.say(&format!("私は町{i}番に住んでいます。趣味は将棋{i}です。")));
                units.push(u(2 * k + 1, "residence", &format!("町{i}番")));
                units.push(u(2 * k + 2, "hobby", &format!("将棋{i}")));
            }
            w.ledger(&turns, &units);
        }
        w
    };
    let w = build(125);
    let order: Vec<usize> = (0..w.ledgers.len()).collect();
    let mut cache = GateCache::new();
    let _ = w.derive_with(&mut cache, &order);
    let t = std::time::Instant::now();
    let d = w.derive_with(&mut cache, &order);
    assert_eq!(d.units.len(), 1_000);
    assert!(t.elapsed().as_secs() < 10, "warm derive of 1,000 units");
}
