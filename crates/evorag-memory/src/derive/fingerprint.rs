//! 導出の指紋 (04 §5.3 の「導出の指紋」、03 §3.1.1 の `memory.derive_fingerprint`) と、単位の ID (04 §5.1 の「単位の
//! ID」)。どちらも、`<ラベル>` ‖ 0x00 の後に、(名前, 値) の組を決まった順に、名前と値をそれぞれ u32be の長さを前に
//! 置いて並べたバイト列の SHA-256 にする。

use std::collections::BTreeMap;
use std::fmt;

use evorag_types::Id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::dates::DateRules;
use crate::b3::{Dictionary, RuleVersions, Rules};
use crate::corrections::CORRECTION_V1;
use crate::ledger_select::{Current, Selection};
use crate::segment_keys::SEGMENT_KEYS_VERSION;
use crate::unit::UnitLayer;

/// 畳みの規則の版 (`derive`、04 §5.3 の指紋の成分)。畳みの規則 (このモジュールの `fold`・`correct`) か、単位の
/// 本体の作り方 (日付の選び方・根拠の並び・private の印) を変えたら上げる。
pub const DERIVE_VERSION: u32 = 1;

/// 指紋のラベル。
const FINGERPRINT_LABEL: &str = "evorag.derive.1";

/// 単位の ID のラベル。
const UNIT_LABEL: &str = "evorag.unit.1";

/// `label` ‖ 0x00 ‖ (u32be(len 名前) ‖ 名前 ‖ u32be(len 値) ‖ 値)… の SHA-256。
fn hash_pairs<'p>(label: &str, pairs: impl IntoIterator<Item = (&'p str, &'p [u8])>) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    h.update([0u8]);
    for (name, value) in pairs {
        for part in [name.as_bytes(), value] {
            let len = u32::try_from(part.len()).expect("a fingerprint component fits in u32");
            h.update(len.to_be_bytes());
            h.update(part);
        }
    }
    h.finalize().into()
}

/// 単位の ID (04 §5.1)。`layer`・`entity`・`aspect` (辞書の属性は名前、自由記述の属性は正規化した文字列)・`free`・
/// `vk` (64 桁の小文字の 16 進) から作り、`episode` は日付 (`when` を解いた `from`、無ければ発話の日) も入れる。値は
/// 本体に書く形の文字列 (`layer` は `world`・`episode`、`free` は `true`・`false`) にし、名前の順 (`aspect`・`date`・
/// `entity`・`free`・`layer`・`vk`) に並べる。SHA-256 の頭の 16 バイトを ID にする。
#[must_use]
pub fn unit_id(
    layer: UnitLayer,
    entity: &str,
    aspect: &str,
    free: bool,
    vk_hex: &str,
    date: Option<&str>,
) -> Id {
    let layer = match layer {
        UnitLayer::World => "world",
        UnitLayer::Episode => "episode",
    };
    let free = if free { "true" } else { "false" };
    let mut pairs: Vec<(&str, &[u8])> = vec![("aspect", aspect.as_bytes())];
    if let Some(d) = date {
        pairs.push(("date", d.as_bytes()));
    }
    pairs.extend([
        ("entity", entity.as_bytes()),
        ("free", free.as_bytes()),
        ("layer", layer.as_bytes()),
        ("vk", vk_hex.as_bytes()),
    ]);
    let h = hash_pairs(UNIT_LABEL, pairs);
    let mut id = [0u8; 16];
    id.copy_from_slice(&h[..16]);
    Id::from_bytes(id)
}

/// 台帳 1 つの指紋の成分 (台帳の ID と、本体のバイト列の SHA-256)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerDigest {
    pub id: Id,
    pub sha256: [u8; 32],
}

/// 指示の件数と最大の ID (指示は追記だけなので、この 2 つで集合が決まる、04 §7.5)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DirectiveStamp {
    pub count: u64,
    pub max: Option<Id>,
}

