//! 試験の事例 (`tests/memory-cases/v0.5/attacks.toml`) のうち、関門 (B3) で決まる部分を走らせる小さな走らせる側。
//!
//! 事例の全体 (導出・プロファイル・検索の鍵) は M5.3 で走らせる。ここでは、区切りの発話に前のふるいと文の分け方 v1
//! を当てて台帳の `sentences` を作り、台帳の `units` の各々に関門を当てて、次を確かめる。
//!
//! - `expect` の `counts` の関門の理由 ([`Reason`]) の数。`counts` に書いていない理由が出たら外れとする (`counts`
//!   が無ければ、どの理由も出てはならない)。
//! - 通った候補の (主語, 属性, 値, 時制) の集合が、`expect` の `units` の集合と同じ (同じ値の候補は導出で 1 つの
//!   単位になるので集合で比べる)。通った候補の叙法は、台帳の候補の叙法と同じ。
//!
//! 操作は `forget` (伏せ字にしない忘れる。`redact = false` だけ) を扱う。指示の `[非表示]` の範囲は、その時点の
//! 利用者の発話の中の値の一致 (生の文字列) にし、後の台帳の発話は [`Utterance::with_directives`] で作る。

use std::collections::{BTreeMap, BTreeSet};

use toml::{Table, Value};

use super::*;
use crate::directive::{DirectiveBody, DirectiveOp, TurnSpan};
use crate::prefilter::{MODE_CHAT, Screened, prefilter};
use crate::sentences::split_v1;

const KEY: ForgetKey = ForgetKey::from_bytes_const([7; 32]);

struct Turn {
    id: Id,
    user: bool,
    text: String,
}

fn str_of<'v>(t: &'v Table, k: &str) -> Option<&'v str> {
    t.get(k).and_then(Value::as_str)
}

fn ledger_unit(v: &Value) -> LedgerUnit {
    let t = v.as_table().expect("a unit is a table");
    let s = |k: &str, d: &str| str_of(t, k).unwrap_or(d).to_owned();
    let parse = |k: &str, d: &str| -> Value { Value::String(s(k, d)) };
    LedgerUnit {
        sentence: u32::try_from(t["sentence"].as_integer().expect("sentence")).expect("fits"),
        layer: parse("layer", "world").try_into().expect("layer"),
        entity: s("entity", "self"),
        aspect: s("aspect", ""),
        value: s("value", ""),
        mood: parse("mood", "fact").try_into().expect("mood"),
        tense: parse("tense", "current").try_into().expect("tense"),
        old: s("old", ""),
        when: s("when", ""),
    }
}

/// 区切りの往復の発話に、前のふるいと文の分け方 v1 を当てて文番号を振る (04 §5.2 の「文番号」)。
fn sentences(turns: &[&Turn]) -> Vec<SentenceSpan> {
    let mut out = Vec::new();
    let mut n = 1;
    for t in turns {
        let Screened::Send(f) = prefilter(MODE_CHAT, &t.text) else {
            continue;
        };
        for r in split_v1(f.text()) {
            let stored = f.to_stored(r).expect("a v1 sentence stays in one piece");
            out.push(SentenceSpan::new(
                n,
                t.id,
                0,
                u32::try_from(stored.start).expect("fits"),
                u32::try_from(stored.end).expect("fits"),
            ));
            n += 1;
        }
    }
    out
}

/// 期待と比べる単位の形 (主語, 属性, 値, 時制)。
type UnitKey = (String, String, String, &'static str);

fn tense_name(t: UnitTense) -> &'static str {
    match t {
        UnitTense::Current => "current",
        UnitTense::Past => "past",
        UnitTense::Planned => "planned",
    }
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).expect("fits")
}

