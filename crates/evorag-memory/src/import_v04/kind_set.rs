//! v0.4 のデータ根の種類 (03 §3.3)。会話の層の種類 (`crate::kinds`) に、v0.4 だけの種類 ([`V04_ONLY_KINDS`]) を
//! 足したもの。v0.4 だけの種類の名前・版・関係と副索引の作り方 (`derive`) をここに持ち、旧エンジンを外した後も
//! v0.4 の根を同じ種類で開けるようにする。
//!
//! 関係と副索引を作るのに要る項目は、1 つずつ別の型で読む (03 §4.2 の規則 11)。新しい版でほかの項目の型が
//! 変わっても、読める項目の関係と副索引 (特に再び覚えない印の `mark`) を失わないようにする。

use std::collections::BTreeSet;

use evorag_store::key::OwnedPart;
use evorag_store::{Envelope, KindSpec, WriteOp};
use evorag_types::Id;
use serde::Deserialize;

/// 事実 (04 §5.1、v0.3)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_FACT: &str = "fact";
/// 事実への指示 (04 §7.5、v0.3)。利用者の明示の操作なのでジャーナルに書く。追記だけ。
pub const KIND_FACT_DIRECTIVE: &str = "fact_directive";
/// 訂正の候補と検証済みの訂正 (04 §6、03 §3.3、v0.3)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_CORRECTION: &str = "correction";
/// 話題の検索の鍵 (04 §2、v0.2)。作り直せる派生の記録なので、ジャーナルに書かない。
pub const KIND_NOTE: &str = "note";
/// 関係: 事実 → 出典のターン (03 §3.3)。
pub const REL_FACT_SRC: &str = "fact_src";
/// 関係: 指示 → 解く相手の指示。
pub const REL_CANCELS: &str = "cancels";
/// 関係: 訂正 → 候補の発話のターン (03 §3.3 の「対象のターンへ」)。
pub const REL_CORRECTION_OF: &str = "correction_of";
/// 関係: ノート → 出典の往復の発話のターン (04 §2)。
pub const REL_NOTE_OF: &str = "note_of";

/// `fact` の本体の版 (03 §3.3)。これより新しい `v` の記録は読むだけにし、書き換えない (03 §4.2 の規則 6)。
pub(crate) const FACT_V: u32 = 1;
/// `fact_directive` の本体の版。
pub(crate) const FACT_DIRECTIVE_V: u32 = 1;
/// `correction` の本体の版。これより新しい `v` の記録は読むだけにし、書き換えない (03 §4.2 の規則 6)。
pub(crate) const CORRECTION_V: u32 = 1;
/// `note` の本体の版。
const NOTE_V: u32 = 1;

/// 副索引: 主語・U+0000・属性 (03 §3.3)。
const LOOKUP_SA: &str = "sa";
/// 副索引: 値の鍵の 16 進。
pub(crate) const LOOKUP_VK: &str = "vk";
/// 副索引: 再び覚えない印の 16 進 (指示だけ)。
pub(crate) const LOOKUP_MARK: &str = "mark";

/// v0.5 の根で作らない種類 (03 §3.3)。v0.5 は、v0.4 のデータ根から取り込むときにだけ読む (`import-v04`)。
pub const V04_ONLY_KINDS: [&str; 4] = [KIND_FACT, KIND_CORRECTION, KIND_NOTE, KIND_FACT_DIRECTIVE];

/// v0.4 のデータ根 (形式の版 `0.3`) の種類。会話の層の種類に [`V04_ONLY_KINDS`] を足したもの。`import-v04` が
/// v0.4 の根を読むだけで開くときに渡す (03 §12.4)。
#[must_use]
pub fn kinds_v04() -> Vec<KindSpec> {
    let mut out = crate::kinds::conversation_kinds();
    out.extend(v04_only_kinds());
    out
}

