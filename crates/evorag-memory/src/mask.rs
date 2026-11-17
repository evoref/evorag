//! 添える往復の中の値を `[非表示]` にする (04 §7.1・§7.5 の「想起での private」、05 §3.4)。I/O を持たない核と、
//! 記録から材料を読む口。
//!
//! 伏せる範囲は、往復 (利用者の発話と、それへの応答) ごとに決める。
//! - private の事実: 出典 (改訂の番号が今のターンと合うもの) がその往復にあれば、往復のすべてのターンで、
//!   値の鍵の正規化 (04 §5.6) の後に値と一致する範囲 (英数字は語の境界で)。出典の根拠の範囲の中に一致が
//!   見つからないときは、根拠の範囲の全体を伏せる (安全の側に倒す)。
//! - 伏せ字にしないで忘れた事実: 忘れるの指示の `hidden` の範囲 (改訂の番号が今のターンと合うもの)。
//!
//! 本文は保存したまま変えない。置き換えは、添える・帯に出す・検証のプロンプトに入れる直前に当てる。

use std::ops::Range;

use evorag_store::ReadTxn;
use evorag_types::Id;

use crate::facts::{HiddenBody, load_facts, load_hidden};
use crate::fold::{FactSet, FactState, Source};
use crate::read::MemoryError;
use crate::value_key::{Edges, Mapped, normalize_v1};

/// `[非表示]` の印 (プロンプトの言語で選ぶ、04 §7.5)。
#[must_use]
pub fn hidden_marker(language: &str) -> &'static str {
    if language == "ja" {
        "[非表示]"
    } else {
        "[hidden]"
    }
}

/// 往復の中のターン 1 つ (保存した本文と、その改訂の番号)。
#[derive(Debug, Clone, Copy)]
pub struct TurnText<'a> {
    pub id: Id,
    pub rev: u32,
    pub text: &'a str,
}

/// private の値 1 つ (正規化した値と出典)。
#[derive(Debug, Clone)]
struct PrivateValue {
    norm: String,
    sources: Vec<Source>,
}

/// 伏せる材料 (private の値と、伏せ字にしないで忘れた範囲)。
#[derive(Debug, Clone, Default)]
pub struct Masker {
    private: Vec<PrivateValue>,
    hidden: Vec<HiddenBody>,
}

impl Masker {
    /// private の事実 (`Retracted` でなく値のあるもの) と、忘れるの指示の `hidden` の範囲から作る。
    #[must_use]
    pub fn new(set: &FactSet, hidden: Vec<HiddenBody>) -> Self {
        let private = set
            .facts
            .iter()
            .filter(|f| f.private && f.state != FactState::Retracted)
            .filter_map(|f| {
                let norm = normalize_v1(f.value.as_deref()?);
                (!norm.is_empty()).then(|| PrivateValue {
                    norm,
                    sources: f.sources.iter().copied().collect(),
                })
            })
            .collect();
        Self { private, hidden }
    }

    /// 記録から作る (事実と、忘れるの指示)。
    ///
    /// # Errors
    /// 記録を読めないとき。
    pub fn load(tx: &ReadTxn<'_>) -> Result<Self, MemoryError> {
        Ok(Self::new(&load_facts(tx)?.set, load_hidden(tx)?))
    }