/// 指紋に入る版 (04 §5.3: 辞書の版、規則 (前のふるい・文の分け方・関門・読み取り・訂正の型・日付・機微) の版、
/// 畳みの規則の版、プロファイルの規則の版、区切りの鍵の作り方の版)。読み取りの規則の版は `rules/<言語>.toml` の
/// 版 (`rules.languages`)、機微の規則の版は `rules/b3.toml` の版 (`rules.b3`) が持つ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Versions {
    /// 辞書の版。
    pub dictionary: u32,
    /// 関門の規則の版 (関門の実装・`rules/common.toml`・`rules/<言語>.toml`・`rules/b3.toml`・前のふるい・文の
    /// 分け方)。
    pub rules: RuleVersions,
    /// 訂正の字句の型の版。
    pub correction: u32,
    /// 日付の規則の版。
    pub dates: u32,
    /// 畳みの規則の版 ([`DERIVE_VERSION`])。
    pub derive: u32,
    /// プロファイルの規則の版 (プロファイルを作る側が渡す)。
    pub profile: u32,
    /// 区切りの鍵の作り方の版 ([`SEGMENT_KEYS_VERSION`])。
    pub segment_keys: u32,
}

impl Versions {
    /// この版の規則と、辞書 `dictionary`・規則 `rules`・日付の規則 `dates`・プロファイルの規則の版 `profile` の版。
    #[must_use]
    pub fn new(
        dictionary: &Dictionary,
        rules: &Rules<'_>,
        dates: &DateRules,
        profile: u32,
    ) -> Self {
        Self {
            dictionary: dictionary.version(),
            rules: rules.versions(),
            correction: CORRECTION_V1,
            dates: dates.version,
            derive: DERIVE_VERSION,
            profile,
            segment_keys: SEGMENT_KEYS_VERSION,
        }
    }

    fn pairs(&self) -> Vec<(String, Vec<u8>)> {
        let n = |v: u32| v.to_string().into_bytes();
        let r = &self.rules;
        let mut out = vec![
            ("version.dictionary".to_owned(), n(self.dictionary)),
            ("version.gates".to_owned(), n(r.gates)),
            ("version.common".to_owned(), n(r.common)),
        ];
        let mut languages = r.languages.clone();
        languages.sort();
        out.extend(
            languages
                .iter()
                .map(|(lang, v)| (format!("version.language.{lang}"), n(*v))),
        );
        out.extend([
            ("version.b3".to_owned(), n(r.b3)),
            ("version.prefilter".to_owned(), n(r.prefilter)),
            ("version.splitter".to_owned(), n(r.splitter)),
            ("version.correction".to_owned(), n(self.correction)),
            ("version.dates".to_owned(), n(self.dates)),
            ("version.derive".to_owned(), n(self.derive)),
            ("version.profile".to_owned(), n(self.profile)),
            ("version.segment_keys".to_owned(), n(self.segment_keys)),
        ]);
        out
    }
}

/// 導出の指紋の成分 (04 §5.3)。
#[derive(Debug, Clone, Copy)]
pub struct Components<'a> {
    /// 台帳ごとの (ID, 本体の SHA-256)。読めた・読めないに依らず、すべての台帳 (`proposal`) を渡す。順は問わない。
    pub ledgers: &'a [LedgerDigest],
    /// 指示の件数と最大の ID。
    pub directives: DirectiveStamp,
    pub versions: &'a Versions,
    /// 今の選択 (04 §5.2.1)。
    pub current: Current<'a>,
    /// 直前の選択。
    pub previous: Option<Selection<'a>>,
}

/// 選択の形を、区切りの曖昧さの無いバイト列にする (`None` は 0x00、`Some` は 0x01 ‖ u32be(len) ‖ バイト列)。
fn encode(parts: &[Option<&str>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            None => out.push(0),
            Some(s) => {
                out.push(1);
                let len = u32::try_from(s.len()).expect("a selection fits in u32");
                out.extend(len.to_be_bytes());
                out.extend(s.as_bytes());
            }
        }
    }
    out
}

fn current_bytes(c: Current<'_>) -> Vec<u8> {
    match c {
        Current::Ready(s) => encode(&[Some("ready"), Some(s.variant), Some(s.model_key)]),
        Current::Fallback {
            variant,
            intended,
            called,
        } => encode(&[Some("fallback"), Some(variant), intended, Some(called)]),
    }
}

fn previous_bytes(p: Option<Selection<'_>>) -> Vec<u8> {
    match p {
        None => encode(&[None]),
        Some(s) => encode(&[Some(s.variant), Some(s.model_key)]),
    }
}

fn id_bytes(id: Option<Id>) -> Vec<u8> {
    id.map_or_else(Vec::new, |i| i.as_bytes().to_vec())
}

