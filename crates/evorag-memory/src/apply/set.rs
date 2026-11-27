//! 指示の集合と、指示ごとの当て方 (04 §7.4・§7.5・§7.6、§5.2 の関門 8、§5.3 の入力の順、§6 の `forgotten`)。

use std::collections::{BTreeMap, BTreeSet};

use evorag_types::Id;

use super::keys::SubstringKeys;
use super::marks::TurnRedactions;
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveOrigin, PrivateScope, TurnSpan};
use crate::value_key::{
    ForgetKey, NORM_V1, ValueKey, loose_key, normalize_loose, normalize_v1, value_key,
};

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

/// 今の規則で解いた entity (04 §5.1 の主語・§7.6 の同値類)。
///
/// 主語を明示する語 (`subject_self`) を `self` に直し、正規化 v1 を当てたもの。指示を当てる相手の entity は、この型で
/// だけ渡せる (解き忘れた呼び名で、`self` の忘れるを抜けないため)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntity {
    v1: String,
    loose: String,
}

impl ResolvedEntity {
    /// 呼び名 `raw` を今の規則 `resolve` (主語を明示する語なら `self` を返し、ほかは呼び名を返す関数) で解く。
    #[must_use]
    pub fn new(raw: &str, resolve: impl Fn(&str) -> String) -> Self {
        let r = resolve(raw);
        Self {
            v1: normalize_v1(&r),
            loose: normalize_loose(&r),
        }
    }

    /// 解いた entity (正規化 v1 の後)。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.v1
    }
}

/// 指示を当てる相手の属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAspect<'a> {
    /// 辞書の属性。
    Dict {
        /// 辞書の属性の名前。
        name: &'a str,
        /// 属性の決め方 (04 §5.2) で辞書の属性に写す前の、自由記述の属性の鍵 (`aspect_mac` と同じ 16 進。写して
        /// いなければ `None`)。`end` と採る・戻すは、写す前の鍵とも比べる (04 §7.5 の「当て方の補い」)。
        was_free: Option<&'a str>,
    },
    /// 自由記述の属性。
    Free {
        /// 属性の鍵の 16 進 (04 §5.6)。
        mac: &'a str,
    },
}

/// 指示を当てる相手 (候補・戻す値・単位)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe<'a> {
    entity: ResolvedEntity,
    aspect: ProbeAspect<'a>,
    keys: ValueKeys,
    turn: Id,
}

impl<'a> Probe<'a> {
    /// `entity` は記録する主語を今の規則で解いたもの、`keys` は記録する値の鍵、`turn` は根拠のターン (印を解いた
    /// 指示の比べ、04 §7.5 の `unblock`。単位なら根拠の最も新しいターンで、後継の比べにも使う)。
    #[must_use]
    pub fn new(entity: ResolvedEntity, aspect: ProbeAspect<'a>, keys: ValueKeys, turn: Id) -> Self {
        Self {
            entity,
            aspect,
            keys,
            turn,
        }
    }

    /// 解いた entity。
    #[must_use]
    pub fn entity(&self) -> &ResolvedEntity {
        &self.entity
    }

    /// 属性。
    #[must_use]
    pub fn aspect(&self) -> ProbeAspect<'a> {
        self.aspect
    }

    /// 値の鍵と照合の鍵。
    #[must_use]
    pub fn keys(&self) -> ValueKeys {
        self.keys
    }

    /// 根拠のターン。
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

/// entity の同値類 (04 §7.6): 正規化 v1 か照合の形で同じ呼び名と、主語を明示する語を `self` とみなしたもの、指示が
/// 持つその時点の別名 (`aliases`)。指示の entity と別名は、書いたときの形と今の規則で解き直した形の両方を入れる
/// (規則の版を上げても、当たる相手の集合が縮まないため)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EntityClass {
    v1: BTreeSet<String>,
    loose: BTreeSet<String>,
    /// entity を持たない (壊れた) 忘れる・private の指示。止めすぎる側に倒して、どの entity にも当てる。
    any: bool,
}

