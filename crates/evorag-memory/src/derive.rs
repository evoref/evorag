//! 導出 (B4) の純粋な関数 (04 §5.3、§5.1・§5.6・§6・§7.4・§7.5。08 §2.6 の M5.3 の「導出と指紋」)。
//!
//! 使う台帳 (区切りごとに 1 つ、`ledger_select::select_ledger`) の候補に関門 (B3) を当て、通った候補・検証済みの
//! 訂正・採る・戻す・`end` を入力の順に 1 本の列にして先頭から畳み、単位を丸ごと作る (**全件の導出を定義に
//! する**。増分の導出は持たない)。I/O・LLM・時計・乱数を持たず、同じ入力から同じ結果を返す。台帳と指示を読む順に
//! 依らない (09 §3.5.1 の導出の決定性)。
//!
//! - [`derive()`]: 導出の本体。関門 1〜7 の結果は、呼び手が持つ [`GateCache`] に覚える (鍵は
//!   [`b3::CacheKey`](crate::b3::CacheKey)。鍵が同じなら結果は同じ)。関門 8 と private は、指示の集合の引き当てで
//!   毎回当てる。
//! - [`fingerprint()`]: 導出の指紋 (04 §5.3)。[`Components::stamp`] は起動時の軽い検査の成分。
//! - [`unit_id`]: 単位の ID (中身から作る、04 §5.1)。
//! - [`import_ends`]: `import-v04` の表 (`import.v04_ended`) で足す `end` (04 §7.4、03 §12.4)。呼び手は返った `end`
//!   を書いて導き直し、無くなるまで繰り返す。
//! - [`dates`]: 日付の表現を解く (04 §5.1 の「日付」)。
//!
//! 畳みの規則は、旧エンジンの `fold.rs` の純粋な規則を写した (写した元は `fold` の各関数の注)。v0.5 で変えた主な
//! ところ: 同じ中身の単位は 1 件 (ID が中身から決まる) なので、置き換えられた値を後で言い直すと同じ単位が状態を
//! 変えて今の値に戻る / 自由記述の属性 (置き換えない) と出来事 (日付ごとに 1 件) を足した / 今の値が無いときの同じ
//! 値の `Contested` は言い直しで `Live` になる (`end` の後) / 長い一度の入力の候補は置き換えの条件を満たさない /
//! 訂正は単位を作らず、同じ位置の新しい値の候補の置き換えの条件になる / `end` を畳みの列に混ぜる / 忘れる・private
//! は `apply` の指示の当て方の述語で当てる。
//!
//! **依存の規則**: このモジュールは、旧エンジンのモジュール (`facts`・`fold`・`correction`・`note`・`extract`・
//! `table`・`forget`・`mask`・`view`・`gates`) に依存しない (旧エンジンは v0.5 の判定の前に外す、08 §2.6)。
//!
//! 旧エンジンの型と取り違えないように、クレートの最上位に型を出さず、`derive::…` で呼ぶ。

mod correct;
pub mod dates;
mod fingerprint;
mod fold;
mod import;

#[cfg(test)]
mod cases;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::ops::Range;

use evorag_types::Id;

pub use correct::CorrectionReason;
pub use fingerprint::{
    Components, DERIVE_VERSION, DirectiveStamp, Fingerprint, LedgerDigest, Stamp, Versions,
    fingerprint, unit_id,
};
pub use import::{NewEnd, import_ends};

use crate::apply::{DirectiveSet, PrivateHit, UnitProbe};
use crate::b3::{self, Aspect, CacheKey, Env, Evidence, Note, Passed, Reason, Utterance};
use crate::ledger_select::Selected;
use crate::prefilter::Prefiltered;
use crate::rules::Cardinality;
use crate::unit::{Anchor, AnchorKind, UnitBody, UnitLayer, UnitState, UnitWhen};
use crate::value_key::NORM_V1;
use dates::{DateRules, Spoken};

/// ターンの話し手。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    /// 利用者の発話 (単位の根拠になる)。
    User,
    /// 応答 (根拠にならない。訂正の関門 4 の「直前の応答」に使う)。
    Assistant,
}

