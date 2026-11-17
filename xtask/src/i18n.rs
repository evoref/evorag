//! `cargo xtask i18n`: 文言カタログとコードの文言キーの差分の検査 (07 §6、NFR-602)。
//!
//! コードの文言キーは `evorag_types::MessageKey::ALL`、カタログは `locales/<言語>.toml`。
//! カタログの表は入れ子にしてよく、点でつないだ名前を文言キーとして扱う。値は文字列に限る。
//!
//! カタログの `[attr]` の表は文言キーではなく、属性の表示名 (07 §3 の `/memory`)。属性の一覧
//! (`crates/evorag-memory/attributes.toml`) の名前とちょうど同じ組を持つことを検査する。

use std::collections::BTreeSet;
use std::path::Path;

use evorag_types::MessageKey;

/// 同梱する言語 (v0.1 は ja だけ、07 §6)。
const LOCALES: &[&str] = &["ja"];

/// 属性の表示名の表 (evorag-tui の `catalog::ATTR_TABLE` と同じ)。
const ATTR_TABLE: &str = "attr";

/// 属性の一覧の名前。
fn attribute_names(root: &Path) -> Result<BTreeSet<String>, String> {
    let path = root
        .join("crates")
        .join("evorag-memory")
        .join("attributes.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
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
    let code: BTreeSet<&str> = MessageKey::ALL.iter().map(|k| k.as_str()).collect();
    let mut errors = Vec::new();
    let attributes = attribute_names(root)?;
    if code.len() != MessageKey::ALL.len() {
        errors.push("MessageKey has duplicate key strings".to_owned());
    }
    for lang in LOCALES {
        let path = root.join("locales").join(format!("{lang}.toml"));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        let mut catalog = BTreeSet::new();
        flatten(&table, "", &mut catalog, &mut errors, lang);
        let prefix = format!("{ATTR_TABLE}.");
        let labels: BTreeSet<String> = catalog
            .iter()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        catalog.retain(|k| !k.starts_with(&prefix));
        for a in attributes.difference(&labels) {
            errors.push(format!(
                "{lang}: missing label `{ATTR_TABLE}.{a}` (defined in attributes.toml)"
            ));
        }
        for a in labels.difference(&attributes) {
            errors.push(format!(
                "{lang}: unknown label `{ATTR_TABLE}.{a}` (not in attributes.toml)"
            ));
        }
        for k in code.iter().filter(|k| !catalog.contains(**k)) {
            errors.push(format!("{lang}: missing key `{k}` (defined in MessageKey)"));
        }
        for k in catalog.iter().filter(|k| !code.contains(k.as_str())) {
            errors.push(format!("{lang}: unknown key `{k}` (not in MessageKey)"));
        }
        println!(
            "{lang}: {} keys in catalog, {} in code",
            catalog.len(),
            code.len()
        );
    }
    Ok(errors)
}

fn flatten(
    table: &toml::Table,
    prefix: &str,
    out: &mut BTreeSet<String>,
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
            toml::Value::String(_) => {
                out.insert(key);
            }
            toml::Value::Table(t) => flatten(t, &key, out, errors, lang),
            _ => errors.push(format!("{lang}: `{key}` must be a string or a table")),
        }
    }
}
