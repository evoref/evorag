//! 開発用の作業コマンド (`cargo xtask <コマンド>`)。
//!
//! 仕様: 02 §4.1, 09 §6.2, 11 §6。コマンド (check・check-deps・i18n) は M1.1 の次のタスクで入れる。

use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>\n\n\
                     commands are added in later tasks of M1.1 (docs/08-roadmap.md)";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            eprintln!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
