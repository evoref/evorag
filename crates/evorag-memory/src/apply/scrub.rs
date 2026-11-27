//! 台帳の掃除 (04 §7.2 の「台帳の掃除」。LLM を呼ばない純粋な関数)。
//!
//! 生きている DB・退避の写し・当て直し・台帳を書く前・`doctor --deep` で、同じ関数を使う。忘れるの指示を ID の順に
//! 1 つずつ当て、何度当てても同じ結果になる (除いた要素はもう無く、位置は改訂の番号で判定する)。伏せ字の前の本文が
//! 手元にあるか (操作のとき) と、伏せ字の後の本文しか無いか (当て直し) に依らず、同じ結果になる。

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use evorag_store::forget::{ForgetOutcome, Redaction, apply_forget};
use evorag_store::{BodyError, Envelope, RawValue};
use evorag_types::Id;

use super::keys::SubstringKeys;
use super::set::{DirectiveSet, Forget};
use super::strings::{AllStrings, all_strings, element_strings};
use crate::ledger::{
    ByteRange, LedgerBody, LedgerCorrection, LedgerOutput, LedgerUnit, SentenceSpan, UnknownFields,
};
use crate::value_key::{
    Edges, ForgetKey, Haystack, Needle, Placed, ValueKey, Widen, loose_key, value_key,
};

/// ターンの本文 1 つ (改訂の番号 `rev` のときのもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevText<'a> {
    pub turn: Id,
    pub rev: u32,
    pub text: &'a str,
}

/// 掃除に渡すターンの本文 (ターンごとに、改訂の番号ごとの本文)。
///
/// 台帳の出典のターン (`sentences`・`refs`・`corr` のターン) はすべて渡す。本文を読めない・合わないターンは
/// [`TurnTexts::unreadable`] で「渡したが本文が無い」と記す。渡していないターンを掃除が引くと誤り
/// ([`ScrubError::TurnNotGiven`]) にする (本文の渡し忘れで、要素を黙って除かないため)。
#[derive(Debug, Clone, Default)]
pub struct TurnTexts<'a> {
    turns: BTreeMap<Id, Vec<(u32, &'a str)>>,
}

impl<'a> TurnTexts<'a> {
    /// 空の表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// ターン `turn` の改訂の番号 `rev` の本文を足す。
    pub fn insert(&mut self, turn: Id, rev: u32, text: &'a str) -> &mut Self {
        let e = self.turns.entry(turn).or_default();
        e.retain(|(r, _)| *r != rev);
        e.push((rev, text));
        e.sort_by_key(|(r, _)| *r);
        self
    }

    /// ターン `turn` を渡したが、本文が無い (読めない・合わない) と記す。
    pub fn unreadable(&mut self, turn: Id) -> &mut Self {
        self.turns.entry(turn).or_default();
        self
    }

    fn versions(&self, turn: Id) -> Result<&[(u32, &'a str)], ScrubError> {
        self.turns
            .get(&turn)
            .map(Vec::as_slice)
            .ok_or(ScrubError::TurnNotGiven(turn))
    }

    fn get(&self, turn: Id, rev: u32) -> Result<Option<&'a str>, ScrubError> {
        Ok(self
            .versions(turn)?
            .iter()
            .find(|(r, _)| *r == rev)
            .map(|(_, t)| *t))
    }

    /// `rev` 以後の、最も早い改訂の番号の本文。
    fn from(&self, turn: Id, rev: u32) -> Result<Option<(u32, &'a str)>, ScrubError> {
        Ok(self
            .versions(turn)?
            .iter()
            .find(|(r, _)| *r >= rev)
            .copied())
    }
}

impl<'a> FromIterator<RevText<'a>> for TurnTexts<'a> {
    fn from_iter<I: IntoIterator<Item = RevText<'a>>>(iter: I) -> Self {
        let mut out = Self::new();
        for t in iter {
            out.insert(t.turn, t.rev, t.text);
        }
        out
    }
}