impl EntityClass {
    fn of(entity: &str, aliases: &[String], resolve: &dyn Fn(&str) -> String) -> Self {
        let mut out = Self::default();
        for n in std::iter::once(entity).chain(aliases.iter().map(String::as_str)) {
            for form in [n.to_owned(), resolve(n)] {
                out.v1.insert(normalize_v1(&form));
                out.loose.insert(normalize_loose(&form));
            }
        }
        out
    }

    fn any() -> Self {
        Self {
            any: true,
            ..Self::default()
        }
    }

    fn contains(&self, p: &Probe<'_>) -> bool {
        self.any || self.v1.contains(&p.entity.v1) || self.loose.contains(&p.entity.loose)
    }
}

/// 指示の属性 (04 §7.5 の `aspect`・`aspect_mac`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveAspect {
    /// 辞書の属性の名前。
    Dict(String),
    /// 自由記述の属性の鍵 (16 進)。
    Free(String),
    /// どちらも無い (壊れた) 指示。
    Missing,
}

impl DirectiveAspect {
    fn of(b: &DirectiveBody) -> Self {
        match (&b.aspect, &b.aspect_mac) {
            (Some(a), _) => Self::Dict(a.clone()),
            (None, Some(m)) => Self::Free(m.clone()),
            (None, None) => Self::Missing,
        }
    }

    /// 忘れるの当て方の述語の属性の比べ (04 §7.5): 辞書の属性の指示と辞書の属性の相手のときだけ名前を比べる。
    /// 自由記述の属性の指示はどの相手にも、どの指示も自由記述の属性の相手に、属性を見ずに当てる。属性の無い指示も
    /// 属性を見ない (止めすぎる側)。
    fn admits(&self, p: &Probe<'_>) -> bool {
        match (self, p.aspect) {
            (Self::Dict(a), ProbeAspect::Dict { name, .. }) => a == name,
            _ => true,
        }
    }

    /// 同じ (entity, 属性) の組か (`end`・採る・戻す)。辞書の属性は名前、自由記述の属性は鍵で比べ、辞書の写しの
    /// 前の自由記述の属性の鍵とも比べる (04 §7.5 の「当て方の補い」)。属性の無い指示は当てない (作るときに除く)。
    fn same_group(&self, p: &Probe<'_>) -> bool {
        match (self, p.aspect) {
            (Self::Dict(a), ProbeAspect::Dict { name, .. }) => a == name,
            (Self::Free(m), ProbeAspect::Free { mac }) => m == mac,
            (Self::Free(m), ProbeAspect::Dict { was_free, .. }) => was_free == Some(m.as_str()),
            (Self::Dict(_), ProbeAspect::Free { .. }) | (Self::Missing, _) => false,
        }
    }
}

/// 記録の `vks` (16 進の並び) を読む。読めない要素があれば `false` も返す (数える、`doctor` 用)。
fn parse_vks(vks: &[String]) -> (BTreeSet<ValueKey>, bool) {
    let keys: BTreeSet<ValueKey> = vks.iter().filter_map(|h| ValueKey::from_hex(h)).collect();
    let all = vks.iter().all(|h| ValueKey::from_hex(h).is_some());
    (keys, all)
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
    aspect: DirectiveAspect,
    unblocked: Option<Id>,
    targets: BTreeSet<Id>,
}

impl Forget {
    fn new(
        id: Id,
        b: &DirectiveBody,
        vks: BTreeSet<ValueKey>,
        resolve: &dyn Fn(&str) -> String,
    ) -> Self {
        let targets = b
            .turns
            .iter()
            .copied()
            .chain(b.redactions.iter().map(|r| r.turn))
            .chain(b.hidden.iter().map(|h| h.turn))
            .collect();
        Self {
            id,
            vks,
            entity: b.entity.as_deref().map_or_else(EntityClass::any, |e| {
                EntityClass::of(e, &b.aliases, resolve)
            }),
            aspect: DirectiveAspect::of(b),
            unblocked: None,
            targets,
        }
    }

