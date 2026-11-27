//! 関門 (B3) の規則と辞書 (04 §5.1 の「辞書」、§5.2 の「規則の TOML」)。
//!
//! - [`Rules`]: 関門に当てる規則の集まり。旧エンジンと共有する同梱の規則 (`rules/common.toml`・
//!   `rules/<言語>.toml`) と、v0.5 で足した規則 (`rules/b3.toml`: 関門 3b の否定の印・関門 5 の応答する側を表す語・
//!   関門 6a の機微な情報) を合わせて持つ。
//! - [`Dictionary`]: 辞書の属性 (`attributes.toml`) に、v0.5 の宣言 (層・別名・値の意味が否定を含む属性) を足した
//!   もの。属性の決め方 (§5.2) で自由記述の属性を辞書の属性に写すのに使う。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::rules::{
    Attribute, Attributes, CommonRules, LanguageRules, all_language_rules, attributes,
    common_rules, fold,
};
use crate::unit::UnitLayer;
use crate::value_key::normalize_v1;

/// 関門の実装の版 (規則の TOML の版とは別。関門の当て方のコードを変えたら上げる。導出の指紋と関門の結果の覚え
/// (04 §5.3) に入る)。
pub const GATES_VERSION: u32 = 1;

/// 前のふるいの規則の版 (台帳の `inputs` の `prefilter`、04 §5.2.1)。B2 の側で足す前のふるいの版の定数と同じ値に
/// する (M5.3 で、その定数につなぎ替える)。関門は今の規則の前のふるいで本文を見るので、関門の結果の覚えの鍵に入れる。
pub const PREFILTER_VERSION: u32 = 1;

/// `rules/b3.toml` の言語ごとの部分。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageExtra {
    /// 関門 3b の否定の印。
    pub negation_mark: Vec<String>,
    /// 否定の印の一致が、この語の一致 1 つにすっかり収まるときは数えない。
    pub negation_mark_exclude: Vec<String>,
    /// 関門 3b の節の区切り (接続の区切り。読点だけでは切らない)。
    pub polarity_clause_break: Vec<String>,
    /// 関門 3b: 値の意味が否定を含む属性の `negated` を通す、否定を終える語 (「食べられるようになった」など)。
    pub negative_end: Vec<String>,
    /// 関門 6a の機微な情報。
    pub sensitive: Sensitive,
}

/// 関門 6a の機微な情報のカテゴリの名前 (04 §5.2、OQ-56)。
pub const SENSITIVE_CATEGORIES: &[&str] = &[
    "health",
    "religion",
    "politics",
    "orientation",
    "address",
    "race",
    "creed",
    "social_status",
    "criminal_record",
    "crime_victim",
    "disability",
];

/// 関門 6a の機微な情報 (04 §5.2)。日本語の語は部分一致、英語の語は ASCII の語の境界で当てる (末尾の `*` は語の
/// 頭だけの境界)。電話番号・カード番号・メールアドレスは、コードの型で見る。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sensitive {
    /// カテゴリ ([`SENSITIVE_CATEGORIES`]) → 語。
    pub words: BTreeMap<String, Vec<String>>,
    /// 語の一致が、この語の一致 1 つにすっかり収まるときは数えない (照合の前に伏せる)。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 番地・号室・郵便番号の形の型 ([`shape_parts`])。
    pub address_shapes: Vec<String>,
}

impl Sensitive {
    /// 語の一覧のすべて (カテゴリの名前の順)。
    pub fn all_words(&self) -> impl Iterator<Item = &String> {
        self.words.values().flatten()
    }
}

/// `rules/b3.toml` の `[dictionary]` (辞書の v0.5 の宣言)。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DictionaryDecl {
    /// 値の意味が否定を含む属性 (関門 3b を当てない)。
    #[serde(default)]
    pub negative: Vec<String>,
    /// 層の宣言 (書かない属性は `world`)。
    #[serde(default)]
    pub layer: BTreeMap<String, UnitLayer>,
    /// 別名 (属性の名前 → 自由記述の属性をその属性に写す語)。
    #[serde(default)]
    pub aliases: BTreeMap<String, Vec<String>>,
}

/// `rules/b3.toml` の `[subject]`。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectRules {
    /// 関門 5: 応答する側を表す語。
    pub assistant: Vec<String>,
    /// 関門 5: 二人称 (応答する側を指す)。
    pub second_person: Vec<String>,
}

