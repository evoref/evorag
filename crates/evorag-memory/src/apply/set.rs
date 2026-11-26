//! 指示の集合と、指示ごとの当て方 (04 §7.4・§7.5・§7.6、§5.2 の関門 8、§5.3 の入力の順)。

use std::collections::{BTreeMap, BTreeSet};

use evorag_types::Id;

use super::keys::substring_keys;
use super::marks::TurnRedactions;
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveOrigin, PrivateScope, TurnSpan};
use crate::value_key::{ForgetKey, ValueKey, loose_key, normalize_loose, normalize_v1, value_key};

/// 値の鍵と照合の鍵の組 (04 §5.6)。指示の当て方は、相手の値のこの 2 つで引く (§7.6)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueKeys {
    /// 値の鍵 (正規化 v1)。
    pub vk: ValueKey,
    /// 照合の鍵 (照合の形 v1)。落とす空白の無い値では `vk` と同じ。
    pub loose: ValueKey,
}

impl ValueKeys {
    /// 値 `value` の 2 つの鍵。
    #[must_use]
    pub fn of(key: &ForgetKey, value: &str) -> Self {
        Self {
            vk: value_key(key, value),
            loose: loose_key(key, value),
        }
    }

    /// 指示の `vks` に書く形 (64 桁の小文字の 16 進。2 つが同じなら 1 つ、04 §7.6)。
    #[must_use]
    pub fn to_hex(&self) -> Vec<String> {
        let mut out = vec![self.vk.to_hex()];
        if self.loose != self.vk {
            out.push(self.loose.to_hex());
        }
        out
    }

    fn any_in(&self, set: &BTreeSet<ValueKey>) -> bool {
        set.contains(&self.vk) || set.contains(&self.loose)
    }
}

/// 指示を当てる相手の属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAspect<'a> {
    /// 辞書の属性 (名前)。`was_free` は、属性の決め方 (04 §5.2) で辞書の属性に写す前の、自由記述の属性の鍵
    /// (`aspect_mac` と同じ 16 進。写していなければ `None`)。`end` は写す前の鍵とも比べる (04 §7.5 の「当て方の補い」)。
    Dict {
        name: &'a str,
        was_free: Option<&'a str>,
    },
    /// 自由記述の属性 (属性の鍵の 16 進、04 §5.6)。
    Free { mac: &'a str },
}

/// 指示を当てる相手 (候補・訂正の新しい値・戻す値・単位)。
///
/// `entity` は記録する主語 (04 §5.2 の関門 5): `self` か、呼び名に正規化 v1 を当てた文字列。主語を明示する語
/// (`subject_self`) は、呼び手が `self` に直してから渡す (04 §5.1。主語を「私」と書いて忘れるを抜けないため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe<'a> {
    entity_v1: String,
    entity_loose: String,
    aspect: ProbeAspect<'a>,
    keys: ValueKeys,
    turn: Id,
}

impl<'a> Probe<'a> {
    /// `turn` は根拠のターン (印を解いた指示の比べ、04 §7.5 の `unblock`)。単位なら根拠の最も新しいターン。
    #[must_use]
    pub fn new(entity: &str, aspect: ProbeAspect<'a>, keys: ValueKeys, turn: Id) -> Self {
        Self {
            entity_v1: normalize_v1(entity),
            entity_loose: normalize_loose(entity),
            aspect,
            keys,
            turn,
        }
    }

    #[must_use]
    pub fn aspect(&self) -> ProbeAspect<'a> {
        self.aspect
    }

    #[must_use]
    pub fn keys(&self) -> ValueKeys {
        self.keys
    }

    #[must_use]
    pub fn turn(&self) -> Id {
        self.turn
    }
}

/// private・採る・戻すを当てる単位 (根拠の位置で当てるため、根拠の値の範囲を添える)。
#[derive(Debug, Clone, Copy)]
pub struct UnitProbe<'p, 'a> {
    /// 単位の鍵。`turn` は根拠の最も新しいターン (後継の比べ、04 §7.5 の「後継」)。
    pub probe: &'p Probe<'a>,
    /// 根拠ごとの値の範囲 (ターン・改訂の番号・バイト範囲。04 §5.1 の `anchors` の `start`・`end`)。
    pub anchors: &'p [TurnSpan],
}

/// entity の同値類 (04 §7.6): 正規化 v1 か照合の形で同じ呼び名と、指示が持つその時点の別名 (`aliases`)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EntityClass {
    v1: BTreeSet<String>,
    loose: BTreeSet<String>,
    /// entity を持たない (壊れた) 指示。安全の側に倒して、どの entity にも当てる。
    any: bool,
}

impl EntityClass {
    fn of(entity: Option<&str>, aliases: &[String]) -> Self {
        let Some(entity) = entity else {
            return Self {
                any: true,
                ..Self::default()
            };
        };
        let names = std::iter::once(entity).chain(aliases.iter().map(String::as_str));
        let mut out = Self::default();
        for n in names {
            out.v1.insert(normalize_v1(n));
            out.loose.insert(normalize_loose(n));
        }
        out
    }

    fn contains(&self, p: &Probe<'_>) -> bool {
        self.any || self.v1.contains(&p.entity_v1) || self.loose.contains(&p.entity_loose)
    }
}

