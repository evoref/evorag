//! 添える往復の中の値を `[非表示]` にする (04 §7.1・§7.5 の「想起での private」、05 §3.4)。I/O を持たない核と、
//! 記録から材料を読む口。
//!
//! 伏せる範囲は、往復 (利用者の発話と、それへの応答) ごとに決める。
//! - private の事実: 出典 (改訂の番号が今のターンと合うもの) がその往復にあれば、往復のすべてのターンで、
//!   値の鍵の正規化 v1 (04 §5.6) の後に値と一致する範囲と、照合の形でそれと重ならずに一致する範囲 (英数字は
//!   語の境界で)。出典の根拠の範囲の中に一致が
//!   見つからないときは、根拠の範囲の全体を伏せる (安全の側に倒す)。
//! - 伏せ字にしないで忘れた事実: 忘れるの指示の `hidden` の範囲。指示は書き換えないので、その指示より後の忘れるの
//!   伏せ字で今の本文の位置に写す (04 §7.3 の「位置の写し直し」、[`TurnForgets`])。
//!
//! 本文は保存したまま変えない。置き換えは、添える・帯に出す・検証のプロンプトに入れる直前に当てる。

use std::collections::BTreeSet;
use std::ops::Range;

use evorag_store::{Read, ReadTxn};
use evorag_types::Id;

use crate::apply::{DirectiveSet, TurnRedactions, locate_keys};
use crate::directive::{DirectiveBody, DirectiveOp, KIND_DIRECTIVE, TurnSpan};
use crate::value_key::{ForgetKey, ValueKey};

use crate::facts::load_facts;
use crate::fold::{FactSet, FactState, Source};
use crate::forget::TurnForgets;
use crate::read::MemoryError;
use crate::value_key::{Edges, Haystack, Needle, Placed, Widen};

/// `[非表示]` の印 (プロンプトの言語で選ぶ、04 §7.5)。
#[must_use]
pub fn hidden_marker(language: &str) -> &'static str {
    if language == "ja" {
        "[非表示]"
    } else {
        "[hidden]"
    }
}

// 伏せ字の印は、v0.5 でも残る保存の層の忘れるの核に置く (旧エンジンを外しても `import-v04` が使えるように)。
pub use evorag_store::forget::{REDACTION_MARKERS, redaction_marker};

/// 往復の中のターン 1 つ (保存した本文と、その改訂の番号)。
#[derive(Debug, Clone, Copy)]
pub struct TurnText<'a> {
    pub id: Id,
    pub rev: u32,
    pub text: &'a str,
}

/// private の値 1 つ (正規化 v1 の形と照合の形と、出典)。
#[derive(Debug, Clone)]
struct PrivateValue {
    forms: Needle,
    sources: Vec<Source>,
}

/// v0.5 の根の、解かれていない `private` の指示 (`directive`)。指示は値の文字列を持たないので、`vks` に当たる部分
/// 文字列と、根拠の位置 (`at`) で伏せる (08 §2.6 の M5.1〜M5.3 の扱い。M5.4 で新しい当て方に替える)。
#[derive(Debug, Clone)]
struct DirectivePrivates {
    key: ForgetKey,
    vks: BTreeSet<ValueKey>,
    spans: Vec<TurnSpan>,
    redactions: TurnRedactions,
}

impl DirectivePrivates {
    /// 記録から読む。`private` の指示が無ければ `None`。あるのに鍵が使えなければ、伏せる値を確かめられないので
    /// 失敗する。
    fn load(tx: &ReadTxn<'_>, redaction_marker: &str) -> Result<Option<Self>, MemoryError> {
        let mut all: Vec<(Id, DirectiveBody)> = Vec::new();
        tx.for_each(KIND_DIRECTIVE, |r| {
            if let Read::Ok(e) = r
                && let Ok(b) = e.body_as::<DirectiveBody>()
            {
                all.push((e.id, b));
            }
            true
        })?;
        if !all.iter().any(|(_, b)| b.op() == DirectiveOp::Private) {
            return Ok(None);
        }
        let set = DirectiveSet::new(all, redaction_marker, str::to_owned);
        if set.privates().is_empty() {
            return Ok(None);
        }
        let key = tx
            .meta(evorag_store::meta::FORGET_KEY)?
            .and_then(|h| ForgetKey::from_hex(&h))
            .ok_or(MemoryError::KeyUnavailable)?;
        let vks = set
            .privates()
            .iter()
            .flat_map(|p| p.vks().iter().copied())
            .collect();
        let spans = set
            .privates()
            .iter()
            .filter_map(|p| p.at().copied())
            .collect();
        Ok(Some(Self {
            key,
            vks,
            spans,
            redactions: set.redactions().clone(),
        }))
    }