/// 伏せ字にしない忘れるの指示 (`op = { kind = "forget", target, redact = false }`)。
fn forget_directive(
    op: &Table,
    turns: &BTreeMap<String, Turn>,
    rules: &Rules<'_>,
) -> DirectiveBody {
    let target = op["target"].as_table().expect("target");
    let value = str_of(target, "value").expect("value");
    let aspect = str_of(target, "aspect").expect("aspect");
    let free = target
        .get("free")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| Dictionary::bundled().get(aspect).is_none());
    let mut b = DirectiveBody::new(DirectiveOp::Forget);
    b.entity = Some(normalize_v1(
        &rules.resolve_entity(str_of(target, "entity").unwrap_or("self")),
    ));
    if free {
        b.aspect_mac = Some(aspect_key(&KEY, &normalize_v1(aspect)).to_hex());
    } else {
        b.aspect = Some(aspect.to_owned());
    }
    b.vks = ValueKeys::of(&KEY, value).to_hex();
    b.norm = Some(1);
    assert_eq!(
        op.get("redact").and_then(Value::as_bool),
        Some(false),
        "the runner handles only forgets without redaction"
    );
    b.hidden = turns
        .values()
        .filter(|t| t.user)
        .flat_map(|t| {
            t.text.match_indices(value).map(|(i, _)| TurnSpan {
                turn: t.id,
                rev: 0,
                start: u32_of(i),
                end: u32_of(i + value.len()),
            })
        })
        .collect();
    b
}

/// 事例 1 つを走らせる。外れた点の説明の並びを返す (空なら合う)。
fn run_case(case: &Table) -> Vec<String> {
    let case_id = str_of(case, "id").unwrap_or("?");
    let rules = Rules::bundled();
    let env = Env {
        dictionary: Dictionary::bundled(),
        rules: &rules,
        key: &KEY,
    };
    let mut bodies: Vec<(Id, DirectiveBody)> = Vec::new();
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    let mut segments: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut counts: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut passed: BTreeSet<UnitKey> = BTreeSet::new();
    let mut problems = Vec::new();
    let steps = case["step"].as_array().expect("steps");
    for step in steps {
        let step = step.as_table().expect("a step is a table");
        if let Some(t) = step.get("turn").and_then(Value::as_table) {
            let name = str_of(t, "id").expect("turn id").to_owned();
            let n = u8::try_from(turns.len() + 1).expect("few turns");
            turns.insert(
                name,
                Turn {
                    id: Id::from_bytes([n; 16]),
                    user: str_of(t, "role").unwrap_or("user") == "user",
                    text: str_of(t, "text").expect("turn text").to_owned(),
                },
            );
        } else if let Some(s) = step.get("segment").and_then(Value::as_table) {
            let pairs = s["pairs"]
                .as_array()
                .expect("pairs")
                .iter()
                .map(|p| p.as_str().expect("turn name").to_owned())
                .collect();
            segments.insert(str_of(s, "name").expect("segment name").to_owned(), pairs);
        } else if let Some(op) = step.get("op").and_then(Value::as_table) {
            match str_of(op, "kind") {
                Some("forget") => {
                    let id = Id::from_bytes([200 + u8::try_from(bodies.len()).expect("few"); 16]);
                    bodies.push((id, forget_directive(op, &turns, &rules)));
                }
                Some("derive") => {}
                other => problems.push(format!(
                    "{case_id}: the runner does not handle op {other:?}"
                )),
            }
        } else if let Some(l) = step.get("ledger").and_then(Value::as_table) {
            let directives = DirectiveSet::new(bodies.iter().cloned(), "[伏せ字]", |s| {
                rules.resolve_entity(s)
            });
            let seg = &segments[str_of(l, "seg").expect("seg")];
            let users: Vec<&Turn> = seg.iter().map(|n| &turns[n]).filter(|t| t.user).collect();
            let table = sentences(&users);
            let mut unmatched = Vec::new();
            let utterances: Vec<Utterance<'_>> = users
                .iter()
                .filter_map(|t| {
                    let u = Utterance::with_directives(t.id, 0, &t.text, &directives);
                    if u.is_none() {
                        unmatched.push(t.id);
                    }
                    u
                })
                .collect();
            let ev = Evidence::new(&table, utterances).with_unmatched(unmatched);
            for u in l
                .get("units")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let u = ledger_unit(u);
                match check(&env, &ev, &directives, &u) {
                    Ok(p) => {
                        let same_mood = matches!(
                            (p.mood, u.mood),
                            (Mood::Fact, LedgerMood::Fact) | (Mood::Negated, LedgerMood::Negated)
                        );
                        if !same_mood {
                            problems.push(format!("{case_id}: mood of {:?} changed", u.value));
                        }
                        passed.insert((
                            p.entity,
                            p.aspect.as_str().to_owned(),
                            p.value.text,
                            tense_name(p.tense),
                        ));
                    }
                    Err(r) => *counts.entry(r.as_str()).or_default() += 1,
                }
            }
        } else if let Some(e) = step.get("expect").and_then(Value::as_table) {
            let want: BTreeMap<String, i64> = e
                .get("counts")
                .and_then(Value::as_table)
                .map(|t| {
                    t.iter()
                        .filter(|(name, _)| Reason::ALL.iter().any(|r| r.as_str() == name.as_str()))
                        .map(|(name, n)| (name.clone(), n.as_integer().expect("a count")))
                        .collect()
                })
                .unwrap_or_default();
            let got: BTreeMap<String, i64> =
                counts.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
            if got != want {
                problems.push(format!("{case_id}: counts {got:?}, want {want:?}"));
            }
            if let Some(units) = e.get("units").and_then(Value::as_array) {
                let want: BTreeSet<UnitKey> = units
                    .iter()
                    .map(|u| {
                        let t = u.as_table().expect("a unit is a table");
                        (
                            normalize_v1(str_of(t, "entity").unwrap_or("self")),
                            str_of(t, "aspect").expect("aspect").to_owned(),
                            str_of(t, "value").expect("value").to_owned(),
                            match str_of(t, "tense") {
                                Some("past") => "past",
                                Some("planned") => "planned",
                                _ => "current",
                            },
                        )
                    })
                    .collect();
                if want != passed {
                    problems.push(format!("{case_id}: passed {passed:?}, want {want:?}"));
                }
            }
        }
    }
    problems
}

