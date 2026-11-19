//! 忘れるの純粋な核 (04 §7.2 の `apply_forget`、§7.3)。値の鍵を使わない。
//!
//! - [`apply_forget`]: 1 つの指示の 1 つのターンの伏せ字を、本文と改訂の番号に当てる。何度当てても同じ。
//! - [`apply_forgets`] / [`apply_to_turn`]: 同じターンのすべての指示を、指示の ID の順に当てる (DB の層が、
//!   生きている DB と退避の写しの両方で使う)。
//! - [`Redaction::remap`]: 伏せ字の前の位置を後の位置に直す、ただ 1 つの関数 (§7.3 の「位置の写し直し」)。
//! - [`display_marks`]: 画面への印の範囲 (§7.3 の「画面への印の範囲」)。

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use sha2::{Digest, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use crate::envelope::{BodyError, BodyPatch, Envelope};

/// `turn` の本体の本文の項目 (03 §3.3)。
pub const TURN_TEXT: &str = "text";
/// `turn` の本体の改訂の番号の項目 (03 §3.3、04 §7.3)。無いターンは 0 として読む。
pub const TURN_REV: &str = "rev";

/// 本文の SHA-256 (伏せ字の後の本文のハッシュ、04 §7.3)。指示の本体では 64 桁の小文字の 16 進で持つ。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextHash([u8; 32]);

impl TextHash {
    /// `text` の UTF-8 のバイト列の SHA-256。
    #[must_use]
    pub fn of(text: &str) -> Self {
        Self(Sha256::digest(text.as_bytes()).into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for TextHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for TextHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TextHash({self})")
    }
}

/// 64 桁の小文字の 16 進として読めない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a text hash must be 64 lowercase hex digits")]
pub struct ParseTextHashError;

impl FromStr for TextHash {
    type Err = ParseTextHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return Err(ParseTextHashError);
        }
        let digit = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(ParseTextHashError),
        };
        let mut out = [0u8; 32];
        let (pairs, _) = b.as_chunks::<2>();
        for (o, [hi, lo]) in out.iter_mut().zip(pairs) {
            *o = (digit(*hi)? << 4) | digit(*lo)?;
        }
        Ok(Self(out))
    }
}

/// 伏せ字の記述が形として正しくない (指示の本体が壊れているか、新しい版の形)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRedaction {
    #[error("rev_after {after} is not greater than rev_before {before}")]
    Revisions { before: u32, after: u32 },
    #[error("a redaction needs at least one range")]
    NoRanges,
    #[error("range {start}..{end} is empty or reversed")]
    EmptyRange { start: usize, end: usize },
    #[error("range {start}..{end} is outside the text or not on a character boundary")]
    OutOfText { start: usize, end: usize },
    #[error("a stored position does not fit in this platform's usize")]
    Position,
    #[error("the stored hash is not 64 lowercase hex digits")]
    Hash,
}

/// DB に記録した位置の組を範囲にする (`usize` に収まらなければ `None`)。
fn stored_range((start, end): (u64, u64)) -> Option<Range<usize>> {
    Some(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
}

/// 1 つの忘れるの指示が 1 つのターンにした伏せ字 (04 §7.5 の `redactions` のうち、そのターンの要素をまとめたもの)。
///
/// 範囲は伏せ字の前の本文 (`rev_before`) の UTF-8 のバイト位置で、始まりの順に並べ、重なる範囲と接する範囲を
/// まとめたもの (§7.3)。作るときにまとめるので、同じ指示の要素を順不同で渡してよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    rev_before: u32,
    rev_after: u32,
    ranges: Vec<Range<usize>>,
    hash: TextHash,
}

impl Redaction {
    /// 記録した伏せ字 (指示の本体) から作る。
    ///
    /// # Errors
    /// `rev_after` が `rev_before` より大きくないとき、範囲が無いとき、空の範囲があるとき。
    pub fn new(
        rev_before: u32,
        rev_after: u32,
        ranges: impl IntoIterator<Item = Range<usize>>,
        hash: TextHash,
    ) -> Result<Self, InvalidRedaction> {
        if rev_after <= rev_before {
            return Err(InvalidRedaction::Revisions {
                before: rev_before,
                after: rev_after,
            });
        }
        let ranges: Vec<Range<usize>> = ranges.into_iter().collect();
        if let Some(r) = ranges.iter().find(|r| r.start >= r.end) {
            return Err(InvalidRedaction::EmptyRange {
                start: r.start,
                end: r.end,
            });
        }
        let ranges = merge_ranges(ranges);
        if ranges.is_empty() {
            return Err(InvalidRedaction::NoRanges);
        }
        Ok(Self {
            rev_before,
            rev_after,
            ranges,
            hash,
        })
    }

