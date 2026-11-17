//! 統合の純粋な核 (04 §5.3・§5.4・§7.4〜§7.6)。I/O・時計・乱数を持たない。
//!
//! - 事実 (`Fact`) の集まりに、出来事 (`Event`: 候補・検証済みの訂正・採る/戻す) を位置の順に当てる。
//!   位置は、候補と訂正は (ターンの ID, span の始まり, span の終わり, 主語, 属性, 正規化した値)、
//!   採る・戻すは指示の ID (04 §5.3 の入力の順)。ターンも指示も UUID v7 なので 1 本の列に並ぶ。
//! - private と再び覚えない印は、時点に依らず常に効く制約として `Directives` から毎回計算する
//!   (04 §7.5 の「当てる時点」)。
//! - 同じ (主語, 属性) で値の鍵が同じ事実が、すでにその候補の出典を持っていれば、当て済みとして何も
//!   しない (04 §5.3 の「済んだ候補」)。
//!
//! 保存の形 (封筒) への写しと、書き込みの入口は別の層 (M3.1 の後の段) に置く。

use std::collections::BTreeSet;

use evorag_types::{Id, Timestamp};

use crate::correction::Target;
use crate::lexical::Signals;
use crate::rules::{Attributes, Cardinality};
use crate::value_key::{BlockMark, ForgetKey, ValueKey, block_mark, normalize_v1};

/// 事実の出典 1 つ (04 §5.1)。並びは (ターン, 始まり, 終わり, 改訂の番号)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source {
    pub turn: Id,
    /// 改訂 `rev` の本文の UTF-8 のバイト位置。
    pub start: u32,
    pub end: u32,
    pub rev: u32,
}

/// 時制 (04 §5.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tense {
    Current,
    Past,
    Planned,
}

/// 事実の状態 (04 §5.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactState {
    Live,
    /// 単値の属性で、今の値 (`against`) と違う値が、置き換えの条件を満たさずに出てきた。
    Contested {
        against: Id,
    },
    /// 置き換えられた (`by`)、または終わった (`by` 無し)。`at` は置き換えた出来事 (ターンか指示) の
    /// ID の時刻 (作り直しても同じ値になる、04 §5.1 の `state_at`)。
    Superseded {
        by: Option<Id>,
        at: Timestamp,
    },
    /// 忘れた (M3.4)。
    Retracted,
}

/// 事実 1 件。保存の記録と 1 対 1 に対応し、隠れた状態を持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub id: Id,
    /// `self` か、正規化 v1 の後の呼び名。
    pub subject: String,
    pub attribute: String,
    /// 最初に言った表記。`Retracted` では無い。
    pub value: Option<String>,
    pub vk: Option<ValueKey>,
    pub tense: Tense,
    pub state: FactState,
    pub sources: BTreeSet<Source>,
    /// 指示 (04 §7.5) から計算した値の写し。
    pub private: bool,
}

impl Fact {
    /// 出典の最も古いターン (private の「後継」の判定に使う、04 §7.5)。
    pub fn earliest_turn(&self) -> Option<Id> {
        self.sources.iter().map(|s| s.turn).min()
    }

    /// 出典の最も新しいターン (事実の表の予算の並び、04 §5.4)。
    pub fn latest_turn(&self) -> Option<Id> {
        self.sources.iter().map(|s| s.turn).max()
    }

    fn distinct_turns(&self) -> usize {
        self.sources
            .iter()
            .map(|s| s.turn)
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn is_open(&self) -> bool {
        matches!(self.state, FactState::Live | FactState::Contested { .. })
    }
}

/// 叙法 (関門 3 の後に残るもの)。`question`・`hypothetical` は関門 3 で捨てるので、ここには来ない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Fact,
    Negated,
}

/// 関門を通った抽出の候補 (04 §5.2)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub source: Source,
    /// `self` か、正規化 v1 の後の呼び名。
    pub subject: String,
    pub attribute: String,
    pub value: String,
    pub vk: ValueKey,
    pub mood: Mood,
    pub tense: Tense,
    /// 置き換える古い値の値の鍵 (発話に古い値があったとき)。
    pub old_vk: Option<ValueKey>,
    /// 根拠から読み取った印 (`lexical::signals`)。
    pub signals: Signals,
}

/// 検証済みの訂正 (04 §6)。検証の関門 ([`crate::verify`]) を通ったものと、保存した検証済みの訂正を読んだもの
/// だけが作れる型にする (候補の型 [`crate::CorrectionCandidate`] とは別)。事実の置き換え (04 §5.3) と学習 (13)
/// には、この型しか渡せない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCorrection {
    /// 新しい値の範囲 (候補の発話の保存した本文の位置と、その改訂の番号)。
    source: Source,
    /// 古い値の範囲 (`{old}` の穴の中)。古い値の穴の無い型 (`I meant {new}` など) では無く、その訂正は事実を
    /// 変えない (学習にだけ使う、04 §6 の「事実への効き方」)。
    old_source: Option<Source>,
    target: Target,
    old_value: String,
    old_vk: ValueKey,
    new_value: String,
    new_vk: ValueKey,
}

impl VerifiedCorrection {
    /// 検証を通ったものとして作る。検証の関門 (`correction::verify`) と、保存した検証済みの訂正を読む層と、
    /// 試験だけが使う。
    pub(crate) fn assume_verified(
        source: Source,
        old_source: Option<Source>,
        target: Target,
        old_value: String,
        old_vk: ValueKey,
        new_value: String,
        new_vk: ValueKey,
    ) -> Self {
        Self {
            source,
            old_source,
            target,
            old_value,
            old_vk,
            new_value,
            new_vk,
        }
    }

    /// 訂正の出典 (新しい値の範囲。書き込みの入口で、改訂の番号を今のターンと突き合わせる)。
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// 古い値の範囲 (古い値の穴の無い型では `None`。そのときは事実を変えない)。
    pub fn old_source(&self) -> Option<&Source> {
        self.old_source.as_ref()
    }

    /// 直したのが事実か応答の誤りか。
    pub fn target(&self) -> Target {
        self.target
    }

    /// 古い値 (検証の LLM の出力)。
    pub fn old_value(&self) -> &str {
        &self.old_value
    }

    /// 古い値の値の鍵。
    pub fn old_vk(&self) -> &ValueKey {
        &self.old_vk
    }

    /// 新しい値 (検証の LLM の出力)。
    pub fn new_value(&self) -> &str {
        &self.new_value
    }

    /// 新しい値の値の鍵。
    pub fn new_vk(&self) -> &ValueKey {
        &self.new_vk
    }
}

/// private の効く範囲 (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Successors,
    ValueOnly,
}

/// 事実への指示の種類 (04 §7.5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveKind {
    Private {
        scope: Scope,
    },
    /// `target` の private の指示を解く。
    PrivateOff {
        target: Id,
    },
    Accept,
    Revert,
    /// 忘れる (伏せ字は M3.4)。印は値ごとに 1 つ。
    Forget {
        marks: Vec<BlockMark>,
    },
    /// `target` の忘れるの指示の印を解く。
    Unblock {
        target: Id,
    },
    /// 新しい版の知らない指示。印だけを当てる (04 §7.5 の「知らない op の読み方」)。
    Unknown {
        marks: Vec<BlockMark>,
    },
}

/// 事実への指示 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// 指示の ID (= 指示が効き始めた時点)。
    pub id: Id,
    pub kind: DirectiveKind,
    pub subject: String,
    pub attribute: String,
    pub vk: Option<ValueKey>,
}

/// 指示の集まり。常に効く制約 (private・印) をここで計算する。
#[derive(Debug, Clone, Default)]
pub struct Directives {
    list: Vec<Directive>,
}

