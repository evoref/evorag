//! 台帳の掃除 (04 §7.2 の「台帳の掃除」。LLM を呼ばない純粋な関数)。
//!
//! 生きている DB・退避の写し・当て直し・台帳を書く前・`doctor --deep` で、同じ関数を使う。忘れるの指示を ID の順に
//! 1 つずつ当て、何度当てても同じ結果になる (除いた要素はもう無く、位置は改訂の番号で判定する)。

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use evorag_store::forget::Redaction;
use evorag_store::{BodyError, RawValue};
use evorag_types::Id;
use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::keys::substring_keys_of;
use super::set::{DirectiveSet, Forget};
use crate::ledger::{
    ByteRange, LedgerBody, LedgerCorrection, LedgerOutput, LedgerUnit, SentenceSpan,
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

/// 台帳の掃除の入力 (台帳の本体のほか)。
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
    /// 台帳の出典のターン (`sentences`・`refs`・`corr` のターン) の本文。今の本文と、手元にあれば伏せ字の前の本文
    /// (伏せ字と同じトランザクションで掃除するとき)。掃除の 1 と 3 は、要素の改訂の番号のときの本文で判定し、
    /// それが無ければ後の本文で判定する (04 §7.2)。
    pub texts: &'a [RevText<'a>],
}

impl ScrubContext<'_> {
    fn text(&self, turn: Id, rev: u32) -> Option<&str> {
        self.texts
            .iter()
            .find(|t| t.turn == turn && t.rev == rev)
            .map(|t| t.text)
    }

    /// `rev` より後の、最も早い改訂の番号の本文。
    fn later_text(&self, turn: Id, rev: u32) -> Option<(u32, &str)> {
        self.texts
            .iter()
            .filter(|t| t.turn == turn && t.rev > rev)
            .min_by_key(|t| t.rev)
            .map(|t| (t.rev, t.text))
    }
}

/// 掃除で変えたもの。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// 伏せ字の後の位置に写した `sentences`・`refs`・`corr` の要素の数。
    pub remapped: u32,
    /// `inputs` の `input_hash` を null にした。
    pub hash_cleared: bool,
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
        self.removed() > 0 || self.corr > 0 || self.remapped > 0 || self.hash_cleared
    }
}

