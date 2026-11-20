//! LongMemEval での評価 (09 §5.2 の「LongMemEval」)。合否の判定は、版の間の対の比較で行う
//! (`cargo xtask lme-compare`)。`cargo xtask longmemeval --data <path>` で流す。
//!
//! - データはリポジトリに置かない。`EVORAG_LONGMEMEVAL_DATA` に元のファイル (oracle) のパスを置く。SHA-256 が
//!   問いの組の定義と違えば流さない。データは信用しない入力として、ここの型で読むだけにする。
//! - 問いの組 (`EVORAG_LME_SET`): `dev` (既定。開発の組 `tests/longmemeval/subset-v0.3.toml`、調整に使う) か、
//!   `holdout:<切片>` (保留の問い `tests/longmemeval/holdout-v0.4.toml`、判定にだけ使う)。封をした切片
//!   (`opened` が空) は、取り込みだけ (`EVORAG_LME_INGEST_ONLY`) を許す。
//! - 問いごとに新しいデータ根を作り、以前のセッションのターン (利用者と応答の両方、データのまま) を、各
//!   セッションの日時で書き込みの入口から書く。コアは問いの日時の時計で開き、sleep-time の手順 (点検・抽出・
//!   訂正の検証・事実の表。埋め込みとリランカーを置けば埋め込みと較正も) が済むまで待ち (上限は
//!   `EVORAG_LME_SETTLE_SECS`)、いったん閉じる。開き直して、新しいセッションで、入力が止まったことを知らせて
//!   (05 §4.5。`EVORAG_LME_TYPING=0` で省く) から問う。
//! - `EVORAG_LME_ROOTS` を置くと、閉じた時点のデータ根を問いごとに保存し、次からは取り込みを省いて使い回す
//!   (取り込みの手順の版 `WRITE_PATH` とモデルが同じときだけ)。想起と文脈だけを変える比べ方で使う。
//! - 判定は決定論 (LLM で判定しない。元の評価は GPT-4o の判定なので、値は元の論文の値と比べられない)。
//!   答えと正解を NFKC・小文字にして語に分け (数詞は数字に)、正解の語の並びがそのまま含まれるか、正解の内容語が
//!   すべて含まれれば正しい。`_abs` の問いは、断った印があれば正しい。内容語の再現率の平均と、照合で解ける
//!   見込みの問い (正解の内容語が `SOLVABLE_WORDS` 以下) の正解率も記録する。
//! - 結果: `EVAL-longmemeval[-holdout-<切片>].json` (`EVORAG_BENCH_OUT`、無ければ target の試験用のフォルダ)。
//!   問いごとに、TTFT・想起の記録と観測値 (ID と数値だけ) を残す。問いと答えの本文はリポジトリに戻さないように、
//!   target の試験用のフォルダの `*.details.json` にだけ書く。

mod eval;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
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
const HOLDOUT_FILE: &str = "tests/longmemeval/holdout-v0.4.toml";
const HOLDOUT: &str = include_str!("../../../tests/longmemeval/holdout-v0.4.toml");
const ABSTENTION: &str = "abstention";
/// 照合で解ける見込みの問いの、正解の内容語の数の上限 (開発の組では、これを超える 7 問は言い回しで外れる)。
const SOLVABLE_WORDS: u64 = 5;

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

