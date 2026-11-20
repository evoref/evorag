//! 事実の想起の評価 (09 §5.2 の「事実の想起」、08 §2.4 の v0.3 の合格条件 (3))。`cargo xtask live-llm` で、
//! `EVORAG_LIVE_LLM_URL` を置いたときだけ流す。
//!
//! - 事例は `tests/facts/recall-v0.3.toml` (埋め込んで読む)。1 件ずつ新しいデータ根を作り、以前のセッションの
//!   会話を書き込みの入口で書く (応答は事例に書いた固定の文)。
//! - コアを実際の llama-server で開き、sleep-time の手順 (点検・抽出・訂正の検証・事実の表。埋め込みと
//!   リランカーを置けば埋め込みと較正も) が済むまで待ってから、新しいセッションで問う。
//! - 判定は決定論 (LLM で判定しない): 答えを NFKC・小文字にして、期待する値をすべて含めば合格。答えの無い
//!   問い (abstain) は、断った印 (`eval::judge::ABSTAIN_MARKERS`) があれば合格。
//! - 合格の条件は、事実を答える事例 (abstain 以外) の 9 割以上 (20 件なら 18 件)。abstain は記録だけする。
//! - 事実の正しさ (合否とは別の指標): `facts_expect`・`facts_forbid` を書いた事例は、sleep-time の後の事実の一覧
//!   を確かめ、正しい事例の数 (`facts_correct`) を記録する (09 §5.2)。
//! - `EVORAG_LIVE_FACTS_ONLY` (ID をカンマで区切る) を置くと、その事例だけを流す (合否は判定しない)。
//! - 結果は `EVAL-facts.json` (`EVORAG_BENCH_OUT` か、無ければ target の試験用のフォルダ)。

mod eval;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eval::judge;
use eval::{Checks, Servers, Session, Speaker};
use evorag_core::{Core, PromptLanguage};
use evorag_store::testutil::TempDir;
use evorag_types::{Clock, ManualClock, SystemClock, Timestamp};
use serde::Deserialize;

const CASE_FILE: &str = "tests/facts/recall-v0.3.toml";
const CASES: &str = include_str!("../../../tests/facts/recall-v0.3.toml");
/// 日本語の想起の評価の事例 (09 §5.2、v0.4。記録だけで合否には使わない)。
const JA_FILE: &str = "tests/memory-eval/ja-v0.4.toml";
const JA_CASES: &str = include_str!("../../../tests/memory-eval/ja-v0.4.toml");

/// 裏の処理を待つ上限 (1 件あたり。初めの事例は点検と較正を含む)。
const SETTLE: Duration = Duration::from_secs(900);
/// 答えを待つ上限。
const ANSWER: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    case: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    #[serde(default)]
    lang: Lang,
    kind: Kind,
    attributes: Vec<String>,
    sessions: Vec<Vec<String>>,
    question: String,
    expect: Vec<Expect>,
    /// 事実の一覧に含まれるべき行 (`"<主語>/<属性>=<値> (<状態>, <時制>)"`。別表記の並びならどれか 1 つ)。
    #[serde(default)]
    facts_expect: Vec<Expect>,
    /// `Live` で残ってはならない組 (`"<主語>/<属性>=<値>"`。時制は問わない)。
    #[serde(default)]
    facts_forbid: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Lang {
    #[default]
    Ja,
    En,
}