/// 指示の属性 (04 §7.5 の `aspect`・`aspect_mac`)。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Aspect {
    Dict(String),
    Free(String),
    /// どちらも無い (壊れた) 指示。属性を見ない側に倒す。
    Unknown,
}

impl Aspect {
    fn of(b: &DirectiveBody) -> Self {
        match (&b.aspect, &b.aspect_mac) {
            (Some(a), _) => Aspect::Dict(a.clone()),
            (None, Some(m)) => Aspect::Free(m.clone()),
            (None, None) => Aspect::Unknown,
        }
    }

    /// 忘れるの当て方の述語の属性の比べ (04 §7.5): 辞書の属性の指示と辞書の属性の相手のときだけ名前を比べる。
    /// 自由記述の属性の指示はどの相手にも、どの指示も自由記述の属性の相手に、属性を見ずに当てる。
    fn admits(&self, p: &Probe<'_>) -> bool {
        match (self, p.aspect) {
            (Aspect::Dict(a), ProbeAspect::Dict { name, .. }) => a == name,
            _ => true,
        }
    }

    /// 同じ (entity, 属性) の組か (`end`・採る・戻す)。辞書の属性は名前、自由記述の属性は鍵で比べ、辞書の写しの
    /// 前の自由記述の属性の鍵とも比べる (04 §7.5 の「当て方の補い」)。
    fn same_group(&self, p: &Probe<'_>) -> bool {
        match (self, p.aspect) {
            (Aspect::Dict(a), ProbeAspect::Dict { name, .. }) => a == name,
            (Aspect::Free(m), ProbeAspect::Free { mac }) => m == mac,
            (Aspect::Free(m), ProbeAspect::Dict { was_free, .. }) => was_free == Some(m.as_str()),
            (Aspect::Dict(_), ProbeAspect::Free { .. }) => false,
            (Aspect::Unknown, _) => true,
        }
    }
}

/// 記録の `vks` (16 進の並び) を読む。読めない要素は捨てる (当たらない)。
fn parse_vks(vks: &[String]) -> BTreeSet<ValueKey> {
    vks.iter().filter_map(|h| ValueKey::from_hex(h)).collect()
}

/// 根拠の位置 `at` が、根拠の値の範囲 `anchor` に当たるか (04 §7.5)。`at` は指示を書いたときの改訂の番号の位置
/// なので、後の忘れるの伏せ字で写してから比べる (§7.3)。取り込んだ指示は範囲が重なれば当たり、ほかは同じ範囲の
/// ときだけ当たる (§7.5 の「取り込んだ指示の位置」)。
fn at_hits(at: &TurnSpan, imported: bool, anchor: &TurnSpan, red: &TurnRedactions) -> bool {
    if at.turn != anchor.turn {
        return false;
    }
    let Some(r) = red.remap(
        at.turn,
        at.rev,
        to_usize(at.start)..to_usize(at.end),
        anchor.rev,
    ) else {
        return false;
    };
    let a = to_usize(anchor.start)..to_usize(anchor.end);
    if imported {
        r.start < a.end && a.start < r.end
    } else {
        r == a
    }
}

fn to_usize(p: u32) -> usize {
    usize::try_from(p).unwrap_or(usize::MAX)
}

/// 忘れるの指示 1 つ (04 §7.5 の `forget`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forget {
    id: Id,
    vks: BTreeSet<ValueKey>,
    entity: EntityClass,
    aspect: Aspect,
    unblocked: Option<Id>,
    targets: BTreeSet<Id>,
}

impl Forget {
    fn new(id: Id, b: &DirectiveBody) -> Self {
        let targets = b
            .turns
            .iter()
            .copied()
            .chain(b.redactions.iter().map(|r| r.turn))
            .chain(b.hidden.iter().map(|h| h.turn))
            .collect();
        Self {
            id,
            vks: parse_vks(&b.vks),
            entity: EntityClass::of(b.entity.as_deref(), &b.aliases),
            aspect: Aspect::of(b),
            unblocked: None,
            targets,
        }
    }

    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// 印を解いた `unblock` の指示の ID (無ければ `None`)。同じ忘れるを解く `unblock` が 2 つ以上あれば、最も早い
    /// もの (04 §7.5)。
    #[must_use]
    pub fn unblocked(&self) -> Option<Id> {
        self.unblocked
    }

    /// 根拠のターン `turn` の相手に、この指示を当てるか (04 §7.5 の `unblock`): 印を解いていないか、`turn` が
    /// `unblock` の指示の ID より前のとき。
    #[must_use]
    pub fn applies_at(&self, turn: Id) -> bool {
        self.unblocked.is_none_or(|u| turn < u)
    }

    /// 値の鍵か照合の鍵の 1 つが `vks` にあるか。
    #[must_use]
    pub fn has_key(&self, k: &ValueKey) -> bool {
        self.vks.contains(k)
    }

    pub(crate) fn vks(&self) -> &BTreeSet<ValueKey> {
        &self.vks
    }

    /// 対象のターン (04 §7.2 の台帳の掃除): 伏せ字にしたターン、忘れた単位の根拠のターン (`turns`)、`[非表示]` の
    /// 範囲を持つターン。
    #[must_use]
    pub fn targets(&self) -> &BTreeSet<Id> {
        &self.targets
    }