    /// 指示の ID。
    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// 印を解いた `unblock` の指示の ID (無ければ `None`)。同じ忘れるを解く `unblock` が 2 つ以上あれば、最も早い
    /// もの (04 §7.5 の「当て方の補い」)。
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

    /// 読めた `vks`。
    #[must_use]
    pub fn vks(&self) -> &BTreeSet<ValueKey> {
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
    entity_raw: Option<String>,
    entity: EntityClass,
    aspect: DirectiveAspect,
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
    fn new(
        id: Id,
        b: &DirectiveBody,
        vks: BTreeSet<ValueKey>,
        resolve: &dyn Fn(&str) -> String,
    ) -> Self {
        Self {
            id,
            vks,
            entity_raw: b.entity.clone(),
            entity: b.entity.as_deref().map_or_else(EntityClass::any, |e| {
                EntityClass::of(e, &b.aliases, resolve)
            }),
            aspect: DirectiveAspect::of(b),
            at: b.at,
            imported: b.imported,
            scope: b.scope().unwrap_or(PrivateScope::Successors),
            origin: b.origin(),
        }
    }

    /// 指示の ID。
    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// 効く範囲。
    #[must_use]
    pub fn scope(&self) -> PrivateScope {
        self.scope
    }

    /// 指示を足した経路。
    #[must_use]
    pub fn origin(&self) -> DirectiveOrigin {
        self.origin
    }

    /// `import-v04` で写した指示か (どの単位にも当たらない取り込みの private を示すため、04 §7.5)。
    #[must_use]
    pub fn imported(&self) -> bool {
        self.imported
    }

    /// 指示に書いた entity (書いたときの形)。
    #[must_use]
    pub fn entity(&self) -> Option<&str> {
        self.entity_raw.as_deref()
    }

    /// 指示の属性。
    #[must_use]
    pub fn aspect(&self) -> &DirectiveAspect {
        &self.aspect
    }

    /// 根拠の位置 (書いたときの改訂の番号の位置)。
    #[must_use]
    pub fn at(&self) -> Option<&TurnSpan> {
        self.at.as_ref()
    }

    /// 読めた `vks` (値の鍵と照合の鍵)。
    #[must_use]
    pub fn vks(&self) -> &BTreeSet<ValueKey> {
        &self.vks
    }

    /// 単位に当たるか (04 §7.5): 値の鍵か照合の鍵 (`vks`。entity の同値類と属性は忘れるの当て方の述語と同じ形で
    /// 比べる)、根拠の位置、後継のどれか。後継は「後継まで」の指示だけで、同じ (entity, 辞書の属性) の単位のうち
    /// 根拠の最も新しいターンが指示の ID より後のもの。属性の無い (壊れた) 指示は、どの辞書の属性の後継にも当てる
    /// (止めすぎる側)。
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
        let same_attribute = match (&self.aspect, p.aspect) {
            (DirectiveAspect::Dict(a), ProbeAspect::Dict { name, .. }) => a == name,
            (DirectiveAspect::Missing, ProbeAspect::Dict { .. }) => true,
            _ => false,
        };
        let successor = self.scope == PrivateScope::Successors
            && same_attribute
            && self.entity.contains(p)
            && p.turn > self.id;
        successor.then_some(PrivateHit::Successor)
    }
}

/// 採る・戻す (04 §7.5 の `accept`・`revert`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdoptOp {
    /// `Contested` の値を今の値にする (`/memory accept`)。
    Accept,
    /// 履歴の値を今の値に戻す (`/memory revert`)。
    Revert,
}

/// 採る・戻すの指示 1 つ。導出の入力の順の中で、指示の ID の時点で 1 回だけ当てる出来事 (04 §7.5)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    id: Id,
    op: AdoptOp,
    vks: BTreeSet<ValueKey>,
    entity: EntityClass,
    aspect: DirectiveAspect,
    at: Option<TurnSpan>,
    imported: bool,
}

