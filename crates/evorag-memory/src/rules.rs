//! 事実の属性の一覧と、抽出・訂正の規則 (同梱の TOML) を読む。
//!
//! 仕様: 04 §5.1 (属性の一覧)、04 §5.2 (前のふるいと関門)、04 §5.3 (置き換えの条件)、04 §6 (訂正の型)。
//! ここは読むことと形の検査だけを持ち、規則を当てる処理は持たない (抽出・統合は M3.1・M3.2 で足す)。
//!
//! 同梱のファイル:
//! - `attributes.toml`: 属性の一覧。属性は足すだけにする (名前は事実の記録に残るため)。
//! - `rules/common.toml`: 言語に依らない規則 (制御文字・表の区切り・URL・役割の詐称)。
//! - `rules/<言語>.toml`: 言語ごとの規則。`[safety]` と `[prefilter].greetings` は全言語を合わせて当て、
//!   ほかは根拠の言語で選んで当てる。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

/// 値の長さの上限の最大 (04 §5.1 の既定 200 文字)。
pub const MAX_VALUE_CHARS: u16 = 200;

/// 同梱する規則の言語 (プロンプトの言語と同じ、07 §6.1)。
pub const RULE_LANGUAGES: &[&str] = &["ja", "en"];

/// Unicode の一般カテゴリの名前 (2 文字)。`forbidden_categories` の検査に使う。
const GENERAL_CATEGORIES: &[&str] = &[
    "Lu", "Ll", "Lt", "Lm", "Lo", "Mn", "Mc", "Me", "Nd", "Nl", "No", "Pc", "Pd", "Ps", "Pe", "Pi",
    "Pf", "Po", "Sm", "Sc", "Sk", "So", "Zs", "Zl", "Zp", "Cc", "Cf", "Cs", "Co", "Cn",
];

/// 規則の語を照合の前にそろえる形 (NFKC → 小文字)。規則の語はこの形で変わらないように書く。
fn fold(s: &str) -> String {
    s.nfkc().flat_map(char::to_lowercase).collect()
}

/// 単値か多値か (04 §5.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    /// 今の値は 1 つ。新しい値は置き換えの条件を満たしたときだけ古い値を置き換える。
    Single,
    /// 並べて持つ。古い値が特定できたときだけ、その 1 件を置き換える。
    Multi,
}

/// 属性 1 つ (04 §5.1)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attribute {
    /// 属性の名前 (英小文字・数字・`_`)。記録とプロンプトの両方で使う。
    pub name: String,
    pub cardinality: Cardinality,
    /// 値の長さの上限 (文字数)。
    pub max_chars: u16,
    /// 事実の表に入れる優先の順。小さいほど先 (04 §5.4)。
    pub priority: u32,
    /// 利用者本人 (`self`) 以外の主語にも使えるか。
    pub other_subjects: bool,
    /// 抽出のプロンプトに書く説明 (プロンプトの言語 → 文)。
    pub describe: BTreeMap<String, String>,
}

/// 属性の一覧。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attributes {
    pub version: u32,
    #[serde(rename = "attribute")]
    pub list: Vec<Attribute>,
}

impl Attributes {
    /// 名前で引く。一覧に無い属性の候補は捨てる (04 §5.1)。
    pub fn get(&self, name: &str) -> Option<&Attribute> {
        self.list.iter().find(|a| a.name == name)
    }

    /// 一覧の検査。問題の一覧を返す (空なら問題なし)。
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.version == 0 {
            out.push("attributes: version must be 1 or more".into());
        }
        if self.list.is_empty() {
            out.push("attributes: the list is empty".into());
        }
        let mut names = BTreeSet::new();
        let mut priorities = BTreeSet::new();
        for a in &self.list {
            let valid_name = a
                .name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && a.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !valid_name {
                out.push(format!(
                    "attribute `{}`: name must be [a-z][a-z0-9_]*",
                    a.name
                ));
            }
            if !names.insert(a.name.as_str()) {
                out.push(format!("attribute `{}`: duplicate name", a.name));
            }
            if !priorities.insert(a.priority) {
                out.push(format!(
                    "attribute `{}`: duplicate priority {}",
                    a.name, a.priority
                ));
            }
            if a.max_chars == 0 || a.max_chars > MAX_VALUE_CHARS {
                out.push(format!(
                    "attribute `{}`: max_chars must be 1..={MAX_VALUE_CHARS}",
                    a.name
                ));
            }
            for lang in RULE_LANGUAGES {
                if a.describe.get(*lang).is_none_or(|d| d.trim().is_empty()) {
                    out.push(format!(
                        "attribute `{}`: describe.{lang} is missing",
                        a.name
                    ));
                }
            }
        }
        out
    }
}

