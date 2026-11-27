//! B2 の要求を組み立てる (04 §5.2・§5.2.1、§6)。I/O と LLM を使わない。
//!
//! 1. [`Layout::new`]: 区切りの往復に前のふるいと文の分け方 v1 を当てて文番号を振り、送りうる行 (文の行・応答の
//!    枠・訂正の候補・見出し) を作る。
//! 2. 呼び手が [`Layout::pieces`] の行ごと (行と改行) にトークンを数え (`/tokenize`。使えなければ見積もり、
//!    06 §8)、[`Layout::plan`] が予算 ([`user_budget`]) に収まる形を決める。
//! 3. [`Layout::render`] が、呼び出しごとのメッセージと JSON Schema と、台帳に書く入力 (呼び出しごとの
//!    `sentences`・`corr`・`refs`) を作る。組み立てた利用者のメッセージを数えられるなら、[`Layout::refit`] で
//!    最後に確かめる。

use std::fmt::Write as _;
use std::ops::Range;

use evorag_types::{Id, PromptLanguage};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::prompt::{self, Shape, Words};
use super::{
    KEY_MAX_CHARS, KEYS_MAX, MAX_TOKENS, PREVIOUS_CHARS, REPLY_CHARS, REPLY_SHORT_CHARS,
    REQUEST_TOKENS, UNITS_MAX,
};
use crate::ledger::{
    ByteRange, CorrInput, LedgerInputs, LedgerRef, SentenceSpan, Through, UnknownFields,
};
use crate::prefilter::{MODE_CHAT, PREFILTER_V1, Prefiltered, Screened, prefilter};
use crate::sentences::{SPLITTER_V1, split_v1};

/// 要求の予算から引く余白 (トークン、04 §5.2.1 の「数え方」)。チャットのテンプレートのぶん。
pub const BUDGET_MARGIN: usize = 64;

/// トークンの数え方 (06 §8)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counting {
    /// `/tokenize` で数えた。
    Exact,
    /// `/tokenize` が使えず、文字数から見積もった (1 つでも見積もりなら、こちら)。
    Estimated,
}

/// 利用者のメッセージに使える予算 (04 §5.2.1 の「数え方」): 要求の上限 − システムのメッセージ − 余白。見積もりで
/// 数えたときは 2 割減らす (06 §8)。
#[must_use]
pub fn user_budget(system_tokens: usize, counting: Counting) -> usize {
    let budget = REQUEST_TOKENS
        .saturating_sub(system_tokens)
        .saturating_sub(BUDGET_MARGIN);
    match counting {
        Counting::Exact => budget,
        Counting::Estimated => budget * 4 / 5,
    }
}

/// B2 の要求 (プロンプトの上限と生成の上限) が、スロットの文脈の長さ `n_ctx` に収まるか (04 §5.2.1)。収まらない
/// 構成では呼ばない。
#[must_use]
pub fn fits(n_ctx: usize) -> bool {
    REQUEST_TOKENS + MAX_TOKENS as usize <= n_ctx
}

/// ターンの本文 1 つ (保存した本文と改訂の番号と、忘れるの `[非表示]` の範囲 (保存した本文のバイト位置))。
#[derive(Debug, Clone, Copy)]
pub struct Text<'a> {
    pub turn: Id,
    pub rev: u32,
    pub text: &'a str,
    pub hidden: &'a [Range<usize>],
}

/// 区切りの往復 1 つ (利用者の発話と、組にする最新の完了した応答、04 §2)。
#[derive(Debug, Clone, Copy)]
pub struct Pair<'a> {
    pub utterance: Text<'a>,
    pub reply: Option<Text<'a>>,
}

/// 字句の規則で見つけた訂正の候補 1 つ (04 §6)。範囲は候補の発話の保存した本文のバイト位置。
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    /// 候補の発話のターン (区切りの往復の発話のどれか)。
    pub turn: Id,
    /// 当たった訂正の字句の型。
    pub pattern: &'a str,
    /// `{new}` の穴の範囲。
    pub new: Range<usize>,
    /// `{old}` の穴の範囲 (古い値の穴の無い型は `None`)。
    pub old: Option<Range<usize>>,
    /// 候補の発話の直前の応答 (同じセッションの、候補の発話より前の最後の応答。無ければ `None`)。
    pub previous: Option<Text<'a>>,
}

/// 要求を組み立てる入力。
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    /// データ根のプロンプトの言語 (07 §6.1)。
    pub language: PromptLanguage,
    /// 区切りの往復 (古い順)。区切りの全体を渡す (`through` になる)。
    pub pairs: &'a [Pair<'a>],
    /// 訂正の候補 (どの順でもよい)。
    pub candidates: &'a [Candidate<'a>],
    /// 候補を見つけた訂正の字句の型の版 (台帳の `inputs` の `correction`)。
    pub correction_version: u32,
}