impl Components<'_> {
    fn sorted_ledgers(&self) -> Vec<LedgerDigest> {
        let mut l = self.ledgers.to_vec();
        l.sort();
        l.dedup();
        l
    }

    /// 指紋の (名前, 値) の組 (決まった順)。`light` なら、台帳ごとの成分の代わりに台帳の件数と最大の ID を入れる
    /// (起動時の軽い検査の成分、04 §5.3)。
    fn pairs(&self, light: bool) -> Vec<(String, Vec<u8>)> {
        let ledgers = self.sorted_ledgers();
        let mut out = Vec::new();
        if light {
            out.push((
                "ledgers.count".to_owned(),
                ledgers.len().to_string().into_bytes(),
            ));
            out.push((
                "ledgers.max".to_owned(),
                id_bytes(ledgers.last().map(|l| l.id)),
            ));
        } else {
            let mut v = Vec::with_capacity(ledgers.len() * 48);
            for l in &ledgers {
                v.extend(l.id.as_bytes());
                v.extend(l.sha256);
            }
            out.push(("ledgers".to_owned(), v));
        }
        out.push((
            "directives.count".to_owned(),
            self.directives.count.to_string().into_bytes(),
        ));
        out.push(("directives.max".to_owned(), id_bytes(self.directives.max)));
        out.extend(self.versions.pairs());
        out.push(("selection.current".to_owned(), current_bytes(self.current)));
        out.push((
            "selection.previous".to_owned(),
            previous_bytes(self.previous),
        ));
        out
    }

    /// 起動時の軽い検査に使う成分 (04 §5.3: 台帳の件数と最大の ID・指示の件数と最大の ID・版・今の選択と直前の
    /// 選択)。指紋と一緒に記録し、今の成分と比べて、合わなければ全件を導き直す。
    #[must_use]
    pub fn stamp(&self) -> Stamp {
        Stamp {
            components: self
                .pairs(true)
                .into_iter()
                .map(|(k, v)| (k, hex(&v)))
                .collect(),
        }
    }
}

