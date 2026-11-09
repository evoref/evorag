//! 開発用の作業コマンド (`cargo xtask <コマンド>`)。
//!
//! 仕様: 02 §4.1, 02 §4.3, 09 §6.2, 09 §7, 11 §6。

mod deps;
mod i18n;
mod metadata;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check                    fast lane before a PR: fmt, clippy, tests, check-deps, i18n (11 §6)
  check-deps               dependency rules (02 §4.3) and transitive count (NFR-203)
      [--manifest-path P]  measure another workspace (C-code check and count only)
  i18n                     message keys in code vs locales/*.toml (NFR-602)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = workspace_root();
    let result = match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("check") => check(&root),
        Some("check-deps") => check_deps(&root, &args[1..]),
        Some("i18n") => report("i18n", i18n::run(&root)),
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if result {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// xtask の crate の 1 つ上が workspace の根。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .to_path_buf()
}

fn report(name: &str, result: Result<Vec<String>, String>) -> bool {
    match result {
        Ok(errors) if errors.is_empty() => {
            println!("{name}: ok");
            true
        }
        Ok(errors) => {
            for e in &errors {
                eprintln!("error: {e}");
            }
            eprintln!("{name}: {} violation(s)", errors.len());
            false
        }
        Err(e) => {
            eprintln!("error: {e}");
            false
        }
    }
}

fn check_deps(root: &Path, args: &[String]) -> bool {
    let manifest = match args {
        [] => None,
        [flag, path] if flag == "--manifest-path" => Some(PathBuf::from(path)),
        _ => {
            eprintln!("{USAGE}");
            return false;
        }
    };
    let result = deps::load_policy(&root.join("deps-policy.toml"))
        .and_then(|policy| deps::run(&policy, manifest.as_deref()));
    report("check-deps", result)
}

/// 速い車線 (11 §6)。失敗があっても最後まで流し、まとめて報告する。
fn check(root: &Path) -> bool {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let steps: &[(&str, &[&str])] = &[
        ("fmt", &["fmt", "--all", "--check"]),
        (
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ),
        (
            "test",
            &[
                "nextest",
                "run",
                "--workspace",
                "--locked",
                "--no-tests=pass",
            ],
        ),
    ];
    let mut failed = Vec::new();
    for (name, args) in steps {
        println!("==> {name}: cargo {}", args.join(" "));
        let ok = Command::new(&cargo)
            .args(*args)
            .current_dir(root)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            failed.push(*name);
        }
    }
    println!("==> check-deps");
    if !check_deps(root, &[]) {
        failed.push("check-deps");
    }
    println!("==> i18n");
    if !report("i18n", i18n::run(root)) {
        failed.push("i18n");
    }
    if failed.is_empty() {
        println!("check: ok");
        true
    } else {
        eprintln!("check: failed: {}", failed.join(", "));
        false
    }
}