/// 入力が組み立てに使えない理由。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    #[error("the segment has no pairs")]
    NoPairs,
    /// 範囲が本文の外か、文字の境界に無い。
    #[error("a range of turn {0} is outside the text or not on a character boundary")]
    BadRange(Id),
    /// 候補のターンが、区切りの往復の発話に無い。
    #[error("candidate turn {0} is not an utterance of the segment")]
    CandidateTurn(Id),
    /// 位置か数が台帳の形 (u32) に入らない。
    #[error("a position or count of turn {0} does not fit in u32")]
    TooLong(Id),
}

/// [`Layout::plan`] に渡した数の並びが、[`Layout::pieces`] と合わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("got {got} token counts for {want} pieces")]
pub struct CountMismatch {
    pub want: usize,
    pub got: usize,
}

#[derive(Debug, Clone)]
struct SentenceLayout {
    /// 文番号 (区切りの中で 1 から通し)。
    number: u32,
    /// 保存した本文の範囲 (`Layout::new` で u32 に入ることを確かめた)。
    start: u32,
    end: u32,
    piece: usize,
}

impl SentenceLayout {
    fn contains(&self, at: usize) -> bool {
        u32::try_from(at).is_ok_and(|a| self.start <= a && a < self.end)
    }
}

#[derive(Debug, Clone)]
struct ReplyLayout {
    turn: Id,
    rev: u32,
    /// 頭の 500 文字と 200 文字の枠。
    full: usize,
    short: usize,
}

#[derive(Debug, Clone)]
struct PairLayout {
    turn: Id,
    rev: u32,
    sentences: Vec<SentenceLayout>,
    reply: Option<ReplyLayout>,
}

#[derive(Debug, Clone)]
struct CandidateLayout {
    pair: usize,
    /// 候補の `{new}` の穴の始まりを含む文の、`PairLayout::sentences` の位置。
    sentence: usize,
    corr: CorrInput,
    previous: Option<(Id, u32)>,
    /// 直前の応答を 800 文字にした行と、半分にした行。
    full: usize,
    half: usize,
}

/// 区切りの往復に文番号を振り、送りうる行を作ったもの ([`Layout::new`])。
#[derive(Debug, Clone)]
pub struct Layout {
    language: PromptLanguage,
    pairs: Vec<PairLayout>,
    candidates: Vec<CandidateLayout>,
    /// 送りうる行 (どれも改行で終わる)。
    pieces: Vec<String>,
    /// 訂正の候補の見出し (空行と「訂正の候補:」) の行。
    header: usize,
    through: Through,
    correction_version: u32,
}

/// 範囲が本文の中にあり、文字の境界に乗っているか。
fn valid_range(text: &str, r: &Range<usize>) -> bool {
    r.start <= r.end
        && r.end <= text.len()
        && text.is_char_boundary(r.start)
        && text.is_char_boundary(r.end)
}

/// `[非表示]` の範囲を確かめ、並べて重なりと接するものをまとめる。
fn merged_hidden(t: &Text<'_>) -> Result<Vec<Range<usize>>, InputError> {
    let mut v: Vec<Range<usize>> = Vec::with_capacity(t.hidden.len());
    for r in t.hidden {
        if !valid_range(t.text, r) {
            return Err(InputError::BadRange(t.turn));
        }
        if !r.is_empty() {
            v.push(r.clone());
        }
    }
    v.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    Ok(out)
}

fn to_u32(turn: Id, n: usize) -> Result<u32, InputError> {
    u32::try_from(n).map_err(|_| InputError::TooLong(turn))
}

fn byte_range(turn: Id, r: &Range<usize>) -> Result<ByteRange, InputError> {
    Ok(ByteRange {
        start: to_u32(turn, r.start)?,
        end: to_u32(turn, r.end)?,
    })
}