    /// 指示の本体に記録した値 (`rev_before`・`rev_after`・`{start, end}` の並び・64 桁の 16 進のハッシュ) から作る。
    /// 位置は、この環境の `usize` に収まることを確かめる (本文に収まるかは、当てるときに確かめる)。
    ///
    /// # Errors
    /// 位置が `usize` に収まらないとき、ハッシュが読めないとき、ほかは [`Self::new`] と同じ。
    pub fn from_stored(
        rev_before: u32,
        rev_after: u32,
        ranges: impl IntoIterator<Item = (u64, u64)>,
        hash: &str,
    ) -> Result<Self, InvalidRedaction> {
        let hash: TextHash = hash.parse().map_err(|_| InvalidRedaction::Hash)?;
        let ranges = ranges
            .into_iter()
            .map(stored_range)
            .collect::<Option<Vec<_>>>()
            .ok_or(InvalidRedaction::Position)?;
        Self::new(rev_before, rev_after, ranges, hash)
    }

    /// 操作のときに、今の本文 `text` (改訂の番号 `rev`) の範囲 `ranges` を伏せ字にする記述を作る。改訂の番号は
    /// 範囲の数に関わらず 1 だけ上げ、ハッシュはすべての範囲を置き換えた後の本文から求める (§7.3)。伏せ字の後の
    /// 本文も返す。範囲は書記素の境界に広げてから渡す ([`widen_to_graphemes`])。
    ///
    /// # Errors
    /// 範囲が無い・空・本文の外・文字の境界でないとき、`rev` が上げられないとき。
    pub fn plan(
        text: &str,
        rev: u32,
        ranges: impl IntoIterator<Item = Range<usize>>,
        marker: &str,
    ) -> Result<(Self, String), InvalidRedaction> {
        let after = rev.checked_add(1).ok_or(InvalidRedaction::Revisions {
            before: rev,
            after: rev,
        })?;
        // ハッシュは置き換えた後に決まるので、仮の値で範囲を確かめてから置き換える。
        let mut r = Self::new(rev, after, ranges, TextHash([0; 32]))?;
        let redacted = replace_ranges(text, &r.ranges, marker)?;
        r.hash = TextHash::of(&redacted);
        Ok((r, redacted))
    }

    #[must_use]
    pub fn rev_before(&self) -> u32 {
        self.rev_before
    }

    #[must_use]
    pub fn rev_after(&self) -> u32 {
        self.rev_after
    }

    /// まとめた範囲 (伏せ字の前の位置、始まりの順)。
    #[must_use]
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    #[must_use]
    pub fn hash(&self) -> TextHash {
        self.hash
    }

    /// 伏せ字の前の位置 `pos` を、伏せ字の後の位置に直す (§7.3 の「位置の写し直し」)。
    ///
    /// `pos` 以前に終わる範囲 (終わり ≦ `pos`) の (範囲の長さ − 印の長さ) の和だけ前へずらす。範囲の内側
    /// (始まり < `pos` < 終わり) の位置は、`edge` が [`Edge::Start`] なら印の始まりへ、[`Edge::End`] なら印の
    /// 終わりへ寄せる。範囲の始まりちょうどの位置は、その範囲ではずらさない。
    #[must_use]
    pub fn remap(&self, pos: usize, marker: &str, edge: Edge) -> usize {
        // 範囲はまとめて始まりの順に並んでいるので減算は負にならないが、DB から読んだ値で桁あふれしないように
        // 飽和させる。
        let (mut removed, mut added) = (0usize, 0usize);
        for r in &self.ranges {
            if r.end <= pos {
                removed = removed.saturating_add(r.end - r.start);
                added = added.saturating_add(marker.len());
            } else if r.start < pos {
                let at = r.start.saturating_sub(removed).saturating_add(added);
                return match edge {
                    Edge::Start => at,
                    Edge::End => at.saturating_add(marker.len()),
                };
            } else {
                break;
            }
        }
        pos.saturating_sub(removed).saturating_add(added)
    }

    /// 範囲 (span・貼り付けの範囲・`[非表示]` の範囲) を伏せ字の後の位置に直す。始まりは [`Edge::Start`]、
    /// 終わりは [`Edge::End`] で写すので、伏せ字に掛かる範囲は印を含むように広がる (§7.3)。
    #[must_use]
    pub fn remap_range(&self, range: Range<usize>, marker: &str) -> Range<usize> {
        let start = self.remap(range.start, marker, Edge::Start);
        let end = self.remap(range.end, marker, Edge::End).max(start);
        start..end
    }

    /// 伏せ字の後の本文での、印の範囲 (まとめた範囲ごとに 1 つ、始まりの順)。
    #[must_use]
    pub fn marker_ranges(&self, marker: &str) -> Vec<Range<usize>> {
        self.ranges
            .iter()
            .map(|r| {
                let at = self.remap(r.start, marker, Edge::Start);
                at..at.saturating_add(marker.len())
            })
            .collect()
    }
}

/// 位置が範囲の内側に落ちたときに寄せる側 ([`Redaction::remap`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// 範囲の始まりの位置: 印の始まりへ寄せる。
    Start,
    /// 範囲の終わりの位置: 印の終わりへ寄せる。
    End,
}

