//! メモリの回帰ケース (`tests/memory-cases/`、09 §3.5) の形の検査。
//!
//! 事例の中身 (期待する結果) を確かめる試験は、抽出・統合・忘れるを作る M3.1〜M3.4 で足す。ここでは、
//! 事例が書式どおりで、属性の一覧と食い違わず、書き間違い (根拠が発話に無い、値が上限を超える など) が
//! 無いことだけを確かめる。

use std::collections::BTreeSet;
use std::path::PathBuf;

use evorag_memory::attributes;
use toml::Value;

fn cases_dir() -> PathBuf {
    // 実行時の値を先に見る (no-network の CI は、nextest の --workspace-remap で写しの中で走らせる)。
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dir.join("../../tests/memory-cases")
}

fn load(file: &str) -> Vec<Value> {
    let path = cases_dir().join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{file}: {e}"));
    let doc: Value = toml::from_str(&text).unwrap_or_else(|e| panic!("{file} parses: {e}"));
    doc.get("case")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{file}: no [[case]]"))
        .clone()
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn req<'a>(v: &'a Value, key: &str, ctx: &str) -> &'a str {
    s(v, key).unwrap_or_else(|| panic!("{ctx}: `{key}` is missing"))
}

/// 事例の共通の検査 (ID の一意と接頭辞、題、言語、モード)。ID を返す。
fn common(case: &Value, prefix: char, seen: &mut BTreeSet<String>, file: &str) -> String {
    let id = req(case, "id", file).to_owned();
    assert!(
        id.starts_with(prefix),
        "{file}: id `{id}` must start with {prefix}"
    );
    assert!(seen.insert(id.clone()), "{file}: duplicate id `{id}`");
    req(case, "title", &id);
    req(case, "spec", &id);
    if let Some(lang) = s(case, "lang") {
        assert!(["ja", "en"].contains(&lang), "{id}: lang `{lang}`");
    }
    id
}

fn chars(v: &str) -> usize {
    v.chars().count()
}

/// `old` を根拠の中に求めるか (筋書きの候補は、根拠に無い `old` も書ける。関門 2 で空として扱う
/// `old_ignored` を写すため、04 §5.2)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Old {
    InEvidence,
    Anywhere,
}

/// 候補の検査。`text` があれば、根拠と値の書き間違いも確かめる。
fn check_candidate(
    id: &str,
    c: &Value,
    text: Option<&str>,
    raw_evidence: bool,
    raw_value: bool,
    old_at: Old,
) {
    req(c, "a", id);
    let subject = req(c, "s", id);
    let v = req(c, "v", id);
    let e = req(c, "e", id);
    if let Some(m) = s(c, "m") {
        assert!(
            ["fact", "negated", "question", "hypothetical"].contains(&m),
            "{id}: m `{m}`"
        );
    }
    if let Some(tn) = s(c, "tn") {
        assert!(
            ["current", "past", "planned"].contains(&tn),
            "{id}: tn `{tn}`"
        );
    }
    assert!(!subject.is_empty(), "{id}: empty subject");
    // 正規化 v1 か照合の形 (04 §5.6) で含まれればよい (改行をまたぐ値の事例は、LLM が改行を空白にして写す)。
    let within = |hay: &str, needle: &str| {
        use evorag_memory::{normalize_loose, normalize_v1};
        normalize_v1(hay).contains(&normalize_v1(needle))
            || normalize_loose(hay).contains(&normalize_loose(needle))
    };
    if raw_evidence && let Some(text) = text {
        assert!(within(text, e), "{id}: evidence `{e}` is not in the text");
    }
    if raw_value {
        assert!(within(e, v), "{id}: value `{v}` is not in the evidence");
        if let Some(old) = s(c, "old")
            && old_at == Old::InEvidence
        {
            assert!(within(e, old), "{id}: old `{old}` is not in the evidence");
        }
    }
}

