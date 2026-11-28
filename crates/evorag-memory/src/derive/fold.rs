//! 畳みの規則 (04 §5.3・§7.4・§7.5)。候補・検証済みの訂正・採る・戻す・`end` を入力の順に 1 本の列にして、先頭から
//! 単位の集合に当てる。単位は (層, entity, 属性, 辞書か自由記述かの別) の組・値の鍵と照合の鍵・根拠のターンの索引で
//! 引く (線形に探さない)。
//!
//! 写した元: 旧エンジンの `fold.rs` (`fold`・`apply_candidate`・`apply_single_current`・`apply_multi_current`・
//! `promote_if_repeated`・`apply_correction`・`apply_choice`・`FactSet::make_live`)。v0.5 で変えたところは、各関数の
//! 注と、モジュールの [`super`] の注に書く。

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

use evorag_types::Id;

use super::correct::{CorrectionReason, OLD_MAX_CHARS, Prepared};
use super::{Cand, Counts, FoldNote, GroupKey};
use crate::apply::{
    AdoptHit, Adoption, DirectiveSet, End, Probe, ProbeAspect, ResolvedEntity, Slot, Timed,
    UnitProbe, ValueKeys,
};
use crate::b3::{Aspect, Mood, Passed, Rules, check_text};
use crate::directive::TurnSpan;
use crate::rules::Cardinality;
use crate::unit::{Anchor, AnchorKind, UnitLayer, UnitState, UnitTense};
use crate::value_key::{Edges, Haystack, Needle, ValueKey, normalize_loose, normalize_v1};

/// 訂正の関門 4 で古い値を探す本文 1 つ (正規化 v1 と照合の形を 1 回だけ作り、含まないものは対応表を作らずに
/// 飛ばす)。
#[derive(Debug, Clone)]
pub(super) struct ContextText {
    text: String,
    v1: String,
    loose: String,
}

impl ContextText {
    pub(super) fn new(text: String) -> Self {
        Self {
            v1: normalize_v1(&text),
            loose: normalize_loose(&text),
            text,
        }
    }

    /// `needle` を語の境界で含むか (04 §5.6 の照合の順)。
    fn contains(&self, needle: &Needle) -> bool {
        (self.v1.contains(&needle.v1) || self.loose.contains(&needle.loose))
            && !Haystack::new(&self.text)
                .find(needle, Edges::Word)
                .is_empty()
    }
}

/// 根拠 1 つと、それが長い一度の入力 (04 §5.2 の前のふるい) の範囲にあるか。
#[derive(Debug, Clone)]
pub(super) struct FoldAnchor {
    pub anchor: Anchor,
    pub long: bool,
}

/// 単位の属性の別。自由記述の属性は必ず属性の鍵を持つ。
#[derive(Debug, Clone)]
pub(super) enum UnitAspect {
    /// 辞書の属性。写す前の自由記述の属性の鍵 (`end`・採る・戻すは、写す前の鍵とも比べる、04 §7.5)。
    Dict { was_free: BTreeSet<String> },
    /// 自由記述の属性の鍵 (16 進)。
    Free { mac: String },
}

/// 畳みの中の単位。
#[derive(Debug, Clone)]
pub(super) struct FoldUnit {
    pub id: Id,
    /// 組の番号 (`Folder::groups` の位置)。
    gid: usize,
    pub group: GroupKey,
    /// 辞書の属性の `world` なら、単値か多値か。
    pub card: Option<Cardinality>,
    pub aspect: UnitAspect,
    pub value: String,
    pub keys: ValueKeys,
    pub tense: UnitTense,
    pub state: Option<UnitState>,
    pub against: Option<Id>,
    pub by: Option<Id>,
    pub state_at: Option<i64>,
    /// 置き換えられた単位が言い直しで `Contested` になったターン。言い直しの数は、このターンより後の別のターンの
    /// 言った根拠だけで数える (旧エンジンが新しい事実を作っていたときと同じ時点で繰り上げる、04 §5.3)。
    count_from: Option<Id>,
    pub anchors: Vec<FoldAnchor>,
}

impl FoldUnit {
    fn is_open(&self) -> bool {
        matches!(self.state, Some(UnitState::Live | UnitState::Contested))
    }

    fn is_live_current(&self) -> bool {
        self.state == Some(UnitState::Live) && self.tense == UnitTense::Current
    }

    /// 言い直された (単値の属性の置き換えの条件の 3 つ目、04 §5.3): 値を言った根拠 (長い一度の入力を除く) が 2 つ
    /// 以上の別のターンにある。言い直しで `Contested` になった単位は、そのターンより後の言った根拠があるとき。
    fn repeated(&self) -> bool {
        let said = self
            .anchors
            .iter()
            .filter(|a| a.anchor.kind == AnchorKind::Said && !a.long)
            .map(|a| a.anchor.turn);
        match self.count_from {
            Some(from) => said.into_iter().any(|t| t > from),
            None => said.collect::<BTreeSet<_>>().len() >= 2,
        }
    }

    /// 根拠の最も新しいターン。
    fn latest_turn(&self) -> Id {
        self.anchors
            .iter()
            .map(|a| a.anchor.turn)
            .max()
            .unwrap_or(self.id)
    }