/// `rules/b3.toml` の全体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct B3Rules {
    pub version: u32,
    pub dictionary: DictionaryDecl,
    pub subject: SubjectRules,
    pub ja: LanguageExtra,
    pub en: LanguageExtra,
}

impl B3Rules {
    /// 言語の部分 (`ja`・`en`)。
    #[must_use]
    pub fn language(&self, lang: &str) -> Option<&LanguageExtra> {
        match lang {
            "ja" => Some(&self.ja),
            "en" => Some(&self.en),
            _ => None,
        }
    }

    /// 規則の検査。問題の一覧を返す (空なら問題なし)。`attributes` は辞書の宣言の名前を確かめるのに使う。
    #[must_use]
    pub fn problems(&self, attributes: &Attributes) -> Vec<String> {
        let mut out = Vec::new();
        if self.version == 0 {
            out.push("rules/b3.toml: version must be 1 or more".to_owned());
        }
        let d = &self.dictionary;
        let named = d
            .negative
            .iter()
            .chain(d.layer.keys())
            .chain(d.aliases.keys());
        for name in named {
            if attributes.get(name).is_none() {
                out.push(format!(
                    "rules/b3.toml: dictionary names `{name}`, which is not an attribute"
                ));
            }
        }
        words_problems(&mut out, "subject.assistant", &self.subject.assistant);
        words_problems(
            &mut out,
            "subject.second_person",
            &self.subject.second_person,
        );
        for (lang, x) in [("ja", &self.ja), ("en", &self.en)] {
            let at = |n: &str| format!("{lang}.{n}");
            for (name, list) in [
                ("negation_mark", &x.negation_mark),
                ("negation_mark_exclude", &x.negation_mark_exclude),
                ("polarity_clause_break", &x.polarity_clause_break),
                ("negative_end", &x.negative_end),
                ("sensitive.exclude", &x.sensitive.exclude),
                ("sensitive.address_shapes", &x.sensitive.address_shapes),
            ] {
                words_problems(&mut out, &at(name), list);
            }
            for (category, list) in &x.sensitive.words {
                if !SENSITIVE_CATEGORIES.contains(&category.as_str()) {
                    out.push(format!(
                        "rules/b3.toml: {lang}.sensitive.words.{category} is not a category"
                    ));
                }
                let stems: Vec<String> = list
                    .iter()
                    .map(|w| w.strip_suffix('*').unwrap_or(w).to_owned())
                    .collect();
                words_problems(
                    &mut out,
                    &at(&format!("sensitive.words.{category}")),
                    &stems,
                );
            }
            for shape in &x.sensitive.address_shapes {
                if shape_parts(shape).is_none() {
                    out.push(format!(
                        "rules/b3.toml: {lang}.sensitive.address_shapes `{shape}` needs a number hole and only known holes"
                    ));
                }
            }
        }
        out
    }
}

/// 語の一覧の共通の検査: 空でない、重複しない、NFKC と casefold で変わらない。
fn words_problems(out: &mut Vec<String>, name: &str, list: &[String]) {
    let mut seen = BTreeSet::new();
    for w in list {
        if w.trim().is_empty() {
            out.push(format!("rules/b3.toml: {name} has an empty entry"));
        }
        if !seen.insert(w.as_str()) {
            out.push(format!("rules/b3.toml: {name} has a duplicate `{w}`"));
        }
        if fold(w) != *w {
            out.push(format!(
                "rules/b3.toml: {name} entry `{w}` changes under NFKC and case folding (it would never match)"
            ));
        }
    }
}

/// 番地の形の型の部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ShapePart {
    /// 字のまま当てる部分。
    Text(String),
    /// 数字の並び (`{n}`。算用数字と漢数字の 1 文字以上)。
    Number,
    /// ちょうど k 桁の数字 (`{n3}` など。前後に数字が続かない)。
    Digits(usize),
    /// 英字の語 1 つ (`{w}`)。
    Word,
    /// 空白 1 つで区切った英字の語の 1〜3 個 (`{ws}`)。
    Words,
    /// 漢字かカタカナの 1 文字 (`{c}`。町名の終わりの字)。
    Cjk,
}