/// 文字の位置 `n` のバイト位置 (本文が短ければ終わり)。
fn char_pos(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// 前のふるいの後の本文を文に分ける: 文の分け方 v1 の文を、除いた行の境 (保存した本文の続いた片の境) でさらに
/// 分ける (片をまたぐ文は保存した本文に戻せないため)。範囲は保存した本文の位置。
fn numbered_ranges(p: &Prefiltered) -> Vec<Range<usize>> {
    let text = p.text();
    let cuts: Vec<usize> = p.piece_starts().collect();
    let mut out = Vec::new();
    for r in split_v1(text) {
        let mut start = r.start;
        let inner = cuts.iter().copied().filter(|&c| c > r.start && c < r.end);
        for end in inner.chain(std::iter::once(r.end)) {
            let s = &text[start..end];
            let lead = s.len() - s.trim_start().len();
            let trail = s.len() - s.trim_end().len();
            if lead + trail < s.len()
                && let Some(stored) = p.to_stored(start + lead..end - trail)
            {
                out.push(stored);
            }
            start = end;
        }
    }
    out
}

/// 行を足し、その位置を返す (行は改行で終える。予算は「行 + 改行」で数える)。
fn push(pieces: &mut Vec<String>, mut text: String) -> usize {
    text.push('\n');
    pieces.push(text);
    pieces.len() - 1
}

impl Layout {
    /// 文番号を振り、送りうる行を作る。
    ///
    /// - 前のふるい (04 §5.2) で送る発話の、前のふるいの後の本文を文に分けて、区切りの中で 1 から番号を振る。
    ///   送らない発話 (挨拶・相槌・短い) でも、訂正の候補があれば番号を振る。
    /// - 応答は頭の 500 文字 (縮めた形は 200 文字) を `[参照のみ]` の枠に入れる (番号を振らない)。
    /// - 訂正の候補は、`{new}` の穴の始まりを含む文の番号で示し、直前の応答の末尾の 800 文字を枠に入れる。
    ///   番号を振った文に穴の無い候補は送らない。
    /// - 枠に入れる本文と候補の穴の文字列は、NFKC で `[`・`]` になる文字を `〔`・`〕` に替える。
    ///
    /// # Errors
    /// 往復が無いとき、範囲が本文の外か文字の境界に無いとき、候補のターンが区切りの発話に無いとき、位置が u32 に
    /// 入らないとき。
    pub fn new(input: &Input<'_>) -> Result<Self, InputError> {
        let last = input.pairs.last().ok_or(InputError::NoPairs)?;
        let w = prompt::words(input.language);
        for c in input.candidates {
            let Some(pair) = input.pairs.iter().find(|p| p.utterance.turn == c.turn) else {
                return Err(InputError::CandidateTurn(c.turn));
            };
            let text = pair.utterance.text;
            if !valid_range(text, &c.new) || c.old.as_ref().is_some_and(|o| !valid_range(text, o)) {
                return Err(InputError::BadRange(c.turn));
            }
        }
        let mut pieces: Vec<String> = Vec::new();
        let mut pairs: Vec<PairLayout> = Vec::with_capacity(input.pairs.len());
        let mut hidden_of: Vec<Vec<Range<usize>>> = Vec::with_capacity(input.pairs.len());
        let mut number = 0u32;
        for pair in input.pairs {
            let u = &pair.utterance;
            let hidden = merged_hidden(u)?;
            let has_candidate = input.candidates.iter().any(|c| c.turn == u.turn);
            let filtered = match prefilter(MODE_CHAT, u.text) {
                Screened::Send(p) => Some(p),
                Screened::Skip(_) if has_candidate => {
                    Some(Prefiltered::from_stored(u.text)).filter(|p| !p.text().trim().is_empty())
                }
                Screened::Skip(_) => None,
            };
            let mut sentences = Vec::new();
            for stored in filtered.as_ref().map(numbered_ranges).unwrap_or_default() {
                number = number.checked_add(1).ok_or(InputError::TooLong(u.turn))?;
                let shown = prompt::shaped(u.text, stored.clone(), &hidden, w.hidden, Shape::Line);
                sentences.push(SentenceLayout {
                    number,
                    start: to_u32(u.turn, stored.start)?,
                    end: to_u32(u.turn, stored.end)?,
                    piece: push(&mut pieces, format!("[{number}] {shown}")),
                });
            }
            let reply = match &pair.reply {
                Some(r) => {
                    let rh = merged_hidden(r)?;
                    let mut head = |chars: usize| {
                        let range = 0..char_pos(r.text, chars);
                        let body = prompt::shaped(r.text, range, &rh, w.hidden, Shape::Frame);
                        push(&mut pieces, prompt::frame(w, None, &body))
                    };
                    let full = head(REPLY_CHARS);
                    let short = head(REPLY_SHORT_CHARS);
                    Some(ReplyLayout {
                        turn: r.turn,
                        rev: r.rev,
                        full,
                        short,
                    })
                }
                None => None,
            };
            hidden_of.push(hidden);
            pairs.push(PairLayout {
                turn: u.turn,
                rev: u.rev,
                sentences,
                reply,
            });
        }
        let candidates = Self::lay_candidates(input, &pairs, &hidden_of, w, &mut pieces)?;
        let header = push(&mut pieces, format!("\n{}", w.candidates));
        Ok(Self {
            language: input.language,
            pairs,
            candidates,
            pieces,
            header,
            through: Through {
                turn: last.utterance.turn,
                pairs: to_u32(last.utterance.turn, input.pairs.len())?,
            },
            correction_version: input.correction_version,
        })
    }

    /// 訂正の候補の行を作る。番号は、往復の順・`{new}` の穴・`{old}` の穴・型の順に 1 から振る (入力の順に
    /// 依らない)。
    fn lay_candidates(
        input: &Input<'_>,
        pairs: &[PairLayout],
        hidden_of: &[Vec<Range<usize>>],
        w: &Words,
        pieces: &mut Vec<String>,
    ) -> Result<Vec<CandidateLayout>, InputError> {
        let mut order: Vec<(usize, &Candidate<'_>)> = input
            .candidates
            .iter()
            .filter_map(|c| {
                let i = pairs.iter().position(|p| p.turn == c.turn)?;
                Some((i, c))
            })
            .collect();
        order.sort_by(|(i, a), (j, b)| {
            let key = |c: &Candidate<'_>| {
                (
                    c.new.start,
                    c.new.end,
                    c.old.as_ref().map(|o| (o.start, o.end)),
                )
            };
            (i, key(a), a.pattern).cmp(&(j, key(b), b.pattern))
        });
        let mut out = Vec::new();
        let mut id = 0u32;
        for (i, c) in order {
            let pair = &pairs[i];
            let Some(sentence) = pair.sentences.iter().position(|s| s.contains(c.new.start)) else {
                continue;
            };
            id += 1;
            let text = input.pairs[i].utterance.text;
            let hidden = &hidden_of[i];
            let hole = |r: &Range<usize>| {
                prompt::shaped(text, r.clone(), hidden, w.hidden, Shape::UntrustedLine)
            };
            let pattern = prompt::shaped(
                c.pattern,
                0..c.pattern.len(),
                &[],
                w.hidden,
                Shape::UntrustedLine,
            );
            let line = prompt::candidate_line(
                input.language,
                id,
                pair.sentences[sentence].number,
                &pattern,
                c.old.as_ref().map(hole).as_deref(),
                &hole(&c.new),
            );
            let (full, half) = match &c.previous {
                Some(p) => {
                    let ph = merged_hidden(p)?;
                    let tail = |max: usize| {
                        let n = p.text.chars().count();
                        let start = char_pos(p.text, n.saturating_sub(max));
                        let body = prompt::shaped(
                            p.text,
                            start..p.text.len(),
                            &ph,
                            w.hidden,
                            Shape::Frame,
                        );
                        prompt::frame(w, Some(w.previous), &body)
                    };
                    (tail(PREVIOUS_CHARS), tail(PREVIOUS_CHARS / 2))
                }
                None => {
                    let none = format!("{} {}", w.previous, w.none);
                    (none.clone(), none)
                }
            };
            out.push(CandidateLayout {
                pair: i,
                sentence,
                corr: CorrInput {
                    id,
                    turn: c.turn,
                    rev: pair.rev,
                    pattern: c.pattern.to_owned(),
                    new: byte_range(c.turn, &c.new)?,
                    old: c.old.as_ref().map(|o| byte_range(c.turn, o)).transpose()?,
                    extra: UnknownFields::new(),
                },
                previous: c.previous.map(|p| (p.turn, p.rev)),
                full: push(pieces, format!("{line}\n{full}")),
                half: push(pieces, format!("{line}\n{half}")),
            });
        }
        Ok(out)
    }

    /// 番号を振った文が 1 つも無いか (呼ばずに、出力が空の台帳を書く、04 §5.2.1)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.iter().all(|p| p.sentences.is_empty())
    }

    /// 番号を振った文の数。
    #[must_use]
    pub fn sentence_count(&self) -> usize {
        self.pairs.iter().map(|p| p.sentences.len()).sum()
    }

    /// システムのメッセージ (プロンプトの言語ごとに、バイト列まで毎回同じ)。
    #[must_use]
    pub fn system(&self) -> &'static str {
        prompt::system(self.language)
    }

    /// トークンを数える行 (どれも改行で終わる)。この順の数を [`Layout::plan`] に渡す。
    pub fn pieces(&self) -> impl ExactSizeIterator<Item = &str> {
        self.pieces.iter().map(String::as_str)
    }

    /// 行のトークンの数 `counts` ([`Layout::pieces`] の順) と、利用者のメッセージの予算 `budget`
    /// ([`user_budget`]) から、呼び出しの形を決める (04 §5.2.1 の「予算での分け方」)。
    ///
    /// 1. 区切りの全体が予算に入れば 1 回の呼び出しにする。入らなければ、すべての往復の応答の枠を 200 文字に
    ///    縮め、それでも入らなければ応答の枠を外す。
    /// 2. それでも入らなければ (応答の枠を外したまま)、往復を古い順に詰め、予算を超える往復の前で次の呼び出しに
    ///    分ける (区切りの境界は変えない)。
    /// 3. 1 つの往復だけで予算を超えるときは、直前の応答を半分 (400 文字) にし、それでも超えれば文を前から予算に
    ///    入るだけ送る。残りの文は送らずに数える ([`Plan::unsent`])。
    /// 4. 番号を振った文の無い呼び出しは作らない (その応答の枠は送らない)。
    ///
    /// # Errors
    /// `counts` の数が行の数と合わないとき。
    pub fn plan(&self, counts: &[usize], budget: usize) -> Result<Plan, CountMismatch> {
        self.check_counts(counts)?;
        let mut plan = Plan::default();
        for part in self.plan_range(0..self.pairs.len(), counts, budget) {
            plan.push(part);
        }
        Ok(plan)
    }

    fn check_counts(&self, counts: &[usize]) -> Result<(), CountMismatch> {
        if counts.len() == self.pieces.len() {
            Ok(())
        } else {
            Err(CountMismatch {
                want: self.pieces.len(),
                got: counts.len(),
            })
        }
    }

    /// 組み立てた呼び出し `index` の利用者のメッセージを数えた数 `measured` が予算を超えていれば、その呼び出しの
    /// 往復を、超えた分だけ減らした予算で決め直した計画と、置き換えた呼び出しの位置の範囲を返す (超えていなければ
    /// `None`)。予算は呼ぶたびに小さくなるので、`None` になるまで繰り返してよい。
    ///
    /// # Errors
    /// `counts` の数が行の数と合わないとき。
    pub fn refit(
        &self,
        plan: &Plan,
        index: usize,
        counts: &[usize],
        budget: usize,
        measured: usize,
    ) -> Result<Option<(Plan, Range<usize>)>, CountMismatch> {
        self.check_counts(counts)?;
        let Some(part) = plan.parts.get(index) else {
            return Ok(None);
        };
        if measured <= budget {
            return Ok(None);
        }
        let reduced = budget.saturating_sub(measured - budget);
        let parts = self.plan_range(part.pairs.clone(), counts, reduced);
        Ok(Some(plan.replace(index, parts)))
    }

    /// 呼び出し 1 つの行の数の和。
    fn cost(&self, part: &Part, counts: &[usize]) -> usize {
        let sentences: usize = part
            .sentences
            .iter()
            .map(|&(i, k)| counts[self.pairs[i].sentences[k].piece])
            .sum();
        let replies: usize = part
            .replies
            .iter()
            .filter_map(|&(i, cut)| {
                self.pairs[i].reply.as_ref().map(|r| match cut {
                    ReplyCut::Full => counts[r.full],
                    ReplyCut::Short => counts[r.short],
                })
            })
            .sum();
        let candidates: usize = part
            .candidates
            .iter()
            .map(|&(c, cut)| match cut {
                Cut::Full => counts[self.candidates[c].full],
                Cut::Half => counts[self.candidates[c].half],
            })
            .sum();
        let header = if part.candidates.is_empty() {
            0
        } else {
            counts[self.header]
        };
        sentences + replies + candidates + header
    }

    /// 往復 `i` の全部 (応答は `reply` の長さ、候補は直前の応答を 800 文字) を `part` に足す。
    fn add_pair(&self, part: &mut Part, i: usize, reply: Option<ReplyCut>) {
        if part.pairs.is_empty() {
            part.pairs = i..i + 1;
        } else {
            part.pairs.end = i + 1;
        }
        let p = &self.pairs[i];
        part.sentences
            .extend((0..p.sentences.len()).map(|k| (i, k)));
        if let (Some(cut), Some(_)) = (reply, &p.reply) {
            part.replies.push((i, cut));
        }
        part.candidates.extend(
            (0..self.candidates.len())
                .filter(|&c| self.candidates[c].pair == i)
                .map(|c| (c, Cut::Full)),
        );
    }

    /// 往復の範囲 `range` を予算 `budget` に収める呼び出しの並び ([`Layout::plan`] の手順)。
    fn plan_range(&self, range: Range<usize>, counts: &[usize], budget: usize) -> Vec<Part> {
        for reply in [Some(ReplyCut::Full), Some(ReplyCut::Short), None] {
            let mut whole = Part::default();
            for i in range.clone() {
                self.add_pair(&mut whole, i, reply);
            }
            if self.cost(&whole, counts) <= budget {
                return vec![whole];
            }
        }
        let mut out = Vec::new();
        let mut cur = Part::default();
        for i in range {
            let mut one = Part::default();
            self.add_pair(&mut one, i, None);
            if self.cost(&one, counts) > budget {
                out.push(std::mem::take(&mut cur));
                out.push(self.squeeze(i, counts, budget));
                continue;
            }
            let mut grown = cur.clone();
            self.add_pair(&mut grown, i, None);
            if self.cost(&grown, counts) <= budget {
                cur = grown;
            } else {
                out.push(std::mem::replace(&mut cur, one));
            }
        }
        out.push(cur);
        out
    }

    /// 1 つで予算を超える往復を、予算に入る形にする (応答の枠は外したまま)。
    fn squeeze(&self, i: usize, counts: &[usize], budget: usize) -> Part {
        let p = &self.pairs[i];
        let mine = |k: usize| -> Vec<(usize, Cut)> {
            (0..self.candidates.len())
                .filter(|&c| self.candidates[c].pair == i && self.candidates[c].sentence == k)
                .map(|c| (c, Cut::Half))
                .collect()
        };
        let mut part = Part {
            pairs: i..i + 1,
            ..Part::default()
        };
        for k in 0..p.sentences.len() {
            let mut grown = part.clone();
            grown.sentences.push((i, k));
            grown.candidates.extend(mine(k));
            if self.cost(&grown, counts) > budget {
                // 文番号は往復の中で 1 ずつ増えるので、残りの文の数は番号の差で求まる (u32 に入る)。
                let last = p.sentences.last().map_or(0, |l| l.number);
                part.unsent = last - p.sentences[k].number + 1;
                break;
            }
            part = grown;
        }
        part
    }

    /// 呼び出しのメッセージ・JSON Schema と、呼び出しごとの台帳の入力 (`sentences`・`corr`・`refs`) を作る。
    #[must_use]
    pub fn render(&self, plan: &Plan) -> Call {
        let requests = plan.parts.iter().map(|p| self.render_part(p)).collect();
        Call {
            requests,
            through: self.through,
            correction_version: self.correction_version,
        }
    }

    fn render_part(&self, part: &Part) -> Request {
        let mut user = String::new();
        let mut sentences = Vec::new();
        let mut refs: Vec<LedgerRef> = Vec::new();
        let mut corr = Vec::new();
        for i in part.pairs.clone() {
            let p = &self.pairs[i];
            for &(_, k) in part.sentences.iter().filter(|(pi, _)| *pi == i) {
                let s = &p.sentences[k];
                user.push_str(&self.pieces[s.piece]);
                sentences.push(SentenceSpan::new(s.number, p.turn, p.rev, s.start, s.end));
            }
            if let (Some(r), Some(&(_, cut))) = (
                p.reply.as_ref(),
                part.replies.iter().find(|(pi, _)| *pi == i),
            ) {
                user.push_str(
                    &self.pieces[match cut {
                        ReplyCut::Full => r.full,
                        ReplyCut::Short => r.short,
                    }],
                );
                add_ref(&mut refs, r.turn, r.rev);
            }
        }
        if !part.candidates.is_empty() {
            user.push_str(&self.pieces[self.header]);
        }
        for &(c, cut) in &part.candidates {
            let cand = &self.candidates[c];
            user.push_str(
                &self.pieces[match cut {
                    Cut::Full => cand.full,
                    Cut::Half => cand.half,
                }],
            );
            if let Some((turn, rev)) = cand.previous {
                add_ref(&mut refs, turn, rev);
            }
            corr.push(cand.corr.clone());
        }
        if user.ends_with('\n') {
            user.pop();
        }
        let numbers: Vec<u32> = sentences.iter().map(|s| s.s).collect();
        let ids: Vec<u32> = corr.iter().map(|c| c.id).collect();
        Request {
            system: self.system(),
            user,
            schema: schema(&numbers, &ids),
            sentences,
            corr,
            refs,
        }
    }
}