    /// 当て方の述語 (04 §7.5、§7.6、§5.2 の関門 8): 相手の値の鍵か照合の鍵が `vks` にあり、相手の entity が指示の
    /// entity の同値類にあり、辞書の属性どうしなら属性も同じで、印を解いた指示なら相手の根拠のターンが `unblock`
    /// の指示の ID より前のとき。
    #[must_use]
    pub fn hits(&self, p: &Probe<'_>) -> bool {
        self.applies_at(p.turn)
            && p.keys.any_in(&self.vks)
            && self.entity.contains(p)
            && self.aspect.admits(p)
    }
}

/// private の指示 1 つ (04 §7.5 の `private`)。`private_off` で解いたものは [`DirectiveSet`] に入らない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Private {
    id: Id,
    vks: BTreeSet<ValueKey>,
    entity: EntityClass,
    aspect: Aspect,
    at: Option<TurnSpan>,
    imported: bool,
    scope: PrivateScope,
    origin: DirectiveOrigin,
}

/// private が単位に当たった理由 (04 §7.5 の「private の理由の表示」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivateHit {
    /// 値の鍵か照合の鍵 (`vks`) で当たった。
    Value,
    /// 根拠の位置 (`at`) で当たった。
    Position,
    /// 後継 (同じ (entity, 辞書の属性) に指示の後に言われた値) で当たった。
    Successor,
}

impl Private {
    fn new(id: Id, b: &DirectiveBody) -> Self {
        Self {
            id,
            vks: parse_vks(&b.vks),
            entity: EntityClass::of(b.entity.as_deref(), &b.aliases),
            aspect: Aspect::of(b),
            at: b.at,
            imported: b.imported,
            scope: b.scope().unwrap_or(PrivateScope::Successors),
            origin: b.origin(),
        }
    }

    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    #[must_use]
    pub fn scope(&self) -> PrivateScope {
        self.scope
    }

    #[must_use]
    pub fn origin(&self) -> DirectiveOrigin {
        self.origin
    }

    /// 単位に当たるか (04 §7.5): 値の鍵か照合の鍵 (`vks`。entity の同値類と属性は忘れるの当て方の述語と同じ形で
    /// 比べる)、根拠の位置、後継のどれか。後継は「後継まで」の辞書の属性の指示だけで、同じ (entity, 辞書の属性)
    /// の単位のうち根拠の最も新しいターンが指示の ID より後のもの。
    #[must_use]
    pub fn hits(&self, u: &UnitProbe<'_, '_>, red: &TurnRedactions) -> Option<PrivateHit> {
        let p = u.probe;
        if p.keys.any_in(&self.vks) && self.entity.contains(p) && self.aspect.admits(p) {
            return Some(PrivateHit::Value);
        }
        if let Some(at) = &self.at
            && u.anchors.iter().any(|a| at_hits(at, self.imported, a, red))
        {
            return Some(PrivateHit::Position);
        }
        let successor = self.scope == PrivateScope::Successors
            && matches!((&self.aspect, p.aspect), (Aspect::Dict(a), ProbeAspect::Dict { name, .. }) if a == name)
            && self.entity.contains(p)
            && p.turn > self.id;
        successor.then_some(PrivateHit::Successor)
    }

    /// 区切りの鍵 `text` の語句か部分文字列の値の鍵か照合の鍵が、この指示の `vks` に当たるか (04 §5.5)。
    fn screens(&self, keys: &BTreeSet<ValueKey>) -> bool {
        self.vks.iter().any(|k| keys.contains(k))
    }
}

/// 採る・戻す (04 §7.5 の `accept`・`revert`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdoptOp {
    Accept,
    Revert,
}

/// 採る・戻すの指示 1 つ。導出の入力の順の中で、指示の ID の時点で 1 回だけ当てる出来事 (04 §7.5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    id: Id,
    op: AdoptOp,
    vks: BTreeSet<ValueKey>,
    entity: EntityClass,
    aspect: Aspect,
    at: Option<TurnSpan>,
    imported: bool,
}

/// 採る・戻すが単位に当たった理由。根拠の位置で当たった単位を、値の鍵で当たった単位より先に採る (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdoptHit {
    Position,
    Value,
}

impl Adoption {
    fn new(id: Id, op: AdoptOp, b: &DirectiveBody) -> Self {
        Self {
            id,
            op,
            vks: parse_vks(&b.vks),
            entity: EntityClass::of(b.entity.as_deref(), &b.aliases),
            aspect: Aspect::of(b),
            at: b.at,
            imported: b.imported,
        }
    }

    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    #[must_use]
    pub fn op(&self) -> AdoptOp {
        self.op
    }

    /// 単位に当たるか (04 §7.5): 根拠の位置で探し、無ければ値の鍵 (正規化 v1 の値の鍵だけ、§5.6) で、同じ
    /// (entity, 属性) の組のものに当てる。競合の組 (`Contested` か履歴) にあるかと、忘れた値でないか (関門 8 の
    /// 述語、[`DirectiveSet::forgotten_by`]) は、畳みの側で確かめる。
    #[must_use]
    pub fn hits(&self, u: &UnitProbe<'_, '_>, red: &TurnRedactions) -> Option<AdoptHit> {
        if let Some(at) = &self.at
            && u.anchors.iter().any(|a| at_hits(at, self.imported, a, red))
        {
            return Some(AdoptHit::Position);
        }
        let p = u.probe;
        (self.vks.contains(&p.keys.vk) && self.entity.contains(p) && self.aspect.same_group(p))
            .then_some(AdoptHit::Value)
    }
}