/// 型を部分に分ける。数字の穴を 1 つも含まないか、知らない穴を含むなら `None`。
pub(super) fn shape_parts(shape: &str) -> Option<Vec<ShapePart>> {
    let mut parts = Vec::new();
    let mut rest = shape;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            parts.push(ShapePart::Text(rest[..open].to_owned()));
        }
        let close = rest[open..].find('}')? + open;
        let hole = &rest[open + 1..close];
        parts.push(match hole {
            "n" => ShapePart::Number,
            "w" => ShapePart::Word,
            "ws" => ShapePart::Words,
            "c" => ShapePart::Cjk,
            _ => {
                let k: usize = hole.strip_prefix('n')?.parse().ok()?;
                if !(1..=9).contains(&k) {
                    return None;
                }
                ShapePart::Digits(k)
            }
        });
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        parts.push(ShapePart::Text(rest.to_owned()));
    }
    parts
        .iter()
        .any(|p| matches!(p, ShapePart::Number | ShapePart::Digits(_)))
        .then_some(parts)
}

/// 同梱の `rules/b3.toml`。
///
/// # Panics
///
/// 同梱のファイルが読めないとき (試験 `bundled_b3_rules_are_valid` で防ぐ)。
#[must_use]
pub fn b3_rules() -> &'static B3Rules {
    static CELL: OnceLock<B3Rules> = OnceLock::new();
    CELL.get_or_init(|| {
        toml::from_str(include_str!("../../rules/b3.toml"))
            .unwrap_or_else(|e| panic!("the bundled rules/b3.toml parses: {e}"))
    })
}

/// 関門に当てる規則の集まり。安全の規則・問いの印・節の区切り・手がかりの語・否定の印・機微な情報は、同梱の
/// すべての言語のものを合わせて当てる (04 §5.2)。
#[derive(Debug, Clone)]
pub struct Rules<'r> {
    pub(super) common: &'r CommonRules,
    pub(super) languages: Vec<&'r LanguageRules>,
    pub(super) b3: &'r B3Rules,
}

impl<'r> Rules<'r> {
    /// 規則を合わせる。`languages` には、同梱のすべての言語を渡す。
    #[must_use]
    pub fn new(
        common: &'r CommonRules,
        languages: Vec<&'r LanguageRules>,
        b3: &'r B3Rules,
    ) -> Self {
        Self {
            common,
            languages,
            b3,
        }
    }

    /// 言語ごとの規則と、v0.5 で足したその言語の規則の組 (`rules/b3.toml` に無い言語は除く)。
    pub(super) fn extras(&self) -> Vec<(&'r LanguageRules, &'r LanguageExtra)> {
        self.languages
            .iter()
            .filter_map(|r| self.b3.language(&r.language).map(|x| (*r, x)))
            .collect()
    }

    /// 規則の版 (関門の結果の覚えの鍵と、導出の指紋の成分、04 §5.3)。
    #[must_use]
    pub fn versions(&self) -> RuleVersions {
        RuleVersions {
            gates: GATES_VERSION,
            common: self.common.version,
            languages: self
                .languages
                .iter()
                .map(|r| (r.language.clone(), r.version))
                .collect(),
            b3: self.b3.version,
            prefilter: PREFILTER_VERSION,
            splitter: crate::sentences::SPLITTER_V1,
        }
    }
}

impl Rules<'static> {
    /// 同梱の規則。
    #[must_use]
    pub fn bundled() -> Self {
        Self::new(common_rules(), all_language_rules(), b3_rules())
    }
}

/// 関門の規則の版。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleVersions {
    /// 関門の実装の版 ([`GATES_VERSION`])。
    pub gates: u32,
    /// `rules/common.toml` の版。
    pub common: u32,
    /// 言語ごとの規則の版 (言語, 版)。
    pub languages: Vec<(String, u32)>,
    /// `rules/b3.toml` の版。
    pub b3: u32,
    /// 前のふるいの規則の版 ([`PREFILTER_VERSION`])。
    pub prefilter: u32,
    /// 文の分け方の版 (文番号の文、[`crate::SPLITTER_V1`])。
    pub splitter: u32,
}

/// 辞書の属性 1 つと、その v0.5 の宣言。
#[derive(Debug, Clone)]
pub struct DictAttribute {
    attribute: Attribute,
    layer: UnitLayer,
    negative: bool,
}

