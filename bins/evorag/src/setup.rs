//! `evorag setup` (07 §5、FR-510・FR-513、06 §3.2): llama-server を探し、能力を確かめ、プロンプトの
//! 言語を尋ね (新しいデータ根のとき、07 §6.1)、`config.toml` を書き、役割ごとのモデルを記録する。
//!
//! 1. 最初にデータ根のロックを確かめる。TUI が動いていれば、設定だけを書けることを先に示す。
//! 2. 探すのはループバックだけ (`--url`、設定の接続先、`127.0.0.1:8080`〜`:8082`)。役割は API で
//!    見分ける (`/v1/embeddings` と `/v1/rerank` に答えるか、S-02)。足りない役割には起動の例を示す。
//!    起動例には `--host 127.0.0.1` と `--api-key` を付ける (06 §9)。
//! 3. chat が見つからなければ、入手と起動の手順を示し、設定ファイルを書かない (AC-FR-510-1)。
//!    既にある設定ファイルを書き換えるときは、確かめてから、元のファイルを写して残す。
//!    ルーターモード (1 つの URL で複数のモデル、06 §2) を見つけたら、モデルを 1 つずつ確かめて役割を
//!    見分け、役割ごとに候補が複数あれば選ばせる。設定には URL とモデルの名前を書く。
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
    /// ルーターモード。モデルの名前の一覧。
    Router(Vec<String>),
    NeedsKey,
    Missing,
}

/// 見つけた役割の接続先 (ルーターなら `model` にモデルの名前)。
struct Found {
    role: &'static str,
    url: String,
    model: Option<String>,
    info: ServerInfo,
    client: Arc<HttpClient>,
}

fn probe(rt: &tokio::runtime::Runtime, url: &str, key: Option<String>, remote: bool) -> Probe {
    connect(rt, url, key, remote, None, Duration::from_secs(5))
}

/// 繋いで能力を読む。`model` はルーターのモデルの名前 (読み込みを待つので、締切を長くする)。
fn connect(
    rt: &tokio::runtime::Runtime,
    url: &str,
    key: Option<String>,
    remote: bool,
    model: Option<&str>,
    limit: Duration,
) -> Probe {
    let Ok(endpoint) = Endpoint::new(url, key, remote) else {
        return Probe::Missing;
    };
    // 探すときは再試行しない (無い接続先で待たない)。
    let Ok(client) = HttpClient::new(endpoint, RetryPolicy { delays: Vec::new() }) else {
        return Probe::Missing;
    };
    let client = Arc::new(client.with_model(model.map(str::to_owned)));
    let r = rt.block_on(async {
        tokio::time::timeout(limit, async {
            client.health().await?;
            client.server_info().await
        })
        .await
    });
    match r {
        Ok(Ok(info)) => Probe::Found(client, info),
        Ok(Err(LlmError::RouterNeedsModel { models })) => Probe::Router(models),
        Ok(Err(LlmError::Http { status, .. })) if status == 401 || status == 403 => Probe::NeedsKey,
        _ => Probe::Missing,
    }
}

/// ルーターのモデルを 1 つずつ確かめて、役割を見分ける (06 §2)。読み込んでいないモデルは読み込む。
fn router_roles(
    rt: &tokio::runtime::Runtime,
    c: &Catalog,
    url: &str,
    key: Option<String>,
    remote: bool,
    models: &[String],
) -> Vec<Found> {
    let mut out = Vec::new();
    for m in models {
        eprintln!(
            "{}",
            c.with(MessageKey::CliSetupRouterChecking, &[("model", m)])
        );
        if let Probe::Found(client, info) = connect(
            rt,
            url,
            key.clone(),
            remote,
            Some(m),
            Duration::from_secs(150),
        ) {
            let role = role_of(rt, &client);
            out.push(Found {
                role,
                url: url.to_owned(),
                model: Some(m.clone()),
                info,
                client,
            });
        }
    }
    out
}

/// 役割の候補から 1 つを選ぶ。設定に書いた名前があればそれ、1 つならそれ。複数なら、`--yes` では
/// 先頭を、対話では番号で選ばせる。`None` は候補が無い。
fn choose(
    cands: &[&Found],
    preferred: Option<&str>,
    ask_number: impl Fn() -> usize,
) -> Option<usize> {
    if cands.is_empty() {
        return None;
    }
    if let Some(i) =
        preferred.and_then(|p| cands.iter().position(|f| f.model.as_deref() == Some(p)))
    {
        return Some(i);
    }
    if cands.len() == 1 {
        return Some(0);
    }
    Some(ask_number().min(cands.len() - 1))
}