/// 送った応答を `refs` に足す (同じターンは 1 つだけ)。
fn add_ref(refs: &mut Vec<LedgerRef>, turn: Id, rev: u32) {
    if !refs.iter().any(|r| r.turn == turn) {
        refs.push(LedgerRef::new(turn, rev));
    }
}

/// 応答の枠の長さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyCut {
    /// 頭の 500 文字。
    Full,
    /// 区切りの全体が予算に入らないので 200 文字にした。
    Short,
}

/// 直前の応答の長さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// 末尾の 800 文字。
    Full,
    /// 1 つの往復が予算に入らないので半分 (400 文字) にした。
    Half,
}

/// 1 回の呼び出しに入れるもの。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Part {
    /// 往復の範囲 ([`Layout`] の往復の位置)。
    pairs: Range<usize>,
    /// 送る文 (往復の位置, 往復の中の文の位置)。
    sentences: Vec<(usize, usize)>,
    /// 応答の枠を送る往復の位置と、枠の長さ。
    replies: Vec<(usize, ReplyCut)>,
    /// 送る訂正の候補と、直前の応答の長さ。
    candidates: Vec<(usize, Cut)>,
    /// 予算に入らずに送らなかった文の数。
    unsent: u32,
}

/// 区切りの呼び出しの形 ([`Layout::plan`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    parts: Vec<Part>,
    /// 番号を振った文が無いので作らなかった呼び出しの、送らなかった文の数。
    lost: u32,
}