/// 掃除の入力 (台帳の本体のほか)。
#[derive(Debug, Clone, Copy)]
pub struct ScrubContext<'a> {
    /// 値の鍵の HMAC の鍵 (`memory.forget_key`)。
    pub key: &'a ForgetKey,
    /// 効いている指示 (掃除は忘れるの指示だけを当てる。private は台帳を掃除しない、04 §7.2)。台帳を書く前の掃除も、
    /// `forgets_seen` に関わらず、すべての忘れるの指示を渡す (§7.5)。
    pub directives: &'a DirectiveSet,
    /// 区切りの最初の往復の発話のターン (`keys` を印を解いた指示と比べる、04 §7.2)。分からなければ `None`
    /// (印を解いた指示も当て続ける)。
    pub first_turn: Option<Id>,
    /// 台帳の出典のターンの本文。今の本文と、手元にあれば伏せ字の前の本文 (どちらがあっても結果は同じ)。
    pub texts: &'a TurnTexts<'a>,
}

/// 掃除できない理由。
#[derive(Debug, thiserror::Error)]
pub enum ScrubError {
    /// 台帳の出典のターンを [`TurnTexts`] に渡していない (呼び手の誤り)。
    #[error("turn {0} was not given to the scrub")]
    TurnNotGiven(Id),
    #[error(transparent)]
    Body(#[from] BodyError),
}

/// 掃除で変えたもの。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// 除いた `keys` の要素の数。
    pub keys: u32,
    /// 除いた `units` の要素の数。
    pub units: u32,
    /// 除いた `corrections` の要素の数。
    pub corrections: u32,
    /// 除いた、この版の知らない `output` の節の要素の数。
    pub other: u32,
    /// 除いた `inputs` の `corr` の要素の数 (`scrubbed` には数えない、04 §7.2)。
    pub corr: u32,
    /// 除いた知らない項目 (本体・`inputs`・`sentences`・`corr`・`refs` の) の数と、符号に替えた `invalid` の理由の数。
    pub unknown_fields: u32,
    /// 伏せ字の後の位置に写した `sentences`・`refs`・`corr` の要素の数。
    pub remapped: u32,
    /// `inputs` の `input_hash` を null にした。
    pub hash_cleared: bool,
    /// 除いた `keys` の文字列の値の鍵 (区切りの鍵の埋め込みと索引の項目を、台帳の ID と組にして消すため、04 §5.5)。
    pub removed_keys: Vec<ValueKey>,
}

impl Scrubbed {
    /// `scrubbed` に足す数 (`output` の要素の数)。
    #[must_use]
    pub fn removed(&self) -> u32 {
        self.keys + self.units + self.corrections + self.other
    }

    /// 台帳の本体を変えたか。
    #[must_use]
    pub fn changed(&self) -> bool {
        self.removed() > 0
            || self.corr > 0
            || self.unknown_fields > 0
            || self.remapped > 0
            || self.hash_cleared
    }
}

/// `invalid` の理由の文字列を替える固定の符号 (04 §5.2.1。理由に値を書かない)。
const SCRUBBED_REASON: &str = "scrubbed";