/// 導出に渡すターン 1 つ (今の本文と観測値)。
#[derive(Debug, Clone, Copy)]
pub struct TurnInput<'a> {
    /// セッション (訂正の関門 4 の「同じセッション」)。
    pub session: Id,
    pub speaker: Speaker,
    /// 今の改訂の番号。
    pub rev: u32,
    /// 今の保存した本文。
    pub text: &'a str,
    /// 発話のときの UTC からのずれ (分、05 §3.6 の `tz_offset`。記録の無いターンは `None`)。
    pub tz_offset: Option<i32>,
    /// 長い一度の入力の範囲 (保存した本文の位置、04 §5.2 の前のふるい)。値の位置がこの範囲に掛かる候補は、単値の
    /// 辞書の属性の置き換えの条件を満たさない (§5.3)。
    pub long_inputs: &'a [Range<usize>],
}

/// 導出の入力 (04 §5.3 の「全件の導出を定義にする」)。
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// 区切りごとに使う台帳 (`ledger_select::select_ledger` の結果。順は問わない)。
    pub ledgers: &'a [Selected<'a>],
    /// ターン (ID → 今の本文)。台帳の根拠のターンと、訂正の関門 4 のための同じセッションのターンを含める。
    pub turns: &'a BTreeMap<Id, TurnInput<'a>>,
    /// 指示の集合 (忘れる・private・採る・戻す・`end`)。
    pub directives: &'a DirectiveSet,
    /// 関門の環境 (辞書・規則・鍵)。
    pub env: Env<'a>,
    /// 日付の規則。
    pub dates: &'a DateRules,
}

/// 関門 1〜7 の結果の覚え (04 §5.3)。作り直せる派生の値で、呼び手がプロセスの中で持つ (ディスクには置かない)。
/// [`derive()`] は、その回に使った鍵だけを残す (覚えが増え続けないため)。
#[derive(Debug, Clone, Default)]
pub struct GateCache {
    map: HashMap<CacheKey, Result<Passed, Reason>>,
}

impl GateCache {
    /// 空の覚え。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 覚えている結果の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 何も覚えていないか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 忘れる (規則や鍵を替えたとき。鍵に版と鍵の由来が入るので、替えなくても誤らない)。
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// 畳みで数える理由 (状態の行の知らせと `doctor` の材料、04 §5.3 の「数えた理由」「知らせ」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FoldNote {
    /// 新しい単位を作った。
    Added,
    /// 既にある単位に根拠を足した。
    Merged,
    /// 今の値を置き換えた・終えた・今の値にした。
    Replaced,
    /// 矛盾 (`Contested`) として記録した。
    Contested,
    /// 当て済みの候補 (同じ根拠をすでに持つ)。
    Already,
    /// 否定の候補で、今の値を変えなかった。
    NegationDropped,
    /// `end` で今の値を終えた。
    Ended,
    /// 採る・戻すで今の値にした。
    Adopted,
    /// 採る・戻すの当たる単位が無かった。
    AdoptMissed,
    /// 採る・戻すの値が忘れた値だった。
    AdoptForgotten,
}

impl FoldNote {
    /// 数えるときの名前。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Merged => "merged",
            Self::Replaced => "replaced",
            Self::Contested => "contested",
            Self::Already => "already",
            Self::NegationDropped => "negation_dropped",
            Self::Ended => "ended",
            Self::Adopted => "adopted",
            Self::AdoptMissed => "adopt_missed",
            Self::AdoptForgotten => "adopt_forgotten",
        }
    }
}

impl fmt::Display for FoldNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 導出で数えた件数 (値は持たない、04 §5.3 の「数えた理由」)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// 関門で捨てた候補 (理由ごと。関門 8 を含む)。
    pub gates: BTreeMap<Reason, usize>,
    /// 関門を通ったが別に数える候補 (関門 1〜7 を通ったもの)。
    pub notes: BTreeMap<Note, usize>,
    /// 訂正の関門と効き方。
    pub corrections: BTreeMap<CorrectionReason, usize>,
    /// 畳み。
    pub fold: BTreeMap<FoldNote, usize>,
    /// 読むときの型が合わずに捨てた台帳の要素 (`units`・`corrections` の節)。
    pub mismatched: usize,
}

impl Counts {
    /// 関門の理由の数。
    #[must_use]
    pub fn gate(&self, r: Reason) -> usize {
        self.gates.get(&r).copied().unwrap_or(0)
    }

