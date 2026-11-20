//! LongMemEval の結果の対の比較 (09 §5.2 の「判定の方法」): `cargo xtask lme-compare`。
//!
//! 起点 (`--base`) と新しい版 (`--new`) の `EVAL-longmemeval*.json` を問いの ID で組にし、問いごとの正誤の
//! 食い違いで McNemar の正確検定 (二項検定) を行う。保留の問いは、開けた切片の結果を並べて渡す (累積で判定する)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 1 問の結果 (比べるのに使う項目だけ)。
#[derive(Debug, Clone)]
struct Row {
    stratum: String,
    correct: bool,
    token_recall: f64,
    solvable: bool,
    /// 想起で添えた (`Fire`) か。
    fired: bool,
    ttft: Option<f64>,
}

/// 照合で解ける見込みの問いの、正解の内容語の数の上限 (結果のファイルに書いた値が無いときの既定)。
const SOLVABLE_WORDS: u64 = 5;

fn read(paths: &[PathBuf]) -> Result<BTreeMap<String, Row>, String> {
    let mut out = BTreeMap::new();
    for p in paths {
        let text =
            std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
        if v["partial"] == true {
            return Err(format!(
                "{}: a partial run (--limit) cannot be compared",
                p.display()
            ));
        }
        let limit = v["solvable_words"].as_u64().unwrap_or(SOLVABLE_WORDS);
        let rows = v["results"]
            .as_array()
            .ok_or_else(|| format!("{}: no results", p.display()))?;
        for r in rows {
            let id = r["id"]
                .as_str()
                .ok_or_else(|| format!("{}: a result without an id", p.display()))?;
            let stratum = r["stratum"].as_str().unwrap_or("?").to_owned();
            let row = Row {
                solvable: stratum == "abstention"
                    || r["gold_content_words"].as_u64().is_none_or(|w| w <= limit),
                stratum,
                correct: r["correct"] == true,
                token_recall: r["token_recall"].as_f64().unwrap_or(0.0),
                fired: r["answer"]["recall"]["decision"] == "fire",
                ttft: r["ttft_seconds"].as_f64(),
            };
            if out.insert(id.to_owned(), row).is_some() {
                return Err(format!("{id} appears twice in --{}", p.display()));
            }
        }
    }
    Ok(out)
}

/// 二項分布 Bin(n, 1/2) の下側の確率 P(X ≤ k)。
fn binom_cdf_half(n: u64, k: u64) -> f64 {
    let mut term = 0.5f64.powi(i32::try_from(n).unwrap_or(i32::MAX));
    let mut sum = term;
    for i in 0..k.min(n) {
        term *= (n - i) as f64 / (i + 1) as f64;
        sum += term;
    }
    sum.min(1.0)
}

/// McNemar の正確検定。`b` は起点だけ正しい数、`c` は新しい版だけ正しい数。(両側の p, 改善の片側の p)。
fn mcnemar(b: u64, c: u64) -> (f64, f64) {
    let n = b + c;
    if n == 0 {
        return (1.0, 1.0);
    }
    let two = (2.0 * binom_cdf_half(n, b.min(c))).min(1.0);
    // 改善の片側: c 以上が偶然に出る確率 = P(X ≤ b)。
    let one = binom_cdf_half(n, b);
    (two, one)
}

#[derive(Debug, Default)]
struct Tally {
    n: u64,
    base: u64,
    new: u64,
    /// 起点だけ正しい。
    b: u64,
    /// 新しい版だけ正しい。
    c: u64,
    base_recall: f64,
    new_recall: f64,
}

impl Tally {
    fn add(&mut self, x: &Row, y: &Row) {
        self.n += 1;
        self.base += u64::from(x.correct);
        self.new += u64::from(y.correct);
        self.b += u64::from(x.correct && !y.correct);
        self.c += u64::from(!x.correct && y.correct);
        self.base_recall += x.token_recall;
        self.new_recall += y.token_recall;
    }

    fn json(&self) -> serde_json::Value {
        let (two, one) = mcnemar(self.b, self.c);
        let n = self.n.max(1) as f64;
        serde_json::json!({
            "questions": self.n,
            "base_correct": self.base,
            "new_correct": self.new,
            "only_base": self.b,
            "only_new": self.c,
            "p_two_sided": two,
            "p_improved": one,
            "base_mean_token_recall": self.base_recall / n,
            "new_mean_token_recall": self.new_recall / n,
        })
    }

    fn line(&self, name: &str) -> String {
        let (two, one) = mcnemar(self.b, self.c);
        let n = self.n.max(1) as f64;
        format!(
            "{name:26} {:4} {:4} → {:4}  (−{:3} +{:3})  p2={two:.4} p+={one:.4}  recall {:.3} → {:.3}",
            self.n,
            self.base,
            self.new,
            self.b,
            self.c,
            self.base_recall / n,
            self.new_recall / n
        )
    }
}