impl DictAttribute {
    /// 属性の宣言 (単値か多値か・値の長さの上限・主語の許し・手がかりの語)。
    #[must_use]
    pub fn attribute(&self) -> &Attribute {
        &self.attribute
    }

    /// 属性の名前。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.attribute.name
    }

    /// 層 (v0.5 の辞書の属性はすべて `world`)。
    #[must_use]
    pub fn layer(&self) -> UnitLayer {
        self.layer
    }

    /// 値の意味が否定を含む属性か (関門 3b を当てない)。
    #[must_use]
    pub fn negative(&self) -> bool {
        self.negative
    }
}

/// 辞書の宣言の誤り。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DictionaryError {
    /// 宣言が、辞書に無い属性の名前を指す。
    #[error("the dictionary declaration names `{0}`, which is not an attribute")]
    UnknownAttribute(String),
    /// 正規化すると空になる別名。
    #[error("an alias of `{0}` is empty after normalization")]
    EmptyAlias(String),
    /// 別名が、ほかの属性の名前か別名と (正規化 v1 で) 同じ。
    #[error("the alias `{0}` collides with an attribute name or another alias")]
    AliasCollides(String),
}

/// 辞書 (04 §5.1)。辞書の属性と、別名 (正規化 v1 の後) → 属性の表。
#[derive(Debug, Clone)]
pub struct Dictionary {
    version: u32,
    entries: Vec<DictAttribute>,
    /// 正規化 v1 の後の別名 → `entries` の番号。
    aliases: BTreeMap<String, usize>,
}

impl Dictionary {
    /// 属性の一覧 `attributes` に、v0.5 の宣言 `decl` を足す。版は `attributes.version`。
    ///
    /// # Errors
    ///
    /// 宣言が辞書に無い属性を指すとき、別名が正規化で空になるとき、別名が属性の名前かほかの別名とぶつかるとき。
    pub fn new(attributes: &Attributes, decl: &DictionaryDecl) -> Result<Self, DictionaryError> {
        for name in decl
            .negative
            .iter()
            .chain(decl.layer.keys())
            .chain(decl.aliases.keys())
        {
            if attributes.get(name).is_none() {
                return Err(DictionaryError::UnknownAttribute(name.clone()));
            }
        }
        let entries = attributes
            .list
            .iter()
            .map(|a| DictAttribute {
                attribute: a.clone(),
                layer: decl.layer.get(&a.name).copied().unwrap_or(UnitLayer::World),
                negative: decl.negative.contains(&a.name),
            })
            .collect();
        let mut out = Self {
            version: attributes.version,
            entries,
            aliases: BTreeMap::new(),
        };
        out.add_aliases(&decl.aliases)?;
        Ok(out)
    }

