//! 開発用の作業コマンド (`cargo xtask <コマンド>`)。
//!
//! 仕様: 02 §4.1, 02 §4.3, 09 §5.2, 09 §6.2, 09 §7, 11 §6。

mod deps;
mod dist;
mod i18n;
mod lme;
mod metadata;
mod trace;
mod verify_tag;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check                    fast lane before a PR: fmt, clippy, tests, check-deps, i18n (11 §6)
  check-deps               dependency rules (02 §4.3) and transitive count (NFR-203)
      [--manifest-path P]  measure another workspace (C-code check and count only)
  i18n                     message keys in code vs locales/*.toml (NFR-602)
  trace [--version X.Y]    every Must AC up to X.Y (default 0.2) has a test or steps (NFR-702)
  dist                     build the release archive, SHA256SUMS and release.json (12 §5)
      [--target T] [--out DIR] [--skip-build] [--tag vX.Y.Z]
      [--manifest-only]    only rewrite SHA256SUMS and release.json for the archives in --out
  bench [--out DIR]        benchmarks BM-101..104, 106, 201 in release (09 §5.1); results to docs/perf/<X.Y.Z>
  verify-tag TAG           check a release tag before building (09 §9.2 step 4)
  live-llm [--out DIR]     tests against real llama-servers (09 §3.4): chat and the fact recall eval
                           (09 §5.2) at EVORAG_LIVE_LLM_URL, recall at EVORAG_LIVE_EMBED_URL and
                           EVORAG_LIVE_RERANK_URL; eval results (EVAL-*.json) to --out
  longmemeval --data FILE  LongMemEval at EVORAG_LIVE_LLM_URL (09 §5.2); results to docs/perf/<X.Y.Z>
      [--out DIR]          (default)
      [--set S]            dev (default, the tuning subset) or holdout:<slice> (judging only)
      [--roots DIR]        save each question's ingested data root and reuse it later
      [--ingest-only]      ingest (and save) without asking; allowed for sealed slices
      [--settle SECS]      wait for the background steps up to SECS per question (default 1800)
      [--no-typing]        do not send 'input idle' before the question (05 §4.5)
      [--limit N]          only the first N questions (a smoke run; marked partial)
  lme-compare --base FILE... --new FILE... [--out FILE]
                           paired comparison of LongMemEval results (McNemar exact test, 09 §5.2)
  verify-release DIR       check SHA256SUMS, release.json and the minisign signatures (AC-FR-801-1)
      [--pubkey FILE] [--require-signatures]";

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
        Some("trace") => report("trace", trace::run(&root, &args[1..])),
        Some("bench") => bench(&root, &args[1..]),
        Some("live-llm") => live_llm(&root, &args[1..]),
        Some("longmemeval") => longmemeval(&root, &args[1..]),
        Some("lme-compare") => done("lme-compare", lme::compare(&args[1..])),
        Some("dist") => done("dist", dist::dist(&root, &args[1..])),
        Some("verify-tag") => done("verify-tag", verify_tag::verify_tag(&root, &args[1..])),
        Some("verify-release") => done("verify-release", dist::verify_release(&root, &args[1..])),
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

fn done(name: &str, r: Result<(), String>) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{name}: {e}");
            false
        }
    }
}

/// ベンチマーク (09 §5.1)。基準機で、release で流す。結果は docs/perf/<版>/BM-<ID>.json。
fn bench(root: &Path, args: &[String]) -> bool {
    let out = match args {
        // 記録の置き場所は docs/perf/<X.Y.Z>/ (09 §9.1)。
        [] => match dist::workspace_version(root) {
            Ok(v) => root.join("docs").join("perf").join(v),
            Err(e) => {
                eprintln!("bench: {e}");
                return false;
            }
        },
        // 試験は crate のフォルダで動くので、相対パスは起動したフォルダからの絶対パスにして渡す。
        [flag, dir] if flag == "--out" => match std::env::current_dir() {
            Ok(cwd) => cwd.join(dir),
            Err(e) => {
                eprintln!("bench: {e}");
                return false;
            }
        },
        _ => {
            eprintln!("usage: cargo xtask bench [--out DIR]");
            return false;
        }
    };
    println!("==> bench (results in {})", out.display());
    Command::new("cargo")
        .current_dir(root)
        .args([
            "nextest",
            "run",
            "--release",
            "--locked",
            "-p",
            "evorag-core",
            "-p",
            "evorag-tui",
            "-p",
            "evorag-index",
            "--run-ignored",
            "only",
            "--test-threads",
            "1",
            "--no-capture",
            "-E",
            "test(/^bm_/)",
        ])
        .env("EVORAG_BENCH_OUT", &out)
        .status()
        .is_ok_and(|s| s.success())
}

/// 起動したフォルダからの絶対パス (試験は crate のフォルダで動くので、相対パスのままでは渡せない)。
fn absolute(path: &str) -> Option<PathBuf> {
    match std::env::current_dir() {
        Ok(cwd) => Some(cwd.join(path)),
        Err(e) => {
            eprintln!("{e}");
            None
        }
    }
}

/// 実際の llama-server を使う試験 (09 §3.4)。接続先は EVORAG_LIVE_LLM_URL。応答の llama-server があれば、
/// 事実の想起の評価 (09 §5.2、`live_facts`) も流す。`--out` は評価の結果 (`EVAL-*.json`) の置き場所。
fn live_llm(root: &Path, args: &[String]) -> bool {
    let out = match args {
        [] => None,
        [flag, dir] if flag == "--out" => match absolute(dir) {
            Some(d) => Some(d),
            None => return false,
        },
        _ => {
            eprintln!("usage: cargo xtask live-llm [--out DIR]");
            return false;
        }
    };
    let chat = std::env::var_os("EVORAG_LIVE_LLM_URL").is_some();
    let recall = std::env::var_os("EVORAG_LIVE_EMBED_URL").is_some()
        && std::env::var_os("EVORAG_LIVE_RERANK_URL").is_some();
    if !chat && !recall {
        eprintln!(
            "live-llm: set EVORAG_LIVE_LLM_URL (e.g. http://127.0.0.1:8080) for the chat tests, \
             and/or EVORAG_LIVE_EMBED_URL and EVORAG_LIVE_RERANK_URL for the recall tests"
        );
        return false;
    }
    let mut ok = true;
    for (on, test) in [
        (chat, "live_llm"),
        (chat, "live_facts"),
        (recall, "live_recall"),
    ] {
        if !on {
            continue;
        }
        println!("==> live-llm: {test}");
        let mut cmd = Command::new("cargo");
        cmd.current_dir(root).args([
            "nextest",
            "run",
            "--locked",
            "-p",
            "evorag-core",
            "--test",
            test,
            "--run-ignored",
            "only",
            "--no-capture",
        ]);
        if let Some(dir) = &out {
            cmd.env("EVORAG_BENCH_OUT", dir);
        }
        ok &= cmd.status().is_ok_and(|s| s.success());
    }
    ok
}

/// LongMemEval での評価 (09 §5.2)。データ (`--data`) はリポジトリの外に置く。結果は既定で
/// docs/perf/<X.Y.Z>/EVAL-longmemeval[-holdout-<切片>].json。
fn longmemeval(root: &Path, args: &[String]) -> bool {
    const USAGE_LME: &str = "usage: cargo xtask longmemeval --data FILE [--out DIR] [--set S] [--roots DIR]          [--ingest-only] [--settle SECS] [--no-typing] [--limit N]";
    let (mut data, mut out, mut limit, mut set, mut roots, mut settle) =
        (None, None, None, None, None, None);
    let (mut ingest_only, mut no_typing) = (false, false);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--ingest-only" => {
                ingest_only = true;
                continue;
            }
            "--no-typing" => {
                no_typing = true;
                continue;
            }
            _ => {}
        }
        let Some(value) = it.next() else {
            eprintln!("{USAGE_LME}");
            return false;
        };
        match flag.as_str() {
            "--data" => data = absolute(value),
            "--out" => out = absolute(value),
            "--roots" => roots = absolute(value),
            "--set" => set = Some(value.clone()),
            "--limit" if value.parse::<usize>().is_ok() => limit = Some(value.clone()),
            "--settle" if value.parse::<u64>().is_ok() => settle = Some(value.clone()),
            _ => {
                eprintln!("{USAGE_LME}");
                return false;
            }
        }
    }
    let Some(data) = data else {
        eprintln!("{USAGE_LME}");
        return false;
    };
    if std::env::var_os("EVORAG_LIVE_LLM_URL").is_none() {
        eprintln!("longmemeval: set EVORAG_LIVE_LLM_URL (e.g. http://127.0.0.1:8080)");
        return false;
    }
    let out = match out {
        Some(o) => o,
        None => match dist::workspace_version(root) {
            Ok(v) => root.join("docs").join("perf").join(v),
            Err(e) => {
                eprintln!("longmemeval: {e}");
                return false;
            }
        },
    };
    println!("==> longmemeval (results in {})", out.display());
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .args([
            "nextest",
            "run",
            "--locked",
            "-p",
            "evorag-core",
            "--test",
            "longmemeval",
            "--run-ignored",
            "only",
            "--no-capture",
            "-E",
            "test(live_longmemeval_baseline)",
        ])
        .env("EVORAG_LONGMEMEVAL_DATA", &data)
        .env("EVORAG_BENCH_OUT", &out);
    if let Some(n) = limit {
        cmd.env("EVORAG_LONGMEMEVAL_LIMIT", n);
    }
    if let Some(s) = set {
        cmd.env("EVORAG_LME_SET", s);
    }
    if let Some(r) = roots {
        cmd.env("EVORAG_LME_ROOTS", r);
    }
    if let Some(s) = settle {
        cmd.env("EVORAG_LME_SETTLE_SECS", s);
    }
    if ingest_only {
        cmd.env("EVORAG_LME_INGEST_ONLY", "1");
    }
    if no_typing {
        cmd.env("EVORAG_LME_TYPING", "0");
    }
    cmd.status().is_ok_and(|s| s.success())
}