impl Directives {
    pub fn new(mut list: Vec<Directive>) -> Self {
        list.sort_by_key(|d| d.id);
        Self { list }
    }

    pub fn push(&mut self, d: Directive) {
        self.list.push(d);
        self.list.sort_by_key(|d| d.id);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Directive> {
        self.list.iter()
    }

    /// private の指示 `id` が解かれたか。解くのは `PrivateOff` だけ (04 §7.5 の「解く指示の相手」)。
    fn private_cancelled(&self, id: Id) -> bool {
        self.list
            .iter()
            .any(|d| matches!(d.kind, DirectiveKind::PrivateOff { target } if target == id))
    }

    /// 忘れるの指示 `id` の印が解かれたか。解くのは `Unblock` だけ。
    fn forget_cancelled(&self, id: Id) -> bool {
        self.list
            .iter()
            .any(|d| matches!(d.kind, DirectiveKind::Unblock { target } if target == id))
    }

    /// 事実が private か (04 §7.5)。`successors` は、同じ (主語, 属性) で、出典の最も新しいターンが指示
    /// より後の事実 (指示の後に言われた値) にも効く。
    pub fn is_private(&self, f: &Fact) -> bool {
        !self.private_cover(f).is_empty()
    }

    /// 事実に効いている、解かれていない private の指示と、その効き方 (指示の ID の順)。`/private off` は、
    /// これをすべて解く (04 §7.5 の「private の解除」)。
    pub fn private_cover(&self, f: &Fact) -> Vec<(&Directive, Cover)> {
        self.list
            .iter()
            .filter_map(|d| {
                let DirectiveKind::Private { scope } = d.kind else {
                    return None;
                };
                if d.subject != f.subject
                    || d.attribute != f.attribute
                    || self.private_cancelled(d.id)
                {
                    return None;
                }
                if d.vk.is_some() && d.vk == f.vk {
                    Some((d, Cover::SameValue))
                } else if scope == Scope::Successors && f.latest_turn().is_some_and(|t| t > d.id) {
                    Some((d, Cover::Successor))
                } else {
                    None
                }
            })
            .collect()
    }

    /// (主語, 属性) の、解かれていない後継まで効く private の指示 (指示の ID の順)。
    pub fn successor_rules(&self, subject: &str, attribute: &str) -> Vec<&Directive> {
        self.list
            .iter()
            .filter(|d| {
                d.kind
                    == DirectiveKind::Private {
                        scope: Scope::Successors,
                    }
                    && d.subject == subject
                    && d.attribute == attribute
                    && !self.private_cancelled(d.id)
            })
            .collect()
    }

    /// 指示 `id`。
    pub fn get(&self, id: Id) -> Option<&Directive> {
        self.list.iter().find(|d| d.id == id)
    }

    /// 忘れるの指示 `id` の印が、印を解く指示で解かれたか。
    pub fn is_unblocked(&self, id: Id) -> bool {
        self.forget_cancelled(id)
    }

    /// 再び覚えない印に当たるか (04 §7.6)。解かれた印は当てない。知らない指示の印も当てる。
    pub fn is_blocked(&self, mark: &BlockMark) -> bool {
        self.list.iter().any(|d| match &d.kind {
            DirectiveKind::Forget { marks } => marks.contains(mark) && !self.forget_cancelled(d.id),
            DirectiveKind::Unknown { marks } => marks.contains(mark),
            _ => false,
        })
    }

    /// 新しい版の知らない指示の数 (印だけを当て、ほかは当てずに数える、04 §7.5)。
    pub fn unknown_count(&self) -> usize {
        self.list
            .iter()
            .filter(|d| matches!(d.kind, DirectiveKind::Unknown { .. }))
            .count()
    }

    /// 位置を持つ出来事 (採る・戻す) を、作り直しのときに混ぜて並べるために取り出す。
    pub fn choices(&self) -> Vec<Event> {
        self.list
            .iter()
            .filter_map(|d| {
                let choice = match d.kind {
                    DirectiveKind::Accept => Choice::Accept,
                    DirectiveKind::Revert => Choice::Revert,
                    _ => return None,
                };
                Some(Event::Choose {
                    directive: d.id,
                    subject: d.subject.clone(),
                    attribute: d.attribute.clone(),
                    vk: d.vk?,
                    choice,
                })
            })
            .collect()
    }
}

/// private の指示が事実に効く形 (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cover {
    /// 指示と同じ値の鍵。
    SameValue,
    /// 後継まで効く指示の後に言われた値。
    Successor,
}

/// 採るか戻すか (04 §7.4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// `Contested` の値を今の値にする。
    Accept,
    /// 履歴 (`Superseded`) の値を今の値に戻す。
    Revert,
}

/// 畳みに当てる出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Candidate(Candidate),
    Correction(VerifiedCorrection),
    /// 採る・戻す。指示の ID の位置で当てる。
    Choose {
        directive: Id,
        subject: String,
        attribute: String,
        vk: ValueKey,
        choice: Choice,
    },
}

/// 並べる位置 (04 §5.3 の入力の順)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    at: Id,
    start: u32,
    end: u32,
    /// 訂正 (0) を候補 (1) より先に当てる。
    kind: u8,
    subject: String,
    attribute: String,
    /// `current` (0) → `planned` (1) → `past` (2)。値の綴りに依らず、置き換えの連鎖にするため。
    tense: u8,
    /// 言い切り (0) → 否定 (1)。
    mood: u8,
    value: String,
    has_old: bool,
}

fn tense_rank(t: Tense) -> u8 {
    match t {
        Tense::Current => 0,
        Tense::Planned => 1,
        Tense::Past => 2,
    }
}

impl Event {
    pub(crate) fn pos(&self) -> Pos {
        match self {
            Event::Candidate(c) => Pos {
                at: c.source.turn,
                start: c.source.start,
                end: c.source.end,
                kind: 1,
                subject: c.subject.clone(),
                attribute: c.attribute.clone(),
                tense: tense_rank(c.tense),
                mood: u8::from(c.mood == Mood::Negated),
                value: normalize_v1(&c.value),
                has_old: c.old_vk.is_some(),
            },
            Event::Correction(c) => Pos {
                at: c.source.turn,
                start: c.source.start,
                end: c.source.end,
                kind: 0,
                subject: String::new(),
                attribute: String::new(),
                tense: 0,
                mood: 0,
                value: normalize_v1(&c.new_value),
                has_old: true,
            },
            Event::Choose { directive, .. } => Pos {
                at: *directive,
                start: 0,
                end: 0,
                kind: 0,
                subject: String::new(),
                attribute: String::new(),
                tense: 0,
                mood: 0,
                value: String::new(),
                has_old: false,
            },
        }
    }
}

/// 畳みで数えたもの (状態の行・`doctor` の材料)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// 新しい事実を作った。
    pub added: usize,
    /// 今の値を置き換えた・終えた。
    pub replaced: usize,
    /// 矛盾として記録した。
    pub contested: usize,
    /// 既にある事実に出典を足した。
    pub merged: usize,
    /// 当て済みだった。
    pub already: usize,
    /// 属性の一覧に無かった。
    pub unknown_attribute: usize,
    /// 再び覚えない印に当たった (関門 8)。
    pub blocked: usize,
    /// 否定の候補で、今の値を変えなかった。
    pub negation_dropped: usize,
    /// 検証済みの訂正で、古い値が今の値のどれとも一致しないので、事実を変えなかった (学習にだけ使う)。
    pub correction_no_effect: usize,
    /// 検証済みの訂正で、古い値が 2 つ以上の (主語, 属性) の今の値と一致し、どれを直したのか決められないので、
    /// 事実を変えなかった (04 §6 の「事実への効き方」)。
    pub correction_ambiguous: usize,
    /// 検証済みの訂正で、古い値の穴の無い型 (`I meant {new}` など) なので、事実を変えなかった (04 §6)。
    pub correction_unanchored: usize,
    /// 書き込みの入口で、出典の改訂の番号が今のターンと合わない (ターンが無いものを含む) ので捨てた
    /// (04 §4・§5.2.1)。
    pub stale_source: usize,
    /// 書き込みの入口で、値が関門 6・7 に外れたので捨てた (04 §5.3 の書き込みの入口の 1)。
    pub value_rejected: usize,
    /// 書き込みの入口で、主語が関門 5 (根拠を見ない部分) か、主語に当てる関門 6 に外れたので捨てた (04 §5.2)。
    pub subject_rejected: usize,
    /// 事実の表に入りうる状態 (`Live` で private でない) だった事実のうち、畳みで消えた・`Live` でなくなった・
    /// private になった・値が変わったもの (事実の ID の順)。呼び出す側は、覚えた事実の表にこれがあれば、次の
    /// ターンの前に表を作り直す (04 §5.4)。書き込みの入口 (`consolidate`・`rebuild`) だけが埋める。
    pub withdrawn: Vec<Id>,
}

