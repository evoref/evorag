//! 試験の事例 (`tests/memory-cases/v0.5/derive.toml`) のうち、純粋な導出で決まるものを走らせる小さな走らせる側。
//!
//! 走らせるのは `stage = "M5.3"` の事例 ([`CASES`]) の、ターン・区切り・選択・台帳・導出の段と、期待の `units`
//! (完全に比べる)・`label`・`same_as` (単位の集合・本体・導出の指紋)。区切りの発話に前のふるいと文の分け方 v1 を
//! 当てて台帳の `sentences` を作り、区切りごとに `select_ledger` で使う台帳を選んでから導く。`order = "shuffle"`
//! の導出は、台帳を読む順 (区切りの到着の順・同じ区切りの複数の選択の台帳の順) と、台帳の中の候補の順を乱す。
//!
//! 走らせない段 (プロファイル・鍵の使えない状態・再起動) は [`IGNORED_OPS`] と [`IGNORED_EXPECT`] に挙げる (プロファイル
//! は M5.3 の次の PR、起動し直しは覚えを空にして導くだけ)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use toml::{Table, Value};

use super::tests::id_at;
use super::*;
use crate::b3::{Dictionary, Rules};
use crate::corrections::CORRECTION_V1;
use crate::ledger::{CALL_SEGMENT, LedgerBody, LedgerOutput, SentenceSpan};
use crate::ledger_select::{Current, SelectEnv, Selection, StoredLedger, select_ledger};
use crate::prefilter::{MODE_CHAT, PREFILTER_V1, Screened, prefilter};
use crate::sentences::{SPLITTER_V1, split_v1};
use crate::unit::{UnitState, UnitTense, WhenBasis};
use crate::value_key::ForgetKey;

const KEY: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

/// 走らせる事例 (`derive.toml` の `stage = "M5.3"`)。
const CASES: &[&str] = &["DR03", "DR06"];

/// 走らせない操作。
const IGNORED_OPS: &[&str] = &["forget_key", "restart"];

/// 比べない期待の項目 (プロファイルは M5.3 の次の PR)。
const IGNORED_EXPECT: &[&str] = &["profile"];

/// 事例の変種 (台帳の `variant` の既定)。
const VARIANT: &str = "v1";

fn root() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn cases() -> Vec<Table> {
    let path = root().join("../../tests/memory-cases/v0.5/derive.toml");
    let text = std::fs::read_to_string(&path).expect("derive.toml");
    let doc: Table = toml::from_str(&text).expect("parses");
    doc["case"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|c| c.as_table().expect("a case is a table").clone())
        .collect()
}

fn str_of<'v>(t: &'v Table, k: &str) -> Option<&'v str> {
    t.get(k).and_then(Value::as_str)
}

