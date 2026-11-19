//! `cargo xtask i18n`: 文言カタログとコードの文言キーの差分の検査 (07 §6、NFR-602、AC-FR-908-1)。
//!
//! コードの文言キーは `evorag_types::MessageKey::ALL`、カタログは `locales/<言語>.toml`。
//! カタログの表は入れ子にしてよく、点でつないだ名前を文言キーとして扱う。値は文字列に限る。
//!
//! カタログの `[attr]` の表は文言キーではなく、属性の表示名 (07 §3 の `/memory`)。属性の一覧
//! (`crates/evorag-memory/attributes.toml`) の名前とちょうど同じ組を持つことを検査する。
//!
//! 言語の間では、文言キーごとの引数 (`{名前}`) の組が `ja` と同じことを検査する。`ja` 以外のカタログの
//! 文言には、漢字・仮名・全角の文字を置かない (英語の画面に日本語が混じらない)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use evorag_types::{MessageKey, has_cjk};

/// 同梱する言語 (07 §6。`en` は v0.3 から)。
const LOCALES: &[&str] = &["ja", "en"];

/// 基準の言語 (ほかの言語の引数の組を比べる相手。日本語の文字を許すのはこの言語だけ)。
const BASE: &str = "ja";

/// 属性の表示名の表 (evorag-tui の `catalog::ATTR_TABLE` と同じ)。
const ATTR_TABLE: &str = "attr";

/// 属性の一覧の名前。
fn attribute_names(text: &str) -> Result<BTreeSet<String>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(table
        .get("attribute")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("name").and_then(toml::Value::as_str))
        .map(str::to_owned)
        .collect())
}

pub fn run(root: &Path) -> Result<Vec<String>, String> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
    };
    let attr_path = root
        .join("crates")
        .join("evorag-memory")
        .join("attributes.toml");
    let attributes = attribute_names(&read(&attr_path)?)
        .map_err(|e| format!("failed to parse {}: {e}", attr_path.display()))?;
    // 読めないカタログがあっても、読めたカタログの検査は続ける (読めないことも誤りにする)。
    let mut read_errors = Vec::new();
    let mut catalogs = Vec::new();
    for lang in LOCALES {
        let path = root.join("locales").join(format!("{lang}.toml"));
        match read(&path) {
            Ok(text) => catalogs.push((*lang, text)),
            Err(e) => read_errors.push(format!("{lang}: {e}")),
        }
    }
    let catalogs: Vec<(&str, &str)> = catalogs.iter().map(|(l, t)| (*l, t.as_str())).collect();
    let code: Vec<&str> = MessageKey::ALL.iter().map(|k| k.as_str()).collect();
    let report = check(&code, &attributes, &catalogs);
    for (lang, n) in &report.counts {
        println!("{lang}: {n} keys in catalog, {} in code", code.len());
    }
    read_errors.extend(report.errors);
    Ok(read_errors)
}

/// 検査の結果。
#[derive(Debug, Default)]
pub struct Report {
    /// 言語ごとのカタログの文言キーの数 (属性の表示名を除く)。
    pub counts: Vec<(String, usize)>,
    pub errors: Vec<String>,
}

/// コードの文言キー (`code`)・属性の一覧 (`attributes`) と、言語ごとのカタログ (`(言語, 本文)`) を突き合わせる。
/// 基準の言語 (`BASE`) のカタログが無い・読めなくても、ほかの言語の検査 (キー・属性・日本語の文字) は行う。
pub fn check(code: &[&str], attributes: &BTreeSet<String>, catalogs: &[(&str, &str)]) -> Report {
    let mut report = Report::default();
    let errors = &mut report.errors;
    let code_set: BTreeSet<&str> = code.iter().copied().collect();
    if code_set.len() != code.len() {
        errors.push("MessageKey has duplicate key strings".to_owned());
    }
    let prefix = format!("{ATTR_TABLE}.");
    let mut parsed: Vec<(&str, BTreeMap<String, String>)> = Vec::new();
    for (lang, text) in catalogs {
        let table: toml::Table = match toml::from_str(text) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{lang}: failed to parse: {e}"));
                continue;
            }
        };
        let mut catalog = BTreeMap::new();
        flatten(&table, "", &mut catalog, errors, lang);
        let labels: BTreeSet<String> = catalog
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        for a in attributes.difference(&labels) {
            errors.push(format!(
                "{lang}: missing label `{ATTR_TABLE}.{a}` (defined in attributes.toml)"
            ));
        }
        for a in labels.difference(attributes) {
            errors.push(format!(
                "{lang}: unknown label `{ATTR_TABLE}.{a}` (not in attributes.toml)"
            ));
        }
        for k in code_set.iter().filter(|k| !catalog.contains_key(**k)) {
            errors.push(format!("{lang}: missing key `{k}` (defined in MessageKey)"));
        }
        for k in catalog
            .keys()
            .filter(|k| !k.starts_with(&prefix) && !code_set.contains(k.as_str()))
        {
            errors.push(format!("{lang}: unknown key `{k}` (not in MessageKey)"));
        }
        if *lang != BASE {
            for (k, v) in &catalog {
                if has_cjk(v) {
                    errors.push(format!("{lang}: `{k}` contains Japanese text"));
                }
            }
        }
        let keys = catalog.keys().filter(|k| !k.starts_with(&prefix)).count();
        report.counts.push(((*lang).to_owned(), keys));
        parsed.push((lang, catalog));
    }
    // 引数の組を基準の言語と比べる。
    if let Some((_, base)) = parsed.iter().find(|(l, _)| *l == BASE) {
        for (lang, catalog) in parsed.iter().filter(|(l, _)| *l != BASE) {
            for (k, v) in catalog {
                let Some(b) = base.get(k) else {
                    continue;
                };
                let (mine, theirs) = (placeholders(v), placeholders(b));
                if mine != theirs {
                    report.errors.push(format!(
                        "{lang}: `{k}` has the arguments {mine:?}, but {BASE} has {theirs:?}"
                    ));
                }
            }
        }
    }
    report
}

