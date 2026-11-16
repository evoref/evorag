//! `evorag export <出力先>` (03 §11、FR-209)。記録は `evorag-store` が書き出し、ここでは設定を
//! API キーを除いて写し、結果を知らせる。

use std::path::Path;
use std::process::ExitCode;

use evorag_store::export::{ExportError, export};
use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;

/// 設定から API キー (`api_key` という名前のキー) を除く。
fn strip_secrets(v: &mut toml::Table) {
    v.remove("api_key");
    for (_, item) in v.iter_mut() {
        match item {
            toml::Value::Table(t) => strip_secrets(t),
            toml::Value::Array(a) => {
                for x in a {
                    if let toml::Value::Table(t) = x {
                        strip_secrets(t);
                    }
                }
            }
            _ => {}
        }
    }
}

/// 設定ファイルを、API キーを除いて `out/config.toml` に写す。無ければ写さない。
fn copy_config(config_file: &Path, out: &Path) -> std::io::Result<bool> {
    let text = match std::fs::read_to_string(config_file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| std::io::Error::other(e.to_string()))?;
    strip_secrets(&mut table);
    let path = out.join("config.toml");
    evorag_store::fsutil::prepare_private_file(&path)?;
    std::fs::write(
        &path,
        toml::to_string(&table).map_err(std::io::Error::other)?,
    )?;
    Ok(true)
}

pub fn run(c: &Catalog, data_root: &Path, config_file: &Path, out: &Path, now_ms: i64) -> ExitCode {
    // 書き出すのは 03 §11 の「含めるもの」だけ。作り直せる派生の記録 (チャンク・埋め込み・点検の結果) と
    // 手順の記録 (job) は書き出さない。ノートは含める (OQ-37 の 9)。
    let report = match export(
        data_root,
        out,
        env!("CARGO_PKG_VERSION"),
        now_ms,
        &[
            evorag_memory::KIND_MODEL_PROBE,
            evorag_memory::KIND_CHUNK,
            evorag_memory::KIND_EMBEDDING,
            evorag_memory::KIND_JOB,
        ],
    ) {
        Ok(r) => r,
        Err(ExportError::NotEmpty(p)) => {
            eprintln!(
                "{}",
                c.with(
                    MessageKey::CliExportNotEmpty,
                    &[("path", &p.display().to_string())]
                )
            );
            return ExitCode::from(1);
        }
        Err(ExportError::Open(evorag_store::OpenError::Lock(
            evorag_store::lock::LockError::InUse { pid },
        ))) => {
            return crate::data_root_in_use(c, data_root, pid);
        }
        Err(e @ ExportError::Open(_)) => {
            eprintln!(
                "{}",
                c.with(
                    MessageKey::CliDataRootUnusable,
                    &[("detail", &e.to_string())]
                )
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!(
                "{}",
                c.with(MessageKey::CliError, &[("detail", &e.to_string())])
            );
            return ExitCode::from(1);
        }
    };
    if let Err(e) = copy_config(config_file, out) {
        eprintln!(
            "{}",
            c.with(MessageKey::CliError, &[("detail", &e.to_string())])
        );
        return ExitCode::from(1);
    }
    let count: u64 = report.kinds.values().sum();
    let kinds = report
        .kinds
        .iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join("、");
    println!(
        "{}",
        c.with(
            MessageKey::CliExported,
            &[
                ("count", &count.to_string()),
                ("kinds", &kinds),
                ("path", &out.display().to_string()),
            ],
        )
    );
    if report.skipped_newer + report.skipped_corrupt > 0 {
        eprintln!(
            "{}",
            c.with(
                MessageKey::CliExportSkipped,
                &[
                    ("newer", &report.skipped_newer.to_string()),
                    ("corrupt", &report.skipped_corrupt.to_string()),
                ],
            )
        );
    }
    if report.pending_journal > 0 {
        eprintln!(
            "{}",
            c.with(
                MessageKey::CliExportPending,
                &[("count", &report.pending_journal.to_string())]
            )
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_are_removed_everywhere() {
        let mut t: toml::Table = r#"
version = 1
[llm.chat]
url = "http://127.0.0.1:8080"
api_key = "secret"
api_key_env = "KEY"
[[llm.chat.models]]
name = "big"
url = "http://127.0.0.1:8081"
api_key = "secret2"
"#
        .parse()
        .unwrap();
        strip_secrets(&mut t);
        let s = toml::to_string(&t).unwrap();
        assert!(!s.contains("secret"), "{s}");
        assert!(
            s.contains("api_key_env"),
            "the name of the variable is not a secret"
        );
    }
}
