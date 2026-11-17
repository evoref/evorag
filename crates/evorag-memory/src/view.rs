//! 事実の一覧と置き換えの履歴の読み取り用の形 (04 §7.1・§7.4・§7.5、07 §3 の `/memory`)。I/O を持たない。
//!
//! - 行の参照は、事実への指示と同じ鍵 (主語, 属性, 値の鍵) と、表示したときの状態の種類と時制で持つ
//!   ([`find_row`])。事実の ID は参考にだけ持つ (作り直すと変わるため)。
//! - 番号を振る行は、今の値・予定の値・`Contested` の値に加え、今の値が消えた属性 (`Vacant`) と、値が 1 つも
//!   無く後継まで効く private の指示だけが残る属性 (`RuleOnly`)。
//! - 並びは事実の表と同じ: 属性の一覧の優先の順 → 主語 (`self` が先、残りは呼び名の順)。属性の中は、今の値
//!   → 予定の値 → `Contested` の値 (それぞれ記録した順)。

use std::collections::{BTreeMap, BTreeSet};

use evorag_types::Id;
use evorag_types::api::{
    AttrInfo, FactGroup, FactGroupKey, FactHistory, FactHistoryRow, FactKey, FactList, FactRow,
    FactStatus, FactSubject, FactTense, PrivateCause, PrivateHistoryValue, PrivateInfo,
    PrivateScope,
};

use crate::fold::{Cover, DirectiveKind, Directives, Fact, FactSet, FactState, Scope, Tense};
use crate::rules::{Attributes, Cardinality};

/// 主語 `self` (04 §5.1)。
const SELF: &str = "self";

/// 記録の主語を読み取り用の形にする。
#[must_use]
pub fn subject_of(s: &str) -> FactSubject {
    if s == SELF {
        FactSubject::User
    } else {
        FactSubject::Named(s.to_owned())
    }
}

/// 読み取り用の主語を記録の形にする。
#[must_use]
pub fn subject_name(s: &FactSubject) -> &str {
    match s {
        FactSubject::User => SELF,
        FactSubject::Named(n) => n,
    }
}

fn tense_of(t: Tense) -> FactTense {
    match t {
        Tense::Current => FactTense::Current,
        Tense::Past => FactTense::Past,
        Tense::Planned => FactTense::Planned,
    }
}

fn status_of(s: FactState) -> Option<FactStatus> {
    match s {
        FactState::Live => Some(FactStatus::Live),
        FactState::Contested { .. } => Some(FactStatus::Contested),
        FactState::Superseded { .. } => Some(FactStatus::Superseded),
        FactState::Retracted => None,
    }
}

/// 事実の行の参照。値の鍵の無い・`Retracted` の事実は `None`。
#[must_use]
pub fn row_key(f: &Fact) -> Option<FactKey> {
    Some(FactKey {
        subject: subject_of(&f.subject),
        attribute: f.attribute.clone(),
        vk: f.vk?.to_hex(),
        status: status_of(f.state)?,
        tense: tense_of(f.tense),
        fact: Some(f.id),
    })
}

/// 行の参照が指す事実 (同じ鍵の記録が、表示したときの状態の種類と時制で今もあるもの)。無ければ `None`
/// (「一覧が更新されました」、04 §7.1)。
#[must_use]
pub fn find_row<'a>(set: &'a FactSet, key: &FactKey) -> Option<&'a Fact> {
    set.facts
        .iter()
        .find(|f| row_key(f).is_some_and(|k| k == *key))
}

fn attr_info(attributes: &Attributes, name: &str) -> AttrInfo {
    AttrInfo {
        name: name.to_owned(),
        multi: attributes
            .get(name)
            .is_some_and(|a| a.cardinality == Cardinality::Multi),
    }
}

/// 値の private の印と理由 (04 §7.5 の「private の理由の表示」)。`kept` は、セッションの削除で足した指示の ID
/// ([`crate::kept_after_delete`])。private でなければ `None`。
#[must_use]
pub fn private_info(f: &Fact, dirs: &Directives, kept: &BTreeSet<Id>) -> Option<PrivateInfo> {
    let cover = dirs.private_cover(f);
    if cover.is_empty() {
        return None;
    }
    let mut causes = Vec::new();
    let mut directives = Vec::new();
    for (d, how) in cover {
        directives.push(d.id);
        let cause = match (how, &d.kind) {
            (Cover::Successor, _) => PrivateCause::Successor,
            (Cover::SameValue, DirectiveKind::Private { .. }) if kept.contains(&d.id) => {
                PrivateCause::KeptAfterDelete
            }
            (Cover::SameValue, DirectiveKind::Private { scope }) => match scope {
                Scope::Successors => PrivateCause::Chosen {
                    scope: PrivateScope::Successors,
                },
                Scope::ValueOnly => PrivateCause::Chosen {
                    scope: PrivateScope::ValueOnly,
                },
            },
            (Cover::SameValue, _) => continue,
        };
        if !causes.contains(&cause) {
            causes.push(cause);
        }
    }
    Some(PrivateInfo { causes, directives })
}