/// 文言の引数の名前 (`{名前}`。名前は英小文字・数字・`_`)。
fn placeholders(text: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end)
                if end > 0
                    && after[..end]
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
            {
                out.insert(&after[..end]);
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    out
}

fn flatten(
    table: &toml::Table,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
    errors: &mut Vec<String>,
    lang: &str,
) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::String(s) => {
                out.insert(key, s.clone());
            }
            toml::Value::Table(t) => flatten(t, &key, out, errors, lang),
            _ => errors.push(format!("{lang}: `{key}` must be a string or a table")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JA: &str = include_str!("../../locales/ja.toml");
    const EN: &str = include_str!("../../locales/en.toml");
    const ATTRIBUTES: &str = include_str!("../../crates/evorag-memory/attributes.toml");

    fn code() -> Vec<&'static str> {
        MessageKey::ALL.iter().map(|k| k.as_str()).collect()
    }

    /// 同梱の `ja` と `en` は、コードの文言キー・属性の一覧とちょうど合い、キーごとの引数も同じ。
    ///
    /// verifies: AC-FR-908-1 (キーの差分がゼロ。NFR-602 も同じ検査)
    #[test]
    fn the_bundled_catalogs_match_the_code_and_each_other() {
        let attributes = attribute_names(ATTRIBUTES).unwrap();
        let r = check(&code(), &attributes, &[("ja", JA), ("en", EN)]);
        assert!(r.errors.is_empty(), "{:#?}", r.errors);
        let n = code().len();
        assert_eq!(
            r.counts,
            [("ja".to_owned(), n), ("en".to_owned(), n)],
            "every language has exactly the keys of MessageKey"
        );
    }

    #[test]
    fn missing_and_unknown_keys_labels_and_arguments_are_reported() {
        let attributes: BTreeSet<String> = ["age".to_owned(), "pet".to_owned()].into();
        let ja = "[a]\nb = \"{x} と {y}\"\nc = \"ok\"\n[attr]\nage = \"年齢\"\npet = \"ペット\"\n";
        let en =
            "[a]\nb = \"{x} and {z}\"\nd = \"extra\"\n[attr]\nage = \"Age\"\nhobby = \"Hobbies\"\n";
        let r = check(&["a.b", "a.c"], &attributes, &[("ja", ja), ("en", en)]);
        let want = [
            "en: missing label `attr.pet` (defined in attributes.toml)",
            "en: unknown label `attr.hobby` (not in attributes.toml)",
            "en: missing key `a.c` (defined in MessageKey)",
            "en: unknown key `a.d` (not in MessageKey)",
            "en: `a.b` has the arguments {\"x\", \"z\"}, but ja has {\"x\", \"y\"}",
        ];
        assert_eq!(r.errors, want);
    }

    #[test]
    fn japanese_text_in_another_language_and_non_strings_are_reported() {
        let none = BTreeSet::new();
        let r = check(
            &["k"],
            &none,
            &[
                ("ja", "k = \"日本語\"\n"),
                ("en", "k = \"English、\"\nn = 1\n"),
            ],
        );
        assert_eq!(
            r.errors,
            [
                "en: `n` must be a string or a table",
                "en: `k` contains Japanese text",
            ]
        );
    }

    /// 基準の言語 (ja) のカタログが読めない・壊れていても、en の検査は飛ばさない。
    #[test]
    fn other_languages_are_checked_without_the_base_catalog() {
        let none = BTreeSet::new();
        let want = [
            "en: missing key `b` (defined in MessageKey)",
            "en: `a` contains Japanese text",
        ];
        let r = check(&["a", "b"], &none, &[("en", "a = \"日本語\"\n")]);
        assert_eq!(r.errors, want);
        let r = check(
            &["a", "b"],
            &none,
            &[("ja", "a = "), ("en", "a = \"日本語\"\n")],
        );
        assert_eq!(r.errors.len(), 3, "{:#?}", r.errors);
        assert!(
            r.errors[0].starts_with("ja: failed to parse"),
            "{:#?}",
            r.errors
        );
        assert_eq!(r.errors[1..], want);
    }

    #[test]
    fn placeholders_are_lowercase_names_in_braces() {
        assert_eq!(
            placeholders("{a} {b_1} {Not} {} { x } {a}"),
            BTreeSet::from(["a", "b_1"])
        );
        assert!(placeholders("no braces").is_empty());
    }
}