/// 範囲を始まりの順に並べ、重なる範囲と接する範囲 (前の終わり = 次の始まり) を 1 つにまとめる (§7.3)。
/// 空の範囲は捨てる。
#[must_use]
pub fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// 範囲を本文の長さに収め、両端を書記素の境界 (結合文字・異体字セレクタ・ZWJ の並びを分けない) まで外へ広げる
/// (§7.3)。値の鍵の検査は広げる前の範囲で行い、記録して置き換えるのは広げた範囲にする。
#[must_use]
pub fn widen_to_graphemes(text: &str, range: Range<usize>) -> Range<usize> {
    let end_limit = range.end.min(text.len());
    let start_limit = range.start.min(end_limit);
    let boundaries = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()));
    let (mut start, mut end) = (0, text.len());
    for b in boundaries {
        if b <= start_limit {
            start = b;
        }
        if b >= end_limit {
            end = b;
            break;
        }
    }
    start..end
}

/// まとめた範囲を `marker` に置き換える。範囲が本文の外か文字の境界でなければ `Err`。
fn replace_ranges(
    text: &str,
    ranges: &[Range<usize>],
    marker: &str,
) -> Result<String, InvalidRedaction> {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in ranges {
        let (Some(before), Some(_)) = (text.get(at..r.start), text.get(r.clone())) else {
            return Err(InvalidRedaction::OutOfText {
                start: r.start,
                end: r.end,
            });
        };
        out.push_str(before);
        out.push_str(marker);
        at = r.end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// [`apply_forget`] の結果 (04 §7.2 の「純粋な核」)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ForgetOutcome {
    /// 範囲を伏せ字にし、ハッシュが合った。本文と改訂の番号をこれにする。
    Applied { text: String, rev: u32 },
    /// すでに当たっている (この指示か、後の指示が)。本文を変えない。
    AlreadyApplied,
    /// 先の忘れるが当たっていない (今の改訂の番号が伏せ字の前の番号より小さい)。
    NeedsEarlier,
    /// 本文が記録と合わない。本文を変えない (層ごとの扱いは 04 §7.2)。
    Mismatch,
}

/// 1 つの指示の 1 つのターンの伏せ字を、本文 `text` (改訂の番号 `rev`、無いターンは 0) に当てる (04 §7.2)。
///
/// - `rev` が伏せ字の後の番号と同じで、本文のハッシュが合う → 当て済み。
/// - `rev` が伏せ字の前の番号と同じ → 範囲を `marker` に置き換え、ハッシュが合えば「当てた本文」、合わなければ
///   (範囲が本文に収まらないときも)「合わない」。
/// - `rev` が伏せ字の後の番号より大きい → 当て済み (後の忘れるが当たっている)。
/// - `rev` が伏せ字の前の番号より小さい → 先の指示が要る。
/// - それ以外 → 合わない。
pub fn apply_forget(text: &str, rev: u32, redaction: &Redaction, marker: &str) -> ForgetOutcome {
    if rev == redaction.rev_after && TextHash::of(text) == redaction.hash {
        return ForgetOutcome::AlreadyApplied;
    }
    if rev == redaction.rev_before {
        return match replace_ranges(text, &redaction.ranges, marker) {
            Ok(out) if TextHash::of(&out) == redaction.hash => ForgetOutcome::Applied {
                text: out,
                rev: redaction.rev_after,
            },
            _ => ForgetOutcome::Mismatch,
        };
    }
    if rev > redaction.rev_after {
        return ForgetOutcome::AlreadyApplied;
    }
    if rev < redaction.rev_before {
        return ForgetOutcome::NeedsEarlier;
    }
    ForgetOutcome::Mismatch
}

/// 1 つのターンに、そのターンのすべての忘れるの指示を当てた結果 ([`apply_forgets`])。
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum TurnOutcome {
    /// どの指示もすでに当たっている (または指示が無い)。本文を変えない。
    Unchanged,
    /// 1 つ以上の指示を当てた。本文と改訂の番号をこれにする。
    Changed { text: String, rev: u32 },
    /// 先の指示が無い (指示の ID の順に当てても、伏せ字の前の番号に届かない)。
    NeedsEarlier,
    /// 本文が記録と合わない (または本文と改訂の番号を読めない)。
    Mismatch,
}

/// 同じターンの指示の伏せ字 `steps` (指示の ID の順) を、先のものから順に当てる (04 §7.2 の DB の層)。
pub fn apply_forgets(text: &str, rev: u32, steps: &[Redaction], marker: &str) -> TurnOutcome {
    let mut current: Option<(String, u32)> = None;
    for step in steps {
        let (t, r) = current
            .as_ref()
            .map_or((text, rev), |(t, r)| (t.as_str(), *r));
        match apply_forget(t, r, step, marker) {
            ForgetOutcome::Applied { text, rev } => current = Some((text, rev)),
            ForgetOutcome::AlreadyApplied => {}
            ForgetOutcome::NeedsEarlier => return TurnOutcome::NeedsEarlier,
            ForgetOutcome::Mismatch => return TurnOutcome::Mismatch,
        }
    }
    match current {
        Some((text, rev)) => TurnOutcome::Changed { text, rev },
        None => TurnOutcome::Unchanged,
    }
}

/// `turn` の封筒の本文 (`text`) と改訂の番号 (`rev`、無ければ 0) に [`apply_forgets`] を当てる。本体を読めなければ
/// 「合わない」。
pub fn apply_to_turn(turn: &Envelope, steps: &[Redaction], marker: &str) -> TurnOutcome {
    let Some(rev) = turn.body_uint(TURN_REV).and_then(|r| u32::try_from(r).ok()) else {
        return TurnOutcome::Mismatch;
    };
    let ciborium::Value::Map(map) = &turn.body else {
        return TurnOutcome::Mismatch;
    };
    let Some(text) = map
        .iter()
        .find(|(k, _)| k.as_text() == Some(TURN_TEXT))
        .and_then(|(_, v)| v.as_text())
    else {
        return TurnOutcome::Mismatch;
    };
    apply_forgets(text, rev, steps, marker)
}

/// 伏せ字にした本文と改訂の番号を `turn` の本体に書く差し替え (ほかの項目はそのまま残す)。
///
/// # Errors
/// 値を CBOR にできないとき (起きない)。
pub fn turn_patch(text: &str, rev: u32) -> Result<BodyPatch, BodyError> {
    BodyPatch::new().set(TURN_TEXT, &text)?.set(TURN_REV, &rev)
}

/// 画面への印の種類 (§7.3 の「画面への印の範囲」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarkKind {
    /// 本文のその範囲が、すでに伏せ字の印の文字列。
    Redacted,
    /// 本文には値が残っていて、画面が表示するときに `[非表示]` の印に置き換える。
    Hidden,
}