/// 台帳 `ledger` を掃除する (04 §7.2 の「台帳の掃除」)。忘れるの指示を ID の順に当て、各指示について:
///
/// 1. 単位の候補のうち、`v` か `old` の一致の範囲が、その指示の伏せ字の範囲に掛かるもの (根拠の文の改訂の番号が
///    伏せ字の前の番号のときだけ) を除く。
/// 2. 単位の候補のうち、`v`・`old`・`when`・`a`・`e` (と知らない項目) の語句か部分文字列の値の鍵か照合の鍵が
///    `vks` に当たるものを除く (entity と属性は問わない)。
/// 3. 訂正の候補の表 (`corr`) と判定 (`corrections`) を、同じ `id` の組で除く: 候補の発話が対象のターンにあるか、
///    `old`・`new` の範囲の文字列の値の鍵か照合の鍵が `vks` に当たるもの。
/// 4. `keys` のうち、語句か部分文字列の値の鍵か照合の鍵が `vks` に当たるものを除く。対象のターンを含む台帳と、
///    その指示で 1〜3 の要素を除いた台帳は、`keys` をすべて除く。
///
/// 印を解いた指示は、根拠のターン (単位の候補は根拠の文の、訂正は候補の発話の、`keys` は区切りの最初の往復の発話
/// のターン) が `unblock` の指示の ID より前の要素にだけ当てる。読むときの型に合わない要素と、この版の知らない節の
/// 要素は、中の文字列の語句か部分文字列で当てる。
///
/// その後、その指示の伏せ字の前の改訂の番号の `sentences`・`refs`・`corr` を、伏せ字の後の位置と番号に写す。
/// 除いた `output` の要素の数を `scrubbed` に足し、要素を除いたか対象のターンを含む台帳は `input_hash` を null に
/// する。変えるものが無ければ本体に触れない (バイト列まで同じ)。
///
/// # Errors
/// 残した要素を節の値に書き戻せないとき (起きない)。
pub fn scrub_ledger(ledger: &mut LedgerBody, cx: &ScrubContext<'_>) -> Result<Scrubbed, BodyError> {
    let mut report = Scrubbed::default();
    let forgets = cx.directives.forgets();
    if forgets.is_empty() {
        return Ok(report);
    }
    let mut work = Work::read(&ledger.output);
    let mut corr_removed: BTreeSet<u32> = BTreeSet::new();
    for f in forgets {
        let turns: BTreeSet<Id> = ledger.sources().into_iter().map(|s| s.turn).collect();
        let touches = f.targets().iter().any(|t| turns.contains(t));
        // 知らない節の要素は掃除の 1〜3 ではないので、`keys` をすべて除く条件には数えない。
        work.scrub_others(cx, f);
        let by_f = work.scrub_units(cx, f, &ledger.sentences)
            + work.scrub_corrections(cx, f, ledger, &mut corr_removed);
        let keys_apply = cx.first_turn.is_none_or(|t| f.applies_at(t));
        if keys_apply {
            work.scrub_keys(cx, f, touches || by_f > 0);
        }
        if touches && ledger.inputs.input_hash.is_some() {
            ledger.inputs.input_hash = None;
            report.hash_cleared = true;
        }
        report.remapped += remap_positions(cx.directives, f, ledger);
    }
    let before = ledger.inputs.corr.len();
    ledger.inputs.corr.retain(|c| !corr_removed.contains(&c.id));
    report.corr = count(before - ledger.inputs.corr.len());
    let counts = work.write(&mut ledger.output)?;
    report.keys = counts.keys;
    report.units = counts.units;
    report.corrections = counts.corrections;
    report.other = counts.other;
    if report.removed() > 0 {
        ledger.scrubbed = ledger.scrubbed.saturating_add(report.removed());
        if ledger.inputs.input_hash.take().is_some() {
            report.hash_cleared = true;
        }
    }
    Ok(report)
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

/// 本文 `text` の中の `needle` の一致のすべて (正規化 v1 の一致を先に、照合の形で重ならない一致を足す。書記素の
/// 境界に広げる)。位置を戻せなければ `None`。部分一致で探す (語の途中で切った写しも掛かりとして見る。安全の側)。
fn matches(text: &str, needle: &Needle) -> Option<Vec<Range<usize>>> {
    match Haystack::new(text).find_all(needle, Edges::Any, Widen::ToGraphemes) {
        Placed::Ranges(r) => Some(r),
        Placed::Partial(_) | Placed::Unplaceable => None,
    }
}

/// 掃除の 1: 単位の候補 `unit` の `v` か `old` の一致の範囲が、指示 `f` の伏せ字の範囲に掛かるか。
///
/// 根拠の文 `sent` の改訂の番号が、そのターンの `f` の伏せ字の前の番号のときだけ当てる。伏せ字の前の本文が手元に
/// あればその本文で一致を探す。無ければ (当て直しで、ターンがすでに伏せ字の後の本文になっている) 後の本文に写した
/// 根拠の文に `v` か (空でない) `old` が無ければ掛かったとみなし、後の本文も無ければ根拠の文が伏せ字に掛かれば
/// 掛かったとみなす (安全の側、04 §7.2 の「掃除の補い」)。
fn overlaps_redaction(
    cx: &ScrubContext<'_>,
    f: &Forget,
    sent: &SentenceSpan,
    unit: &LedgerUnit,
) -> bool {
    let red = cx.directives.redactions();
    let Some(r) = red.of(f.id(), sent.turn) else {
        return false;
    };
    if r.rev_before() != sent.rev {
        return false;
    }
    let s_range = to_usize(sent.start)..to_usize(sent.end);
    let redacted = |x: &Range<usize>| r.ranges().iter().any(|y| overlaps(x, y));
    let needles: Vec<Needle> = [unit.value.as_str(), unit.old.as_str()]
        .into_iter()
        .map(Needle::new)
        .filter(|n| !n.is_empty())
        .collect();
    if let Some(text) = cx.text(sent.turn, sent.rev) {
        return needles.iter().any(|n| match matches(text, n) {
            Some(ms) => ms.iter().filter(|m| overlaps(m, &s_range)).any(redacted),
            None => redacted(&s_range),
        });
    }
    if let Some((rev, text)) = cx.later_text(sent.turn, sent.rev)
        && let Some(after) = red.remap(sent.turn, sent.rev, s_range.clone(), rev)
    {
        return needles.iter().any(|n| match matches(text, n) {
            Some(ms) => !ms
                .iter()
                .any(|m| after.start <= m.start && m.end <= after.end),
            None => true,
        });
    }
    redacted(&s_range)
}

/// 掃除の 3: 訂正の候補の `old`・`new` の範囲の文字列の値の鍵か照合の鍵が `f` の `vks` に当たるか。候補の改訂の
/// 番号の本文が無ければ、後の本文に写した範囲の文字列で見る。どちらも無い・範囲が本文の外なら当たったとみなす
/// (確かめられない値の写しを残さない)。
fn corr_hits(cx: &ScrubContext<'_>, f: &Forget, turn: Id, rev: u32, ranges: &[ByteRange]) -> bool {
    let ranges: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| to_usize(r.start)..to_usize(r.end))
        .collect();
    let (text, ranges) = if let Some(text) = cx.text(turn, rev) {
        (text, ranges)
    } else if let Some((later, text)) = cx.later_text(turn, rev)
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
        return true;
    };
    ranges.iter().any(|r| match text.get(r.clone()) {
        Some(s) => whole_hits(cx.key, f, s),
        None => true,
    })
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

