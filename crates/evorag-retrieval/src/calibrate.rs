//! リランクのしきい値の較正 (05 §3.7、S-14)。
//!
//! 本体に同梱した事例 (作った会話と問い、プロンプトの言語ごと) で、リランカーのしきい値と帯の下限を
//! 求める。利用者の入力は要らず、埋め込みにも頼らない。
//!
//! 1. 同梱の会話のターンを、手元だけの語彙の索引に入れる。
//! 2. 問いごとに、語彙の上位のターンを往復にまとめて上位 6 往復を候補にする (関係のある問いでは、
//!    正解の往復を必ず含める)。
//! 3. 候補をリランクし、最上位の値と、それが正解かを数える。
//! 4. しきい値は、関係の無い問いの最上位の値で 9 割が下回る点の直上。帯の下限は 7 割 5 分の点。
//!    合否は「順位」(関係のある問いの 8 割以上で正解が最上位) と「値」(語の重なる問いの 8 割以上で、
//!    正解が最上位かつしきい値以上) の 2 つ。

use evorag_index::lex::{DocInput, Snapshot};
use evorag_llm::{LlmClient, LlmError};
use evorag_types::Id;
use serde::Deserialize;

use crate::normalize::units;

/// 同梱の事例の版。事例を変えたら上げる (記録したしきい値を求め直すため)。
pub const CASES_VERSION: u32 = 1;
/// 候補にする往復の数 (05 §3.4)。
pub const TOP: usize = 6;

/// 同梱の事例の名前 (`ja-1` など)。しきい値の記録に残し、変わったら求め直す。
pub fn cases_id(lang: &str) -> String {
    format!("{lang}-{CASES_VERSION}")
}

/// リランカーに渡す往復の本文 (利用者の発話と、それへの応答)。応答の経路と同じ形にする。
pub fn exchange_text(lang: &str, user: &str, assistant: &str) -> String {
    match lang {
        "en" => format!("User: {user}\nAssistant: {assistant}"),
        _ => format!("利用者: {user}\nアシスタント: {assistant}"),
    }
}

#[derive(Deserialize)]
struct File {
    session: Vec<SessionDef>,
    case: Vec<CaseDef>,
}

#[derive(Deserialize)]
struct SessionDef {
    id: String,
    turns: Vec<String>,
}

#[derive(Deserialize)]
struct CaseDef {
    category: String,
    #[serde(default)]
    gold: Vec<String>,
    query: String,
}

/// 問いの区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 語の重なる問い。
    Easy,
    /// 言い方を変えた問い。
    Variant,
    /// 遠回しの問い。
    Para,
    /// 関係の無い問い。
    None,
}

struct Case {
    kind: Kind,
    /// 正解の往復の番号 (関係の無い問いは `None`)。
    gold: Option<usize>,
    query: String,
}

/// 同梱の事例を読み、索引を作ったもの。
pub struct Bundled {
    lang: String,
    /// 往復の本文。
    exchanges: Vec<String>,
    /// 索引の文書 (ターン) の番号 → 往復の番号。
    turn_exchange: Vec<usize>,
    snapshot: Snapshot,
    cases: Vec<Case>,
}

fn doc_id(n: usize) -> Id {
    Id::from_bytes((n as u128 + 1).to_be_bytes())
}

fn strip(turn: &str) -> &str {
    turn.strip_prefix("U: ")
        .or_else(|| turn.strip_prefix("A: "))
        .unwrap_or(turn)
}

impl Bundled {
    /// プロンプトの言語の事例 (`ja`・`en`。ほかは `ja`)。
    pub fn load(lang: &str) -> Self {
        let (lang, text) = match lang {
            "en" => ("en", include_str!("../calibration/en.toml")),
            _ => ("ja", include_str!("../calibration/ja.toml")),
        };
        let file: File = toml::from_str(text).expect("the bundled calibration cases parse");
        let mut exchanges = Vec::new();
        let mut turn_exchange = Vec::new();
        let mut docs = Vec::new();
        let mut gold_of = std::collections::HashMap::new();
        for (si, s) in file.session.iter().enumerate() {
            for (pair, chunk) in s.turns.chunks(2).enumerate() {
                let ex = exchanges.len();
                let user = strip(&chunk[0]);
                let assistant = chunk.get(1).map_or("", |a| strip(a));
                exchanges.push(exchange_text(lang, user, assistant));
                gold_of.insert(format!("{}-t{:02}", s.id, pair * 2 + 1), ex);
                for t in chunk {
                    let n = turn_exchange.len();
                    turn_exchange.push(ex);
                    docs.push(DocInput {
                        id: doc_id(n),
                        session: doc_id(1_000_000 + si),
                        terms: units(strip(t)).into_iter().map(|u| u.term).collect(),
                    });
                }
            }
        }
        let cases = file
            .case
            .into_iter()
            .map(|c| Case {
                kind: match c.category.as_str() {
                    "easy" => Kind::Easy,
                    "variant" => Kind::Variant,
                    "para" => Kind::Para,
                    _ => Kind::None,
                },
                gold: c.gold.iter().find_map(|g| gold_of.get(g).copied()),
                query: c.query,
            })
            .collect();
        Self {
            lang: lang.to_owned(),
            exchanges,
            turn_exchange,
            snapshot: Snapshot::in_memory(&docs),
            cases,
        }
    }