fn run_file(text: &str) -> (usize, Vec<String>) {
    let file: Table = toml::from_str(text).expect("the case file parses");
    let cases = file["case"].as_array().expect("cases");
    let problems = cases
        .iter()
        .flat_map(|c| run_case(c.as_table().expect("a case is a table")))
        .collect();
    (cases.len(), problems)
}

/// `attacks.toml` の全事例の、関門で決まる部分 (09 §3.5.2 の「同梱の攻撃の事例」)。
#[test]
fn attack_cases_are_stopped_by_the_gates() {
    let (n, problems) = run_file(include_str!(
        "../../../../tests/memory-cases/v0.5/attacks.toml"
    ));
    assert!(n >= 11);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// 走らせる側の `[非表示]` と忘れるの指示の扱い (同じ書式の事例を、ここに置く)。
const FORGET_CASES: &str = r#"
[[case]]
id = "B3F1"
title = "伏せ字にしない忘れるの後: 隠れた値は一致せず、隠れていない書き方の値は関門 8 で落ち、同じ文のほかの値は通る"
[[case.step]]
turn = { id = "a1", session = "A", at = "2026-06-01T10:00:00Z", text = "趣味は将棋と囲碁です。" } # 1
[[case.step]]
op = { kind = "forget", target = { aspect = "hobby", value = "将棋" }, redact = false }
[[case.step]]
turn = { id = "a2", session = "A", at = "2026-06-02T10:00:00Z", text = "週末は将 棋を指します。" } # 2
[[case.step]]
segment = { name = "S1", pairs = ["a1", "a2"] }
[[case.step]]
ledger = { name = "L1", seg = "S1", keys = ["趣味"], units = [
  { sentence = 1, aspect = "hobby", value = "将棋" },
  { sentence = 1, aspect = "hobby", value = "囲碁" },
  { sentence = 2, aspect = "hobby", value = "将 棋" },
  { sentence = 2, entity = "私", aspect = "hobby", value = "将棋" },
] }
[[case.step]]
expect = { units = [{ aspect = "hobby", value = "囲碁" }], counts = { value_not_in_evidence = 1, forgotten = 2 } }
"#;

#[test]
fn hidden_ranges_and_forgets_run_in_the_case_format() {
    let (n, problems) = run_file(FORGET_CASES);
    assert_eq!(n, 1);
    assert!(problems.is_empty(), "{problems:#?}");
}
