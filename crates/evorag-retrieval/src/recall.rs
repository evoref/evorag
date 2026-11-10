//! 以前の会話を引いて選ぶ (05 §3)。v0.1 は語彙の索引だけ。
//!
//! 1. 問いを単位に分ける (追い質問なら、直前の発話を足した問いも作る、§3.1)。
//! 2. BM25 で候補を集める (既定 50 件、§3.2)。
//! 3. 上位から、内容の比 (§2.1) がしきい値 (0.14) 以上で、上位の最大の 0.7 倍以上のものを選ぶ
//!    (§3.4)。同点は新しいものを先にする。

use std::collections::HashMap;

use evorag_index::lex::{Doc, Snapshot};
use evorag_types::Id;

use crate::normalize::{Unit, units};

/// 判定点 (05 §3.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 添えるものがあった。
    Fire,
    /// 候補はあったが、しきい値を超えなかった。
    Abstain,
    /// 索引が無くて引けなかった。
    Skip,
}

/// 選び方の設定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecallConfig {
    /// 集める候補の数。
    pub candidates: usize,
    /// しきい値を調べる上位の数。
    pub top: usize,
    /// 内容の比のしきい値。
    pub threshold: f64,
    /// 上位の最大に対する比 (これ未満は添えない)。
    pub relative: f64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            candidates: 50,
            top: 3,
            threshold: 0.14,
            relative: 0.7,
        }
    }
}

/// 選んだ 1 件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: Id,
    pub session: Id,
    /// BM25 のスコア (全単位)。
    pub score: f64,
    /// 内容の比 (0〜1)。
    pub ratio: f64,
}

/// 引いた結果。
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    pub decision: Decision,
    /// 添えるもの (スコアの高い順)。
    pub hits: Vec<Hit>,
    /// 集めた候補 (スコアの高い順。評価のため)。
    pub candidates: Vec<Hit>,
}

/// 指示語 (追い質問の判定、05 §3.1)。
const DEICTIC: &[&str] = &[
    "それ",
    "あれ",
    "これ",
    "その",
    "あの",
    "この",
    "そこ",
    "あそこ",
    "そっち",
    "あっち",
    "こっち",
    "前者",
    "後者",
];
/// 追い質問とみなす長さ (文字) の上限。
const FOLLOW_UP_CHARS: usize = 20;

/// 前の発話が無いと意味が通らない、短い指示語つきの問いか。
pub fn is_follow_up(query: &str) -> bool {
    query.chars().count() <= FOLLOW_UP_CHARS && DEICTIC.iter().any(|d| query.contains(d))
}

fn rank(snap: &Snapshot, q: &[Unit], skip: &dyn Fn(&Doc) -> bool, cfg: &RecallConfig) -> Vec<Hit> {
    let all: Vec<&str> = q.iter().map(|u| u.term.as_str()).collect();
    let content: Vec<&str> = q
        .iter()
        .filter(|u| u.content)
        .map(|u| u.term.as_str())
        .collect();
    let scores = snap.score(&all, skip);
    let content_scores = snap.score(&content, skip);
    let denom = snap.self_score(&content);
    let mut hits: Vec<Hit> = scores
        .into_iter()
        .map(|(r, score)| {
            let d = snap.doc(r);
            let c = content_scores.get(&r).copied().unwrap_or(0.0);
            Hit {
                id: d.id,
                session: d.session,
                score,
                ratio: if denom > 0.0 {
                    (c / denom).min(1.0)
                } else {
                    0.0
                },
            }
        })
        .collect();
    // 同点は新しいものを先に (ID は生成の順)。
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.id.cmp(&a.id)));
    hits.truncate(cfg.candidates);
    hits
}

/// 引いて選ぶ。`previous` は直前の利用者の発話 (追い質問のとき足す)。`skip` が真の文書
/// (今のセッションの直近の会話に入っているターンなど) は除く。
pub fn recall(
    snap: &Snapshot,
    query: &str,
    previous: Option<&str>,
    skip: &dyn Fn(&Doc) -> bool,
    cfg: &RecallConfig,
) -> Recall {
    if snap.is_empty() {
        return Recall {
            decision: Decision::Skip,
            hits: Vec::new(),
            candidates: Vec::new(),
        };
    }
    let mut candidates = rank(snap, &units(query), skip, cfg);
    if let Some(prev) = previous.filter(|_| is_follow_up(query)) {
        let joined = format!("{prev}\n{query}");
        let mut by_id: HashMap<Id, Hit> = candidates.iter().map(|h| (h.id, *h)).collect();
        for h in rank(snap, &units(&joined), skip, cfg) {
            by_id
                .entry(h.id)
                .and_modify(|e| {
                    if h.ratio > e.ratio {
                        *e = h;
                    }
                })
                .or_insert(h);
        }
        candidates = by_id.into_values().collect();
        candidates.sort_by(|a, b| {
            b.ratio
                .total_cmp(&a.ratio)
                .then(b.score.total_cmp(&a.score))
                .then(b.id.cmp(&a.id))
        });
        candidates.truncate(cfg.candidates);
    }
    let top = &candidates[..candidates.len().min(cfg.top)];
    let best = top.iter().map(|h| h.ratio).fold(0.0, f64::max);
    let hits: Vec<Hit> = top
        .iter()
        .filter(|h| h.ratio >= cfg.threshold && h.ratio >= cfg.relative * best)
        .copied()
        .collect();
    let decision = if hits.is_empty() {
        Decision::Abstain
    } else {
        Decision::Fire
    };
    Recall {
        decision,
        hits,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_ups_are_short_and_deictic() {
        assert!(is_follow_up("それはいつ？"));
        assert!(is_follow_up("あそこの宿は？"));
        assert!(!is_follow_up("京都の宿は？"));
        assert!(!is_follow_up(
            "それとは別に、来月の北海道旅行で使うレンタカーの予約について教えてほしい"
        ));
    }
}