/// 台帳 `ledger` を掃除する (04 §7.2 の「台帳の掃除」と「掃除の補い」)。忘れるの指示を ID の順に当て、各指示に
/// ついて:
///
/// 1. 単位の候補のうち、根拠の文の改訂の番号がその指示の伏せ字の前の番号で、根拠の文が伏せ字の範囲に掛かり、
///    伏せ字の後の本文に写した根拠の文に `v` か空でない `old` が無いものを除く。
/// 2. 単位の候補のうち、`v`・`old`・`when`・`a`・`e` と知らない項目の、語句か部分文字列の値の鍵か照合の鍵が
///    `vks` に当たるものを除く (entity と属性は問わない)。
/// 3. 訂正の候補の表 (`corr`) と判定 (`corrections`) を、同じ `id` の組で除く: 候補の発話が対象のターンにあるか、
///    `old`・`new` の範囲の文字列の値の鍵か照合の鍵が `vks` に当たるもの。
/// 4. `keys` のうち、語句か部分文字列の値の鍵か照合の鍵が `vks` に当たるものを除く。対象のターンを含む台帳と、
///    その指示で 1〜3 の要素を除いた台帳は、`keys` をすべて除く。
///
/// 印を解いた指示は、根拠のターン (単位の候補は根拠の文の、訂正は候補の発話の、`keys` は区切りの最初の往復の発話
/// のターン) が `unblock` の指示の ID より前の要素にだけ当てる。読むときの型に合わない要素・この版の知らない節の
/// 要素・知らない項目・`invalid` の理由も、中の文字列の語句か部分文字列で当てる (文字列を集められなければ当たった
/// とみなす)。
///
/// その後、その指示の伏せ字の前の改訂の番号の `sentences`・`refs`・`corr` を、伏せ字の後の位置と番号に写す。
/// 除いた `output` の要素の数を `scrubbed` に足し、要素か `corr` を除いたか対象のターンを含む台帳は `input_hash`
/// を null にする。変えるものが無ければ本体に触れない (バイト列まで同じ)。誤りのときは台帳を変えない。
///
/// # Errors
/// 台帳の出典のターンを `texts` に渡していないとき。残した要素を節の値に書き戻せないとき (起きない)。
pub fn scrub_ledger(
    ledger: &mut LedgerBody,
    cx: &ScrubContext<'_>,
) -> Result<Scrubbed, ScrubError> {
    let mut report = Scrubbed::default();
    let forgets = cx.directives.forgets();
    if forgets.is_empty() {
        return Ok(report);
    }
    let mut next = ledger.clone();
    let mut work = Work::read(&next.output);
    let mut reasons = Reasons::read(&next.output);
    let mut corr_removed: BTreeSet<u32> = BTreeSet::new();
    for f in forgets {
        let turns: BTreeSet<Id> = next.sources().into_iter().map(|s| s.turn).collect();
        let touches = f.targets().iter().any(|t| turns.contains(t));
        // 知らない節・知らない項目・理由は掃除の 1〜3 ではないので、`keys` をすべて除く条件には数えない。
        work.scrub_others(cx, f);
        report.unknown_fields += scrub_unknown_fields(cx, f, &mut next) + reasons.scrub(cx, f);
        let by_f = work.scrub_units(cx, f, &next.sentences)?
            + work.scrub_corrections(cx, f, &next, &mut corr_removed)?;
        if cx.first_turn.is_none_or(|t| f.applies_at(t)) {
            work.scrub_keys(cx, f, touches || by_f > 0);
        }
        if touches && next.inputs.input_hash.is_some() {
            next.inputs.input_hash = None;
            report.hash_cleared = true;
        }
        report.remapped += remap_positions(cx.directives, f, &mut next);
    }
    let before = next.inputs.corr.len();
    next.inputs.corr.retain(|c| !corr_removed.contains(&c.id));
    report.corr = count(before - next.inputs.corr.len());
    let counts = work.write(&mut next.output)?;
    reasons.write(&mut next.output)?;
    report.keys = counts.keys;
    report.units = counts.units;
    report.corrections = counts.corrections;
    report.other = counts.other;
    report.removed_keys = counts.removed_keys(cx.key);
    if report.removed() > 0 || report.corr > 0 {
        next.scrubbed = next.scrubbed.saturating_add(report.removed());
        if next.inputs.input_hash.take().is_some() {
            report.hash_cleared = true;
        }
    }
    if report.changed() {
        *ledger = next;
    }
    Ok(report)
}

/// 生のまま当てる台帳の扱い ([`scan_raw_ledger`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawLedger {
    /// 忘れるの鍵に当たる文字列が無い。
    Keep,
    /// 忘れるの鍵に当たる文字列がある (か、文字列を集められない)。台帳を消す (`quarantine` の台帳と同じ、04 §7.2)。
    Delete,
}

