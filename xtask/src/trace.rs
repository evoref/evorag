//! `cargo xtask trace [--version X.Y]`: 受入基準 (AC) と、試験・手順の突き合わせ (NFR-702、OQ-19)。
//!
//! - 要件の表 (`docs/01-requirements.md`) から、版 `X.Y` までに Must になる要件の AC を求める。FR は
//!   「優先度 / 版」の列の `Must / vX.Y` を、NFR は「版」の列のいちばん早い版を見る。
//! - コード (`crates/`・`bins/`・`xtask/`) の `verifies:` に名前のある AC を「試験で判定する」とみなす。
//! - `tests/ac-checklist.toml` の `status = "manual"`・`"review"` の AC を「手順で判定する」とみなす。
//!   `status = "later"` は、その AC が後の版の範囲 (`note` に理由) であることを示し、数えない。
//! - 次のどれかがあれば失敗にする: どれにも当たらない AC、要件の表に無い AC を指す `verifies:`、
//!   対応表の `tests` に書いた試験の関数が見つからないもの。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 要件の表の 1 行から、版 `want` までに Must になるかを決める。
fn must_by(cells: &[&str], is_fr: bool, want: (u32, u32)) -> bool {
    let versions = |s: &str| -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == b'v' && b[i + 1].is_ascii_digit() {
                let rest = &s[i + 1..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .unwrap_or(rest.len());
                let mut parts = rest[..end].split('.');
                if let (Some(Ok(major)), Some(Ok(minor))) = (
                    parts.next().map(str::parse::<u32>),
                    parts.next().map(str::parse::<u32>),
                ) {
                    out.push((major, minor));
                }
                i += 1 + end;
            } else {
                i += 1;
            }
        }
        out
    };
    if is_fr {
        // `Must / v0.2 / Must / v0.3 (事実)` のように、優先度と版の組が並ぶ。括弧の中は注なので見ない。
        let col = cells.get(3).copied().unwrap_or_default();
        let head = col.split('(').next().unwrap_or_default();
        let parts: Vec<&str> = head.split('/').map(str::trim).collect();
        parts
            .windows(2)
            .any(|w| w[0] == "Must" && versions(w[1]).first().is_some_and(|v| *v <= want))
    } else {
        // NFR は優先度の列が無く、節によって目標の列の有無が違う。要件の文より後の列の、いちばん早い版。
        cells
            .iter()
            .skip(3)
            .flat_map(|c| versions(c))
            .min()
            .is_some_and(|v| v <= want)
    }
}

/// 要件の表から、要件の ID → 版 `want` までに Must か、と AC の ID の一覧を読む。
fn read_requirements(text: &str, want: (u32, u32)) -> (BTreeMap<String, bool>, BTreeSet<String>) {
    let mut reqs = BTreeMap::new();
    let mut acs = BTreeSet::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        let Some(id) = cells.get(1) else {
            continue;
        };
        if id.starts_with("AC-") {
            // `AC-FR-206-1 (= AC-NFR-303-1)` のような別名と、`AC-NFR-101〜104-1` のような範囲も読む。
            acs.extend(ac_ids(id));
            acs.extend(ac_range(id));
        } else if id.starts_with("FR-") || id.starts_with("NFR-") {
            reqs.insert(
                (*id).to_owned(),
                must_by(&cells, id.starts_with("FR-"), want),
            );
        }
    }
    (reqs, acs)
}

/// `AC-NFR-101〜104-1` のような範囲を広げる。
fn ac_range(s: &str) -> Vec<String> {
    let Some(rest) = s.strip_prefix("AC-") else {
        return Vec::new();
    };
    let Some((kind, rest)) = rest.split_once('-') else {
        return Vec::new();
    };
    let Some((from, rest)) = rest.split_once('〜') else {
        return Vec::new();
    };
    let Some((to, n)) = rest.split_once('-') else {
        return Vec::new();
    };
    let n: String = n.chars().take_while(char::is_ascii_digit).collect();
    match (from.parse::<u32>(), to.parse::<u32>()) {
        (Ok(a), Ok(b)) if a <= b && !n.is_empty() => {
            (a..=b).map(|r| format!("AC-{kind}-{r}-{n}")).collect()
        }
        _ => Vec::new(),
    }
}

/// AC の ID から要件の ID (`AC-FR-212-1` → `FR-212`)。
fn requirement_of(ac: &str) -> Option<String> {
    let rest = ac.strip_prefix("AC-")?;
    let (req, _) = rest.rsplit_once('-')?;
    Some(req.to_owned())
}

