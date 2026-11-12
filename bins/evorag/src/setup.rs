//! `evorag setup` (07 §5、FR-510・FR-513、06 §3.2): llama-server を探し、能力を確かめ、プロンプトの
//! 言語を尋ね (新しいデータ根のとき、07 §6.1)、`config.toml` を書き、役割ごとのモデルを記録する。
//!
//! 1. 最初にデータ根のロックを確かめる。TUI が動いていれば、設定だけを書けることを先に示す。
//! 2. 探すのはループバックだけ (`--url`、設定の接続先、`127.0.0.1:8080`〜`:8082`)。役割は API で
//!    見分ける (`/v1/embeddings` と `/v1/rerank` に答えるか、S-02)。足りない役割には起動の例を示す。
//!    起動例には `--host 127.0.0.1` と `--api-key` を付ける (06 §9)。
//! 3. chat が見つからなければ、入手と起動の手順を示し、設定ファイルを書かない (AC-FR-510-1)。
//!    既にある設定ファイルを書き換えるときは、確かめてから、元のファイルを写して残す。
//! 4. 役割ごとのモデルを、データ根の記録と比べる。記録が無ければ記録する。違えば、違いの中身と
//!    やり直しになるものを示し、利用者が対話で確かめたときだけ書き換える (`--yes` では書き換えない)。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use evorag_core::rebind::{DataRootModels, RoleDiff};
use evorag_llm::{Endpoint, HttpClient, LlmClient, LlmError, RetryPolicy, ServerInfo};
use evorag_tui::catalog::Catalog;
use evorag_types::{MessageKey, SystemClock, UuidV7Gen};

use crate::config::Config;

/// chat を探す接続先の既定。
const CANDIDATES: &[&str] = &["http://127.0.0.1:8080", "http://127.0.0.1:8081"];
/// embed と rerank を探す接続先の既定 (06 §3.2 の 2)。
const AUX_CANDIDATES: &[&str] = &[
    "http://127.0.0.1:8080",
    "http://127.0.0.1:8081",
    "http://127.0.0.1:8082",
];

pub struct SetupArgs {
    pub url: Option<String>,
    pub api_key_env: Option<String>,
    pub yes: bool,
}

enum Probe {
    Found(Arc<HttpClient>, ServerInfo),
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
    let client = Arc::new(client);
    let r = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            client.health().await?;
            client.server_info().await
        })
        .await
    });
    match r {
        Ok(Ok(info)) => Probe::Found(client, info),
        Ok(Err(LlmError::Http { status, .. })) if status == 401 || status == 403 => Probe::NeedsKey,
        _ => Probe::Missing,
    }
}

/// サーバの役割を、API に答えるかで見分ける (chat のサーバは埋め込みとリランクを 501 で断る、S-02)。
fn role_of(rt: &tokio::runtime::Runtime, client: &HttpClient) -> &'static str {
    rt.block_on(async {
        let t = Duration::from_secs(10);
        if matches!(
            tokio::time::timeout(t, client.embed(&["x".to_owned()])).await,
            Ok(Ok(_))
        ) {
            return "embed";
        }
        if matches!(
            tokio::time::timeout(t, client.rerank("x", &["y".to_owned()])).await,
            Ok(Ok(_))
        ) {
            return "rerank";
        }
        "chat"
    })
}

/// 新しいデータ根か (プロンプトの言語がまだ記録されていない)。
fn language_recorded(data_root: &Path) -> bool {
    evorag_store::inspect::inspect(data_root, evorag_memory::kinds(), None)
        .ok()
        .is_some_and(|i| i.meta.contains_key(evorag_store::meta::PROMPT_LANGUAGE))
}

/// データ根を、ほかの evorag (TUI) が使っているか。
fn in_use(data_root: &Path) -> bool {
    data_root.join(evorag_store::DB_FILE).exists()
        && matches!(
            evorag_store::lock::DataRootLock::acquire_quiet(data_root),
            Err(evorag_store::lock::LockError::InUse { .. })
        )
}