    /// 根拠の値の範囲 (指示の根拠の位置の当たり、04 §7.5)。
    pub(super) fn spans(&self) -> Vec<TurnSpan> {
        self.anchors
            .iter()
            .map(|a| TurnSpan {
                turn: a.anchor.turn,
                rev: a.anchor.rev,
                start: a.anchor.start,
                end: a.anchor.end,
            })
            .collect()
    }

    /// 指示を当てる相手。辞書の属性は、写す前の自由記述の属性の鍵ごとにも作る (無ければ 1 つ)。最初のものは、写す前の
    /// 鍵を持たない形。
    pub(super) fn probes(&self) -> Vec<Probe<'_>> {
        let entity = || ResolvedEntity::new(&self.group.entity, str::to_owned);
        let turn = self.latest_turn();
        match &self.aspect {
            UnitAspect::Free { mac } => {
                vec![Probe::new(
                    entity(),
                    ProbeAspect::Free { mac },
                    self.keys,
                    turn,
                )]
            }
            UnitAspect::Dict { was_free } => {
                let name = self.group.aspect.as_str();
                std::iter::once(None)
                    .chain(was_free.iter().map(|m| Some(m.as_str())))
                    .map(|was_free| {
                        Probe::new(
                            entity(),
                            ProbeAspect::Dict { name, was_free },
                            self.keys,
                            turn,
                        )
                    })
                    .collect()
            }
        }
    }
}

/// 時制の並び (`current` → `planned` → `past`。値の綴りに依らず置き換えの連鎖にするため、04 §5.3)。
fn tense_rank(t: UnitTense) -> u8 {
    match t {
        UnitTense::Current => 0,
        UnitTense::Planned => 1,
        UnitTense::Past => 2,
    }
}

/// 同じ位置の中の前後 (訂正を候補より先に当てる、04 §5.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum EventKind {
    Correction,
    Candidate,
}

/// 同じ言及 (位置・値・組・時制・叙法・`old` の有無) の候補を、入力の順に依らず並べるための残りの項目。
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Tie {
    /// 正規化した `old`。
    old: String,
    /// `when` の表現。
    when: String,
    rev: u32,
    s_start: u32,
    s_end: u32,
}

/// 入力の順 (04 §5.3): (位置, 値の始まり, 値の終わり, 訂正 → 候補, entity, 属性, 層, 辞書か自由記述か, 時制,
/// 叙法, 正規化した値, `old` の有無, 残りの項目)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Order {
    slot: Slot,
    start: u32,
    end: u32,
    kind: EventKind,
    entity: String,
    aspect: String,
    layer: u8,
    free: bool,
    tense: u8,
    mood: u8,
    value: String,
    has_old: bool,
    tie: Tie,
}

impl Order {
    fn directive(slot: Slot) -> Self {
        Self {
            slot,
            start: 0,
            end: 0,
            kind: EventKind::Correction,
            entity: String::new(),
            aspect: String::new(),
            layer: 0,
            free: false,
            tense: 0,
            mood: 0,
            value: String::new(),
            has_old: false,
            tie: Tie::default(),
        }
    }

    /// 候補の入力の順の鍵。
    pub(super) fn candidate(p: &Passed) -> Self {
        Self {
            slot: Slot::candidates(p.turn),
            start: p.value.range.start,
            end: p.value.range.end,
            kind: EventKind::Candidate,
            entity: p.entity.clone(),
            aspect: p.aspect.as_str().to_owned(),
            layer: u8::from(p.layer == UnitLayer::Episode),
            free: p.aspect.is_free(),
            tense: tense_rank(p.tense),
            mood: u8::from(p.mood == Mood::Negated),
            value: normalize_v1(&p.value.text),
            has_old: p.old.is_some(),
            tie: Tie {
                old: p
                    .old
                    .as_ref()
                    .map(|o| normalize_v1(&o.text))
                    .unwrap_or_default(),
                when: p.when.as_ref().map(|w| w.text.clone()).unwrap_or_default(),
                rev: p.rev,
                s_start: p.sentence.start,
                s_end: p.sentence.end,
            },
        }
    }

    fn correction(c: &Prepared) -> Self {
        Self {
            start: c.new_range.start,
            end: c.new_range.end,
            value: normalize_v1(&c.old.text),
            has_old: true,
            tie: Tie {
                rev: c.rev,
                s_start: c.old.range.start,
                s_end: c.old.range.end,
                ..Tie::default()
            },
            ..Self::directive(Slot::candidates(c.turn))
        }
    }

    /// 同じ言及か (`old` の有無と残りの項目だけが違う)。同じ言及の候補は、`old` が今の値に当たるものを先に当てる。
    fn same_mention(&self, o: &Self) -> bool {
        (
            &self.slot,
            self.start,
            self.end,
            self.kind,
            &self.entity,
            &self.aspect,
            self.layer,
            self.free,
            self.tense,
            self.mood,
            &self.value,
        ) == (
            &o.slot, o.start, o.end, o.kind, &o.entity, &o.aspect, o.layer, o.free, o.tense,
            o.mood, &o.value,
        )
    }
}