#[test]
fn prefilter_cases_are_well_formed() {
    let mut seen = BTreeSet::new();
    for case in load("prefilter.toml") {
        let id = common(&case, 'P', &mut seen, "prefilter.toml");
        req(&case, "text", &id);
        match req(&case, "expect", &id) {
            "send" => assert!(s(&case, "reason").is_none(), "{id}: send has a reason"),
            "skip" => {
                let r = req(&case, "reason", &id);
                assert!(
                    ["mode", "empty", "greeting", "short"].contains(&r),
                    "{id}: reason `{r}`"
                );
            }
            other => panic!("{id}: expect `{other}`"),
        }
    }
    assert!(seen.len() >= 10);
}

const GATE_REASONS: &[&str] = &[
    "accept",
    "unknown_attribute",
    "evidence_missing",
    "value_not_in_evidence",
    "mood",
    "no_cue",
    "quoted",
    "subject_not_allowed",
    "forbidden_category",
    "forbidden_char",
    "url",
    "role_marker",
    "mixed_script",
    "imperative",
    "instruction_ref",
    "redaction_marker",
    "too_long",
    "forgotten",
];

#[test]
fn gate_cases_are_well_formed_and_agree_with_the_attribute_list() {
    let attrs = attributes();
    let mut seen = BTreeSet::new();
    let mut reasons = BTreeSet::new();
    for case in load("gates.toml") {
        let id = common(&case, 'G', &mut seen, "gates.toml");
        let text = req(&case, "text", &id);
        let expect = req(&case, "expect", &id);
        assert!(GATE_REASONS.contains(&expect), "{id}: expect `{expect}`");
        reasons.insert(expect.to_owned());
        if let Some(count) = s(&case, "count") {
            assert!(
                ["evidence_normalized", "evidence_loose", "old_ignored"].contains(&count),
                "{id}: count `{count}`"
            );
            assert_eq!(
                expect, "accept",
                "{id}: count is only for accepted candidates"
            );
        }
        if let Some(subject) = s(&case, "stored_subject") {
            assert_eq!(
                expect, "accept",
                "{id}: stored_subject is only for accepted candidates"
            );
            assert_eq!(
                subject,
                evorag_memory::normalize_v1(subject),
                "{id}: stored_subject is in the normalized form"
            );
        }
        if let Some(stored) = s(&case, "stored") {
            assert_eq!(
                expect, "accept",
                "{id}: stored is only for accepted candidates"
            );
            assert!(
                text.contains(stored),
                "{id}: stored `{stored}` is not in the text"
            );
        }
        let c = case
            .get("candidate")
            .unwrap_or_else(|| panic!("{id}: no candidate"));
        let raw_evidence = expect != "evidence_missing" && s(&case, "count").is_none();
        let raw_value = !matches!(expect, "evidence_missing" | "value_not_in_evidence")
            && s(&case, "count").is_none();
        check_candidate(&id, c, Some(text), raw_evidence, raw_value, Old::InEvidence);
        let a = req(c, "a", &id);
        let v = req(c, "v", &id);
        let attr = attrs.get(a);
        if expect == "unknown_attribute" {
            assert!(attr.is_none(), "{id}: `{a}` is in the attribute list");
            continue;
        }
        let attr = attr.unwrap_or_else(|| panic!("{id}: unknown attribute `{a}`"));
        let subject = req(c, "s", &id);
        let other = subject != "self";
        match expect {
            // 属性が許さない・長すぎる・根拠に無い・事実の表で本人を表す語 (04 §5.2 の関門 5)。
            "subject_not_allowed" => {
                let folded = subject.to_lowercase();
                assert!(
                    other
                        && (!attr.other_subjects
                            || chars(subject) > evorag_memory::MAX_SUBJECT_CHARS
                            || !req(c, "e", &id).to_lowercase().contains(&folded)
                            || evorag_memory::SELF_LABELS.iter().any(|w| {
                                evorag_memory::normalize_loose(w)
                                    == evorag_memory::normalize_loose(subject)
                            })),
                    "{id}"
                );
            }
            "too_long" => assert!(chars(v) > usize::from(attr.max_chars), "{id}: not too long"),
            "accept" => {
                assert!(!other || attr.other_subjects, "{id}: subject not allowed");
                assert!(chars(v) <= usize::from(attr.max_chars), "{id}: too long");
            }
            "forgotten" => assert!(case.get("forgotten").is_some(), "{id}: no forgotten"),
            _ => {}
        }
        if let Some(f) = case.get("forgotten").and_then(Value::as_array) {
            for m in f {
                assert!(
                    attrs.get(req(m, "a", &id)).is_some(),
                    "{id}: forgotten attribute"
                );
            }
        }
    }
    // 関門のすべての理由に、少なくとも 1 件の事例がある。
    for r in GATE_REASONS {
        assert!(reasons.contains(*r), "gates.toml: no case for `{r}`");
    }
}