impl Plan {
    /// 呼び出しの数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// 呼び出しが無いか (番号を振った文の無い区切りか、1 文も送れなかった区切り)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// 予算に入らずに送らなかった文の数。
    #[must_use]
    pub fn unsent(&self) -> u32 {
        self.parts
            .iter()
            .fold(self.lost, |n, p| n.saturating_add(p.unsent))
    }

    /// 番号を振った文はあるのに、予算に入らず 1 文も送れなかったか (台帳を書かずに数える、04 §5.2.1)。
    #[must_use]
    pub fn nothing_sent(&self) -> bool {
        self.parts.is_empty() && self.lost > 0
    }

    /// 番号を振った文のある呼び出しだけを足す (無ければ、送らなかった文の数だけを残す)。
    fn push(&mut self, part: Part) {
        if part.sentences.is_empty() {
            self.lost = self.lost.saturating_add(part.unsent);
        } else {
            self.parts.push(part);
        }
    }

    /// 呼び出し `index` を `parts` で置き換えた計画と、置き換えた後の位置の範囲。
    fn replace(&self, index: usize, parts: Vec<Part>) -> (Self, Range<usize>) {
        let mut out = Self {
            parts: self.parts[..index].to_vec(),
            lost: self.lost,
        };
        for p in parts {
            out.push(p);
        }
        let range = index..out.parts.len();
        out.parts.extend(self.parts[index + 1..].iter().cloned());
        (out, range)
    }

