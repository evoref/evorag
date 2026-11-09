//! `cargo xtask check-deps`: 依存の規則 (02 §4.3) と推移的な依存の数 (NFR-203) の検査。
//!
//! 規則の入力は `deps-policy.toml`。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::metadata::{self, Metadata, Tree};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub targets: Vec<String>,
    pub transitive: Transitive,
    pub c_code: CCode,
    #[serde(default)]
    pub direct_only_in: Vec<OnlyIn>,
    #[serde(default)]
    pub tree_only_in: Vec<OnlyIn>,
    #[serde(default)]
    pub direct_allow: Vec<DirectAllow>,
    pub layers: Layers,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transitive {
    pub max: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CCode {
    pub forbidden: Vec<String>,
    pub forbidden_build: Vec<String>,
    #[serde(default)]
    pub links_allowed: Vec<LinksAllowed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinksAllowed {
    pub name: String,
    /// 人が読むための理由 (空にしない)。
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnlyIn {
    pub rule: String,
    pub crates: Vec<String>,
    pub only_in: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectAllow {
    pub rule: String,
    #[serde(rename = "crate")]
    pub krate: String,
    pub workspace: Option<Vec<String>>,
    pub external: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layers {
    pub order: Vec<Vec<String>>,
}

/// 層の外に置いてよい workspace の crate (開発用の道具)。
const OUTSIDE_LAYERS: &[&str] = &["xtask"];
/// 推移的な依存を数える起点 (配布物)。
const DIST_BIN: &str = "evorag";

pub fn load_policy(path: &Path) -> Result<Policy, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// 検査を流し、違反の一覧を返す。`manifest_path` が `Some` のときは、別の workspace に対して
/// C を含む依存の検査と数の計測だけを行う (上限を決めるための計測用。workspace の規則は当てない)。
pub fn run(policy: &Policy, manifest_path: Option<&Path>) -> Result<Vec<String>, String> {
    let mut errors = Vec::new();
    let meta = metadata::load(manifest_path)?;
    let members = meta.member_names();

    for l in &policy.c_code.links_allowed {
        if l.reason.trim().is_empty() {
            errors.push(format!("c_code.links_allowed `{}` has no reason", l.name));
        }
    }

    let root = if manifest_path.is_none() {
        if !members.contains(DIST_BIN) {
            return Err(format!("workspace has no package `{DIST_BIN}`"));
        }
        DIST_BIN
    } else {
        // 計測用の workspace では、最初の member を起点にする。
        members
            .iter()
            .next()
            .copied()
            .ok_or("no workspace members")?
    };

    for target in &policy.targets {
        // dev 依存も含めた workspace の全体: C を含む依存。
        let all = Tree {
            manifest_path,
            package: None,
            target,
            edges: "normal,build,dev",
            stop_at: Vec::new(),
        }
        .packages()?;
        check_c_code(policy, &meta, &all, target, &mut errors);

        // 配布物の推移的な依存の数。
        let dist = Tree {
            manifest_path,
            package: Some(root),
            target,
            edges: "normal,build",
            stop_at: Vec::new(),
        }
        .packages()?;
        let count = dist
            .iter()
            .filter(|p| !members.contains(p.name.as_str()))
            .count();
        println!(
            "transitive dependencies of `{root}` on {target}: {count} (max {})",
            policy.transitive.max
        );
        if count > policy.transitive.max {
            errors.push(format!(
                "NFR-203: `{root}` has {count} transitive dependencies on {target}, over the limit {} \
                 (raise it in deps-policy.toml with a reason, docs/11-dev-workflow.md §9.5)",
                policy.transitive.max
            ));
        }

        if manifest_path.is_none() {
            check_tree_only_in(policy, &members, target, &mut errors)?;
        }
    }

    if manifest_path.is_none() {
        check_direct_only_in(policy, &meta, &mut errors);
        check_direct_allow(policy, &meta, &mut errors);
        check_layers(policy, &meta, &mut errors);
    }

    errors.sort();
    errors.dedup();
    Ok(errors)
}

fn check_c_code(
    policy: &Policy,
    meta: &Metadata,
    used: &BTreeSet<metadata::Pkg>,
    target: &str,
    errors: &mut Vec<String>,
) {
    let c = &policy.c_code;
    for p in used {
        if c.forbidden.contains(&p.name) {
            errors.push(format!(
                "R5: `{} {}` is in the dependency tree on {target} (C code, ADR-0015)",
                p.name, p.version
            ));
        }
        if c.forbidden_build.contains(&p.name) {
            errors.push(format!(
                "R5: `{} {}` (a C build tool) is in the dependency tree on {target} (ADR-0015)",
                p.name, p.version
            ));
        }
        if let Some(links) = meta.package(p).and_then(|m| m.links.as_ref())
            && !c.links_allowed.iter().any(|l| l.name == p.name)
        {
            errors.push(format!(
                "R5: `{} {}` declares `links = \"{links}\"` on {target} (links a native library; \
                 add it to c_code.links_allowed only if it builds no C)",
                p.name, p.version
            ));
        }
    }
}

fn check_tree_only_in(
    policy: &Policy,
    members: &BTreeSet<&str>,
    target: &str,
    errors: &mut Vec<String>,
) -> Result<(), String> {
    for member in members {
        let rules: Vec<&OnlyIn> = policy
            .tree_only_in
            .iter()
            .filter(|r| !r.only_in.iter().any(|m| m == member))
            .collect();
        if rules.is_empty() {
            continue;
        }
        // ほかの workspace の crate の先はたどらない (その crate は自分で検査される)。
        let tree = Tree {
            manifest_path: None,
            package: Some(member),
            target,
            edges: "normal,build",
            stop_at: members.iter().copied().filter(|m| m != member).collect(),
        }
        .packages()?;
        for rule in rules {
            for p in tree.iter().filter(|p| rule.crates.contains(&p.name)) {
                errors.push(format!(
                    "{}: `{}` is in the dependency tree of `{member}` on {target}; only {} may have it",
                    rule.rule,
                    p.name,
                    rule.only_in.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// workspace の crate ごとの直接の依存 (宣言のとおり、dev も含む) の名前。
fn direct_deps(meta: &Metadata) -> BTreeMap<&str, Vec<&str>> {
    meta.members()
        .into_iter()
        .map(|p| {
            (
                p.name.as_str(),
                p.dependencies.iter().map(|d| d.name.as_str()).collect(),
            )
        })
        .collect()
}

fn check_direct_only_in(policy: &Policy, meta: &Metadata, errors: &mut Vec<String>) {
    for (member, deps) in direct_deps(meta) {
        for rule in &policy.direct_only_in {
            if rule.only_in.iter().any(|m| m == member) {
                continue;
            }
            for dep in deps.iter().filter(|d| rule.crates.iter().any(|c| c == *d)) {
                errors.push(format!(
                    "{}: `{member}` depends on `{dep}` directly; only {} may",
                    rule.rule,
                    rule.only_in.join(", ")
                ));
            }
        }
    }
}

fn check_direct_allow(policy: &Policy, meta: &Metadata, errors: &mut Vec<String>) {
    let direct = direct_deps(meta);
    let members = meta.member_names();
    for rule in &policy.direct_allow {
        let Some(deps) = direct.get(rule.krate.as_str()) else {
            errors.push(format!(
                "{}: deps-policy.toml names `{}`, which is not in the workspace",
                rule.rule, rule.krate
            ));
            continue;
        };
        for dep in deps {
            let allowed = if members.contains(dep) {
                &rule.workspace
            } else {
                &rule.external
            };
            if let Some(list) = allowed
                && !list.iter().any(|a| a == dep)
            {
                let list = if list.is_empty() {
                    "none".to_owned()
                } else {
                    list.join(", ")
                };
                errors.push(format!(
                    "{}: `{}` may not depend on `{dep}` (allowed: {list})",
                    rule.rule, rule.krate
                ));
            }
        }
    }
}

fn check_layers(policy: &Policy, meta: &Metadata, errors: &mut Vec<String>) {
    let layer_of: BTreeMap<&str, usize> = policy
        .layers
        .order
        .iter()
        .enumerate()
        .flat_map(|(i, names)| names.iter().map(move |n| (n.as_str(), i)))
        .collect();
    let members = meta.member_names();
    for (member, deps) in direct_deps(meta) {
        if OUTSIDE_LAYERS.contains(&member) {
            continue;
        }
        let Some(&own) = layer_of.get(member) else {
            errors.push(format!(
                "layers: `{member}` is not in [layers] of deps-policy.toml (02 §4.2)"
            ));
            continue;
        };
        for dep in deps.into_iter().filter(|d| members.contains(d)) {
            if OUTSIDE_LAYERS.contains(&dep) {
                errors.push(format!(
                    "layers: `{member}` depends on the dev tool `{dep}`"
                ));
            } else if layer_of.get(dep).is_some_and(|&l| l < own) {
                errors.push(format!(
                    "layers: `{member}` depends on `{dep}`, which is in a higher layer (02 §4.2)"
                ));
            }
        }
    }
}