fn quantile(mut v: Vec<f64>, q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 - 1.0) * q).round() as usize;
    Some(v[i])
}

/// `cargo xtask lme-compare --base FILE... --new FILE... [--out FILE]`。
pub fn compare(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: cargo xtask lme-compare --base FILE... --new FILE... [--out FILE]";
    let (mut base, mut new, mut out) = (Vec::new(), Vec::new(), None);
    let mut target: Option<&mut Vec<PathBuf>> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => target = Some(&mut base),
            "--new" => target = Some(&mut new),
            "--out" => {
                out = Some(PathBuf::from(it.next().ok_or(USAGE)?));
                target = None;
            }
            path => target.as_mut().ok_or(USAGE)?.push(PathBuf::from(path)),
        }
    }
    if base.is_empty() || new.is_empty() {
        return Err(USAGE.into());
    }
    let (x, y) = (read(&base)?, read(&new)?);
    let missing: Vec<&String> = x.keys().filter(|k| !y.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} question(s) of --base are not in --new (e.g. {})",
            missing.len(),
            missing[0]
        ));
    }
    let mut all = Tally::default();
    let mut solvable = Tally::default();
    let mut per: BTreeMap<String, Tally> = BTreeMap::new();
    let (mut abs_n, mut abs_base_fired, mut abs_new_fired) = (0u64, 0u64, 0u64);
    let (mut ttft_base, mut ttft_new) = (Vec::new(), Vec::new());
    for (id, a) in &x {
        let b = &y[id];
        all.add(a, b);
        if a.solvable {
            solvable.add(a, b);
        }
        per.entry(a.stratum.clone()).or_default().add(a, b);
        if a.stratum == "abstention" {
            abs_n += 1;
            abs_base_fired += u64::from(a.fired);
            abs_new_fired += u64::from(b.fired);
        }
        ttft_base.extend(a.ttft);
        ttft_new.extend(b.ttft);
    }
    println!("{:26} {:>4} {:>4}   {:>4}", "", "n", "base", "new");
    println!("{}", all.line("all"));
    println!("{}", solvable.line("solvable"));
    for (k, t) in &per {
        println!("{}", t.line(k));
    }
    println!(
        "abstention questions with recall attached: {abs_base_fired}/{abs_n} → {abs_new_fired}/{abs_n}"
    );
    let fmt = |v: Option<f64>| v.map_or("-".to_owned(), |s| format!("{s:.2}s"));
    println!(
        "TTFT p50 {} → {}, p95 {} → {}",
        fmt(quantile(ttft_base.clone(), 0.5)),
        fmt(quantile(ttft_new.clone(), 0.5)),
        fmt(quantile(ttft_base.clone(), 0.95)),
        fmt(quantile(ttft_new.clone(), 0.95))
    );
    if let Some(path) = out {
        let names = |v: &[PathBuf]| -> Vec<String> {
            v.iter()
                .map(|p| {
                    p.file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                })
                .collect()
        };
        let v = serde_json::json!({
            "id": "EVAL-longmemeval-compare",
            "test": "McNemar exact (binomial) on per-question correctness",
            "base": names(&base),
            "new": names(&new),
            "all": all.json(),
            "solvable": solvable.json(),
            "per_type": per.iter().map(|(k, t)| (k.clone(), t.json())).collect::<serde_json::Map<_, _>>(),
            "abstention_fired": { "questions": abs_n, "base": abs_base_fired, "new": abs_new_fired },
            "ttft_seconds": {
                "base_p50": quantile(ttft_base.clone(), 0.5),
                "new_p50": quantile(ttft_new.clone(), 0.5),
                "base_p95": quantile(ttft_base, 0.95),
                "new_p95": quantile(ttft_new, 0.95),
            },
        });
        write(&path, &v)?;
    }
    Ok(())
}

fn write(path: &Path, v: &serde_json::Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())? + "\n";
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exact_mcnemar_test_matches_known_values() {
        // 18 対 6: 両側 p ≈ 0.0227 (提案書 §14 の見積もり)。
        let (two, one) = mcnemar(6, 18);
        assert!((two - 0.022_655).abs() < 1e-4, "{two}");
        assert!((one - 0.011_328).abs() < 1e-4, "{one}");
        // 14 対 6: 両側 p ≈ 0.115。
        let (two, _) = mcnemar(6, 14);
        assert!((two - 0.115_318).abs() < 1e-4, "{two}");
        // 食い違いが無ければ 1。
        assert_eq!(mcnemar(0, 0), (1.0, 1.0));
        // 同数なら両側は 1 を超えない。
        assert!(mcnemar(5, 5).0 <= 1.0);
    }

    #[test]
    fn the_binomial_cdf_sums_to_one() {
        assert!((binom_cdf_half(10, 10) - 1.0).abs() < 1e-12);
        assert!((binom_cdf_half(1, 0) - 0.5).abs() < 1e-12);
        assert!((binom_cdf_half(4, 1) - 5.0 / 16.0).abs() < 1e-12);
    }
}
