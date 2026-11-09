//! `evorag setup` (07 §5、FR-510): llama-server を探し、能力を確かめ、プロンプトの言語を尋ね
//! (新しいデータ根のとき、07 §6.1)、`config.toml` を書く。
//!
//! - 探すのはループバックだけ (`--url`、設定の `llm.chat.url`、`127.0.0.1:8080`・`:8081`)。
//! - 見つからなければ、入手と起動の手順を示し、設定ファイルを書かない (AC-FR-510-1)。
//!   起動例には `--host 127.0.0.1` と `--api-key` を付ける (06 §9)。
//! - 既にある設定ファイルを書き換えるときは、確かめてから、元のファイルを写して残す。

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use evorag_llm::{Endpoint, HttpClient, LlmClient, LlmError, RetryPolicy, ServerInfo};
use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;

use crate::config::Config;

/// 探す接続先の既定。
const CANDIDATES: &[&str] = &["http://127.0.0.1:8080", "http://127.0.0.1:8081"];

pub struct SetupArgs {
    pub url: Option<String>,
    pub api_key_env: Option<String>,
    pub yes: bool,
}

enum Probe {
    Found(ServerInfo),
    NeedsKey,
    Missing,
}

fn probe(rt: &tokio::runtime::Runtime, url: &str, key: Option<String>, remote: bool) -> Probe {
    let Ok(endpoint) = Endpoint::new(url, key, remote) else {
        return Probe::Missing;
    };
    // 探すときは再試行しない (無い接続先で待たない)。
    let Ok(client) = HttpClient::new(endpoint, RetryPolicy { delays: Vec::new() }) else {
        return Probe::Missing;
    };
    let r = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            client.health().await?;
            client.server_info().await
        })
        .await
    });
    match r {
        Ok(Ok(info)) => Probe::Found(info),
        Ok(Err(LlmError::Http { status, .. })) if status == 401 || status == 403 => Probe::NeedsKey,
        _ => Probe::Missing,
    }
}

/// 新しいデータ根か (プロンプトの言語がまだ記録されていない)。
fn language_recorded(data_root: &Path) -> bool {
    evorag_store::inspect::inspect(data_root, evorag_memory::kinds(), None)
        .ok()
        .is_some_and(|i| i.meta.contains_key(evorag_store::meta::PROMPT_LANGUAGE))
}