impl Lang {
    fn prompt(self) -> PromptLanguage {
        match self {
            Self::Ja => PromptLanguage::Ja,
            Self::En => PromptLanguage::En,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Fact,
    Correction,
    Update,
    Multi,
    Abstain,
    /// 答えが以前の応答の後ろのほうにある (05 §3.8、日本語の評価の事例)。
    Window,
    /// 答えが複数のセッションの往復にまたがる (05 §3.4、日本語の評価の事例)。
    Aggregate,
}

/// 期待する値 1 つ (別表記の並びなら、どれか 1 つでよい)。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Expect {
    One(String),
    Any(Vec<String>),
}

impl Expect {
    fn alternatives(&self) -> &[String] {
        match self {
            Self::One(v) => std::slice::from_ref(v),
            Self::Any(v) => v,
        }
    }
}

fn cases() -> Vec<Case> {
    toml::from_str::<CaseFile>(CASES)
        .expect("tests/facts/recall-v0.3.toml is valid")
        .case
}

fn ja_cases() -> Vec<Case> {
    toml::from_str::<CaseFile>(JA_CASES)
        .expect("tests/memory-eval/ja-v0.4.toml is valid")
        .case
}

/// 事実の一覧の行 (`eval::facts` の形 `"<主語>/<属性>=<値> (<状態>, <時制>)"`) を、組
/// (`<主語>/<属性>=<値>`) と状態と時制に分ける。状態の無い行 (`(vacant)` など) は `None`。
fn fact_line(line: &str) -> Option<(&str, &str, &str)> {
    let (head, tail) = line.rsplit_once(" (")?;
    let (state, tense) = tail.strip_suffix(')')?.split_once(", ")?;
    Some((head, state, tense))
}

/// 事実の判定 (09 §5.2 の「事実の正しさ」)。
#[derive(Debug, PartialEq, Eq)]
struct FactsVerdict {
    /// 一覧に無かった `facts_expect` (別表記は ` | ` でつなぐ)。
    missing: Vec<String>,
    /// `Live` だった `facts_forbid`。
    live_forbidden: Vec<String>,
}

impl FactsVerdict {
    fn correct(&self) -> bool {
        self.missing.is_empty() && self.live_forbidden.is_empty()
    }
}

/// 事実の一覧 `facts` を、事例の `facts_expect`・`facts_forbid` で確かめる。どちらも無い事例は `None`。比べる前に、
/// 答えの判定と同じ正規化を当てる。
fn judge_facts(case: &Case, facts: &[String]) -> Option<FactsVerdict> {
    if case.facts_expect.is_empty() && case.facts_forbid.is_empty() {
        return None;
    }
    let n = judge::normalize;
    let rows: Vec<(String, String, String)> = facts
        .iter()
        .filter_map(|l| fact_line(l))
        .map(|(head, state, tense)| (n(head), n(state), n(tense)))
        .collect();
    let has = |want: &str| {
        fact_line(want).is_some_and(|(head, state, tense)| {
            let want = (n(head), n(state), n(tense));
            rows.contains(&want)
        })
    };
    let missing = case
        .facts_expect
        .iter()
        .filter(|e| !e.alternatives().iter().any(|v| has(v)))
        .map(|e| e.alternatives().join(" | "))
        .collect();
    let live = n("Live");
    let live_forbidden = case
        .facts_forbid
        .iter()
        .filter(|f| {
            rows.iter()
                .any(|(head, state, _)| *head == n(f) && *state == live)
        })
        .cloned()
        .collect();
    Some(FactsVerdict {
        missing,
        live_forbidden,
    })
}

/// 事例のターン (`"U: "`・`"A: "`) を話し手と本文に分ける。
fn turn(t: &str) -> Option<(Speaker, &str)> {
    if let Some(rest) = t.strip_prefix("U: ") {
        Some((Speaker::User, rest))
    } else {
        t.strip_prefix("A: ").map(|rest| (Speaker::Assistant, rest))
    }
}

/// 事例の以前のセッションを、書き込む形にする (どのセッションも `start` から。書くときに順にずらす)。
fn sessions_of(case: &Case, start: Timestamp) -> Vec<Session> {
    case.sessions
        .iter()
        .enumerate()
        .map(|(i, turns)| Session {
            title: format!("{} #{}", case.id, i + 1),
            start,
            turns: turns
                .iter()
                .map(|t| {
                    let (s, text) = turn(t).expect("turns start with U: or A:");
                    (s, text.to_owned())
                })
                .collect(),
        })
        .collect()
}

/// 合格に要る数 (9 割以上。20 件なら 18 件)。
fn needed(n: usize) -> usize {
    (n * 9).div_ceil(10)
}

/// 1 件の判定。
#[derive(Debug, PartialEq, Eq)]
struct Verdict {
    passed: bool,
    /// 答えに無かった期待の値 (abstain では空)。
    missing: Vec<String>,
}

fn judge_case(case: &Case, answer: &str) -> Verdict {
    if case.kind == Kind::Abstain {
        return Verdict {
            passed: judge::abstained(answer),
            missing: Vec::new(),
        };
    }
    let missing: Vec<String> = case
        .expect
        .iter()
        .filter(|e| !e.alternatives().iter().any(|v| judge::contains(answer, v)))
        .map(|e| e.alternatives().join(" | "))
        .collect();
    Verdict {
        passed: missing.is_empty(),
        missing,
    }
}

/// 1 件を流す。
async fn run_case(
    case: &Case,
    servers: &Servers,
    checks: &mut Checks,
    chat: &Arc<dyn evorag_llm::LlmClient>,
    typing: bool,
) -> serde_json::Value {
    let started = Instant::now();
    let dir = TempDir::new("live-facts");
    // 以前の会話は前の日に話したことにする (問いのセッションより前の時刻)。
    let day_ago = SystemClock.now().as_millis() - 24 * 60 * 60 * 1000;
    let clock = Arc::new(ManualClock::new(Timestamp::from_millis(day_ago)));
    let sessions = sessions_of(case, Timestamp::from_millis(day_ago));
    eval::write_sessions(dir.path(), &clock, &sessions);
    checks.seed(dir.path());
    let system: Arc<dyn Clock> = Arc::new(SystemClock);
    let (core, mut rx, _) = Core::start(
        eval::config(dir.path(), case.lang.prompt(), servers, system),
        chat.clone(),
    )
    .await
    .expect("the core starts against the live llama-server");
    core.settle().await;
    let settled = eval::settle(&core, SETTLE).await;
    checks.capture(&core);
    let facts = eval::facts(&core, &mut rx).await;
    let turn = if typing {
        eval::ask_timed(&core, &mut rx, &case.question, ANSWER, true)
            .await
            .map(|t| t.turn)
    } else {
        eval::ask_in_new_session(&core, &mut rx, &case.question, ANSWER).await
    };
    core.shutdown().await;
    let (answer, sources) = turn.map_or_else(|| (String::new(), 0), |t| (t.text, t.sources));
    let verdict = judge_case(case, &answer);
    let facts_verdict = judge_facts(case, &facts);
    println!(
        "{} ({:?}): {} {:?} | {} | facts {:?}{}",
        case.id,
        case.kind,
        if verdict.passed { "PASS" } else { "FAIL" },
        verdict.missing,
        answer.replace('\n', " "),
        facts,
        facts_verdict.as_ref().map_or_else(String::new, |f| format!(
            " | facts {} missing {:?} live_forbidden {:?}",
            if f.correct() { "OK" } else { "WRONG" },
            f.missing,
            f.live_forbidden
        ))
    );
    serde_json::json!({
        "id": case.id,
        "lang": case.lang.name(),
        "kind": format!("{:?}", case.kind).to_lowercase(),
        "attributes": case.attributes,
        "question": case.question,
        "expect": case.expect.iter().map(Expect::alternatives).collect::<Vec<_>>(),
        "answer": answer,
        "passed": verdict.passed,
        "missing": verdict.missing,
        "sources": sources,
        "facts": facts,
        "facts_checked": facts_verdict.is_some(),
        "facts_correct": facts_verdict.as_ref().map(FactsVerdict::correct),
        "facts_missing": facts_verdict.as_ref().map(|f| f.missing.clone()),
        "facts_live_forbidden": facts_verdict.as_ref().map(|f| f.live_forbidden.clone()),
        "settled": settled,
        "seconds": started.elapsed().as_secs_f64(),
    })
}

/// 別のセッションで話した事実を答える (08 §2.4 の v0.3 の合格条件 (3))。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real llama-server (cargo xtask live-llm)"]
async fn live_facts_are_recalled_in_a_new_session() {
    let Some(servers) = Servers::from_env() else {
        eprintln!("live facts: set EVORAG_LIVE_LLM_URL to run (skipped)");
        return;
    };
    // `EVORAG_LIVE_FACTS_ONLY=F01,F21` で一部だけ流す (試し。合否は判定せず、結果に partial と書く)。
    let only: Option<Vec<String>> = std::env::var("EVORAG_LIVE_FACTS_ONLY")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect());
    let cases: Vec<Case> = cases()
        .into_iter()
        .filter(|c| only.as_ref().is_none_or(|o| o.contains(&c.id)))
        .collect();
    eval::init_log();
    let chat = servers.chat_client();
    let models = eval::models(&servers).await;
    let mut checks = Checks::default();
    let mut results = Vec::new();
    for c in &cases {
        results.push(run_case(c, &servers, &mut checks, &chat, false).await);
    }
    let count = |abstain: bool, passed: bool| {
        results
            .iter()
            .filter(|r| (r["kind"] == "abstain") == abstain && (!passed || r["passed"] == true))
            .count()
    };
    let (facts_n, facts_ok) = (count(false, false), count(false, true));
    let (abstain_n, abstain_ok) = (count(true, false), count(true, true));
    let need = needed(facts_n);
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| r["passed"] == false)
        .filter_map(|r| r["id"].as_str())
        .collect();
    // 事実の正しさ (合否には使わない、09 §5.2)。
    let checked = results
        .iter()
        .filter(|r| r["facts_checked"] == true)
        .count();
    let wrong_facts: Vec<&str> = results
        .iter()
        .filter(|r| r["facts_correct"] == false)
        .filter_map(|r| r["id"].as_str())
        .collect();
    let facts_correct = checked - wrong_facts.len();
    println!(
        "live facts: {facts_ok}/{facts_n} facts recalled (need {need}); abstained on {abstain_ok}/{abstain_n}; failed {failed:?}"
    );
    println!(
        "live facts: stored facts correct in {facts_correct}/{checked} cases; wrong {wrong_facts:?}"
    );
    let v = serde_json::json!({
        "id": "EVAL-facts",
        "evorag": env!("CARGO_PKG_VERSION"),
        "date": eval::today(),
        "cases_file": CASE_FILE,
        "setup": servers.setup(),
        "models": models,
        "judge": "answer contains every expected value after NFKC + lowercase (abstain: contains an abstention marker)",
        "fact_cases": facts_n,
        "fact_passed": facts_ok,
        "fact_needed": need,
        "abstain_cases": abstain_n,
        "abstain_passed": abstain_ok,
        "facts_judge": "facts_expect lines are in the fact list and no facts_forbid pair is Live (after the same normalization); not part of the gate",
        "facts_checked_cases": checked,
        "facts_correct": facts_correct,
        "partial": only.is_some(),
        "passed": facts_ok >= need,
        "cases": results,
    });
    eval::write_json(&eval::out_dir().join("EVAL-facts.json"), &v);
    if only.is_some() {
        println!("live facts: partial run (EVORAG_LIVE_FACTS_ONLY); the gate is not judged");
        return;
    }
    assert!(
        facts_ok >= need,
        "v0.3 gate (3): {facts_ok}/{facts_n} facts recalled, need {need} (failed {failed:?})"
    );
}