    /// ターン `t` の伏せる範囲: `vks` に当たる部分文字列と、根拠の位置を今の本文に写したもの (写せなければ、安全の
    /// 側に倒して本文の全体)。
    fn ranges(&self, t: &TurnText<'_>) -> Vec<Range<usize>> {
        let mut out = locate_keys(&self.key, &self.vks, t.text);
        for at in self.spans.iter().filter(|s| s.turn == t.id) {
            let span = usize::try_from(at.start).unwrap_or(usize::MAX)
                ..usize::try_from(at.end).unwrap_or(usize::MAX);
            match self.redactions.remap(t.id, at.rev, span, t.rev) {
                Some(r) => out.push(widen(t.text, r)),
                None => out.push(0..t.text.len()),
            }
        }
        out
    }
}

/// 伏せる材料 (private の値と、伏せ字にしないで忘れた範囲)。
#[derive(Debug, Clone, Default)]
pub struct Masker {
    private: Vec<PrivateValue>,
    /// v0.5 の根の private の指示 (v0.4 の根では `None`)。
    directive_privates: Option<DirectivePrivates>,
    forgets: TurnForgets,
    /// 伏せ字の印 (`[非表示]` の範囲を、後の伏せ字で写すのに使う)。
    redaction_marker: String,
}

impl Masker {
    /// private の事実 (`Retracted` でなく値のあるもの) と、忘れるの指示 (`hidden` の範囲と、それを写す後の伏せ字)
    /// から作る。`redaction_marker` はデータ根の伏せ字の印 ([`redaction_marker`])。
    #[must_use]
    pub fn new(set: &FactSet, forgets: TurnForgets, redaction_marker: &str) -> Self {
        let private = set
            .facts
            .iter()
            .filter(|f| f.private && f.state != FactState::Retracted)
            .filter_map(|f| {
                let forms = Needle::new(f.value.as_deref()?);
                (!forms.is_empty()).then(|| PrivateValue {
                    forms,
                    sources: f.sources.iter().copied().collect(),
                })
            })
            .collect();
        Self {
            private,
            directive_privates: None,
            forgets,
            redaction_marker: redaction_marker.to_owned(),
        }
    }

    /// 記録から作る (事実と、忘れるの指示)。`redaction_marker` はデータ根の伏せ字の印。
    ///
    /// # Errors
    /// 記録を読めないとき。
    /// v0.5 の根では、解かれていない `private` の指示 (`directive`) も読む ([`DirectivePrivates`])。
    pub fn load(tx: &ReadTxn<'_>, redaction_marker: &str) -> Result<Self, MemoryError> {
        let mut m = Self::new(
            &load_facts(tx)?.set,
            TurnForgets::load(tx)?,
            redaction_marker,
        );
        m.directive_privates = DirectivePrivates::load(tx, redaction_marker)?;
        Ok(m)
    }

