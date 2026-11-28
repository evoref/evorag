//! 畳みの規則 (04 §5.3・§7.4・§7.5)。候補・検証済みの訂正・採る・戻す・`end` を入力の順に 1 本の列にして、先頭から
//! 単位の集合に当てる。単位の集合は (層, entity, 属性, 辞書か自由記述かの別) で引く (線形に探さない)。
//!
//! 写した元: 旧エンジンの `fold.rs` (`fold`・`apply_candidate`・`apply_single_current`・`apply_multi_current`・
//! `promote_if_repeated`・`apply_correction`・`apply_choice`・`FactSet::make_live`)。v0.5 で変えたところは、各関数の
//! 注と、モジュールの [`super`] の注に書く。

use std::collections::{BTreeSet, HashMap};

use evorag_types::Id;

use super::correct::{CorrectionReason, OLD_MAX_CHARS, Prepared};
use super::{Cand, Counts, FoldNote, GroupKey};
use crate::apply::{
    AdoptHit, AdoptOp, Adoption, DirectiveSet, End, Probe, ProbeAspect, ResolvedEntity, Slot,
    Timed, ValueKeys,
};
use crate::b3::{Mood, Rules, check_text};
use crate::directive::TurnSpan;
use crate::rules::Cardinality;
use crate::unit::{Anchor, AnchorKind, UnitLayer, UnitState, UnitTense};
use crate::value_key::{Edges, Haystack, Needle, normalize_v1};

/// 根拠 1 つと、それが長い一度の入力 (04 §5.2 の前のふるい) の範囲にあるか。
#[derive(Debug, Clone)]
pub(super) struct FoldAnchor {
    pub anchor: Anchor,
    pub long: bool,
}

/// 畳みの中の単位。
#[derive(Debug, Clone)]
pub(super) struct FoldUnit {
    pub id: Id,
    pub group: GroupKey,
    /// 辞書の属性の `world` なら、単値か多値か。
    pub card: Option<Cardinality>,
    /// 自由記述の属性の鍵 (16 進)。
    pub mac: Option<String>,
    /// 辞書の属性に写す前の自由記述の属性の鍵 (`end`・採る・戻すは、写す前の鍵とも比べる、04 §7.5)。
    pub was_free: BTreeSet<String>,
    pub value: String,
    pub keys: ValueKeys,
    pub tense: UnitTense,
    pub state: Option<UnitState>,
    pub against: Option<Id>,
    pub by: Option<Id>,
    pub state_at: Option<i64>,
    pub anchors: Vec<FoldAnchor>,
}

impl FoldUnit {
    fn is_open(&self) -> bool {
        matches!(self.state, Some(UnitState::Live | UnitState::Contested))
    }

    fn is_live_current(&self) -> bool {
        self.state == Some(UnitState::Live) && self.tense == UnitTense::Current
    }