/// 日本語の想起の評価の事例を流し、記録する (09 §5.2、v0.4。合否には使わない)。入力が止まったことを知らせて
/// から問う (実際の使い方に合わせる)。結果は `EVAL-ja.json`。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real llama-server (cargo xtask live-llm)"]
async fn live_japanese_recall_is_recorded() {
    let Some(servers) = Servers::from_env() else {
        eprintln!("live ja: set EVORAG_LIVE_LLM_URL to run (skipped)");
        return;
    };
    eval::init_log();
    let chat = servers.chat_client();
    let models = eval::models(&servers).await;
    let mut checks = Checks::default();
    let mut results = Vec::new();
    for c in &ja_cases() {
        let mut cfg_checks = std::mem::take(&mut checks);
        results.push(run_case(c, &servers, &mut cfg_checks, &chat, true).await);
        checks = cfg_checks;
    }
    let passed = results.iter().filter(|r| r["passed"] == true).count();
    let mut per: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for r in &results {
        let k = r["kind"].as_str().unwrap_or("?").to_owned();
        let e = per.entry(k).or_default();
        e.0 += 1;
        e.1 += usize::from(r["passed"] == true);
    }
    println!("live ja: {passed}/{} passed {per:?}", results.len());
    let v = serde_json::json!({
        "id": "EVAL-ja",
        "evorag": env!("CARGO_PKG_VERSION"),
        "date": eval::today(),
        "cases_file": JA_FILE,
        "setup": servers.setup(),
        "models": models,
        "typing": true,
        "judge": "answer contains every expected value after NFKC + lowercase (abstain: contains an abstention marker)",
        "cases": results.len(),
        "passed": passed,
        "per_kind": per.iter().map(|(k, (n, p))| (k.clone(), serde_json::json!({"cases": n, "passed": p}))).collect::<serde_json::Map<_, _>>(),
        "results": results.iter().map(|r| serde_json::json!({
            "id": r["id"], "kind": r["kind"], "passed": r["passed"], "sources": r["sources"],
        })).collect::<Vec<_>>(),
    });
    eval::write_json(&eval::out_dir().join("EVAL-ja.json"), &v);
}

