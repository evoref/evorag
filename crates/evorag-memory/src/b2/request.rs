//! B2 の要求を組み立てる (04 §5.2・§5.2.1、§6)。I/O と LLM を使わない。
//!
//! 1. [`Layout::new`]: 区切りの往復に前のふるいと文の分け方 v1 を当てて文番号を振り、送りうる行 (文の行・応答の
//!    枠・訂正の候補) を作る。
//! 2. 呼び手が [`Layout::pieces`] の行ごとにトークンを数え (`/tokenize`。使えなければ見積もり、06 §8)、
//!    [`Layout::plan`] が予算 ([`user_budget`]) に収まるように往復の境で呼び出しに分ける。
//! 3. [`Layout::render`] が、呼び出しごとのメッセージと JSON Schema と、台帳に書く `sentences`・`inputs` を作る。

use std::fmt;
use std::ops::Range;

use evorag_types::Id;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::prompt::{self, Words};
use super::{
    ASPECT_MAX_CHARS, KEY_MAX_CHARS, KEYS_MAX, Language, MAX_TOKENS, PREVIOUS_CHARS, REPLY_CHARS,
    REQUEST_TOKENS, UNITS_MAX,
};
use crate::ledger::{ByteRange, CorrInput, LedgerInputs, LedgerRef, SentenceSpan, Through};
use crate::prefilter::{MODE_CHAT, PREFILTER_V1, Prefiltered, Screened, prefilter};
use crate::sentences::{SPLITTER_V1, split_v1};

/// 要求の予算から引く余白 (トークン、04 §5.2.1 の「数え方」)。行をつなぐ改行と、チャットのテンプレートのぶん。
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
    pub language: Language,
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
    /// 位置が台帳の形 (u32) に入らない。
    #[error("a position of turn {0} does not fit in u32")]
    TooLong(Id),
}

/// [`Layout::plan`] に渡した数の並びが、[`Layout::pieces`] と合わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("got {got} token counts for {want} pieces")]
pub struct CountMismatch {
    pub want: usize,
    pub got: usize,
}

/// 送りうる行 1 つ (トークンを数える単位)。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    text: String,
}

#[derive(Debug, Clone)]
struct SentenceLayout {
    /// 文番号 (区切りの中で 1 から通し)。
    number: u32,
    /// 保存した本文の範囲。
    stored: Range<usize>,
    piece: usize,
}

#[derive(Debug, Clone)]
struct ReplyLayout {
    turn: Id,
    rev: u32,
    piece: usize,
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
    id: u32,
    pair: usize,
    /// 候補の `{new}` の穴の始まりを含む文の、`PairLayout::sentences` の位置。
    sentence: usize,
    turn: Id,
    rev: u32,
    pattern: String,
    new: ByteRange,
    old: Option<ByteRange>,
    previous: Option<(Id, u32)>,
    /// 直前の応答を 800 文字にした行と、半分にした行。
    full: usize,
    half: usize,
}