fn ask(prompt: &str) -> String {
    use std::io::{BufRead, Write};
    eprint!("{prompt} ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_owned()
}

fn yes_no(prompt: &str) -> bool {
    matches!(ask(prompt).as_str(), "y" | "Y" | "yes")
}

/// 新しく書く設定ファイルの中身 (説明つき)。
fn template(
    url: &str,
    api_key_env: Option<&str>,
    language: Option<&str>,
    aux: &[(&str, &str)],
) -> String {
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
    for (role, u) in aux {
        s.push_str(&format!("\n[llm.{role}]\nurl = \"{u}\"\n"));
        if let Some(v) = api_key_env {
            s.push_str(&format!("api_key_env = \"{v}\"\n"));
        }
    }
    s
}

/// 既にある設定に、見つけた接続先などを当てる。変わらなければ `None`。
fn updated(
    text: &str,
    url: &str,
    api_key_env: Option<&str>,
    language: Option<&str>,
    aux: &[(&str, &str)],
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
    for (role, u) in aux {
        let mut r = table(&mut llm, role)?;
        r.insert("url".into(), toml::Value::String((*u).into()));
        llm.insert((*role).into(), toml::Value::Table(r));
    }
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

    // 1. ロックを先に確かめる (06 §3.2 の 1)。
    let config_only = in_use(data_root);
    if config_only {
        say(MessageKey::CliSetupTuiRunning, &[]);
    }

    // 2. chat を探す順: 指定 → 設定 → 既定。
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
            Probe::Found(client, info) if role_of(rt, &client) == "chat" => {
                found = Some((u.clone(), info, client));
                break;
            }
            Probe::Found(..) => {}
            Probe::NeedsKey => {
                say(MessageKey::CliSetupNeedsKey, &[("url", u)]);
                return ExitCode::from(3);
            }
            Probe::Missing => {}
        }
    }
    let Some((url, info, chat_client)) = found else {
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

    // embed と rerank を探す (設定の接続先 → 既定)。chat の接続先は見ない。
    let mut aux_found: Vec<(&'static str, String, ServerInfo, Arc<HttpClient>)> = Vec::new();
    for role in ["embed", "rerank"] {
        let configured = cfg.role(role);
        let role_key = configured
            .as_ref()
            .and_then(|r| r.api_key.clone())
            .or_else(|| key.clone());
        let mut tried: Vec<String> = configured.iter().map(|r| r.url.clone()).collect();
        tried.extend(AUX_CANDIDATES.iter().map(|s| (*s).to_owned()));
        tried.dedup();
        for u in tried.iter().filter(|u| **u != url) {
            if aux_found.iter().any(|(_, x, _, _)| x == u) {
                continue;
            }
            if let Probe::Found(client, info) = probe(rt, u, role_key.clone(), cfg.llm.allow_remote)
            {
                let r = role_of(rt, &client);
                if r == role {
                    aux_found.push((role, u.clone(), info, client));
                    break;
                }
                if r == "chat" && role == "embed" {
                    // v0.1 の /model の候補だった chat のサーバかもしれない (06 §3.2 の 2)。
                    say(MessageKey::CliSetupExtraChat, &[("url", u)]);
                }
            }
        }
    }
    for role in ["embed", "rerank"] {
        match aux_found.iter().find(|(r, _, _, _)| *r == role) {
            Some((_, u, i, _)) => say(
                MessageKey::CliSetupRoleFound,
                &[
                    ("role", role),
                    ("url", u),
                    ("file", evorag_llm::file_name(&i.model_path)),
                ],
            ),
            None => {
                let example = if role == "embed" {
                    MessageKey::CliSetupEmbedExample
                } else {
                    MessageKey::CliSetupRerankExample
                };
                say(
                    MessageKey::CliSetupRoleMissing,
                    &[("role", role), ("example", &c.key(example))],
                );
            }
        }
    }
    let aux_urls: Vec<(&str, &str)> = aux_found
        .iter()
        .map(|(r, u, _, _)| (*r, u.as_str()))
        .filter(|(r, u)| cfg.role(r).map(|x| x.url) != Some((*u).to_owned()))
        .collect();

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

    // 3. 設定ファイルを書く。
    let path = config_file.display().to_string();
    let written = match std::fs::read_to_string(config_file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_private(
            config_file,
            &template(&url, args.api_key_env.as_deref(), language, &aux_urls),
        )
        .map(|()| true),
        Err(e) => Err(e),
        Ok(text) => match updated(
            &text,
            &url,
            args.api_key_env.as_deref(),
            language,
            &aux_urls,
        ) {
            Err(e) => {
                say(MessageKey::CliConfigError, &[("detail", &e)]);
                return ExitCode::from(4);
            }
            Ok(None) => {
                say(MessageKey::CliSetupUnchanged, &[("path", &path)]);
                Ok(false)
            }
            Ok(Some(new)) => {
                let backup = config_file.with_extension(format!(
                    "toml.bak-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs())
                ));
                let shown = backup.display().to_string();
                let yes = args.yes
                    || yes_no(&c.with(
                        MessageKey::CliSetupAskOverwrite,
                        &[("path", &path), ("backup", &shown)],
                    ));
                if yes {
                    std::fs::copy(config_file, &backup)
                        .and_then(|_| write_private(config_file, &new))
                        .map(|()| true)
                } else {
                    say(MessageKey::CliSetupNotWritten, &[("path", &path)]);
                    Ok(false)
                }
            }
        },
    };
    match written {
        Ok(true) => say(MessageKey::CliSetupWritten, &[("path", &path)]),
        Ok(false) => {}
        Err(e) => {
            say(MessageKey::CliError, &[("detail", &e.to_string())]);
            return ExitCode::from(1);
        }
    }

    // 4. 役割ごとのモデルを、データ根の記録と比べる (06 §3.2)。新しいデータ根では、最初の起動で記録する。
    if config_only || !data_root.join(evorag_store::DB_FILE).exists() {
        return ExitCode::SUCCESS;
    }
    let clock = Arc::new(SystemClock);
    let models = match DataRootModels::open(
        data_root,
        env!("CARGO_PKG_VERSION"),
        clock.clone(),
        Arc::new(UuidV7Gen::new(clock)),
    ) {
        Ok(m) => m,
        Err(e) => {
            say(
                MessageKey::CliDataRootUnusable,
                &[("detail", &e.to_string())],
            );
            return ExitCode::from(2);
        }
    };
    let recorded = models.recorded();
    let mut roles: Vec<(&str, &ServerInfo, &Arc<HttpClient>)> = vec![("chat", &info, &chat_client)];
    roles.extend(aux_found.iter().map(|(r, _, i, c)| (*r, i, c)));
    // 記録と合っている役割 (このあと点検する)。
    let mut usable_roles: Vec<(&str, evorag_memory::ModelOrigin, &Arc<HttpClient>)> = Vec::new();
    for (role, info, client) in roles {
        let now = evorag_core::origin(info);
        let file = now.file_name.clone();
        let bind = |what: MessageKey| match models.bind(role, now.clone()) {
            Ok(()) => {
                say(what, &[("role", role), ("file", &file)]);
                true
            }
            Err(e) => {
                say(MessageKey::CliError, &[("detail", &e)]);
                false
            }
        };
        let usable = match evorag_core::rebind::diff(role, recorded.get(role), &now) {
            RoleDiff::Same => true,
            RoleDiff::New => bind(MessageKey::CliSetupModelRecorded),
            RoleDiff::Different {
                recorded,
                maybe_same,
            } => {
                say(
                    MessageKey::CliSetupModelDiff,
                    &[
                        ("role", role),
                        ("recorded", &recorded.file_name),
                        ("connected", &file),
                    ],
                );
                let redo = match role {
                    "chat" => MessageKey::CliSetupRedoChat,
                    "embed" => MessageKey::CliSetupRedoEmbed,
                    _ => MessageKey::CliSetupRedoRerank,
                };
                say(redo, &[]);
                let switched = if args.yes {
                    say(MessageKey::CliSetupNeedsConfirm, &[]);
                    false
                } else if maybe_same && yes_no(&c.key(MessageKey::CliSetupAskSame)) {
                    true
                } else {
                    yes_no(&c.with(MessageKey::CliSetupAskSwitch, &[("role", role)]))
                };
                if switched {
                    bind(MessageKey::CliSetupSwitched)
                } else if role == "chat" {
                    say(
                        MessageKey::CliSetupKeptChat,
                        &[("recorded", &recorded.file_name)],
                    );
                    false
                } else {
                    say(MessageKey::CliSetupKept, &[("role", role)]);
                    false
                }
            }
        };
        if usable {
            usable_roles.push((role, now, client));
        }
    }

    // 5. 使うモデルの性質を点検する (06 §3.3)。前回の点検と model_key・`build_info` が同じなら行わない。
    let aux_extra: evorag_llm::RequestExtra = cfg
        .llm
        .chat
        .aux_request_extra
        .as_ref()
        .and_then(|t| serde_json::to_value(t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (role, o, client) in usable_roles {
        if models.probed(role, &o.model_key, &o.build_info) {
            continue;
        }
        say(MessageKey::CliSetupProbing, &[("role", role)]);
        let body = rt.block_on(async {
            match role {
                "chat" => {
                    evorag_core::probe::chat(
                        &**client,
                        &o.model_key,
                        &o.build_info,
                        &aux_extra,
                        None,
                    )
                    .await
                }
                "embed" => evorag_core::probe::embed(&**client, &o.model_key, &o.build_info).await,
                _ => evorag_core::probe::rerank(&**client, &o.model_key, &o.build_info).await,
            }
        });
        for line in crate::doctor::probe_lines(c, role, &body) {
            eprintln!("{line}");
        }
        if let Err(e) = models.save_probe(&body) {
            say(MessageKey::CliError, &[("detail", &e)]);
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_config() {
        let t = template("http://127.0.0.1:8080", None, Some("ja"), &[]);
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(l.config.prompt.language.as_deref(), Some("ja"));
        let t = template(
            "http://127.0.0.1:8081",
            Some("K"),
            None,
            &[("embed", "http://127.0.0.1:8082")],
        );
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(l.config.llm.chat.api_key_env.as_deref(), Some("K"));
        let e = l.config.role("embed").unwrap();
        assert_eq!(e.url, "http://127.0.0.1:8082");
        assert!(l.config.role("rerank").is_none());
    }

    #[test]
    fn updating_keeps_other_keys_and_reports_no_change() {
        let text = "version = 1\n[llm.chat]\nurl = \"http://127.0.0.1:8080\"\n[storage]\nallow_unsafe_location = true\n";
        assert_eq!(
            updated(text, "http://127.0.0.1:8080", None, None, &[]).unwrap(),
            None
        );
        let new = updated(
            text,
            "http://127.0.0.1:8081",
            None,
            Some("en"),
            &[("rerank", "http://127.0.0.1:8082")],
        )
        .unwrap()
        .unwrap();
        let l = crate::config::parse(&new).unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8081")
        );
        assert_eq!(l.config.storage.allow_unsafe_location, Some(true));
        assert_eq!(l.config.prompt.language.as_deref(), Some("en"));
        assert_eq!(
            l.config.role("rerank").unwrap().url,
            "http://127.0.0.1:8082"
        );
    }
}