/// 日本語の想起の評価の事例のファイルの形 (サーバなしで流す)。
#[test]
fn the_japanese_cases_are_well_formed() {
    let cases = ja_cases();
    let mut ids = std::collections::BTreeSet::new();
    for c in &cases {
        assert!(ids.insert(c.id.as_str()), "{}: duplicate id", c.id);
        assert_eq!(c.lang, Lang::Ja, "{}", c.id);
        assert!(
            !c.sessions.is_empty() && !c.question.trim().is_empty(),
            "{}",
            c.id
        );
        for s in &c.sessions {
            for (i, t) in s.iter().enumerate() {
                let want = if i % 2 == 0 { "U: " } else { "A: " };
                assert!(
                    t.starts_with(want),
                    "{}: turn {i} must start with {want}",
                    c.id
                );
            }
        }
        assert_eq!(c.kind == Kind::Abstain, c.expect.is_empty(), "{}", c.id);
        if c.kind == Kind::Window {
            // 答えは、応答の 400 文字より後ろにある (05 §3.8 の文の窓を確かめる事例)。
            let answer_text: String = c
                .sessions
                .iter()
                .flatten()
                .filter(|t| t.starts_with("A: "))
                .cloned()
                .collect();
            let pos = c.expect[0]
                .alternatives()
                .iter()
                .filter_map(|v| answer_text.find(v.as_str()))
                .min()
                .unwrap_or_else(|| panic!("{}: the expected value is in no answer", c.id));
            assert!(
                answer_text[..pos].chars().count() > 400,
                "{}: the answer is too early",
                c.id
            );
        }
    }
    for k in [Kind::Window, Kind::Aggregate, Kind::Abstain] {
        assert!(cases.iter().any(|c| c.kind == k), "no {k:?} case");
    }
}