/// 区切りの往復に文番号を振り、送りうる行を作ったもの ([`Layout::new`])。
#[derive(Debug, Clone)]
pub struct Layout {
    language: Language,
    pairs: Vec<PairLayout>,
    candidates: Vec<CandidateLayout>,
    pieces: Vec<Piece>,
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

impl Layout {
    /// 文番号を振り、送りうる行を作る。
    ///
    /// - 前のふるい (04 §5.2) で送る発話の、前のふるいの後の本文を文に分けて、区切りの中で 1 から番号を振る。
    ///   送らない発話 (挨拶・相槌・短い) でも、訂正の候補があれば番号を振る。
    /// - 応答は頭の 500 文字を `[参照のみ]` の枠に入れる (番号を振らない)。
    /// - 訂正の候補は、`{new}` の穴の始まりを含む文の番号で示し、直前の応答の末尾の 800 文字を枠に入れる。
    ///   番号を振った文に穴の無い候補は送らない。
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
        let mut pieces: Vec<Piece> = Vec::new();
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
                to_u32(u.turn, stored.end)?;
                number += 1;
                let shown =
                    prompt::one_line(&prompt::masked(u.text, stored.clone(), &hidden, w.hidden));
                sentences.push(SentenceLayout {
                    number,
                    stored,
                    piece: push(&mut pieces, format!("[{number}] {shown}")),
                });
            }
            let reply = match &pair.reply {
                Some(r) => {
                    let rh = merged_hidden(r)?;
                    let head = 0..char_pos(r.text, REPLY_CHARS);
                    let body = prompt::frame_body(&prompt::masked(r.text, head, &rh, w.hidden));
                    Some(ReplyLayout {
                        turn: r.turn,
                        rev: r.rev,
                        piece: push(&mut pieces, prompt::frame(w, None, &body)),
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
        Ok(Self {
            language: input.language,
            pairs,
            candidates,
            pieces,
            through: Through {
                turn: last.utterance.turn,
                pairs: to_u32(last.utterance.turn, input.pairs.len())?,
            },
            correction_version: input.correction_version,
        })
    }

    /// 訂正の候補の行を作る。番号は、往復の順・`{new}` の穴の始まりの順に 1 から振る。
    fn lay_candidates(
        input: &Input<'_>,
        pairs: &[PairLayout],
        hidden_of: &[Vec<Range<usize>>],
        w: &Words,
        pieces: &mut Vec<Piece>,
    ) -> Result<Vec<CandidateLayout>, InputError> {
        let mut order: Vec<(usize, &Candidate<'_>)> = input
            .candidates
            .iter()
            .filter_map(|c| {
                let i = pairs.iter().position(|p| p.turn == c.turn)?;
                Some((i, c))
            })
            .collect();
        order.sort_by_key(|(i, c)| (*i, c.new.start, c.new.end));
        let mut out = Vec::new();
        let mut id = 0u32;
        for (i, c) in order {
            let pair = &pairs[i];
            let Some(sentence) = pair
                .sentences
                .iter()
                .position(|s| s.stored.start <= c.new.start && c.new.start < s.stored.end)
            else {
                continue;
            };
            id += 1;
            let text = input.pairs[i].utterance.text;
            let hidden = &hidden_of[i];
            let hole = |r: &Range<usize>| {
                prompt::one_line(&prompt::masked(text, r.clone(), hidden, w.hidden))
            };
            let line = prompt::candidate_line(
                input.language,
                id,
                pair.sentences[sentence].number,
                c.pattern,
                c.old.as_ref().map(hole).as_deref(),
                &hole(&c.new),
            );
            let (full, half) = match &c.previous {
                Some(p) => {
                    let ph = merged_hidden(p)?;
                    let tail = |max: usize| {
                        let n = p.text.chars().count();
                        let start = char_pos(p.text, n.saturating_sub(max));
                        prompt::frame_body(&prompt::masked(
                            p.text,
                            start..p.text.len(),
                            &ph,
                            w.hidden,
                        ))
                    };
                    (
                        prompt::frame(w, Some(w.previous), &tail(PREVIOUS_CHARS)),
                        prompt::frame(w, Some(w.previous), &tail(PREVIOUS_CHARS / 2)),
                    )
                }
                None => {
                    let none = format!("{} {}", w.previous, w.none);
                    (none.clone(), none)
                }
            };
            out.push(CandidateLayout {
                id,
                pair: i,
                sentence,
                turn: c.turn,
                rev: pair.rev,
                pattern: c.pattern.to_owned(),
                new: byte_range(c.turn, &c.new)?,
                old: c.old.as_ref().map(|o| byte_range(c.turn, o)).transpose()?,
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

    /// トークンを数える行 (この順の数を [`Layout::plan`] に渡す)。
    pub fn pieces(&self) -> impl ExactSizeIterator<Item = &str> {
        self.pieces.iter().map(|p| p.text.as_str())
    }

    /// 行のトークンの数 `counts` ([`Layout::pieces`] の順) と、利用者のメッセージの予算 `budget`
    /// ([`user_budget`]) から、呼び出しに分ける (04 §5.2.1 の「収まらない区切り」)。
    ///
    /// - 往復を古い順に詰め、予算を超える往復の前で次の呼び出しに分ける (区切りの境界は変えない)。
    /// - 1 つの往復だけで予算を超えるときは、直前の応答を半分 (400 文字) にし、それでも超えれば応答の枠を外し、
    ///   文を前から予算に入るだけ送る。残りの文は送らずに数える ([`Plan::unsent`])。
    /// - 番号を振った文の無い呼び出しは作らない (その応答の枠は送らない)。
    ///
    /// # Errors
    /// `counts` の数が行の数と合わないとき。
    pub fn plan(&self, counts: &[usize], budget: usize) -> Result<Plan, CountMismatch> {
        if counts.len() != self.pieces.len() {
            return Err(CountMismatch {
                want: self.pieces.len(),
                got: counts.len(),
            });
        }
        let mut plan = Plan::default();
        let mut cur = Part::default();
        let mut sum = 0usize;
        for (i, p) in self.pairs.iter().enumerate() {
            let cands: Vec<usize> = (0..self.candidates.len())
                .filter(|&c| self.candidates[c].pair == i)
                .collect();
            let full = p.sentences.iter().map(|s| counts[s.piece]).sum::<usize>()
                + p.reply.as_ref().map_or(0, |r| counts[r.piece])
                + cands
                    .iter()
                    .map(|&c| counts[self.candidates[c].full])
                    .sum::<usize>();
            if full <= budget {
                if sum + full > budget {
                    plan.push(std::mem::take(&mut cur));
                    sum = 0;
                }
                cur.add_pair(i, p, &cands, Cut::Full);
                sum += full;
                continue;
            }
            plan.push(std::mem::take(&mut cur));
            sum = 0;
            let squeezed = self.squeeze(i, &cands, counts, budget, &mut plan.unsent);
            plan.push(squeezed);
        }
        plan.push(cur);
        Ok(plan)
    }

    /// 1 つで予算を超える往復を、予算に入る形にする。
    fn squeeze(
        &self,
        i: usize,
        cands: &[usize],
        counts: &[usize],
        budget: usize,
        unsent: &mut u32,
    ) -> Part {
        let p = &self.pairs[i];
        let mut part = Part {
            pairs: i..i + 1,
            ..Part::default()
        };
        let reply = p.reply.as_ref().map_or(0, |r| counts[r.piece]);
        let all = p.sentences.iter().map(|s| counts[s.piece]).sum::<usize>()
            + cands
                .iter()
                .map(|&c| counts[self.candidates[c].half])
                .sum::<usize>();
        let with_reply = all + reply <= budget;
        let mut sum = if with_reply { reply } else { 0 };
        for (k, s) in p.sentences.iter().enumerate() {
            let mine: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&c| self.candidates[c].sentence == k)
                .collect();
            let cost = counts[s.piece]
                + mine
                    .iter()
                    .map(|&c| counts[self.candidates[c].half])
                    .sum::<usize>();
            if sum + cost > budget {
                *unsent += u32::try_from(p.sentences.len() - k).unwrap_or(u32::MAX);
                break;
            }
            sum += cost;
            part.sentences.push((i, k));
            part.candidates
                .extend(mine.into_iter().map(|c| (c, Cut::Half)));
        }
        if with_reply && p.reply.is_some() {
            part.replies.push(i);
        }
        part
    }

    /// 呼び出しのメッセージ・JSON Schema と、台帳に書く `sentences`・`corr`・`refs`・`through` を作る。
    #[must_use]
    pub fn render(&self, plan: &Plan) -> Call {
        let w = prompt::words(self.language);
        let mut call = Call {
            requests: Vec::with_capacity(plan.parts.len()),
            sentences: Vec::new(),
            corr: Vec::new(),
            refs: Vec::new(),
            through: self.through,
            unsent: plan.unsent,
            correction_version: self.correction_version,
        };
        for part in &plan.parts {
            let mut lines: Vec<&str> = Vec::new();
            let mut numbers: Vec<u32> = Vec::new();
            for i in part.pairs.clone() {
                let p = &self.pairs[i];
                for &(_, k) in part.sentences.iter().filter(|(pi, _)| *pi == i) {
                    let s = &p.sentences[k];
                    lines.push(&self.pieces[s.piece].text);
                    numbers.push(s.number);
                    call.sentences.push(SentenceSpan::new(
                        s.number,
                        p.turn,
                        p.rev,
                        // 位置は Layout::new で u32 に入ることを確かめた。
                        u32::try_from(s.stored.start).unwrap_or(u32::MAX),
                        u32::try_from(s.stored.end).unwrap_or(u32::MAX),
                    ));
                }
                if let Some(r) = p.reply.as_ref().filter(|_| part.replies.contains(&i)) {
                    lines.push(&self.pieces[r.piece].text);
                    add_ref(&mut call.refs, r.turn, r.rev);
                }
            }
            let mut ids: Vec<u32> = Vec::new();
            if !part.candidates.is_empty() {
                lines.push("");
                lines.push(w.candidates);
            }
            for &(c, cut) in &part.candidates {
                let cand = &self.candidates[c];
                lines.push(
                    &self.pieces[match cut {
                        Cut::Full => cand.full,
                        Cut::Half => cand.half,
                    }]
                    .text,
                );
                ids.push(cand.id);
                if let Some((turn, rev)) = cand.previous {
                    add_ref(&mut call.refs, turn, rev);
                }
                call.corr.push(CorrInput {
                    id: cand.id,
                    turn: cand.turn,
                    rev: cand.rev,
                    pattern: cand.pattern.clone(),
                    new: cand.new,
                    old: cand.old,
                    extra: Default::default(),
                });
            }
            call.requests.push(Request {
                system: self.system(),
                user: lines.join("\n"),
                schema: schema(&numbers, &ids),
                sentences: numbers,
                candidates: ids,
            });
        }
        call.sentences.sort_by_key(|s| s.s);
        call
    }
}

/// 送った応答を `refs` に足す (同じターンは 1 つだけ)。
fn add_ref(refs: &mut Vec<LedgerRef>, turn: Id, rev: u32) {
    if !refs.iter().any(|r| r.turn == turn) {
        refs.push(LedgerRef::new(turn, rev));
    }
}

fn push(pieces: &mut Vec<Piece>, text: String) -> usize {
    pieces.push(Piece { text });
    pieces.len() - 1
}

/// 直前の応答の長さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// 末尾の 800 文字。
    Full,
    /// 予算に入らないので半分 (400 文字) にした。
    Half,
}

/// 1 回の呼び出しに入れるもの。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Part {
    /// 往復の範囲 ([`Layout`] の往復の位置)。
    pairs: Range<usize>,
    /// 送る文 (往復の位置, 往復の中の文の位置)。
    sentences: Vec<(usize, usize)>,
    /// 応答の枠を送る往復の位置。
    replies: Vec<usize>,
    /// 送る訂正の候補と、直前の応答の長さ。
    candidates: Vec<(usize, Cut)>,
}

impl Part {
    fn add_pair(&mut self, i: usize, p: &PairLayout, cands: &[usize], cut: Cut) {
        if self.pairs.is_empty() {
            self.pairs = i..i + 1;
        } else {
            self.pairs.end = i + 1;
        }
        self.sentences
            .extend((0..p.sentences.len()).map(|k| (i, k)));
        if p.reply.is_some() {
            self.replies.push(i);
        }
        self.candidates.extend(cands.iter().map(|&c| (c, cut)));
    }

    /// `range` の往復だけを残した呼び出し。
    fn within(&self, range: Range<usize>) -> Self {
        Self {
            pairs: range.clone(),
            sentences: self
                .sentences
                .iter()
                .copied()
                .filter(|(i, _)| range.contains(i))
                .collect(),
            replies: self
                .replies
                .iter()
                .copied()
                .filter(|i| range.contains(i))
                .collect(),
            candidates: self.candidates.clone(),
        }
    }
}

/// 区切りを呼び出しに分けたもの ([`Layout::plan`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    parts: Vec<Part>,
    unsent: u32,
}

impl Plan {
    /// 呼び出しの数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// 呼び出しが無いか (番号を振った文が無い区切り)。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// 1 つの往復だけで予算を超えたので送らなかった文の数。
    #[must_use]
    pub fn unsent(&self) -> u32 {
        self.unsent
    }

    /// 番号を振った文のある呼び出しだけを足す。
    fn push(&mut self, part: Part) {
        if !part.sentences.is_empty() {
            self.parts.push(part);
        }
    }

    /// 出力が上限で切れた (`finish_reason` が `length`) 呼び出し `index` を、往復の境で半分に分けた計画
    /// (04 §5.2.1 の「出力が上限で切れたとき」)。往復が 1 つなら `None` (型が合わないときと同じに扱う)。
    #[must_use]
    pub fn halve(&self, index: usize, layout: &Layout) -> Option<Self> {
        let part = self.parts.get(index)?;
        let n = part.pairs.len();
        if n < 2 {
            return None;
        }
        let mid = part.pairs.start + n / 2;
        let mut out = Self {
            parts: Vec::with_capacity(self.parts.len() + 1),
            unsent: self.unsent,
        };
        for (k, p) in self.parts.iter().enumerate() {
            if k != index {
                out.parts.push(p.clone());
                continue;
            }
            for range in [part.pairs.start..mid, mid..part.pairs.end] {
                let mut half = p.within(range.clone());
                half.candidates
                    .retain(|(c, _)| range.contains(&layout.candidates[*c].pair));
                out.push(half);
            }
        }
        Some(out)
    }
}

/// 1 回の呼び出し (チャットのメッセージ 2 つと、出力の型縛りの JSON Schema)。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// システムのメッセージ (指示・辞書・必ず書くこと・出力の型の説明)。
    pub system: &'static str,
    /// 利用者のメッセージ (区切りの往復と訂正の候補)。
    pub user: String,
    /// 出力の JSON Schema (`s` と `id` の列挙だけが呼び出しごとに変わる、06 §6)。
    pub schema: Value,
    sentences: Vec<u32>,
    candidates: Vec<u32>,
}

impl Request {
    /// 型縛りの JSON Schema の名前。
    pub const SCHEMA_NAME: &'static str = "b2_segment";

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

    /// この呼び出しで送った文番号 (昇順)。
    #[must_use]
    pub fn sentence_numbers(&self) -> &[u32] {
        &self.sentences
    }

    /// この呼び出しで送った訂正の候補の番号。
    #[must_use]
    pub fn candidate_ids(&self) -> &[u32] {
        &self.candidates
    }
}

/// 出力の JSON Schema (04 §5.2 の「出力の型」の書くときの型。すべての項目が必須で、上限は `maxItems` で縛る)。
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
                        "a": {"type": "string", "maxLength": ASPECT_MAX_CHARS},
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

/// 区切りの呼び出しのすべて ([`Layout::render`])。呼び出しの結果と合わせて台帳を書く。
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// 呼び出し (順に送る)。空なら、番号を振った文の無い区切り (呼ばずに、出力が空の台帳を書く)。
    pub requests: Vec<Request>,
    /// 台帳の `sentences` (送った文だけ。文番号の順)。
    pub sentences: Vec<SentenceSpan>,
    /// 台帳の `inputs` の `corr` (送った候補だけ)。
    pub corr: Vec<CorrInput>,
    /// 台帳の `inputs` の `refs` (送った応答。区切りの応答と、候補の直前の応答)。
    pub refs: Vec<LedgerRef>,
    /// 台帳の `inputs` の `through` (区切りの最後の往復と往復の数)。
    pub through: Through,
    /// 1 つの往復だけで予算を超えたので送らなかった文の数。
    pub unsent: u32,
    correction_version: u32,
}

/// 台帳の `inputs` のうち、呼び手が知っているもの (04 §5.2.1)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// 入力を読んだときに見えていた最新の忘れるの指示の ID。
    pub forgets_seen: Option<Id>,
    /// 縮退で chat のモデルを使ったか。
    pub role_fallback: bool,
    /// 意図した `memory` の役割の model_key。
    pub role_model_key: Option<String>,
}

impl Call {
    /// 送ったメッセージのバイト列の SHA-256 (64 桁の小文字の 16 進。04 §5.2.1 の `input_hash`)。呼び出しの順に、
    /// システムと利用者のメッセージを、それぞれ u32be の長さを前に置いて並べたバイト列のハッシュ。呼び出しが無ければ
    /// `None`。
    #[must_use]
    pub fn input_hash(&self) -> Option<String> {
        if self.requests.is_empty() {
            return None;
        }
        let mut h = Sha256::new();
        for r in &self.requests {
            for m in [r.system, r.user.as_str()] {
                let len = u32::try_from(m.len()).unwrap_or(u32::MAX);
                h.update(len.to_be_bytes());
                h.update(m.as_bytes());
            }
        }
        Some(
            h.finalize()
                .iter()
                .fold(String::with_capacity(64), |mut s, b| {
                    use fmt::Write;
                    let _ = write!(s, "{b:02x}");
                    s
                }),
        )
    }

    /// 台帳の `inputs` (規則の版・`input_hash`・`through`・`corr`・`refs` と、呼び手の `selection`)。
    #[must_use]
    pub fn inputs(&self, selection: &Selection) -> LedgerInputs {
        LedgerInputs {
            prefilter: PREFILTER_V1,
            splitter: SPLITTER_V1,
            correction: self.correction_version,
            forgets_seen: selection.forgets_seen,
            input_hash: self.input_hash(),
            role_fallback: selection.role_fallback,
            role_model_key: selection.role_model_key.clone(),
            through: Some(self.through),
            corr: self.corr.clone(),
            refs: self.refs.clone(),
            extra: Default::default(),
        }
    }
}