/// 畳みの環境。
pub struct FoldEnv<'a> {
    pub key: &'a ForgetKey,
    pub attributes: &'a Attributes,
    /// 新しい事実の ID を作る。
    pub new_id: &'a mut dyn FnMut() -> Id,
}

/// 事実の集まり (数百件の見込みなので、並びで持つ、04 §5.4)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactSet {
    pub facts: Vec<Fact>,
}

impl FactSet {
    fn group(&self, subject: &str, attribute: &str) -> Vec<usize> {
        (0..self.facts.len())
            .filter(|&i| self.facts[i].subject == subject && self.facts[i].attribute == attribute)
            .collect()
    }

    /// 単値の属性の今の値 (時制 `current` の `Live`)。
    fn live_current(&self, group: &[usize]) -> Option<usize> {
        group.iter().copied().find(|&i| {
            let f = &self.facts[i];
            f.state == FactState::Live && f.tense == Tense::Current
        })
    }

    fn find_open(&self, group: &[usize], vk: &ValueKey) -> Option<usize> {
        group
            .iter()
            .copied()
            .find(|&i| self.facts[i].is_open() && self.facts[i].vk.as_ref() == Some(vk))
    }

    fn push(&mut self, f: Fact) -> usize {
        self.facts.push(f);
        self.facts.len() - 1
    }

    /// `new` を今の値にする。`old` (今の値) は `Superseded { by: new }` にし、同じ (主語, 属性) の矛盾の
    /// 相手を `new` に付け替える。`new` と同じ値の矛盾は、出典を `new` に合わせて消す (04 §5.3)。
    fn make_live(&mut self, old: Option<usize>, new: usize, at: Id) {
        let new_id = self.facts[new].id;
        if let Some(o) = old
            && o != new
        {
            self.facts[o].state = FactState::Superseded {
                by: Some(new_id),
                at: at.timestamp(),
            };
        }
        self.facts[new].state = FactState::Live;
        let (subject, attribute, vk) = {
            let f = &self.facts[new];
            (f.subject.clone(), f.attribute.clone(), f.vk)
        };
        let mut absorbed = Vec::new();
        for i in self.group(&subject, &attribute) {
            if i == new {
                continue;
            }
            if let FactState::Contested { .. } = self.facts[i].state {
                if self.facts[i].vk == vk {
                    absorbed.push(i);
                } else {
                    self.facts[i].state = FactState::Contested { against: new_id };
                }
            }
        }
        for &i in &absorbed {
            let src = std::mem::take(&mut self.facts[i].sources);
            self.facts[new].sources.extend(src);
        }
        let gone: BTreeSet<Id> = absorbed.iter().map(|&i| self.facts[i].id).collect();
        self.facts.retain(|f| !gone.contains(&f.id));
    }

    fn index_of(&self, id: Id) -> Option<usize> {
        self.facts.iter().position(|f| f.id == id)
    }
}

/// 出来事を位置の順に当て、最後に private を計算し直す。
pub fn fold(
    set: &mut FactSet,
    directives: &Directives,
    mut events: Vec<Event>,
    env: &mut FoldEnv<'_>,
) -> Report {
    let mut report = Report::default();
    events.sort_by_cached_key(Event::pos);
    for e in events {
        match e {
            Event::Candidate(c) => apply_candidate(set, directives, &c, env, &mut report),
            Event::Correction(c) => apply_correction(set, directives, &c, env, &mut report),
            Event::Choose {
                directive,
                subject,
                attribute,
                vk,
                choice,
            } => {
                let ch = Chosen {
                    directive,
                    subject: &subject,
                    attribute: &attribute,
                    vk: &vk,
                    choice,
                };
                apply_choice(set, directives, &ch, env);
            }
        }
    }
    refresh_private(set, directives);
    report
}

/// private の写しを指示から計算し直す (指示が変わったときにも呼ぶ)。
pub fn refresh_private(set: &mut FactSet, directives: &Directives) {
    for f in &mut set.facts {
        f.private = directives.is_private(f);
    }
}

fn cardinality(env: &FoldEnv<'_>, attribute: &str) -> Option<Cardinality> {
    env.attributes.get(attribute).map(|a| a.cardinality)
}

fn new_fact(env: &mut FoldEnv<'_>, c: &Candidate, tense: Tense, state: FactState) -> Fact {
    Fact {
        id: (env.new_id)(),
        subject: c.subject.clone(),
        attribute: c.attribute.clone(),
        value: Some(c.value.clone()),
        vk: Some(c.vk),
        tense,
        state,
        sources: BTreeSet::from([c.source]),
        private: false,
    }
}

fn apply_candidate(
    set: &mut FactSet,
    directives: &Directives,
    c: &Candidate,
    env: &mut FoldEnv<'_>,
    report: &mut Report,
) {
    let Some(card) = cardinality(env, &c.attribute) else {
        report.unknown_attribute += 1;
        return;
    };
    if directives.is_blocked(&block_mark(env.key, &c.subject, &c.attribute, &c.vk)) {
        report.blocked += 1;
        return;
    }
    let group = set.group(&c.subject, &c.attribute);
    // 当て済み (04 §5.3 の「済んだ候補」)。
    if group.iter().any(|&i| {
        let f = &set.facts[i];
        f.vk == Some(c.vk) && f.sources.contains(&c.source)
    }) {
        report.already += 1;
        return;
    }
    let turn = c.source.turn;
    match (c.mood, c.tense) {
        (Mood::Negated, _) => {
            // 否定: 今の値と一致し、終える語があるときだけ終える (04 §5.3 の「否定」)。
            let hit = group.iter().copied().find(|&i| {
                let f = &set.facts[i];
                f.state == FactState::Live && f.tense != Tense::Past && f.vk == Some(c.vk)
            });
            match hit {
                Some(i) if c.signals.negation_change => {
                    set.facts[i].state = FactState::Superseded {
                        by: None,
                        at: turn.timestamp(),
                    };
                    set.facts[i].sources.insert(c.source);
                    report.replaced += 1;
                }
                _ => report.negation_dropped += 1,
            }
        }
        (Mood::Fact, Tense::Past) => {
            let live = group.iter().copied().find(|&i| {
                let f = &set.facts[i];
                f.state == FactState::Live && f.tense == Tense::Current && f.vk == Some(c.vk)
            });
            // 同じ値の `Superseded` の記録 (時制を問わない) があれば、そこにまとめる (04 §5.3 の時制)。
            let history = group.iter().copied().find(|&i| {
                let f = &set.facts[i];
                matches!(f.state, FactState::Superseded { .. }) && f.vk == Some(c.vk)
            });
            if let Some(i) = live {
                // 過去形で今の値を終える (置き換えではない)。終えた発言を出典に足す。
                set.facts[i].state = FactState::Superseded {
                    by: None,
                    at: turn.timestamp(),
                };
                set.facts[i].sources.insert(c.source);
                report.replaced += 1;
            } else if let Some(i) = history {
                set.facts[i].sources.insert(c.source);
                report.merged += 1;
            } else {
                let f = new_fact(
                    env,
                    c,
                    Tense::Past,
                    FactState::Superseded {
                        by: None,
                        at: turn.timestamp(),
                    },
                );
                set.push(f);
                report.added += 1;
            }
        }
        (Mood::Fact, Tense::Planned) => {
            if let Some(i) = set.find_open(&group, &c.vk) {
                set.facts[i].sources.insert(c.source);
                report.merged += 1;
                // 同じ値の矛盾の出典が 2 つの別のターンになったら今の値にする (04 §5.3)。
                if card == Cardinality::Single {
                    promote_if_repeated(set, &group, i, turn, report);
                }
                return;
            }
            if card == Cardinality::Single {
                let old_plan = group.iter().copied().find(|&i| {
                    let f = &set.facts[i];
                    f.state == FactState::Live && f.tense == Tense::Planned
                });
                let n = set.push(new_fact(env, c, Tense::Planned, FactState::Live));
                if let Some(o) = old_plan {
                    let by = set.facts[n].id;
                    set.facts[o].state = FactState::Superseded {
                        by: Some(by),
                        at: turn.timestamp(),
                    };
                    report.replaced += 1;
                }
            } else {
                set.push(new_fact(env, c, Tense::Planned, FactState::Live));
            }
            report.added += 1;
        }
        (Mood::Fact, Tense::Current) => match card {
            Cardinality::Single => apply_single_current(set, &group, c, env, report),
            Cardinality::Multi => apply_multi_current(set, &group, c, env, report),
        },
    }
}

