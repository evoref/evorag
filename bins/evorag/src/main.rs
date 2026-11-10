//! サブコマンドの入口 (07 §5)。v0.1 の M1.4 は、TUI を開く `evorag` だけ。
//!
//! 終了コード: 0 正常、1 一般のエラー、2 データ根が使えない (ロック・新しい形式)、
//! 3 llama-server に繋がらない、4 設定の誤り (07 §5)。

mod config;
mod doctor;
mod export;
mod setup;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use evorag_core::{Core, CoreConfig, PromptLanguage, StartError};
use evorag_llm::{Endpoint, Generation, HttpClient, RetryPolicy};
use evorag_store::OpenError;
use evorag_store::lock::LockError;
use evorag_tui::app::{App, Status};
use evorag_tui::catalog::Catalog;
use evorag_types::{Clock, MessageKey, SystemClock, UuidV7Gen};

const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// `--version` (FR-906): 版・作りの情報・入れ方。
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "
commit: ",
    env!("EVORAG_COMMIT"),
    "
target: ",
    env!("EVORAG_TARGET"),
    "
rustc: ",
    env!("EVORAG_RUSTC"),
    "
installed via: ",
    env!("EVORAG_CHANNEL"),
);

#[derive(Parser)]
#[command(
    name = "evorag",
    version = VERSION,
    about = "Chat with a local llama-server, with long-term memory"
)]
struct Args {
    /// データ根 (設定の storage.data_root より優先する)
    #[arg(long, value_name = "PATH", global = true)]
    data_root: Option<PathBuf>,
    /// 設定ファイル
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// llama-server を探して能力を確かめ、設定ファイルを書く
    Setup {
        /// 接続先 (省くと、設定の llm.chat.url と 127.0.0.1:8080・8081 を探す)
        #[arg(long)]
        url: Option<String>,
        /// API キーを入れた環境変数の名前 (llama-server を --api-key で起動したとき)
        #[arg(long, value_name = "NAME")]
        api_key_env: Option<String>,
        /// 尋ねずに進める (プロンプトの言語は ja、設定ファイルは書き換える)
        #[arg(long)]
        yes: bool,
    },
    /// 設定・接続・データ根・ジャーナル・退避を、読むだけで点検する
    Doctor,
    /// 退避の操作
    Backups {
        #[command(subcommand)]
        action: BackupsAction,
    },
    /// 記録を種類ごとの JSON Lines に書き出す (設定は API キーを除いて写す)
    Export {
        /// 書き出すフォルダ (空か、まだ無いフォルダ)
        out: PathBuf,
    },
    /// 退避から戻す (その後の会話はジャーナルから足す)
    Restore {
        /// 退避のフォルダ (backups/daily/<日付> など) か、その中の evorag.redb
        backup: PathBuf,
    },
}

#[derive(clap::Subcommand)]
enum BackupsAction {
    /// 退避の一覧 (新しい順)
    List,
}

