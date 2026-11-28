//! 試験の事例 (`tests/memory-cases/v0.5/`) のうち、純粋な導出で決まるものを走らせる小さな走らせる側。
//!
//! 走らせるのは [`CASES`] の事例の、ターン・区切り・選択・台帳・導出・`import_v04` の段と、期待の `units` (完全に
//! 比べる)・`label`・`same_as` (単位の集合・本体・導出の指紋)・`ends`・`not_live`・`counts` (書いた関門の理由)・
//! `doctor` (`import_private_unmatched`)・`profile` (導いた単位から `profile::rows`・`render` で作った行。`absent` は
//! `forget_key` の段の鍵の状態で `profile::decide` が置かないと決めること)。区切りの発話に前のふるいと文の分け方 v1 を当てて台帳の `sentences` と訂正の
//! 候補の表 (`corr`) を作り、区切りごとに `select_ledger` で使う台帳を選んでから導く。取り込んだ根では、導くたびに
//! `import.v04_ended` の表で `end` を足して導き直す (返るものが無くなるまで)。`order = "shuffle"` の導出は、台帳を
//! 読む順 (区切りの到着の順・同じ区切りの複数の選択の台帳の順) と、台帳の中の候補の順を乱す。
//!
//! 走らせない段 (再起動) は [`IGNORED_OPS`]、比べない期待 (台帳の掃除の期待は書き込みの経路の PR) は
//! [`IGNORED_EXPECT`] に挙げる。

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;

use evorag_types::PromptLanguage;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use toml::{Table, Value};

use super::tests::id_at;
use super::*;
use crate::apply::ValueKeys;
use crate::b3::{Dictionary, Rules};
use crate::corrections::{CORRECTION_V1, find_holes};
use crate::directive::{DirectiveBody, DirectiveOp, TurnSpan};
use crate::import_v04::{EndedValue, V04Ended};
use crate::ledger::{ByteRange, CALL_SEGMENT, CorrInput, LedgerBody, LedgerOutput, SentenceSpan};
use crate::ledger_select::{
    Current, RuleVersions as SelectVersions, SelectEnv, Selection, StoredLedger, select_ledger,
};
use crate::prefilter::{MODE_CHAT, PREFILTER_V1, Screened, prefilter};
use crate::profile::{self, History, KeyState, PROFILE_BUDGET, Place, SwapPoint};
use crate::sentences::{SPLITTER_V1, split_v1};
use crate::unit::{UnitState, UnitTense, WhenBasis};
use crate::value_key::{ForgetKey, value_key};

const KEY: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

/// 走らせる事例 (ファイル, ID)。
const CASES: &[(&str, &str)] = &[
    ("derive.toml", "DR03"),
    ("derive.toml", "DR06"),
    ("governance.toml", "GV10"),
    ("governance.toml", "GV11"),
    ("governance.toml", "GV12"),
    ("attacks.toml", "AT01"),
    ("attacks.toml", "AT03"),
    ("attacks.toml", "AT04"),
    ("attacks.toml", "AT06"),
    ("attacks.toml", "AT07"),
    ("attacks.toml", "AT08"),
    ("attacks.toml", "AT09"),
    ("attacks.toml", "AT10"),
    ("attacks.toml", "AT11"),
];

/// `stage = "M5.3"` のうち、ここで走らせない事例。攻撃の事例は関門の走らせる側 (`b3::cases`) も関門で決まる部分を
/// 走らせ、ここでは導出とプロファイルまでを走らせる (区切りの鍵の期待 (`seg_keys`) を持つ AT02・AT05 は `b3::cases`
/// だけ)。FI02 は書き手と強制終了の注入が要る (core の PR で走らせる)。
const ELSEWHERE: &[&str] = &["AT02", "AT05", "FI02"];

/// 走らせない操作。
const IGNORED_OPS: &[&str] = &["restart"];

/// 比べない期待の項目。
const IGNORED_EXPECT: &[&str] = &["ledgers"];

/// 事例の変種 (台帳の `variant` の既定)。
const VARIANT: &str = "v1";

