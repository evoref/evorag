//! `import-v04` の表 (`meta` の `import.v04_ended`) で足す `end` (04 §7.4 の「`import-v04` の `end`」、03 §12.4)。
//!
//! v0.4 で `Superseded` (か `Retracted`) だった値が、v0.5 の導出で `Live` になった組に、その値の `vks` で `end` を
//! 足す (`origin` は `import`、位置は表の出典のターン)。呼び手は返った `end` を指示として書き、導き直して、もう一度
//! この関数を呼ぶ (返るものが無くなるまで。組ごとに表の値の数で止まる)。どの区切りの台帳がそろったら照らし終えるか
//! (表の `done`) は、呼び手が決める。

use std::collections::BTreeSet;

use evorag_types::Id;

use super::Derived;
use crate::apply::{DirectiveSet, Probe, ProbeAspect, ResolvedEntity, ValueKeys};
use crate::directive::DirectiveOrigin;
use crate::import_v04::V04Ended;
use crate::unit::{AnchorKind, UnitLayer, UnitState};
use crate::value_key::{ForgetKey, ValueKey, normalize_v1};

/// 足す `end` 1 つ (04 §7.5 の `end` の本体の項目)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NewEnd {
    /// 正規化した entity (今の規則で解いたもの)。
    pub entity: String,
    /// entity の生の文字列 (v0.4 の事実の主語)。
    pub entity_raw: String,
    /// 辞書の属性の名前 (v0.4 の属性は v0.5 の辞書の初版に入る)。
    pub aspect: String,
    /// 終える値の値の鍵と照合の鍵 (表のまま)。
    pub vks: Vec<String>,
    /// 終える位置 (表の出典の最も新しいターン。無ければ `None` で、指示の ID を位置にする)。
    pub turn: Option<Id>,
}

/// 表 `table` の値のうち、導出の結果 `derived` で `Live` になった値の `end` (04 §7.4)。
///
/// - 当てるのは、辞書の属性の `world` の `Live` の単位で、entity (表の主語を今の規則 `resolve` で解いて正規化
///   したもの) と属性が同じで、値の鍵か照合の鍵 (`key` で作る) が表の `vks` にあるもの。
/// - 表の位置より後のターンで言った根拠を持つ単位は、取り込みの後に利用者が言い直した値なので当てない (`end` の
///   後の候補として `Live` になってよい)。
/// - 同じ組と値に、取り込みで足した `end` (`origin` が `import`) が既にあれば足さない (繰り返しで止まるため)。
#[must_use]
pub fn import_ends(
    derived: &Derived,
    table: &V04Ended,
    directives: &DirectiveSet,
    key: &ForgetKey,
    resolve: impl Fn(&str) -> String,
) -> Vec<NewEnd> {
    let mut out: BTreeSet<NewEnd> = BTreeSet::new();
    for v in &table.values {
        let vks: BTreeSet<ValueKey> = v.vks.iter().filter_map(|h| ValueKey::from_hex(h)).collect();
        let entity = normalize_v1(&resolve(&v.entity));
        for u in &derived.units {
            let b = &u.body;
            if b.layer != UnitLayer::World
                || b.free
                || b.state != Some(UnitState::Live)
                || b.entity != entity
                || b.aspect != v.aspect
            {
                continue;
            }
            let keys = ValueKeys::of(key, &b.value);
            if !(vks.contains(&keys.vk) || vks.contains(&keys.loose)) {
                continue;
            }
            let said_later = v.turn.is_some_and(|t| {
                b.anchors
                    .iter()
                    .any(|a| a.kind == AnchorKind::Said && a.turn > t)
            });
            if said_later {
                continue;
            }
            let latest = b.anchors.iter().map(|a| a.turn).max().unwrap_or(u.id);
            let probe = Probe::new(
                ResolvedEntity::new(&b.entity, str::to_owned),
                ProbeAspect::Dict {
                    name: &b.aspect,
                    was_free: None,
                },
                keys,
                latest,
            );
            let added = directives.ends().iter().any(|e| {
                e.origin() == DirectiveOrigin::Import
                    && v.turn.is_none_or(|t| e.position() == t)
                    && e.hits(&probe)
            });
            if added {
                continue;
            }
            out.insert(NewEnd {
                entity: entity.clone(),
                entity_raw: v.entity.clone(),
                aspect: v.aspect.clone(),
                vks: v.vks.clone(),
                turn: v.turn,
            });
        }
    }
    out.into_iter().collect()
}