/// v0.4 だけの種類 ([`V04_ONLY_KINDS`] と同じ順)。
pub(crate) fn v04_only_kinds() -> [KindSpec; 4] {
    [
        // 事実は LLM で作る派生の記録 (04 §2)。ジャーナルに書かない。関係と副索引は本体から作り直せる。
        KindSpec {
            derive: Some(derive_fact),
            derived_rels: FACT_RELS,
            derived_lookups: FACT_LOOKUPS,
            ..KindSpec::new(KIND_FACT, FACT_V)
        },
        // 訂正の候補と検証済みの訂正は、字句の規則と LLM で作る派生の記録 (04 §6)。ジャーナルに書かない。関係は
        // 本体 (`turn`) から作り直せる。
        KindSpec {
            derive: Some(derive_correction),
            derived_rels: &[REL_CORRECTION_OF],
            ..KindSpec::new(KIND_CORRECTION, CORRECTION_V)
        },
        // ノートは作り直せる派生の記録 (04 §2)。関係は書くときに足し、`repair` は本体から作り直す (03 §10)。
        KindSpec {
            derive: Some(derive_note),
            derived_rels: &[REL_NOTE_OF],
            ..KindSpec::new(KIND_NOTE, NOTE_V)
        },
        // 事実への指示は利用者の明示の操作の正本 (04 §7.5)。ジャーナルに書き、追記だけにする。
        KindSpec {
            create_when_newer_present: true,
            journaled: true,
            derive: Some(derive_directive),
            derived_rels: DIRECTIVE_RELS,
            derived_lookups: DIRECTIVE_LOOKUPS,
            ..KindSpec::new(KIND_FACT_DIRECTIVE, FACT_DIRECTIVE_V)
        },
    ]
}

fn sa_key(subject: &str, attribute: &str) -> String {
    format!("{subject}\u{0}{attribute}")
}

#[derive(Deserialize)]
pub(crate) struct SubjectAttribute {
    pub(crate) subject: String,
    pub(crate) attribute: String,
}

#[derive(Deserialize)]
struct VkField {
    #[serde(default)]
    vk: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SourcesField {
    #[serde(default)]
    pub(crate) sources: Vec<SourceTurn>,
}

#[derive(Deserialize)]
pub(crate) struct SourceTurn {
    pub(crate) turn: Id,
}

#[derive(Deserialize)]
struct TargetField {
    #[serde(default)]
    target: Option<Id>,
}

#[derive(Deserialize)]
pub(crate) struct MarksField {
    #[serde(default)]
    pub(crate) marks: Vec<String>,
}

/// 記録から作る関係と副索引の 1 項目。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IndexEntry {
    Lookup { field: &'static str, value: String },
    Link { rel: &'static str, to: Id },
}

impl IndexEntry {
    pub(crate) fn put(&self, kind: &str, id: Id) -> WriteOp {
        match self {
            IndexEntry::Lookup { field, value } => WriteOp::PutLookup {
                kind: kind.into(),
                field: (*field).into(),
                value: OwnedPart::Str(value.clone()),
                id,
            },
            IndexEntry::Link { rel, to } => WriteOp::PutLink {
                from: id,
                rel: (*rel).into(),
                to: *to,
                attrs: Vec::new(),
            },
        }
    }

    pub(crate) fn delete(&self, kind: &str, id: Id) -> WriteOp {
        match self {
            IndexEntry::Lookup { field, value } => WriteOp::DeleteLookup {
                kind: kind.into(),
                field: (*field).into(),
                value: OwnedPart::Str(value.clone()),
                id,
            },
            IndexEntry::Link { rel, to } => WriteOp::DeleteLink {
                from: id,
                rel: (*rel).into(),
                to: *to,
            },
        }
    }
}

/// `fact` の関係と副索引の集合 (本体から決まる)。
pub(crate) fn fact_entries(e: &Envelope) -> BTreeSet<IndexEntry> {
    let mut out = BTreeSet::new();
    if let Ok(b) = e.body_as::<SubjectAttribute>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_SA,
            value: sa_key(&b.subject, &b.attribute),
        });
    }
    if let Ok(VkField { vk: Some(vk) }) = e.body_as::<VkField>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_VK,
            value: vk,
        });
    }
    if let Ok(b) = e.body_as::<SourcesField>() {
        out.extend(b.sources.into_iter().map(|s| IndexEntry::Link {
            rel: REL_FACT_SRC,
            to: s.turn,
        }));
    }
    out
}