/// 型として読めない台帳 (`LedgerRead::Unreadable`) と、`call` が `segment` 以外の台帳を、本体を生のまま走査する
/// (04 §7.2 の「掃除の補い」)。本体のどれかの文字列 (map の項目の名前を含む) の語句か部分文字列の値の鍵か照合の鍵
/// が、どれかの忘れるの指示の `vks` に当たれば [`RawLedger::Delete`] を返す (印を解いた指示も当てる。台帳の中の
/// 根拠のターンを読めないため)。
#[must_use]
pub fn scan_raw_ledger(key: &ForgetKey, directives: &DirectiveSet, ledger: &Envelope) -> RawLedger {
    let forgets = directives.forgets();
    if forgets.iter().all(|f| f.vks().is_empty()) {
        return RawLedger::Keep;
    }
    let Ok(AllStrings(strings)) = ledger.body_as::<AllStrings>() else {
        return RawLedger::Delete;
    };
    let keys = SubstringKeys::of_all(key, strings.iter().map(String::as_str));
    if forgets.iter().any(|f| keys.hits(f.vks())) {
        RawLedger::Delete
    } else {
        RawLedger::Keep
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn to_usize(p: u32) -> usize {
    usize::try_from(p).unwrap_or(usize::MAX)
}

fn to_u32(p: usize) -> u32 {
    u32::try_from(p).unwrap_or(u32::MAX)
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// 値か照合の形の鍵が `f` の `vks` に当たるか (§7.2 の 3 の範囲の文字列)。
fn whole_hits(key: &ForgetKey, f: &Forget, s: &str) -> bool {
    f.has_key(&value_key(key, s)) || f.has_key(&loose_key(key, s))
}

/// 文字列の並びの語句か部分文字列が `f` の `vks` に当たるか。集められなかった (`None`) ときは当たったとみなす。
fn strings_hit(key: &ForgetKey, f: &Forget, strings: Option<&[String]>) -> bool {
    match strings {
        None => !f.vks().is_empty(),
        Some(s) => {
            !f.vks().is_empty()
                && !s.is_empty()
                && SubstringKeys::of_all(key, s.iter().map(String::as_str)).hits(f.vks())
        }
    }
}

/// 伏せ字 `r` (ターン `turn`) の後の本文。後の本文が手元にあればそれを、無ければ前の本文 (改訂の番号 `rev`) に
/// 伏せ字を当てて作り、どちらも無ければ、より後の本文 (と、その改訂の番号) を返す。作れなければ `None`。
fn text_after<'t>(
    cx: &ScrubContext<'t>,
    turn: Id,
    rev: u32,
    r: &Redaction,
) -> Result<Option<(Cow<'t, str>, u32)>, ScrubError> {
    if let Some(t) = cx.texts.get(turn, r.rev_after())? {
        return Ok(Some((Cow::Borrowed(t), r.rev_after())));
    }
    if let Some(pre) = cx.texts.get(turn, rev)? {
        return Ok(
            match apply_forget(pre, rev, r, cx.directives.redactions().marker()) {
                ForgetOutcome::Applied { text, rev } => Some((Cow::Owned(text), rev)),
                _ => None,
            },
        );
    }
    Ok(cx
        .texts
        .from(turn, r.rev_after())?
        .map(|(rev, t)| (Cow::Borrowed(t), rev)))
}

/// `needle` が、本文 `text` の範囲 `within` の中に一致を持つか (正規化 v1 → 照合の形、部分一致)。位置を戻せない
/// ときは持たないとみなす (除く側)。
fn present_in(text: &str, needle: &Needle, within: &Range<usize>) -> bool {
    match Haystack::new(text).find_all(needle, Edges::Any, Widen::ToGraphemes) {
        Placed::Ranges(ms) => ms
            .iter()
            .any(|m| within.start <= m.start && m.end <= within.end),
        Placed::Partial(_) | Placed::Unplaceable => false,
    }
}

/// 掃除の 1 (04 §7.2 の「掃除の補い」の形): 単位の候補 `unit` を、指示 `f` の伏せ字で除くか。
///
/// 根拠の文 `sent` の改訂の番号がそのターンの `f` の伏せ字の前の番号で、根拠の文が伏せ字の範囲に掛かるときだけ
/// 見る。伏せ字の後の本文 ([`text_after`]) に写した根拠の文に、`v` か空でない `old` の一致が無ければ除く。後の本文を
/// 作れない・写せないときは除く (安全の側)。
fn rule1(
    cx: &ScrubContext<'_>,
    f: &Forget,
    sent: &SentenceSpan,
    unit: &LedgerUnit,
) -> Result<bool, ScrubError> {
    let red = cx.directives.redactions();
    let Some(r) = red.of(f.id(), sent.turn) else {
        return Ok(false);
    };
    let s_range = to_usize(sent.start)..to_usize(sent.end);
    if r.rev_before() != sent.rev || !r.ranges().iter().any(|y| overlaps(&s_range, y)) {
        return Ok(false);
    }
    let needles: Vec<Needle> = [unit.value.as_str(), unit.old.as_str()]
        .into_iter()
        .map(Needle::new)
        .filter(|n| !n.is_empty())
        .collect();
    let Some((text, rev)) = text_after(cx, sent.turn, sent.rev, r)? else {
        return Ok(true);
    };
    let Some(after) = red.remap(sent.turn, sent.rev, s_range, rev) else {
        return Ok(true);
    };
    Ok(needles.iter().any(|n| !present_in(&text, n, &after)))
}

/// 掃除の 3: 訂正の候補の `old`・`new` の範囲の文字列の値の鍵か照合の鍵が `f` の `vks` に当たるか。候補の改訂の
/// 番号の本文が無ければ、後の本文に写した範囲の文字列で見る。どちらも無い・範囲が本文の外なら当たったとみなす
/// (確かめられない値の写しを残さない)。
fn corr_hits(
    cx: &ScrubContext<'_>,
    f: &Forget,
    turn: Id,
    rev: u32,
    ranges: &[ByteRange],
) -> Result<bool, ScrubError> {
    let ranges: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| to_usize(r.start)..to_usize(r.end))
        .collect();
    let (text, ranges) = if let Some(text) = cx.texts.get(turn, rev)? {
        (text, ranges)
    } else if let Some((later, text)) = cx.texts.from(turn, rev)?
        && let Some(mapped) = ranges
            .iter()
            .map(|r| {
                cx.directives
                    .redactions()
                    .remap(turn, rev, r.clone(), later)
            })
            .collect::<Option<Vec<_>>>()
    {
        (text, mapped)
    } else {
        return Ok(true);
    };
    Ok(ranges.iter().any(|r| match text.get(r.clone()) {
        Some(s) => whole_hits(cx.key, f, s),
        None => true,
    }))
}

/// 指示 `f` の伏せ字の前の改訂の番号の位置を、伏せ字の後の位置と番号に写す (§7.3 の「位置の写し直し」)。
fn remap_positions(set: &DirectiveSet, f: &Forget, ledger: &mut LedgerBody) -> u32 {
    let red = set.redactions();
    let marker = red.marker();
    let mut n = 0;
    for s in &mut ledger.sentences {
        if let Some(r) = red.of(f.id(), s.turn)
            && r.rev_before() == s.rev
        {
            let m = r.remap_range(to_usize(s.start)..to_usize(s.end), marker);
            (s.start, s.end, s.rev) = (to_u32(m.start), to_u32(m.end), r.rev_after());
            n += 1;
        }
    }
    for x in &mut ledger.inputs.refs {
        if let Some(r) = red.of(f.id(), x.turn)
            && r.rev_before() == x.rev
        {
            x.rev = r.rev_after();
            n += 1;
        }
    }
    for c in &mut ledger.inputs.corr {
        if let Some(r) = red.of(f.id(), c.turn)
            && r.rev_before() == c.rev
        {
            c.new = remap_bytes(r, c.new, marker);
            c.old = c.old.map(|b| remap_bytes(r, b, marker));
            c.rev = r.rev_after();
            n += 1;
        }
    }
    n
}

/// 範囲 `b` を伏せ字 `r` の後の位置に写す。
fn remap_bytes(r: &Redaction, b: ByteRange, marker: &str) -> ByteRange {
    let m = r.remap_range(to_usize(b.start)..to_usize(b.end), marker);
    ByteRange {
        start: to_u32(m.start),
        end: to_u32(m.end),
    }
}

/// 知らない項目のうち、名前か値の文字列が `f` の `vks` に当たるものを除く。除いた数を返す。
fn scrub_fields(cx: &ScrubContext<'_>, f: &Forget, fields: &mut UnknownFields) -> u32 {
    let before = fields.len();
    fields.retain(|k, v| {
        let strings = all_strings(v).map(|mut s| {
            s.push(k.clone());
            s
        });
        !strings_hit(cx.key, f, strings.as_deref())
    });
    count(before - fields.len())
}

/// 台帳の本体と入れ子の型 (`inputs`・`sentences`・`corr`・`refs`) の知らない項目を掃除する。
fn scrub_unknown_fields(cx: &ScrubContext<'_>, f: &Forget, l: &mut LedgerBody) -> u32 {
    let mut n = scrub_fields(cx, f, &mut l.extra) + scrub_fields(cx, f, &mut l.inputs.extra);
    for s in &mut l.sentences {
        n += scrub_fields(cx, f, &mut s.extra);
    }
    for c in &mut l.inputs.corr {
        n += scrub_fields(cx, f, &mut c.extra);
    }
    for r in &mut l.inputs.refs {
        n += scrub_fields(cx, f, &mut r.extra);
    }
    n
}

/// `invalid` の理由 (節の名前 → 理由の文字列)。読めない形なら、全体を当たったものとして除く。
struct Reasons {
    reasons: Option<BTreeMap<String, String>>,
    present: bool,
    changed: bool,
}

impl Reasons {
    fn read(out: &LedgerOutput) -> Self {
        let reasons = out.invalid_reasons();
        let present = reasons.as_ref().is_none_or(|r| !r.is_empty());
        Self {
            reasons,
            present,
            changed: false,
        }
    }

    /// 理由の文字列が当たれば固定の符号に替える。替えた数を返す。
    fn scrub(&mut self, cx: &ScrubContext<'_>, f: &Forget) -> u32 {
        if !self.present || f.vks().is_empty() {
            return 0;
        }
        let Some(reasons) = &mut self.reasons else {
            // 読めない理由は、すべて除く。
            self.reasons = Some(BTreeMap::new());
            self.changed = true;
            return 1;
        };
        let mut n = 0;
        for (name, reason) in reasons.iter_mut() {
            if reason != SCRUBBED_REASON
                && strings_hit(cx.key, f, Some(&[name.clone(), reason.clone()]))
            {
                SCRUBBED_REASON.clone_into(reason);
                n += 1;
            }
        }
        self.changed |= n > 0;
        n
    }

    fn write(&self, out: &mut LedgerOutput) -> Result<(), BodyError> {
        match (&self.reasons, self.changed) {
            (Some(r), true) => out.replace_invalid_reasons(r),
            _ => Ok(()),
        }
    }
}

/// 単位の候補の項目 (型として読めた要素では、型の項目を必ず見たうえで、ほかの項目を知らない項目として見る)。
const UNIT_FIELDS: &[&str] = &["s", "l", "e", "a", "v", "m", "tn", "old", "when"];
/// 訂正の判定の項目。
const CORRECTION_FIELDS: &[&str] = &["id", "verdict"];

/// 節の要素 1 つ。
struct Elem<T> {
    raw: RawValue,
    /// 読むときの型で読めたもの (読めなければ `None`)。
    parsed: Option<T>,
    /// 値を写しうる文字列 (集められなければ `None`。当たったとみなす)。
    strings: Option<Vec<String>>,
    keys: OnceCell<SubstringKeys>,
    removed: bool,
}

impl<T> Elem<T> {
    fn new(raw: RawValue, parsed: Option<T>, strings: Option<Vec<String>>) -> Self {
        Self {
            raw,
            parsed,
            strings,
            keys: OnceCell::new(),
            removed: false,
        }
    }

    /// 要素の文字列の語句か部分文字列の鍵のどれかが `f` の `vks` に当たるか。
    fn hits(&self, key: &ForgetKey, f: &Forget) -> bool {
        if f.vks().is_empty() {
            return false;
        }
        let Some(strings) = &self.strings else {
            return true;
        };
        self.keys
            .get_or_init(|| SubstringKeys::of_all(key, strings.iter().map(String::as_str)))
            .hits(f.vks())
    }
}

/// 節 1 つ。並びでない節は、全体を 1 つの要素として持つ (`whole`)。
struct Sec<T> {
    elems: Vec<Elem<T>>,
    whole: bool,
}

impl<T> Sec<T> {
    fn read(
        raw: Option<&RawValue>,
        parse: impl Fn(&RawValue) -> Option<T>,
        strings: impl Fn(&RawValue, Option<&T>) -> Option<Vec<String>>,
    ) -> Option<Self> {
        let raw = raw?;
        let Some(elements) = raw.elements() else {
            return Some(Self {
                elems: vec![Elem::new(raw.clone(), None, all_strings(raw))],
                whole: true,
            });
        };
        let elems = elements
            .into_iter()
            .map(|e| {
                let parsed = parse(&e);
                let s = strings(&e, parsed.as_ref());
                Elem::new(e, parsed, s)
            })
            .collect();
        Some(Self {
            elems,
            whole: false,
        })
    }

    fn live(&mut self) -> impl Iterator<Item = &mut Elem<T>> {
        self.elems.iter_mut().filter(|e| !e.removed)
    }

    fn removed(&self) -> u32 {
        count(self.elems.iter().filter(|e| e.removed).count())
    }

    /// 残した要素で節を書き戻す (除いた要素が無ければ触れない)。並びでない節を除いたときは、節を除く。
    fn write(&self, name: &str, out: &mut LedgerOutput) -> Result<(), BodyError> {
        if self.removed() == 0 {
            return Ok(());
        }
        if self.whole {
            out.remove_raw_section(name);
            return Ok(());
        }
        let kept: Vec<&RawValue> = self
            .elems
            .iter()
            .filter(|e| !e.removed)
            .map(|e| &e.raw)
            .collect();
        out.put_raw_section(name, RawValue::from_serialize(&kept)?);
        Ok(())
    }
}

/// 除いた要素の数 (節ごと) と、除いた鍵の文字列。
#[derive(Default)]
struct Counts {
    keys: u32,
    units: u32,
    corrections: u32,
    other: u32,
    removed_keys: Vec<String>,
}

impl Counts {
    fn removed_keys(&self, key: &ForgetKey) -> Vec<ValueKey> {
        let set: BTreeSet<ValueKey> = self
            .removed_keys
            .iter()
            .map(|k| value_key(key, k))
            .collect();
        set.into_iter().collect()
    }
}

/// 型として読めた要素の文字列: 型の項目 `typed` と、`known` に無い項目 (知らない項目) の名前と値。
fn typed_strings(raw: &RawValue, known: &[&str], typed: Vec<String>) -> Option<Vec<String>> {
    element_strings(raw, known).map(|mut rest| {
        rest.extend(typed);
        rest
    })
}

/// 掃除の途中の `output`。
struct Work {
    keys: Option<Sec<String>>,
    units: Option<Sec<LedgerUnit>>,
    corrections: Option<Sec<LedgerCorrection>>,
    /// この版の知らない節 (名前の順)。
    others: Vec<(String, Sec<()>)>,
}

impl Work {
    fn read(out: &LedgerOutput) -> Self {
        let keys = Sec::read(
            out.raw_section(LedgerOutput::KEYS),
            |e| e.to::<String>().ok(),
            |e, p: Option<&String>| p.map_or_else(|| all_strings(e), |s| Some(vec![s.clone()])),
        );
        let units = Sec::read(
            out.raw_section(LedgerOutput::UNITS),
            |e| e.to::<LedgerUnit>().ok(),
            |e, p: Option<&LedgerUnit>| match p {
                Some(u) => typed_strings(
                    e,
                    UNIT_FIELDS,
                    vec![
                        u.value.clone(),
                        u.old.clone(),
                        u.when.clone(),
                        u.aspect.clone(),
                        u.entity.clone(),
                    ],
                ),
                None => all_strings(e),
            },
        );
        let corrections = Sec::read(
            out.raw_section(LedgerOutput::CORRECTIONS),
            |e| e.to::<LedgerCorrection>().ok(),
            |e, p: Option<&LedgerCorrection>| match p {
                Some(_) => typed_strings(e, CORRECTION_FIELDS, Vec::new()),
                None => all_strings(e),
            },
        );
        let known = [
            LedgerOutput::KEYS,
            LedgerOutput::UNITS,
            LedgerOutput::CORRECTIONS,
        ];
        let others = out
            .sections()
            .filter(|n| !known.contains(n))
            .filter_map(|n| {
                Sec::read(out.raw_section(n), |_| Some(()), |e, _| all_strings(e))
                    .map(|s| (n.to_owned(), s))
            })
            .collect();
        Self {
            keys,
            units,
            corrections,
            others,
        }
    }

    /// 掃除の 1・2。除いた数を返す。
    fn scrub_units(
        &mut self,
        cx: &ScrubContext<'_>,
        f: &Forget,
        sentences: &[SentenceSpan],
    ) -> Result<u32, ScrubError> {
        let Some(units) = &mut self.units else {
            return Ok(0);
        };
        let mut n = 0;
        for e in units.live() {
            let sent = e
                .parsed
                .as_ref()
                .and_then(|u| sentences.iter().find(|s| s.s == u.sentence));
            if sent.is_some_and(|s| !f.applies_at(s.turn)) {
                continue;
            }
            let remove = e.hits(cx.key, f)
                || match (&e.parsed, sent) {
                    (Some(u), Some(s)) => rule1(cx, f, s, u)?,
                    _ => false,
                };
            if remove {
                e.removed = true;
                n += 1;
            }
        }
        Ok(n)
    }

    /// 掃除の 3。`corr` の要素は `corr_removed` に `id` で集め、判定は同じ `id` の組で除く。除いた数を返す。
    fn scrub_corrections(
        &mut self,
        cx: &ScrubContext<'_>,
        f: &Forget,
        ledger: &LedgerBody,
        corr_removed: &mut BTreeSet<u32>,
    ) -> Result<u32, ScrubError> {
        let mut n = 0;
        let mut ids: BTreeSet<u32> = BTreeSet::new();
        for c in &ledger.inputs.corr {
            if corr_removed.contains(&c.id) || !f.applies_at(c.turn) {
                continue;
            }
            let ranges: Vec<ByteRange> = std::iter::once(c.new).chain(c.old).collect();
            if f.targets().contains(&c.turn) || corr_hits(cx, f, c.turn, c.rev, &ranges)? {
                ids.insert(c.id);
            }
        }
        if let Some(corrections) = &mut self.corrections {
            let turn_of = |id: u32| {
                ledger
                    .inputs
                    .corr
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.turn)
            };
            for e in corrections.live() {
                let id = e.parsed.map(|c| c.id);
                if id.and_then(turn_of).is_some_and(|t| !f.applies_at(t)) {
                    continue;
                }
                if id.is_some_and(|i| ids.contains(&i) || corr_removed.contains(&i))
                    || e.hits(cx.key, f)
                {
                    e.removed = true;
                    n += 1;
                    ids.extend(id);
                }
            }
        }
        let fresh: Vec<u32> = ids
            .into_iter()
            .filter(|i| ledger.inputs.corr.iter().any(|c| c.id == *i) && !corr_removed.contains(i))
            .collect();
        n += count(fresh.len());
        corr_removed.extend(fresh);
        Ok(n)
    }

    /// 知らない節の要素 (中の文字列で当てる)。
    fn scrub_others(&mut self, cx: &ScrubContext<'_>, f: &Forget) {
        for (_, sec) in &mut self.others {
            for e in sec.live() {
                if e.hits(cx.key, f) {
                    e.removed = true;
                }
            }
        }
    }

    /// 掃除の 4。
    fn scrub_keys(&mut self, cx: &ScrubContext<'_>, f: &Forget, all: bool) {
        let Some(keys) = &mut self.keys else {
            return;
        };
        for e in keys.live() {
            if all || e.hits(cx.key, f) {
                e.removed = true;
            }
        }
    }

    fn write(&self, out: &mut LedgerOutput) -> Result<Counts, BodyError> {
        let mut c = Counts::default();
        if let Some(s) = &self.keys {
            s.write(LedgerOutput::KEYS, out)?;
            c.keys = s.removed();
            c.removed_keys = s
                .elems
                .iter()
                .filter(|e| e.removed)
                .filter_map(|e| e.parsed.clone())
                .collect();
        }
        if let Some(s) = &self.units {
            s.write(LedgerOutput::UNITS, out)?;
            c.units = s.removed();
        }
        if let Some(s) = &self.corrections {
            s.write(LedgerOutput::CORRECTIONS, out)?;
            c.corrections = s.removed();
        }
        for (name, s) in &self.others {
            s.write(name, out)?;
            c.other += s.removed();
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests;
