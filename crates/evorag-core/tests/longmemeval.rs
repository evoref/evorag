//! LongMemEval の一部での基準値 (09 §5.2 の「LongMemEval の一部」、08 §2.4 の v0.3 の合格条件 (6))。
//! 合否には使わず、版の間の比較に使う。`cargo xtask longmemeval --data <path>` で流す。
//!
//! - データはリポジトリに置かない。`EVORAG_LONGMEMEVAL_DATA` に元のファイル (oracle) のパスを置く。SHA-256 が
//!   一部の定義 (`tests/longmemeval/subset-v0.3.toml`) と違えば流さない。データは信用しない入力として、
//!   ここの型で読むだけにする。
//! - 問いごとに新しいデータ根を作り、以前のセッションのターン (利用者と応答の両方、データのまま) を、各
//!   セッションの日時で書き込みの入口から書く。コアは問いの日時の時計で開き、sleep-time の手順 (点検・抽出・
//!   訂正の検証・事実の表。埋め込みとリランカーを置けば埋め込みと較正も) が済むまで待ってから、新しいセッション
//!   で問う。
//! - 判定は決定論 (LLM で判定しない。元の評価は GPT-4o の判定なので、値は元の論文の値と比べられない)。
//!   答えと正解を NFKC・小文字にして語に分け (数詞は数字に)、正解の語の並びがそのまま含まれるか、正解の内容語が
//!   すべて含まれれば正しい。`_abs` の問いは、断った印があれば正しい。内容語の再現率の平均も記録する。
//! - 結果: `EVAL-longmemeval.json` (`EVORAG_BENCH_OUT`、無ければ target の試験用のフォルダ)。問いと答えの本文は
//!   リポジトリに戻さないように、target の試験用のフォルダの `EVAL-longmemeval.details.json` にだけ書く。

mod eval;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eval::judge;
use eval::{Checks, Servers, Session, Speaker};
use evorag_core::{Core, PromptLanguage};
use evorag_store::testutil::TempDir;
use evorag_types::{Clock, ManualClock, Timestamp};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SUBSET_FILE: &str = "tests/longmemeval/subset-v0.3.toml";
const SUBSET: &str = include_str!("../../../tests/longmemeval/subset-v0.3.toml");
const ABSTENTION: &str = "abstention";

/// 裏の処理を待つ上限 (1 問あたり)。
const SETTLE: Duration = Duration::from_secs(1800);
/// 答えを待つ上限。
const ANSWER: Duration = Duration::from_secs(600);

/// 一部の定義。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subset {
    source: String,
    source_url: String,
    source_sha256: String,
    license: String,
    seed: String,
    excluded_types: Vec<String>,
    size: usize,
    stratum: Vec<Stratum>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stratum {
    name: String,
    pool: usize,
    quota: usize,
    ids: Vec<String>,
}

impl Subset {
    /// 選んだ問いの ID (層の順)。
    fn ids(&self) -> impl Iterator<Item = (&str, &str)> {
        self.stratum
            .iter()
            .flat_map(|s| s.ids.iter().map(move |id| (s.name.as_str(), id.as_str())))
    }

    /// ID の並び (1 行に 1 つ、改行で終える) の SHA-256。
    fn ids_sha256(&self) -> String {
        let mut h = Sha256::new();
        for (_, id) in self.ids() {
            h.update(id.as_bytes());
            h.update(b"\n");
        }
        hex(&h.finalize())
    }
}

fn subset() -> Subset {
    toml::from_str(SUBSET).expect("tests/longmemeval/subset-v0.3.toml is valid")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").expect("writing to a String");
        s
    })
}

/// データの 1 問 (使う項目だけ。ほかの項目は読まない)。
#[derive(Debug, Deserialize)]
struct Entry {
    question_id: String,
    question_type: String,
    question: String,
    /// 文字列か数。
    answer: serde_json::Value,
    question_date: String,
    haystack_dates: Vec<String>,
    haystack_sessions: Vec<Vec<Message>>,
}

#[derive(Debug, Deserialize)]
struct Message {
    role: String,
    content: String,
}

impl Entry {
    /// 層の名前 (`_abs` の問いは abstention)。
    fn stratum(&self) -> &str {
        if self.question_id.ends_with("_abs") {
            ABSTENTION
        } else {
            &self.question_type
        }
    }