/// 採る・戻すが単位に当たった理由。根拠の位置で当たった単位を、値の鍵で当たった単位より先に採る (04 §7.5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdoptHit {
    /// 根拠の位置 (`at`) で当たった。
    Position,
    /// 値の鍵で、同じ (entity, 属性) の組に当たった。
    Value,
}

impl Adoption {
    /// 指示の ID。
    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// 採るか戻すか。
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
    entity: EntityClass,
    aspect: DirectiveAspect,
    turn: Option<Id>,
    cause: Option<Id>,
    origin: DirectiveOrigin,
}

impl End {
    /// 指示の ID。
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

    /// 指示を足した経路。
    #[must_use]
    pub fn origin(&self) -> DirectiveOrigin {
        self.origin
    }

    /// 単位 (その時点の `Live`) に当たるか (04 §7.4 の「`end` の当て方」): 値の鍵か照合の鍵が `vks` にあり、
    /// 同じ (entity, 属性) の組のとき。entity は、指示の生の文字列を今の規則で解き直したものと比べる
    /// ([`DirectiveSet::new`] の `resolve`)。状態が `Live` かは畳みの側で確かめる。
    #[must_use]
    pub fn hits(&self, p: &Probe<'_>) -> bool {
        p.keys.any_in(&self.vks) && self.entity.contains(p) && self.aspect.same_group(p)
    }
}

/// 畳みの列の中の位置 (04 §5.3 の「入力の順」)。指示もターンも UUID v7 なので、1 本の列に並ぶ。
///
/// 候補は、根拠のターンの [`Slot::candidates`] の位置にあり、その中は (値の始まり …) の順に並ぶ。`end` は終えた
/// 位置のターンの候補の後 ([`Phase::AfterCandidates`])、採る・戻すは指示の ID の位置に置く。同じ位置の指示は
/// 指示の ID の順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot {
    /// 位置のターンの ID (採る・戻すは指示の ID)。
    pub at: Id,
    /// 同じ位置の中の前後。
    pub phase: Phase,
    /// 置いた指示 (候補の位置は `None`。同じ位置の指示より前に並ぶ)。
    pub directive: Option<Id>,
}