    /// 畳みの数。
    #[must_use]
    pub fn folded(&self, n: FoldNote) -> usize {
        self.fold.get(&n).copied().unwrap_or(0)
    }

    /// 訂正の数。
    #[must_use]
    pub fn correction(&self, r: CorrectionReason) -> usize {
        self.corrections.get(&r).copied().unwrap_or(0)
    }
}

/// 関門 1〜7 の結果の覚えの当たり。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// 覚えから採った候補の数。
    pub hits: usize,
    /// 関門を当てた候補の数。
    pub misses: usize,
}

/// 導出した単位 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedUnit {
    /// 中身から作った ID (04 §5.1)。
    pub id: Id,
    pub body: UnitBody,
    /// private の印を付けた指示 (ID の最も小さいもの) と理由 (`/memory` の private の理由の表示、04 §7.5)。
    pub private_by: Option<(Id, PrivateHit)>,
}

/// 導出の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// 単位 (ID の順)。
    pub units: Vec<DerivedUnit>,
    pub counts: Counts,
    pub cache: CacheStats,
}

impl Derived {
    /// ID で引く。
    #[must_use]
    pub fn get(&self, id: Id) -> Option<&DerivedUnit> {
        self.units
            .binary_search_by_key(&id, |u| u.id)
            .ok()
            .map(|i| &self.units[i])
    }
}

/// 単位の組 (層, entity, 属性, 辞書か自由記述かの別)。畳みの中の単位の集合は、この組で引く (04 §5.3)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GroupKey {
    pub layer: UnitLayer,
    pub entity: String,
    pub aspect: String,
    pub free: bool,
}

/// 畳みに渡す候補 1 つ (関門 1〜8 を通ったもの)。
#[derive(Debug, Clone)]
pub(crate) struct Cand {
    pub passed: Passed,
    /// 値の位置が長い一度の入力の範囲に掛かる。
    pub long: bool,
    /// 根拠の候補の `when` を解いたもの (出来事で表現が無いか読めなければ発話の日)。
    pub when: Option<UnitWhen>,
    /// 単位の ID。
    pub id: Id,
    pub group: GroupKey,
    /// 辞書の属性の `world` なら、単値か多値か。
    pub card: Option<Cardinality>,
    /// 辞書の属性に写す前の自由記述の属性の鍵。
    pub was_free: Option<String>,
}

impl Cand {
    fn anchor(&self, kind: AnchorKind) -> fold::FoldAnchor {
        let p = &self.passed;
        fold::FoldAnchor {
            anchor: Anchor {
                turn: p.turn,
                rev: p.rev,
                start: p.value.range.start,
                end: p.value.range.end,
                s_start: p.sentence.start,
                s_end: p.sentence.end,
                kind,
                when: self.when.clone(),
            },
            long: self.long,
        }
    }

