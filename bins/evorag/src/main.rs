//! サブコマンドの入口 (07 §5)。v0.1 の M1.4 は、TUI を開く `evorag` だけ。
//!
//! 終了コード: 0 正常、1 一般のエラー、2 データ根が使えない (ロック・新しい形式)、
//! 3 llama-server に繋がらない、4 設定の誤り (07 §5)。

mod config;

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
use evorag_types::{MessageKey, SystemClock, UuidV7Gen};

const DEFAULT_URL: &str = "http://127.0.0.1:8080";

#[derive(Parser)]
#[command(
    name = "evorag",
    version,
    about = "Chat with a local llama-server, with long-term memory"
)]
struct Args {
    /// データ根 (設定の storage.data_root より優先する)
    #[arg(long, value_name = "PATH")]
    data_root: Option<PathBuf>,
    /// 設定ファイル
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
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
    let loaded = match config::load(&config_file) {
        Ok(l) => l,
        Err(e) => return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]),
    };
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
    let core_cfg = CoreConfig {
        data_root,
        config_file: Some(config_file),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        prompt_language,
        generation,
        reserve_for_output: cfg.context.reserve_for_output,
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock)),
    };
    let started = rt.block_on(Core::start(core_cfg, llm));
    let (core, events, report) = match started {
        Ok(v) => v,
        Err(StartError::LlmUnreachable(e)) => {
            // TUI を開かずに案内を出して終わる (AC-FR-503-1)。
            return fail(
                &catalog,
                3,
                MessageKey::CliLlmUnreachable,
                &[("url", &url), ("detail", &e.to_string())],
            );
        }
        Err(
            e @ (StartError::Store(OpenError::Lock(LockError::InUse { .. }))
            | StartError::Store(OpenError::DatabaseInUse)
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
    if report.prompt_language_mismatch {
        let m = app.catalog.with(
            MessageKey::CliPromptLanguageFixed,
            &[("recorded", report.prompt_language.as_str())],
        );
        app.notices.push(m);
    }
    let result = evorag_tui::run::run(&core, events, app);
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