fn apply_single_current(
    set: &mut FactSet,
    group: &[usize],
    c: &Candidate,
    env: &mut FoldEnv<'_>,
    report: &mut Report,
) {
    let turn = c.source.turn;
    let cond = c.signals.explicit_subject || c.signals.change;
    if let Some(s) = set.find_open(group, &c.vk) {
        set.facts[s].sources.insert(c.source);
        let cur = set.live_current(group).filter(|&i| i != s);
        match set.facts[s].state {
            FactState::Live if set.facts[s].tense == Tense::Current => {
                report.merged += 1;
            }
            FactState::Live => {
                // 予定の値が実現した: 同じ記録の時制を current にする (04 §5.3 の時制)。
                set.facts[s].tense = Tense::Current;
                match cur {
                    None => {
                        // 今の値になったので、残りの矛盾の相手を付け替える。
                        set.make_live(None, s, turn);
                        report.merged += 1;
                    }
                    Some(o) if cond => {
                        set.make_live(Some(o), s, turn);
                        report.replaced += 1;
                    }
                    Some(o) => {
                        let against = set.facts[o].id;
                        set.facts[s].state = FactState::Contested { against };
                        report.contested += 1;
                        promote_if_repeated(set, group, s, turn, report);
                    }
                }
            }
            FactState::Contested { .. } => {
                if cond || set.facts[s].distinct_turns() >= 2 {
                    set.make_live(cur, s, turn);
                    report.replaced += 1;
                } else {
                    report.merged += 1;
                }
            }
            _ => {}
        }
        return;
    }
    let cur = set.live_current(group);
    let n = set.push(new_fact(env, c, Tense::Current, FactState::Live));
    report.added += 1;
    match cur {
        // 今の値が無ければ今の値にし、残りの矛盾の相手を付け替える (04 §5.3)。
        None => set.make_live(None, n, turn),
        Some(o) if cond => {
            set.make_live(Some(o), n, turn);
            report.replaced += 1;
        }
        Some(o) => {
            let against = set.facts[o].id;
            set.facts[n].state = FactState::Contested { against };
            report.contested += 1;
        }
    }
}

/// 単値の属性で、矛盾の値 `i` の出典が 2 つ以上の別のターンになっていれば、今の値にする (04 §5.3)。
fn promote_if_repeated(
    set: &mut FactSet,
    group: &[usize],
    i: usize,
    turn: Id,
    report: &mut Report,
) {
    if matches!(set.facts[i].state, FactState::Contested { .. })
        && set.facts[i].distinct_turns() >= 2
    {
        let cur = set.live_current(group).filter(|&o| o != i);
        set.make_live(cur, i, turn);
        report.replaced += 1;
    }
}

fn apply_multi_current(
    set: &mut FactSet,
    group: &[usize],
    c: &Candidate,
    env: &mut FoldEnv<'_>,
    report: &mut Report,
) {
    if let Some(s) = set.find_open(group, &c.vk) {
        set.facts[s].sources.insert(c.source);
        set.facts[s].tense = Tense::Current;
        report.merged += 1;
        return;
    }
    // 古い値の 1 件がちょうど特定できたときだけ置き換える。
    let target = c.old_vk.and_then(|old| {
        let hits: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&i| {
                let f = &set.facts[i];
                f.state == FactState::Live && f.tense == Tense::Current && f.vk == Some(old)
            })
            .collect();
        (hits.len() == 1).then(|| hits[0])
    });
    let n = set.push(new_fact(env, c, Tense::Current, FactState::Live));
    report.added += 1;
    if let Some(t) = target {
        let by = set.facts[n].id;
        set.facts[t].state = FactState::Superseded {
            by: Some(by),
            at: c.source.turn.timestamp(),
        };
        report.replaced += 1;
    }
}

fn apply_correction(
    set: &mut FactSet,
    directives: &Directives,
    c: &VerifiedCorrection,
    env: &mut FoldEnv<'_>,
    report: &mut Report,
) {
    // 古い値の位置を発話で確かめられない訂正は、事実を変えない (学習にだけ使う、04 §6)。
    let Some(old_source) = c.old_source else {
        report.correction_unanchored += 1;
        return;
    };
    // 訂正は出典では当て済みを判定しない。古い値がもう今の値でなければ、下で効かない (04 §5.3)。
    // 古い値の値の鍵が、今の値のちょうど 1 件と一致するときだけ事実を変える (04 §6)。
    let hits: Vec<usize> = (0..set.facts.len())
        .filter(|&i| {
            let f = &set.facts[i];
            f.state == FactState::Live && f.tense == Tense::Current && f.vk == Some(c.old_vk)
        })
        .collect();
    let t = match hits[..] {
        [t] => t,
        [] => {
            report.correction_no_effect += 1;
            return;
        }
        _ => {
            report.correction_ambiguous += 1;
            return;
        }
    };
    let (subject, attribute) = (set.facts[t].subject.clone(), set.facts[t].attribute.clone());
    let Some(attr) = env.attributes.get(&attribute) else {
        report.correction_no_effect += 1;
        return;
    };
    let card = attr.cardinality;
    // 関門 7: 新しい値の長さを、当てる属性の上限で見る (関門 6 は書き込みの入口が先に当てる、04 §5.3)。
    if c.new_value.chars().count() > usize::from(attr.max_chars) {
        report.value_rejected += 1;
        return;
    }
    // 新しい値が忘れた値なら、訂正でも蘇らせない (04 §7.5 の「再び覚えない印と訂正・戻す」)。
    if directives.is_blocked(&block_mark(env.key, &subject, &attribute, &c.new_vk)) {
        report.blocked += 1;
        return;
    }
    let turn = c.source.turn;
    let group = set.group(&subject, &attribute);
    let n = match set.find_open(&group, &c.new_vk) {
        Some(s) => {
            set.facts[s].sources.insert(c.source);
            set.facts[s].tense = Tense::Current;
            s
        }
        None => {
            report.added += 1;
            set.push(Fact {
                id: (env.new_id)(),
                subject,
                attribute,
                value: Some(c.new_value.clone()),
                vk: Some(c.new_vk),
                tense: Tense::Current,
                state: FactState::Live,
                sources: BTreeSet::from([c.source]),
                private: false,
            })
        }
    };
    let t_id = set.facts[t].id;
    // 訂正の発話の中で古い値に一致した範囲を、置き換えた事実の出典に足す (04 §5.3 の「終えた発言の出典」)。
    set.facts[t].sources.insert(old_source);
    match card {
        Cardinality::Single => {
            let t = set.index_of(t_id).expect("the corrected fact is present");
            set.make_live(Some(t), n, turn);
        }
        Cardinality::Multi => {
            let by = set.facts[n].id;
            let t = set.index_of(t_id).expect("the corrected fact is present");
            set.facts[t].state = FactState::Superseded {
                by: Some(by),
                at: turn.timestamp(),
            };
        }
    }
    report.replaced += 1;
}