    /// 言った根拠の別のターンの数 (長い一度の入力の根拠は数えない、04 §5.3)。
    fn said_turns(&self) -> usize {
        self.anchors
            .iter()
            .filter(|a| a.anchor.kind == AnchorKind::Said && !a.long)
            .map(|a| a.anchor.turn)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// 根拠の最も新しいターン。
    pub(super) fn latest_turn(&self) -> Id {
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

    /// 指示を当てる相手。辞書の属性は、写す前の自由記述の属性の鍵ごとにも作る (無ければ 1 つ)。
    pub(super) fn probes(&self) -> Vec<Probe<'_>> {
        let entity = || ResolvedEntity::new(&self.group.entity, str::to_owned);
        let turn = self.latest_turn();
        if self.group.free {
            let mac = self.mac.as_deref().unwrap_or("");
            return vec![Probe::new(
                entity(),
                ProbeAspect::Free { mac },
                self.keys,
                turn,
            )];
        }
        let name = self.group.aspect.as_str();
        let mut out = vec![Probe::new(
            entity(),
            ProbeAspect::Dict {
                name,
                was_free: None,
            },
            self.keys,
            turn,
        )];
        out.extend(self.was_free.iter().map(|m| {
            Probe::new(
                entity(),
                ProbeAspect::Dict {
                    name,
                    was_free: Some(m.as_str()),
                },
                self.keys,
                turn,
            )
        }));
        out
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

/// 列の 1 つ。
#[derive(Debug, Clone, Copy)]
enum Event<'s> {
    Correction(usize),
    Candidate(usize),
    Directive(Timed<'s>),
}

/// 入力の順 (04 §5.3): (位置, 値の始まり, 値の終わり, 訂正 → 候補, entity, 属性, 層, 辞書か自由記述か, 時制,
/// 叙法, 正規化した値, `old` の有無, 残りの項目)。最後の項目は、同じ位置の同じ候補を、入力の順に依らず並べるため。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    slot: Slot,
    start: u32,
    end: u32,
    kind: u8,
    entity: String,
    aspect: String,
    layer: u8,
    free: bool,
    tense: u8,
    mood: u8,
    value: String,
    has_old: bool,
    rest: (String, String, u32, u32, u32),
}

impl Order {
    fn directive(slot: Slot) -> Self {
        Self {
            slot,
            start: 0,
            end: 0,
            kind: 0,
            entity: String::new(),
            aspect: String::new(),
            layer: 0,
            free: false,
            tense: 0,
            mood: 0,
            value: String::new(),
            has_old: false,
            rest: (String::new(), String::new(), 0, 0, 0),
        }
    }

    fn candidate(c: &Cand) -> Self {
        let p = &c.passed;
        Self {
            slot: Slot::candidates(p.turn),
            start: p.value.range.start,
            end: p.value.range.end,
            kind: 1,
            entity: p.entity.clone(),
            aspect: p.aspect.as_str().to_owned(),
            layer: u8::from(p.layer == UnitLayer::Episode),
            free: p.aspect.is_free(),
            tense: tense_rank(p.tense),
            mood: u8::from(p.mood == Mood::Negated),
            value: normalize_v1(&p.value.text),
            has_old: p.old.is_some(),
            rest: (
                p.old
                    .as_ref()
                    .map(|o| normalize_v1(&o.text))
                    .unwrap_or_default(),
                p.when.as_ref().map(|w| w.text.clone()).unwrap_or_default(),
                p.rev,
                p.sentence.start,
                p.sentence.end,
            ),
        }
    }

    fn correction(c: &Prepared) -> Self {
        Self {
            slot: Slot::candidates(c.turn),
            start: c.new_range.start,
            end: c.new_range.end,
            kind: 0,
            value: normalize_v1(&c.old.text),
            has_old: true,
            rest: (
                String::new(),
                String::new(),
                c.rev,
                c.old.range.start,
                c.old.range.end,
            ),
            ..Self::directive(Slot::candidates(c.turn))
        }
    }
}

/// 検証済みの訂正が、新しい値の候補に与えた置き換えの許し (04 §5.3 の「単値の辞書の属性」の 2 つ目)。
#[derive(Debug, Clone)]
struct Authorized {
    /// 置き換える今の値の単位。
    target: Id,
    /// 訂正の発話の中で古い値に一致した範囲 (置き換えた単位の根拠、種類 `corrected`)。
    old: Anchor,
}

/// 畳みの状態。
pub(super) struct Folder<'f> {
    units: Vec<FoldUnit>,
    by_id: HashMap<Id, usize>,
    groups: HashMap<GroupKey, Vec<usize>>,
    authorized: HashMap<usize, Authorized>,
    cands: &'f [Cand],
    corrections: &'f [Prepared],
    directives: &'f DirectiveSet,
    rules: &'f Rules<'f>,
    counts: &'f mut Counts,
}

impl<'f> Folder<'f> {
    pub(super) fn new(
        cands: &'f [Cand],
        corrections: &'f [Prepared],
        directives: &'f DirectiveSet,
        rules: &'f Rules<'f>,
        counts: &'f mut Counts,
    ) -> Self {
        Self {
            units: Vec::new(),
            by_id: HashMap::new(),
            groups: HashMap::new(),
            authorized: HashMap::new(),
            cands,
            corrections,
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
                .map(|(i, c)| (Order::candidate(c), Event::Candidate(i))),
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
        for (_, e) in events {
            match e {
                Event::Candidate(i) => self.candidate(i),
                Event::Correction(i) => self.correction(i),
                Event::Directive(Timed::Adopt(a)) => self.adopt(a),
                Event::Directive(Timed::End(e)) => self.end(e),
            }
        }
        self.units
    }

    fn note(&mut self, n: FoldNote) {
        *self.counts.fold.entry(n).or_default() += 1;
    }

    fn group(&self, key: &GroupKey) -> Vec<usize> {
        self.groups.get(key).cloned().unwrap_or_default()
    }

    /// 単値の属性の今の値 (時制 `current` の `Live`)。
    fn live_current(&self, group: &[usize]) -> Option<usize> {
        group
            .iter()
            .copied()
            .find(|&i| self.units[i].is_live_current())
    }

    fn push(&mut self, c: &Cand, tense: UnitTense, state: Option<UnitState>) -> usize {
        let p = &c.passed;
        let mac = match &p.aspect {
            crate::b3::Aspect::Free(f) => Some(f.mac.clone()),
            crate::b3::Aspect::Dict { .. } => None,
        };
        let i = self.units.len();
        self.units.push(FoldUnit {
            id: c.id,
            group: c.group.clone(),
            card: c.card,
            mac,
            was_free: c.was_free.iter().cloned().collect(),
            value: p.value.text.clone(),
            keys: p.value.keys,
            tense,
            state,
            against: None,
            by: None,
            state_at: None,
            anchors: vec![c.anchor(AnchorKind::Said)],
        });
        self.by_id.insert(c.id, i);
        self.groups.entry(c.group.clone()).or_default().push(i);
        self.note(FoldNote::Added);
        i
    }

    fn add_anchor(&mut self, i: usize, c: &Cand, kind: AnchorKind) {
        let u = &mut self.units[i];
        u.anchors.push(c.anchor(kind));
        if let Some(m) = &c.was_free {
            u.was_free.insert(m.clone());
        }
    }

    fn mark_live(&mut self, i: usize) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Live);
        u.against = None;
        u.by = None;
        u.state_at = None;
    }