    /// 事例の名前 (`ja-1` など)。
    pub fn id(&self) -> String {
        cases_id(&self.lang)
    }

    /// 問いの数。
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    /// 問いが無いか。
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }

    /// 問いの候補の往復 (語彙の上位のターンを往復にまとめた上位 [`TOP`] 件)。
    fn candidates(&self, case: &Case) -> Vec<usize> {
        let q = units(&case.query);
        let terms: Vec<&str> = q.iter().map(|u| u.term.as_str()).collect();
        let mut scored: Vec<(Id, f64)> = self
            .snapshot
            .score(&terms, &|_| false)
            .into_iter()
            .map(|(r, s)| (self.snapshot.doc(r).id, s))
            .collect();
        // 同点は新しいもの (ID の大きいもの) を先にする。
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        let mut out: Vec<usize> = Vec::new();
        for (id, _) in scored {
            let n = u128::from_be_bytes(*id.as_bytes()) as usize - 1;
            let ex = self.turn_exchange[n];
            if !out.contains(&ex) {
                out.push(ex);
                if out.len() == TOP {
                    break;
                }
            }
        }
        if let Some(g) = case.gold
            && !out.contains(&g)
        {
            if out.len() == TOP {
                out.pop();
            }
            out.push(g);
        }
        out
    }

    /// すべての問いをリランクする。`progress` は (済んだ数, 全体の数) で呼ぶ。
    pub async fn run(
        &self,
        rerank: &dyn LlmClient,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<Vec<CaseResult>, LlmError> {
        let mut out = Vec::with_capacity(self.cases.len());
        for (i, case) in self.cases.iter().enumerate() {
            let cand = self.candidates(case);
            let result = if cand.is_empty() {
                CaseResult {
                    kind: case.kind,
                    top: f32::NEG_INFINITY,
                    top_ok: false,
                }
            } else {
                let docs: Vec<String> = cand.iter().map(|&e| self.exchanges[e].clone()).collect();
                let scores = rerank.rerank(&case.query, &docs).await?;
                let best = (0..scores.len())
                    .max_by(|&a, &b| scores[a].total_cmp(&scores[b]))
                    .unwrap_or(0);
                CaseResult {
                    kind: case.kind,
                    top: scores.get(best).copied().unwrap_or(f32::NEG_INFINITY),
                    top_ok: case.gold == Some(cand[best]),
                }
            };
            out.push(result);
            progress(i + 1, self.cases.len());
        }
        Ok(out)
    }
}

/// 1 つの問いの結果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaseResult {
    pub kind: Kind,
    /// 最上位の往復の生の値。
    pub top: f32,
    /// 最上位が正解の往復だった (関係の無い問いでは常に偽)。
    pub top_ok: bool,
}

/// 較正の結果。
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    pub threshold: f64,
    pub band_low: f64,
    /// 順位: 正解が最上位だった関係のある問いの数と、関係のある問いの数。
    pub rank_ok: u32,
    pub rank_n: u32,
    /// 値: 正解が最上位かつしきい値以上だった語の重なる問い (easy・variant) の数と、その数。
    pub value_ok: u32,
    pub value_n: u32,
    /// 遠回しの問い: しきい値以上、帯、全体の数 (合否には使わない)。
    pub para_above: u32,
    pub para_band: u32,
    pub para_n: u32,
    /// 関係の無い問いの数と、そのうちしきい値未満だった数。
    pub none_n: u32,
    pub none_below: u32,
    pub passed: bool,
}

/// 8 割以上か。
fn eighty(ok: u32, n: u32) -> bool {
    n > 0 && ok * 5 >= n * 4
}