/// `end` の指示 1 つ (04 §7.4)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    id: Id,
    vks: BTreeSet<ValueKey>,
    entity_raw: Option<String>,
    aliases: Vec<String>,
    aspect: Aspect,
    turn: Option<Id>,
    cause: Option<Id>,
    origin: DirectiveOrigin,
}

impl End {
    fn new(id: Id, b: &DirectiveBody) -> Self {
        Self {
            id,
            vks: parse_vks(&b.vks),
            entity_raw: b.entity_raw.clone().or_else(|| b.entity.clone()),
            aliases: b.aliases.clone(),
            aspect: Aspect::of(b),
            turn: b.turn,
            cause: b.cause,
            origin: b.origin(),
        }
    }

    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// 終えた位置 (ターンの ID。決まらなかった `end` は指示の ID、04 §7.4)。
    #[must_use]
    pub fn position(&self) -> Id {
        self.turn.unwrap_or(self.id)
    }

    /// きっかけの忘れるの指示 (削除と取り込みでは `None`)。
    #[must_use]
    pub fn cause(&self) -> Option<Id> {
        self.cause
    }

    #[must_use]
    pub fn origin(&self) -> DirectiveOrigin {
        self.origin
    }

    /// 単位 (その時点の `Live`) に当たるか (04 §7.4 の「`end` の当て方」): 値の鍵か照合の鍵が `vks` にあり、
    /// 同じ (entity, 属性) の組のとき。entity は、指示が持つ生の文字列を今の規則で解き直して比べる (`resolve`:
    /// 正規化 v1 と主語を明示する語。呼び手が今の規則を渡す)。状態が `Live` かは畳みの側で確かめる。
    #[must_use]
    pub fn hits(&self, p: &Probe<'_>, resolve: impl Fn(&str) -> String) -> bool {
        let entity = EntityClass::of(
            self.entity_raw.as_deref().map(&resolve).as_deref(),
            &self.aliases,
        );
        p.keys.any_in(&self.vks) && entity.contains(p) && self.aspect.same_group(p)
    }
}

/// 畳みの列の中の位置 (04 §5.3 の「入力の順」)。指示もターンも UUID v7 なので、1 本の列に並ぶ。
///
/// 候補は (根拠のターンの ID, 値の始まり …) の順に並び、そのターンの候補は `Slot { at: ターン, phase:
/// Candidates }` の位置にある。`end` は終えた位置のターンの候補の後 (`AfterCandidates`)、採る・戻すは指示の ID の
/// 位置に置く。同じ位置の指示は指示の ID の順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot {
    pub at: Id,
    pub phase: Phase,
    pub directive: Id,
}

/// [`Slot`] の中の前後。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// その位置の候補と同じ所 (採る・戻すは、指示の ID のターンが無いので、前後の候補との順だけが効く)。
    Candidates,
    /// その位置のターンの候補の後 (`end`)。
    AfterCandidates,
}

