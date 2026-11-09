//! 想起の事例 (09 §5.2): AC-FR-402-1 (正しい以前の発言が 80% 以上で添えられる) と、
//! 関係の無い問いの Abstain の率の記録。事例は `tests/retrieval/recall-v0.1.toml`。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use evorag_index::lex::LexIndex;
use evorag_retrieval::recall::{Decision, RecallConfig, recall};
use evorag_retrieval::{doc_input, normalize::RULES_VERSION};
use evorag_types::Id;
use serde::Deserialize;

#[derive(Deserialize)]
struct Cases {
    session: Vec<Session>,
    case: Vec<Case>,
}

#[derive(Deserialize)]
struct Session {
    id: String,
    turns: Vec<String>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    category: String,
    session: String,
    gold: Vec<String>,
    query: String,
}

fn workspace() -> PathBuf {
    // 実行の時の値を使う (別の場所に写して流す試験のため)。
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).join("..").join("..")
}

fn id_of(session: usize, turn: usize) -> Id {
    let mut b = [0u8; 16];
    b[0] = session as u8;
    b[1] = turn as u8;
    Id::from_bytes(b)
}

/// verifies: AC-FR-402-1
#[test]
fn recall_cases_reach_80_percent() {
    let text = std::fs::read_to_string(workspace().join("tests/retrieval/recall-v0.1.toml"))
        .expect("the recall cases");
    let cases: Cases = toml::from_str(&text).expect("valid cases");

    let dir = std::env::temp_dir().join(format!("evorag-recall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (index, _) = LexIndex::open(&dir, RULES_VERSION).unwrap();
    let mut names: HashMap<Id, String> = HashMap::new();
    let mut session_ids: HashMap<String, Id> = HashMap::new();
    let mut docs = Vec::new();
    for (si, s) in cases.session.iter().enumerate() {
        let sid = id_of(si + 1, 0);
        session_ids.insert(s.id.clone(), sid);
        for (ti, t) in s.turns.iter().enumerate() {
            let id = id_of(si + 1, ti + 1);
            names.insert(id, format!("{}-t{:02}", s.id, ti + 1));
            docs.push(doc_input(id, sid, &t[3..]));
        }
    }
    index.add(docs).unwrap();
    let snap = index.snapshot();

    let cfg = RecallConfig::default();
    let (mut related, mut fired_right, mut none, mut abstained) = (0, 0, 0, 0);
    let mut by_category: HashMap<&str, usize> = HashMap::new();
    let mut misses = Vec::new();
    for c in &cases.case {
        *by_category.entry(&c.category).or_default() += 1;
        let skip_session = session_ids.get(&c.session).copied();
        let r = recall(
            &snap,
            &c.query,
            None,
            &|d| Some(d.session) == skip_session,
            &cfg,
        );
        let chosen: HashSet<&str> = r.hits.iter().map(|h| names[&h.id].as_str()).collect();
        if c.gold.is_empty() {
            none += 1;
            if r.decision == Decision::Abstain {
                abstained += 1;
            }
        } else {
            related += 1;
            if c.gold.iter().any(|g| chosen.contains(g.as_str())) {
                fired_right += 1;
            } else {
                misses.push(format!("{} {}", c.id, c.query));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    // 事例の組み方 (09 §5.2): 30 件以上、各区分が 3 割以上、関係の無い問いが 10 件以上。
    assert!(related >= 30, "{related} related cases");
    for cat in ["easy", "variant", "para"] {
        let n = by_category.get(cat).copied().unwrap_or(0);
        assert!(n * 10 >= related * 3, "{cat}: {n} of {related}");
    }
    assert!(none >= 10, "{none} unrelated cases");
    println!(
        "recall: {fired_right}/{related} attached the right turn; abstained on {abstained}/{none} unrelated questions"
    );
    println!("missed: {misses:#?}");
    assert!(
        fired_right * 10 >= related * 8,
        "AC-FR-402-1: {fired_right}/{related} (missed {misses:#?})"
    );
}