/// 結果からしきい値と帯の下限を求め、合否を決める。関係の無い問いが無ければ `None`。
pub fn calibrate(results: &[CaseResult]) -> Option<Calibration> {
    let mut neg: Vec<f64> = results
        .iter()
        .filter(|r| r.kind == Kind::None)
        .map(|r| f64::from(r.top))
        .filter(|v| v.is_finite())
        .collect();
    if neg.is_empty() {
        return None;
    }
    neg.sort_by(f64::total_cmp);
    let at = |q: f64| neg[((q * neg.len() as f64).ceil() as usize).clamp(1, neg.len()) - 1];
    let threshold = at(0.9) + 0.01;
    let band_low = at(0.75);
    let count = |f: &dyn Fn(&CaseResult) -> bool| results.iter().filter(|r| f(r)).count() as u32;
    let relevant = |r: &CaseResult| r.kind != Kind::None;
    let overlap = |r: &CaseResult| matches!(r.kind, Kind::Easy | Kind::Variant);
    let above = |r: &CaseResult| r.top_ok && f64::from(r.top) >= threshold;
    let rank_ok = count(&|r| relevant(r) && r.top_ok);
    let rank_n = count(&relevant);
    let value_ok = count(&|r| overlap(r) && above(r));
    let value_n = count(&overlap);
    let para = |r: &CaseResult| r.kind == Kind::Para;
    let para_above = count(&|r| para(r) && above(r));
    let para_band = count(&|r| {
        para(r) && r.top_ok && f64::from(r.top) < threshold && f64::from(r.top) >= band_low
    });
    let none_below = count(&|r| r.kind == Kind::None && f64::from(r.top) < threshold);
    Some(Calibration {
        threshold,
        band_low,
        rank_ok,
        rank_n,
        value_ok,
        value_n,
        para_above,
        para_band,
        para_n: count(&para),
        none_n: count(&|r| r.kind == Kind::None),
        none_below,
        passed: eighty(rank_ok, rank_n) && eighty(value_ok, value_n),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_cases_load_with_gold_exchanges() {
        for lang in ["ja", "en"] {
            let b = Bundled::load(lang);
            assert_eq!(b.len(), 60, "{lang}");
            assert_eq!(b.id(), format!("{lang}-{CASES_VERSION}"));
            let relevant: Vec<&Case> = b.cases.iter().filter(|c| c.kind != Kind::None).collect();
            assert_eq!(relevant.len(), 36);
            assert!(relevant.iter().all(|c| c.gold.is_some()), "{lang}");
            // 候補は最大 6 往復で、関係のある問いでは正解を含む。
            for c in &b.cases {
                let cand = b.candidates(c);
                assert!(cand.len() <= TOP);
                if let Some(g) = c.gold {
                    assert!(cand.contains(&g));
                }
            }
        }
    }

    fn r(kind: Kind, top: f32, top_ok: bool) -> CaseResult {
        CaseResult { kind, top, top_ok }
    }

    #[test]
    fn the_threshold_is_just_above_the_ninetieth_percentile_of_irrelevant_queries() {
        let mut v: Vec<CaseResult> = (1..=10)
            .map(|i| r(Kind::None, -(i as f32), false))
            .collect();
        v.extend((0..8).map(|_| r(Kind::Easy, 2.0, true)));
        v.extend((0..2).map(|_| r(Kind::Para, -1.5, true)));
        let c = calibrate(&v).unwrap();
        // 関係の無い問いの値 −10..−1 の 9 割の点は −2。
        assert!((c.threshold - (-1.99)).abs() < 1e-6, "{c:?}");
        assert!((c.band_low - (-3.0)).abs() < 1e-6, "{c:?}");
        assert_eq!((c.rank_ok, c.rank_n, c.value_ok, c.value_n), (10, 10, 8, 8));
        assert_eq!((c.para_above, c.para_band, c.para_n), (2, 0, 2));
        assert_eq!((c.none_below, c.none_n), (9, 10));
        assert!(c.passed);
        // 語の重なる問いが値で落ちると不合格 (遠回しの問いは合否に使わない)。
        let mut low = v.clone();
        for x in low.iter_mut().filter(|x| x.kind == Kind::Easy).take(2) {
            x.top = -5.0;
        }
        assert!(!calibrate(&low).unwrap().passed);
        assert!(calibrate(&[r(Kind::Easy, 1.0, true)]).is_none());
    }
}