/// 畳みの列に混ぜる指示 ([`DirectiveSet::timeline`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timed<'s> {
    Adopt(&'s Adoption),
    End(&'s End),
}

/// 効いている指示の集合 (04 §7.5)。指示の ID の順に並べ、解く指示 (`private_off`・`unblock`) を解く相手に当てた
/// 後のもの。入力の順に依らず、同じ指示の集合から同じものを作る。
///
/// - `private_off` は private の指示だけを、`unblock` は忘れるの指示だけを解く (種類の違う相手・無い相手を指す
///   解く指示は当てない、04 §7.5 の「解く指示の相手」)。どちらも時点に依らず常に効く。
/// - 知らない `op` の指示は当てずに数える ([`DirectiveSet::unknown`])。
#[derive(Debug, Clone)]
pub struct DirectiveSet {
    forgets: Vec<Forget>,
    privates: Vec<Private>,
    adoptions: Vec<Adoption>,
    ends: Vec<End>,
    redactions: TurnRedactions,
    unknown: usize,
    unmatched_cancels: usize,
}

impl DirectiveSet {
    /// 指示の (ID, 本体) から作る。`redaction_marker` は伏せ字の印 (位置の写し直しに使う。データ根のプロンプトの
    /// 言語の印、04 §7.3)。
    #[must_use]
    pub fn new(
        directives: impl IntoIterator<Item = (Id, DirectiveBody)>,
        redaction_marker: &str,
    ) -> Self {
        let all: BTreeMap<Id, DirectiveBody> = directives.into_iter().collect();
        let mut unblocks: BTreeMap<Id, Id> = BTreeMap::new();
        let mut cancelled: BTreeSet<Id> = BTreeSet::new();
        let mut unknown = 0;
        let mut unmatched_cancels = 0;
        for (&id, b) in &all {
            let wanted = match b.op() {
                DirectiveOp::Unblock => DirectiveOp::Forget,
                DirectiveOp::PrivateOff => DirectiveOp::Private,
                DirectiveOp::Unknown => {
                    unknown += 1;
                    continue;
                }
                _ => continue,
            };
            let Some(target) = b
                .target
                .filter(|t| all.get(t).is_some_and(|t| t.op() == wanted))
            else {
                unmatched_cancels += 1;
                continue;
            };
            if wanted == DirectiveOp::Forget {
                unblocks
                    .entry(target)
                    .and_modify(|u| *u = (*u).min(id))
                    .or_insert(id);
            } else {
                cancelled.insert(target);
            }
        }
        let mut out = Self {
            forgets: Vec::new(),
            privates: Vec::new(),
            adoptions: Vec::new(),
            ends: Vec::new(),
            redactions: TurnRedactions::new(
                all.iter()
                    .filter(|(_, b)| b.op() == DirectiveOp::Forget)
                    .map(|(id, b)| (*id, b)),
                redaction_marker,
            ),
            unknown,
            unmatched_cancels,
        };
        for (&id, b) in &all {
            match b.op() {
                DirectiveOp::Forget => {
                    let mut f = Forget::new(id, b);
                    f.unblocked = unblocks.get(&id).copied();
                    out.forgets.push(f);
                }
                DirectiveOp::Private if !cancelled.contains(&id) => {
                    out.privates.push(Private::new(id, b));
                }
                DirectiveOp::Accept => out.adoptions.push(Adoption::new(id, AdoptOp::Accept, b)),
                DirectiveOp::Revert => out.adoptions.push(Adoption::new(id, AdoptOp::Revert, b)),
                DirectiveOp::End => out.ends.push(End::new(id, b)),
                _ => {}
            }
        }
        out
    }

    /// 忘れるの指示 (印を解いたものを含む。指示の ID の順)。台帳の掃除は、印を解いた指示も根拠のターンで当て続ける。
    #[must_use]
    pub fn forgets(&self) -> &[Forget] {
        &self.forgets
    }

    /// 効いている private の指示 (`private_off` で解いたものを除く。指示の ID の順)。
    #[must_use]
    pub fn privates(&self) -> &[Private] {
        &self.privates
    }

    /// 採る・戻すの指示 (指示の ID の順)。
    #[must_use]
    pub fn adoptions(&self) -> &[Adoption] {
        &self.adoptions
    }

    /// `end` の指示 (指示の ID の順)。
    #[must_use]
    pub fn ends(&self) -> &[End] {
        &self.ends
    }

    /// 忘れるの伏せ字と `[非表示]` の範囲 (ターンごと、指示の ID の順)。
    #[must_use]
    pub fn redactions(&self) -> &TurnRedactions {
        &self.redactions
    }

    /// 知らない `op` の指示の数 (当てずに残す。`doctor` が報告する、04 §7.5)。
    #[must_use]
    pub fn unknown(&self) -> usize {
        self.unknown
    }

    /// 相手を見つけられなかった解く指示 (`private_off`・`unblock`) の数。
    #[must_use]
    pub fn unmatched_cancels(&self) -> usize {
        self.unmatched_cancels
    }

    /// 関門 8 (`forgotten`、04 §5.2) と当て方の述語 (§7.5): 相手に当たる忘れるの指示のうち、ID の最も小さいもの。
    /// 当たれば相手を黙って捨てて数える (§7.6)。訂正の新しい値と戻す値にも、同じ関数で当てる (§7.5 の「忘れるの
    /// 指示と訂正・戻す」)。
    #[must_use]
    pub fn forgotten_by(&self, p: &Probe<'_>) -> Option<Id> {
        self.forgets.iter().find(|f| f.hits(p)).map(Forget::id)
    }

    /// 単位に当たる private の指示のうち、ID の最も小さいものと理由 (04 §7.5)。当たれば単位に private の印を付ける。
    #[must_use]
    pub fn private_hit(&self, u: &UnitProbe<'_, '_>) -> Option<(Id, PrivateHit)> {
        self.privates
            .iter()
            .find_map(|p| p.hits(u, &self.redactions).map(|h| (p.id, h)))
    }

    /// 区切りの鍵 `text` を、private の値で導出の時点で除くか (04 §5.5): 語句か部分文字列の値の鍵か照合の鍵が、
    /// 効いている private の指示の `vks` に当たるとき。
    #[must_use]
    pub fn screens_key(&self, key: &ForgetKey, text: &str) -> bool {
        if self.privates.is_empty() {
            return false;
        }
        let keys = substring_keys(key, text);
        self.privates.iter().any(|p| p.screens(&keys))
    }

    /// 畳みの列に混ぜる指示 (採る・戻す・`end`) を、列の位置の順に並べたもの (04 §5.3 の「入力の順」)。
    #[must_use]
    pub fn timeline(&self) -> Vec<(Slot, Timed<'_>)> {
        let mut out: Vec<(Slot, Timed<'_>)> = self
            .adoptions
            .iter()
            .map(|a| {
                (
                    Slot {
                        at: a.id,
                        phase: Phase::Candidates,
                        directive: a.id,
                    },
                    Timed::Adopt(a),
                )
            })
            .chain(self.ends.iter().map(|e| {
                (
                    Slot {
                        at: e.position(),
                        phase: Phase::AfterCandidates,
                        directive: e.id,
                    },
                    Timed::End(e),
                )
            }))
            .collect();
        out.sort_by_key(|(s, _)| *s);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::DirectiveRedaction;
    use crate::value_key::aspect_key;

    const K: ForgetKey = ForgetKey::from_bytes_const([3; 32]);
    const M: &str = "[伏せ字]";

    /// 時刻の順に並ぶ ID (UUID v7 の形でなくても、バイト列の順で比べられればよい)。
    fn id(n: u64) -> Id {
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(b)
    }

    fn mac(aspect: &str) -> String {
        aspect_key(&K, aspect).to_hex()
    }

    fn forget(entity: &str, aspect: ProbeAspect<'_>, value: &str) -> DirectiveBody {
        let mut b = DirectiveBody::new(DirectiveOp::Forget);
        b.entity = Some(entity.to_owned());
        match aspect {
            ProbeAspect::Dict { name, .. } => b.aspect = Some(name.to_owned()),
            ProbeAspect::Free { mac } => b.aspect_mac = Some(mac.to_owned()),
        }
        b.vks = ValueKeys::of(&K, value).to_hex();
        b
    }

    fn cancel(op: DirectiveOp, target: Id) -> DirectiveBody {
        let mut b = DirectiveBody::new(op);
        b.target = Some(target);
        b
    }

    fn dict(name: &str) -> ProbeAspect<'_> {
        ProbeAspect::Dict {
            name,
            was_free: None,
        }
    }

    fn probe<'a>(entity: &str, aspect: ProbeAspect<'a>, value: &str, turn: u64) -> Probe<'a> {
        Probe::new(entity, aspect, ValueKeys::of(&K, value), id(turn))
    }

    fn set(ds: Vec<(u64, DirectiveBody)>) -> DirectiveSet {
        DirectiveSet::new(ds.into_iter().map(|(n, b)| (id(n), b)), M)
    }

    /// verifies: 04 §7.5 (当て方の述語。値の鍵か照合の鍵・entity の同値類・辞書の属性どうしは属性も比べる)、§5.2 の関門 8
    #[test]
    fn the_forget_predicate_compares_keys_entity_and_dictionary_attribute() {
        let s = set(vec![(10, forget("self", dict("residence"), "7月22日"))]);
        let hit = |p: &Probe<'_>| s.forgotten_by(p);
        // 同じ値・照合の形だけが違う値 (漢字に接する空白)・全角。
        assert_eq!(
            hit(&probe("self", dict("residence"), "7月22日", 1)),
            Some(id(10))
        );
        assert_eq!(
            hit(&probe("self", dict("residence"), "7 月 22 日", 1)),
            Some(id(10))
        );
        assert_eq!(
            hit(&probe("self", dict("residence"), "７月２２日", 20)),
            Some(id(10))
        );
        // 別の辞書の属性の同じ値には当たらない。
        assert_eq!(hit(&probe("self", dict("birthday"), "7月22日", 1)), None);
        // 自由記述の属性の相手には、辞書の属性の指示も属性を見ずに当てる。
        let m = mac("記念日");
        assert_eq!(
            hit(&probe("self", ProbeAspect::Free { mac: &m }, "7月22日", 1)),
            Some(id(10))
        );
        // 別の entity には当たらない。
        assert_eq!(hit(&probe("妻", dict("residence"), "7月22日", 1)), None);
        // 違う値には当たらない (英数字の間の空白の違いは同じ値とみなさない、§7.8)。
        assert_eq!(hit(&probe("self", dict("residence"), "7月23日", 1)), None);
    }

    /// verifies: 04 §7.5 (自由記述の属性の指示は属性を見ない)、§7.6
    #[test]
    fn a_free_attribute_forget_ignores_the_attribute() {
        let m = mac("よく行く店");
        let s = set(vec![(
            10,
            forget("self", ProbeAspect::Free { mac: &m }, "喫茶ミモザ"),
        )]);
        let other = mac("行きつけの喫茶店");
        assert!(
            s.forgotten_by(&probe(
                "self",
                ProbeAspect::Free { mac: &other },
                "喫茶ミモザ",
                1
            ))
            .is_some()
        );
        assert!(
            s.forgotten_by(&probe("self", dict("likes"), "喫茶ミモザ", 1))
                .is_some()
        );
        assert!(
            s.forgotten_by(&probe("self", dict("likes"), "喫茶ミモザのナポリタン", 1))
                .is_none()
        );
    }

    /// verifies: 04 §7.6 (entity の同値類: 正規化 v1 か照合の形で同じ呼び名と別名)
    #[test]
    fn the_entity_class_covers_normalized_names_and_aliases() {
        let mut b = forget("田中 さん", dict("hobby"), "将棋");
        b.aliases = vec!["課長".to_owned()];
        let s = set(vec![(10, b)]);
        assert!(
            s.forgotten_by(&probe("田中さん", dict("hobby"), "将棋", 1))
                .is_some()
        );
        assert!(
            s.forgotten_by(&probe("ＴＡＮＡＫＡ", dict("hobby"), "将棋", 1))
                .is_none()
        );
        assert!(
            s.forgotten_by(&probe("課長", dict("hobby"), "将棋", 1))
                .is_some()
        );
        // entity の無い (壊れた) 指示は、安全の側に倒してどの entity にも当てる。
        let mut broken = forget("self", dict("hobby"), "囲碁");
        broken.entity = None;
        let s = set(vec![(10, broken)]);
        assert!(
            s.forgotten_by(&probe("妻", dict("hobby"), "囲碁", 1))
                .is_some()
        );
    }

    /// verifies: 04 §7.5 (unblock は根拠のターンの ID で決める。解く相手の種類が違えば当てない)
    #[test]
    fn unblock_releases_only_later_evidence() {
        let s = set(vec![
            (10, forget("self", dict("name"), "ハッカ")),
            (20, cancel(DirectiveOp::Unblock, id(10))),
        ]);
        assert_eq!(s.forgets()[0].unblocked(), Some(id(20)));
        assert!(
            s.forgotten_by(&probe("self", dict("name"), "ハッカ", 15))
                .is_some()
        );
        assert!(
            s.forgotten_by(&probe("self", dict("name"), "ハッカ", 25))
                .is_none()
        );
        // 2 つ目の unblock があっても、最も早い unblock で決める。
        let s = set(vec![
            (10, forget("self", dict("name"), "ハッカ")),
            (30, cancel(DirectiveOp::Unblock, id(10))),
            (20, cancel(DirectiveOp::Unblock, id(10))),
        ]);
        assert_eq!(s.forgets()[0].unblocked(), Some(id(20)));
        // private_off は忘れるを解かない。unblock は private を解かない。無い相手も数えるだけ。
        let mut p = forget("self", dict("name"), "ハッカ");
        p.op = "private".to_owned();
        let s = set(vec![
            (10, forget("self", dict("name"), "ハッカ")),
            (11, p),
            (20, cancel(DirectiveOp::PrivateOff, id(10))),
            (21, cancel(DirectiveOp::Unblock, id(11))),
            (22, cancel(DirectiveOp::Unblock, id(99))),
        ]);
        assert_eq!(s.forgets()[0].unblocked(), None);
        assert_eq!(s.privates().len(), 1);
        assert_eq!(s.unmatched_cancels(), 3);
    }

    fn private(value: &str, scope: PrivateScope, at: Option<TurnSpan>) -> DirectiveBody {
        let mut b = forget("self", dict("dietary"), value);
        b.op = "private".to_owned();
        b.scope = Some(scope.name().to_owned());
        b.at = at;
        b
    }

    fn span(turn: u64, rev: u32, start: u32, end: u32) -> TurnSpan {
        TurnSpan {
            turn: id(turn),
            rev,
            start,
            end,
        }
    }

    /// verifies: 04 §7.5 (private は vks か根拠の位置で当たり、「後継まで」は後に言われた同じ (entity, 辞書の属性) の値にも当たる。private_off で解く)
    #[test]
    fn private_hits_by_value_position_and_successor() {
        let s = set(vec![(
            10,
            private("くるみ", PrivateScope::Successors, Some(span(1, 0, 0, 9))),
        )]);
        let unit = |value: &str, turn: u64, anchors: &[TurnSpan]| {
            let p = probe("self", dict("dietary"), value, turn);
            s.private_hit(&UnitProbe { probe: &p, anchors })
        };
        assert_eq!(
            unit("くるみ", 1, &[span(1, 0, 0, 9)]),
            Some((id(10), PrivateHit::Value))
        );
        // 値の鍵が違っても根拠の位置で当たる。
        assert_eq!(
            unit("胡桃", 1, &[span(1, 0, 0, 9)]),
            Some((id(10), PrivateHit::Position))
        );
        // 範囲が違えば位置では当たらない (取り込んでいない指示はちょうど同じ範囲だけ)。
        assert_eq!(unit("胡桃", 1, &[span(1, 0, 0, 6)]), None);
        // 指示の後に言われた同じ属性の値は後継。前に言われた値は当たらない。
        assert_eq!(
            unit("えび", 30, &[span(30, 0, 0, 6)]),
            Some((id(10), PrivateHit::Successor))
        );
        assert_eq!(unit("えび", 5, &[span(5, 0, 0, 6)]), None);
        // 「この値だけ」は後継に当たらない。
        let only = set(vec![(10, private("くるみ", PrivateScope::ValueOnly, None))]);
        let p = probe("self", dict("dietary"), "えび", 30);
        assert_eq!(
            only.private_hit(&UnitProbe {
                probe: &p,
                anchors: &[]
            }),
            None
        );
        // private_off で解く。
        let off = set(vec![
            (10, private("くるみ", PrivateScope::Successors, None)),
            (20, cancel(DirectiveOp::PrivateOff, id(10))),
        ]);
        assert!(off.privates().is_empty());
    }

    /// verifies: 04 §7.5 (取り込んだ指示の位置は範囲が重なれば当たる)、§7.3 (根拠の位置は後の伏せ字で写して比べる)
    #[test]
    fn positions_follow_later_redactions_and_imported_positions_overlap() {
        // ターン 1 の本文 "abc secret def くるみ" の "secret" (4..10) を後の忘れるで伏せ字にした。
        let text = "abc secret def くるみ";
        let (r, _) = evorag_store::forget::Redaction::plan(text, 0, std::iter::once(4..10), M)
            .expect("plan");
        let mut f = forget("self", dict("hobby"), "secret");
        f.redactions = vec![DirectiveRedaction {
            turn: id(1),
            rev_before: 0,
            rev_after: 1,
            start: 4,
            end: 10,
            hash: r.hash().to_string(),
        }];
        let kurumi = u32::try_from(text.find("くるみ").expect("in text")).expect("fits");
        let at = span(1, 0, kurumi, kurumi + 9);
        let s = set(vec![
            (5, private("胡桃", PrivateScope::ValueOnly, Some(at))),
            (10, f),
        ]);
        // 伏せ字の後の本文での位置 ("secret" の 6 バイトが印に替わった分だけずれる)。
        let shift = u32::try_from(M.len()).expect("fits") - 6;
        let after = span(1, 1, kurumi + shift, kurumi + shift + 9);
        let p = probe("self", dict("dietary"), "くるみ", 1);
        assert_eq!(
            s.private_hit(&UnitProbe {
                probe: &p,
                anchors: &[after]
            }),
            Some((id(5), PrivateHit::Position))
        );
        // 取り込んだ指示は、写した後の範囲が重なれば当たる。
        let mut imported = private(
            "胡桃",
            PrivateScope::ValueOnly,
            Some(span(1, 0, kurumi - 2, kurumi + 3)),
        );
        imported.imported = true;
        let s = set(vec![(5, imported)]);
        assert_eq!(
            s.private_hit(&UnitProbe {
                probe: &p,
                anchors: &[span(1, 0, kurumi, kurumi + 9)]
            }),
            Some((id(5), PrivateHit::Position))
        );
    }

    /// verifies: 04 §7.4 (end は entity を解き直し、同じ組の vks に当たる値だけを終える。辞書の写しの前の鍵とも比べる)
    #[test]
    fn end_hits_its_group_and_the_value_keys_only() {
        let free = mac("住まい");
        let mut e = forget("self", ProbeAspect::Free { mac: &free }, "港町");
        e.op = "end".to_owned();
        e.entity_raw = Some("私".to_owned());
        e.turn = Some(id(7));
        let s = set(vec![(10, e)]);
        let end = &s.ends()[0];
        assert_eq!(end.position(), id(7));
        let resolve = |raw: &str| {
            if raw == "私" {
                "self".to_owned()
            } else {
                normalize_v1(raw)
            }
        };
        let written = probe("self", ProbeAspect::Free { mac: &free }, "港町", 1);
        assert!(end.hits(&written, resolve));
        // 辞書の版を上げて写った後も、写す前の鍵で当たる。
        let mapped = probe(
            "self",
            ProbeAspect::Dict {
                name: "residence",
                was_free: Some(&free),
            },
            "港町",
            1,
        );
        assert!(end.hits(&mapped, resolve));
        // 兄弟の値と、別の組には当たらない。
        assert!(!end.hits(
            &probe("self", ProbeAspect::Free { mac: &free }, "山町", 1),
            resolve
        ));
        assert!(!end.hits(&probe("self", dict("residence"), "港町", 1), resolve));
        // 生の文字列を解き直さない規則では当たらない (解き直すことが効いている)。
        assert!(!end.hits(&written, |raw: &str| normalize_v1(raw)));
    }

    /// verifies: 04 §5.3 (採る・戻すは指示の ID の位置に、end は終えた位置のターンの候補の後に混ぜる)、§7.5
    #[test]
    fn the_timeline_orders_adoptions_and_ends() {
        let mut accept = forget("self", dict("residence"), "東京");
        accept.op = "accept".to_owned();
        let mut end_at_turn = forget("self", dict("residence"), "大阪");
        end_at_turn.op = "end".to_owned();
        end_at_turn.turn = Some(id(5));
        let mut end_self = end_at_turn.clone();
        end_self.turn = None;
        let s = set(vec![(30, end_self), (20, accept), (40, end_at_turn)]);
        let order: Vec<(Id, Phase)> = s
            .timeline()
            .iter()
            .map(|(sl, _)| (sl.at, sl.phase))
            .collect();
        assert_eq!(
            order,
            [
                (id(5), Phase::AfterCandidates),
                (id(20), Phase::Candidates),
                (id(30), Phase::AfterCandidates),
            ]
        );
        // 採る・戻すは根拠の位置を先に、無ければ値の鍵で同じ組に当てる。
        let a = &s.adoptions()[0];
        let p = probe("self", dict("residence"), "東京", 1);
        assert_eq!(
            a.hits(
                &UnitProbe {
                    probe: &p,
                    anchors: &[]
                },
                s.redactions()
            ),
            Some(AdoptHit::Value)
        );
        let other = probe("self", dict("hometown"), "東京", 1);
        assert_eq!(
            a.hits(
                &UnitProbe {
                    probe: &other,
                    anchors: &[]
                },
                s.redactions()
            ),
            None
        );
    }

    /// verifies: 04 §5.5 (private の vks に語句か部分文字列が当たる区切りの鍵を導出の時点で除く)
    #[test]
    fn private_screens_keys_by_substrings() {
        let s = set(vec![(
            10,
            private("くるみ", PrivateScope::Successors, None),
        )]);
        assert!(s.screens_key(&K, "くるみの代わり"));
        assert!(!s.screens_key(&K, "食べられないもの"));
        assert!(!set(vec![]).screens_key(&K, "くるみの代わり"));
    }

    /// verifies: 04 §7.5 (知らない op は当てずに数える。入力の順に依らない)
    #[test]
    fn the_set_is_order_independent_and_counts_unknown_ops() {
        let mut unknown = forget("self", dict("hobby"), "将棋");
        unknown.op = "pin".to_owned();
        let ds = vec![
            (10, forget("self", dict("hobby"), "将棋")),
            (20, cancel(DirectiveOp::Unblock, id(10))),
            (30, unknown),
        ];
        let mut rev = ds.clone();
        rev.reverse();
        let (a, b) = (set(ds), set(rev));
        assert_eq!(a.forgets(), b.forgets());
        assert_eq!(a.unknown(), 1);
        assert_eq!(b.unknown(), 1);
    }
}