/// 導出の指紋 (04 §5.3)。
#[must_use]
pub fn fingerprint(c: &Components<'_>) -> Fingerprint {
    let pairs = c.pairs(false);
    Fingerprint(hash_pairs(
        FINGERPRINT_LABEL,
        pairs.iter().map(|(k, v)| (k.as_str(), v.as_slice())),
    ))
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// 導出の指紋の値 (SHA-256)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// 記録に書く形 (64 桁の小文字の 16 進)。
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// バイト列。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// 起動時の軽い検査の成分 ([`Components::stamp`])。名前 → 値 (16 進)。指紋と一緒に記録し、等しさだけで比べる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub components: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::super::dates::date_rules;
    use super::*;
    use crate::b3::Rules;

    fn id(n: u8) -> Id {
        let mut b = [0u8; 16];
        b[5] = n;
        b[6] = 0x70;
        Id::from_bytes(b)
    }

    fn versions() -> Versions {
        Versions::new(Dictionary::bundled(), &Rules::bundled(), date_rules(), 1)
    }

    const M1: Selection<'static> = Selection {
        variant: "sr2",
        model_key: "m1",
    };

    fn base<'a>(ledgers: &'a [LedgerDigest], v: &'a Versions) -> Components<'a> {
        Components {
            ledgers,
            directives: DirectiveStamp {
                count: 2,
                max: Some(id(9)),
            },
            versions: v,
            current: Current::Ready(M1),
            previous: None,
        }
    }

    #[test]
    fn unit_ids_follow_the_content() {
        let a = unit_id(UnitLayer::World, "self", "residence", false, "ab", None);
        assert_eq!(
            a,
            unit_id(UnitLayer::World, "self", "residence", false, "ab", None)
        );
        for b in [
            unit_id(UnitLayer::Episode, "self", "residence", false, "ab", None),
            unit_id(UnitLayer::World, "妻", "residence", false, "ab", None),
            unit_id(UnitLayer::World, "self", "residence", true, "ab", None),
            unit_id(UnitLayer::World, "self", "hobby", false, "ab", None),
            unit_id(UnitLayer::World, "self", "residence", false, "ac", None),
            unit_id(
                UnitLayer::World,
                "self",
                "residence",
                false,
                "ab",
                Some("2026-01-01"),
            ),
        ] {
            assert_ne!(a, b);
        }
        // 長さを前に置くので、名前と値の境を動かしても同じにならない。
        assert_ne!(
            unit_id(UnitLayer::World, "ab", "c", false, "x", None),
            unit_id(UnitLayer::World, "a", "bc", false, "x", None)
        );
    }

    /// 入力を変える成分のどれを変えても指紋が変わり、台帳の順は効かない (09 §3.5.1)。
    #[test]
    fn every_component_changes_the_fingerprint() {
        let v = versions();
        let l = [
            LedgerDigest {
                id: id(1),
                sha256: [1; 32],
            },
            LedgerDigest {
                id: id(2),
                sha256: [2; 32],
            },
        ];
        let reversed = [l[1], l[0]];
        let f0 = fingerprint(&base(&l, &v));
        assert_eq!(f0, fingerprint(&base(&reversed, &v)), "order of ledgers");
        assert_eq!(f0.to_hex().len(), 64);

        let mut changed: Vec<Fingerprint> = Vec::new();
        // 台帳の追加・掃除 (本体の変化)・置き換え (ID の変化)。
        let added = [
            l[0],
            l[1],
            LedgerDigest {
                id: id(3),
                sha256: [3; 32],
            },
        ];
        changed.push(fingerprint(&base(&added, &v)));
        let scrubbed = [
            l[0],
            LedgerDigest {
                id: id(2),
                sha256: [9; 32],
            },
        ];
        changed.push(fingerprint(&base(&scrubbed, &v)));
        let replaced = [
            l[0],
            LedgerDigest {
                id: id(4),
                sha256: [2; 32],
            },
        ];
        changed.push(fingerprint(&base(&replaced, &v)));
        // 指示の追加。
        let mut c = base(&l, &v);
        c.directives = DirectiveStamp {
            count: 3,
            max: Some(id(10)),
        };
        changed.push(fingerprint(&c));
        // 版。
        let mut bumps: Vec<Versions> = Vec::new();
        for k in 0..9 {
            let mut w = v.clone();
            match k {
                0 => w.dictionary += 1,
                1 => w.rules.gates += 1,
                2 => w.rules.b3 += 1,
                3 => w.rules.prefilter += 1,
                4 => w.correction += 1,
                5 => w.dates += 1,
                6 => w.derive += 1,
                7 => w.profile += 1,
                _ => w.segment_keys += 1,
            }
            bumps.push(w);
        }
        let mut lang = v.clone();
        lang.rules.languages[0].1 += 1;
        bumps.push(lang);
        for w in &bumps {
            changed.push(fingerprint(&base(&l, w)));
        }
        // 選択。
        let mut c = base(&l, &v);
        c.current = Current::Ready(Selection {
            variant: "sr2",
            model_key: "m2",
        });
        changed.push(fingerprint(&c));
        let mut c = base(&l, &v);
        c.previous = Some(M1);
        changed.push(fingerprint(&c));
        let mut c = base(&l, &v);
        c.current = Current::Fallback {
            variant: "sr2",
            intended: Some("m1"),
            called: "chat",
        };
        changed.push(fingerprint(&c));
        let mut c = base(&l, &v);
        c.current = Current::Fallback {
            variant: "sr2",
            intended: None,
            called: "chat",
        };
        changed.push(fingerprint(&c));

        for (i, f) in changed.iter().enumerate() {
            assert_ne!(*f, f0, "component {i} does not change the fingerprint");
        }
        let distinct: std::collections::BTreeSet<String> =
            changed.iter().map(Fingerprint::to_hex).collect();
        assert_eq!(distinct.len(), changed.len());
    }

    #[test]
    fn the_stamp_follows_the_light_components() {
        let v = versions();
        let l = [LedgerDigest {
            id: id(1),
            sha256: [1; 32],
        }];
        let s = base(&l, &v).stamp();
        // 台帳の本体だけの変化 (掃除) は軽い検査では見えない (導き直しのときの指紋で分かる)。
        let scrubbed = [LedgerDigest {
            id: id(1),
            sha256: [7; 32],
        }];
        assert_eq!(s, base(&scrubbed, &v).stamp());
        let added = [
            l[0],
            LedgerDigest {
                id: id(2),
                sha256: [2; 32],
            },
        ];
        assert_ne!(s, base(&added, &v).stamp());
        let json = serde_json::to_string(&s).expect("serializes");
        assert_eq!(serde_json::from_str::<Stamp>(&json).expect("parses"), s);
    }
}