/// `2026-04-01T10:00:00Z` を UNIX 時刻のミリ秒に。
fn parse_at(s: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| -> i64 { s[r].parse().expect("a number") };
    let (y, mo, d) = (n(0..4), n(5..7), n(8..10));
    let (h, mi, sec) = (n(11..13), n(14..16), n(17..19));
    // H. Hinnant の days_from_civil。
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    ((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000
}

/// `"+09:00"` を分に。`"none"` は記録しない。
fn parse_tz(s: &str) -> Option<i32> {
    if s == "none" {
        return None;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let h: i32 = s[1..3].parse().expect("hours");
    let m: i32 = s[4..6].parse().expect("minutes");
    Some(sign * (h * 60 + m))
}

struct Turn {
    id: Id,
    session: Id,
    speaker: Speaker,
    text: String,
    tz: Option<i32>,
}

/// 期待と比べる単位の形 (書いた項目だけ)。
type Snapshot = (Vec<DerivedUnit>, Fingerprint);

#[derive(Default)]
struct Run {
    case: String,
    turns: Vec<Turn>,
    names: BTreeMap<String, Id>,
    sessions: BTreeMap<String, Id>,
    segments: BTreeMap<String, Vec<Id>>,
    ledgers: Vec<(Id, LedgerBody, Vec<Json>)>,
    current: String,
    previous: Option<String>,
    derived: Option<Snapshot>,
    labels: BTreeMap<String, Snapshot>,
    cache: GateCache,
    problems: Vec<String>,
    seq: u16,
}

/// 台帳の候補の要素を wire の形に (長い名前 → `s`・`l`・`e`・`a`・`v`・`m`・`tn`)。
fn wire_unit(v: &Value) -> Json {
    let t = v.as_table().expect("a unit is a table");
    let s = |k: &str, d: &str| str_of(t, k).unwrap_or(d).to_owned();
    json!({
        "s": t["sentence"].as_integer().expect("sentence"),
        "l": s("layer", "world"),
        "e": s("entity", "self"),
        "a": s("aspect", ""),
        "v": s("value", ""),
        "m": s("mood", "fact"),
        "tn": s("tense", "current"),
        "old": s("old", ""),
        "when": s("when", ""),
    })
}

impl Run {
    fn fail(&mut self, msg: String) {
        let case = self.case.clone();
        self.problems.push(format!("{case}: {msg}"));
    }

    fn turn(&self, id: Id) -> &Turn {
        self.turns.iter().find(|t| t.id == id).expect("a turn")
    }

    fn name_of(&self, id: Id) -> String {
        self.names
            .iter()
            .find(|(_, v)| **v == id)
            .map_or_else(|| id.to_string(), |(k, _)| k.clone())
    }

    fn add_turn(&mut self, t: &Table) {
        let name = str_of(t, "id").expect("id").to_owned();
        let session = str_of(t, "session").expect("session").to_owned();
        let next = self.sessions.len();
        let session = *self
            .sessions
            .entry(session)
            .or_insert_with(|| id_at(0, u16::try_from(next + 1).expect("fits")));
        self.seq += 1;
        let id = id_at(parse_at(str_of(t, "at").expect("at")), self.seq);
        let speaker = match str_of(t, "role").unwrap_or("user") {
            "assistant" => Speaker::Assistant,
            _ => Speaker::User,
        };
        self.names.insert(name, id);
        self.turns.push(Turn {
            id,
            session,
            speaker,
            text: str_of(t, "text").expect("text").to_owned(),
            tz: parse_tz(str_of(t, "tz").unwrap_or("+09:00")),
        });
    }

    fn add_ledger(&mut self, l: &Table) {
        let seg = str_of(l, "seg").expect("seg");
        let pairs = self
            .segments
            .get(seg)
            .cloned()
            .expect("the segment is declared");
        let mut sentences = Vec::new();
        let mut n = 1;
        for t in &pairs {
            let Screened::Send(f) = prefilter(MODE_CHAT, &self.turn(*t).text) else {
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
        let units: Vec<Json> = l
            .get("units")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(wire_unit).collect())
            .unwrap_or_default();
        let keys: Vec<String> = l
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut output = LedgerOutput::default();
        output.set_section(LedgerOutput::KEYS, &keys).expect("keys");
        output
            .set_section(LedgerOutput::UNITS, &units)
            .expect("units");
        output
            .set_section(LedgerOutput::CORRECTIONS, &Vec::<Json>::new())
            .expect("corrections");
        let mut body = LedgerBody {
            call: CALL_SEGMENT.to_owned(),
            segment: Some(pairs[0]),
            schema: 1,
            variant: str_of(l, "variant").unwrap_or(VARIANT).to_owned(),
            model_key: str_of(l, "model").unwrap_or(&self.current).to_owned(),
            sentences,
            output,
            status: str_of(l, "status").unwrap_or("complete").to_owned(),
            ..LedgerBody::default()
        };
        body.inputs.prefilter = PREFILTER_V1;
        body.inputs.splitter = SPLITTER_V1;
        body.inputs.correction = CORRECTION_V1;
        let latest = self.turns.iter().map(|t| t.id).max().expect("turns");
        self.seq += 1;
        let id = id_at(latest.timestamp().as_millis() + 1, self.seq);
        self.ledgers.push((id, body, units));
    }

    /// 全件の導出。`shuffle` があれば、台帳を読む順と台帳の中の候補の順をその種で乱す。
    fn derive(&mut self, shuffle: Option<u64>) {
        let mut ledgers = self.ledgers.clone();
        if let Some(seed) = shuffle {
            let mut r = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut next = |n: usize| {
                r ^= r >> 12;
                r ^= r << 25;
                r ^= r >> 27;
                usize::try_from(r.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64).expect("fits")
            };
            for i in (1..ledgers.len()).rev() {
                let j = next(i + 1);
                ledgers.swap(i, j);
            }
            for (_, body, items) in &mut ledgers {
                for i in (1..items.len()).rev() {
                    let j = next(i + 1);
                    items.swap(i, j);
                }
                body.output
                    .set_section(LedgerOutput::UNITS, &*items)
                    .expect("units");
            }
        }
        let turns: BTreeMap<Id, TurnInput<'_>> = self
            .turns
            .iter()
            .map(|t| {
                (
                    t.id,
                    TurnInput {
                        session: t.session,
                        speaker: t.speaker,
                        rev: 0,
                        text: &t.text,
                        tz_offset: t.tz,
                        long_inputs: &[],
                    },
                )
            })
            .collect();
        let current = Selection {
            variant: VARIANT,
            model_key: &self.current,
        };
        let previous = self.previous.as_deref().map(|m| Selection {
            variant: VARIANT,
            model_key: m,
        });
        let env = SelectEnv {
            current: Current::Ready(current),
            build_info: "",
            previous,
            versions: crate::ledger_select::RuleVersions::current(CORRECTION_V1),
            turn_rev: |id: Id| turns.get(&id).map(|t| t.rev),
        };
        let stored: Vec<StoredLedger<'_>> = ledgers
            .iter()
            .map(|(id, body, _)| StoredLedger { id: *id, body })
            .collect();
        let segments: Vec<Id> = self
            .segments
            .values()
            .map(|p| p[0])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let selected: Vec<Selected<'_>> = segments
            .iter()
            .filter_map(|s| select_ledger(*s, &stored, &env))
            .collect();
        let rules = Rules::bundled();
        let directives = DirectiveSet::new(Vec::new(), "[伏せ字]", |s| rules.resolve_entity(s));
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
        let d = super::derive(&inputs, &mut self.cache);
        // 指紋は記録した台帳の本体から作る (乱すのは読む順と、覚えと畳みに渡す候補の順だけ)。
        let digests: Vec<LedgerDigest> = self
            .ledgers
            .iter()
            .map(|(id, body, _)| LedgerDigest {
                id: *id,
                sha256: Sha256::digest(serde_json::to_vec(body).expect("serializes")).into(),
            })
            .collect();
        let versions = Versions::new(Dictionary::bundled(), &rules, dates::date_rules(), 1);
        let f = fingerprint(&Components {
            ledgers: &digests,
            directives: DirectiveStamp::default(),
            versions: &versions,
            current: Current::Ready(current),
            previous,
        });
        self.derived = Some((d.units, f));
    }

    fn expect(&mut self, e: &Table) {
        let Some((units, f)) = self.derived.clone() else {
            return self.fail("an expectation before any derivation".to_owned());
        };
        if let Some(want) = e.get("units").and_then(Value::as_array) {
            self.compare_units(&units, want);
        }
        if let Some(label) = str_of(e, "same_as") {
            let Some((u0, f0)) = self.labels.get(label).cloned() else {
                return self.fail(format!("no label {label}"));
            };
            if u0 != units {
                self.fail(format!("units are not the same as {label}"));
            }
            if f0 != f {
                self.fail(format!("the fingerprint is not the same as {label}"));
            }
        }
        if let Some(label) = str_of(e, "label") {
            self.labels.insert(label.to_owned(), (units, f));
        }
        for k in e.keys() {
            if !["units", "same_as", "label"].contains(&k.as_str())
                && !IGNORED_EXPECT.contains(&k.as_str())
            {
                self.fail(format!("the runner does not compare `{k}`"));
            }
        }
    }

    fn compare_units(&mut self, got: &[DerivedUnit], want: &[Value]) {
        if got.len() != want.len() {
            let names: Vec<String> = got
                .iter()
                .map(|u| format!("{}={}", u.body.aspect, u.body.value))
                .collect();
            self.fail(format!(
                "{} units, want {}: {names:?}",
                got.len(),
                want.len()
            ));
        }
        let value_of = |id: Id| {
            got.iter()
                .find(|u| u.id == id)
                .map(|u| u.body.value.clone())
        };
        for w in want {
            let w = w.as_table().expect("a unit is a table");
            let layer = str_of(w, "layer").unwrap_or("world");
            let entity = str_of(w, "entity").unwrap_or("self");
            let aspect = str_of(w, "aspect").expect("aspect");
            let value = str_of(w, "value").expect("value");
            let free = w.get("free").and_then(Value::as_bool).unwrap_or(false);
            let Some(u) = got.iter().find(|u| {
                let b = &u.body;
                (b.layer == UnitLayer::Episode) == (layer == "episode")
                    && b.entity == entity
                    && b.aspect == aspect
                    && b.free == free
                    && b.value == value
            }) else {
                self.fail(format!("no unit {entity}/{aspect}={value}"));
                continue;
            };
            let b = &u.body;
            let tag = format!("{entity}/{aspect}={value}");
            let tense = match b.tense {
                UnitTense::Current => "current",
                UnitTense::Past => "past",
                UnitTense::Planned => "planned",
            };
            if tense != str_of(w, "tense").unwrap_or("current") {
                self.fail(format!("{tag}: tense {tense}"));
            }
            if b.private != w.get("private").and_then(Value::as_bool).unwrap_or(false) {
                self.fail(format!("{tag}: private {}", b.private));
            }
            let state = b.state.map(|s| match s {
                UnitState::Live => "Live",
                UnitState::Contested => "Contested",
                UnitState::Superseded => "Superseded",
            });
            if state != str_of(w, "state") {
                self.fail(format!("{tag}: state {state:?}"));
            }
            if let Some(a) = w.get("anchors").and_then(Value::as_array) {
                let want: BTreeSet<String> = a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect();
                let have: BTreeSet<String> =
                    b.anchors.iter().map(|x| self.name_of(x.turn)).collect();
                if want != have {
                    self.fail(format!("{tag}: anchors {have:?}"));
                }
            }
            for (k, field) in [("by", b.by), ("against", b.against)] {
                if let Some(x) = str_of(w, k) {
                    let have = field
                        .and_then(value_of)
                        .unwrap_or_else(|| "(none)".to_owned());
                    if have != x {
                        self.fail(format!("{tag}: {k} {have}"));
                    }
                }
            }
            if let Some(when) = w.get("when").and_then(Value::as_table) {
                let basis = |s: &str| match s {
                    "stated" => WhenBasis::Stated,
                    "relative" => WhenBasis::Relative,
                    "utterance" => WhenBasis::Utterance,
                    _ => WhenBasis::NoOffset,
                };
                let want = (
                    str_of(when, "from").map(str::to_owned),
                    str_of(when, "to").map(str::to_owned),
                    basis(str_of(when, "basis").expect("basis")),
                );
                let have = b
                    .when
                    .as_ref()
                    .map(|x| (x.from.clone(), x.to.clone(), x.basis));
                if have.as_ref() != Some(&want) {
                    self.fail(format!("{tag}: when {have:?}"));
                }
            }
        }
    }

    fn step(&mut self, s: &Table) {
        if let Some(t) = s.get("turn").and_then(Value::as_table) {
            self.add_turn(t);
        } else if let Some(seg) = s.get("segment").and_then(Value::as_table) {
            let name = str_of(seg, "name").expect("name").to_owned();
            let pairs: Vec<Id> = seg["pairs"]
                .as_array()
                .expect("pairs")
                .iter()
                .map(|p| self.names[p.as_str().expect("a turn name")])
                .collect();
            self.segments.insert(name, pairs);
        } else if let Some(l) = s.get("ledger").and_then(Value::as_table) {
            self.add_ledger(l);
        } else if let Some(op) = s.get("op").and_then(Value::as_table) {
            match str_of(op, "kind").expect("kind") {
                "derive" => {
                    let shuffle = (str_of(op, "order") == Some("shuffle")).then(|| {
                        u64::try_from(op["seed"].as_integer().expect("seed")).expect("fits")
                    });
                    self.derive(shuffle);
                }
                "select" => {
                    let model = str_of(op, "model").expect("model").to_owned();
                    let old = std::mem::replace(&mut self.current, model);
                    self.previous = Some(old);
                }
                k if IGNORED_OPS.contains(&k) => {
                    if k == "restart" {
                        self.cache.clear();
                    }
                }
                k => self.fail(format!("the runner does not run op `{k}`")),
            }
        } else if let Some(e) = s.get("expect").and_then(Value::as_table) {
            self.expect(e);
        } else {
            self.fail(format!("unknown step {s:?}"));
        }
    }
}

/// `derive.toml` の M5.3 の事例 (DR03 の導出の決定性、DR06 の単位と日付)。
///
/// verifies: AC-FR-315-1 (DR03: 読む順を乱しても単位の集合・本体・指紋が同じ)、AC-FR-315-4 (DR03・DR06: 相対の日付を
/// 発話のターンの UTC からのずれで解き、ずれの無いターンは `no_offset` を残す)
#[test]
fn derive_cases_of_m5_3_run() {
    let all = cases();
    let mut problems = Vec::new();
    for id in CASES {
        let case = all
            .iter()
            .find(|c| str_of(c, "id") == Some(id))
            .unwrap_or_else(|| panic!("no case {id}"));
        assert_eq!(str_of(case, "stage"), Some("M5.3"), "{id}");
        let mut run = Run {
            case: (*id).to_owned(),
            current: "m0".to_owned(),
            ..Run::default()
        };
        for s in case["step"].as_array().expect("steps") {
            run.step(s.as_table().expect("a step is a table"));
        }
        problems.extend(run.problems);
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// M5.3 の事例は、すべてここで走らせる (ほかの段の事例は、その段で走らせる)。
#[test]
fn every_m5_3_case_is_run() {
    let staged: Vec<String> = cases()
        .iter()
        .filter(|c| str_of(c, "stage") == Some("M5.3"))
        .filter_map(|c| str_of(c, "id").map(str::to_owned))
        .collect();
    assert_eq!(staged, CASES);
}