impl Slot {
    /// ターン `turn` の候補の位置。
    #[must_use]
    pub fn candidates(turn: Id) -> Self {
        Self {
            at: turn,
            phase: Phase::Candidates,
            directive: None,
        }
    }
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
    /// 採る・戻す。
    Adopt(&'s Adoption),
    /// `end`。
    End(&'s End),
}

/// 当てずに数えた指示の数 (`doctor` が報告する)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirectiveCounts {
    /// 知らない `op` の指示 (当てずに残す、04 §7.5)。
    pub unknown: usize,
    /// 相手を見つけられない解く指示 (`private_off`・`unblock`。相手が無い・種類が違う・相手より ID が小さい)。
    pub unmatched_cancels: usize,
    /// entity か属性を欠いて当てない `end`・採る・戻す (04 §7.5 の「当て方の補い」)。
    pub incomplete: usize,
    /// `vks` に読めない要素 (64 桁の小文字の 16 進でないもの) を持つ指示 (読めた要素だけで当てる)。
    pub unreadable_keys: usize,
    /// 値の鍵の正規化の版 (`norm`) が 1 でない指示 (v0.5 の版の鍵とは当たらない)。
    pub other_norm: usize,
}

/// 効いている指示の集合 (04 §7.5)。指示の ID の順に並べ、解く指示 (`private_off`・`unblock`) を解く相手に当てた
/// 後のもの。
///
/// - `private_off` は private の指示だけを、`unblock` は忘れるの指示だけを解く (種類の違う相手・無い相手・相手より
///   ID の小さい解く指示は当てない、04 §7.5 の「解く指示の相手」)。どちらも時点に依らず常に効く。
/// - 知らない `op` の指示と、項目を欠いて当てない指示は数える ([`DirectiveSet::counts`])。
#[derive(Debug, Clone)]
pub struct DirectiveSet {
    forgets: Vec<Forget>,
    privates: Vec<Private>,
    adoptions: Vec<Adoption>,
    ends: Vec<End>,
    redactions: TurnRedactions,
    counts: DirectiveCounts,
}

impl DirectiveSet {
    /// 指示の (ID, 本体) から作る。
    ///
    /// `redaction_marker` は伏せ字の印 (位置の写し直しに使う。データ根のプロンプトの言語の印、04 §7.3)。`resolve` は
    /// 今の規則で呼び名を解く関数 (主語を明示する語なら `self`、04 §5.1)。指示の entity と別名は、書いたときの形と
    /// `resolve` で解き直した形の両方で比べる (§7.6)。
    #[must_use]
    pub fn new(
        directives: impl IntoIterator<Item = (Id, DirectiveBody)>,
        redaction_marker: &str,
        resolve: impl Fn(&str) -> String,
    ) -> Self {
        let resolve: &dyn Fn(&str) -> String = &resolve;
        let all: BTreeMap<Id, DirectiveBody> = directives.into_iter().collect();
        let mut counts = DirectiveCounts::default();
        let mut unblocks: BTreeMap<Id, Id> = BTreeMap::new();
        let mut cancelled: BTreeSet<Id> = BTreeSet::new();
        for (&id, b) in &all {
            let wanted = match b.op() {
                DirectiveOp::Unblock => DirectiveOp::Forget,
                DirectiveOp::PrivateOff => DirectiveOp::Private,
                DirectiveOp::Unknown => {
                    counts.unknown += 1;
                    continue;
                }
                _ => continue,
            };
            // 解く指示は、相手より後に足したものだけ (時計が戻ったとき、忘れる自身の根拠まで解けないため)。
            let Some(target) = b
                .target
                .filter(|t| *t < id && all.get(t).is_some_and(|t| t.op() == wanted))
            else {
                counts.unmatched_cancels += 1;
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
            counts,
        };
        for (&id, b) in &all {
            let op = b.op();
            if matches!(
                op,
                DirectiveOp::Unknown | DirectiveOp::Unblock | DirectiveOp::PrivateOff
            ) {
                continue;
            }
            let (vks, readable) = parse_vks(&b.vks);
            if !readable {
                out.counts.unreadable_keys += 1;
            }
            if b.norm.is_some_and(|n| n != NORM_V1) {
                out.counts.other_norm += 1;
            }
            match op {
                DirectiveOp::Forget => {
                    let mut f = Forget::new(id, b, vks, resolve);
                    f.unblocked = unblocks.get(&id).copied();
                    out.forgets.push(f);
                }
                DirectiveOp::Private => {
                    if !cancelled.contains(&id) {
                        out.privates.push(Private::new(id, b, vks, resolve));
                    }
                }
                DirectiveOp::Accept | DirectiveOp::Revert | DirectiveOp::End => {
                    out.push_event(id, op, b, vks, resolve);
                }
                _ => {}
            }
        }
        out
    }

    /// `end`・採る・戻すを足す。entity か属性を欠くものは当てずに数える (採りすぎ・終えすぎにしないため)。
    fn push_event(
        &mut self,
        id: Id,
        op: DirectiveOp,
        b: &DirectiveBody,
        vks: BTreeSet<ValueKey>,
        resolve: &dyn Fn(&str) -> String,
    ) {
        let aspect = DirectiveAspect::of(b);
        // `end` は生の文字列を今の規則で解き直す (04 §7.4)。書いたときの正規化した entity も同値類に入れる。
        let raw = if op == DirectiveOp::End {
            b.entity_raw.as_deref().or(b.entity.as_deref())
        } else {
            b.entity.as_deref()
        };
        let (Some(raw), false) = (raw, aspect == DirectiveAspect::Missing) else {
            self.counts.incomplete += 1;
            return;
        };
        let mut entity = EntityClass::of(raw, &b.aliases, resolve);
        if let Some(e) = b.entity.as_deref() {
            let more = EntityClass::of(e, &[], resolve);
            entity.v1.extend(more.v1);
            entity.loose.extend(more.loose);
        }
        match op {
            DirectiveOp::End => self.ends.push(End {
                id,
                vks,
                entity,
                aspect,
                turn: b.turn,
                cause: b.cause,
                origin: b.origin(),
            }),
            _ => self.adoptions.push(Adoption {
                id,
                op: if op == DirectiveOp::Accept {
                    AdoptOp::Accept
                } else {
                    AdoptOp::Revert
                },
                vks,
                entity,
                aspect,
                at: b.at,
                imported: b.imported,
            }),
        }
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

    /// 当てずに数えた指示の数。
    #[must_use]
    pub fn counts(&self) -> DirectiveCounts {
        self.counts
    }

    /// 関門 8 (`forgotten`、04 §5.2) と当て方の述語 (§7.5): 相手に当たる忘れるの指示のうち、ID の最も小さいもの。
    /// 当たれば相手を黙って捨てて数える (§7.6)。戻す値 (採る・戻すで今の値にする単位) にも同じ関数を使う。訂正の
    /// 新しい値と古い値は [`DirectiveSet::forgotten_correction`] で当てる。
    #[must_use]
    pub fn forgotten_by(&self, p: &Probe<'_>) -> Option<Id> {
        self.forgets.iter().find(|f| f.hits(p)).map(Forget::id)
    }

    /// 訂正の関門の `forgotten` (04 §6 の「判定の入力」): 検証済みになる結果の `old`・`new` の値の鍵と照合の鍵の
    /// どれかが、どれかの忘れるの指示の `vks` にあれば、その指示 (ID の最も小さいもの)。entity と属性は問わない
    /// (忘れた値を訂正で戻さないため。関門 8 より広い側)。印を解いた指示は、候補の発話のターン `turn` が `unblock`
    /// の指示の ID より前のときだけ当てる。
    #[must_use]
    pub fn forgotten_correction(&self, values: &[ValueKeys], turn: Id) -> Option<Id> {
        self.forgets
            .iter()
            .find(|f| f.applies_at(turn) && values.iter().any(|v| v.any_in(&f.vks)))
            .map(Forget::id)
    }

    /// 単位に当たる private の指示のうち、ID の最も小さいものと理由 (04 §7.5)。当たれば単位に private の印を付ける。
    #[must_use]
    pub fn private_hit(&self, u: &UnitProbe<'_, '_>) -> Option<(Id, PrivateHit)> {
        self.privates
            .iter()
            .find_map(|p| p.hits(u, &self.redactions).map(|h| (p.id, h)))
    }

    /// 区切りの鍵を、private の値で導出の時点で除くか (04 §5.5): 鍵の語句か部分文字列の値の鍵か照合の鍵
    /// (`key_substrings`、[`SubstringKeys`]) が、効いている private の指示の `vks` か、導出で private の印を付けた
    /// 単位の値の鍵と照合の鍵 (`private_units`。後継・根拠の位置で private になった値を含む) に当たるとき。
    #[must_use]
    pub fn screens_key(
        &self,
        key_substrings: &SubstringKeys,
        private_units: &BTreeSet<ValueKey>,
    ) -> bool {
        key_substrings.hits(private_units)
            || self.privates.iter().any(|p| key_substrings.hits(&p.vks))
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
                        directive: Some(a.id),
                    },
                    Timed::Adopt(a),
                )
            })
            .chain(self.ends.iter().map(|e| {
                (
                    Slot {
                        at: e.position(),
                        phase: Phase::AfterCandidates,
                        directive: Some(e.id),
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
mod tests;