/// 採る・戻すの出来事の中身 (`apply_choice` に渡す形)。
struct Chosen<'a> {
    directive: Id,
    subject: &'a str,
    attribute: &'a str,
    vk: &'a ValueKey,
    choice: Choice,
}

fn apply_choice(set: &mut FactSet, directives: &Directives, ch: &Chosen<'_>, env: &FoldEnv<'_>) {
    let Chosen {
        directive,
        subject,
        attribute,
        vk,
        choice,
    } = *ch;
    // 忘れた値は、戻すでも蘇らせない (04 §7.5)。
    if directives.is_blocked(&block_mark(env.key, subject, attribute, vk)) {
        return;
    }
    let group = set.group(subject, attribute);
    let card = cardinality(env, attribute);
    // 同じ値の開いた記録があれば、履歴の記録は戻さない (04 §7.5 の「戻すの重なり」)。
    if choice == Choice::Revert
        && let Some(open) = set.find_open(&group, vk)
    {
        if card == Some(Cardinality::Single)
            && matches!(set.facts[open].state, FactState::Contested { .. })
        {
            let cur = set.live_current(&group).filter(|&i| i != open);
            set.make_live(cur, open, directive);
        }
        return;
    }
    let target = group.iter().copied().find(|&i| {
        let f = &set.facts[i];
        f.vk.as_ref() == Some(vk)
            && match choice {
                Choice::Revert => matches!(f.state, FactState::Superseded { .. }),
                Choice::Accept => matches!(f.state, FactState::Contested { .. }),
            }
    });
    let Some(t) = target else {
        return;
    };
    let t_id = set.facts[t].id;
    set.facts[t].tense = Tense::Current;
    match card {
        Some(Cardinality::Single) => {
            let cur = set.live_current(&group).filter(|&i| i != t);
            set.make_live(cur, t, directive);
        }
        _ => {
            let t = set.index_of(t_id).expect("present");
            set.facts[t].state = FactState::Live;
        }
    }
}

/// 出典のターンが消えたときに変えたもの ([`drop_sources`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    /// 畳みの前にあって、後に無い事実 (ID の順)。出典が 1 つも残らないので消したものと、繰り上げた値に
    /// 合わせて消した同じ値の矛盾を含む。
    pub deleted: Vec<Id>,
    /// 出典の一部を外して残した事実の数。
    pub trimmed: usize,
    /// 消えた今の値に代わって、今の値にした矛盾の値 (ID の順、04 §7.4)。
    pub promoted: Vec<Id>,
    /// 出典が外れて後継でなくなり、private でなくなるはずだった事実に足した、その値だけの private の指示
    /// (04 §7.5 の「後継」)。呼ぶ側は、同じ依頼で指示の記録として書く。
    pub directives: Vec<Directive>,
}

impl Dropped {
    /// 何も変えなかったか。
    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty()
            && self.trimmed == 0
            && self.promoted.is_empty()
            && self.directives.is_empty()
    }
}

/// 出典のターンが消えた (セッションの削除 04 §7.1、出典の無い事実の掃除 04 §5.3・03 §4.2 の規則 12)。
///
/// - 消えたターン (`gone` が `true` を返すもの) の出典を外す。それで出典が 1 つも残らなくなった事実は消す
///   (`Retracted` も含む)。もともと出典の無い事実には触れない (関係の無い削除で、消したり兄弟を繰り上げたり
///   しない)。
/// - 消した事実が単値の属性の今の値 (時制 `current` の `Live`) なら、04 §7.4 のとおりにする: 古い側の
///   `Superseded` の `by` は消えた ID を指したまま残し、`Live` に戻さない。同じ (主語, 属性) の `Contested`
///   のうち、残った出典の最も新しいターンが消えた値の出典の最も新しいターンより後のもの (2 つ以上あれば最も
///   後のもの、同じなら事実の ID の大きいもの) を今の値にし、残りの `Contested` の `against` をその値に
///   付け替える。そうでない `Contested` は、`against` に消えた ID を指したまま残す (今の値なし)。予定の値、
///   多値の属性の値、過去形・否定で終えた値が消えたときは繰り上げない。
/// - private の写しを指示から計算し直す (書き込みの入口が毎回計算し直す、04 §5.1)。出典が外れて後継で
///   なくなり private でなくなる事実には、その値だけ (`value_only`) の private の指示を `new_id` の ID で
///   足してから計算し直す (04 §7.5)。足した指示は `directives` にも入り、`Dropped::directives` で返す。
pub fn drop_sources(
    set: &mut FactSet,
    directives: &mut Directives,
    attributes: &Attributes,
    gone: &dyn Fn(Id) -> bool,
    new_id: &mut dyn FnMut() -> Id,
) -> Dropped {
    let mut out = Dropped::default();
    let ids_before: BTreeSet<Id> = set.facts.iter().map(|f| f.id).collect();
    let was_private: BTreeSet<Id> = set
        .facts
        .iter()
        .filter(|f| f.private)
        .map(|f| f.id)
        .collect();
    // 消した今の値の (主語, 属性, 出典の最も新しいターン)。
    let mut ended: Vec<(String, String, Option<Id>)> = Vec::new();
    let mut emptied: BTreeSet<Id> = BTreeSet::new();
    for f in &mut set.facts {
        let latest = f.latest_turn();
        let before = f.sources.len();
        f.sources.retain(|s| !gone(s.turn));
        if f.sources.len() == before {
            continue;
        }
        if !f.sources.is_empty() {
            out.trimmed += 1;
            continue;
        }
        emptied.insert(f.id);
        let single = attributes
            .get(&f.attribute)
            .is_some_and(|a| a.cardinality == Cardinality::Single);
        if single && f.state == FactState::Live && f.tense == Tense::Current {
            ended.push((f.subject.clone(), f.attribute.clone(), latest));
        }
    }
    set.facts.retain(|f| !emptied.contains(&f.id));
    out.promoted = promote_after(set, ended);
    // private でなくなる事実に、その値だけの private の指示を足す (04 §7.5 の「後継」)。
    for f in &set.facts {
        if !was_private.contains(&f.id) || directives.is_private(f) {
            continue;
        }
        let Some(vk) = f.vk else {
            continue;
        };
        let d = Directive {
            id: new_id(),
            kind: DirectiveKind::Private {
                scope: Scope::ValueOnly,
            },
            subject: f.subject.clone(),
            attribute: f.attribute.clone(),
            vk: Some(vk),
        };
        out.directives.push(d.clone());
        directives.push(d);
    }
    refresh_private(set, directives);
    let ids_after: BTreeSet<Id> = set.facts.iter().map(|f| f.id).collect();
    out.deleted = ids_before.difference(&ids_after).copied().collect();
    out
}

