//! 依存の情報を cargo から読む。
//!
//! - package の情報 (workspace の crate、宣言した依存、`links`) は `cargo metadata` から読む。
//! - 実際に入る依存の木は `cargo tree` から読む。`cargo metadata` の resolve は機能 (feature) の選択を
//!   反映せず、使われない optional の依存まで含むため、数や C の判定には使わない。

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub packages: Vec<Package>,
    pub workspace_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub links: Option<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
pub struct Dependency {
    /// package の名前 (rename していても元の名前)。
    pub name: String,
}

impl Metadata {
    pub fn members(&self) -> Vec<&Package> {
        self.packages
            .iter()
            .filter(|p| self.workspace_members.contains(&p.id))
            .collect()
    }

    pub fn member_names(&self) -> BTreeSet<&str> {
        self.members()
            .into_iter()
            .map(|p| p.name.as_str())
            .collect()
    }

    pub fn package(&self, pkg: &Pkg) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.name == pkg.name && p.version == pkg.version)
    }
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
}

fn run(mut cmd: Command, what: &str) -> Result<Vec<u8>, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run cargo {what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo {what} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

pub fn load(manifest_path: Option<&Path>) -> Result<Metadata, String> {
    let mut cmd = cargo();
    cmd.args(["metadata", "--format-version", "1", "--locked"]);
    if let Some(p) = manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    let out = run(cmd, "metadata")?;
    serde_json::from_slice(&out).map_err(|e| format!("failed to parse cargo metadata: {e}"))
}

/// 依存の木の 1 つの package (名前と版)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pkg {
    pub name: String,
    pub version: String,
}

/// `cargo tree` の引数。
pub struct Tree<'a> {
    pub manifest_path: Option<&'a Path>,
    /// `None` は workspace の全体。
    pub package: Option<&'a str>,
    pub target: &'a str,
    /// `cargo tree -e` の値 (例: `normal,build`)。
    pub edges: &'a str,
    /// この名前の package に来たら、その先 (下の枝) をたどらない。package 自体は集合に入る。
    /// `cargo tree --prune` は木に無い名前を渡すと失敗するので、出力を読む側で枝を切る。
    pub stop_at: Vec<&'a str>,
}

impl Tree<'_> {
    /// 木に入る package の集合 (起点も含む)。
    pub fn packages(&self) -> Result<BTreeSet<Pkg>, String> {
        let mut cmd = cargo();
        cmd.args(["tree", "--locked", "--prefix", "depth", "--format", "{p}"]);
        cmd.args(["--target", self.target, "-e", self.edges]);
        if !self.stop_at.is_empty() {
            // 重複を省くと、切った枝の下で初めて出た package の子が、ほかの場所で出なくなる。
            cmd.arg("--no-dedupe");
        }
        match self.package {
            Some(p) => cmd.args(["-p", p]),
            None => cmd.arg("--workspace"),
        };
        if let Some(m) = self.manifest_path {
            cmd.arg("--manifest-path").arg(m);
        }
        let out = run(cmd, "tree")?;
        Ok(parse_tree(&String::from_utf8_lossy(&out), &self.stop_at))
    }
}

/// `cargo tree --prefix depth --format {p}` の出力を読む。
/// 行の例: `0evorag-types v0.0.0 (E:\...)`、`1serde v1.0.229`、`2memchr v2.8.3 (*)`。
fn parse_tree(text: &str, stop_at: &[&str]) -> BTreeSet<Pkg> {
    let mut set = BTreeSet::new();
    // この深さより深い行は、切った枝の中。
    let mut cut: Option<usize> = None;
    for line in text.lines() {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(depth) = line[..digits].parse::<usize>() else {
            continue;
        };
        if cut.is_some_and(|c| depth > c) {
            continue;
        }
        cut = None;
        let mut parts = line[digits..].split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            continue;
        };
        if depth > 0 && stop_at.contains(&name) {
            cut = Some(depth);
        }
        set.insert(Pkg {
            name: name.to_owned(),
            version: version.to_owned(),
        });
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(set: &BTreeSet<Pkg>) -> Vec<&str> {
        set.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn parse_tree_reads_names_and_versions() {
        let text = "0a v0.1.0 (C:\\x)
1b v1.2.3
2c v0.0.1 (*)
1d v4.0.0 (proc-macro)
";
        let set = parse_tree(text, &[]);
        assert_eq!(names(&set), ["a", "b", "c", "d"]);
        assert!(set.contains(&Pkg {
            name: "b".into(),
            version: "1.2.3".into()
        }));
    }

    #[test]
    fn parse_tree_cuts_branches_below_stop_at() {
        let text = "0root v0.0.0
1member v0.0.0
2hidden v1.0.0
3deeper v1.0.0
1kept v1.0.0
2also v1.0.0
";
        let set = parse_tree(text, &["member"]);
        assert_eq!(names(&set), ["also", "kept", "member", "root"]);
    }

    #[test]
    fn parse_tree_skips_headers_and_blank_lines() {
        let text = "0root v0.0.0

[build-dependencies]
1b v1.0.0
";
        assert_eq!(names(&parse_tree(text, &[])), ["b", "root"]);
    }
}