fn root() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn cases(file: &str) -> Vec<Table> {
    let path = root().join("../../tests/memory-cases/v0.5").join(file);
    let text = std::fs::read_to_string(&path).expect(file);
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
    let n = |r: Range<usize>| -> i64 { s[r].parse().expect("a number") };
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

fn byte_range(r: &Range<usize>) -> ByteRange {
    ByteRange {
        start: u32::try_from(r.start).expect("fits"),
        end: u32::try_from(r.end).expect("fits"),
    }
}

struct Turn {
    id: Id,
    session: Id,
    speaker: Speaker,
    text: String,
    tz: Option<i32>,
    long: Vec<Range<usize>>,
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
    directives: Vec<(Id, DirectiveBody)>,
    /// `import.v04_ended` の表と、取り込んだ時点。
    imported: Option<(V04Ended, Id)>,
    current: String,
    previous: Option<String>,
    derived: Option<Snapshot>,
    last: Option<Derived>,
    labels: BTreeMap<String, Snapshot>,
    cache: GateCache,
    problems: Vec<String>,
    seq: u16,
    /// 事例の言語 (`lang`。省くと `ja`)。
    language: Option<PromptLanguage>,
    /// `forget_key` の段で鍵を使えなくした。
    key_unavailable: bool,
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

    /// これまでのターンより後の ID (台帳・指示の記録の ID)。
    fn next_id(&mut self) -> Id {
        let latest = self.turns.iter().map(|t| t.id).max().expect("turns");
        self.seq += 1;
        id_at(latest.timestamp().as_millis() + 1, self.seq)
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
        let text = str_of(t, "text").expect("text").to_owned();
        let long = if t.get("long_input").and_then(Value::as_bool) == Some(true) {
            std::iter::once(0..text.len()).collect()
        } else {
            Vec::new()
        };
        self.names.insert(name, id);
        self.turns.push(Turn {
            id,
            session,
            speaker,
            text,
            tz: parse_tz(str_of(t, "tz").unwrap_or("+09:00")),
            long,
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
        let mut corr = Vec::new();
        let mut n = 1;
        for t in &pairs {
            let text = &self.turn(*t).text;
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
        let list = |k: &str| -> Vec<Json> {
            l.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|v| serde_json::to_value(v).expect("json"))
                        .collect()
                })
                .unwrap_or_default()
        };
        let units: Vec<Json> = l
            .get("units")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(wire_unit).collect())
            .unwrap_or_default();
        let mut output = LedgerOutput::default();
        output
            .set_section(LedgerOutput::KEYS, &list("keys"))
            .expect("keys");
        output
            .set_section(LedgerOutput::UNITS, &units)
            .expect("units");
        output
            .set_section(LedgerOutput::CORRECTIONS, &list("corrections"))
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
        body.inputs.corr = corr;
        let id = self.next_id();
        self.ledgers.push((id, body, units));
    }

    /// `import_v04` の段: v0.4 の事実の表から `import.v04_ended` を作り、指示を写す (03 §12.4)。
    fn import(&mut self, op: &Table) {
        let v04 = op["v04"].as_table().expect("v04");
        let mut values = Vec::new();
        for f in v04["facts"].as_array().expect("facts") {
            let f = f.as_table().expect("a fact");
            let state = str_of(f, "state").expect("state");
            if state != "Superseded" && state != "Retracted" {
                continue;
            }
            let turn = f["sources"]
                .as_array()
                .expect("sources")
                .iter()
                .map(|s| self.names[s.as_str().expect("a turn name")])
                .max();
            values.push(EndedValue {
                entity: str_of(f, "entity").unwrap_or("self").to_owned(),
                aspect: str_of(f, "aspect").expect("aspect").to_owned(),
                vks: ValueKeys::of(&KEY, str_of(f, "value").expect("value")).to_hex(),
                turn,
            });
        }
        for d in v04["directives"].as_array().expect("directives") {
            let d = d.as_table().expect("a directive");
            let target = d["target"].as_table().expect("target");
            let value = str_of(target, "value").expect("value");
            let op = match str_of(d, "op").expect("op") {
                "forget" => DirectiveOp::Forget,
                "private" => DirectiveOp::Private,
                k => {
                    self.fail(format!("the runner does not copy a v0.4 `{k}`"));
                    return;
                }
            };
            let mut b = DirectiveBody::new(op);
            b.entity = Some(str_of(target, "entity").unwrap_or("self").to_owned());
            b.aspect = Some(str_of(target, "aspect").expect("aspect").to_owned());
            b.norm = Some(1);
            b.imported = true;
            if op == DirectiveOp::Forget {
                b.vks = ValueKeys::of(&KEY, value).to_hex();
                b.redact = d.get("redact").and_then(Value::as_bool).unwrap_or(true);
            } else {
                b.vks = vec![value_key(&KEY, value).to_hex()];
            }
            if let Some(at) = d.get("at").and_then(Value::as_table) {
                let turn = self.names[str_of(at, "turn").expect("turn")];
                let text = &self.turn(turn).text;
                let start = text
                    .find(str_of(at, "text").expect("text"))
                    .expect("the range text is in the turn");
                let len = str_of(at, "text").expect("text").len();
                b.at = Some(TurnSpan {
                    turn,
                    rev: 0,
                    start: u32::try_from(start).expect("fits"),
                    end: u32::try_from(start + len).expect("fits"),
                });
            }
            let id = self.next_id();
            self.directives.push((id, b));
        }
        let at = self.next_id();
        self.imported = Some((
            V04Ended {
                v: 1,
                done: false,
                values,
            },
            at,
        ));
    }

    fn turn_inputs(&self) -> BTreeMap<Id, TurnInput<'_>> {
        let mut order: Vec<&Turn> = self.turns.iter().collect();
        order.sort_by_key(|t| t.id);
        let mut last: BTreeMap<Id, Id> = BTreeMap::new();
        order
            .into_iter()
            .map(|t| {
                (
                    t.id,
                    TurnInput {
                        session: t.session,
                        prev: last.insert(t.session, t.id),
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

    /// 全件の導出 (取り込んだ根では、表の `end` を足して導き直す)。`shuffle` があれば、台帳を読む順と台帳の中の候補の
    /// 順をその種で乱す。
    fn derive(&mut self, shuffle: Option<u64>) {
        let rules = Rules::bundled();
        let resolve = |s: &str| rules.resolve_entity(s);
        for _ in 0..8 {
            let d = self.derive_once(shuffle);
            let Some((table, at)) = &self.imported else {
                self.last = Some(d);
                return;
            };
            let directives =
                DirectiveSet::new(self.directives.iter().cloned(), "[伏せ字]", resolve);
            let ends = import_ends(&d, table, &directives, &KEY, *at, resolve);
            if ends.is_empty() {
                self.last = Some(d);
                return;
            }
            for e in ends {
                let id = self.next_id();
                self.directives.push((id, e.body()));
            }
        }
        self.fail("the import ends do not settle".to_owned());
    }

    fn derive_once(&mut self, shuffle: Option<u64>) -> Derived {
        let mut cache = std::mem::take(&mut self.cache);
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
        let turns = self.turn_inputs();
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
            versions: SelectVersions::current(CORRECTION_V1),
            turn_rev: |id: Id| turns.get(&id).map(|t| t.rev),
        };
        let stored: Vec<StoredLedger<'_>> = ledgers
            .iter()
            .map(|(id, body, _)| StoredLedger { id: *id, body })
            .collect();
        let segments: BTreeSet<Id> = self.segments.values().map(|p| p[0]).collect();
        let selected: Vec<Selected<'_>> = segments
            .iter()
            .filter_map(|s| select_ledger(*s, &stored, &env))
            .collect();
        let rules = Rules::bundled();
        let directives = DirectiveSet::new(self.directives.iter().cloned(), "[伏せ字]", |s| {
            rules.resolve_entity(s)
        });
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
        let d = super::derive(&inputs, &mut cache);
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
            directives: DirectiveStamp {
                count: u64::try_from(self.directives.len()).expect("fits"),
                max: self.directives.iter().map(|(id, _)| *id).max(),
            },
            versions: &versions,
            current: Current::Ready(current),
            previous,
        });
        self.cache = cache;
        self.derived = Some((d.units.clone(), f));
        d
    }

    fn expect(&mut self, e: &Table) {
        let (Some((units, f)), Some(last)) = (self.derived.clone(), self.last.clone()) else {
            self.fail("an expectation before any derivation".to_owned());
            return;
        };
        if let Some(want) = e.get("units").and_then(Value::as_array) {
            self.compare_units(&units, want);
        }
        if let Some(label) = str_of(e, "same_as") {
            let Some((u0, f0)) = self.labels.get(label).cloned() else {
                self.fail(format!("no label {label}"));
                return;
            };
            if u0 != units {
                self.fail(format!("units are not the same as {label}"));
            }
            if f0 != f {
                self.fail(format!("the fingerprint is not the same as {label}"));
            }
        }
        if let Some(label) = str_of(e, "label") {
            self.labels.insert(label.to_owned(), (units.clone(), f));
        }
        if let Some(want) = e.get("ends").and_then(Value::as_array) {
            self.compare_ends(want);
        }
        if let Some(want) = e.get("not_live").and_then(Value::as_array) {
            for w in want.iter().filter_map(Value::as_str) {
                let live = units.iter().any(|u| {
                    let b = &u.body;
                    b.state == Some(UnitState::Live)
                        && format!("{}/{}={}", b.entity, b.aspect, b.value) == w
                });
                if live {
                    self.fail(format!("{w} is live"));
                }
            }
        }
        if let Some(want) = e.get("counts").and_then(Value::as_table) {
            for (name, n) in want {
                let Some(r) = Reason::ALL.iter().find(|r| r.as_str() == name) else {
                    self.fail(format!(
                        "the runner compares only gate reasons, not `{name}`"
                    ));
                    continue;
                };
                let have = last.counts.gate(*r);
                if i64::try_from(have).ok() != n.as_integer() {
                    self.fail(format!("count {name}: {have}"));
                }
            }
        }
        if let Some(want) = e.get("profile") {
            self.compare_profile(&last, want);
        }
        if let Some(want) = e.get("doctor").and_then(Value::as_array) {
            for w in want.iter().filter_map(Value::as_str) {
                let rules = Rules::bundled();
                let directives =
                    DirectiveSet::new(self.directives.iter().cloned(), "[伏せ字]", |s| {
                        rules.resolve_entity(s)
                    });
                let ok = match w {
                    "import_private_unmatched" => {
                        !unmatched_imported_privates(&last, &directives, &KEY).is_empty()
                    }
                    _ => false,
                };
                if !ok {
                    self.fail(format!("doctor does not report {w}"));
                }
            }
        }
        for k in e.keys() {
            let known = [
                "units", "same_as", "label", "ends", "not_live", "counts", "doctor", "profile",
            ];
            if !known.contains(&k.as_str()) && !IGNORED_EXPECT.contains(&k.as_str()) {
                self.fail(format!("the runner does not compare `{k}`"));
            }
        }
    }

    /// プロファイルの行を比べる (README の `profile`): 属性と entity の組の並びは順で、同じ組の中は集合で比べる。
    fn compare_profile(&mut self, last: &Derived, want: &Value) {
        let language = self.language.unwrap_or(PromptLanguage::Ja);
        let rules = Rules::bundled();
        let rows = profile::rows(
            last.units.iter().map(|u| (u.id, &u.body)),
            Dictionary::bundled(),
            &rules,
            language,
        );
        let made = profile::render(&rows, language, PROFILE_BUDGET, |s| s.chars().count());
        let at = SwapPoint {
            governance: 0,
            key: if self.key_unavailable {
                KeyState::Unavailable
            } else {
                KeyState::Available
            },
            history_tokens: 0,
            history: History::Kept,
        };
        let place = profile::decide(None, made.as_ref().map(|p| p.version.as_str()), &at).place;
        if let Some(t) = want.as_table() {
            if t.get("absent").and_then(Value::as_bool) == Some(true) && place != Place::Omit {
                self.fail("the profile is placed".to_owned());
            }
            return;
        }
        if place == Place::Omit {
            self.fail("the profile is omitted".to_owned());
            return;
        }
        let tags: Vec<String> = made
            .iter()
            .flat_map(|p| &p.units)
            .filter_map(|id| last.get(*id))
            .map(|u| {
                let b = &u.body;
                let planned = if b.tense == UnitTense::Planned {
                    " (予定)"
                } else {
                    ""
                };
                format!("{}/{}={}{planned}", b.entity, b.aspect, b.value)
            })
            .collect();
        let want: Vec<String> = want
            .as_array()
            .expect("profile rows")
            .iter()
            .map(|v| v.as_str().expect("a row").to_owned())
            .collect();
        // 同じ (entity, 属性) の続く行を 1 つの組にまとめる (組の中の順は値の鍵の順で、データ根の鍵に依る)。
        let grouped = |rows: &[String]| -> Vec<(String, BTreeSet<String>)> {
            let mut out: Vec<(String, BTreeSet<String>)> = Vec::new();
            for r in rows {
                let group = r.split_once('=').map_or(r.as_str(), |(g, _)| g).to_owned();
                match out.last_mut() {
                    Some((g, set)) if *g == group => {
                        set.insert(r.clone());
                    }
                    _ => out.push((group, BTreeSet::from([r.clone()]))),
                }
            }
            out
        };
        if grouped(&tags) != grouped(&want) {
            self.fail(format!("profile {tags:?}"));
        }
    }

    fn compare_ends(&mut self, want: &[Value]) {
        let ends: Vec<(Id, DirectiveBody)> = self
            .directives
            .iter()
            .filter(|(_, b)| b.op() == DirectiveOp::End)
            .cloned()
            .collect();
        if ends.len() != want.len() {
            self.fail(format!("{} ends, want {}", ends.len(), want.len()));
        }
        for w in want {
            let w = w.as_table().expect("an end");
            let aspect = str_of(w, "aspect").expect("aspect");
            let vk = value_key(&KEY, str_of(w, "value").expect("value")).to_hex();
            let origin = str_of(w, "origin").unwrap_or("user");
            let turn = str_of(w, "turn").expect("turn");
            let found = ends.iter().any(|(id, b)| {
                let at = b.turn.unwrap_or(*id);
                let at_name = if b.turn.is_none() {
                    "(self)".to_owned()
                } else {
                    self.name_of(at)
                };
                b.aspect.as_deref() == Some(aspect)
                    && b.vks.contains(&vk)
                    && b.origin().name() == origin
                    && at_name == turn
            });
            if !found {
                self.fail(format!("no end {aspect} at {turn} ({origin})"));
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
                "import_v04" => self.import(op),
                "forget_key" => {
                    self.key_unavailable =
                        op.get("available").and_then(Value::as_bool) == Some(false);
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

/// M5.3 の事例 (DR03 の導出の決定性、DR06 の単位と日付とプロファイル、GV10〜GV12 の取り込みの `end` と private、
/// 攻撃の事例の単位とプロファイル (AT03 の長い一度の入力、AT09 のプロファイルの書式を壊す型など))。
///
/// verifies: AC-FR-316-1 (DR06・AT09 など: プロファイルの行)、AC-FR-315-1 (DR03: 読む順を乱しても単位の集合・本体・指紋が同じ)、AC-FR-315-4 (DR03・DR06: 相対の日付を
/// 発話のターンの UTC からのずれで解き、ずれの無いターンは `no_offset` を残す)
#[test]
fn derive_cases_of_m5_3_run() {
    let mut problems = Vec::new();
    for (file, id) in CASES {
        let all = cases(file);
        let case = all
            .iter()
            .find(|c| str_of(c, "id") == Some(id))
            .unwrap_or_else(|| panic!("no case {id}"));
        assert_eq!(str_of(case, "stage"), Some("M5.3"), "{id}");
        let mut run = Run {
            case: (*id).to_owned(),
            current: "m0".to_owned(),
            language: str_of(case, "lang").map(|l| PromptLanguage::parse(l).expect("a language")),
            ..Run::default()
        };
        for s in case["step"].as_array().expect("steps") {
            run.step(s.as_table().expect("a step is a table"));
        }
        problems.extend(run.problems);
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// M5.3 の事例は、ここか、挙げたほかの走らせる側で走らせる。
#[test]
fn every_m5_3_case_is_run() {
    let mut staged: BTreeSet<String> = BTreeSet::new();
    for file in [
        "derive.toml",
        "governance.toml",
        "attacks.toml",
        "forget.toml",
        "faults.toml",
    ] {
        staged.extend(
            cases(file)
                .iter()
                .filter(|c| str_of(c, "stage") == Some("M5.3"))
                .filter_map(|c| str_of(c, "id").map(str::to_owned)),
        );
    }
    let covered: BTreeSet<String> = CASES
        .iter()
        .map(|(_, id)| *id)
        .chain(ELSEWHERE.iter().copied())
        .map(str::to_owned)
        .collect();
    assert_eq!(staged, covered);
}