/// 保留の問いの定義 (`holdout-v0.4.toml`)。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Holdout {
    source: String,
    source_url: String,
    source_sha256: String,
    license: String,
    seed: String,
    excluded_types: Vec<String>,
    excluded_subset: String,
    slice: Vec<Slice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slice {
    name: String,
    size: usize,
    /// 判定のために開けた版。空なら封をしている。
    opened: String,
    stratum: Vec<SliceStratum>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceStratum {
    name: String,
    quota: usize,
    ids: Vec<String>,
}

fn holdout() -> Holdout {
    toml::from_str(HOLDOUT).expect("tests/longmemeval/holdout-v0.4.toml is valid")
}

/// 最大剰余法で `size` を層の数 `counts` の比で割り当てる (剰余が同じなら名前の順)。
fn quotas(size: usize, counts: &BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    let total: usize = counts.values().sum();
    let mut q: BTreeMap<String, usize> = counts
        .iter()
        .map(|(k, v)| (k.clone(), size * v / total))
        .collect();
    let mut by_rest: Vec<(usize, &str)> = counts
        .iter()
        .map(|(k, v)| (size * v % total, k.as_str()))
        .collect();
    by_rest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let left = size - q.values().sum::<usize>();
    for (_, k) in by_rest.into_iter().take(left) {
        *q.get_mut(k).expect("a stratum") += 1;
    }
    q
}

/// 保留の問いの分け方 (定義の説明のとおり) で、データから切片を選び直す。(切片の名前, 層 → ID)。
fn split_holdout(
    h: &Holdout,
    dev: &Subset,
    entries: &[Entry],
) -> Vec<(String, BTreeMap<String, Vec<String>>)> {
    let dev_ids: std::collections::HashSet<&str> = dev.ids().map(|(_, i)| i).collect();
    let mut ranked: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for e in entries.iter().filter(|e| {
        !h.excluded_types.contains(&e.question_type) && !dev_ids.contains(e.question_id.as_str())
    }) {
        let key = hex(&Sha256::digest(
            format!("{}:{}", h.seed, e.question_id).as_bytes(),
        ));
        ranked
            .entry(e.stratum().to_owned())
            .or_default()
            .push((key, e.question_id.clone()));
    }
    for v in ranked.values_mut() {
        v.sort();
    }
    let mut out = Vec::new();
    for s in &h.slice {
        let counts: BTreeMap<String, usize> =
            ranked.iter().map(|(k, v)| (k.clone(), v.len())).collect();
        let q = quotas(s.size, &counts);
        let mut chosen = BTreeMap::new();
        for (k, v) in &mut ranked {
            let n = q[k];
            chosen.insert(k.clone(), v.drain(..n).map(|(_, id)| id).collect());
        }
        out.push((s.name.clone(), chosen));
    }
    out
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

    /// 内容語の数。
    pub fn content(key: &str) -> usize {
        tokens(key)
            .iter()
            .filter(|w| !STOP.contains(&w.as_str()))
            .count()
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

/// 流す問いの組 (`EVORAG_LME_SET`): `dev` (既定、開発の組) か `holdout:<切片>` (保留の問い)。
#[derive(Debug)]
struct Plan {
    /// 結果のファイルの名前に使う (`dev`・`holdout-A`)。
    name: String,
    file: &'static str,
    ids_sha256: String,
    /// (層, ID)。
    ids: Vec<(String, String)>,
    /// 問いに答えさせてよいか (封をした切片は取り込みだけ)。
    open: bool,
}

fn plan(subset: &Subset) -> Plan {
    let set = std::env::var("EVORAG_LME_SET").unwrap_or_else(|_| "dev".into());
    if set == "dev" {
        return Plan {
            name: "dev".into(),
            file: SUBSET_FILE,
            ids_sha256: subset.ids_sha256(),
            ids: subset
                .ids()
                .map(|(s, i)| (s.to_owned(), i.to_owned()))
                .collect(),
            open: true,
        };
    }
    let Some(name) = set.strip_prefix("holdout:") else {
        panic!("EVORAG_LME_SET is dev or holdout:<slice> (got {set:?})");
    };
    let h = holdout();
    let s = h
        .slice
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no slice {name:?} in {HOLDOUT_FILE}"));
    let ids: Vec<(String, String)> = s
        .stratum
        .iter()
        .flat_map(|st| st.ids.iter().map(|i| (st.name.clone(), i.clone())))
        .collect();
    let mut sha = Sha256::new();
    for (_, id) in &ids {
        sha.update(id.as_bytes());
        sha.update(b"\n");
    }
    Plan {
        name: format!("holdout-{name}"),
        file: HOLDOUT_FILE,
        ids_sha256: hex(&sha.finalize()),
        ids,
        open: !s.opened.is_empty(),
    }
}

/// 取り込みの手順の版。抽出・ノート・埋め込みなど、保存したデータ根の中身を変える変更で上げる (上げると、
/// 保存したデータ根を使わずに取り込み直す)。
const WRITE_PATH: u32 = 1;

/// 保存したデータ根 (`EVORAG_LME_ROOTS`)。問いごとに `<dir>/<id>/data` と `<dir>/<id>/ingest.json`。
/// 取り込み (書き込みと裏の処理) を済ませ、問いを送る前の状態を保存し、想起と文脈だけを変える比べ方で使い回す
/// (09 §5.2)。
struct Roots(PathBuf);

impl Roots {
    fn from_env() -> Option<Self> {
        std::env::var_os("EVORAG_LME_ROOTS").map(|d| Self(PathBuf::from(d)))
    }

    /// 同じ手順の版と同じモデルで取り込んだデータ根があれば、`to` に写して、その記録を返す。
    fn load(
        &self,
        id: &str,
        fingerprint: &serde_json::Value,
        to: &Path,
    ) -> Option<serde_json::Value> {
        let dir = self.0.join(id);
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("ingest.json")).ok()?).ok()?;
        if &marker["fingerprint"] != fingerprint {
            println!("{id}: the saved data root was ingested differently; ingesting again");
            return None;
        }
        copy_dir(&dir.join("data"), to);
        Some(marker)
    }

    fn save(&self, id: &str, from: &Path, marker: &serde_json::Value) {
        let dir = self.0.join(id);
        let data = dir.join("data");
        if data.exists() {
            std::fs::remove_dir_all(&data).expect("remove the old saved data root");
        }
        copy_dir(from, &data);
        eval::write_json(&dir.join("ingest.json"), marker);
    }
}

/// フォルダを写す (錠のプロセスの記録と、空きの予約のファイルは写さない。開くときに作り直す)。
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the folder");
    for e in std::fs::read_dir(from).expect("read the folder") {
        let e = e.expect("a folder entry");
        let name = e.file_name();
        if name == "lock.pid" || name == "reserve.space" {
            continue;
        }
        let ty = e.file_type().expect("a file type");
        if ty.is_dir() {
            copy_dir(&e.path(), &to.join(&name));
        } else {
            std::fs::copy(e.path(), to.join(&name)).expect("copy a file");
        }
    }
}