/// `fact_directive` の関係と副索引の集合。
pub(crate) fn directive_entries(e: &Envelope) -> BTreeSet<IndexEntry> {
    let mut out = BTreeSet::new();
    if let Ok(b) = e.body_as::<SubjectAttribute>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_SA,
            value: sa_key(&b.subject, &b.attribute),
        });
    }
    if let Ok(VkField { vk: Some(vk) }) = e.body_as::<VkField>() {
        out.insert(IndexEntry::Lookup {
            field: LOOKUP_VK,
            value: vk,
        });
    }
    if let Ok(b) = e.body_as::<MarksField>() {
        out.extend(b.marks.into_iter().map(|m| IndexEntry::Lookup {
            field: LOOKUP_MARK,
            value: m,
        }));
    }
    if let Ok(TargetField { target: Some(t) }) = e.body_as::<TargetField>() {
        out.insert(IndexEntry::Link {
            rel: REL_CANCELS,
            to: t,
        });
    }
    out
}

/// [`derive_fact`] が作りうる関係の名前 ([`fact_entries`])。
const FACT_RELS: &[&str] = &[REL_FACT_SRC];
/// [`derive_fact`] が作りうる副索引の項目。
const FACT_LOOKUPS: &[&str] = &[LOOKUP_SA, LOOKUP_VK];
/// [`derive_directive`] が作りうる関係の名前 ([`directive_entries`])。
const DIRECTIVE_RELS: &[&str] = &[REL_CANCELS];
/// [`derive_directive`] が作りうる副索引の項目。
const DIRECTIVE_LOOKUPS: &[&str] = &[LOOKUP_SA, LOOKUP_VK, LOOKUP_MARK];

/// `fact` の関係と副索引 (ジャーナルの取り込みや repair で、本体から作り直すときにも使う)。
pub(crate) fn derive_fact(e: &Envelope) -> Vec<WriteOp> {
    fact_entries(e)
        .iter()
        .map(|x| x.put(KIND_FACT, e.id))
        .collect()
}

/// `fact_directive` の関係と副索引。
pub(crate) fn derive_directive(e: &Envelope) -> Vec<WriteOp> {
    directive_entries(e)
        .iter()
        .map(|x| x.put(KIND_FACT_DIRECTIVE, e.id))
        .collect()
}

/// 本体の `turn` だけ (関係を作り直すのに要る項目、03 §4.2 の規則 11)。
#[derive(Deserialize)]
struct TurnField {
    turn: Id,
}

/// `correction` の関係 (ジャーナルの取り込みや repair で、本体から作り直すときにも使う)。
pub(crate) fn derive_correction(e: &Envelope) -> Vec<WriteOp> {
    e.body_as::<TurnField>()
        .map(|b| {
            vec![WriteOp::PutLink {
                from: e.id,
                rel: REL_CORRECTION_OF.into(),
                to: b.turn,
                attrs: Vec::new(),
            }]
        })
        .unwrap_or_default()
}

/// ノートの本体のうち、関係を作るのに要る項目だけ。
#[derive(Deserialize)]
struct NoteLinks {
    exchanges: Vec<Id>,
}

/// `note` の関係 (出典の往復の発話のターンへ)。
fn derive_note(e: &Envelope) -> Vec<WriteOp> {
    match e.body_as::<NoteLinks>() {
        Ok(b) => b
            .exchanges
            .into_iter()
            .map(|t| WriteOp::PutLink {
                from: e.id,
                rel: REL_NOTE_OF.into(),
                to: t,
                attrs: Vec::new(),
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.4 の根の種類は、会話の層の種類と v0.4 だけの種類を重ならずに合わせたもの。v0.5 の根の種類は、v0.4 だけの
    /// 種類を持たない。
    #[test]
    fn v04_kinds_are_the_conversation_kinds_and_the_v04_only_kinds() {
        let names = |ks: Vec<KindSpec>| -> Vec<&'static str> {
            let mut v: Vec<&'static str> = ks.into_iter().map(|k| k.name).collect();
            v.sort_unstable();
            v
        };
        let conversation = names(crate::kinds::conversation_kinds());
        let only = names(v04_only_kinds().into());
        assert!(conversation.iter().all(|n| !only.contains(n)), "{only:?}");
        let mut want = only.clone();
        want.extend(&conversation);
        want.sort_unstable();
        assert_eq!(names(kinds_v04()), want);
        let mut declared = V04_ONLY_KINDS.to_vec();
        declared.sort_unstable();
        assert_eq!(only, declared);
        let v05 = names(crate::kinds_v05());
        assert!(V04_ONLY_KINDS.iter().all(|n| !v05.contains(n)), "{v05:?}");
    }
}
