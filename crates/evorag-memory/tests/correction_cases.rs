//! 訂正の回帰ケース (`tests/memory-cases/corrections.toml`、09 §3.5) を、公開の API (`find_correction`・`verify`・
//! `fold`) で走らせる。
//!
//! 書式は `tests/memory-cases/README.md`。事例ごとに、字句の規則で候補になるか (`expect_candidate`)、型から決まる
//! 向き (`direction`)、固定した検証の LLM の出力 (`llm`) に関門を当てた結果 (`expect`) を比べる。`direction` の
//! 値は、型の穴の本文 (`dir_old`・`dir_new`) に含まれることを確かめる (穴は「でした」などを含みうる)。`live` の
//! ある検証済みの事例は、畳みで今の値が置き換わるか (`replaces`、既定 true) も確かめる。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use evorag_memory::{
    CorrectionCandidate, Directives, Event, Fact, FactSet, FactState, FoldEnv, ForgetKey,
    Prefiltered, Rejection, Safety, Source, Target, Tense, Utterance, VerifiedCorrection,
    VerifyContext, VerifyOutput, attributes, find_correction, fold, normalize_v1, value_key,
    verify,
};
use evorag_types::Id;
use toml::Value;

const KEY: [u8; 32] = [0x3c; 32];

fn load() -> Vec<Value> {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let path = dir.join("../../tests/memory-cases/corrections.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("corrections.toml: {e}"));
    let doc: Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("corrections.toml parses: {e}"));
    doc["case"].as_array().expect("[[case]]").clone()
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// 時刻の順に並ぶ ID (UUID v7 の形)。
fn id_at(ms: u64, seq: u64) -> Id {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[8..].copy_from_slice(&seq.to_be_bytes());
    Id::from_bytes(b)
}

fn strings(case: &Value, key: &str) -> Vec<String> {
    case.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 事例の `live` (今の `Live` の値。時制は `current`)。
fn live_facts(id: &str, case: &Value, key: &ForgetKey) -> Vec<Fact> {
    case.get("live")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, f)| {
                    let field = |k: &str| s(f, k).unwrap_or_else(|| panic!("{id}: live.{k}"));
                    let value = field("v");
                    Fact {
                        id: id_at(1, u64::try_from(i).expect("few")),
                        subject: field("s").to_owned(),
                        attribute: field("a").to_owned(),
                        value: Some(value.to_owned()),
                        vk: Some(value_key(key, value)),
                        tense: Tense::Current,
                        state: FactState::Live,
                        sources: BTreeSet::from([Source {
                            turn: id_at(1, 100),
                            start: 0,
                            end: 0,
                            rev: 0,
                        }]),
                        private: false,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn llm_output(id: &str, case: &Value) -> VerifyOutput {
    let llm = case.get("llm").unwrap_or_else(|| panic!("{id}: llm"));
    let field = |k: &str| {
        s(llm, k)
            .unwrap_or_else(|| panic!("{id}: llm.{k}"))
            .to_owned()
    };
    VerifyOutput {
        target: match field("target").as_str() {
            "fact" => Target::Fact,
            "response" => Target::Response,
            other => panic!("{id}: target `{other}`"),
        },
        old: field("old"),
        new: field("new"),
        evidence: field("e"),
    }
}

/// 1 件の事例を走らせる。検証した結果の名前 (`verified` か理由) を返す (候補にならなければ `None`)。
fn run_case(case: &Value) -> Option<&'static str> {
    let id = s(case, "id").expect("id");
    let text = s(case, "text").expect("text");
    let mode = s(case, "mode").unwrap_or("chat");
    let want_candidate = case
        .get("expect_candidate")
        .and_then(Value::as_bool)
        .expect("expect_candidate");

    let found = find_correction(mode, text);
    assert_eq!(found.is_some(), want_candidate, "{id}: candidate {found:?}");
    let m = found?;

    // 型から決まる向き。
    let dir = case.get("direction").expect("direction");
    let new = s(dir, "new").expect("direction.new");
    assert!(
        normalize_v1(&m.dir_new).contains(&normalize_v1(new)),
        "{id}: dir_new {:?} lacks {new:?} ({})",
        m.dir_new,
        m.pattern
    );
    match s(dir, "old") {
        Some(old) => {
            let got = m
                .dir_old
                .as_deref()
                .unwrap_or_else(|| panic!("{id}: dir_old"));
            assert!(
                normalize_v1(got).contains(&normalize_v1(old)),
                "{id}: dir_old {got:?} lacks {old:?} ({})",
                m.pattern
            );
        }
        None => assert_eq!(m.dir_old, None, "{id}: a pattern without {{old}}"),
    }

    // 検証の関門。
    let key = ForgetKey::from_bytes(KEY);
    let turn = id_at(2, 0);
    let candidate = CorrectionCandidate {
        id: id_at(3, 0),
        turn,
        rev: 0,
        previous: Some(id_at(1, 200)),
        pattern: m.pattern.clone(),
        dir_old: m.dir_old.clone(),
        dir_new: m.dir_new.clone(),
    };
    let previous = s(case, "previous").expect("previous");
    let earlier: Vec<String> = strings(case, "earlier")
        .iter()
        .map(|t| Prefiltered::from_stored(t).text().to_owned())
        .collect();
    let live = live_facts(id, case, &key);
    let live_values: Vec<String> = live.iter().filter_map(|f| f.value.clone()).collect();
    let filtered = Prefiltered::from_stored(text);
    let safety = Safety::bundled();
    let ctx = VerifyContext {
        utterance: Utterance {
            turn,
            rev: 0,
            text,
            filtered: &filtered,
        },
        previous: Some(previous),
        earlier: &earlier,
        live: &live_values,
        key: &key,
        safety: &safety,
    };
    let out = llm_output(id, case);
    let want = s(case, "expect").expect("expect");
    match verify(&candidate, &out, &ctx) {
        Ok(v) => {
            assert_eq!(want, "verified", "{id}: verified, want `{want}`");
            let span = v.source().start as usize..v.source().end as usize;
            assert_eq!(
                normalize_v1(&text[span]),
                normalize_v1(&out.new),
                "{id}: the source is the new value's span"
            );
            assert_eq!(v.source().turn, turn, "{id}");
            assert_eq!(v.target(), out.target, "{id}");
            assert_eq!(v.old_vk(), &value_key(&key, &out.old), "{id}");
            match (v.old_source(), &m.dir_old) {
                (Some(o), Some(_)) => assert_eq!(
                    normalize_v1(&text[o.start as usize..o.end as usize]),
                    normalize_v1(&out.old),
                    "{id}: the old value's span"
                ),
                (None, None) => {}
                other => panic!("{id}: old span {other:?}"),
            }
            if !live.is_empty() {
                let replaces = case
                    .get("replaces")
                    .is_none_or(|r| r.as_bool().expect("replaces"));
                check_live(id, &key, live, v, replaces);
            }
            Some("verified")
        }
        Err(r) => {
            assert_eq!(r.as_str(), want, "{id}: rejected as `{r}`");
            Some(r.as_str())
        }
    }
}

/// 検証済みの訂正を畳みに渡すと、古い値と一致する単値の `Live` を新しい値で置き換え、訂正の発話の古い値の範囲を
/// 置き換えた事実の出典に足す (04 §5.3・§6)。`replaces` が false なら事実を変えない。
fn check_live(id: &str, key: &ForgetKey, live: Vec<Fact>, c: VerifiedCorrection, replaces: bool) {
    let old_vk = *c.old_vk();
    let new_value = c.new_value().to_owned();
    let old_source = c.old_source().copied();
    let before = live.clone();
    let mut set = FactSet { facts: live };
    let mut seq = 0;
    let mut new_id = || {
        seq += 1;
        id_at(4, seq)
    };
    let mut env = FoldEnv {
        key,
        attributes: attributes(),
        new_id: &mut new_id,
    };
    let report = fold(
        &mut set,
        &Directives::default(),
        vec![Event::Correction(c)],
        &mut env,
    );
    if !replaces {
        assert_eq!(set.facts, before, "{id}: the facts are unchanged");
        assert_eq!(report.replaced, 0, "{id}: {report:?}");
        return;
    }
    assert_eq!(report.replaced, 1, "{id}: {report:?}");
    let old = set
        .facts
        .iter()
        .find(|f| f.vk == Some(old_vk))
        .expect("the old value");
    assert!(
        matches!(old.state, FactState::Superseded { .. }),
        "{id}: {:?}",
        old.state
    );
    let old_source = old_source.expect("an anchored correction");
    assert!(
        old.sources.contains(&old_source),
        "{id}: the correction's old span is a source of the superseded fact"
    );
    let now = set
        .facts
        .iter()
        .find(|f| f.state == FactState::Live && f.attribute == old.attribute)
        .expect("a live value");
    assert_eq!(now.value.as_deref(), Some(new_value.as_str()), "{id}");
}

/// 訂正の候補・向き・検証の関門・事実への効き方の事例がすべて期待どおり。
///
/// verifies: AC-FR-306-1 (検証を通らない訂正は検証済みの型にならないので事実を変えない。古い値の穴の無い型の
/// 検証済みの訂正も事実を変えない)
#[test]
fn correction_cases_pass() {
    let cases = load();
    let mut outcomes: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &cases {
        if let Some(o) = run_case(case) {
            *outcomes.entry(o).or_default() += 1;
        }
    }
    assert_eq!(cases.len(), 26);
    for r in Rejection::ALL {
        assert!(outcomes.contains_key(r.as_str()), "no case for `{r}`");
    }
    assert!(
        outcomes.get("verified").copied().unwrap_or(0) >= 5,
        "{outcomes:?}"
    );
}