/// 消えた・取り消した今の値 (単値の属性の、時制 `current` の `Live`) の代わりに、同じ (主語, 属性) の
/// `Contested` のうち、出典の最も新しいターンが消えた値の出典の最も新しいターンより後のもの (2 つ以上あれば
/// 最も後のもの、同じなら事実の ID の大きいもの) を今の値にする (04 §7.4)。`ended` は消えた値の
/// (主語, 属性, 出典の最も新しいターン)。今の値にした事実の ID を ID の順に返す。
fn promote_after(set: &mut FactSet, ended: Vec<(String, String, Option<Id>)>) -> Vec<Id> {
    let mut promoted = Vec::new();
    for (subject, attribute, latest) in ended {
        let group = set.group(&subject, &attribute);
        if set.live_current(&group).is_some() {
            continue;
        }
        let next = group
            .iter()
            .copied()
            .filter(|&i| matches!(set.facts[i].state, FactState::Contested { .. }))
            .filter_map(|i| {
                let t = set.facts[i].latest_turn()?;
                (Some(t) > latest).then_some((t, set.facts[i].id, i))
            })
            .max();
        if let Some((turn, id, i)) = next {
            set.make_live(None, i, turn);
            promoted.push(id);
        }
    }
    promoted.sort_unstable();
    promoted
}

/// 忘れるで取り消したもの ([`retract`])。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retraction {
    /// `Retracted` にした事実 (ID の順)。もう `Retracted` だった事実は含まない。
    pub retracted: Vec<Id>,
    /// 取り消した今の値に代わって、今の値にした矛盾の値 (ID の順、04 §7.4)。
    pub promoted: Vec<Id>,
}

/// 事実 `ids` を忘れる (04 §7.1・§7.4): `Retracted` にし、値・値の鍵・span を消す (出典のターンと改訂の番号は
/// 残す)。単値の属性の今の値 (時制 `current` の `Live`) を取り消したときは、セッションの削除と同じく
/// `Contested` を繰り上げる ([`drop_sources`] と同じ規則)。置き換えの連鎖の `by`・`against` は、取り消した記録を
/// 指したまま残す。private の写しは呼ぶ側が指示から計算し直す ([`refresh_private`])。
pub fn retract(set: &mut FactSet, ids: &BTreeSet<Id>, attributes: &Attributes) -> Retraction {
    let mut out = Retraction::default();
    let mut ended: Vec<(String, String, Option<Id>)> = Vec::new();
    for f in &mut set.facts {
        if !ids.contains(&f.id) || f.state == FactState::Retracted {
            continue;
        }
        let single = attributes
            .get(&f.attribute)
            .is_some_and(|a| a.cardinality == Cardinality::Single);
        if single && f.state == FactState::Live && f.tense == Tense::Current {
            ended.push((f.subject.clone(), f.attribute.clone(), f.latest_turn()));
        }
        f.state = FactState::Retracted;
        f.value = None;
        f.vk = None;
        // 保存の形は `Retracted` の span を持たない (`FactBody`)。読み直した形と同じにする。
        f.sources = f
            .sources
            .iter()
            .map(|s| Source {
                start: 0,
                end: 0,
                ..*s
            })
            .collect();
        out.retracted.push(f.id);
    }
    out.retracted.sort_unstable();
    out.promoted = promote_after(set, ended);
    out
}

