//! `import-v04` の表 (`meta` の `import.v04_ended`) で足す `end` (04 §7.4 の「`import-v04` の `end`」、03 §12.4) と、
//! どの単位にも当たらなかった取り込みの private (04 §7.5 の「取り込んだ指示の位置」)。
//!
//! v0.4 で `Superseded` (か `Retracted`) だった値が、v0.5 の導出で `Live` になった組に、その値の `vks` で `end` を
//! 足す (`origin` は `import`、位置は表の出典のターン。無ければ取り込んだ時点)。呼び手は返った `end` を指示として
//! 書き、導き直して、もう一度この関数を呼ぶ (返るものが無くなるまで。組ごとに表の値の数で止まる)。どの区切りの台帳が
//! そろったら照らし終えるか (表の `done`) は、呼び手が決める。

use std::collections::BTreeSet;

use evorag_types::Id;

use super::Derived;
use crate::apply::{DirectiveSet, End, Probe, ProbeAspect, ResolvedEntity, UnitProbe, ValueKeys};
use crate::directive::{DirectiveBody, DirectiveOp, DirectiveOrigin, TurnSpan};
use crate::import_v04::V04Ended;
use crate::unit::{AnchorKind, UnitBody, UnitLayer, UnitState};
use crate::value_key::{ForgetKey, NORM_V1, aspect_key};

/// 足す `end` 1 つ (04 §7.5 の `end` の本体の項目)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NewEnd {
    /// entity の生の文字列 (v0.4 の事実の主語。`end` の `entity` と `entity_raw` に書く)。
    pub entity: String,
    /// 辞書の属性の名前 (v0.4 の属性は v0.5 の辞書の初版に入る)。
    pub aspect: String,
    /// 終える値の値の鍵と照合の鍵 (表のまま)。
    pub vks: Vec<String>,
    /// 終える位置 (表の出典の最も新しいターン。無ければ取り込んだ時点)。
    pub turn: Id,
}

impl NewEnd {
    /// `end` の指示の本体 (`origin` は `import`)。
    #[must_use]
    pub fn body(&self) -> DirectiveBody {
        let mut b = DirectiveBody::new(DirectiveOp::End);
        b.entity = Some(self.entity.clone());
        b.entity_raw = Some(self.entity.clone());
        b.aspect = Some(self.aspect.clone());
        b.vks = self.vks.clone();
        b.norm = Some(NORM_V1);
        b.turn = Some(self.turn);
        b.origin = DirectiveOrigin::Import.name().to_owned();
        b
    }
}

/// 単位を指示に当てる相手 (辞書の属性の単位は `aspect` の名前で、自由記述の属性は属性の鍵で)。
fn probe_of<'b>(b: &'b UnitBody, mac: &'b str, key: &ForgetKey) -> Probe<'b> {
    let aspect = if b.free {
        ProbeAspect::Free { mac }
    } else {
        ProbeAspect::Dict {
            name: &b.aspect,
            was_free: None,
        }
    };
    let latest = b
        .anchors
        .iter()
        .map(|a| a.turn)
        .max()
        .unwrap_or(Id::from_bytes([0; 16]));
    Probe::new(
        ResolvedEntity::new(&b.entity, str::to_owned),
        aspect,
        ValueKeys::of(key, &b.value),
        latest,
    )
}

/// 表 `table` の値のうち、導出の結果 `derived` で `Live` になった値の `end` (04 §7.4)。
///
/// - 当てるのは、辞書の属性の `world` の `Live` の単位のうち、足す `end` の当て方 (`End::hits`。entity の同値類を
///   今の規則 `resolve` で解き、属性と、値の鍵か照合の鍵を比べる) で当たるもの。
/// - 位置は表の出典のターン。無ければ取り込んだ時点 `imported_at` (`import.source` の取り込んだ時刻の位置)。
/// - 位置より後のターンで言った根拠を持つ単位は、取り込みの後に利用者が言い直した値なので当てない (`end` の後の
///   候補として `Live` になってよい)。
/// - 同じ位置で当たる、取り込みで足した `end` (`origin` が `import`) が既にあれば足さない (繰り返しで止まるため)。
#[must_use]
pub fn import_ends(
    derived: &Derived,
    table: &V04Ended,
    directives: &DirectiveSet,
    key: &ForgetKey,
    imported_at: Id,
    resolve: impl Fn(&str) -> String,
) -> Vec<NewEnd> {
    let resolve: &dyn Fn(&str) -> String = &resolve;
    let mut out: BTreeSet<NewEnd> = BTreeSet::new();
    for v in &table.values {
        let proposed = NewEnd {
            entity: v.entity.clone(),
            aspect: v.aspect.clone(),
            vks: v.vks.clone(),
            turn: v.turn.unwrap_or(imported_at),
        };
        let Some(end) = End::from_body(imported_at, &proposed.body(), resolve) else {
            continue;
        };
        let hits = derived.units.iter().any(|u| {
            let b = &u.body;
            if b.layer != UnitLayer::World || b.free || b.state != Some(UnitState::Live) {
                return false;
            }
            let probe = probe_of(b, "", key);
            let said_later = b
                .anchors
                .iter()
                .any(|a| a.kind == AnchorKind::Said && a.turn > proposed.turn);
            let added = directives.ends().iter().any(|e| {
                e.origin() == DirectiveOrigin::Import
                    && e.position() == proposed.turn
                    && e.hits(&probe)
            });
            end.hits(&probe) && !said_later && !added
        });
        if hits {
            out.insert(proposed);
        }
    }
    out.into_iter().collect()
}

/// どの単位にも当たらなかった取り込みの private の指示 (ID の順。04 §7.5 の「取り込んだ指示の位置」: 取り込みの
/// 最初の導出の後に `doctor` と `/memory` で示す)。
#[must_use]
pub fn unmatched_imported_privates(
    derived: &Derived,
    directives: &DirectiveSet,
    key: &ForgetKey,
) -> Vec<Id> {
    let red = directives.redactions();
    let units: Vec<(String, Vec<TurnSpan>)> = derived
        .units
        .iter()
        .map(|u| {
            let mac = if u.body.free {
                aspect_key(key, &u.body.aspect).to_hex()
            } else {
                String::new()
            };
            let spans = u
                .body
                .anchors
                .iter()
                .map(|a| TurnSpan {
                    turn: a.turn,
                    rev: a.rev,
                    start: a.start,
                    end: a.end,
                })
                .collect();
            (mac, spans)
        })
        .collect();
    directives
        .privates()
        .iter()
        .filter(|p| p.imported())
        .filter(|p| {
            !derived.units.iter().zip(&units).any(|(u, (mac, spans))| {
                let probe = probe_of(&u.body, mac, key);
                p.hits(
                    &UnitProbe {
                        probe: &probe,
                        anchors: spans,
                    },
                    red,
                )
                .is_some()
            })
        })
        .map(|p| p.id())
        .collect()
}