    fn answer_text(&self) -> String {
        match &self.answer {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// データのファイルを読む (SHA-256 を確かめる)。
fn load(subset: &Subset) -> Option<(PathBuf, Vec<Entry>)> {
    let path = PathBuf::from(std::env::var_os("EVORAG_LONGMEMEVAL_DATA")?);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let sha = hex(&Sha256::digest(&bytes));
    assert_eq!(
        sha,
        subset.source_sha256,
        "{} is not the {} that {SUBSET_FILE} was made from ({})",
        path.display(),
        subset.source,
        subset.source_url
    );
    let entries: Vec<Entry> = serde_json::from_slice(&bytes).expect("LongMemEval JSON");
    Some((path, entries))
}

/// 一部の選び方 (subset の説明のとおり) で、データから選び直す。層の名前 → (元の数, 選んだ ID)。
fn select(subset: &Subset, entries: &[Entry]) -> BTreeMap<String, (usize, Vec<String>)> {
    let mut pools: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for e in entries
        .iter()
        .filter(|e| !subset.excluded_types.contains(&e.question_type))
    {
        pools
            .entry(e.stratum().to_owned())
            .or_default()
            .push(&e.question_id);
    }
    let total: usize = pools.values().map(Vec::len).sum();
    // 最大剰余法: 床の数を配り、残りを剰余の大きい順 (同じなら名前の順) に 1 つずつ足す。
    let mut quotas: BTreeMap<&str, usize> = pools
        .iter()
        .map(|(k, v)| (k.as_str(), subset.size * v.len() / total))
        .collect();
    let mut by_rest: Vec<(usize, &str)> = pools
        .iter()
        .map(|(k, v)| (subset.size * v.len() % total, k.as_str()))
        .collect();
    by_rest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let left = subset.size - quotas.values().sum::<usize>();
    for (_, k) in by_rest.into_iter().take(left) {
        *quotas.get_mut(k).expect("a stratum") += 1;
    }
    pools
        .iter()
        .map(|(k, ids)| {
            let mut ranked: Vec<(String, &str)> = ids
                .iter()
                .map(|id| {
                    let key = hex(&Sha256::digest(format!("{}:{id}", subset.seed).as_bytes()));
                    (key, *id)
                })
                .collect();
            ranked.sort();
            let chosen = ranked
                .into_iter()
                .take(quotas[k.as_str()])
                .map(|(_, id)| id.to_owned())
                .collect();
            (k.clone(), (ids.len(), chosen))
        })
        .collect()
}

/// `2023/05/20 (Sat) 02:21` を地方時として読む。
fn parse_date(s: &str) -> Option<Timestamp> {
    let (date, rest) = s.split_once(' ')?;
    let time = rest.rsplit(' ').next()?;
    let mut d = date.split('/').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (hh, mm) = time.split_once(':')?;
    let (hh, mm): (i64, i64) = (hh.parse().ok()?, mm.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 {
        return None;
    }
    let local = (days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60) * 1000;
    let off = evorag_core::localtime::offset(local).unwrap_or(0);
    Some(Timestamp::from_millis(local - i64::from(off) * 1000))
}

/// 年月日から 1970-01-01 からの日数 (Howard Hinnant の days_from_civil)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 書き込みの数 (結果に書く)。
#[derive(Debug, Default)]
struct Ingest {
    sessions: usize,
    user_turns: usize,
    assistant_turns: usize,
    /// セッションの頭の応答 (前に利用者の発話が無い) で、書かなかったもの。
    dropped: usize,
    /// 同じ話し手が続いたので、前のターンにつないだもの。
    merged: usize,
}

/// 1 問の以前の会話を、書き込む形にする (日時の順に並べ、交互にする)。
fn sessions_of(e: &Entry) -> (Vec<Session>, Ingest) {
    let mut n = Ingest::default();
    let mut dated: Vec<(Timestamp, usize)> = e
        .haystack_dates
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let at =
                parse_date(d).unwrap_or_else(|| panic!("{}: unreadable date {d:?}", e.question_id));
            (at, i)
        })
        .collect();
    dated.sort();
    let mut out = Vec::new();
    for (at, i) in dated {
        let mut turns: Vec<(Speaker, String)> = Vec::new();
        for m in &e.haystack_sessions[i] {
            let speaker = match m.role.as_str() {
                "user" => Speaker::User,
                "assistant" => Speaker::Assistant,
                other => panic!("{}: unknown role {other:?}", e.question_id),
            };
            match turns.last_mut() {
                None if speaker == Speaker::Assistant => n.dropped += 1,
                Some((last, text)) if *last == speaker => {
                    text.push_str("\n\n");
                    text.push_str(&m.content);
                    n.merged += 1;
                }
                _ => turns.push((speaker, m.content.clone())),
            }
        }
        if turns.is_empty() {
            continue;
        }
        n.sessions += 1;
        n.user_turns += turns.iter().filter(|t| t.0 == Speaker::User).count();
        n.assistant_turns += turns.iter().filter(|t| t.0 == Speaker::Assistant).count();
        out.push(Session {
            title: format!("{} #{}", e.question_id, n.sessions),
            start: at,
            turns,
        });
    }
    (out, n)
}

/// 判定の語の扱い。
mod score {
    use super::judge;

    /// 語の比べ方で除く語 (英語の機能語。否定の語は残す)。
    const STOP: &[&str] = &[
        "a", "an", "the", "i", "you", "my", "your", "me", "we", "our", "it", "its", "is", "are",
        "was", "were", "be", "been", "am", "do", "did", "does", "have", "has", "had", "of", "in",
        "on", "at", "to", "for", "with", "from", "by", "and", "or", "as", "that", "this", "these",
        "those", "there", "their", "they", "he", "she", "his", "her", "them", "so", "s", "t",
    ];

    /// 数詞を数字に (0〜20 と 10 の倍数)。
    const NUMBERS: &[(&str, &str)] = &[
        ("zero", "0"),
        ("one", "1"),
        ("two", "2"),
        ("three", "3"),
        ("four", "4"),
        ("five", "5"),
        ("six", "6"),
        ("seven", "7"),
        ("eight", "8"),
        ("nine", "9"),
        ("ten", "10"),
        ("eleven", "11"),
        ("twelve", "12"),
        ("thirteen", "13"),
        ("fourteen", "14"),
        ("fifteen", "15"),
        ("sixteen", "16"),
        ("seventeen", "17"),
        ("eighteen", "18"),
        ("nineteen", "19"),
        ("twenty", "20"),
        ("thirty", "30"),
        ("forty", "40"),
        ("fifty", "50"),
        ("sixty", "60"),
        ("seventy", "70"),
        ("eighty", "80"),
        ("ninety", "90"),
        ("hundred", "100"),
    ];

    /// 正規化 (NFKC・小文字) して、英数字以外で区切った語の並び (数詞は数字に)。
    pub fn tokens(s: &str) -> Vec<String> {
        judge::normalize(s)
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| {
                NUMBERS
                    .iter()
                    .find(|(k, _)| *k == w)
                    .map_or_else(|| w.to_owned(), |(_, v)| (*v).to_owned())
            })
            .collect()
    }

    /// 正解の別表記 (「X. Y (including the last day) is also acceptable.」の形なら X と Y)。
    pub fn keys(answer: &str) -> Vec<String> {
        let a = answer.trim();
        if let Some((first, rest)) = a.split_once(". ")
            && rest.contains("also acceptable")
        {
            let second = rest
                .split(" (including")
                .next()
                .unwrap_or(rest)
                .split(" is also acceptable")
                .next()
                .unwrap_or(rest);
            return vec![first.to_owned(), second.to_owned()];
        }
        vec![a.to_owned()]
    }

    /// (語の並びがそのまま含まれるか、内容語の再現率)。
    pub fn compare(key: &str, response: &[String]) -> (bool, f64) {
        let k = tokens(key);
        if k.is_empty() {
            return (false, 0.0);
        }
        let joined = format!(" {} ", response.join(" "));
        let exact = joined.contains(&format!(" {} ", k.join(" ")));
        let content: Vec<&String> = k.iter().filter(|w| !STOP.contains(&w.as_str())).collect();
        let recall = if content.is_empty() {
            f64::from(u8::from(exact))
        } else {
            let hit = content.iter().filter(|w| response.contains(w)).count();
            hit as f64 / content.len() as f64
        };
        (exact, recall)
    }

    /// 判定: (正しいか, 内容語の再現率)。
    pub fn judge(answer: &str, response: &str, abstention: bool) -> (bool, f64) {
        if abstention {
            let ok = judge::abstained(response);
            return (ok, f64::from(u8::from(ok)));
        }
        let r = tokens(response);
        keys(answer)
            .iter()
            .map(|k| {
                let (exact, recall) = compare(k, &r);
                (exact || recall >= 1.0, recall)
            })
            .fold((false, 0.0), |(ok, best), (o, rc)| (ok || o, best.max(rc)))
    }
}

/// LongMemEval の一部を流し、基準値を記録する (合否には使わない)。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real llama-server and the LongMemEval data (cargo xtask longmemeval --data <path>)"]
async fn live_longmemeval_baseline() {
    let subset = subset();
    let Some((path, entries)) = load(&subset) else {
        eprintln!("longmemeval: set EVORAG_LONGMEMEVAL_DATA to the data file (skipped)");
        return;
    };
    let Some(servers) = Servers::from_env() else {
        eprintln!("longmemeval: set EVORAG_LIVE_LLM_URL to run (skipped)");
        return;
    };
    let limit: Option<usize> = std::env::var("EVORAG_LONGMEMEVAL_LIMIT")
        .ok()
        .map(|v| v.parse().expect("EVORAG_LONGMEMEVAL_LIMIT is a number"));
    let by_id: HashMap<&str, &Entry> = entries
        .iter()
        .map(|e| (e.question_id.as_str(), e))
        .collect();
    eval::init_log();
    let chat = servers.chat_client();
    let models = eval::models(&servers).await;
    let mut checks = Checks::default();
    let mut rows = Vec::new();
    let mut details = Vec::new();
    let started = Instant::now();
    for (stratum, id) in subset.ids().take(limit.unwrap_or(usize::MAX)) {
        let e = by_id
            .get(id)
            .unwrap_or_else(|| panic!("{id} is not in {}", path.display()));
        let t0 = Instant::now();
        let (sessions, ingest) = sessions_of(e);
        let dir = TempDir::new("live-lme");
        let first = sessions
            .first()
            .map_or(Timestamp::from_millis(0), |s| s.start);
        let clock = Arc::new(ManualClock::new(first));
        let last = eval::write_sessions(dir.path(), &clock, &sessions);
        let asked = parse_date(&e.question_date)
            .unwrap_or_else(|| panic!("{id}: unreadable question date"));
        // 問いは、以前の会話の後に問う (データで同じ日の後の時刻に会話があっても、順を保つ)。
        let asked = Timestamp::from_millis(asked.as_millis().max(last.as_millis() + 60_000));
        checks.seed(dir.path());
        let core_clock: Arc<dyn Clock> = Arc::new(ManualClock::new(asked));
        let (core, mut rx, _) = Core::start(
            eval::config(dir.path(), PromptLanguage::En, &servers, core_clock),
            chat.clone(),
        )
        .await
        .expect("the core starts against the live llama-server");
        core.settle().await;
        let settled = eval::settle(&core, SETTLE).await;
        checks.capture(&core);
        let facts = eval::facts(&core, &mut rx).await;
        let turn = eval::ask_in_new_session(&core, &mut rx, &e.question, ANSWER).await;
        core.shutdown().await;
        let (response, sources) = turn.map_or_else(|| (String::new(), 0), |t| (t.text, t.sources));
        let answer = e.answer_text();
        let (correct, recall) = score::judge(&answer, &response, stratum == ABSTENTION);
        let seconds = t0.elapsed().as_secs_f64();
        println!(
            "{id} ({stratum}): {} recall {recall:.2} in {seconds:.0}s | {}",
            if correct { "OK" } else { "NG" },
            response.replace('\n', " ")
        );
        rows.push(serde_json::json!({
            "id": id,
            "stratum": stratum,
            "correct": correct,
            "token_recall": recall,
            "sources": sources,
            "facts": facts.len(),
            "settled": settled,
            "sessions": ingest.sessions,
            "user_turns": ingest.user_turns,
            "assistant_turns": ingest.assistant_turns,
            "dropped_turns": ingest.dropped,
            "merged_turns": ingest.merged,
            "seconds": seconds,
        }));
        details.push(serde_json::json!({
            "id": id,
            "question": e.question,
            "answer": answer,
            "response": response,
            "facts": facts,
        }));
    }
    let mut per: BTreeMap<&str, (usize, usize, f64)> = BTreeMap::new();
    for r in &rows {
        let s = r["stratum"].as_str().expect("a stratum");
        let p = per.entry(s).or_default();
        p.0 += 1;
        p.1 += usize::from(r["correct"] == true);
        p.2 += r["token_recall"].as_f64().unwrap_or(0.0);
    }
    let n = rows.len();
    let correct: usize = per.values().map(|p| p.1).sum();
    let recall: f64 = per.values().map(|p| p.2).sum();
    let ratio = |a: f64, b: usize| if b == 0 { 0.0 } else { a / b as f64 };
    let per_type: serde_json::Map<String, serde_json::Value> = per
        .iter()
        .map(|(k, (n, c, r))| {
            (
                (*k).to_owned(),
                serde_json::json!({
                    "questions": n,
                    "correct": c,
                    "accuracy": ratio(*c as f64, *n),
                    "mean_token_recall": ratio(*r, *n),
                }),
            )
        })
        .collect();
    println!(
        "longmemeval: {correct}/{n} correct ({:.3}) in {:.0}s",
        ratio(correct as f64, n),
        started.elapsed().as_secs_f64()
    );
    let v = serde_json::json!({
        "id": "EVAL-longmemeval",
        "evorag": env!("CARGO_PKG_VERSION"),
        "date": eval::today(),
        "subset_file": SUBSET_FILE,
        "subset_ids_sha256": subset.ids_sha256(),
        "source": subset.source,
        "source_sha256": subset.source_sha256,
        "license": subset.license,
        "partial": limit.is_some_and(|l| l < subset.size),
        "setup": servers.setup(),
        "models": models,
        "ingest": "haystack turns (user and assistant, as in the data) written through the store's write path at the session dates; facts, notes and embeddings by evorag's own sleep-time steps",
        "judge": "deterministic: NFKC + lowercase word tokens (number words to digits); correct if the gold answer's word sequence appears in the response or all its content words do; _abs: an abstention marker. Not comparable with the official GPT-4o judge",
        "questions": n,
        "correct": correct,
        "accuracy": ratio(correct as f64, n),
        "mean_token_recall": ratio(recall, n),
        "per_type": per_type,
        "seconds": started.elapsed().as_secs_f64(),
        "results": rows,
    });
    eval::write_json(&eval::out_dir().join("EVAL-longmemeval.json"), &v);
    eval::write_json(
        &PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("EVAL-longmemeval.details.json"),
        &serde_json::json!({ "id": "EVAL-longmemeval", "details": details }),
    );
}

/// 一部の定義が、データから選び直したものと一致する (データを置いたときだけ。サーバは要らない)。
#[test]
#[ignore = "needs the LongMemEval data (EVORAG_LONGMEMEVAL_DATA)"]
fn longmemeval_subset_matches_the_data() {
    let subset = subset();
    let Some((_, entries)) = load(&subset) else {
        eprintln!("longmemeval: set EVORAG_LONGMEMEVAL_DATA to the data file (skipped)");
        return;
    };
    let chosen = select(&subset, &entries);
    assert_eq!(chosen.len(), subset.stratum.len(), "strata: {chosen:?}");
    for s in &subset.stratum {
        let (pool, ids) = chosen
            .get(&s.name)
            .unwrap_or_else(|| panic!("no stratum {}", s.name));
        assert_eq!(*pool, s.pool, "{}: pool", s.name);
        assert_eq!(ids, &s.ids, "{}: chosen ids", s.name);
    }
    for (_, id) in subset.ids() {
        let e = entries
            .iter()
            .find(|e| e.question_id == id)
            .expect("the id is in the data");
        let (sessions, _) = sessions_of(e);
        assert!(!sessions.is_empty(), "{id}: no session");
        assert!(
            parse_date(&e.question_date).is_some(),
            "{id}: question date"
        );
    }
}

/// 一部の定義の形 (サーバもデータも要らない)。
#[test]
fn the_longmemeval_subset_is_well_formed() {
    let s = subset();
    assert_eq!(s.source_sha256.len(), 64);
    assert!(s.source_sha256.chars().all(|c| c.is_ascii_hexdigit()));
    let mut seen = std::collections::BTreeSet::new();
    for st in &s.stratum {
        assert_eq!(st.ids.len(), st.quota, "{}: quota", st.name);
        assert!(st.quota <= st.pool, "{}: quota over pool", st.name);
        for id in &st.ids {
            assert!(seen.insert(id.as_str()), "{id}: listed twice");
            assert_eq!(
                id.ends_with("_abs"),
                st.name == ABSTENTION,
                "{id} in {}",
                st.name
            );
        }
        assert!(
            !s.excluded_types.contains(&st.name),
            "{}: excluded type has a stratum",
            st.name
        );
    }
    assert_eq!(seen.len(), s.size, "subset size");
    assert_eq!(s.ids_sha256().len(), 64);
}

#[test]
fn the_longmemeval_judge_is_deterministic() {
    use score::judge as j;
    assert!(j("Golden Retriever", "Your dog is a golden retriever.", false).0);
    assert!(j("Five", "You have finished 5 issues so far.", false).0);
    assert!(j("$65", "In total you spent 65 dollars.", false).0);
    assert!(
        j(
            "17 days ago. 18 days (including the last day) is also acceptable.",
            "That was 18 days ago.",
            false
        )
        .0
    );
    let (ok, recall) = j(
        "GPS system not functioning correctly",
        "The GPS system was not working.",
        false,
    );
    assert!(!ok);
    assert!((recall - 0.6).abs() < 1e-9, "recall {recall}");
    assert!(!j("Doc Martin", "I am not sure.", false).0);
    assert!(j("x", "You did not mention a 30-gallon tank.", true).0);
    assert!(!j("x", "There are 12 fish.", true).0);
    assert_eq!(
        score::keys("6 days. 7 days (including the last day) is also acceptable."),
        vec!["6 days".to_owned(), "7 days".to_owned()]
    );
}

#[test]
fn longmemeval_dates_are_read_in_order() {
    let a = parse_date("2023/05/20 (Sat) 02:21").expect("a date");
    let b = parse_date("2023/05/20 (Sat) 14:05").expect("a date");
    let c = parse_date("2024/01/02 (Tue) 00:00").expect("a date");
    assert!(a < b && b < c);
    assert_eq!(b.as_millis() - a.as_millis(), (11 * 60 + 44) * 60 * 1000);
    assert!(parse_date("2023-05-20 02:21").is_none());
    assert!(parse_date("2023/13/20 (Sat) 02:21").is_none());
}

#[test]
fn longmemeval_sessions_alternate_from_the_user() {
    let e: Entry = serde_json::from_value(serde_json::json!({
        "question_id": "q1",
        "question_type": "multi-session",
        "question": "?",
        "answer": 3,
        "question_date": "2023/05/21 (Sun) 10:00",
        "haystack_dates": ["2023/05/20 (Sat) 12:00", "2023/05/19 (Fri) 12:00"],
        "haystack_session_ids": ["b", "a"],
        "haystack_sessions": [
            [{"role": "user", "content": "later"}],
            [
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "one"},
                {"role": "user", "content": "two", "has_answer": true},
                {"role": "assistant", "content": "ok"}
            ]
        ],
        "answer_session_ids": ["a"]
    }))
    .expect("an entry");
    assert_eq!(e.answer_text(), "3");
    let (s, n) = sessions_of(&e);
    assert_eq!(s.len(), 2);
    assert_eq!(
        s[0].turns,
        vec![
            (Speaker::User, "one\n\ntwo".to_owned()),
            (Speaker::Assistant, "ok".to_owned())
        ]
    );
    assert_eq!(s[1].turns, vec![(Speaker::User, "later".to_owned())]);
    assert!(s[0].start < s[1].start);
    assert_eq!((n.dropped, n.merged, n.user_turns), (1, 1, 2));
}