/// 事実の表に入る事実を、表の順に返す (04 §5.4。予算と書式は表を作る層で当てる)。
/// `Live` で private でないもの。並びは 属性の優先の順 → 主語 (`self` が先、残りは正規化した呼び名の順)
/// → 記録した順 (ID)。
pub fn table_order<'a>(set: &'a FactSet, attributes: &Attributes) -> Vec<&'a Fact> {
    let mut rows: Vec<&Fact> = set
        .facts
        .iter()
        .filter(|f| f.state == FactState::Live && !f.private)
        .collect();
    rows.sort_by_key(|f| {
        let prio = attributes
            .get(&f.attribute)
            .map_or(u32::MAX, |a| a.priority);
        (prio, f.subject != "self", f.subject.clone(), f.id)
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexical::signals;
    use crate::rules::attributes;
    use crate::value_key::value_key;

    const KEY: ForgetKey = ForgetKey::from_bytes_const([9; 32]);

    fn id(n: u64) -> Id {
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(b)
    }

    /// 1 つの事例の道具: ターンは `turn(n)`、指示は `id(n)` で、n の順に並ぶ。
    struct T {
        set: FactSet,
        dirs: Directives,
        next: u64,
    }

    impl T {
        fn new() -> Self {
            Self {
                set: FactSet::default(),
                dirs: Directives::default(),
                next: 10_000,
            }
        }

        fn cand(turn: u64, a: &str, v: &str, e: &str) -> Candidate {
            Candidate {
                source: Source {
                    turn: id(turn),
                    start: 0,
                    end: u32::try_from(e.len()).expect("short"),
                    rev: 0,
                },
                subject: "self".into(),
                attribute: a.into(),
                value: v.into(),
                vk: value_key(&KEY, v),
                mood: Mood::Fact,
                tense: Tense::Current,
                old_vk: None,
                signals: signals(e, "self"),
            }
        }

        fn fold(&mut self, events: Vec<Event>) -> Report {
            let mut next = self.next;
            let mut new_id = || {
                next += 1;
                id(next)
            };
            let mut env = FoldEnv {
                key: &KEY,
                attributes: attributes(),
                new_id: &mut new_id,
            };
            let r = fold(&mut self.set, &self.dirs, events, &mut env);
            self.next = next;
            r
        }

        fn state_of(&self, a: &str, v: &str) -> Vec<FactState> {
            let vk = value_key(&KEY, v);
            self.set
                .facts
                .iter()
                .filter(|f| f.attribute == a && f.vk == Some(vk))
                .map(|f| f.state)
                .collect()
        }

        fn live(&self, a: &str) -> Vec<String> {
            self.set
                .facts
                .iter()
                .filter(|f| f.attribute == a && f.state == FactState::Live)
                .filter_map(|f| f.value.clone())
                .collect()
        }
    }

    fn forget(dir: u64, a: &str, v: &str) -> Directive {
        let vk = value_key(&KEY, v);
        Directive {
            id: id(dir),
            kind: DirectiveKind::Forget {
                marks: vec![block_mark(&KEY, "self", a, &vk)],
            },
            subject: "self".into(),
            attribute: a.into(),
            vk: None,
        }
    }

    #[test]
    fn a_correction_does_not_bring_back_a_forgotten_value() {
        let mut t = T::new();
        t.fold(vec![Event::Candidate(T::cand(
            1,
            "residence",
            "大阪",
            "大阪に住んでます",
        ))]);
        t.dirs.push(forget(2, "residence", "東京"));
        let c = VerifiedCorrection::assume_verified(
            Source {
                turn: id(3),
                start: 0,
                end: 6,
                rev: 0,
            },
            Some(Source {
                turn: id(3),
                start: 9,
                end: 15,
                rev: 0,
            }),
            Target::Fact,
            "大阪".into(),
            value_key(&KEY, "大阪"),
            "東京".into(),
            value_key(&KEY, "東京"),
        );
        let r = t.fold(vec![Event::Correction(c)]);
        assert_eq!(r.blocked, 1);
        assert_eq!(t.live("residence"), ["大阪"]);
    }

    #[test]
    fn a_revert_does_not_bring_back_a_forgotten_value_nor_duplicate_an_open_one() {
        let mut t = T::new();
        t.fold(vec![
            Event::Candidate(T::cand(1, "residence", "大阪", "私は大阪に住んでいます")),
            Event::Candidate(T::cand(2, "residence", "東京", "私は東京に住んでいます")),
        ]);
        // 戻す値が忘れた値なら戻さない。
        t.dirs.push(forget(3, "residence", "大阪"));
        let revert = |dir: u64, v: &str| Event::Choose {
            directive: id(dir),
            subject: "self".into(),
            attribute: "residence".into(),
            vk: value_key(&KEY, v),
            choice: Choice::Revert,
        };
        t.fold(vec![revert(4, "大阪")]);
        assert_eq!(t.live("residence"), ["東京"]);

        // 多値: 同じ値の開いた記録があれば、履歴の記録は戻さない。
        let mut t = T::new();
        let mut c = T::cand(1, "likes", "うどん", "ラーメンじゃなくてうどんが好き");
        t.fold(vec![Event::Candidate(T::cand(
            1,
            "likes",
            "ラーメン",
            "ラーメンが好き",
        ))]);
        c.source.turn = id(2);
        c.old_vk = Some(value_key(&KEY, "ラーメン"));
        t.fold(vec![Event::Candidate(c)]);
        t.fold(vec![Event::Candidate(T::cand(
            3,
            "likes",
            "ラーメン",
            "ラーメンも好き",
        ))]);
        t.fold(vec![Event::Choose {
            directive: id(4),
            subject: "self".into(),
            attribute: "likes".into(),
            vk: value_key(&KEY, "ラーメン"),
            choice: Choice::Revert,
        }]);
        let open = t
            .state_of("likes", "ラーメン")
            .into_iter()
            .filter(|s| *s == FactState::Live)
            .count();
        assert_eq!(open, 1, "one open record of the same value");
    }

    #[test]
    fn a_planned_value_said_again_on_another_turn_replaces_the_current_one() {
        let mut t = T::new();
        t.fold(vec![Event::Candidate(T::cand(
            1,
            "residence",
            "大阪",
            "大阪に住んでます",
        ))]);
        let mut plan = T::cand(2, "residence", "東京", "東京に住む予定");
        plan.tense = Tense::Planned;
        t.fold(vec![Event::Candidate(plan)]);
        // 主語の語も変化の語も無い言い切りでも、予定と合わせて 2 つの別のターンになれば置き換える。
        t.fold(vec![Event::Candidate(T::cand(
            3,
            "residence",
            "東京",
            "東京に住んでるよ",
        ))]);
        assert_eq!(t.live("residence"), ["東京"]);
    }

    #[test]
    fn a_new_current_value_repoints_the_remaining_contested_values() {
        let mut t = T::new();
        t.fold(vec![
            Event::Candidate(T::cand(1, "residence", "大阪", "大阪に住んでます")),
            Event::Candidate(T::cand(2, "residence", "東京", "東京に住んでるよ")),
        ]);
        let mut neg = T::cand(3, "residence", "大阪", "もう大阪には住んでいない");
        neg.mood = Mood::Negated;
        t.fold(vec![Event::Candidate(neg)]);
        t.fold(vec![Event::Candidate(T::cand(
            4,
            "residence",
            "名古屋",
            "名古屋に住んでる",
        ))]);
        let nagoya = t
            .set
            .facts
            .iter()
            .find(|f| f.value.as_deref() == Some("名古屋"))
            .expect("名古屋")
            .id;
        assert_eq!(
            t.state_of("residence", "東京"),
            [FactState::Contested { against: nagoya }]
        );
    }

    #[test]
    fn a_past_value_merges_into_the_same_superseded_record() {
        let mut t = T::new();
        t.fold(vec![
            Event::Candidate(T::cand(1, "residence", "大阪", "私は大阪に住んでいます")),
            Event::Candidate(T::cand(2, "residence", "東京", "私は東京に引っ越しました")),
        ]);
        let mut past = T::cand(3, "residence", "大阪", "以前は大阪に住んでた");
        past.tense = Tense::Past;
        t.fold(vec![Event::Candidate(past)]);
        assert_eq!(
            t.state_of("residence", "大阪").len(),
            1,
            "no duplicate history"
        );
    }

    #[test]
    fn moving_from_one_place_to_another_in_one_sentence_makes_a_chain() {
        // 「大阪から東京に引っ越した」の {大阪, past} と {東京, current} は同じ範囲。値の綴りに依らず、
        // 東京が大阪を置き換える。
        let e = "大阪から東京に引っ越した";
        let mut t = T::new();
        t.fold(vec![Event::Candidate(T::cand(
            1,
            "residence",
            "大阪",
            "私は大阪に住んでいます",
        ))]);
        let mut past = T::cand(2, "residence", "大阪", e);
        past.tense = Tense::Past;
        t.fold(vec![
            Event::Candidate(past),
            Event::Candidate(T::cand(2, "residence", "東京", e)),
        ]);
        let tokyo = t
            .set
            .facts
            .iter()
            .find(|f| f.value.as_deref() == Some("東京"))
            .expect("東京");
        assert_eq!(tokyo.state, FactState::Live);
        assert!(matches!(
            t.state_of("residence", "大阪")[..],
            [FactState::Superseded { by: Some(b), .. }] if b == tokyo.id
        ));
    }

    #[test]
    fn a_cancel_only_cancels_its_own_kind() {
        let mut t = T::new();
        let vk = value_key(&KEY, "えび");
        t.dirs.push(Directive {
            id: id(1),
            kind: DirectiveKind::Private {
                scope: Scope::ValueOnly,
            },
            subject: "self".into(),
            attribute: "dietary".into(),
            vk: Some(vk),
        });
        // 印を解く指示が private の指示を指していても、private は解けない。
        t.dirs.push(Directive {
            id: id(2),
            kind: DirectiveKind::Unblock { target: id(1) },
            subject: "self".into(),
            attribute: "dietary".into(),
            vk: None,
        });
        t.fold(vec![Event::Candidate(T::cand(
            3,
            "dietary",
            "えび",
            "えびは食べられない",
        ))]);
        assert!(t.set.facts[0].private);
    }

    #[test]
    fn a_successor_stays_private_when_an_older_source_is_added_later() {
        let mut t = T::new();
        t.dirs.push(Directive {
            id: id(5),
            kind: DirectiveKind::Private {
                scope: Scope::Successors,
            },
            subject: "self".into(),
            attribute: "dietary".into(),
            vk: Some(value_key(&KEY, "えび")),
        });
        t.fold(vec![Event::Candidate(T::cand(
            6,
            "dietary",
            "かに",
            "かにも食べられない",
        ))]);
        // 抽出が遅れた、指示より前のターンの出典が足される。
        t.fold(vec![Event::Candidate(T::cand(
            1,
            "dietary",
            "かに",
            "かにが苦手",
        ))]);
        assert!(t.set.facts.iter().all(|f| f.private));
    }

    impl T {
        /// ターン `turns` を消す ([`drop_sources`])。
        fn drop_turns(&mut self, turns: &[u64]) -> Dropped {
            let gone: Vec<Id> = turns.iter().map(|&n| id(n)).collect();
            let mut next = self.next;
            let mut new_id = || {
                next += 1;
                id(next)
            };
            let d = drop_sources(
                &mut self.set,
                &mut self.dirs,
                attributes(),
                &|turn| gone.contains(&turn),
                &mut new_id,
            );
            self.next = next;
            d
        }

        /// 事実を直接置く (畳みでは作れない形を含む)。
        fn put(&mut self, n: u64, a: &str, v: &str, state: FactState, turns: &[u64]) -> Id {
            self.set.facts.push(Fact {
                id: id(n),
                subject: "self".into(),
                attribute: a.into(),
                value: Some(v.into()),
                vk: Some(value_key(&KEY, v)),
                tense: Tense::Current,
                state,
                sources: turns
                    .iter()
                    .map(|&t| Source {
                        turn: id(t),
                        start: 0,
                        end: 3,
                        rev: 0,
                    })
                    .collect(),
                private: false,
            });
            id(n)
        }

        fn contested(&self, a: &str, v: &str) -> Option<Id> {
            match self.state_of(a, v)[..] {
                [FactState::Contested { against }] => Some(against),
                _ => None,
            }
        }
    }

    fn says(turn: u64, a: &str, v: &str) -> Event {
        Event::Candidate(T::cand(turn, a, v, &format!("{v}に住んでます")))
    }

    /// 消えた今の値より古い矛盾の値は、今の値にせず、`against` に消えた ID を指したまま残す。出典が 1 つも
    /// 残らない `Retracted` も消す。何も消えなければ何も変えない (04 §7.4・§7.1)。
    #[test]
    fn an_older_contested_value_stays_when_the_current_one_is_deleted() {
        let mut t = T::new();
        t.fold(vec![says(1, "residence", "大阪")]);
        t.fold(vec![says(2, "residence", "東京")]);
        t.fold(vec![says(3, "residence", "大阪")]);
        let osaka = t.set.facts[0].id;
        assert_eq!(t.contested("residence", "東京"), Some(osaka));
        let retracted = t.put(900, "hobby", "x", FactState::Retracted, &[3]);
        let d = t.drop_turns(&[1, 3]);
        let mut deleted = vec![osaka, retracted];
        deleted.sort_unstable();
        assert_eq!(d.deleted, deleted);
        assert!(d.promoted.is_empty());
        assert_eq!(t.contested("residence", "東京"), Some(osaka));
        assert!(t.live("residence").is_empty());
        let before = t.set.clone();
        assert!(t.drop_turns(&[]).is_empty());
        assert_eq!(t.set, before);
    }

    /// 繰り上げるのは、出典の最も新しいターンが最も後の矛盾の値。残りの矛盾の値は、その値に付け替える。
    #[test]
    fn the_newest_contested_value_is_promoted_and_the_others_follow_it() {
        let mut t = T::new();
        t.fold(vec![says(1, "residence", "大阪")]);
        t.fold(vec![says(2, "residence", "東京")]);
        t.fold(vec![says(4, "residence", "神戸")]);
        t.fold(vec![says(3, "residence", "京都")]);
        let d = t.drop_turns(&[1]);
        assert_eq!(t.live("residence"), ["神戸"]);
        let kobe = t
            .set
            .facts
            .iter()
            .find(|f| f.value.as_deref() == Some("神戸"));
        let kobe = kobe.expect("神戸").id;
        assert_eq!(d.promoted, [kobe]);
        assert_eq!(t.contested("residence", "東京"), Some(kobe));
        assert_eq!(t.contested("residence", "京都"), Some(kobe));
    }

    /// 出典の最も新しいターンが同じなら、事実の ID の大きい矛盾の値を繰り上げる (04 §7.4)。
    #[test]
    fn a_tie_promotes_the_larger_fact_id() {
        let mut t = T::new();
        let osaka = t.put(1, "residence", "大阪", FactState::Live, &[1]);
        let against = FactState::Contested { against: osaka };
        t.put(2, "residence", "東京", against, &[2]);
        let kyoto = t.put(3, "residence", "京都", against, &[2]);
        let d = t.drop_turns(&[1]);
        assert_eq!(d.promoted, [kyoto]);
        assert_eq!(t.contested("residence", "東京"), Some(kyoto));
    }

    /// 矛盾の値の出典の一部が同時に消えたら、残った出典の最も新しいターンで比べる。
    #[test]
    fn a_partly_removed_contested_value_is_judged_by_its_remaining_sources() {
        let mut t = T::new();
        let osaka = t.put(1, "residence", "大阪", FactState::Live, &[2]);
        t.put(
            2,
            "residence",
            "東京",
            FactState::Contested { against: osaka },
            &[1, 3],
        );
        let d = t.drop_turns(&[2, 3]);
        assert_eq!(d.deleted, [osaka]);
        assert_eq!(d.trimmed, 1);
        assert!(d.promoted.is_empty());
        assert_eq!(t.contested("residence", "東京"), Some(osaka));
    }

    /// 予定の値・多値の属性の値・過去形や否定で終えた値・もともと出典の無い値が消えても、矛盾の値は
    /// 繰り上げない (04 §7.4)。
    #[test]
    fn only_a_deleted_current_single_value_promotes() {
        // 予定の値。
        let mut t = T::new();
        let osaka = t.put(1, "residence", "大阪", FactState::Live, &[1]);
        t.put(
            2,
            "residence",
            "東京",
            FactState::Contested { against: osaka },
            &[2],
        );
        t.put(3, "residence", "京都", FactState::Live, &[3]);
        t.set.facts[2].tense = Tense::Planned;
        let d = t.drop_turns(&[3]);
        assert!(d.promoted.is_empty());
        assert_eq!(t.live("residence"), ["大阪"]);
        assert_eq!(t.contested("residence", "東京"), Some(osaka));

        // 多値の属性 (畳みでは作らない形でも、繰り上げない)。
        let mut t = T::new();
        let ramen = t.put(1, "likes", "ラーメン", FactState::Live, &[1]);
        t.put(
            2,
            "likes",
            "うどん",
            FactState::Contested { against: ramen },
            &[2],
        );
        assert!(t.drop_turns(&[1]).promoted.is_empty());
        assert_eq!(t.contested("likes", "うどん"), Some(ramen));

        // 過去形・否定で終えた値。
        let mut t = T::new();
        let ended = FactState::Superseded {
            by: None,
            at: id(3).timestamp(),
        };
        let osaka = t.put(1, "residence", "大阪", ended, &[1, 3]);
        t.put(
            2,
            "residence",
            "東京",
            FactState::Contested { against: osaka },
            &[2],
        );
        let d = t.drop_turns(&[1, 3]);
        assert_eq!(d.deleted, [osaka]);
        assert!(d.promoted.is_empty());
        assert_eq!(t.contested("residence", "東京"), Some(osaka));

        // もともと出典の無い値は、関係の無い削除では消さない。
        let mut t = T::new();
        let osaka = t.put(1, "residence", "大阪", FactState::Live, &[]);
        t.put(
            2,
            "residence",
            "東京",
            FactState::Contested { against: osaka },
            &[2],
        );
        t.put(3, "hobby", "釣り", FactState::Live, &[3]);
        let d = t.drop_turns(&[3]);
        assert_eq!(d.deleted, [id(3)]);
        assert!(d.promoted.is_empty());
        assert_eq!(t.live("residence"), ["大阪"]);
    }

    /// 削除で出典が外れて後継でなくなる値には、その値だけの private の指示を足し、private のまま残す
    /// (04 §7.5 の「後継」)。
    #[test]
    fn a_successor_stays_private_after_its_later_source_is_deleted() {
        let mut t = T::new();
        t.fold(vec![says(1, "residence", "大阪")]);
        t.dirs.push(Directive {
            id: id(2),
            kind: DirectiveKind::Private {
                scope: Scope::Successors,
            },
            subject: "self".into(),
            attribute: "residence".into(),
            vk: Some(value_key(&KEY, "京都")),
        });
        t.fold(vec![says(3, "residence", "大阪")]);
        assert!(t.set.facts[0].private, "a successor after the restatement");
        let d = t.drop_turns(&[3]);
        assert!(t.set.facts[0].private, "still private after the deletion");
        let [added] = &d.directives[..] else {
            panic!("one directive: {:?}", d.directives);
        };
        assert_eq!(
            added.kind,
            DirectiveKind::Private {
                scope: Scope::ValueOnly
            }
        );
        assert_eq!(added.vk, Some(value_key(&KEY, "大阪")));
        assert!(t.dirs.iter().any(|x| x == added));
        // もう一度消しても、指示を重ねない。
        assert!(t.drop_turns(&[3]).directives.is_empty());
    }
}