/// 言語に依らない規則 (`rules/common.toml`)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonRules {
    pub version: u32,
    /// 値に含んだら捨てる、Unicode の一般カテゴリ (`Cc`・`Cf` など)。
    pub forbidden_categories: Vec<String>,
    /// 値に含んだら捨てる、そのほかの文字 (1 文字ずつ)。
    pub forbidden_chars: Vec<String>,
    /// 値に含んだら捨てる、URL とスキームの印。
    pub url_markers: Vec<String>,
    /// 値に含んだら捨てる、役割の詐称と区切りの模倣。
    pub role_markers: Vec<String>,
}

impl CommonRules {
    /// 規則の検査。問題の一覧を返す (空なら問題なし)。
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let at = |what: &str| format!("rules/common.toml: {what}");
        if self.version == 0 {
            out.push(at("version must be 1 or more"));
        }
        for c in &self.forbidden_categories {
            if !GENERAL_CATEGORIES.contains(&c.as_str()) {
                out.push(at(&format!(
                    "forbidden_categories `{c}` is not a general category"
                )));
            }
        }
        for c in &self.forbidden_chars {
            if c.chars().count() != 1 {
                out.push(at(&format!(
                    "forbidden_chars `{c:?}` must be one character"
                )));
            }
        }
        for (name, list) in [
            ("forbidden_chars", &self.forbidden_chars),
            ("url_markers", &self.url_markers),
            ("role_markers", &self.role_markers),
        ] {
            check_entries(&mut out, &at, name, list);
        }
        out
    }
}

/// 規則の語の一覧の共通の検査: 空でない、重複しない、NFKC と小文字化で変わらない。
fn check_entries(out: &mut Vec<String>, at: &dyn Fn(&str) -> String, name: &str, list: &[String]) {
    let mut seen = BTreeSet::new();
    for w in list {
        // 空白だけの語は空とみなす。行・段落の区切り (U+2028・U+2029) は 1 文字の正しい語なので除く。
        if w.is_empty() || w.chars().all(|c| c == ' ') {
            out.push(at(&format!("{name} has an empty entry")));
        }
        if !seen.insert(w.as_str()) {
            out.push(at(&format!("{name} has a duplicate `{w}`")));
        }
        if fold(w) != *w {
            out.push(at(&format!(
                "{name} entry `{w}` changes under NFKC and lowercasing (it would never match)"
            )));
        }
    }
}

/// 前のふるい (04 §5.2)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prefilter {
    /// 内容の単位 (05 §2.1) がこれ未満の発話は送らない (日本語)。
    #[serde(default)]
    pub min_content_units: Option<u32>,
    /// 語がこれ未満の発話は送らない (英語)。
    #[serde(default)]
    pub min_words: Option<u32>,
    /// 発話の全体がこれと一致したら送らない (安全の規則。全言語を合わせて当てる)。
    pub greetings: Vec<String>,
}

/// 引用の括弧 1 組 (04 §5.2 の関門 4)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub open: String,
    pub close: String,
    /// 値がこの括弧の内側にあっても許す属性。
    pub allow_attributes: Vec<String>,
}

/// 安全の規則 (04 §5.2 の関門 4・6)。全言語を合わせて当てる。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyRules {
    /// 命令形の文と依頼の印。
    pub imperative: Vec<String>,
    /// `imperative` に当たっても命令とみなさない語 (照合の前に伏せる)。
    pub imperative_exclude: Vec<String>,
    /// ほかの指示への言及。
    pub instruction_refs: Vec<String>,
    /// 引用の括弧。値がこの内側なら捨てる (属性の例外を除く)。
    pub quote: Vec<Quote>,
}

/// 読み取りの規則 (04 §5.3)。根拠の言語で選んで当てる。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadingRules {
    /// 主語が利用者本人だと明示する語。
    pub subject_self: Vec<String>,
    /// `subject_self` に当たっても主語の語とみなさない語 (照合の前に伏せる)。
    pub subject_self_exclude: Vec<String>,
    /// 変化を示す語。
    pub change: Vec<String>,
    /// 否定の候補で、今の値を終わらせてよい語。
    pub negation_change: Vec<String>,
}