    /// 根拠 `a` が、この候補と同じ (ターン, 改訂の番号, 値の範囲) か (04 §5.3 の「済んだ候補」)。
    fn same_place(&self, a: &Anchor) -> bool {
        let p = &self.passed;
        a.turn == p.turn
            && a.rev == p.rev
            && a.start == p.value.range.start
            && a.end == p.value.range.end
    }
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn to_usize(p: u32) -> usize {
    usize::try_from(p).unwrap_or(usize::MAX)
}

/// 関門を通った候補を、畳みの候補にする (日付・単位の ID・組)。
fn cand_of(inputs: &Inputs<'_>, passed: Passed) -> Cand {
    let turn = inputs.turns.get(&passed.turn);
    let spoken = Spoken {
        at: passed.turn.timestamp(),
        offset_minutes: turn.and_then(|t| t.tz_offset),
    };
    let value_range = to_usize(passed.value.range.start)..to_usize(passed.value.range.end);
    let long = turn.is_some_and(|t| t.long_inputs.iter().any(|r| overlaps(r, &value_range)));
    let mut when = passed
        .when
        .as_ref()
        .and_then(|w| dates::resolve(inputs.dates, &w.text, spoken));
    let episode = passed.layer == UnitLayer::Episode;
    if episode && when.is_none() {
        when = dates::utterance_day(spoken);
    }
    // 出来事の ID の日付: 解いた日付の始まり、無ければ (ずれの記録の無い相対の表現も) 発話の日 (04 §5.1)。
    let date = episode.then(|| {
        when.as_ref()
            .and_then(|w| w.from.clone())
            .or_else(|| dates::utterance_day(spoken).and_then(|w| w.from))
            .unwrap_or_default()
    });
    let (card, was_free) = match &passed.aspect {
        Aspect::Dict { name, from_free } => (
            (passed.layer == UnitLayer::World)
                .then(|| inputs.env.dictionary.get(name))
                .flatten()
                .map(|d| d.attribute().cardinality),
            from_free.as_ref().map(|f| f.mac.clone()),
        ),
        Aspect::Free(_) => (None, None),
    };
    let group = GroupKey {
        layer: passed.layer,
        entity: passed.entity.clone(),
        aspect: passed.aspect.as_str().to_owned(),
        free: passed.aspect.is_free(),
    };
    let id = unit_id(
        group.layer,
        &group.entity,
        &group.aspect,
        group.free,
        &passed.value.keys.vk.to_hex(),
        date.as_deref(),
    );
    Cand {
        passed,
        long,
        when,
        id,
        group,
        card,
        was_free,
    }
}

/// 訂正の関門 4 で古い値を探す本文: 候補の発話の直前の応答 (同じセッションの、候補の発話より前の最後の応答) と、
/// 同じセッションの候補の発話より前の利用者の発話の、前のふるいの後の本文 (04 §6 の「判定の入力」)。
fn correction_context(inputs: &Inputs<'_>, turn: Id) -> Vec<String> {
    let Some(session) = inputs.turns.get(&turn).map(|t| t.session) else {
        return Vec::new();
    };
    let before = inputs
        .turns
        .range(..turn)
        .filter(|(_, t)| t.session == session);
    let mut answer: Option<&str> = None;
    let mut out = Vec::new();
    for (_, t) in before {
        match t.speaker {
            Speaker::Assistant => answer = Some(t.text),
            Speaker::User => out.push(Prefiltered::from_stored(t.text).text().to_owned()),
        }
    }
    out.extend(answer.map(str::to_owned));
    out
}

/// 全件の導出 (04 §5.3)。
///
/// 流れ: (1) 区切りごとの台帳 (`inputs.ledgers`) の候補に、属性の決め方と関門 1〜7 (覚えを使う) と関門 8 を当てる。
/// (2) 訂正の判定に、畳みの前の関門 (§6 の 1〜3・5 と `forgotten`) を当てる (版の不一致の段の台帳の判定は使わない)。
/// (3) 候補・訂正・採る・戻す・`end` を入力の順に畳む。(4) private の印を付け、単位の本体を作って ID の順に並べる。
#[must_use]
pub fn derive(inputs: &Inputs<'_>, cache: &mut GateCache) -> Derived {
    let mut counts = Counts::default();
    let mut stats = CacheStats::default();
    let mut previous = std::mem::take(&mut cache.map);
    let mut used: HashMap<CacheKey, Result<Passed, Reason>> = HashMap::new();
    let mut cands: Vec<Cand> = Vec::new();
    let mut corrections: Vec<correct::Prepared> = Vec::new();
    // 台帳を ID の順に当てる (結果は順に依らないが、数え方と覚えの当たりもそろえる)。
    let mut ledgers: Vec<&Selected<'_>> = inputs.ledgers.iter().collect();
    ledgers.sort_by_key(|s| s.ledger.id);
    for sel in ledgers {
        let body = sel.ledger.body;
        let mut unmatched: Vec<Id> = Vec::new();
        let mut utterances: Vec<Utterance<'_>> = Vec::new();
        let turns: BTreeSet<Id> = body.sentences.iter().map(|s| s.turn).collect();
        for t in turns {
            let Some(turn) = inputs.turns.get(&t).filter(|t| t.speaker == Speaker::User) else {
                continue;
            };
            match Utterance::with_directives(t, turn.rev, turn.text, inputs.directives) {
                Some(u) => utterances.push(u),
                None => unmatched.push(t),
            }
        }
        let evidence = Evidence::new(&body.sentences, utterances).with_unmatched(unmatched);
        let units = body.output.units();
        counts.mismatched += units.mismatched;
        let first = cands.len();
        for unit in &units.items {
            let key = b3::cache_key(&inputs.env, &evidence, unit);
            let result = if let Some(r) = used.get(&key) {
                stats.hits += 1;
                r.clone()
            } else {
                let r = match previous.remove(&key) {
                    Some(r) => {
                        stats.hits += 1;
                        r
                    }
                    None => {
                        stats.misses += 1;
                        b3::screen(&inputs.env, &evidence, unit)
                    }
                };
                used.insert(key, r.clone());
                r
            };
            match result {
                Err(r) => *counts.gates.entry(r).or_default() += 1,
                Ok(p) => {
                    for n in p.notes() {
                        *counts.notes.entry(n).or_default() += 1;
                    }
                    if b3::forgotten(inputs.directives, &p).is_some() {
                        *counts.gates.entry(Reason::Forgotten).or_default() += 1;
                    } else {
                        cands.push(cand_of(inputs, p));
                    }
                }
            }
        }
        let verdicts = body.output.corrections();
        counts.mismatched += verdicts.mismatched;
        if !sel.stage.uses_corrections() {
            *counts
                .corrections
                .entry(CorrectionReason::StaleLedger)
                .or_default() += verdicts.items.len();
            continue;
        }
        let ledger_cands: Vec<(usize, &Cand)> = cands.iter().enumerate().skip(first).collect();
        for v in &verdicts.items {
            let Some(corr) = body.inputs.corr.iter().find(|c| c.id == v.id) else {
                *counts
                    .corrections
                    .entry(CorrectionReason::UnknownId)
                    .or_default() += 1;
                continue;
            };
            match correct::prepare(inputs, corr, v.verdict, &ledger_cands) {
                Ok(Some(mut p)) => {
                    p.context = correction_context(inputs, p.turn);
                    corrections.push(p);
                }
                Ok(None) => {
                    *counts
                        .corrections
                        .entry(CorrectionReason::Unanchored)
                        .or_default() += 1;
                }
                Err(r) => *counts.corrections.entry(r).or_default() += 1,
            }
        }
    }
    cache.map = used;

    let folded = fold::Folder::new(
        &cands,
        &corrections,
        inputs.directives,
        inputs.env.rules,
        &mut counts,
    )
    .run();
    let mut units: Vec<DerivedUnit> = folded
        .into_iter()
        .map(|u| finish(inputs.directives, u))
        .collect();
    units.sort_by_key(|u| u.id);
    Derived {
        units,
        counts,
        cache: stats,
    }
}

/// 畳んだ単位を本体の形にする: 根拠を (ターン, 値の始まり) の順に並べ、`when` を根拠の順で最初の解けた日付にし
/// (どの根拠でも `from` が無ければ、最初の解いた形)、private の印を付ける (04 §5.1・§7.5)。
fn finish(directives: &DirectiveSet, u: fold::FoldUnit) -> DerivedUnit {
    let private_by = {
        let spans = u.spans();
        let probes = u.probes();
        directives.private_hit(&UnitProbe {
            probe: &probes[0],
            anchors: &spans,
        })
    };
    let mut anchors: Vec<Anchor> = u.anchors.into_iter().map(|a| a.anchor).collect();
    anchors.sort_by_key(|a| (a.turn, a.start, a.end, a.rev, kind_rank(a.kind)));
    anchors.dedup();
    let when = anchors
        .iter()
        .filter_map(|a| a.when.as_ref())
        .find(|w| w.from.is_some())
        .or_else(|| anchors.iter().find_map(|a| a.when.as_ref()))
        .cloned();
    let world = u.group.layer == UnitLayer::World;
    let state = if world { u.state } else { None };
    let body = UnitBody {
        layer: u.group.layer,
        entity: u.group.entity,
        aspect: u.group.aspect,
        free: u.group.free,
        value: u.value,
        vk: u.keys.vk.to_hex(),
        norm: NORM_V1,
        when,
        tense: u.tense,
        state,
        against: (state == Some(UnitState::Contested))
            .then_some(u.against)
            .flatten(),
        by: (state == Some(UnitState::Superseded))
            .then_some(u.by)
            .flatten(),
        state_at: (state == Some(UnitState::Superseded))
            .then_some(u.state_at)
            .flatten(),
        private: private_by.is_some(),
        anchors,
    };
    DerivedUnit {
        id: u.id,
        body,
        private_by,
    }
}

fn kind_rank(k: AnchorKind) -> u8 {
    match k {
        AnchorKind::Said => 0,
        AnchorKind::Ended => 1,
        AnchorKind::Corrected => 2,
    }
}