    /// 出力が上限で切れた (`finish_reason` が `length`) 呼び出し `index` を、往復の境で半分に分けた計画と、置き換えた
    /// 呼び出しの位置の範囲 (04 §5.2.1 の「出力が上限で切れたとき」)。往復が 1 つなら `None` (型が合わないときと
    /// 同じに扱う)。
    #[must_use]
    pub fn halve(&self, index: usize, layout: &Layout) -> Option<(Self, Range<usize>)> {
        let part = self.parts.get(index)?;
        let n = part.pairs.len();
        if n < 2 {
            return None;
        }
        let mid = part.pairs.start + n / 2;
        let halves = [part.pairs.start..mid, mid..part.pairs.end].map(|range| Part {
            sentences: part
                .sentences
                .iter()
                .copied()
                .filter(|(i, _)| range.contains(i))
                .collect(),
            replies: part
                .replies
                .iter()
                .copied()
                .filter(|(i, _)| range.contains(i))
                .collect(),
            candidates: part
                .candidates
                .iter()
                .copied()
                .filter(|(c, _)| range.contains(&layout.candidates[*c].pair))
                .collect(),
            unsent: 0,
            pairs: range,
        });
        Some(self.replace(index, halves.into()))
    }
}

/// 1 回の呼び出し (チャットのメッセージ 2 つと、出力の型縛りの JSON Schema) と、その呼び出しの台帳の入力。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// システムのメッセージ (指示・辞書・必ず書くこと・出力の型の説明)。
    pub system: &'static str,
    /// 利用者のメッセージ (区切りの往復と訂正の候補)。
    pub user: String,
    schema: Value,
    sentences: Vec<SentenceSpan>,
    corr: Vec<CorrInput>,
    refs: Vec<LedgerRef>,
}