const STATES: &[&str] = &["Live", "Contested", "Superseded", "Retracted"];
const OPS: &[&str] = &[
    "forget",
    "private",
    "private_off",
    "delete_session",
    "accept",
    "revert",
    "unblock",
    "rebuild_facts",
    "shuffle",
];

/// 筋書きの事例 (consolidation.toml・forget.toml) の検査。
fn check_scenarios(file: &str, prefix: char) -> usize {
    let attrs = attributes();
    let mut seen = BTreeSet::new();
    let cases = load(file);
    for case in &cases {
        let id = common(case, prefix, &mut seen, file);
        let steps = case
            .get("step")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{id}: no steps"));
        let mut turns: Vec<(String, String, String)> = Vec::new(); // (id, session, text)
        let mut last_at = String::new();
        let mut expects = 0;
        for step in steps {
            let keys: Vec<&str> = step
                .as_table()
                .expect("step is a table")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys.len(),
                1,
                "{id}: a step has exactly one of turn/candidate/..."
            );
            let body = &step[keys[0]];
            match keys[0] {
                "turn" => {
                    let tid = req(body, "id", &id).to_owned();
                    assert!(
                        turns.iter().all(|t| t.0 != tid),
                        "{id}: duplicate turn `{tid}`"
                    );
                    let at = req(body, "at", &id).to_owned();
                    assert!(
                        at > last_at,
                        "{id}: turn `{tid}` is not later than the previous"
                    );
                    last_at = at;
                    if let Some(role) = s(body, "role") {
                        assert!(["user", "assistant"].contains(&role), "{id}: role `{role}`");
                    }
                    turns.push((
                        tid,
                        req(body, "session", &id).to_owned(),
                        req(body, "text", &id).to_owned(),
                    ));
                }
                "candidate" => {
                    let t = req(body, "turn", &id);
                    let text = turns
                        .iter()
                        .find(|x| x.0 == t)
                        .unwrap_or_else(|| panic!("{id}: candidate on unknown turn `{t}`"))
                        .2
                        .clone();
                    check_candidate(&id, body, Some(&text), true, true, Old::Anywhere);
                    let a = req(body, "a", &id);
                    let attr = attrs
                        .get(a)
                        .unwrap_or_else(|| panic!("{id}: unknown attribute `{a}`"));
                    assert!(
                        req(body, "s", &id) == "self" || attr.other_subjects,
                        "{id}: subject not allowed for `{a}`"
                    );
                    assert!(
                        chars(req(body, "v", &id)) <= usize::from(attr.max_chars),
                        "{id}"
                    );
                }
                "correction" => {
                    let t = req(body, "turn", &id);
                    let text = &turns
                        .iter()
                        .find(|x| x.0 == t)
                        .unwrap_or_else(|| panic!("{id}: correction on unknown turn `{t}`"))
                        .2;
                    let new = req(body, "new", &id);
                    assert!(text.contains(new), "{id}: new `{new}` is not in the turn");
                    req(body, "old", &id);
                }
                "tamper" => {
                    let t = req(body, "id", &id);
                    assert!(
                        turns.iter().any(|x| x.0 == t),
                        "{id}: tamper on unknown turn"
                    );
                    req(body, "text", &id);
                }
                "op" => {
                    let kind = req(body, "kind", &id);
                    assert!(OPS.contains(&kind), "{id}: op `{kind}`");
                    if kind == "delete_session" {
                        let sess = req(body, "session", &id);
                        assert!(turns.iter().any(|x| x.1 == sess), "{id}: unknown session");
                    } else if !["rebuild_facts", "shuffle"].contains(&kind) {
                        let t = body
                            .get("target")
                            .unwrap_or_else(|| panic!("{id}: no target"));
                        assert!(
                            attrs.get(req(t, "a", &id)).is_some(),
                            "{id}: target attribute"
                        );
                    }
                    if let Some(scope) = s(body, "scope") {
                        assert!(["successors", "value_only"].contains(&scope), "{id}: scope");
                    }
                }
                "expect" => {
                    expects += 1;
                    if let Some(facts) = body.get("facts").and_then(Value::as_array) {
                        for f in facts {
                            let state = req(f, "state", &id);
                            assert!(STATES.contains(&state), "{id}: state `{state}`");
                            assert!(attrs.get(req(f, "a", &id)).is_some(), "{id}: attribute");
                            assert_eq!(
                                f.get("v").is_none(),
                                state == "Retracted",
                                "{id}: a Retracted fact has no value, others have one"
                            );
                            assert_eq!(f.get("against").is_some(), state == "Contested", "{id}");
                            if f.get("by").is_some() {
                                assert_eq!(state, "Superseded", "{id}: by");
                            }
                            let sources = f
                                .get("sources")
                                .and_then(Value::as_array)
                                .unwrap_or_else(|| panic!("{id}: no sources"));
                            assert!(!sources.is_empty(), "{id}: empty sources");
                            for src in sources {
                                let src = src.as_str().expect("source is a string");
                                assert!(
                                    turns.iter().any(|x| x.0 == src),
                                    "{id}: unknown source `{src}`"
                                );
                            }
                        }
                    }
                    for key in ["turns", "recall"] {
                        if let Some(list) = body.get(key).and_then(Value::as_array) {
                            for t in list {
                                let tid = s(t, "id").or_else(|| s(t, "turn")).expect("turn id");
                                assert!(turns.iter().any(|x| x.0 == tid), "{id}: unknown turn");
                            }
                        }
                    }
                }
                other => panic!("{id}: unknown step `{other}`"),
            }
        }
        assert!(expects > 0, "{id}: no expect");
    }
    cases.len()
}