/// 訂正の規則 (04 §6)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionRules {
    /// 候補を探す一次のふるい。
    pub triggers: Vec<String>,
    /// 訂正の型。`{new}` を 1 つ必ず含み、`{old}` (訂正される側) を 1 つまで含む。
    pub patterns: Vec<String>,
}

impl CorrectionRules {
    /// 型を、固定の部分の長い順に並べる (同じ長さは書いた順)。
    /// 「ではなく」が「ではなくて」より先に当たり、新しい値が「て…」になるのを防ぐため (04 §6)。
    pub fn patterns_longest_first(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.patterns.iter().map(String::as_str).collect();
        v.sort_by_key(|p| std::cmp::Reverse(fixed_len(p)));
        v
    }
}

fn fixed_len(pattern: &str) -> usize {
    pattern
        .replace("{new}", "")
        .replace("{old}", "")
        .chars()
        .count()
}

/// 言語ごとの規則 (`rules/<言語>.toml`)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageRules {
    pub version: u32,
    pub language: String,
    pub prefilter: Prefilter,
    pub safety: SafetyRules,
    pub reading: ReadingRules,
    pub correction: CorrectionRules,
}

impl LanguageRules {
    /// 規則の検査。`attributes` は、括弧の例外の属性が一覧にあるかを確かめるのに使う。
    pub fn problems(&self, file_language: &str, attributes: &Attributes) -> Vec<String> {
        let mut out = Vec::new();
        let at = |what: &str| format!("rules/{file_language}.toml: {what}");
        if self.version == 0 {
            out.push(at("version must be 1 or more"));
        }
        if self.language != file_language {
            out.push(at(&format!("language is `{}`", self.language)));
        }
        match file_language {
            "ja" if self.prefilter.min_content_units.is_none() => {
                out.push(at("prefilter.min_content_units is missing"));
            }
            "en" if self.prefilter.min_words.is_none() => {
                out.push(at("prefilter.min_words is missing"));
            }
            _ => {}
        }
        for (name, list) in [
            ("prefilter.greetings", &self.prefilter.greetings),
            ("safety.imperative", &self.safety.imperative),
            ("safety.imperative_exclude", &self.safety.imperative_exclude),
            ("safety.instruction_refs", &self.safety.instruction_refs),
            ("reading.subject_self", &self.reading.subject_self),
            (
                "reading.subject_self_exclude",
                &self.reading.subject_self_exclude,
            ),
            ("reading.change", &self.reading.change),
            ("reading.negation_change", &self.reading.negation_change),
            ("correction.triggers", &self.correction.triggers),
            ("correction.patterns", &self.correction.patterns),
        ] {
            check_entries(&mut out, &at, name, list);
        }
        for q in &self.safety.quote {
            if q.open.is_empty() || q.close.is_empty() {
                out.push(at("safety.quote has an empty bracket"));
            }
            if fold(&q.open) != q.open || fold(&q.close) != q.close {
                out.push(at(&format!(
                    "safety.quote `{}{}` changes under NFKC",
                    q.open, q.close
                )));
            }
            for a in &q.allow_attributes {
                if attributes.get(a).is_none() {
                    out.push(at(&format!(
                        "safety.quote `{}{}` allows `{a}`, which is not an attribute",
                        q.open, q.close
                    )));
                }
            }
        }
        for p in &self.correction.patterns {
            let new = p.matches("{new}").count();
            let old = p.matches("{old}").count();
            if new != 1 || old > 1 || fixed_len(p) == 0 {
                out.push(at(&format!(
                    "correction.patterns `{p}` needs one {{new}}, at most one {{old}} and fixed text"
                )));
            }
            if !self
                .correction
                .triggers
                .iter()
                .any(|t| p.contains(t.as_str()))
            {
                out.push(at(&format!(
                    "correction.patterns `{p}` contains no trigger (it would never be tried)"
                )));
            }
        }
        out
    }
}

fn parse<T: for<'de> Deserialize<'de>>(file: &str, text: &str) -> T {
    toml::from_str(text).unwrap_or_else(|e| panic!("the bundled {file} parses: {e}"))
}

/// 同梱の属性の一覧。
pub fn attributes() -> &'static Attributes {
    static CELL: OnceLock<Attributes> = OnceLock::new();
    CELL.get_or_init(|| parse("attributes.toml", include_str!("../attributes.toml")))
}

/// 同梱の、言語に依らない規則。
pub fn common_rules() -> &'static CommonRules {
    static CELL: OnceLock<CommonRules> = OnceLock::new();
    CELL.get_or_init(|| parse("rules/common.toml", include_str!("../rules/common.toml")))
}