    /// 同梱の辞書 (`attributes.toml` と `rules/b3.toml` の `[dictionary]`)。
    ///
    /// # Panics
    ///
    /// 同梱の宣言が正しくないとき (試験 `bundled_dictionary_is_valid` で防ぐ)。
    #[must_use]
    pub fn bundled() -> &'static Self {
        static CELL: OnceLock<Dictionary> = OnceLock::new();
        CELL.get_or_init(|| {
            Self::new(attributes(), &b3_rules().dictionary)
                .unwrap_or_else(|e| panic!("the bundled dictionary is valid: {e}"))
        })
    }

    /// 版を `version` に上げ、別名 `aliases` (属性の名前 → 語) を足した辞書 (04 §5.1 の「辞書は版を持つ」。足す
    /// だけにする)。
    ///
    /// # Errors
    ///
    /// [`Dictionary::new`] と同じ。
    pub fn with_aliases(
        &self,
        version: u32,
        aliases: &BTreeMap<String, Vec<String>>,
    ) -> Result<Self, DictionaryError> {
        let mut out = self.clone();
        out.version = version;
        out.add_aliases(aliases)?;
        Ok(out)
    }

    fn add_aliases(
        &mut self,
        aliases: &BTreeMap<String, Vec<String>>,
    ) -> Result<(), DictionaryError> {
        for (name, words) in aliases {
            let i = self
                .entries
                .iter()
                .position(|e| e.name() == name)
                .ok_or_else(|| DictionaryError::UnknownAttribute(name.clone()))?;
            for w in words {
                let n = normalize_v1(w);
                if n.is_empty() {
                    return Err(DictionaryError::EmptyAlias(name.clone()));
                }
                let taken = self.entries.iter().any(|e| e.name() == n)
                    || self.aliases.get(&n).is_some_and(|&j| j != i);
                if taken {
                    return Err(DictionaryError::AliasCollides(w.clone()));
                }
                self.aliases.insert(n, i);
            }
        }
        Ok(())
    }

    /// 辞書の版 (導出の指紋と関門の結果の覚えに入る、04 §5.3)。
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// 名前で引く。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DictAttribute> {
        self.entries.iter().find(|e| e.name() == name)
    }

    /// 正規化 v1 の後の属性 `normalized` を、辞書の属性の名前か別名で引く。別名で引いたら 2 つ目が true。
    pub(super) fn resolve(&self, normalized: &str) -> Option<(&DictAttribute, bool)> {
        if let Some(e) = self.get(normalized) {
            return Some((e, false));
        }
        self.aliases
            .get(normalized)
            .map(|&i| (&self.entries[i], true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_b3_rules_are_valid() {
        assert_eq!(b3_rules().problems(attributes()), Vec::<String>::new());
    }

    #[test]
    fn bundled_dictionary_is_valid() {
        let d = Dictionary::bundled();
        assert_eq!(d.version(), attributes().version);
        assert!(d.get("dietary").is_some_and(DictAttribute::negative));
        assert!(d.get("residence").is_some_and(|a| !a.negative()));
        assert!(attributes().list.iter().all(|a| {
            d.get(&a.name)
                .is_some_and(|e| e.layer() == UnitLayer::World)
        }));
        // 初版の別名は空 (自由記述の「住まい」は辞書の属性にならない)。
        assert!(d.resolve(&normalize_v1("住まい")).is_none());
    }

    #[test]
    fn aliases_map_free_aspects_and_must_not_collide() {
        let d = Dictionary::bundled();
        let mut add = BTreeMap::new();
        add.insert("residence".to_owned(), vec!["住まい".to_owned()]);
        let d3 = d.with_aliases(3, &add).expect("valid");
        assert_eq!(d3.version(), 3);
        assert!(
            d3.resolve(&normalize_v1("住まい"))
                .is_some_and(|(a, alias)| a.name() == "residence" && alias)
        );
        assert!(
            d3.resolve("residence")
                .is_some_and(|(a, alias)| a.name() == "residence" && !alias)
        );
        let mut clash = BTreeMap::new();
        clash.insert("hometown".to_owned(), vec!["住まい".to_owned()]);
        assert_eq!(
            d3.with_aliases(4, &clash).map(|_| ()),
            Err(DictionaryError::AliasCollides("住まい".to_owned()))
        );
        let mut name = BTreeMap::new();
        name.insert("hometown".to_owned(), vec!["Residence".to_owned()]);
        assert!(d.with_aliases(4, &name).is_err());
        let mut unknown = BTreeMap::new();
        unknown.insert("nope".to_owned(), vec!["x".to_owned()]);
        assert_eq!(
            d.with_aliases(4, &unknown).map(|_| ()),
            Err(DictionaryError::UnknownAttribute("nope".to_owned()))
        );
    }

    #[test]
    fn shapes_are_parsed() {
        assert_eq!(
            shape_parts("{n}番{n}号"),
            Some(vec![
                ShapePart::Number,
                ShapePart::Text("番".into()),
                ShapePart::Number,
                ShapePart::Text("号".into()),
            ])
        );
        assert_eq!(
            shape_parts("〒{n3}-{n4}"),
            Some(vec![
                ShapePart::Text("〒".into()),
                ShapePart::Digits(3),
                ShapePart::Text("-".into()),
                ShapePart::Digits(4),
            ])
        );
        assert_eq!(
            shape_parts("{c}{n}-{n}"),
            Some(vec![
                ShapePart::Cjk,
                ShapePart::Number,
                ShapePart::Text("-".into()),
                ShapePart::Number,
            ])
        );
        assert_eq!(shape_parts("{n} {ws} street").map(|p| p.len()), Some(4));
        assert_eq!(shape_parts("番地"), None);
        assert_eq!(shape_parts("{w} street"), None);
        assert_eq!(shape_parts("{x}丁目"), None);
        assert_eq!(shape_parts("{n0}丁目"), None);
    }
}