/// 値の中のすべての文字列 (map の値・並びの要素を辿る。map の項目の名前は含めない)。
#[derive(Debug, Default)]
struct AllStrings(Vec<String>);

impl<'de> Deserialize<'de> for AllStrings {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StringsVisitor)
    }
}

struct StringsVisitor;

impl<'de> Visitor<'de> for StringsVisitor {
    type Value = AllStrings;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_i128<E: de::Error>(self, _: i128) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_u128<E: de::Error>(self, _: u128) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<AllStrings, E> {
        Ok(AllStrings(vec![v.to_owned()]))
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<AllStrings, E> {
        // 文字列として読めるバイト列は文字列として見る (値の写しを残さない側)。
        Ok(AllStrings(
            std::str::from_utf8(v)
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default(),
        ))
    }

    fn visit_none<E: de::Error>(self) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_unit<E: de::Error>(self) -> Result<AllStrings, E> {
        Ok(AllStrings::default())
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<AllStrings, D::Error> {
        AllStrings::deserialize(d)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<AllStrings, D::Error> {
        AllStrings::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AllStrings, A::Error> {
        let mut out = Vec::new();
        while let Some(AllStrings(x)) = seq.next_element()? {
            out.extend(x);
        }
        Ok(AllStrings(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AllStrings, A::Error> {
        let mut out = Vec::new();
        while let Some((IgnoredAny, AllStrings(x))) = map.next_entry()? {
            out.extend(x);
        }
        Ok(AllStrings(out))
    }
}

/// 要素の中の、値を写しうる文字列。map の要素は、`skip` の項目 (列挙・番号) を除いた項目の文字列。
fn element_strings(raw: &RawValue, skip: &[&str]) -> Vec<String> {
    if let Ok(map) = raw.to::<BTreeMap<String, AllStrings>>() {
        return map
            .into_iter()
            .filter(|(k, _)| !skip.contains(&k.as_str()))
            .flat_map(|(_, v)| v.0)
            .collect();
    }
    raw.to::<AllStrings>().map(|s| s.0).unwrap_or_default()
}

/// 単位の候補の、値を写さない項目 (文番号と列挙)。
const UNIT_SKIP: &[&str] = &["s", "l", "m", "tn"];
/// 訂正の判定の、値を写さない項目。
const CORRECTION_SKIP: &[&str] = &["id", "verdict"];

/// 節の要素 1 つ。
struct Elem<T> {
    raw: RawValue,
    /// 読むときの型で読めたもの (読めなければ `None`)。
    parsed: Option<T>,
    strings: Vec<String>,
    keys: OnceCell<BTreeSet<ValueKey>>,
    removed: bool,
}

impl<T> Elem<T> {
    fn new(raw: RawValue, parsed: Option<T>, strings: Vec<String>) -> Self {
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
        if f.vks().is_empty() || self.strings.is_empty() {
            return false;
        }
        let keys = self
            .keys
            .get_or_init(|| substring_keys_of(key, self.strings.iter().map(String::as_str)));
        f.vks().iter().any(|k| keys.contains(k))
    }
}

/// 節 1 つ。並びでない節は、全体を 1 つの要素として持つ (`whole`)。
struct Sec<T> {
    elems: Vec<Elem<T>>,
    whole: bool,
}

impl<T> Default for Sec<T> {
    fn default() -> Self {
        Self {
            elems: Vec::new(),
            whole: false,
        }
    }
}

impl<T> Sec<T> {
    fn read(
        raw: Option<&RawValue>,
        parse: impl Fn(&RawValue) -> Option<T>,
        strings: impl Fn(&RawValue, Option<&T>) -> Vec<String>,
    ) -> Option<Self> {
        let raw = raw?;
        let Some(elements) = raw.elements() else {
            let s = raw.to::<AllStrings>().map(|s| s.0).unwrap_or_default();
            return Some(Self {
                elems: vec![Elem::new(raw.clone(), None, s)],
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

/// 除いた要素の数 (節ごと)。
#[derive(Default)]
struct Counts {
    keys: u32,
    units: u32,
    corrections: u32,
    other: u32,
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
        let all = |e: &RawValue| element_strings(e, &[]);
        let keys = Sec::read(
            out.raw_section(LedgerOutput::KEYS),
            |e| e.to::<String>().ok(),
            |e, p: Option<&String>| p.map_or_else(|| all(e), |s| vec![s.clone()]),
        );
        let units = Sec::read(
            out.raw_section(LedgerOutput::UNITS),
            |e| e.to::<LedgerUnit>().ok(),
            |e, p: Option<&LedgerUnit>| {
                element_strings(e, if p.is_some() { UNIT_SKIP } else { &[] })
            },
        );
        let corrections = Sec::read(
            out.raw_section(LedgerOutput::CORRECTIONS),
            |e| e.to::<LedgerCorrection>().ok(),
            |e, p: Option<&LedgerCorrection>| {
                element_strings(e, if p.is_some() { CORRECTION_SKIP } else { &[] })
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
                Sec::read(out.raw_section(n), |_| Some(()), |e, _| all(e))
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
    ) -> u32 {
        let Some(units) = &mut self.units else {
            return 0;
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
            let rule1 = || match (&e.parsed, sent) {
                (Some(u), Some(s)) => overlaps_redaction(cx, f, s, u),
                _ => false,
            };
            if e.hits(cx.key, f) || rule1() {
                e.removed = true;
                n += 1;
            }
        }
        n
    }

    /// 掃除の 3。`corr` の要素は `corr_removed` に `id` で集め、判定は同じ `id` の組で除く。除いた数を返す。
    fn scrub_corrections(
        &mut self,
        cx: &ScrubContext<'_>,
        f: &Forget,
        ledger: &LedgerBody,
        corr_removed: &mut BTreeSet<u32>,
    ) -> u32 {
        let mut n = 0;
        let mut ids: BTreeSet<u32> = BTreeSet::new();
        for c in &ledger.inputs.corr {
            if corr_removed.contains(&c.id) || !f.applies_at(c.turn) {
                continue;
            }
            let ranges: Vec<ByteRange> = std::iter::once(c.new).chain(c.old).collect();
            if f.targets().contains(&c.turn) || corr_hits(cx, f, c.turn, c.rev, &ranges) {
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
        n
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