/// 正解の内容語の数 (判定の照合の上限を見積もる。多いほど言い回しで外れやすい、09 §5.2)。
fn content_words(answer: &str) -> usize {
    score::keys(answer)
        .iter()
        .map(|k| score::content(k))
        .min()
        .unwrap_or(0)
}

/// LongMemEval の問いの組を流し、結果を記録する (合否には使わない。版の間の比較は `cargo xtask lme-compare`)。
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
    let plan = plan(&subset);
    let ingest_only = std::env::var_os("EVORAG_LME_INGEST_ONLY").is_some();
    assert!(
        plan.open || ingest_only,
        "{} is sealed (opened = \"\" in {HOLDOUT_FILE}); set EVORAG_LME_INGEST_ONLY=1 to only ingest it",
        plan.name
    );
    let limit: Option<usize> = std::env::var("EVORAG_LONGMEMEVAL_LIMIT")
        .ok()
        .map(|v| v.parse().expect("EVORAG_LONGMEMEVAL_LIMIT is a number"));
    let settle_limit = std::env::var("EVORAG_LME_SETTLE_SECS")
        .ok()
        .map_or(SETTLE, |v| {
            Duration::from_secs(v.parse().expect("EVORAG_LME_SETTLE_SECS is a number"))
        });
    let roots = Roots::from_env();
    // 送る前に「入力が止まった」を知らせる (既定)。`EVORAG_LME_TYPING=0` で、応答の経路だけで想起する。
    let typing = std::env::var("EVORAG_LME_TYPING").map_or(true, |v| v != "0");
    let by_id: HashMap<&str, &Entry> = entries
        .iter()
        .map(|e| (e.question_id.as_str(), e))
        .collect();
    eval::init_log();
    let chat = servers.chat_client();
    let models = eval::models(&servers).await;
    // 取り込みの結果を左右するもの (手順の版と、応答・埋め込み・リランカーのモデル)。
    let fingerprint = serde_json::json!({
        "write_path": WRITE_PATH,
        "chat": models["chat"]["model_fp"],
        "embed": models.get("embed").map(|m| m["model_fp"].clone()),
        "rerank": models.get("rerank").map(|m| m["model_fp"].clone()),
    });
    let mut checks = Checks::default();
    let mut rows = Vec::new();
    let mut details = Vec::new();
    let started = Instant::now();
    for (stratum, id) in plan.ids.iter().take(limit.unwrap_or(usize::MAX)) {
        let (stratum, id) = (stratum.as_str(), id.as_str());
        let e = by_id
            .get(id)
            .unwrap_or_else(|| panic!("{id} is not in {}", path.display()));
        let t0 = Instant::now();
        let (sessions, ingest) = sessions_of(e);
        let dir = TempDir::new("live-lme");
        let first = sessions
            .first()
            .map_or(Timestamp::from_millis(0), |s| s.start);
        let asked = parse_date(&e.question_date)
            .unwrap_or_else(|| panic!("{id}: unreadable question date"));
        let saved = roots
            .as_ref()
            .and_then(|r| r.load(id, &fingerprint, dir.path()));
        let marker = if let Some(m) = saved {
            m
        } else {
            let clock = Arc::new(ManualClock::new(first));
            let last = eval::write_sessions(dir.path(), &clock, &sessions);
            // 問いは、以前の会話の後に問う (データで同じ日の後の時刻に会話があっても、順を保つ)。
            let at = asked.as_millis().max(last.as_millis() + 60_000);
            checks.seed(dir.path());
            let core_clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
            let (core, mut rx, _) = Core::start(
                eval::config(dir.path(), PromptLanguage::En, &servers, core_clock),
                chat.clone(),
            )
            .await
            .expect("the core starts against the live llama-server");
            core.settle().await;
            let settled = eval::settle(&core, settle_limit).await;
            checks.capture(&core);
            let facts = eval::facts(&core, &mut rx).await;
            core.shutdown().await;
            let m = serde_json::json!({
                "fingerprint": fingerprint,
                "evorag": env!("CARGO_PKG_VERSION"),
                "asked_at_ms": at,
                "settled": settled,
                "facts": facts,
                "sessions": ingest.sessions,
                "user_turns": ingest.user_turns,
                "assistant_turns": ingest.assistant_turns,
                "dropped_turns": ingest.dropped,
                "merged_turns": ingest.merged,
                "ingest_seconds": t0.elapsed().as_secs_f64(),
            });
            if let Some(r) = &roots {
                r.save(id, dir.path(), &m);
            }
            m
        };
        if ingest_only {
            println!(
                "{id} ({stratum}): ingested, settled {} in {:.0}s",
                marker["settled"],
                t0.elapsed().as_secs_f64()
            );
            rows.push(serde_json::json!({
                "id": id,
                "stratum": stratum,
                "settled": marker["settled"],
                "facts": marker["facts"].as_array().map_or(0, Vec::len),
                "ingest_seconds": marker["ingest_seconds"],
            }));
            continue;
        }
        let at = marker["asked_at_ms"]
            .as_i64()
            .expect("the saved data root records when to ask");
        let core_clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::from_millis(at)));
        let mut cfg = eval::config(dir.path(), PromptLanguage::En, &servers, core_clock);
        // 実際の使い方に合わせ、入力が止まったときの処理 (05 §4.5) を済ませてから送る。
        cfg.precompute = typing;
        let (core, mut rx, _) = Core::start(cfg, chat.clone())
            .await
            .expect("the core starts against the live llama-server");
        core.settle().await;
        let ready = eval::settle(&core, settle_limit).await;
        let timed = eval::ask_timed(&core, &mut rx, &e.question, ANSWER, typing).await;
        core.shutdown().await;
        let (response, sources, record, ttft, total) = match timed {
            Some(t) => (
                t.turn.text,
                t.turn.sources,
                eval::answer_record(dir.path(), t.turn.id),
                t.ttft.map(|d| d.as_secs_f64()),
                Some(t.total.as_secs_f64()),
            ),
            None => (String::new(), 0, serde_json::Value::Null, None, None),
        };
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
            "gold_content_words": content_words(&answer),
            "sources": sources,
            "facts": marker["facts"].as_array().map_or(0, Vec::len),
            "settled": marker["settled"],
            "ready": ready,
            "sessions": marker["sessions"],
            "user_turns": marker["user_turns"],
            "assistant_turns": marker["assistant_turns"],
            "dropped_turns": marker["dropped_turns"],
            "merged_turns": marker["merged_turns"],
            "ttft_seconds": ttft,
            "answer_seconds": total,
            "answer": record,
            "seconds": seconds,
        }));
        details.push(serde_json::json!({
            "id": id,
            "question": e.question,
            "answer": answer,
            "response": response,
            "facts": marker["facts"],
        }));
    }
    let n = rows.len();
    let file = format!(
        "EVAL-longmemeval{}.json",
        if plan.name == "dev" {
            String::new()
        } else {
            format!("-{}", plan.name)
        }
    );
    if ingest_only {
        println!(
            "longmemeval: ingested {n} questions in {:.0}s",
            started.elapsed().as_secs_f64()
        );
        return;
    }
    let mut per: BTreeMap<&str, (usize, usize, f64)> = BTreeMap::new();
    for r in &rows {
        let s = r["stratum"].as_str().expect("a stratum");
        let p = per.entry(s).or_default();
        p.0 += 1;
        p.1 += usize::from(r["correct"] == true);
        p.2 += r["token_recall"].as_f64().unwrap_or(0.0);
    }
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
    // 判定の照合で解ける見込みの問い (abstention か、正解の内容語が SOLVABLE_WORDS 以下)。
    let solvable: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| {
            r["stratum"] == ABSTENTION
                || r["gold_content_words"]
                    .as_u64()
                    .is_some_and(|w| w <= SOLVABLE_WORDS)
        })
        .collect();
    let solvable_correct = solvable.iter().filter(|r| r["correct"] == true).count();
    println!(
        "longmemeval ({}): {correct}/{n} correct ({:.3}) in {:.0}s",
        plan.name,
        ratio(correct as f64, n),
        started.elapsed().as_secs_f64()
    );
    let v = serde_json::json!({
        "id": "EVAL-longmemeval",
        "set": plan.name,
        "evorag": env!("CARGO_PKG_VERSION"),
        "date": eval::today(),
        "subset_file": plan.file,
        "subset_ids_sha256": plan.ids_sha256,
        "source": subset.source,
        "source_sha256": subset.source_sha256,
        "license": subset.license,
        "partial": limit.is_some_and(|l| l < plan.ids.len()),
        "typing": typing,
        "setup": servers.setup(),
        "models": models,
        "ingest": "haystack turns (user and assistant, as in the data) written through the store's write path at the session dates; facts, notes and embeddings by evorag's own sleep-time steps",
        "judge": "deterministic: NFKC + lowercase word tokens (number words to digits); correct if the gold answer's word sequence appears in the response or all its content words do; _abs: an abstention marker. Not comparable with the official GPT-4o judge",
        "questions": n,
        "correct": correct,
        "accuracy": ratio(correct as f64, n),
        "mean_token_recall": ratio(recall, n),
        "solvable_words": SOLVABLE_WORDS,
        "solvable_questions": solvable.len(),
        "solvable_correct": solvable_correct,
        "solvable_accuracy": ratio(solvable_correct as f64, solvable.len()),
        "per_type": per_type,
        "seconds": started.elapsed().as_secs_f64(),
        "results": rows,
    });
    eval::write_json(&eval::out_dir().join(&file), &v);
    eval::write_json(
        &PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(file.replace(".json", ".details.json")),
        &serde_json::json!({ "id": "EVAL-longmemeval", "set": plan.name, "details": details }),
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

/// 保留の問いの定義が、データから選び直したものと一致する (データを置いたときだけ。サーバは要らない)。
#[test]
#[ignore = "needs the LongMemEval data (EVORAG_LONGMEMEVAL_DATA)"]
fn longmemeval_holdout_matches_the_data() {
    let dev = subset();
    let Some((_, entries)) = load(&dev) else {
        eprintln!("longmemeval: set EVORAG_LONGMEMEVAL_DATA to the data file (skipped)");
        return;
    };
    let h = holdout();
    let split = split_holdout(&h, &dev, &entries);
    assert_eq!(split.len(), h.slice.len());
    for ((name, chosen), s) in split.iter().zip(&h.slice) {
        assert_eq!(name, &s.name);
        let listed: BTreeMap<String, Vec<String>> = s
            .stratum
            .iter()
            .map(|st| (st.name.clone(), st.ids.clone()))
            .collect();
        let chosen: BTreeMap<String, Vec<String>> = chosen
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(&chosen, &listed, "slice {name}");
    }
}

/// 保留の問いの定義の形 (サーバもデータも要らない)。
#[test]
fn the_longmemeval_holdout_is_well_formed() {
    let dev = subset();
    let h = holdout();
    assert_eq!(h.source_sha256, dev.source_sha256);
    assert_eq!(h.source, dev.source);
    assert_eq!(h.excluded_subset, SUBSET_FILE);
    assert_eq!(h.excluded_types, dev.excluded_types);
    assert!(!h.seed.is_empty() && h.seed != dev.seed);
    assert!(!h.source_url.is_empty() && !h.license.is_empty());
    let dev_ids: std::collections::HashSet<&str> = dev.ids().map(|(_, i)| i).collect();
    let mut seen = std::collections::BTreeSet::new();
    for s in &h.slice {
        let n: usize = s.stratum.iter().map(|st| st.ids.len()).sum();
        assert_eq!(n, s.size, "slice {}: size", s.name);
        for st in &s.stratum {
            assert_eq!(st.ids.len(), st.quota, "{}/{}: quota", s.name, st.name);
            for id in &st.ids {
                assert!(seen.insert(id.as_str()), "{id}: listed twice");
                assert!(!dev_ids.contains(id.as_str()), "{id}: in the dev subset");
                assert_eq!(
                    id.ends_with("_abs"),
                    st.name == ABSTENTION,
                    "{id} in {}",
                    st.name
                );
            }
        }
    }
    // 切片は前から順に開ける (開けた切片の累積で判定するため)。
    let opened: Vec<bool> = h.slice.iter().map(|s| !s.opened.is_empty()).collect();
    assert!(
        opened.windows(2).all(|w| w[0] || !w[1]),
        "slices open in order: {opened:?}"
    );
}

#[test]
fn holdout_quotas_follow_the_largest_remainder() {
    let counts: BTreeMap<String, usize> = [("a", 5), ("b", 3), ("c", 2)]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let q = quotas(4, &counts);
    assert_eq!(q.values().sum::<usize>(), 4);
    assert_eq!((q["a"], q["b"], q["c"]), (2, 1, 1));
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