    /// 伏せるものが無い。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.private.is_empty() && self.hidden.is_empty()
    }

    /// 往復 `exchange` の `i` 番目のターンの、伏せる範囲 (本文のバイト位置。重なりうる)。
    #[must_use]
    pub fn ranges(&self, exchange: &[TurnText<'_>], i: usize) -> Vec<Range<usize>> {
        let Some(t) = exchange.get(i) else {
            return Vec::new();
        };
        let mut out: Vec<Range<usize>> = self
            .hidden
            .iter()
            .filter(|h| h.turn == t.id && h.rev == t.rev)
            .map(|h| widen(t.text, h.start as usize..h.end as usize))
            .collect();
        let current = |s: &Source| exchange.iter().any(|x| x.id == s.turn && x.rev == s.rev);
        let mut mapped: Option<Option<Mapped>> = None;
        for pv in &self.private {
            if !pv.sources.iter().any(current) {
                continue;
            }
            let m = mapped.get_or_insert_with(|| Mapped::new(t.text));
            let hits: Vec<Range<usize>> = m
                .as_ref()
                .map(|m| {
                    m.find(&pv.norm, Edges::Word)
                        .into_iter()
                        .map(|h| h.source)
                        .collect()
                })
                .unwrap_or_default();
            for s in pv
                .sources
                .iter()
                .filter(|s| s.turn == t.id && s.rev == t.rev)
            {
                let span = widen(t.text, s.start as usize..s.end as usize);
                if !hits
                    .iter()
                    .any(|h| h.start < span.end && span.start < h.end)
                {
                    out.push(span);
                }
            }
            out.extend(hits);
        }
        out
    }

    /// 往復の各ターンの本文に、`marker` の置き換えを当てたもの (`exchange` の順)。
    #[must_use]
    pub fn mask(&self, exchange: &[TurnText<'_>], marker: &str) -> Vec<String> {
        (0..exchange.len())
            .map(|i| apply_ranges(exchange[i].text, &self.ranges(exchange, i), marker))
            .collect()
    }
}

/// 範囲を本文の長さに収め、端を文字の境界まで外へ広げる。
fn widen(text: &str, r: Range<usize>) -> Range<usize> {
    let mut start = r.start.min(text.len());
    let mut end = r.end.clamp(start, text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// 範囲 (重なり・隣り合いはまとめる) を `marker` に置き換える。
#[must_use]
pub fn apply_ranges(text: &str, ranges: &[Range<usize>], marker: &str) -> String {
    let mut rs: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| widen(text, r.clone()))
        .filter(|r| !r.is_empty())
        .collect();
    if rs.is_empty() {
        return text.to_owned();
    }
    rs.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(rs.len());
    for r in rs {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in merged {
        out.push_str(&text[at..r.start]);
        out.push_str(marker);
        at = r.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::fold::{Fact, Tense};
    use crate::value_key::{ForgetKey, value_key};

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 16])
    }

    fn private_fact(value: &str, turn: Id, rev: u32, span: Range<u32>) -> Fact {
        Fact {
            id: id(90),
            subject: "self".into(),
            attribute: "pet".into(),
            value: Some(value.into()),
            vk: Some(value_key(&ForgetKey::from_bytes([1; 32]), value)),
            tense: Tense::Current,
            state: FactState::Live,
            sources: BTreeSet::from([Source {
                turn,
                start: span.start,
                end: span.end,
                rev,
            }]),
            private: true,
        }
    }

    /// 出典のある往復では、発話と応答の両方で値に一致する範囲を伏せる。往復の外では伏せない (04 §7.5)。
    #[test]
    fn a_private_value_is_hidden_in_its_exchange_only() {
        let user = "私は猫を飼っています";
        let answer = "猫ちゃん、かわいいですね。ネコ科ですか";
        let start = user.find('猫').unwrap() as u32;
        let set = FactSet {
            facts: vec![private_fact("猫", id(1), 0, start..start + 3)],
        };
        let m = Masker::new(&set, Vec::new());
        let exchange = [
            TurnText {
                id: id(1),
                rev: 0,
                text: user,
            },
            TurnText {
                id: id(2),
                rev: 0,
                text: answer,
            },
        ];
        let masked = m.mask(&exchange, hidden_marker("ja"));
        assert_eq!(masked[0], "私は[非表示]を飼っています");
        // カタカナとひらがな・漢字は統一しない (04 §5.6)。
        assert_eq!(masked[1], "[非表示]ちゃん、かわいいですね。ネコ科ですか");
        // 出典の無い往復では伏せない。
        let other = [TurnText {
            id: id(3),
            rev: 0,
            text: "猫の話",
        }];
        assert_eq!(m.mask(&other, "[hidden]"), ["猫の話"]);
    }

    /// 改訂の番号が今と合わない出典 (伏せ字の前の本文から作ったもの) では伏せない。英数字は語の境界で当てる。
    #[test]
    fn stale_sources_do_not_mask_and_words_keep_their_boundaries() {
        let text = "Ann and annual Ann";
        let set = FactSet {
            facts: vec![private_fact("ann", id(1), 2, 0..3)],
        };
        let m = Masker::new(&set, Vec::new());
        let stale = [TurnText {
            id: id(1),
            rev: 1,
            text,
        }];
        assert_eq!(m.mask(&stale, "[hidden]"), [text]);
        let current = [TurnText {
            id: id(1),
            rev: 2,
            text,
        }];
        assert_eq!(
            m.mask(&current, "[hidden]"),
            ["[hidden] and annual [hidden]"]
        );
    }

    /// 根拠の範囲に一致が見つからないときは、根拠の範囲を伏せる。忘れたときの `hidden` の範囲も伏せる。
    #[test]
    fn spans_without_a_match_and_forgotten_ranges_are_hidden() {
        let text = "うちのペットはとても元気";
        let set = FactSet {
            facts: vec![private_fact("猫", id(1), 0, 0..9)],
        };
        let hidden = vec![HiddenBody {
            turn: id(1),
            rev: 0,
            start: 21,
            end: 30,
        }];
        let m = Masker::new(&set, hidden);
        let t = [TurnText {
            id: id(1),
            rev: 0,
            text,
        }];
        assert_eq!(m.mask(&t, "[非表示]"), ["[非表示]ペットは[非表示]元気"]);
    }

    #[test]
    fn ranges_are_merged_and_kept_on_char_boundaries() {
        assert_eq!(apply_ranges("abcdef", &[1..3, 2..4, 5..6], "*"), "a*e*");
        // 文字の途中の端は、文字の境界まで外へ広げる。
        let inside = vec![1..2, 2..4];
        assert_eq!(apply_ranges("あいう", &inside, "*"), "*う");
        assert_eq!(apply_ranges("abc", &[], "*"), "abc");
    }
}