/// 画面への印 1 つ。範囲は今の本文の UTF-8 のバイト位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub range: Range<usize>,
    pub kind: MarkKind,
}

/// 伏せ字にしないで忘れたときの `[非表示]` の範囲 (04 §7.5 の `hidden`)。範囲は改訂の番号 `rev` の本文の位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenRange {
    pub rev: u32,
    pub range: Range<usize>,
}

impl HiddenRange {
    /// 指示の本体に記録した値 (`{turn, rev, start, end}` の `rev`・`start`・`end`) から作る。位置が `usize` に
    /// 収まらないか、空・逆向きの範囲なら `None`。
    #[must_use]
    pub fn from_stored(rev: u32, start: u64, end: u64) -> Option<Self> {
        let range = stored_range((start, end))?;
        (range.start < range.end).then_some(Self { rev, range })
    }
}

/// 1 つの忘れるの指示が 1 つのターンに持つもの ([`display_marks`] の入力)。
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnDirective<'a> {
    pub redaction: Option<&'a Redaction>,
    pub hidden: &'a [HiddenRange],
}

/// ターンの印の範囲を、今の本文 (改訂の番号 `rev_now`) の位置で求める (§7.3)。`directives` は、そのターンに
/// 掛かる忘れるの指示を指示の ID の順に並べたもの。
///
/// 伏せ字は指示の伏せ字の一覧から、`[非表示]` は `hidden` から作り、どちらもその指示より後の伏せ字で写す
/// ([`Redaction::remap_range`])。`[非表示]` の範囲が後の伏せ字に重なるときは、写した範囲 (印を含む) を
/// `[非表示]` にする。`[非表示]` の範囲に含まれる伏せ字の印は除く (表示では `[非表示]` が覆う)。
///
/// 求めた範囲は今の本文 `text` で確かめ、本文の外・文字の境界でない範囲と、本文がその範囲で印の文字列になって
/// いない伏せ字は除く (DB から読んだ位置が壊れていても、本文の外を指す範囲を返さない)。
///
/// 指示を順に当てて `rev_now` に届かない・追い越す (指示が欠けている・ターンがまだ伏せ字になっていない)、または
/// 範囲の改訂の番号が合わないときは `None` (呼び手は、そのターンを「合わないターン」として扱う、04 §7.2)。
#[must_use]
pub fn display_marks(
    text: &str,
    rev_now: u32,
    directives: &[TurnDirective<'_>],
    marker: &str,
) -> Option<Vec<Mark>> {
    // 伏せ字の無いターンは改訂の番号 0 から始まる (04 §7.3)。最初の伏せ字の前の番号から始める。
    let mut rev = directives
        .iter()
        .find_map(|d| d.redaction.map(Redaction::rev_before))
        .unwrap_or(rev_now)
        .min(rev_now);
    let mut marks: Vec<Mark> = Vec::new();
    for d in directives {
        if let Some(r) = d.redaction {
            if r.rev_before == rev {
                for m in &mut marks {
                    m.range = r.remap_range(m.range.clone(), marker);
                }
                marks.extend(r.marker_ranges(marker).into_iter().map(|range| Mark {
                    range,
                    kind: MarkKind::Redacted,
                }));
                rev = r.rev_after;
            } else if r.rev_after > rev {
                return None;
            }
        }
        for h in d.hidden {
            if h.rev != rev {
                return None;
            }
            marks.push(Mark {
                range: h.range.clone(),
                kind: MarkKind::Hidden,
            });
        }
    }
    if rev != rev_now {
        return None;
    }
    marks.retain(|m| {
        let Some(shown) = text.get(m.range.clone()) else {
            return false;
        };
        !shown.is_empty() && (m.kind == MarkKind::Hidden || shown == marker)
    });
    Some(normalize_marks(marks))
}

/// 重なる `[非表示]` をまとめ、`[非表示]` に含まれる印と同じ伏せ字の重複を除き、始まりの順に並べる。
fn normalize_marks(marks: Vec<Mark>) -> Vec<Mark> {
    let hidden = merge_ranges(
        marks
            .iter()
            .filter(|m| m.kind == MarkKind::Hidden)
            .map(|m| m.range.clone())
            .collect(),
    );
    let mut redacted: Vec<Range<usize>> = marks
        .into_iter()
        .filter(|m| m.kind == MarkKind::Redacted)
        .map(|m| m.range)
        .filter(|r| !hidden.iter().any(|h| h.start <= r.start && r.end <= h.end))
        .collect();
    redacted.sort_by_key(|r| (r.start, r.end));
    redacted.dedup();
    let mut out: Vec<Mark> = hidden
        .into_iter()
        .map(|range| Mark {
            range,
            kind: MarkKind::Hidden,
        })
        .chain(redacted.into_iter().map(|range| Mark {
            range,
            kind: MarkKind::Redacted,
        }))
        .collect();
    out.sort_by_key(|m| (m.range.start, m.range.end, m.kind));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "[伏せ字]";

    fn plan(text: &str, rev: u32, ranges: &[Range<usize>]) -> (Redaction, String) {
        Redaction::plan(text, rev, ranges.iter().cloned(), M).expect("valid plan")
    }

    fn span(text: &str, needle: &str) -> Range<usize> {
        let s = text.find(needle).expect("needle in text");
        s..s + needle.len()
    }

    #[test]
    fn hashes_round_trip_through_hex() {
        let h = TextHash::of("abc");
        let hex = h.to_string();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex.parse::<TextHash>(), Ok(h));
        assert!("BA7816BF".parse::<TextHash>().is_err());
        assert!(hex.to_uppercase().parse::<TextHash>().is_err());
        assert!(format!("{hex}0").parse::<TextHash>().is_err());
    }

    /// verifies: 04 §7.2 (純粋な核の 5 つの場合)、AC-FR-307-4
    #[test]
    fn applying_twice_is_the_same_as_once() {
        let text = "僕の誕生日は 3 月 14 日です";
        let (r, redacted) = plan(text, 0, &[span(text, "3 月 14 日")]);
        assert_eq!(redacted, format!("僕の誕生日は {M}です"));
        assert_eq!((r.rev_before(), r.rev_after()), (0, 1));
        let once = apply_forget(text, 0, &r, M);
        assert_eq!(
            once,
            ForgetOutcome::Applied {
                text: redacted.clone(),
                rev: 1
            }
        );
        // 当てた結果にもう一度当てても変わらない。
        assert_eq!(
            apply_forget(&redacted, 1, &r, M),
            ForgetOutcome::AlreadyApplied
        );
        // ターン単位でも同じ。
        let steps = [r];
        let first = apply_forgets(text, 0, &steps, M);
        let TurnOutcome::Changed { text: t1, rev: r1 } = first.clone() else {
            panic!("expected a change, got {first:?}");
        };
        assert_eq!(apply_forgets(&t1, r1, &steps, M), TurnOutcome::Unchanged);
    }

    #[test]
    fn the_five_cases_of_the_core() {
        let text = "a secret b";
        let (r, redacted) = plan(text, 2, &[span(text, "secret")]);
        // 後の番号でハッシュが合う → 当て済み。
        assert_eq!(
            apply_forget(&redacted, 3, &r, M),
            ForgetOutcome::AlreadyApplied
        );
        // 前の番号 → 当てる。
        assert!(matches!(
            apply_forget(text, 2, &r, M),
            ForgetOutcome::Applied { rev: 3, .. }
        ));
        // 前の番号でも範囲の外の本文が違えばハッシュが合わない。
        assert_eq!(
            apply_forget("A secret b", 2, &r, M),
            ForgetOutcome::Mismatch
        );
        // 範囲が本文に収まらない。
        assert_eq!(apply_forget("a", 2, &r, M), ForgetOutcome::Mismatch);
        // 後の番号より大きい → 後の忘れるが当たっている。
        assert_eq!(
            apply_forget("whatever", 4, &r, M),
            ForgetOutcome::AlreadyApplied
        );
        // 前の番号より小さい → 先の指示が要る。
        assert_eq!(apply_forget(text, 1, &r, M), ForgetOutcome::NeedsEarlier);
        // 後の番号でハッシュが合わない → 合わない。
        assert_eq!(apply_forget(text, 3, &r, M), ForgetOutcome::Mismatch);
    }

    #[test]
    fn a_turn_without_rev_reads_as_zero() {
        let text = "call me at 555-0100";
        let (r, redacted) = plan(text, 0, &[span(text, "555-0100")]);
        let mut env = Envelope::new(
            evorag_types::Id::from_bytes([1; 16]),
            "turn",
            1,
            evorag_types::Timestamp::from_millis(0),
            "0.3.0",
        );
        env.body = ciborium::Value::Map(vec![
            ("text".into(), text.into()),
            ("role".into(), "user".into()),
        ]);
        let out = apply_to_turn(&env, std::slice::from_ref(&r), M);
        assert_eq!(
            out,
            TurnOutcome::Changed {
                text: redacted.clone(),
                rev: 1
            }
        );
        env.apply_patch(&turn_patch(&redacted, 1).unwrap()).unwrap();
        assert_eq!(apply_to_turn(&env, &[r], M), TurnOutcome::Unchanged);
        // ほかの項目は残る。
        let ciborium::Value::Map(map) = &env.body else {
            panic!("body is a map")
        };
        assert!(map.iter().any(|(k, _)| k.as_text() == Some("role")));
        // 本文の無い本体は合わない。
        env.body = ciborium::Value::Map(vec![("rev".into(), 1.into())]);
        assert_eq!(apply_to_turn(&env, &[], M), TurnOutcome::Mismatch);
    }

    #[test]
    fn text_outside_the_ranges_is_kept_byte_for_byte() {
        let text = "前の文。値1 と 値2 の間。後ろの文 🇯🇵 e\u{301}";
        let ranges = [span(text, "値1"), span(text, "値2")];
        let (r, out) = plan(text, 0, &ranges);
        let mut expected = String::new();
        expected.push_str(&text[..ranges[0].start]);
        expected.push_str(M);
        expected.push_str(&text[ranges[0].end..ranges[1].start]);
        expected.push_str(M);
        expected.push_str(&text[ranges[1].end..]);
        assert_eq!(out, expected);
        assert_eq!(r.hash(), TextHash::of(&expected));
        assert_eq!(r.ranges().len(), 2);
    }

    #[test]
    fn overlapping_and_touching_ranges_become_one_marker() {
        let text = "abcdefghij";
        let (r, out) = plan(text, 0, &[5..7, 2..4, 4..5, 3..6]);
        assert_eq!(r.ranges(), std::slice::from_ref(&(2..7)));
        assert_eq!(out, format!("ab{M}hij"));
        // 離れた範囲は 2 つのまま。
        let (r, out) = plan(text, 0, &[1..2, 3..4]);
        assert_eq!(r.ranges(), [1..2, 3..4]);
        assert_eq!(out, format!("a{M}c{M}efghij"));
        // 1 回の操作は改訂の番号を 1 だけ上げる。
        assert_eq!(r.rev_after(), 1);
    }

    #[test]
    fn several_directives_apply_in_order_and_report_gaps() {
        let text = "alpha beta gamma";
        let (first, t1) = plan(text, 0, &[span(text, "alpha")]);
        let (second, t2) = plan(&t1, 1, &[span(&t1, "gamma")]);
        let steps = [first.clone(), second.clone()];
        assert_eq!(
            apply_forgets(text, 0, &steps, M),
            TurnOutcome::Changed {
                text: t2.clone(),
                rev: 2
            }
        );
        // 途中まで当たっているターンにも、残りだけを当てる。
        assert_eq!(
            apply_forgets(&t1, 1, &steps, M),
            TurnOutcome::Changed {
                text: t2.clone(),
                rev: 2
            }
        );
        assert_eq!(apply_forgets(&t2, 2, &steps, M), TurnOutcome::Unchanged);
        // 先の指示が欠けている。
        assert_eq!(
            apply_forgets(text, 0, std::slice::from_ref(&second), M),
            TurnOutcome::NeedsEarlier
        );
        // 本文が違う。
        assert_eq!(
            apply_forgets("alpha beta delta", 0, &steps, M),
            TurnOutcome::Mismatch
        );
    }

    #[test]
    fn invalid_redactions_are_rejected() {
        let h = TextHash::of("");
        assert!(matches!(
            Redaction::new(1, 1, std::iter::once(0..1), h),
            Err(InvalidRedaction::Revisions { .. })
        ));
        assert_eq!(
            Redaction::new(0, 1, std::iter::empty(), h),
            Err(InvalidRedaction::NoRanges)
        );
        assert!(matches!(
            Redaction::new(0, 1, std::iter::once(3..3), h),
            Err(InvalidRedaction::EmptyRange { .. })
        ));
        // 文字の途中 (「あ」は 3 バイト)。
        assert!(matches!(
            Redaction::plan("あい", 0, std::iter::once(1..3), M),
            Err(InvalidRedaction::OutOfText { .. })
        ));
        assert!(matches!(
            Redaction::plan("ab", 0, std::iter::once(1..9), M),
            Err(InvalidRedaction::OutOfText { .. })
        ));
    }

    #[test]
    fn ranges_widen_to_grapheme_boundaries() {
        // 結合文字 (e + U+0301)、異体字セレクタ、ZWJ の並び、国旗。
        let text = "xe\u{301}y 葛\u{E0100}z 👨\u{200D}👩\u{200D}👧 🇯🇵!";
        let e = text.find('e').unwrap();
        // 「e」だけを指す範囲は、結合文字まで広がる。
        assert_eq!(widen_to_graphemes(text, e..e + 1), e..e + 3);
        let k = text.find('葛').unwrap();
        assert_eq!(widen_to_graphemes(text, k..k + 3), k..k + 7);
        let fam = text.find('👨').unwrap();
        let fam_len = "👨\u{200D}👩\u{200D}👧".len();
        assert_eq!(
            widen_to_graphemes(text, fam + 4..fam + 8),
            fam..fam + fam_len
        );
        let flag = text.find('🇯').unwrap();
        assert_eq!(widen_to_graphemes(text, flag..flag + 4), flag..flag + 8);
        // 境界にある範囲は変えない。範囲の外は本文に収める。
        assert_eq!(widen_to_graphemes(text, 0..1), 0..1);
        assert_eq!(widen_to_graphemes(text, 0..999), 0..text.len());
        assert_eq!(widen_to_graphemes("", 0..3), 0..0);
        assert_eq!(widen_to_graphemes("abc", 1..1), 1..1);
    }

    #[test]
    fn positions_shift_by_the_ranges_ending_before_them() {
        let text = "0123456789";
        // 2..4 と 6..9 を「##」(2 バイト) に。長さの差は 0 と 1。
        let (r, out) = Redaction::plan(text, 0, [2..4, 6..9], "##").unwrap();
        assert_eq!(out, "01##45##9");
        let at = |p, e| r.remap(p, "##", e);
        assert_eq!(at(0, Edge::Start), 0);
        // 範囲の始まりちょうどは、その範囲でずらさない。
        assert_eq!(at(2, Edge::Start), 2);
        assert_eq!(at(2, Edge::End), 2);
        // 範囲の内側は、始まりなら印の始まりへ、終わりなら印の終わりへ。
        assert_eq!(at(3, Edge::Start), 2);
        assert_eq!(at(3, Edge::End), 4);
        // 範囲の終わり (終わり ≦ p) はずらす。
        assert_eq!(at(4, Edge::Start), 4);
        assert_eq!(at(7, Edge::Start), 6);
        assert_eq!(at(7, Edge::End), 8);
        assert_eq!(at(9, Edge::Start), 8);
        assert_eq!(at(10, Edge::End), 9);
        // span の写し直し: 範囲に掛かる span は印を含むように広がる。
        assert_eq!(r.remap_range(1..3, "##"), 1..4);
        assert_eq!(r.remap_range(4..6, "##"), 4..6);
        assert_eq!(&out[r.remap_range(4..6, "##")], &text[4..6]);
        assert_eq!(r.marker_ranges("##"), [2..4, 6..8]);
    }

    /// 2 つの指示を順に当てたとき、範囲の外の span を写した位置は、伏せ字の後の本文で同じ文字列を指す。
    /// verifies: 04 §7.3 (位置の写し直し。指示が 2 つ以上あれば、指示の ID の順に 1 つずつ当てる)
    #[test]
    fn remapping_two_directives_in_turn_tracks_the_text() {
        let text = "名前は山田で、誕生日は 3 月 14 日、好物はカレーです。";
        let (first, t1) = plan(text, 0, &[span(text, "山田"), span(text, "14 日")]);
        let (second, t2) = plan(&t1, 1, &[span(&t1, "カレー")]);
        for word in ["名前は", "誕生日は", "好物は", "です。", "3 月 "] {
            let s = span(text, word);
            let after_first = first.remap_range(s.clone(), M);
            assert_eq!(&t1[after_first.clone()], word, "first: {word}");
            let after_second = second.remap_range(after_first, M);
            assert_eq!(&t2[after_second], word, "second: {word}");
        }
        // 伏せ字に掛かる span は、最後の本文で印を含む。
        let s = span(text, "山田で");
        let r = second.remap_range(first.remap_range(s, M), M);
        assert_eq!(&t2[r], format!("{M}で"));
        // 写した位置は単調に増える (順序を保つ)。
        let mut last = 0;
        for p in 0..=text.len() {
            if text.is_char_boundary(p) {
                let q = second.remap(first.remap(p, M, Edge::Start), M, Edge::Start);
                assert!(q >= last, "{p} -> {q} < {last}");
                last = q;
            }
        }
    }

    /// 1 回の操作で 2 つの範囲を伏せたときと、同じ範囲を 2 回の操作に分けたときで、伏せ字の後の本文と、範囲の
    /// 外の位置の写し先が同じになる。
    #[test]
    fn combined_and_sequential_redactions_agree() {
        let text = "aa XX bb YY cc";
        let (both, t_both) = plan(text, 0, &[span(text, "XX"), span(text, "YY")]);
        let (one, t1) = plan(text, 0, &[span(text, "XX")]);
        let (two, t2) = plan(&t1, 1, &[span(&t1, "YY")]);
        assert_eq!(t_both, t2);
        for p in [0, 2, 6, 8, 12, 14] {
            assert_eq!(
                both.remap(p, M, Edge::Start),
                two.remap(one.remap(p, M, Edge::Start), M, Edge::Start),
                "position {p}"
            );
        }
    }

    #[test]
    fn display_marks_follow_later_redactions() {
        let text = "aa XX bb YY cc ZZ";
        // 1 つ目: 伏せ字にしないで忘れる (`[非表示]` の範囲)。
        let hidden_bb = [HiddenRange {
            rev: 0,
            range: span(text, "bb"),
        }];
        // 2 つ目: XX を伏せ字に。
        let (r1, t1) = plan(text, 0, &[span(text, "XX")]);
        // 3 つ目: 「YY cc」を伏せ字に (前の `[非表示]` の bb とは重ならない)。
        let (r2, t2) = plan(&t1, 1, &[span(&t1, "YY cc")]);
        let dirs = [
            TurnDirective {
                redaction: None,
                hidden: &hidden_bb,
            },
            TurnDirective {
                redaction: Some(&r1),
                hidden: &[],
            },
            TurnDirective {
                redaction: Some(&r2),
                hidden: &[],
            },
        ];
        let marks = display_marks(&t2, 2, &dirs, M).expect("marks");
        let shown: Vec<(&str, MarkKind)> = marks
            .iter()
            .map(|m| (&t2[m.range.clone()], m.kind))
            .collect();
        assert_eq!(
            shown,
            [
                (M, MarkKind::Redacted),
                ("bb", MarkKind::Hidden),
                (M, MarkKind::Redacted)
            ]
        );
        // 今の番号が合わない (まだ伏せ字になっていない・指示が欠けている) ときは求めない。
        assert_eq!(display_marks(&t1, 1, &dirs, M), None);
        assert_eq!(display_marks(&t2, 3, &dirs, M), None);
    }

    #[test]
    fn a_hidden_range_under_a_later_redaction_hides_the_marker_too() {
        let text = "aa bb cc";
        let hidden = [HiddenRange {
            rev: 0,
            range: span(text, "bb"),
        }];
        // 後の伏せ字が「b c」に掛かる。
        let (r, t1) = plan(text, 0, std::slice::from_ref(&(4..7)));
        let dirs = [
            TurnDirective {
                redaction: None,
                hidden: &hidden,
            },
            TurnDirective {
                redaction: Some(&r),
                hidden: &[],
            },
        ];
        let marks = display_marks(&t1, 1, &dirs, M).expect("marks");
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert_eq!(marks[0].kind, MarkKind::Hidden);
        assert_eq!(&t1[marks[0].range.clone()], format!("b{M}"));
    }

    /// DB から読んだ壊れた位置でも、桁あふれせず、本文の外を指す範囲を返さない。
    #[test]
    fn stored_positions_are_checked_and_arithmetic_saturates() {
        let text = "aa XX bb";
        let (r, t1) = plan(text, 0, &[span(text, "XX")]);
        let stored = Redaction::from_stored(0, 1, [(3, 5)], &r.hash().to_string()).unwrap();
        assert_eq!(stored, r);
        assert_eq!(
            Redaction::from_stored(0, 1, [(3, 5)], "zz"),
            Err(InvalidRedaction::Hash)
        );
        assert_eq!(HiddenRange::from_stored(0, 5, 5), None);
        assert_eq!(HiddenRange::from_stored(0, 6, 2), None);
        // 本文より遥かに大きい位置: 写しても飽和するだけで、印の範囲からは除く。
        let huge =
            Redaction::new(0, 1, std::iter::once(usize::MAX - 2..usize::MAX), r.hash()).unwrap();
        assert_eq!(huge.remap(usize::MAX, M, Edge::End), usize::MAX);
        assert_eq!(huge.remap(usize::MAX - 1, M, Edge::End), usize::MAX);
        let far = [HiddenRange {
            rev: 1,
            range: 100..200,
        }];
        let dirs = [
            TurnDirective {
                redaction: Some(&r),
                hidden: &[],
            },
            TurnDirective {
                redaction: None,
                hidden: &far,
            },
        ];
        let marks = display_marks(&t1, 1, &dirs, M).expect("marks");
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert_eq!(&t1[marks[0].range.clone()], M);
        // 本文がその位置で印になっていない (伏せ字が当たっていない本文) なら、伏せ字の印を返さない。
        let marks = display_marks(&"x".repeat(t1.len()), 1, &dirs, M).expect("marks");
        assert!(marks.is_empty(), "{marks:?}");
    }
}