/// y / n を尋ねる。端末でなければ n。
fn ask_yes(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

/// データ根をほかの evorag が使っている (終了コード 2、NFR-604)。
pub(crate) fn data_root_in_use(
    catalog: &Catalog,
    data_root: &std::path::Path,
    pid: Option<u32>,
) -> ExitCode {
    let pid = pid.map_or_else(|| "?".to_owned(), |p| p.to_string());
    fail(
        catalog,
        2,
        MessageKey::CliDataRootInUse,
        &[("path", &data_root.display().to_string()), ("pid", &pid)],
    )
}

fn fail(catalog: &Catalog, code: u8, key: MessageKey, args: &[(&str, &str)]) -> ExitCode {
    eprintln!("{}", catalog.with(key, args).replace("\\n", "\n"));
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let args = Args::parse();
    let catalog = Catalog::load("ja");
    let places = config::default_places();
    let Some(config_file) = args
        .config
        .clone()
        .or_else(|| places.as_ref().map(|p| p.config_file.clone()))
    else {
        return fail(
            &catalog,
            1,
            MessageKey::CliError,
            &[("detail", "no home directory")],
        );
    };
    // doctor は読むだけ (設定の移行もしない)。
    let migrate = !matches!(args.command, Some(Sub::Doctor));
    let loaded = match config::load(&config_file, migrate) {
        Ok(l) => l,
        Err(e) => return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]),
    };
    if let Some(b) = &loaded.migrated {
        eprintln!(
            "{}",
            catalog.with(
                MessageKey::CliConfigMigrated,
                &[
                    ("path", &config_file.display().to_string()),
                    ("backup", &b.display().to_string())
                ]
            )
        );
    }
    for k in &loaded.ignored {
        eprintln!(
            "{}",
            catalog.with(MessageKey::CliConfigIgnored, &[("key", k)])
        );
    }
    let cfg = loaded.config;
    let Some(data_root) = args
        .data_root
        .or(cfg.storage.data_root.clone())
        .or_else(|| places.as_ref().map(|p| p.data_root.clone()))
    else {
        return fail(
            &catalog,
            1,
            MessageKey::CliError,
            &[("detail", "no home directory")],
        );
    };

    let url = cfg
        .llm
        .chat
        .url
        .clone()
        .unwrap_or_else(|| DEFAULT_URL.to_owned());
    let api_key = cfg.llm.chat.api_key.clone().or_else(|| {
        cfg.llm
            .chat
            .api_key_env
            .as_ref()
            .and_then(|v| std::env::var(v).ok())
    });
    let endpoint = match Endpoint::new(&url, api_key, cfg.llm.allow_remote) {
        Ok(e) => e,
        Err(e) => {
            return fail(
                &catalog,
                4,
                MessageKey::CliConfigError,
                &[("detail", &e.to_string())],
            );
        }
    };
    let llm = match HttpClient::new(endpoint, RetryPolicy::default()) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            return fail(
                &catalog,
                1,
                MessageKey::CliError,
                &[("detail", &e.to_string())],
            );
        }
    };

    let mut generation = Generation::default();
    let g = &cfg.llm.chat.generation;
    if let Some(t) = g.temperature {
        generation.temperature = t;
    }
    if let Some(p) = g.top_p {
        generation.top_p = p;
    }
    generation.max_tokens = g.max_tokens;
    generation.seed = g.seed;
    let prompt_language = cfg
        .prompt
        .language
        .as_deref()
        .and_then(PromptLanguage::parse)
        .unwrap_or(PromptLanguage::Ja);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return fail(
                &catalog,
                1,
                MessageKey::CliError,
                &[("detail", &e.to_string())],
            );
        }
    };
    let clock = Arc::new(SystemClock);
    let backup_dir = cfg.storage.backup_dir.clone();

    if let Some(Sub::Setup {
        url,
        api_key_env,
        yes,
    }) = &args.command
    {
        return setup::run(
            &catalog,
            &config_file,
            &cfg,
            &data_root,
            &rt,
            &setup::SetupArgs {
                url: url.clone(),
                api_key_env: api_key_env.clone(),
                yes: *yes,
            },
        );
    }
    if let Some(Sub::Doctor) = &args.command {
        return doctor::run(
            &catalog,
            &config_file,
            &data_root,
            backup_dir.as_deref(),
            &url,
            &rt,
            llm,
        );
    }
    if let Some(Sub::Backups {
        action: BackupsAction::List,
    }) = &args.command
    {
        return doctor::list_backups(&catalog, &data_root, backup_dir.as_deref());
    }
    if let Some(Sub::Export { out }) = &args.command {
        return export::run(
            &catalog,
            &data_root,
            &config_file,
            out,
            clock.now().as_millis(),
        );
    }
    if let Some(Sub::Restore { backup }) = &args.command {
        return match evorag_store::restore::restore(
            &data_root,
            backup,
            clock.now().as_millis(),
            Some(&config_file),
        ) {
            Ok(_) => {
                eprintln!(
                    "{}",
                    catalog.with(
                        MessageKey::CliRestored,
                        &[("from", &backup.display().to_string())]
                    )
                );
                ExitCode::SUCCESS
            }
            Err(OpenError::Lock(LockError::InUse { pid })) => {
                data_root_in_use(&catalog, &data_root, pid)
            }
            Err(e @ OpenError::Lock(_)) => fail(
                &catalog,
                2,
                MessageKey::CliDataRootUnusable,
                &[("detail", &e.to_string())],
            ),
            Err(e) => fail(
                &catalog,
                1,
                MessageKey::CliError,
                &[("detail", &e.to_string())],
            ),
        };
    }

    // 設定の表を JSON に写す (`chat_template_kwargs = { enable_thinking = false }` など)。
    let request_extra: evorag_llm::RequestExtra = cfg
        .llm
        .chat
        .request_extra
        .as_ref()
        .and_then(|t| serde_json::to_value(t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    // `/model` の候補 (07 §4.2 の `llm.chat.models`: 名前と url)。
    let mut models = Vec::new();
    for t in cfg.llm.chat.models.iter().flatten() {
        let (Some(name), Some(url)) = (
            t.get("name").and_then(|v| v.as_str()),
            t.get("url").and_then(|v| v.as_str()),
        ) else {
            return fail(
                &catalog,
                4,
                MessageKey::CliConfigError,
                &[("detail", "each [[llm.chat.models]] needs name and url")],
            );
        };
        models.push(evorag_core::ModelChoice {
            name: name.to_owned(),
            url: url.to_owned(),
        });
    }
    let (key_for_switch, allow_remote) = (
        cfg.llm.chat.api_key.clone().or_else(|| {
            cfg.llm
                .chat
                .api_key_env
                .as_ref()
                .and_then(|v| std::env::var(v).ok())
        }),
        cfg.llm.allow_remote,
    );
    let connect: evorag_core::Connector = Arc::new(move |u: &str| {
        let endpoint = Endpoint::new(u, key_for_switch.clone(), allow_remote)?;
        let c = HttpClient::new(endpoint, RetryPolicy::default())?;
        Ok(Arc::new(c) as Arc<dyn evorag_llm::LlmClient>)
    });
    let model_name = cfg.llm.chat.model.clone().unwrap_or_else(|| url.clone());
    let make_cfg = || CoreConfig {
        data_root: data_root.clone(),
        config_file: Some(config_file.clone()),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        prompt_language,
        generation: generation.clone(),
        request_extra: request_extra.clone(),
        reserve_for_output: cfg.context.reserve_for_output,
        backup_dir: backup_dir.clone(),
        allow_unsafe_location: cfg.storage.allow_unsafe_location.unwrap_or(false),
        probe: evorag_core::Probe::system(),
        model_name: model_name.clone(),
        models: models.clone(),
        connect: Some(connect.clone()),
        memory_enabled: cfg.memory_enabled(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock.clone())),
    };
    // ログ (02 §7)。状態のフォルダの logs/。始められなくても会話は続ける。
    if let Some(p) = &places {
        let _ = evorag_core::logging::init(&p.state_dir.join("logs"), clock.now().as_millis());
    }
    let mut started = rt.block_on(Core::start(make_cfg(), llm.clone()));
    // DB のファイルが壊れていたら、退避とジャーナルから戻すかを尋ねる (03 §8、FR-205)。
    if let Err(StartError::Store(OpenError::Broken { moved_to, reason })) = &started {
        eprintln!(
            "{}",
            catalog.with(
                MessageKey::CliBroken,
                &[
                    ("moved_to", &moved_to.display().to_string()),
                    ("reason", reason)
                ]
            )
        );
        if !ask_yes(&catalog.key(MessageKey::CliAskRestore)) {
            return ExitCode::from(2);
        }
        let latest = evorag_store::restore::list_backups(&data_root, backup_dir.as_deref());
        if let Some(b) = latest.first() {
            if let Err(e) = evorag_store::restore::restore(
                &data_root,
                &b.dir,
                clock.now().as_millis(),
                Some(&config_file),
            ) {
                return fail(
                    &catalog,
                    1,
                    MessageKey::CliError,
                    &[("detail", &e.to_string())],
                );
            }
            eprintln!(
                "{}",
                catalog.with(
                    MessageKey::CliRestored,
                    &[("from", &b.dir.display().to_string())]
                )
            );
        } else {
            // 退避が無ければ、ジャーナルだけから作り直す (開くと全部の行が取り込まれる)。
            eprintln!("{}", catalog.key(MessageKey::CliRestoredFromJournal));
        }
        started = rt.block_on(Core::start(make_cfg(), llm));
    }
    let (core, events, report) = match started {
        Ok(v) => v,
        Err(StartError::LlmUnreachable(evorag_llm::LlmError::Http { status, .. }))
            if status == 401 || status == 403 =>
        {
            // llama-server は動いているが、API キーが違う・設定されていない。
            let env = cfg
                .llm
                .chat
                .api_key_env
                .clone()
                .unwrap_or_else(|| "(未設定)".to_owned());
            return fail(
                &catalog,
                3,
                MessageKey::CliLlmUnauthorized,
                &[
                    ("url", &url),
                    ("status", &status.to_string()),
                    ("env", &env),
                ],
            );
        }
        Err(StartError::LlmUnreachable(e)) => {
            // TUI を開かずに案内を出して終わる (AC-FR-503-1)。
            return fail(
                &catalog,
                3,
                MessageKey::CliLlmUnreachable,
                &[("url", &url), ("detail", &e.to_string())],
            );
        }
        Err(StartError::UnsafeLocation(d)) => {
            return fail(
                &catalog,
                2,
                MessageKey::CliUnsafeLocation,
                &[("detail", &d)],
            );
        }
        Err(StartError::Store(OpenError::Lock(LockError::InUse { pid }))) => {
            return data_root_in_use(&catalog, &data_root, pid);
        }
        Err(
            e @ (StartError::Store(OpenError::DatabaseInUse)
            | StartError::Store(OpenError::TooNew { .. })
            | StartError::ReadOnly(_)),
        ) => {
            return fail(
                &catalog,
                2,
                MessageKey::CliDataRootUnusable,
                &[("detail", &e.to_string())],
            );
        }
        Err(e) => {
            return fail(
                &catalog,
                1,
                MessageKey::CliError,
                &[("detail", &e.to_string())],
            );
        }
    };

    let mut app = App::new(
        catalog,
        Status {
            endpoint: url.trim_start_matches("http://").to_owned(),
            model: report.server.model_name.clone(),
            n_ctx: report.server.n_ctx,
        },
    );
    for w in &report.location_warnings {
        let m = app
            .catalog
            .with(MessageKey::CliLocationWarning, &[("detail", w)]);
        app.notices.push(m);
    }
    if report.prompt_language_mismatch {
        let m = app.catalog.with(
            MessageKey::CliPromptLanguageFixed,
            &[("recorded", report.prompt_language.as_str())],
        );
        app.notices.push(m);
    }
    let state_dir = places.as_ref().map(|p| p.state_dir.clone());
    let result = evorag_tui::run::run(&core, events, app, state_dir.as_deref());
    // 生成中なら中断として書き、会話の追記を書き切ってから終わる (02 §5)。
    rt.block_on(core.shutdown());
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}