/// 文字列の中の AC の ID を拾う。
fn ac_ids(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("AC-") {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(tail.len());
        let id = tail[..end].trim_end_matches('-');
        if id.split('-').count() == 4 {
            out.push(id.to_owned());
        }
        rest = &tail[end.max(3)..];
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        if p.is_dir() {
            if name != "target" && name != "fixtures" {
                rust_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// コードの `verifies:` に名前のある AC と、その場所。
fn verified(root: &Path) -> BTreeMap<String, Vec<String>> {
    let mut files = Vec::new();
    for d in ["crates", "bins", "xtask"] {
        rust_files(&root.join(d), &mut files);
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let rel = f
            .strip_prefix(root)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        for (n, line) in text.lines().enumerate() {
            if let Some(i) = line.find("verifies:") {
                for id in ac_ids(&line[i..]) {
                    out.entry(id).or_default().push(format!("{rel}:{}", n + 1));
                }
            }
        }
    }
    out
}

/// 対応表の 1 件。
struct Entry {
    status: String,
    tests: Vec<String>,
}

fn read_checklist(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    let path = root.join("tests").join("ac-checklist.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let doc: toml::Table =
        toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for item in doc
        .get("ac")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let get = |k: &str| item.get(k).and_then(toml::Value::as_str).unwrap_or("");
        out.insert(
            get("id").to_owned(),
            Entry {
                status: get("status").to_owned(),
                tests: item
                    .get("tests")
                    .and_then(toml::Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

/// 対応表の `tests` の 1 件が見つかるか。`path::関数名` は関数を、`path` と `path#ジョブ` (CI の
/// 定義) はファイルを探す。
fn test_exists(root: &Path, t: &str) -> bool {
    let Some((file, func)) = t.rsplit_once("::") else {
        let file = t.split('#').next().unwrap_or(t);
        return root.join(file).exists();
    };
    std::fs::read_to_string(root.join(file)).is_ok_and(|text| text.contains(&format!("fn {func}(")))
}

fn parse_version(args: &[String]) -> Result<(u32, u32), String> {
    let v = match args {
        [] => "0.2",
        [flag, v] if flag == "--version" => v.as_str(),
        _ => return Err("usage: cargo xtask trace [--version X.Y]".into()),
    };
    let (a, b) = v
        .split_once('.')
        .ok_or_else(|| format!("bad version `{v}`"))?;
    Ok((
        a.parse().map_err(|_| format!("bad version `{v}`"))?,
        b.parse().map_err(|_| format!("bad version `{v}`"))?,
    ))
}

pub fn run(root: &Path, args: &[String]) -> Result<Vec<String>, String> {
    let want = parse_version(args)?;
    let path = root.join("docs").join("01-requirements.md");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let (reqs, acs) = read_requirements(&text, want);
    let code = verified(root);
    let list = read_checklist(root)?;
    let mut errors = Vec::new();
    for (id, places) in &code {
        if !acs.contains(id) {
            errors.push(format!("`verifies: {id}` names no AC ({})", places[0]));
        }
    }
    for (id, e) in &list {
        for t in &e.tests {
            if !test_exists(root, t) {
                errors.push(format!(
                    "{id}: the test `{t}` in tests/ac-checklist.toml is not found"
                ));
            }
        }
    }
    // 対応表の `status = "test"` で、書いた試験がすべて見つかるもの (CI のジョブで判定するものを含む)。
    let listed = |ac: &str| {
        list.get(ac).is_some_and(|e| {
            e.status == "test"
                && !e.tests.is_empty()
                && e.tests.iter().all(|t| test_exists(root, t))
        })
    };
    let (mut by_test, mut by_steps, mut later) = (0, 0, 0);
    for ac in &acs {
        let in_scope = requirement_of(ac).is_some_and(|r| reqs.get(&r).copied().unwrap_or(false));
        if !in_scope {
            continue;
        }
        let status = list.get(ac).map(|e| e.status.as_str()).unwrap_or("");
        if status == "later" {
            later += 1;
        } else if code.contains_key(ac) || listed(ac) {
            by_test += 1;
        } else if matches!(status, "manual" | "review") {
            by_steps += 1;
        } else {
            errors.push(format!(
                "{ac}: no test (`verifies:`) and no manual or review steps in tests/ac-checklist.toml"
            ));
        }
    }
    println!(
        "v{}.{}: {} Must ACs judged by tests, {} by manual or review steps, {} left for later versions",
        want.0, want.1, by_test, by_steps, later
    );
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// verifies: AC-NFR-702-2 (要件の表から版までの Must の AC を求める)
    #[test]
    fn must_versions_are_read_from_the_priority_column() {
        let fr = |col: &str| {
            let line = format!("| FR-1 | x | {col} | T | 05 |");
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            must_by(&cells, true, (0, 2))
        };
        assert!(fr("Must / v0.2"));
        assert!(fr(
            "Must / v0.1 (`--deep` は Should。v0.2 に繰り越し、08 §6)"
        ));
        assert!(fr("Must / v0.2 / Must / v0.3 (事実)"));
        assert!(!fr("Should / v0.1 (v0.2 に繰り越し)"));
        assert!(!fr("Must / v0.3"));
        let nfr = |col: &str| {
            let line = format!("| NFR-1 | x | 目標 | {col} | B |");
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            must_by(&cells, false, (0, 2))
        };
        assert!(nfr("v0.1 / v0.2"));
        assert!(nfr("v0.2 (確定、OQ-07)"));
        // 目標の列の無い節。
        let line = "| NFR-402 | Linux で動く | v0.2 (確定、OQ-07) | M |";
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        assert!(must_by(&cells, false, (0, 2)));
        assert!(!nfr("v0.5"));
    }

    #[test]
    fn ac_ids_are_picked_from_a_verifies_line() {
        assert_eq!(
            ac_ids("verifies: AC-FR-402-1 (v0.2: …), AC-FR-416-1, AC-NFR-307-2"),
            ["AC-FR-402-1", "AC-FR-416-1", "AC-NFR-307-2"]
        );
        assert_eq!(requirement_of("AC-NFR-307-2").as_deref(), Some("NFR-307"));
        assert_eq!(
            ac_range("AC-NFR-101〜104-1"),
            [
                "AC-NFR-101-1",
                "AC-NFR-102-1",
                "AC-NFR-103-1",
                "AC-NFR-104-1"
            ]
        );
    }
}
