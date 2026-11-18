//! `evorag --version` に出す作りの情報 (07 §5、FR-906、12 §7.4)。
//!
//! - 入れ方は、ビルドのときの環境変数 `EVORAG_CHANNEL` で埋め込む (配布物は `zip`。無ければ `cargo`)。
//!   パスから推測しない。
//! - コミットは `git` で得る (得られなければ `unknown`)。
//! - リリースの日付は、環境変数 `EVORAG_RELEASE_DATE` (`YYYY-MM-DD`、UTC) で埋め込む (12 §5.3)。
//!   `cargo xtask dist` がタグの指すコミットの committer の日時から求めて渡す。`EVORAG_CHANNEL=zip`
//!   なのに無ければ、ビルドを失敗させる (12 §7.4)。開発のビルドでは埋め込まない
//!   (`option_env!("EVORAG_RELEASE_DATE")` が `None`)。

use std::path::Path;
use std::process::Command;

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// `YYYY-MM-DD` の形か (日付として正しいかは evorag-update の `ReleaseDate` が確かめる)。
fn is_date_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

fn main() {
    let commit =
        output("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc = output(&rustc, &["--version"])
        .and_then(|v| v.split_whitespace().nth(1).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let channel = std::env::var("EVORAG_CHANNEL").unwrap_or_else(|_| "cargo".into());
    println!("cargo:rustc-env=EVORAG_COMMIT={commit}");
    println!("cargo:rustc-env=EVORAG_RUSTC={rustc}");
    println!("cargo:rustc-env=EVORAG_TARGET={target}");
    println!("cargo:rustc-env=EVORAG_CHANNEL={channel}");
    println!("cargo:rerun-if-env-changed=EVORAG_CHANNEL");
    println!("cargo:rerun-if-env-changed=EVORAG_RELEASE_DATE");
    match std::env::var("EVORAG_RELEASE_DATE")
        .ok()
        .filter(|d| !d.is_empty())
    {
        Some(date) if is_date_shape(&date) => {
            println!("cargo:rustc-env=EVORAG_RELEASE_DATE={date}");
        }
        Some(date) => {
            panic!("EVORAG_RELEASE_DATE must be YYYY-MM-DD (UTC), not `{date}` (12 §5.3)")
        }
        None if channel == "zip" => panic!(
            "EVORAG_CHANNEL=zip needs EVORAG_RELEASE_DATE (YYYY-MM-DD, UTC; 12 §7.4); build the release with `cargo xtask dist`"
        ),
        None => {}
    }
    // コミットが変わったら作り直す (無いパスを渡すと毎回作り直すので、あるものだけ)。
    let git = Path::new("../../.git");
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
        if let Some(r) = head.trim().strip_prefix("ref: ")
            && git.join(r).exists()
        {
            println!("cargo:rerun-if-changed=../../.git/{r}");
        }
    }
}