#[test]
fn consolidation_cases_are_well_formed() {
    assert!(check_scenarios("consolidation.toml", 'C') >= 20);
}

#[test]
fn forget_cases_are_well_formed() {
    assert!(check_scenarios("forget.toml", 'F') >= 8);
}

#[test]
fn correction_cases_are_well_formed() {
    let mut seen = BTreeSet::new();
    let reasons = [
        "verified",
        "not_correction",
        "new_not_in_utterance",
        "quoted",
        "old_not_found",
        "direction",
    ];
    for case in load("corrections.toml") {
        let id = common(&case, 'K', &mut seen, "corrections.toml");
        let text = req(&case, "text", &id);
        req(&case, "previous", &id);
        let candidate = case
            .get("expect_candidate")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| panic!("{id}: expect_candidate"));
        if !candidate {
            assert!(case.get("llm").is_none(), "{id}: llm without a candidate");
            continue;
        }
        let expect = req(&case, "expect", &id);
        assert!(reasons.contains(&expect), "{id}: expect `{expect}`");
        let dir = case
            .get("direction")
            .unwrap_or_else(|| panic!("{id}: direction"));
        let new = req(dir, "new", &id);
        assert!(text.contains(new), "{id}: direction.new is not in the text");
        if let Some(old) = s(dir, "old") {
            assert!(text.contains(old), "{id}: direction.old is not in the text");
        }
        let llm = case.get("llm").unwrap_or_else(|| panic!("{id}: llm"));
        let target = req(llm, "target", &id);
        assert!(
            ["fact", "response"].contains(&target),
            "{id}: target `{target}`"
        );
        req(llm, "old", &id);
        req(llm, "new", &id);
        req(llm, "e", &id);
        if let Some(r) = case.get("replaces") {
            assert!(r.as_bool().is_some(), "{id}: replaces must be a bool");
            assert!(case.get("live").is_some(), "{id}: replaces without live");
        }
    }
    assert!(seen.len() >= 10);
}