fn is_open(f: &Fact) -> bool {
    matches!(f.state, FactState::Live | FactState::Contested { .. })
}

fn is_history(f: &Fact) -> bool {
    matches!(f.state, FactState::Superseded { .. })
}

/// 値の行の並び (今の値 → 予定の値 → `Contested`)。
fn open_rank(f: &Fact) -> u8 {
    match (f.state, f.tense) {
        (FactState::Live, Tense::Current) => 0,
        (FactState::Live, _) => 1,
        _ => 2,
    }
}

/// (主語, 属性) の後継まで効く private の指示だけが残る属性か。そうなら、その指示 (ID の順)。
/// 開いた値 (`Live`・`Contested`) が 1 つでもあれば `None`。
#[must_use]
pub fn rule_only(set: &FactSet, dirs: &Directives, group: &FactGroupKey) -> Option<Vec<Id>> {
    let subject = subject_name(&group.subject);
    let open = set
        .facts
        .iter()
        .any(|f| f.subject == subject && f.attribute == group.attribute && is_open(f));
    if open {
        return None;
    }
    let ids: Vec<Id> = dirs
        .successor_rules(subject, &group.attribute)
        .into_iter()
        .map(|d| d.id)
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// 並べる順の鍵: 属性の優先の順 → 主語 (`self` が先、残りは呼び名の順) → 属性の名前。
fn group_rank(
    attributes: &Attributes,
    subject: &str,
    attribute: &str,
) -> (u32, bool, String, String) {
    (
        attributes.get(attribute).map_or(u32::MAX, |a| a.priority),
        subject != SELF,
        subject.to_owned(),
        attribute.to_owned(),
    )
}

/// 事実の一覧を作る (04 §7.1)。出典の残っていない事実は、呼び出す側が `set` から除いておく。`note` は空。
#[must_use]
pub fn fact_list(
    set: &FactSet,
    dirs: &Directives,
    attributes: &Attributes,
    kept: &BTreeSet<Id>,
) -> FactList {
    let mut groups: BTreeMap<(u32, bool, String, String), (String, String)> = BTreeMap::new();
    for f in set.facts.iter().filter(|f| f.state != FactState::Retracted) {
        groups.insert(
            group_rank(attributes, &f.subject, &f.attribute),
            (f.subject.clone(), f.attribute.clone()),
        );
    }
    for d in dirs.iter() {
        if matches!(d.kind, DirectiveKind::Private { .. }) {
            groups.insert(
                group_rank(attributes, &d.subject, &d.attribute),
                (d.subject.clone(), d.attribute.clone()),
            );
        }
    }
    let mut out = Vec::new();
    for (subject, attribute) in groups.into_values() {
        let group = FactGroupKey {
            subject: subject_of(&subject),
            attribute: attribute.clone(),
        };
        let members: Vec<&Fact> = set
            .facts
            .iter()
            .filter(|f| f.subject == subject && f.attribute == attribute)
            .collect();
        let history = members.iter().filter(|f| is_history(f)).count();
        let mut open: Vec<&Fact> = members.iter().copied().filter(|f| is_open(f)).collect();
        open.sort_by_key(|f| (open_rank(f), f.id));
        let mut rows: Vec<FactRow> = open
            .iter()
            .filter_map(|f| {
                Some(FactRow::Value {
                    key: row_key(f)?,
                    value: f.value.clone()?,
                    private: private_info(f, dirs, kept),
                })
            })
            .collect();
        let has_current = members
            .iter()
            .any(|f| f.state == FactState::Live && f.tense == Tense::Current);
        let contested = members
            .iter()
            .any(|f| matches!(f.state, FactState::Contested { .. }));
        if rows.is_empty() {
            if let Some(directives) = rule_only(set, dirs, &group) {
                rows.push(FactRow::RuleOnly {
                    group: group.clone(),
                    directives,
                });
            } else if history > 0 {
                rows.push(FactRow::Vacant {
                    group: group.clone(),
                });
            }
        } else if !has_current && (history > 0 || contested) {
            rows.push(FactRow::Vacant {
                group: group.clone(),
            });
        }
        if rows.is_empty() {
            continue;
        }
        let mut hist: Vec<&Fact> = members.iter().copied().filter(|f| is_history(f)).collect();
        hist.sort_by_key(|f| f.id);
        let private_history = hist
            .into_iter()
            .filter_map(|f| {
                let p = private_info(f, dirs, kept)?;
                Some(PrivateHistoryValue {
                    value: f.value.clone()?,
                    directives: p.directives,
                })
            })
            .collect();
        out.push(FactGroup {
            attr: attr_info(attributes, &attribute),
            group,
            rows,
            history,
            private_history,
        });
    }
    FactList {
        groups: out,
        note: None,
    }
}

/// (主語, 属性) の置き換えの履歴 (`Superseded` の値。置き換えた時刻の新しい順、04 §7.4)。
#[must_use]
pub fn fact_history(set: &FactSet, attributes: &Attributes, group: &FactGroupKey) -> FactHistory {
    let subject = subject_name(&group.subject);
    let mut items: Vec<FactHistoryRow> = set
        .facts
        .iter()
        .filter(|f| f.subject == subject && f.attribute == group.attribute)
        .filter_map(|f| {
            let FactState::Superseded { at, .. } = f.state else {
                return None;
            };
            Some(FactHistoryRow {
                key: row_key(f)?,
                value: f.value.clone()?,
                superseded_at: at,
                private: f.private,
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b.superseded_at
            .cmp(&a.superseded_at)
            .then(b.key.fact.cmp(&a.key.fact))
    });
    FactHistory {
        group: group.clone(),
        attr: attr_info(attributes, &group.attribute),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{Directive, Source, refresh_private};
    use crate::rules::attributes;
    use crate::value_key::{ForgetKey, value_key};

    fn id(n: u64) -> Id {
        let mut b = [0u8; 16];
        b[8..].copy_from_slice(&n.to_be_bytes());
        Id::from_bytes(b)
    }

    fn key() -> ForgetKey {
        ForgetKey::from_bytes([5; 32])
    }

    fn fact(n: u64, a: &str, v: &str, state: FactState, turn: u64) -> Fact {
        Fact {
            id: id(n),
            subject: SELF.into(),
            attribute: a.into(),
            value: Some(v.into()),
            vk: Some(value_key(&key(), v)),
            tense: Tense::Current,
            state,
            sources: BTreeSet::from([Source {
                turn: id(turn),
                start: 0,
                end: 3,
                rev: 0,
            }]),
            private: false,
        }
    }

    fn private(n: u64, a: &str, v: &str, scope: Scope) -> Directive {
        Directive {
            id: id(n),
            kind: DirectiveKind::Private { scope },
            subject: SELF.into(),
            attribute: a.into(),
            vk: Some(value_key(&key(), v)),
        }
    }

    fn values(g: &FactGroup) -> Vec<(String, Option<FactStatus>)> {
        g.rows
            .iter()
            .map(|r| match r {
                FactRow::Value { key, value, .. } => (value.clone(), Some(key.status)),
                FactRow::Vacant { .. } => ("(vacant)".into(), None),
                FactRow::RuleOnly { .. } => ("(rule)".into(), None),
            })
            .collect()
    }

    /// 今の値と矛盾の値に行を振り、今の値が消えた属性と、指示だけが残る属性にも行を振る (04 §7.1)。
    #[test]
    fn rows_cover_values_vacant_attributes_and_rule_only_attributes() {
        let osaka = id(1);
        let mut set = FactSet {
            facts: vec![
                fact(1, "residence", "大阪", FactState::Live, 10),
                fact(
                    2,
                    "residence",
                    "東京",
                    FactState::Contested { against: osaka },
                    11,
                ),
                // 今の値が消えた属性: 履歴だけ。
                fact(
                    3,
                    "pet",
                    "犬",
                    FactState::Superseded {
                        by: Some(id(99)),
                        at: id(12).timestamp(),
                    },
                    12,
                ),
                // 忘れた値は行にしない。
                fact(4, "hobby", "釣り", FactState::Retracted, 13),
            ],
        };
        let dirs = Directives::new(vec![private(20, "dietary", "えび", Scope::Successors)]);
        refresh_private(&mut set, &dirs);
        let list = fact_list(&set, &dirs, attributes(), &BTreeSet::new());
        let names: Vec<&str> = list
            .groups
            .iter()
            .map(|g| g.group.attribute.as_str())
            .collect();
        // 属性の一覧の優先の順 (residence 20 → dietary 25 → pet)。
        assert_eq!(names, ["residence", "dietary", "pet"]);
        assert_eq!(
            values(&list.groups[0]),
            [
                ("大阪".into(), Some(FactStatus::Live)),
                ("東京".into(), Some(FactStatus::Contested))
            ]
        );
        assert_eq!(values(&list.groups[1]), [("(rule)".into(), None)]);
        assert_eq!(values(&list.groups[2]), [("(vacant)".into(), None)]);
        assert!(list.groups[2].private_history.is_empty());
        assert_eq!(list.groups[2].history, 1);
        assert!(list.groups[1].attr.multi, "dietary is multi-valued");
    }

    /// private の履歴の値は、効いている指示とともに一覧に載る (`/private off` の確認で示すため、04 §7.5)。
    #[test]
    fn private_history_values_carry_their_directives() {
        let mut set = FactSet {
            facts: vec![
                fact(
                    1,
                    "residence",
                    "京都",
                    FactState::Superseded {
                        by: Some(id(2)),
                        at: id(11).timestamp(),
                    },
                    30,
                ),
                fact(2, "residence", "大阪", FactState::Live, 31),
            ],
        };
        let dirs = Directives::new(vec![private(20, "residence", "大阪", Scope::Successors)]);
        refresh_private(&mut set, &dirs);
        let list = fact_list(&set, &dirs, attributes(), &BTreeSet::new());
        assert_eq!(
            list.groups[0].private_history,
            [PrivateHistoryValue {
                value: "京都".into(),
                directives: vec![id(20)],
            }]
        );
    }

    /// private の理由を、選んだ値・後継・削除で足した指示で見分ける (04 §7.5)。
    #[test]
    fn private_causes_are_told_apart() {
        let mut set = FactSet {
            facts: vec![
                fact(1, "dietary", "えび", FactState::Live, 10),
                fact(2, "dietary", "かに", FactState::Live, 30),
                fact(3, "dietary", "そば", FactState::Live, 11),
            ],
        };
        let dirs = Directives::new(vec![
            private(20, "dietary", "えび", Scope::Successors),
            private(21, "dietary", "そば", Scope::ValueOnly),
        ]);
        refresh_private(&mut set, &dirs);
        let kept = BTreeSet::from([id(21)]);
        let causes = |n: u64| {
            private_info(&set.facts[(n - 1) as usize], &dirs, &kept)
                .map(|p| (p.causes, p.directives))
        };
        assert_eq!(
            causes(1),
            Some((
                vec![PrivateCause::Chosen {
                    scope: PrivateScope::Successors
                }],
                vec![id(20)]
            ))
        );
        assert_eq!(
            causes(2),
            Some((vec![PrivateCause::Successor], vec![id(20)]))
        );
        assert_eq!(
            causes(3),
            Some((vec![PrivateCause::KeptAfterDelete], vec![id(21)]))
        );
        // 削除で足したのではない「この値だけ」は、選んだ値。
        assert_eq!(
            private_info(&set.facts[2], &dirs, &BTreeSet::new()).map(|p| p.causes),
            Some(vec![PrivateCause::Chosen {
                scope: PrivateScope::ValueOnly
            }])
        );
    }

    /// 行の参照は、鍵と状態の種類と時制で照合し、事実の ID は見ない (04 §7.1)。
    #[test]
    fn a_row_is_found_by_its_key_status_and_tense_only() {
        let set = FactSet {
            facts: vec![fact(1, "residence", "大阪", FactState::Live, 10)],
        };
        let mut k = row_key(&set.facts[0]).expect("a key");
        k.fact = Some(id(77));
        assert!(find_row(&set, &k).is_some());
        k.status = FactStatus::Contested;
        assert!(find_row(&set, &k).is_none(), "the shown state changed");
        k.status = FactStatus::Live;
        k.tense = FactTense::Planned;
        assert!(find_row(&set, &k).is_none(), "the shown tense changed");
    }

    #[test]
    fn history_is_newest_first() {
        let sup = |n: u64, v: &str, at: u64| {
            fact(
                n,
                "residence",
                v,
                FactState::Superseded {
                    by: None,
                    at: id(at).timestamp(),
                },
                at,
            )
        };
        let set = FactSet {
            facts: vec![
                sup(1, "京都", 1),
                sup(2, "神戸", 3),
                fact(3, "residence", "大阪", FactState::Live, 4),
            ],
        };
        let h = fact_history(
            &set,
            attributes(),
            &FactGroupKey {
                subject: FactSubject::User,
                attribute: "residence".into(),
            },
        );
        let values: Vec<&str> = h.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(values, ["神戸", "京都"]);
    }
}