fn ask(prompt: &str) -> String {
    use std::io::{BufRead, Write};
    eprint!("{prompt} ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_owned()
}

/// 新しく書く設定ファイルの中身 (説明つき)。
fn template(url: &str, api_key_env: Option<&str>, language: Option<&str>) -> String {
    let mut s = String::from(
        "# evorag の設定 (evorag setup が書いた。キーの説明は docs/07-tui-config.md §4)\nversion = 1\n",
    );
    if let Some(l) = language {
        s.push_str(&format!(
            "\n[prompt]\n# システムプロンプトの言語 (このデータ根では後から変えられない)\nlanguage = \"{l}\"\n"
        ));
    }
    s.push_str(&format!("\n[llm.chat]\nurl = \"{url}\"\n"));
    match api_key_env {
        Some(v) => s.push_str(&format!("api_key_env = \"{v}\"\n")),
        None => s.push_str("# llama-server を --api-key で起動したときは、キーを入れた環境変数の名前を書く\n# api_key_env = \"EVORAG_API_KEY\"\n"),
    }
    s
}

/// 既にある設定に、見つけた接続先などを当てる。変わらなければ `None`。
fn updated(
    text: &str,
    url: &str,
    api_key_env: Option<&str>,
    language: Option<&str>,
) -> Result<Option<String>, String> {
    let mut t: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let before = t.clone();
    t.insert("version".into(), toml::Value::Integer(1));
    let table = |t: &mut toml::Table, k: &str| -> Result<toml::Table, String> {
        match t.remove(k) {
            None => Ok(toml::Table::new()),
            Some(toml::Value::Table(x)) => Ok(x),
            Some(_) => Err(format!("{k} is not a table")),
        }
    };
    let mut llm = table(&mut t, "llm")?;
    let mut chat = table(&mut llm, "chat")?;
    chat.insert("url".into(), toml::Value::String(url.into()));
    if let Some(v) = api_key_env {
        chat.insert("api_key_env".into(), toml::Value::String(v.into()));
    }
    llm.insert("chat".into(), toml::Value::Table(chat));
    t.insert("llm".into(), toml::Value::Table(llm));
    if let Some(l) = language {
        let mut p = table(&mut t, "prompt")?;
        p.insert("language".into(), toml::Value::String(l.into()));
        t.insert("prompt".into(), toml::Value::Table(p));
    }
    if t == before {
        return Ok(None);
    }
    toml::to_string(&t).map(Some).map_err(|e| e.to_string())
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    evorag_store::fsutil::prepare_private_file(path)?;
    std::fs::write(path, text)
}

pub fn run(
    c: &Catalog,
    config_file: &Path,
    cfg: &Config,
    data_root: &Path,
    rt: &tokio::runtime::Runtime,
    args: &SetupArgs,
) -> ExitCode {
    let say =
        |k: MessageKey, a: &[(&str, &str)]| eprintln!("{}", c.with(k, a).replace("\\n", "\n"));
    // 探す順: 指定 → 設定 → 既定。
    let mut urls: Vec<String> = Vec::new();
    for u in args
        .url
        .iter()
        .chain(cfg.llm.chat.url.iter())
        .map(String::as_str)
        .chain(CANDIDATES.iter().copied())
    {
        if !urls.iter().any(|x| x == u) {
            urls.push(u.to_owned());
        }
        if args.url.is_some() {
            break; // 指定したときは、その接続先だけを見る。
        }
    }
    let key_env = args
        .api_key_env
        .clone()
        .or_else(|| cfg.llm.chat.api_key_env.clone());
    let key = cfg
        .llm
        .chat
        .api_key
        .clone()
        .or_else(|| key_env.as_ref().and_then(|v| std::env::var(v).ok()));

    let mut found = None;
    for u in &urls {
        match probe(rt, u, key.clone(), cfg.llm.allow_remote) {
            Probe::Found(info) => {
                found = Some((u.clone(), info));
                break;
            }
            Probe::NeedsKey => {
                say(MessageKey::CliSetupNeedsKey, &[("url", u)]);
                return ExitCode::from(3);
            }
            Probe::Missing => {}
        }
    }
    let Some((url, info)) = found else {
        say(MessageKey::CliSetupGuide, &[("tried", &urls.join(", "))]);
        return ExitCode::from(3);
    };
    say(
        MessageKey::CliSetupFound,
        &[
            ("url", &url),
            ("model", &info.model_name),
            ("n_ctx", &info.n_ctx.to_string()),
            ("build", &info.build_info),
        ],
    );

    // プロンプトの言語: 設定に無く、データ根にも記録が無ければ尋ねる (07 §6.1)。
    let language = if cfg.prompt.language.is_some() || language_recorded(data_root) {
        None
    } else if args.yes {
        Some("ja")
    } else {
        loop {
            match ask(&c.key(MessageKey::CliSetupAskLanguage)).as_str() {
                "" | "ja" => break Some("ja"),
                "en" => break Some("en"),
                _ => {}
            }
        }
    };

    let path = config_file.display().to_string();
    let result = match std::fs::read_to_string(config_file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_private(
            config_file,
            &template(&url, args.api_key_env.as_deref(), language),
        ),
        Err(e) => Err(e),
        Ok(text) => match updated(&text, &url, args.api_key_env.as_deref(), language) {
            Err(e) => {
                say(MessageKey::CliConfigError, &[("detail", &e)]);
                return ExitCode::from(4);
            }
            Ok(None) => {
                say(MessageKey::CliSetupUnchanged, &[("path", &path)]);
                return ExitCode::SUCCESS;
            }
            Ok(Some(new)) => {
                let backup = config_file.with_extension(format!(
                    "toml.bak-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs())
                ));
                let shown = backup.display().to_string();
                let yes = args.yes || {
                    let a = ask(&c.with(
                        MessageKey::CliSetupAskOverwrite,
                        &[("path", &path), ("backup", &shown)],
                    ));
                    matches!(a.as_str(), "y" | "Y" | "yes")
                };
                if !yes {
                    say(MessageKey::CliSetupNotWritten, &[("path", &path)]);
                    return ExitCode::SUCCESS;
                }
                std::fs::copy(config_file, &backup).and_then(|_| write_private(config_file, &new))
            }
        },
    };
    match result {
        Ok(()) => {
            say(MessageKey::CliSetupWritten, &[("path", &path)]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            say(MessageKey::CliError, &[("detail", &e.to_string())]);
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_config() {
        let t = template("http://127.0.0.1:8080", None, Some("ja"));
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(l.config.prompt.language.as_deref(), Some("ja"));
        let t = template("http://127.0.0.1:8081", Some("K"), None);
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(l.config.llm.chat.api_key_env.as_deref(), Some("K"));
    }

    #[test]
    fn updating_keeps_other_keys_and_reports_no_change() {
        let text = "version = 1\n[llm.chat]\nurl = \"http://127.0.0.1:8080\"\n[storage]\nallow_unsafe_location = true\n";
        assert_eq!(
            updated(text, "http://127.0.0.1:8080", None, None).unwrap(),
            None
        );
        let new = updated(text, "http://127.0.0.1:8081", None, Some("en"))
            .unwrap()
            .unwrap();
        let l = crate::config::parse(&new).unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8081")
        );
        assert_eq!(l.config.storage.allow_unsafe_location, Some(true));
        assert_eq!(l.config.prompt.language.as_deref(), Some("en"));
    }
}