/// 事例のファイルの形 (サーバなしで流す): 事実を答える事例が 20 件以上、abstain が 2 件以上、ID が一意、
/// 属性が同梱の一覧にある、ターンが利用者から始まって交互、期待する値が以前の利用者の発話にある。
#[test]
fn the_fact_cases_are_well_formed() {
    let cases = cases();
    let attrs = evorag_memory::attributes();
    let mut ids = BTreeSet::new();
    for c in &cases {
        assert!(ids.insert(c.id.as_str()), "{}: duplicate id", c.id);
        assert!(!c.question.trim().is_empty(), "{}: empty question", c.id);
        assert!(!c.attributes.is_empty(), "{}: no attribute", c.id);
        for a in &c.attributes {
            assert!(attrs.get(a).is_some(), "{}: unknown attribute {a}", c.id);
        }
        assert!(!c.sessions.is_empty(), "{}: no session", c.id);
        let mut said = Vec::new();
        for s in &c.sessions {
            assert!(!s.is_empty(), "{}: empty session", c.id);
            for (i, t) in s.iter().enumerate() {
                let (speaker, text) =
                    turn(t).unwrap_or_else(|| panic!("{}: turn without U:/A: {t}", c.id));
                let want = if i % 2 == 0 {
                    Speaker::User
                } else {
                    Speaker::Assistant
                };
                assert_eq!(speaker, want, "{}: turns must alternate from U: {t}", c.id);
                if speaker == Speaker::User {
                    // 事実を言う発話は、前のふるいで抽出に送られる (04 §5.2)。
                    assert!(
                        matches!(
                            evorag_memory::prefilter("chat", text),
                            evorag_memory::Screened::Send(_)
                        ),
                        "{}: the prefilter skips {text}",
                        c.id
                    );
                    said.push(text);
                }
            }
        }
        // 事実の確かめの書式 (09 §5.2): `facts_expect` は `"<主語>/<属性>=<値> (<状態>, <時制>)"`、
        // `facts_forbid` は `"<主語>/<属性>=<値>"`。属性は同梱の一覧にある。
        let pair_ok = |pair: &str| {
            pair.split_once('/')
                .and_then(|(_, rest)| rest.split_once('='))
                .is_some_and(|(a, v)| attrs.get(a).is_some() && !v.is_empty())
        };
        for e in &c.facts_expect {
            for line in e.alternatives() {
                let (head, state, tense) =
                    fact_line(line).unwrap_or_else(|| panic!("{}: facts_expect `{line}`", c.id));
                assert!(pair_ok(head), "{}: facts_expect `{line}`", c.id);
                assert!(
                    ["Live", "Contested", "Superseded"].contains(&state),
                    "{}: state in `{line}`",
                    c.id
                );
                assert!(
                    ["Current", "Past", "Planned"].contains(&tense),
                    "{}: tense in `{line}`",
                    c.id
                );
            }
        }
        for f in &c.facts_forbid {
            assert!(
                pair_ok(f) && fact_line(f).is_none(),
                "{}: facts_forbid `{f}`",
                c.id
            );
        }
        // 訂正の事例は、字句の規則で訂正の候補になる発話を含む (04 §6)。
        if c.kind == Kind::Correction {
            assert!(
                said.iter()
                    .any(|t| evorag_memory::find_correction("chat", t).is_some()),
                "{}: no utterance matches a correction pattern",
                c.id
            );
        }
        if c.kind == Kind::Abstain {
            assert!(
                c.expect.is_empty(),
                "{}: abstain cases expect nothing",
                c.id
            );
            continue;
        }
        assert!(!c.expect.is_empty(), "{}: nothing expected", c.id);
        for e in &c.expect {
            assert!(
                e.alternatives()
                    .iter()
                    .any(|v| said.iter().any(|t| judge::contains(t, v))),
                "{}: {:?} is never said by the user",
                c.id,
                e.alternatives()
            );
        }
    }
    let facts = cases.iter().filter(|c| c.kind != Kind::Abstain).count();
    let abstain = cases.len() - facts;
    assert!(facts >= 20, "09 §5.2: 20 or more fact cases, got {facts}");
    assert!(abstain >= 2, "2 or more abstain cases, got {abstain}");
    for kind in [Kind::Correction, Kind::Update, Kind::Multi] {
        assert!(
            cases.iter().any(|c| c.kind == kind),
            "no {kind:?} case in the file"
        );
    }
    for lang in [Lang::Ja, Lang::En] {
        assert!(
            cases.iter().any(|c| c.lang == lang),
            "no {lang:?} case in the file"
        );
    }
}