/// 役割の候補を示して、番号で選ばせる (1 から)。`--yes` では 0。
fn ask_choice(c: &Catalog, role: &str, cands: &[&Found], yes: bool) -> usize {
    if yes {
        return 0;
    }
    let list = cands
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}) {}", i + 1, f.model.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("  ");
    loop {
        let a = ask(&c.with(
            MessageKey::CliSetupRouterChoose,
            &[("role", role), ("choices", &list)],
        ));
        if let Ok(n) = a.parse::<usize>()
            && (1..=cands.len()).contains(&n)
        {
            return n - 1;
        }
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
/// `chat_model` と `aux` の名前は、ルーターモードのモデルの名前 (06 §2)。
fn template(
    url: &str,
    chat_model: Option<&str>,
    api_key_env: Option<&str>,
    language: Option<&str>,
    aux: &[(&str, &str, Option<&str>)],
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
    if let Some(m) = chat_model {
        s.push_str(&format!(
            "# ルーターモードのモデルの名前 (プリセットの節の名前)\nmodel = \"{m}\"\n"
        ));
    }
    match api_key_env {
        Some(v) => s.push_str(&format!("api_key_env = \"{v}\"\n")),
        None => s.push_str("# llama-server を --api-key で起動したときは、キーを入れた環境変数の名前を書く\n# api_key_env = \"EVORAG_API_KEY\"\n"),
    }
    for (role, u, m) in aux {
        s.push_str(&format!("\n[llm.{role}]\nurl = \"{u}\"\n"));
        if let Some(m) = m {
            s.push_str(&format!("model = \"{m}\"\n"));
        }
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
    chat_model: Option<&str>,
    api_key_env: Option<&str>,
    language: Option<&str>,
    aux: &[(&str, &str, Option<&str>)],
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
    if let Some(m) = chat_model {
        chat.insert("model".into(), toml::Value::String(m.into()));
    }
    if let Some(v) = api_key_env {
        chat.insert("api_key_env".into(), toml::Value::String(v.into()));
    }
    llm.insert("chat".into(), toml::Value::Table(chat));
    for (role, u, m) in aux {
        let mut r = table(&mut llm, role)?;
        r.insert("url".into(), toml::Value::String((*u).into()));
        match m {
            Some(m) => r.insert("model".into(), toml::Value::String((*m).into())),
            // ルーターでない接続先に変えたときは、前のルーターの名前を残さない。
            None => r.remove("model"),
        };
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

    let mut found: Option<Found> = None;
    // ルーターで見分けた役割の候補 (chat の接続先がルーターだったとき)。
    let mut routed: Vec<Found> = Vec::new();
    for u in &urls {
        match probe(rt, u, key.clone(), cfg.llm.allow_remote) {
            Probe::Found(client, info) if role_of(rt, &client) == "chat" => {
                found = Some(Found {
                    role: "chat",
                    url: u.clone(),
                    model: None,
                    info,
                    client,
                });
                break;
            }
            Probe::Found(..) => {}
            Probe::Router(models) => {
                say(
                    MessageKey::CliSetupRouterFound,
                    &[("url", u), ("models", &models.join(", "))],
                );
                let cands = router_roles(rt, c, u, key.clone(), cfg.llm.allow_remote, &models);
                let chats: Vec<&Found> = cands.iter().filter(|f| f.role == "chat").collect();
                let pick = choose(&chats, cfg.llm.chat.model.as_deref(), || {
                    ask_choice(c, "chat", &chats, args.yes)
                });
                if let Some(i) = pick {
                    let model = chats[i].model.clone();
                    routed = cands;
                    let at = routed
                        .iter()
                        .position(|f| f.role == "chat" && f.model == model)
                        .unwrap_or_default();
                    found = Some(routed.remove(at));
                    break;
                }
            }
            Probe::NeedsKey => {
                say(MessageKey::CliSetupNeedsKey, &[("url", u)]);
                return ExitCode::from(3);
            }
            Probe::Missing => {}
        }
    }
    let Some(chat) = found else {
        say(MessageKey::CliSetupGuide, &[("tried", &urls.join(", "))]);
        return ExitCode::from(3);
    };
    let (url, info) = (chat.url.clone(), chat.info.clone());
    say(
        MessageKey::CliSetupFound,
        &[
            ("url", &url),
            ("model", &info.model_name),
            ("n_ctx", &info.n_ctx.to_string()),
            ("build", &info.build_info),
        ],
    );

    // embed と rerank を探す。ルーターなら、そのモデルから選ぶ。無ければ設定の接続先 → 既定を探す
    // (chat の接続先は見ない)。
    let mut aux_found: Vec<Found> = Vec::new();
    for role in ["embed", "rerank"] {
        let configured = cfg.role(role);
        let cands: Vec<&Found> = routed.iter().filter(|f| f.role == role).collect();
        if !cands.is_empty() {
            let preferred = configured.as_ref().and_then(|r| r.model.clone());
            if let Some(i) = choose(&cands, preferred.as_deref(), || {
                ask_choice(c, role, &cands, args.yes)
            }) {
                let model = cands[i].model.clone();
                let at = routed
                    .iter()
                    .position(|f| f.role == role && f.model == model)
                    .unwrap_or_default();
                aux_found.push(routed.remove(at));
                continue;
            }
        }
        let role_key = configured
            .as_ref()
            .and_then(|r| r.api_key.clone())
            .or_else(|| key.clone());
        let mut tried: Vec<String> = configured.iter().map(|r| r.url.clone()).collect();
        tried.extend(AUX_CANDIDATES.iter().map(|s| (*s).to_owned()));
        tried.dedup();
        for u in tried.iter().filter(|u| **u != url) {
            if aux_found.iter().any(|f| f.url == *u) {
                continue;
            }
            if let Probe::Found(client, info) = probe(rt, u, role_key.clone(), cfg.llm.allow_remote)
            {
                let r = role_of(rt, &client);
                if r == role {
                    aux_found.push(Found {
                        role,
                        url: u.clone(),
                        model: None,
                        info,
                        client,
                    });
                    break;
                }
                if r == "chat" && role == "embed" {
                    // v0.1 の /model の候補だった chat のサーバかもしれない (06 §3.2 の 2)。
                    say(MessageKey::CliSetupExtraChat, &[("url", u)]);
                }
            }
        }
    }
    let mut missing = false;
    for role in ["embed", "rerank"] {
        match aux_found.iter().find(|f| f.role == role) {
            Some(f) => say(
                MessageKey::CliSetupRoleFound,
                &[
                    ("role", role),
                    ("url", &f.url),
                    ("file", evorag_llm::file_name(&f.info.model_path)),
                ],
            ),
            None => {
                missing = true;
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
    if missing {
        // 3 つの役割を 1 つのプロセスで動かす手段 (S-15 §2)。
        say(MessageKey::CliSetupPresetExample, &[]);
    }
    // 設定に書く役割 (設定の接続先・名前と違うものだけ)。
    let aux_targets: Vec<(&str, &str, Option<&str>)> = aux_found
        .iter()
        .filter(|f| {
            cfg.role(f.role)
                .is_none_or(|x| x.url != f.url || x.model != f.model)
        })
        .map(|f| (f.role, f.url.as_str(), f.model.as_deref()))
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
            &template(
                &url,
                chat.model.as_deref(),
                args.api_key_env.as_deref(),
                language,
                &aux_targets,
            ),
        )
        .map(|()| true),
        Err(e) => Err(e),
        Ok(text) => match updated(
            &text,
            &url,
            chat.model.as_deref(),
            args.api_key_env.as_deref(),
            language,
            &aux_targets,
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
    let mut roles: Vec<(&str, &ServerInfo, &Arc<HttpClient>)> = vec![("chat", &info, &chat.client)];
    roles.extend(aux_found.iter().map(|f| (f.role, &f.info, &f.client)));
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
    let rerank_use = usable_roles
        .iter()
        .find(|(r, _, _)| *r == "rerank")
        .map(|(_, o, c)| (o.model_key.clone(), Arc::clone(c)));
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

    // 6. リランカーのしきい値を、同梱の事例で求める (05 §3.7、FR-413)。今の事例で求めた記録が
    //    あれば行わない (前のリランカーに戻したときは、その記録を使う)。
    if let Some((key, client)) = rerank_use {
        let lang = models.prompt_language().unwrap_or_else(|| "ja".to_owned());
        let prev = models.threshold(&key);
        if !prev
            .as_ref()
            .is_some_and(|b| evorag_core::calibration::is_current(b, &lang))
        {
            let version = prev.map_or(1, |b| b.version + 1);
            let mut progress = |done: usize, total: usize| {
                if done.is_multiple_of(10) || done == total {
                    say(
                        MessageKey::CliSetupCalibrating,
                        &[("done", &done.to_string()), ("total", &total.to_string())],
                    );
                }
            };
            let result = rt.block_on(evorag_core::calibration::calibrate(
                &*client,
                &key,
                &lang,
                version,
                &std::sync::Mutex::default(),
                &mut progress,
            ));
            match result {
                Ok(body) => {
                    let stat = |k: &str| body.stats.get(k).copied().unwrap_or(0).to_string();
                    let counts = [
                        ("rank_ok", stat("rank_ok")),
                        ("rank_n", stat("rank_n")),
                        ("value_ok", stat("value_ok")),
                        ("value_n", stat("value_n")),
                    ];
                    let mut args: Vec<(&str, &str)> =
                        counts.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    let (thr, band) = (
                        format!("{:.2}", body.threshold.unwrap_or(f64::NAN)),
                        format!("{:.2}", body.band_low.unwrap_or(f64::NAN)),
                    );
                    if body.passed {
                        args.push(("threshold", &thr));
                        args.push(("band", &band));
                        say(MessageKey::CliSetupCalibrated, &args);
                    } else {
                        say(MessageKey::CliSetupCalibrationFailed, &args);
                    }
                    if let Err(e) = models.save_threshold(&body) {
                        say(MessageKey::CliError, &[("detail", &e)]);
                    }
                }
                Err(e) => say(MessageKey::CliError, &[("detail", &e.to_string())]),
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_config() {
        let t = template("http://127.0.0.1:8080", None, None, Some("ja"), &[]);
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(
            l.config.llm.chat.url.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(l.config.prompt.language.as_deref(), Some("ja"));
        let t = template(
            "http://127.0.0.1:8081",
            None,
            Some("K"),
            None,
            &[("embed", "http://127.0.0.1:8082", None)],
        );
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(l.config.llm.chat.api_key_env.as_deref(), Some("K"));
        let e = l.config.role("embed").unwrap();
        assert_eq!(e.url, "http://127.0.0.1:8082");
        assert!(l.config.role("rerank").is_none());
    }

    /// verifies: AC-FR-511-2 (ルーターのモデルの名前を、役割ごとに設定に書く)
    #[test]
    fn a_router_is_written_with_a_model_name_per_role() {
        let r = "http://127.0.0.1:8080";
        let t = template(
            r,
            Some("chat"),
            None,
            Some("ja"),
            &[("embed", r, Some("emb")), ("rerank", r, Some("rr"))],
        );
        let l = crate::config::parse(&t).unwrap();
        assert_eq!(l.config.llm.chat.model.as_deref(), Some("chat"));
        assert_eq!(
            l.config.role("embed").unwrap().model.as_deref(),
            Some("emb")
        );
        assert_eq!(l.config.role("rerank").unwrap().url, r);
        // 直接繋ぐ構成に戻すと、役割の名前は消す (chat の名前は表示に使うので残す)。
        let back = updated(
            &t,
            r,
            None,
            None,
            None,
            &[("embed", "http://127.0.0.1:8081", None)],
        )
        .unwrap()
        .unwrap();
        let l = crate::config::parse(&back).unwrap();
        assert_eq!(l.config.role("embed").unwrap().model, None);
        assert_eq!(l.config.llm.chat.model.as_deref(), Some("chat"));
    }

    fn found(role: &'static str, model: &str) -> Found {
        Found {
            role,
            url: "http://127.0.0.1:8080".into(),
            model: Some(model.into()),
            info: ServerInfo {
                build_info: String::new(),
                model_path: String::new(),
                model_name: model.into(),
                total_slots: 1,
                n_ctx: 8192,
                meta: Default::default(),
                model_key: String::new(),
            },
            client: Arc::new(
                HttpClient::new(
                    Endpoint::new("http://127.0.0.1:8080", None, false).unwrap(),
                    RetryPolicy::default(),
                )
                .unwrap(),
            ),
        }
    }

    /// verifies: AC-FR-511-2 (候補が複数のときだけ選ばせ、設定に書いた名前を先に採る)
    #[test]
    fn a_role_is_chosen_from_the_router_candidates() {
        let (a, b) = (found("chat", "a"), found("chat", "b"));
        let none: Vec<&Found> = Vec::new();
        assert_eq!(choose(&none, None, || unreachable!()), None);
        assert_eq!(choose(&[&a], None, || unreachable!()), Some(0));
        assert_eq!(choose(&[&a, &b], Some("b"), || unreachable!()), Some(1));
        assert_eq!(choose(&[&a, &b], None, || 1), Some(1));
        assert_eq!(choose(&[&a, &b], Some("zz"), || 0), Some(0));
    }

    #[test]
    fn updating_keeps_other_keys_and_reports_no_change() {
        let text = "version = 1\n[llm.chat]\nurl = \"http://127.0.0.1:8080\"\n[storage]\nallow_unsafe_location = true\n";
        assert_eq!(
            updated(text, "http://127.0.0.1:8080", None, None, None, &[]).unwrap(),
            None
        );
        let new = updated(
            text,
            "http://127.0.0.1:8081",
            None,
            None,
            Some("en"),
            &[("rerank", "http://127.0.0.1:8082", None)],
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