/// 列の 1 つ。
#[derive(Debug, Clone, Copy)]
enum Event<'s> {
    Correction(usize),
    Candidate(usize),
    Directive(Timed<'s>),
}

/// 検証済みの訂正が、新しい値の候補に与えた置き換えの許し (04 §5.3 の「単値の辞書の属性」の 2 つ目)。
#[derive(Debug, Clone)]
struct Authorized {
    /// 置き換える今の値の単位。
    target: Id,
    /// 訂正の発話の中で古い値に一致した範囲 (置き換えた単位の根拠、種類 `corrected`)。
    old: Anchor,
}

/// 索引の値に `i` を足す (同じものは 1 つ)。
fn index_push<K: Hash + Eq>(map: &mut HashMap<K, Vec<usize>>, k: K, i: usize) {
    let v = map.entry(k).or_default();
    if v.last() != Some(&i) && !v.contains(&i) {
        v.push(i);
    }
}

/// 畳みの状態。
pub(super) struct Folder<'f> {
    units: Vec<FoldUnit>,
    by_id: HashMap<Id, usize>,
    /// 組 → 組の番号。
    group_of: HashMap<GroupKey, usize>,
    /// 組の番号 → 単位。
    groups: Vec<Vec<usize>>,
    /// 値の鍵と照合の鍵 → 単位。
    by_key: HashMap<ValueKey, Vec<usize>>,
    /// 根拠のターン → 単位。
    by_turn: HashMap<Id, Vec<usize>>,
    authorized: HashMap<usize, Authorized>,
    cands: &'f [Cand],
    corrections: &'f [Prepared],
    /// 訂正の関門 4 で古い値を探す本文 (ターン → 本文)。
    context: &'f HashMap<Id, ContextText>,
    directives: &'f DirectiveSet,
    rules: &'f Rules<'f>,
    counts: &'f mut Counts,
}

impl<'f> Folder<'f> {
    pub(super) fn new(
        cands: &'f [Cand],
        corrections: &'f [Prepared],
        context: &'f HashMap<Id, ContextText>,
        directives: &'f DirectiveSet,
        rules: &'f Rules<'f>,
        counts: &'f mut Counts,
    ) -> Self {
        Self {
            units: Vec::new(),
            by_id: HashMap::new(),
            group_of: HashMap::new(),
            groups: Vec::new(),
            by_key: HashMap::new(),
            by_turn: HashMap::new(),
            authorized: HashMap::new(),
            cands,
            corrections,
            context,
            directives,
            rules,
            counts,
        }
    }