/// 事実の判定: `facts_expect` は状態と時制まで一致する行を、`facts_forbid` は `Live` の行だけを外れとする。
#[test]
fn stored_facts_are_judged_by_expect_and_forbid() {
    let c = cases();
    let f23 = c.iter().find(|c| c.id == "F23").expect("F23");
    let good = [
        "self/residence=名古屋 (Superseded, Current)".to_owned(),
        "self/residence=福岡 (Live, Current)".to_owned(),
    ];
    assert_eq!(judge_facts(f23, &good).map(|v| v.correct()), Some(true));
    let bad = ["self/residence=名古屋 (Live, Current)".to_owned()];
    let v = judge_facts(f23, &bad).expect("checked");
    assert_eq!(v.missing, ["self/residence=福岡 (Live, Current)"]);
    assert_eq!(v.live_forbidden, ["self/residence=名古屋"]);
    let contested = ["self/residence=福岡 (Contested, Current)".to_owned()];
    assert!(!judge_facts(f23, &contested).expect("checked").correct());
    let f19 = c.iter().find(|c| c.id == "F19").expect("F19");
    let trip = ["self/event=来月、金沢へ一人旅をする (Live, Planned)".to_owned()];
    assert_eq!(judge_facts(f19, &trip).map(|v| v.correct()), Some(true));
    let kanazawa = ["self/residence=金沢 (Live, Planned)".to_owned()];
    assert_eq!(
        judge_facts(f19, &kanazawa).map(|v| v.correct()),
        Some(false)
    );
    assert_eq!(fact_line("self/hobby=(vacant)"), None);
}