    fn supersede(&mut self, i: usize, by: Option<Id>, at: Id) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Superseded);
        u.against = None;
        u.by = by;
        u.state_at = Some(at.timestamp().as_millis());
    }

    fn contest(&mut self, i: usize, against: Id) {
        let u = &mut self.units[i];
        u.state = Some(UnitState::Contested);
        u.against = Some(against);
        u.by = None;
        u.state_at = None;
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
        for i in self.group(&self.units[new].group.clone()) {
            if i != new && self.units[i].state == Some(UnitState::Contested) {
                self.units[i].against = Some(new_id);
            }
        }
    }

    /// 単値の属性で、`Contested` の値 `i` の言った根拠が 2 つ以上の別のターンになっていれば、今の値にする (04 §5.3)。
    fn promote_if_repeated(&mut self, group: &[usize], i: usize, at: Id) {
        if self.units[i].state == Some(UnitState::Contested) && self.units[i].said_turns() >= 2 {
            let cur = self.live_current(group).filter(|&o| o != i);
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
                let group = self.group(&c.group);
                match (c.passed.mood, c.passed.tense) {
                    (Mood::Negated, _) => self.negated(c, existing),
                    (Mood::Fact, UnitTense::Past) => self.past(c, existing),
                    (Mood::Fact, UnitTense::Planned) => self.planned(c, card, &group, existing),
                    (Mood::Fact, UnitTense::Current) => match card {
                        Cardinality::Single => self.single_current(ci, &group, existing),
                        Cardinality::Multi => self.multi_current(ci, &group, existing),
                    },
                }
            }
        }
    }

    /// 出来事 (04 §5.3 の「出来事」): 置き換えない。同じ entity・属性・値の鍵・日付は 1 つの単位 (同じ ID)。
    fn episode(&mut self, c: &Cand, existing: Option<usize>) {
        match existing {
            Some(i) => {
                self.add_anchor(i, c, AnchorKind::Said);
                self.note(FoldNote::Merged);
            }
            None => {
                self.push(c, c.passed.tense, None);
            }
        }
    }

    /// 自由記述の属性の `world` (04 §5.3 の「自由記述の属性」、v0.5 で足した): 置き換えない。時制 `past` の値は
    /// `Superseded` (`by` なし)、それ以外は `Live`。
    fn free(&mut self, c: &Cand, existing: Option<usize>) {
        let at = c.passed.turn;
        match (existing, c.passed.tense) {
            (Some(i), UnitTense::Past) => {
                let live = self.units[i].state == Some(UnitState::Live);
                self.add_anchor(
                    i,
                    c,
                    if live {
                        AnchorKind::Ended
                    } else {
                        AnchorKind::Said
                    },
                );
                if live {
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
    /// `Superseded` (`by` なし) にする。それ以外の否定は捨てる。終えた発言を根拠 (`ended`) に足す。
    fn negated(&mut self, c: &Cand, existing: Option<usize>) {
        let hit = existing.filter(|&i| {
            let u = &self.units[i];
            u.state == Some(UnitState::Live) && u.tense != UnitTense::Past
        });
        match hit {
            Some(i) if c.passed.signals.negation_change => {
                self.supersede(i, None, c.passed.turn);
                self.add_anchor(i, c, AnchorKind::Ended);
                self.note(FoldNote::Replaced);
            }
            _ => self.note(FoldNote::NegationDropped),
        }
    }

    /// 時制 `past` (04 §5.3 の「時制」): 今の値 (時制 `current` の `Live`) なら終える (置き換えではない。記録の時制は
    /// 変えない)。今の値でなければ、`past` の `Superseded` の記録として履歴に足す。
    ///
    /// v0.5 の違い: 同じ値の単位は 1 件なので、`Contested` の同じ値も `Superseded` (`by` なし) にする (言い直しの数で
    /// 今の値に繰り上げないため)。
    fn past(&mut self, c: &Cand, existing: Option<usize>) {
        let at = c.passed.turn;
        match existing {
            Some(i) if self.units[i].is_live_current() => {
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
    fn planned(&mut self, c: &Cand, card: Cardinality, group: &[usize], existing: Option<usize>) {
        let at = c.passed.turn;
        if let Some(i) = existing
            && self.units[i].is_open()
        {
            self.add_anchor(i, c, AnchorKind::Said);
            self.note(FoldNote::Merged);
            if card == Cardinality::Single {
                self.promote_if_repeated(group, i, at);
            }
            return;
        }
        let old_plan = (card == Cardinality::Single)
            .then(|| {
                group.iter().copied().find(|&i| {
                    let u = &self.units[i];
                    u.state == Some(UnitState::Live) && u.tense == UnitTense::Planned
                })
            })
            .flatten();
        let n = match existing {
            // v0.5 の違い: 置き換えられた同じ値の単位を、予定として開き直す。
            Some(i) => {
                self.add_anchor(i, c, AnchorKind::Said);
                self.units[i].tense = UnitTense::Planned;
                self.mark_live(i);
                self.note(FoldNote::Merged);
                i
            }
            None => self.push(c, UnitTense::Planned, Some(UnitState::Live)),
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
    /// `Contested` にする。今の値が無いときの同じ値の `Contested` は `Live` にする (`end` の後の言い直し、§7.4)。値の
    /// 位置が長い一度の入力の範囲にある候補は、置き換えの条件を満たさない。検証済みの訂正の許しは、訂正の位置
    /// ([`Self::correction`]) で候補に付ける。
    fn single_current(&mut self, ci: usize, group: &[usize], existing: Option<usize>) {
        let cands = self.cands;
        let c = &cands[ci];
        let at = c.passed.turn;
        let cur = self.live_current(group).filter(|&o| Some(o) != existing);
        let auth = self
            .authorized
            .get(&ci)
            .filter(|a| cur.is_some_and(|o| self.units[o].id == a.target))
            .cloned();
        let cond = !c.long
            && (c.passed.signals.explicit_subject || c.passed.signals.change || auth.is_some());
        let replace = |s: &mut Self, o: usize, n: usize| {
            s.make_live(Some(o), n, at);
            if let Some(a) = &auth {
                s.units[o].anchors.push(FoldAnchor {
                    anchor: a.old.clone(),
                    long: false,
                });
            }
            s.note(FoldNote::Replaced);
        };
        let Some(s) = existing else {
            let n = self.push(c, UnitTense::Current, Some(UnitState::Live));
            match cur {
                None => self.make_live(None, n, at),
                Some(o) if cond => replace(self, o, n),
                Some(o) => {
                    let against = self.units[o].id;
                    self.contest(n, against);
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
                    Some(o) if cond => replace(self, o, s),
                    Some(o) => {
                        let against = self.units[o].id;
                        self.contest(s, against);
                        self.note(FoldNote::Contested);
                        self.promote_if_repeated(group, s, at);
                    }
                }
            }
            Some(UnitState::Contested) => match cur {
                None => {
                    self.make_live(None, s, at);
                    self.note(FoldNote::Replaced);
                }
                Some(o) if cond || self.units[s].said_turns() >= 2 => replace(self, o, s),
                Some(_) => self.note(FoldNote::Merged),
            },
            Some(UnitState::Superseded) | None => {
                self.units[s].tense = UnitTense::Current;
                match cur {
                    None => {
                        self.make_live(None, s, at);
                        self.note(FoldNote::Replaced);
                    }
                    Some(o) if cond => replace(self, o, s),
                    Some(o) => {
                        let against = self.units[o].id;
                        self.contest(s, against);
                        self.note(FoldNote::Contested);
                    }
                }
            }
        }
    }

    /// 多値の辞書の属性の `current` (04 §5.3 の「多値の辞書の属性」): 候補の `old` か検証済みの訂正の古い値の値の鍵が、
    /// 今の `Live` の値の 1 件とちょうど一致するときだけ、その 1 件を置き換える。ほかは並べて足す。
    ///
    /// v0.5 の違い: 置き換えられた同じ値の単位は、同じ単位を `Live` に戻す。
    fn multi_current(&mut self, ci: usize, group: &[usize], existing: Option<usize>) {
        let cands = self.cands;
        let c = &cands[ci];
        let at = c.passed.turn;
        if let Some(s) = existing
            && self.units[s].is_open()
        {
            self.add_anchor(s, c, AnchorKind::Said);
            self.units[s].tense = UnitTense::Current;
            self.note(FoldNote::Merged);
            return;
        }
        let auth = self.authorized.get(&ci).cloned().and_then(|a| {
            let t = *self.by_id.get(&a.target)?;
            (group.contains(&t) && self.units[t].is_live_current()).then_some((t, Some(a.old)))
        });
        let by_old = || {
            let old = c.passed.old.as_ref()?;
            let hits: Vec<usize> = group
                .iter()
                .copied()
                .filter(|&i| {
                    Some(i) != existing
                        && self.units[i].is_live_current()
                        && self.units[i].keys.vk == old.keys.vk
                })
                .collect();
            (hits.len() == 1).then(|| (hits[0], None))
        };
        let target = auth.or_else(by_old);
        let n = match existing {
            Some(s) => {
                self.add_anchor(s, c, AnchorKind::Said);
                self.units[s].tense = UnitTense::Current;
                self.mark_live(s);
                self.note(FoldNote::Merged);
                s
            }
            None => self.push(c, UnitTense::Current, Some(UnitState::Live)),
        };
        if let Some((t, old)) = target
            && t != n
        {
            let by = self.units[n].id;
            self.supersede(t, Some(by), at);
            if let Some(old) = old {
                self.units[t].anchors.push(FoldAnchor {
                    anchor: old,
                    long: false,
                });
            }
            self.note(FoldNote::Replaced);
        }
    }

    fn count_correction(&mut self, r: CorrectionReason) {
        *self.counts.corrections.entry(r).or_default() += 1;
    }

    /// 検証済みの訂正 (04 §6 の関門 4 と「単位への効き方」)。古い値が、直前の応答・同じセッションのそれより前の
    /// 利用者の発話・導出のこの時点の今の値 (`Live` の単位の値) のどれかに含まれ、関門 6 と 200 文字の上限に通れば、
    /// 古い値の値の鍵が今の値 (辞書の属性の `world` の、時制 `current` の `Live`) のちょうど 1 件と一致し、新しい値の
    /// 候補が同じ (entity, 属性) にあるときだけ、その候補に置き換えの許しを付ける。
    ///
    /// 写した元: 旧エンジンの `apply_correction` (v0.5 は訂正が単位を作らず、同じ位置の候補の置き換えの条件になる)。
    fn correction(&mut self, pi: usize) {
        let corrections = self.corrections;
        let p = &corrections[pi];
        let needle = Needle::new(&p.old.text);
        let contains = |text: &str| !Haystack::new(text).find(&needle, Edges::Word).is_empty();
        let found = p.context.iter().any(|t| contains(t))
            || self
                .units
                .iter()
                .any(|u| u.state == Some(UnitState::Live) && contains(&u.value));
        if !found
            || check_text(self.rules, &p.old.text).is_err()
            || p.old.text.chars().count() > OLD_MAX_CHARS
        {
            self.count_correction(CorrectionReason::OldNotFound);
            return;
        }
        let hits: Vec<usize> = (0..self.units.len())
            .filter(|&i| {
                let u = &self.units[i];
                u.card.is_some() && u.is_live_current() && u.keys.vk == p.old.keys.vk
            })
            .collect();
        let t = match hits[..] {
            [t] => t,
            [] => return self.count_correction(CorrectionReason::NoEffect),
            _ => return self.count_correction(CorrectionReason::Ambiguous),
        };
        let cands = self.cands;
        let target_group = &self.units[t].group;
        let Some(&ci) = p.cands.iter().find(|&&ci| {
            let c = &cands[ci];
            c.group == *target_group
                && c.passed.mood == Mood::Fact
                && c.passed.tense == UnitTense::Current
        }) else {
            return self.count_correction(CorrectionReason::NoEffect);
        };
        let c = &cands[ci];
        let old = Anchor {
            turn: p.turn,
            rev: p.rev,
            start: p.old.range.start,
            end: p.old.range.end,
            s_start: c.passed.sentence.start,
            s_end: c.passed.sentence.end,
            kind: AnchorKind::Corrected,
            when: None,
        };
        self.authorized.insert(
            ci,
            Authorized {
                target: self.units[t].id,
                old,
            },
        );
        self.count_correction(CorrectionReason::Applied);
    }

    /// 採る・戻す (04 §7.4・§7.5): 指示の ID の時点で、根拠の位置で当たる単位を、無ければ値の鍵で同じ (entity, 属性)
    /// の組に当たる単位を探し、採るなら `Contested`、戻すなら履歴 (`Superseded`) のものを今の値にする。忘れた値は
    /// 戻さない (関門 8 の述語)。
    ///
    /// 写した元: 旧エンジンの `apply_choice` (v0.5 は同じ値の単位が 1 件なので、「戻すの重なり」は起きない)。
    fn adopt(&mut self, a: &Adoption) {
        let directives = self.directives;
        let red = directives.redactions();
        let want = match a.op() {
            AdoptOp::Accept => UnitState::Contested,
            AdoptOp::Revert => UnitState::Superseded,
        };
        let mut best: Option<(AdoptHit, Id, usize)> = None;
        for (i, u) in self.units.iter().enumerate() {
            if u.state != Some(want) {
                continue;
            }
            let spans = u.spans();
            let hit = u.probes().iter().find_map(|p| {
                a.hits(
                    &crate::apply::UnitProbe {
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
            return self.note(FoldNote::AdoptMissed);
        };
        if self.units[t]
            .probes()
            .iter()
            .any(|p| directives.forgotten_by(p).is_some())
        {
            return self.note(FoldNote::AdoptForgotten);
        }
        self.units[t].tense = UnitTense::Current;
        if self.units[t].card == Some(Cardinality::Single) {
            let group = self.group(&self.units[t].group.clone());
            let cur = self.live_current(&group).filter(|&o| o != t);
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
        let hit: Vec<usize> = (0..self.units.len())
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