    /// 入力の順に並べて畳む。結果の単位 (並びは作った順)。
    pub(super) fn run(mut self) -> Vec<FoldUnit> {
        let directives = self.directives;
        let timeline = directives.timeline();
        let mut events: Vec<(Order, Event<'_>)> =
            Vec::with_capacity(self.cands.len() + self.corrections.len() + timeline.len());
        events.extend(
            self.cands
                .iter()
                .enumerate()
                .map(|(i, c)| (c.order.clone(), Event::Candidate(i))),
        );
        events.extend(
            self.corrections
                .iter()
                .enumerate()
                .map(|(i, c)| (Order::correction(c), Event::Correction(i))),
        );
        events.extend(
            timeline
                .into_iter()
                .map(|(slot, t)| (Order::directive(slot), Event::Directive(t))),
        );
        events.sort_by(|a, b| a.0.cmp(&b.0));
        let mut k = 0;
        while k < events.len() {
            match events[k].1 {
                Event::Candidate(_) => {
                    let mut j = k + 1;
                    while j < events.len()
                        && matches!(events[j].1, Event::Candidate(_))
                        && events[j].0.same_mention(&events[k].0)
                    {
                        j += 1;
                    }
                    let mut mention: Vec<usize> = events[k..j]
                        .iter()
                        .filter_map(|e| match e.1 {
                            Event::Candidate(i) => Some(i),
                            _ => None,
                        })
                        .collect();
                    // 同じ言及で `old` だけが違う候補は、`old` が今の値に当たるものを先に当てる (並べ替えは安定)。
                    if mention.len() > 1 {
                        mention.sort_by_key(|&ci| !self.old_hits_live(ci));
                    }
                    for ci in mention {
                        self.candidate(ci);
                    }
                    k = j;
                }
                Event::Correction(i) => {
                    self.correction(i);
                    k += 1;
                }
                Event::Directive(Timed::Adopt(a)) => {
                    self.adopt(a);
                    k += 1;
                }
                Event::Directive(Timed::End(e)) => {
                    self.end(e);
                    k += 1;
                }
            }
        }
        // 置き換えに使われなかった許しは、単位を変えなかった検証済みの訂正。
        let unused = self.authorized.len();
        if unused > 0 {
            *self
                .counts
                .corrections
                .entry(CorrectionReason::NoEffect)
                .or_default() += unused;
        }
        self.units
    }

    fn note(&mut self, n: FoldNote) {
        *self.counts.fold.entry(n).or_default() += 1;
    }

    fn count_correction(&mut self, r: CorrectionReason) {
        *self.counts.corrections.entry(r).or_default() += 1;
    }

    /// 組の番号 (無ければ作る)。
    fn gid(&mut self, key: &GroupKey) -> usize {
        if let Some(&g) = self.group_of.get(key) {
            return g;
        }
        let g = self.groups.len();
        self.groups.push(Vec::new());
        self.group_of.insert(key.clone(), g);
        g
    }

    /// 単値の属性の今の値 (時制 `current` の `Live`)。
    fn live_current(&self, gid: usize) -> Option<usize> {
        self.groups[gid]
            .iter()
            .copied()
            .find(|&i| self.units[i].is_live_current())
    }

    /// 値の鍵か照合の鍵が `k` の単位。
    fn with_key(&self, k: &ValueKey) -> &[usize] {
        self.by_key.get(k).map_or(&[], Vec::as_slice)
    }

    /// 候補 `ci` の `old` が、同じ組の今の値に当たるか。
    fn old_hits_live(&self, ci: usize) -> bool {
        let c = &self.cands[ci];
        let Some(old) = &c.passed.old else {
            return false;
        };
        let Some(&g) = self.group_of.get(&c.group) else {
            return false;
        };
        self.with_key(&old.keys.vk).iter().any(|&i| {
            let u = &self.units[i];
            u.gid == g && u.is_live_current() && u.keys.vk == old.keys.vk
        })
    }

    fn push(&mut self, c: &Cand, tense: UnitTense, state: Option<UnitState>) -> usize {
        let p = &c.passed;
        let aspect = match &p.aspect {
            Aspect::Free(f) => UnitAspect::Free { mac: f.mac.clone() },
            Aspect::Dict { .. } => UnitAspect::Dict {
                was_free: c.was_free.iter().cloned().collect(),
            },
        };
        let gid = self.gid(&c.group);
        let i = self.units.len();
        self.units.push(FoldUnit {
            id: c.id,
            gid,
            group: c.group.clone(),
            card: c.card,
            aspect,
            value: p.value.text.clone(),
            keys: p.value.keys,
            tense,
            state,
            against: None,
            by: None,
            state_at: None,
            count_from: None,
            anchors: Vec::new(),
        });
        self.by_id.insert(c.id, i);
        self.groups[gid].push(i);
        index_push(&mut self.by_key, p.value.keys.vk, i);
        index_push(&mut self.by_key, p.value.keys.loose, i);
        self.push_anchor(i, c.anchor(AnchorKind::Said));
        self.note(FoldNote::Added);
        i
    }

    fn push_anchor(&mut self, i: usize, a: FoldAnchor) {
        index_push(&mut self.by_turn, a.anchor.turn, i);
        self.units[i].anchors.push(a);
    }

    fn add_anchor(&mut self, i: usize, c: &Cand, kind: AnchorKind) {
        self.push_anchor(i, c.anchor(kind));
        if let (Some(m), UnitAspect::Dict { was_free }) = (&c.was_free, &mut self.units[i].aspect) {
            was_free.insert(m.clone());
        }
    }

    fn mark_live(&mut self, i: usize) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Live);
        u.against = None;
        u.by = None;
        u.state_at = None;
        u.count_from = None;
    }

    fn supersede(&mut self, i: usize, by: Option<Id>, at: Id) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Superseded);
        u.against = None;
        u.by = by;
        u.state_at = Some(at.timestamp().as_millis());
        u.count_from = None;
    }

    fn contest(&mut self, i: usize, against: Id, count_from: Option<Id>) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Contested);
        u.against = Some(against);
        u.by = None;
        u.state_at = None;
        u.count_from = count_from;
    }

    /// `new` を今の値にする。`old` (今の値) は `Superseded { by: new }` にし、同じ組の `Contested` の `against` を
    /// `new` に付け替える (04 §5.3)。旧エンジンの `FactSet::make_live` から、同じ値の `Contested` を吸い込む処理を
    /// 除いた (v0.5 では同じ値の単位は 1 件なので)。
    fn make_live(&mut self, old: Option<usize>, new: usize, at: Id) {
        let new_id = self.units[new].id;
        if let Some(o) = old
            && o != new
        {
            self.supersede(o, Some(new_id), at);
        }
        self.mark_live(new);
        let g = self.units[new].gid;
        for k in 0..self.groups[g].len() {
            let i = self.groups[g][k];
            if i != new && self.units[i].state == Some(UnitState::Contested) {
                self.units[i].against = Some(new_id);
            }
        }
    }

    /// 今の値 `o` を、候補 `ci` の単位 `n` で置き換える。許し `auth` があれば、訂正の古い値の範囲を `o` の根拠
    /// (`corrected`) に足し、許しを使ったとして数える。
    fn replace(&mut self, ci: usize, o: usize, n: usize, at: Id, auth: Option<&Authorized>) {
        self.make_live(Some(o), n, at);
        self.used_auth(ci, o, auth);
        self.note(FoldNote::Replaced);
    }

    /// 許しで今の値 `o` を置き換えたとき (04 §5.3 の「終えた発言の根拠」)。
    fn used_auth(&mut self, ci: usize, o: usize, auth: Option<&Authorized>) {
        if let Some(a) = auth {
            self.push_anchor(
                o,
                FoldAnchor {
                    anchor: a.old.clone(),
                    long: false,
                },
            );
            self.authorized.remove(&ci);
            self.count_correction(CorrectionReason::Applied);
        }
    }

    /// 単値の属性で、`Contested` の値 `i` が言い直されていれば今の値にする (04 §5.3)。
    fn promote_if_repeated(&mut self, gid: usize, i: usize, at: Id) {
        if self.units[i].state == Some(UnitState::Contested) && self.units[i].repeated() {
            let cur = self.live_current(gid).filter(|&o| o != i);
            self.make_live(cur, i, at);
            self.note(FoldNote::Replaced);
        }
    }

    fn candidate(&mut self, ci: usize) {
        let cands = self.cands;
        let c = &cands[ci];
        // 済んだ候補 (04 §5.3): 同じ単位がすでにその候補の (ターン, 改訂の番号, 値の範囲) を根拠に持っていれば、何も
        // しない。
        let existing = self.by_id.get(&c.id).copied();
        if let Some(i) = existing
            && self.units[i]
                .anchors
                .iter()
                .any(|a| c.same_place(&a.anchor))
        {
            self.note(FoldNote::Already);
            return;
        }
        match (c.passed.layer, c.card) {
            (UnitLayer::Episode, _) => self.episode(c, existing),
            (UnitLayer::World, None) => self.free(c, existing),
            (UnitLayer::World, Some(card)) => {
                let gid = self.gid(&c.group);
                match (c.passed.mood, c.passed.tense) {
                    (Mood::Negated, _) => self.negated(c, existing),
                    (Mood::Fact, UnitTense::Past) => self.past(c, existing),
                    (Mood::Fact, UnitTense::Planned) => self.planned(c, card, gid, existing),
                    (Mood::Fact, UnitTense::Current) => match card {
                        Cardinality::Single => self.single_current(ci, gid, existing),
                        Cardinality::Multi => self.multi_current(ci, gid, existing),
                    },
                }
            }
        }
    }

    /// 出来事 (04 §5.3 の「出来事」): 置き換えない。同じ entity・属性・値の鍵・日付は 1 つの単位 (同じ ID)。
    fn episode(&mut self, c: &Cand, existing: Option<usize>) {
        if let Some(i) = existing {
            self.add_anchor(i, c, AnchorKind::Said);
            self.note(FoldNote::Merged);
        } else {
            self.push(c, c.passed.tense, None);
        }
    }

    /// 自由記述の属性の `world` (04 §5.3 の「自由記述の属性」、v0.5 で足した): 置き換えない。時制 `past` の値は
    /// `Superseded` (`by` なし)、それ以外は `Live`。`past` の後に言い直された値は、同じ単位を `Live` に戻す。長い
    /// 一度の入力の `past` は、`Live` を終えない。
    fn free(&mut self, c: &Cand, existing: Option<usize>) {
        let at = c.passed.turn;
        match (existing, c.passed.tense) {
            (Some(i), UnitTense::Past) => {
                let ends = self.units[i].state == Some(UnitState::Live) && !c.long;
                let kind = if ends {
                    AnchorKind::Ended
                } else {
                    AnchorKind::Said
                };
                self.add_anchor(i, c, kind);
                if ends {
                    self.supersede(i, None, at);
                    self.note(FoldNote::Replaced);
                } else {
                    self.note(FoldNote::Merged);
                }
            }
            (Some(i), tense) => {
                self.add_anchor(i, c, AnchorKind::Said);
                let u = &mut self.units[i];
                if u.state != Some(UnitState::Live) || tense == UnitTense::Current {
                    u.tense = tense;
                }
                self.mark_live(i);
                self.note(FoldNote::Merged);
            }
            (None, UnitTense::Past) => {
                let n = self.push(c, UnitTense::Past, None);
                self.supersede(n, None, at);
            }
            (None, tense) => {
                self.push(c, tense, Some(UnitState::Live));
            }
        }
    }

    /// 否定 (04 §5.3 の「否定」): 値が今の `Live` の値と一致し、根拠の文に変化を示す語があるときだけ、その `Live` を
    /// `Superseded` (`by` なし) にする。それ以外の否定と、長い一度の入力の否定は捨てる。終えた発言を根拠
    /// (`ended`) に足す。
    fn negated(&mut self, c: &Cand, existing: Option<usize>) {
        let hit = existing.filter(|&i| {
            let u = &self.units[i];
            u.state == Some(UnitState::Live) && u.tense != UnitTense::Past
        });
        match hit {
            Some(i) if c.passed.signals.negation_change && !c.long => {
                self.supersede(i, None, c.passed.turn);
                self.add_anchor(i, c, AnchorKind::Ended);
                self.note(FoldNote::Replaced);
            }
            _ => self.note(FoldNote::NegationDropped),
        }
    }

    /// 時制 `past` (04 §5.3 の「時制」): 今の値 (時制 `current` の `Live`) なら終える (置き換えではない。記録の時制は
    /// 変えない)。今の値でなければ、`past` の `Superseded` の記録として履歴に足す。長い一度の入力の候補は、今の値を
    /// 終えない (根拠だけを足す)。
    ///
    /// v0.5 の違い: 同じ値の単位は 1 件なので、`Contested` の同じ値も `Superseded` (`by` なし) にする (言い直しの数で
    /// 今の値に繰り上げないため)。
    fn past(&mut self, c: &Cand, existing: Option<usize>) {
        let at = c.passed.turn;
        match existing {
            Some(i) if self.units[i].is_live_current() && !c.long => {
                self.supersede(i, None, at);
                self.add_anchor(i, c, AnchorKind::Ended);
                self.note(FoldNote::Replaced);
            }
            Some(i) => {
                self.add_anchor(i, c, AnchorKind::Said);
                if self.units[i].state == Some(UnitState::Contested) {
                    self.supersede(i, None, at);
                }
                self.note(FoldNote::Merged);
            }
            None => {
                let n = self.push(c, UnitTense::Past, None);
                self.supersede(n, None, at);
            }
        }
    }

    /// 時制 `planned` (04 §5.3 の「時制」): 単値の属性では今の値とは別の枠に 1 件だけ `Live` で持ち (前の予定は新しい
    /// 予定で置き換える)、多値の属性では並べて持つ。
    ///
    /// v0.5 の違い: 置き換えられた同じ値の単位を、予定として開き直す。
    fn planned(&mut self, c: &Cand, card: Cardinality, gid: usize, existing: Option<usize>) {
        let at = c.passed.turn;
        if let Some(i) = existing
            && self.units[i].is_open()
        {
            self.add_anchor(i, c, AnchorKind::Said);
            self.note(FoldNote::Merged);
            if card == Cardinality::Single {
                self.promote_if_repeated(gid, i, at);
            }
            return;
        }
        let old_plan = if card == Cardinality::Single {
            self.groups[gid].iter().copied().find(|&i| {
                let u = &self.units[i];
                u.state == Some(UnitState::Live) && u.tense == UnitTense::Planned
            })
        } else {
            None
        };
        let n = if let Some(i) = existing {
            self.add_anchor(i, c, AnchorKind::Said);
            self.units[i].tense = UnitTense::Planned;
            self.mark_live(i);
            self.note(FoldNote::Merged);
            i
        } else {
            self.push(c, UnitTense::Planned, Some(UnitState::Live))
        };
        if let Some(o) = old_plan
            && o != n
        {
            let by = self.units[n].id;
            self.supersede(o, Some(by), at);
            self.note(FoldNote::Replaced);
        }
    }

    /// 単値の辞書の属性の `current` (04 §5.3 の「単値の辞書の属性」)。
    ///
    /// v0.5 の違い: 置き換えられた同じ値の単位 (`Superseded`) は、同じ単位を置き換えの条件で今の値に戻すか、
    /// `Contested` にする (言い直しの数はそのターンより後で数える)。今の値が無いときの同じ値の `Contested` は `Live`
    /// にする (`end` の後の言い直し、§7.4)。値の位置が長い一度の入力の範囲にある候補は、置き換えの条件を満たさ
    /// ない。検証済みの訂正の許しは、訂正の位置 ([`Self::correction`]) で候補に付ける。
    fn single_current(&mut self, ci: usize, gid: usize, existing: Option<usize>) {
        let cands = self.cands;
        let c = &cands[ci];
        let at = c.passed.turn;
        let cur = self.live_current(gid).filter(|&o| Some(o) != existing);
        let auth = if c.long {
            None
        } else {
            self.authorized
                .get(&ci)
                .filter(|a| cur.is_some_and(|o| self.units[o].id == a.target))
                .cloned()
        };
        let cond = !c.long
            && (c.passed.signals.explicit_subject || c.passed.signals.change || auth.is_some());
        let Some(s) = existing else {
            let n = self.push(c, UnitTense::Current, Some(UnitState::Live));
            match cur {
                None => self.make_live(None, n, at),
                Some(o) if cond => self.replace(ci, o, n, at, auth.as_ref()),
                Some(o) => {
                    let against = self.units[o].id;
                    self.contest(n, against, None);
                    self.note(FoldNote::Contested);
                }
            }
            return;
        };
        self.add_anchor(s, c, AnchorKind::Said);
        let state = self.units[s].state;
        let tense = self.units[s].tense;
        match state {
            Some(UnitState::Live) if tense == UnitTense::Current => self.note(FoldNote::Merged),
            Some(UnitState::Live) => {
                // 予定の値が実現した: 同じ単位の時制を current にする。
                self.units[s].tense = UnitTense::Current;
                match cur {
                    None => {
                        self.make_live(None, s, at);
                        self.note(FoldNote::Merged);
                    }
                    Some(o) if cond => self.replace(ci, o, s, at, auth.as_ref()),
                    Some(o) => {
                        let against = self.units[o].id;
                        self.contest(s, against, None);
                        self.note(FoldNote::Contested);
                        self.promote_if_repeated(gid, s, at);
                    }
                }
            }
            Some(UnitState::Contested) => match cur {
                None => {
                    self.make_live(None, s, at);
                    self.note(FoldNote::Replaced);
                }
                Some(o) if cond || self.units[s].repeated() => {
                    self.replace(ci, o, s, at, auth.as_ref());
                }
                Some(_) => self.note(FoldNote::Merged),
            },
            Some(UnitState::Superseded) | None => {
                self.units[s].tense = UnitTense::Current;
                match cur {
                    None => {
                        self.make_live(None, s, at);
                        self.note(FoldNote::Replaced);
                    }
                    Some(o) if cond => self.replace(ci, o, s, at, auth.as_ref()),
                    Some(o) => {
                        let against = self.units[o].id;
                        self.contest(s, against, Some(at));
                        self.note(FoldNote::Contested);
                    }
                }
            }
        }
    }

    /// 多値の辞書の属性の `current` (04 §5.3 の「多値の辞書の属性」): 候補の `old` か検証済みの訂正の古い値の値の鍵が、
    /// 今の `Live` の値の 1 件とちょうど一致するときだけ、その 1 件を置き換える。ほかは並べて足す。長い一度の入力の
    /// 候補は置き換えない。
    ///
    /// v0.5 の違い: 置き換えられた同じ値の単位は、同じ単位を `Live` に戻す。新しい値がすでに開いていても、検証済みの
    /// 訂正の許しがあれば古い値を置き換える (旧エンジンの `apply_correction` と同じ)。
    fn multi_current(&mut self, ci: usize, gid: usize, existing: Option<usize>) {
        let cands = self.cands;
        let c = &cands[ci];
        let at = c.passed.turn;
        let live_other = |s: &Self, t: usize| {
            Some(t) != existing && s.units[t].gid == gid && s.units[t].is_live_current()
        };
        let auth = if c.long {
            None
        } else {
            self.authorized.get(&ci).cloned().and_then(|a| {
                let t = *self.by_id.get(&a.target)?;
                live_other(self, t).then_some((t, a))
            })
        };
        if let Some(s) = existing
            && self.units[s].is_open()
        {
            self.add_anchor(s, c, AnchorKind::Said);
            self.units[s].tense = UnitTense::Current;
            if let Some((t, a)) = auth {
                let by = self.units[s].id;
                self.supersede(t, Some(by), at);
                self.used_auth(ci, t, Some(&a));
                self.note(FoldNote::Replaced);
            } else {
                self.note(FoldNote::Merged);
            }
            return;
        }
        let by_old = || -> Option<usize> {
            if c.long {
                return None;
            }
            let old = c.passed.old.as_ref()?;
            let hits: Vec<usize> = self
                .with_key(&old.keys.vk)
                .iter()
                .copied()
                .filter(|&t| live_other(self, t) && self.units[t].keys.vk == old.keys.vk)
                .collect();
            (hits.len() == 1).then(|| hits[0])
        };
        let target = match auth {
            Some((t, a)) => Some((t, Some(a))),
            None => by_old().map(|t| (t, None)),
        };
        let n = if let Some(s) = existing {
            self.add_anchor(s, c, AnchorKind::Said);
            self.units[s].tense = UnitTense::Current;
            self.mark_live(s);
            self.note(FoldNote::Merged);
            s
        } else {
            self.push(c, UnitTense::Current, Some(UnitState::Live))
        };
        if let Some((t, a)) = target
            && t != n
        {
            let by = self.units[n].id;
            self.supersede(t, Some(by), at);
            self.used_auth(ci, t, a.as_ref());
            self.note(FoldNote::Replaced);
        }
    }

    /// 検証済みの訂正 (04 §6 の関門 4 と「単位への効き方」)。古い値が、直前の応答・同じセッションのそれより前の
    /// 利用者の発話に含まれるか、導出のこの時点の今の値 (`world` の `Live` の単位の値) と正規化 v1 か照合の形で同じで、
    /// 関門 6 と 200 文字の上限に通れば、古い値の値の鍵が今の値 (辞書の属性の `world` の、時制 `current` の `Live`)
    /// のちょうど 1 件と一致し、新しい値の候補が同じ (entity, 属性) にあるときだけ、その候補 (入力の順で最初のもの)
    /// に置き換えの許しを付ける。許しを使ったら `applied`、使わなければ `no_effect` と数える。
    ///
    /// 写した元: 旧エンジンの `apply_correction` (v0.5 は訂正が単位を作らず、同じ位置の候補の置き換えの条件になる)。
    fn correction(&mut self, pi: usize) {
        let corrections = self.corrections;
        let p = &corrections[pi];
        let old = &p.old;
        let needle = Needle::new(&old.text);
        let in_live = [old.keys.vk, old.keys.loose].iter().any(|k| {
            self.with_key(k).iter().any(|&i| {
                let u = &self.units[i];
                u.group.layer == UnitLayer::World && u.state == Some(UnitState::Live)
            })
        });
        let found = in_live
            || p.context
                .iter()
                .any(|t| self.context.get(t).is_some_and(|c| c.contains(&needle)));
        if !found
            || check_text(self.rules, &old.text).is_err()
            || old.text.chars().count() > OLD_MAX_CHARS
        {
            self.count_correction(CorrectionReason::OldNotFound);
            return;
        }
        let hits: Vec<usize> = self
            .with_key(&old.keys.vk)
            .iter()
            .copied()
            .filter(|&i| {
                let u = &self.units[i];
                u.card.is_some() && u.is_live_current() && u.keys.vk == old.keys.vk
            })
            .collect();
        let t = match hits[..] {
            [t] => t,
            [] => {
                self.count_correction(CorrectionReason::NoEffect);
                return;
            }
            _ => {
                self.count_correction(CorrectionReason::Ambiguous);
                return;
            }
        };
        let cands = self.cands;
        let target_group = &self.units[t].group;
        let found = p.cands.iter().copied().find(|&ci| {
            let c = &cands[ci];
            c.group == *target_group
                && c.passed.mood == Mood::Fact
                && c.passed.tense == UnitTense::Current
        });
        let Some(ci) = found else {
            self.count_correction(CorrectionReason::NoEffect);
            return;
        };
        let c = &cands[ci];
        let anchor = Anchor {
            turn: p.turn,
            rev: p.rev,
            start: old.range.start,
            end: old.range.end,
            s_start: c.passed.sentence.start,
            s_end: c.passed.sentence.end,
            kind: AnchorKind::Corrected,
            when: None,
        };
        let target = self.units[t].id;
        // 同じ候補への 2 つ目の許しは、前の許しに代わる (前のものは単位を変えなかったとして数える)。
        if self
            .authorized
            .insert(
                ci,
                Authorized {
                    target,
                    old: anchor,
                },
            )
            .is_some()
        {
            self.count_correction(CorrectionReason::NoEffect);
        }
    }

    /// 値の鍵か照合の鍵が `keys` のどれかの単位 (索引で引く。番号の順)。指示の当たりは値の鍵か照合の鍵を求めるので、
    /// 当たりうる単位はこの中にある。
    fn by_keys<'a>(&self, keys: impl IntoIterator<Item = &'a ValueKey>) -> BTreeSet<usize> {
        keys.into_iter()
            .flat_map(|k| self.with_key(k).iter().copied())
            .collect()
    }

    /// 採る・戻す (04 §7.4・§7.5): 指示の ID の時点で、根拠の位置で当たる単位 (同じ (entity, 属性) の単位を先に) を、
    /// 無ければ値の鍵で同じ (entity, 属性) の組に当たる単位を探し、競合の組 (`Contested` か履歴の `Superseded`) に
    /// あるものを今の値にする。忘れた値は戻さない (関門 8 の述語)。
    ///
    /// 写した元: 旧エンジンの `apply_choice` (v0.5 は同じ値の単位が 1 件なので、「戻すの重なり」は起きない。採る・
    /// 戻すのどちらも、`Contested` と `Superseded` の両方を相手にする)。
    fn adopt(&mut self, a: &Adoption) {
        let directives = self.directives;
        let red = directives.redactions();
        let mut pool = self.by_keys(a.keys());
        if let Some(at) = a.at()
            && let Some(v) = self.by_turn.get(&at.turn)
        {
            pool.extend(v.iter().copied());
        }
        let mut best: Option<(AdoptHit, Id, usize)> = None;
        for i in pool {
            let u = &self.units[i];
            if !matches!(u.state, Some(UnitState::Contested | UnitState::Superseded)) {
                continue;
            }
            let spans = u.spans();
            let hit = u.probes().iter().find_map(|p| {
                a.hits(
                    &UnitProbe {
                        probe: p,
                        anchors: &spans,
                    },
                    red,
                )
            });
            if let Some(h) = hit
                && best.is_none_or(|b| (h, u.id) < (b.0, b.1))
            {
                best = Some((h, u.id, i));
            }
        }
        let Some((_, _, t)) = best else {
            self.note(FoldNote::AdoptMissed);
            return;
        };
        if self.units[t]
            .probes()
            .iter()
            .any(|p| directives.forgotten_by(p).is_some())
        {
            self.note(FoldNote::AdoptForgotten);
            return;
        }
        self.units[t].tense = UnitTense::Current;
        if self.units[t].card == Some(Cardinality::Single) {
            let cur = self.live_current(self.units[t].gid).filter(|&o| o != t);
            self.make_live(cur, t, a.id());
        } else {
            self.mark_live(t);
        }
        self.note(FoldNote::Adopted);
    }

    /// `end` (04 §7.4): 終えた位置で、その組の `Live` のうち値の鍵か照合の鍵が `vks` に当たるものだけを `Superseded`
    /// (`by` なし) にする。
    fn end(&mut self, e: &End) {
        let at = e.position();
        let hit: Vec<usize> = self
            .by_keys(e.keys())
            .into_iter()
            .filter(|&i| {
                let u = &self.units[i];
                u.group.layer == UnitLayer::World
                    && u.state == Some(UnitState::Live)
                    && u.probes().iter().any(|p| e.hits(p))
            })
            .collect();
        for i in hit {
            self.supersede(i, None, at);
            self.note(FoldNote::Ended);
        }
    }
}