#[test]
fn the_judge_normalizes_width_and_case() {
    assert_eq!(judge::normalize("ＴｈｉｎｋＰａｄ  X1\n"), "thinkpad x1");
    assert!(judge::contains("神戸にお住まいです。", "神戸"));
    assert!(judge::contains("You live in BOULDER.", "Boulder"));
    assert!(judge::contains("７月２２日ですね", "7月22日"));
    assert!(judge::contains("誕生日は**5 月 13 日**でした", "5月13日"));
    assert_eq!(
        judge::normalize("dental  hygienist です"),
        "dental hygienistです"
    );
    assert!(!judge::contains("大阪にお住まいです。", "神戸"));
    assert!(!judge::contains("anything", ""));
}

#[test]
fn the_judge_finds_abstentions() {
    assert!(judge::abstained("お誕生日はまだ伺っていないようです。"));
    assert!(judge::abstained("申し訳ありませんが、分かりません。"));
    assert!(judge::abstained(
        "事実の表には妹さんの居住地は記載されていますが、あなた自身の居住地は記載されていません。"
    ));
    assert!(judge::abstained(
        "You haven’t told me your favorite food yet."
    ));
    assert!(!judge::abstained("あなたの誕生日は5月13日です。"));
    assert!(!judge::abstained("Your favorite food is ramen."));
}

#[test]
fn a_case_passes_only_with_every_expected_value() {
    let c = cases();
    let multi = c.iter().find(|c| c.id == "F24").expect("F24");
    assert!(judge_case(multi, "将棋と囲碁です").passed);
    let v = judge_case(multi, "将棋です");
    assert!(!v.passed);
    assert_eq!(v.missing, vec!["囲碁".to_owned()]);
    let any = c.iter().find(|c| c.id == "F11").expect("F11");
    assert!(judge_case(any, "土壌の研究でしたね").passed);
    let abstain = c.iter().find(|c| c.id == "A02").expect("A02");
    assert!(judge_case(abstain, "誕生日は聞いていません").passed);
    assert!(!judge_case(abstain, "誕生日は3月3日です").passed);
    assert_eq!(needed(20), 18);
    assert_eq!(needed(30), 27);
}

/// 事例の会話を書き込み、コアがそのまま開ける (サーバなし。応答の llama-server は記録の再生で置き換え、
/// 裏の点検は失敗するに任せる)。
#[tokio::test(flavor = "multi_thread")]
async fn the_case_sessions_open_in_the_core() {
    let case = cases().into_iter().find(|c| c.id == "F22").expect("F22");
    let dir = TempDir::new("facts-write");
    let start = Timestamp::from_millis(1_762_000_000_000);
    let clock = Arc::new(ManualClock::new(start));
    let last = eval::write_sessions(dir.path(), &clock, &sessions_of(&case, start));
    assert!(last > start);
    let replay = evorag_llm::replay::ReplayClient::from_json(
        r#"{
          "props": {"build_info": "b1-test", "model_path": "/m/chat.gguf", "total_slots": 1,
                    "default_generation_settings": {"n_ctx": 8192}},
          "models": {"data": [{"id": "test", "meta": {"n_params": 1, "size": 2, "n_vocab": 3}}]},
          "chats": []
        }"#,
    )
    .expect("a replay cassette");
    let servers = Servers {
        chat: "http://127.0.0.1:9".into(),
        api_key: None,
        aux: None,
    };
    let core_clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::from_millis(
        last.as_millis() + 60_000,
    )));
    let (core, _rx, _) = Core::start(
        eval::config(dir.path(), PromptLanguage::Ja, &servers, core_clock),
        Arc::new(replay),
    )
    .await
    .expect("the core opens the written data root");
    let mut turns: Vec<(String, usize)> = core
        .sessions(10)
        .iter()
        .map(|s| (s.title.clone(), core.turns(s.id).len()))
        .collect();
    turns.sort();
    core.shutdown().await;
    assert_eq!(
        turns,
        vec![("F22 #1".to_owned(), 2), ("F22 #2".to_owned(), 4)]
    );
}
