//! 試験の事例 (`tests/memory-cases/v0.5/attacks.toml`) のうち、関門 (B3) で決まる部分を走らせる小さな走らせる側。
//!
//! 事例の全体 (導出・プロファイル・検索の鍵) は M5.3 で走らせる。ここでは、区切りの発話に前のふるいと文の分け方 v1
//! を当てて台帳の `sentences` を作り、台帳の `units` の各々に関門を当てて、`expect` の `counts` のうち関門の理由
//! ([`Reason`]) の数と、通った候補の (属性, 値) の集合が `expect` の `units` の (属性, 値) の集合と同じことを
//! 確かめる (同じ値の候補は導出で 1 つの単位になるので、集合で比べる)。

use std::collections::{BTreeMap, BTreeSet};

use toml::{Table, Value};

use super::*;
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

/// 事例 1 つを走らせる。外れた点の説明の並びを返す (空なら合う)。
fn run_case(case: &Table) -> Vec<String> {
    let case_id = str_of(case, "id").unwrap_or("?");
    let rules = Rules::bundled();
    let env = Env {
        dictionary: Dictionary::bundled(),
        rules: &rules,
        key: &KEY,
    };
    let directives = DirectiveSet::new([], "[伏せ字]", |s| rules.resolve_entity(s));
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    let mut segments: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut counts: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut passed: BTreeSet<(String, String)> = BTreeSet::new();
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
        } else if let Some(l) = step.get("ledger").and_then(Value::as_table) {
            let seg = &segments[str_of(l, "seg").expect("seg")];
            let users: Vec<&Turn> = seg.iter().map(|n| &turns[n]).filter(|t| t.user).collect();
            let table = sentences(&users);
            let ev = Evidence::new(
                &table,
                users
                    .iter()
                    .map(|t| Utterance::new(t.id, 0, &t.text, Vec::new())),
            );
            for u in l
                .get("units")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let u = ledger_unit(u);
                match check(&env, &ev, &directives, &u) {
                    Ok(p) => {
                        passed.insert((p.aspect.as_str().to_owned(), p.value.text));
                    }
                    Err(r) => *counts.entry(r.as_str()).or_default() += 1,
                }
            }
        } else if let Some(e) = step.get("expect").and_then(Value::as_table) {
            if let Some(want) = e.get("counts").and_then(Value::as_table) {
                for (name, n) in want {
                    if !Reason::ALL.iter().any(|r| r.as_str() == name) {
                        continue;
                    }
                    let got = counts.get(name.as_str()).copied().unwrap_or(0);
                    if Some(got) != n.as_integer() {
                        problems.push(format!("{case_id}: counts.{name} = {got}, want {n}"));
                    }
                }
            }
            if let Some(units) = e.get("units").and_then(Value::as_array) {
                let want: BTreeSet<(String, String)> = units
                    .iter()
                    .map(|u| {
                        let t = u.as_table().expect("a unit is a table");
                        (
                            str_of(t, "aspect").expect("aspect").to_owned(),
                            str_of(t, "value").expect("value").to_owned(),
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

/// `attacks.toml` の全事例の、関門で決まる部分 (09 §3.5.2 の「同梱の攻撃の事例」)。
#[test]
fn attack_cases_are_stopped_by_the_gates() {
    let file: Table = toml::from_str(include_str!(
        "../../../../tests/memory-cases/v0.5/attacks.toml"
    ))
    .expect("attacks.toml parses");
    let cases = file["case"].as_array().expect("cases");
    assert!(cases.len() >= 11);
    let problems: Vec<String> = cases
        .iter()
        .flat_map(|c| run_case(c.as_table().expect("a case is a table")))
        .collect();
    assert!(problems.is_empty(), "{problems:#?}");
}
