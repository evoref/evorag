//! リリースのタグの検証 (`cargo xtask verify-tag <タグ>`、09 §9.2 の手順 4)。`release.yml` が最初に流す。
//!
//! 1. タグが署名付きで、署名が `.github/allowed_signers` (Git の SSH 署名鍵) の鍵のものである
//!    (`git verify-tag`)。
//! 2. タグの名前が `v<workspace の版>` と同じである (プレリリースは出さない、09 §9.1)。
//! 3. タグが `main` のマージコミット (親が 2 つ) を指し、`origin/main` に含まれる。
//! 4. `docs/compat/<X.Y.Z>/release-checklist.md` の `判定コミット:` の SHA がタグの祖先で、判定コミットと
//!    タグの差分が記録の許可リスト (`docs/compat/<X.Y.Z>/`・`docs/perf/<X.Y.Z>/`・`tests/ac-checklist.toml`)
//!    だけである (09 §8.2)。

use std::path::Path;
use std::process::Command;

const ALLOWED_SIGNERS: &str = ".github/allowed_signers";
const JUDGE_PREFIX: &str = "判定コミット:";

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// リリースのチェックリストの先頭の `判定コミット: <40 桁の SHA>` を読む。
pub fn judge_commit(checklist: &str) -> Result<String, String> {
    let line = checklist
        .lines()
        .find(|l| l.trim_start().starts_with(JUDGE_PREFIX))
        .ok_or("the release checklist has no `判定コミット:` line")?;
    let sha = line.trim_start()[JUDGE_PREFIX.len()..].trim();
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "`判定コミット:` must be a 40-digit SHA, got `{sha}`"
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

/// 判定の後に入れてよいパスか (記録の許可リスト、09 §8.2)。
pub fn allowed_after_judge(version: &str, path: &str) -> bool {
    path.starts_with(&format!("docs/compat/{version}/"))
        || path.starts_with(&format!("docs/perf/{version}/"))
        || path == "tests/ac-checklist.toml"
}

/// タグの名前が `v<版>` そのものか (接尾辞を許さない)。
pub fn tag_matches(tag: &str, version: &str) -> bool {
    tag.strip_prefix('v') == Some(version)
}

/// `cargo xtask verify-tag <タグ>`
pub fn verify_tag(root: &Path, args: &[String]) -> Result<(), String> {
    let [tag] = args else {
        return Err("usage: cargo xtask verify-tag <tag>".into());
    };
    let version = crate::dist::workspace_version(root)?;

    // 2. 名前と版。
    if !tag_matches(tag, &version) {
        return Err(format!(
            "the tag {tag} must be exactly v{version} (pre-releases are not used, 09 §9.1)"
        ));
    }
    println!("ok  the tag matches the workspace version {version}");

    // 1. 署名。
    if !root.join(ALLOWED_SIGNERS).is_file() {
        return Err(format!(
            "{ALLOWED_SIGNERS} is missing (the public SSH keys allowed to sign tags, 11 §9.7)"
        ));
    }
    git(
        root,
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("gpg.ssh.allowedSignersFile={ALLOWED_SIGNERS}"),
            "verify-tag",
            tag,
        ],
    )?;
    println!("ok  the tag is signed by a key in {ALLOWED_SIGNERS}");

    // 3. main のマージコミット。
    let commit = git(root, &["rev-parse", &format!("{tag}^{{commit}}")])?;
    let parents = git(root, &["rev-list", "--parents", "-n", "1", &commit])?;
    if parents.split_whitespace().count() != 3 {
        return Err(format!("{tag} must point to a merge commit on main"));
    }
    git(
        root,
        &["merge-base", "--is-ancestor", &commit, "origin/main"],
    )
    .map_err(|_| format!("{tag} is not on origin/main"))?;
    println!("ok  the tag points to a merge commit on main");

    // 4. 判定コミットと、その後の差分。
    let checklist_path = format!("docs/compat/{version}/release-checklist.md");
    let checklist = git(root, &["show", &format!("{commit}:{checklist_path}")])
        .map_err(|_| format!("{checklist_path} is missing at {tag}"))?;
    let judge = judge_commit(&checklist)?;
    git(root, &["merge-base", "--is-ancestor", &judge, &commit])
        .map_err(|_| format!("the judge commit {judge} is not an ancestor of {tag}"))?;
    let changed = git(root, &["diff", "--name-only", &judge, &commit])?;
    let outside: Vec<&str> = changed
        .lines()
        .filter(|p| !p.is_empty() && !allowed_after_judge(&version, p))
        .collect();
    if !outside.is_empty() {
        return Err(format!(
            "files changed after the judge commit {judge} outside the allowed records (redo the gate, 09 §8.2): {}",
            outside.join(", ")
        ));
    }
    println!("ok  only the allowed records changed after the judge commit {judge}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_judge_line_is_a_full_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            judge_commit(&format!("# x\n\n判定コミット: {sha}\n")).unwrap(),
            sha
        );
        // 雛形のまま・短い SHA は通さない。
        assert!(judge_commit("判定コミット: <ゲートを判定した…>\n").is_err());
        assert!(judge_commit("判定コミット: 0123abc\n").is_err());
        assert!(judge_commit("# 何も無い\n").is_err());
    }

    #[test]
    fn only_the_records_may_change_after_the_judge() {
        for ok in [
            "docs/compat/0.1.0/release-checklist.md",
            "docs/perf/0.1.0/BM-101.json",
            "tests/ac-checklist.toml",
        ] {
            assert!(allowed_after_judge("0.1.0", ok), "{ok}");
        }
        for ng in [
            "docs/compat/0.2.0/acceptance.md",
            "docs/09-testing-quality.md",
            "Cargo.toml",
            "crates/evorag-core/src/lib.rs",
        ] {
            assert!(!allowed_after_judge("0.1.0", ng), "{ng}");
        }
    }

    #[test]
    fn tags_must_match_exactly() {
        assert!(tag_matches("v0.1.0", "0.1.0"));
        assert!(!tag_matches("v0.1.0-rc.1", "0.1.0"));
        assert!(!tag_matches("0.1.0", "0.1.0"));
        assert!(!tag_matches("v0.1.1", "0.1.0"));
    }
}