    /// 伏せるものが無い。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.private.is_empty() && self.directive_privates.is_none() && self.forgets.is_empty()
    }

    /// ターン `t` の、伏せ字にしないで忘れた `[非表示]` の範囲 (今の本文の位置)。忘れるの指示と合わないターン
    /// (04 §7.2 の「合わないターン」) は `None`。
    #[must_use]
    pub fn hidden_ranges(&self, t: &TurnText<'_>) -> Option<Vec<Range<usize>>> {
        self.forgets
            .hidden_ranges(t.id, t.text, t.rev, &self.redaction_marker)
    }

    /// 忘れるの指示と合わないターンか。呼び手は、そのターンを想起・候補の帯・裏の LLM の呼び出し・書き出しから除く
    /// (04 §7.2)。[`Self::mask`] と [`Self::mask_hidden`] は、念のため本文の全体を伏せる。
    #[must_use]
    pub fn excluded(&self, t: &TurnText<'_>) -> bool {
        self.hidden_ranges(t).is_none()
    }

    /// 合わないターンなら本文の全体、そうでなければ `[非表示]` の範囲。
    fn hidden_or_whole(&self, t: &TurnText<'_>) -> Vec<Range<usize>> {
        self.hidden_ranges(t)
            .unwrap_or_else(|| std::iter::once(0..t.text.len()).collect())
    }

    /// 往復の各ターンの本文に、伏せ字にしないで忘れた範囲だけの置き換えを当てたもの (`exchange` の順)。ノートの
    /// 鍵を作るときに使う (04 §7.1: `[非表示]` の範囲を持つ往復のノートは、伏せた本文から作り直す)。
    #[must_use]
    pub fn mask_hidden(&self, exchange: &[TurnText<'_>], marker: &str) -> Vec<String> {
        exchange
            .iter()
            .map(|t| apply_ranges(t.text, &self.hidden_or_whole(t), marker))
            .collect()
    }

    /// 往復 `exchange` の `i` 番目のターンの、伏せる範囲 (本文のバイト位置。重なりうる)。
    #[must_use]
    pub fn ranges(&self, exchange: &[TurnText<'_>], i: usize) -> Vec<Range<usize>> {
        let Some(t) = exchange.get(i) else {
            return Vec::new();
        };
        let mut out: Vec<Range<usize>> = self
            .hidden_or_whole(t)
            .into_iter()
            .map(|r| widen(t.text, r))
            .collect();
        if let Some(p) = &self.directive_privates {
            out.extend(p.ranges(t));
        }
        let current = |s: &Source| exchange.iter().any(|x| x.id == s.turn && x.rev == s.rev);
        let hay = Haystack::new(t.text);
        for pv in &self.private {
            if !pv.sources.iter().any(current) {
                continue;
            }
            let hits = value_hits(&hay, &pv.forms);
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

/// 本文 `hay` の中で、値 `forms` に一致する範囲 (元の本文の位置)。正規化 v1 の一致を先に採り、照合の形 (04 §5.6)
/// では、それと重ならない一致を足す。文字の展開の途中に端が来る一致は、その書記素まで広げる。位置の対応表を
/// 作れない本文で、正規化した本文に値が含まれるときは、位置を戻せないので本文の全体を返す (伏せる側に倒す)。
fn value_hits(hay: &Haystack<'_>, forms: &Needle) -> Vec<Range<usize>> {
    match hay.find_all(forms, Edges::Word, Widen::ToGraphemes) {
        Placed::Ranges(r) => r,
        // 照合の形の一致の位置を戻せないときは、正規化 v1 の一致を含む本文の全体を伏せる。
        Placed::Partial(_) | Placed::Unplaceable => std::iter::once(0..hay.text().len()).collect(),
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
    use crate::import_v04::HiddenBody;
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
        let m = Masker::new(&set, TurnForgets::default(), "[伏せ字]");
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
        let m = Masker::new(&set, TurnForgets::default(), "[伏せ字]");
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
        let m = Masker::new(&set, TurnForgets::from_hidden(hidden), "[伏せ字]");
        let t = [TurnText {
            id: id(1),
            rev: 0,
            text,
        }];
        assert_eq!(m.mask(&t, "[非表示]"), ["[非表示]ペットは[非表示]元気"]);
    }

    /// 一致の端が文字の展開の途中に来るとき (`㈱` の中の `株`) は、その書記素を伏せる。位置の対応表を作れない
    /// 本文で値が含まれるときは、本文の全体を伏せる (伏せる側に倒す)。
    #[test]
    fn matches_inside_an_expansion_or_without_a_map_still_hide() {
        let text = "㈱見本の社員です";
        let set = FactSet {
            facts: vec![private_fact("株", id(1), 0, 0..3)],
        };
        let m = Masker::new(&set, TurnForgets::default(), "[伏せ字]");
        let t = [TurnText {
            id: id(1),
            rev: 0,
            text,
        }];
        assert_eq!(m.mask(&t, "[非表示]"), ["[非表示]見本の社員です"]);
        // 照合の形の一致も伏せる (空白を挟んだ写し)。正規化 v1 の一致と重なる一致は足さない。
        let spaced = "誕生日は7 月 22 日、7月22日です";
        let hits = value_hits(&Haystack::new(spaced), &Needle::new("7月22日"));
        let got: Vec<&str> = hits.iter().map(|r| &spaced[r.clone()]).collect();
        assert_eq!(got, ["7 月 22 日", "7月22日"]);
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