impl Request {
    /// 型縛りの JSON Schema の名前。
    pub const SCHEMA_NAME: &'static str = "b2_segment";

    /// 出力の JSON Schema (`s`・`id` の列挙と `corrections` の `maxItems` だけが呼び出しごとに変わる、06 §6)。
    #[must_use]
    pub fn schema(&self) -> &Value {
        &self.schema
    }

    /// llama-server に送る `response_format` (06 §6)。
    #[must_use]
    pub fn response_format(&self) -> Value {
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": Self::SCHEMA_NAME,
                "strict": true,
                "schema": self.schema.clone(),
            }
        })
    }

    /// この呼び出しで送った文 (台帳の `sentences` の要素、文番号の順)。
    #[must_use]
    pub fn sentences(&self) -> &[SentenceSpan] {
        &self.sentences
    }

    /// この呼び出しで送った文番号 (昇順)。
    pub fn sentence_numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.sentences.iter().map(|s| s.s)
    }

    /// この呼び出しで送った訂正の候補 (台帳の `inputs` の `corr` の要素)。
    #[must_use]
    pub fn corr(&self) -> &[CorrInput] {
        &self.corr
    }

    /// この呼び出しで送った応答 (台帳の `inputs` の `refs` の要素)。
    #[must_use]
    pub fn refs(&self) -> &[LedgerRef] {
        &self.refs
    }
}