/// 同梱の、言語ごとの規則 (`ja`・`en`。ほかは無い)。
pub fn language_rules(lang: &str) -> Option<&'static LanguageRules> {
    static JA: OnceLock<LanguageRules> = OnceLock::new();
    static EN: OnceLock<LanguageRules> = OnceLock::new();
    match lang {
        "ja" => Some(JA.get_or_init(|| parse("rules/ja.toml", include_str!("../rules/ja.toml")))),
        "en" => Some(EN.get_or_init(|| parse("rules/en.toml", include_str!("../rules/en.toml")))),
        _ => None,
    }
}

/// 同梱のすべての言語の規則 (安全の規則を合わせて当てるときに使う、04 §5.2)。
pub fn all_language_rules() -> Vec<&'static LanguageRules> {
    RULE_LANGUAGES
        .iter()
        .filter_map(|l| language_rules(l))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_attributes_are_valid() {
        let a = attributes();
        assert_eq!(a.problems(), Vec::<String>::new());
        assert!(a.get("residence").is_some());
        assert!(a.get("unknown").is_none());
    }

    #[test]
    fn bundled_common_rules_are_valid() {
        let c = common_rules();
        assert_eq!(c.problems(), Vec::<String>::new());
        assert!(c.forbidden_categories.iter().any(|s| s == "Cc"));
        assert!(c.forbidden_categories.iter().any(|s| s == "Cf"));
        assert!(c.forbidden_chars.iter().any(|s| s == "|"));
        assert!(c.url_markers.iter().any(|s| s == "://"));
    }

    #[test]
    fn bundled_language_rules_are_valid() {
        for lang in RULE_LANGUAGES {
            let r = language_rules(lang).expect("bundled");
            assert_eq!(r.problems(lang, attributes()), Vec::<String>::new());
        }
        assert!(language_rules("fr").is_none());
        assert_eq!(all_language_rules().len(), RULE_LANGUAGES.len());
    }

    #[test]
    fn longer_patterns_are_tried_first() {
        let ja = language_rules("ja").expect("bundled");
        let order = ja.correction.patterns_longest_first();
        let pos = |p: &str| order.iter().position(|x| *x == p).expect("present");
        // 「東京ではなくて大阪」で、新しい値が「て大阪」にならない (04 §6)。
        assert!(pos("{old}ではなくて{new}") < pos("{old}ではなく{new}"));
    }

    #[test]
    fn problems_are_reported() {
        let bad: Attributes = toml::from_str(
            r#"
            version = 1
            [[attribute]]
            name = "Name"
            cardinality = "single"
            max_chars = 0
            priority = 1
            other_subjects = true
            describe.ja = "x"
            [[attribute]]
            name = "Name"
            cardinality = "multi"
            max_chars = 300
            priority = 1
            other_subjects = false
            describe.ja = "y"
            "#,
        )
        .expect("parses");
        let p = bad.problems();
        assert!(p.iter().any(|s| s.contains("name must be")));
        assert!(p.iter().any(|s| s.contains("duplicate name")));
        assert!(p.iter().any(|s| s.contains("duplicate priority")));
        assert!(p.iter().any(|s| s.contains("max_chars")));
        assert!(p.iter().any(|s| s.contains("describe.en")));

        let common = CommonRules {
            version: 1,
            forbidden_categories: vec!["Xx".into()],
            forbidden_chars: vec!["ab".into()],
            url_markers: vec!["HTTP:".into()],
            role_markers: vec!["ｓｙｓｔｅｍ:".into()],
        };
        let p = common.problems();
        assert!(p.iter().any(|s| s.contains("not a general category")));
        assert!(p.iter().any(|s| s.contains("one character")));
        assert!(p.iter().filter(|s| s.contains("never match")).count() >= 2);
    }

    #[test]
    fn a_pattern_without_a_trigger_is_reported() {
        let mut r = language_rules("en").expect("bundled").clone();
        r.correction.patterns.push("{new} versus {old}".into());
        let p = r.problems("en", attributes());
        assert!(p.iter().any(|s| s.contains("contains no trigger")));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let r: Result<Attributes, _> = toml::from_str(
            r#"
            version = 1
            [[attribute]]
            name = "x"
            cardinality = "single"
            max_chars = 1
            priority = 1
            other_subjects = true
            describe.ja = "x"
            describe.en = "x"
            typo = 1
            "#,
        );
        assert!(r.is_err());
    }
}