/// 出力の JSON Schema (04 §5.2 の「出力の型」の書くときの型。すべての項目が必須で、上限は `maxItems` で縛る)。
/// `a` には長さを縛らない (文法で切り詰めた属性が関門を通らないように、40 文字を超える属性は関門で捨てる)。
fn schema(sentences: &[u32], candidates: &[u32]) -> Value {
    let id = if candidates.is_empty() {
        json!({"type": "integer"})
    } else {
        json!({"type": "integer", "enum": candidates})
    };
    json!({
        "type": "object",
        "properties": {
            "keys": {
                "type": "array",
                "items": {"type": "string", "maxLength": KEY_MAX_CHARS},
                "maxItems": KEYS_MAX
            },
            "units": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "s": {"type": "integer", "enum": sentences},
                        "l": {"type": "string", "enum": ["world", "episode"]},
                        "e": {"type": "string"},
                        "a": {"type": "string"},
                        "v": {"type": "string"},
                        "m": {"type": "string", "enum": ["fact", "negated", "question", "hypothetical"]},
                        "tn": {"type": "string", "enum": ["current", "past", "planned"]},
                        "old": {"type": "string"},
                        "when": {"type": "string"}
                    },
                    "required": ["s", "l", "e", "a", "v", "m", "tn", "old", "when"],
                    "additionalProperties": false
                },
                "maxItems": UNITS_MAX
            },
            "corrections": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": id,
                        "verdict": {"type": "string", "enum": ["accept", "reject", "unclear"]}
                    },
                    "required": ["id", "verdict"],
                    "additionalProperties": false
                },
                "maxItems": candidates.len()
            }
        },
        "required": ["keys", "units", "corrections"],
        "additionalProperties": false
    })
}

/// 区切りの呼び出しのすべて ([`Layout::render`])。呼び出しの結果と合わせて台帳を書く ([`super::ledger_body`])。
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// 呼び出し (順に送る)。空なら、番号を振った文の無い区切り (呼ばずに、出力が空の台帳を書く) か、1 文も
    /// 送れなかった区切り ([`Plan::nothing_sent`]。台帳を書かない)。
    pub requests: Vec<Request>,
    /// 台帳の `inputs` の `through` (区切りの最後の往復と往復の数)。
    pub through: Through,
    correction_version: u32,
}

/// 台帳の `inputs` のうち、呼び手が知っているもの (04 §5.2.1)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerInputs {
    /// 入力を読んだときに見えていた最新の忘れるの指示の ID。
    pub forgets_seen: Option<Id>,
    /// 縮退で chat のモデルを使ったか。
    pub role_fallback: bool,
    /// 意図した `memory` の役割の `model_key`。
    pub role_model_key: Option<String>,
}

impl Call {
    /// 呼び出し `used` (位置の昇順) で送ったメッセージのバイト列の SHA-256 (64 桁の小文字の 16 進。04 §5.2.1 の
    /// `input_hash`)。呼び出しの順に、システムと利用者のメッセージを、それぞれ u32be の長さを前に置いて並べた
    /// バイト列のハッシュ。`used` が空なら `None`。
    ///
    /// # Panics
    /// メッセージが 4 GiB 以上のとき (要求は 3,072 トークンまでなので起きない)。
    #[must_use]
    pub fn input_hash(&self, used: &[usize]) -> Option<String> {
        if used.is_empty() {
            return None;
        }
        let mut h = Sha256::new();
        for r in used.iter().filter_map(|&i| self.requests.get(i)) {
            for m in [r.system, r.user.as_str()] {
                let len = u32::try_from(m.len()).expect(
                    "a B2 message is far below 4 GiB (the request is at most 3,072 tokens)",
                );
                h.update(len.to_be_bytes());
                h.update(m.as_bytes());
            }
        }
        let mut out = String::with_capacity(64);
        for b in h.finalize() {
            let _ = write!(out, "{b:02x}");
        }
        Some(out)
    }

    /// 呼び出し `used` (位置の昇順) で送った文 (台帳の `sentences`、文番号の順)。
    #[must_use]
    pub fn sentences_for(&self, used: &[usize]) -> Vec<SentenceSpan> {
        let mut out: Vec<SentenceSpan> = used
            .iter()
            .filter_map(|&i| self.requests.get(i))
            .flat_map(|r| r.sentences.iter().cloned())
            .collect();
        out.sort_by_key(|s| s.s);
        out
    }

    /// 呼び出し `used` (位置の昇順) だけで組み立てた台帳の `inputs` (規則の版・`input_hash`・`through`・
    /// `corr`・`refs` と、呼び手の `caller`)。
    #[must_use]
    pub fn inputs_for(&self, used: &[usize], caller: &CallerInputs) -> LedgerInputs {
        let mut corr = Vec::new();
        let mut refs: Vec<LedgerRef> = Vec::new();
        for r in used.iter().filter_map(|&i| self.requests.get(i)) {
            corr.extend(r.corr.iter().cloned());
            for x in &r.refs {
                add_ref(&mut refs, x.turn, x.rev);
            }
        }
        LedgerInputs {
            prefilter: PREFILTER_V1,
            splitter: SPLITTER_V1,
            correction: self.correction_version,
            forgets_seen: caller.forgets_seen,
            input_hash: self.input_hash(used),
            role_fallback: caller.role_fallback,
            role_model_key: caller.role_model_key.clone(),
            through: Some(self.through),
            corr,
            refs,
            extra: UnknownFields::new(),
        }
    }
}
