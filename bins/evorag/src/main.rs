//! サブコマンドの入口 (07 §5)。
//!
//! 終了コード: 0 正常、1 一般のエラー、2 データ根が使えない (ロック・新しい形式)、
//! 3 llama-server に繋がらない、4 設定の誤り (07 §5)。

mod config;
mod doctor;
mod export;
mod import;
mod setup;
mod update;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, LazyLock};

use clap::{FromArgMatches, Parser};
use evorag_core::{Core, CoreConfig, PromptLanguage, RootFormat, StartError};
use evorag_llm::{Endpoint, Generation, HttpClient, RetryPolicy};
use evorag_store::lock::LockError;
use evorag_store::{OpenError, ReadOnlyReason};
use evorag_tui::app::{App, Status};
use evorag_tui::catalog::{Catalog, Language};
use evorag_types::{Clock, MessageKey, SystemClock, UuidV7Gen};

const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// `--version` (FR-906、07 §5): 版・作りの情報・入れ方・書く DB の形式の版。1 行目 (`evorag <版>`) の形は
/// 変えない (`evorag update` が読む、12 §7.2)。通信せず、データ根・設定・`update/` に触れない。
static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}
commit: {}
target: {}
rustc: {}
installed via: {}
db format: {}",
        env!("CARGO_PKG_VERSION"),
        env!("EVORAG_COMMIT"),
        env!("EVORAG_TARGET"),
        env!("EVORAG_RUSTC"),
        env!("EVORAG_CHANNEL"),
        evorag_store::FORMAT_V05,
    )
});

// `--help` の文は、UI の言語のカタログから当てる (`localized_command`、07 §5・§6)。ここには書かない。
#[derive(Parser)]
#[command(name = "evorag", version = VERSION.as_str())]
struct Args {
    // データ根 (設定の storage.data_root より優先する)
    #[arg(long, value_name = "PATH", global = true)]
    data_root: Option<PathBuf>,
    // 設定ファイル
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand)]
enum Sub {
    // llama-server を探して能力を確かめ、設定ファイルを書く
    Setup {
        // 接続先 (省くと、設定の llm.chat.url と 127.0.0.1:8080・8081 を探す)
        #[arg(long)]
        url: Option<String>,
        // API キーを入れた環境変数の名前 (llama-server を --api-key で起動したとき)
        #[arg(long, value_name = "NAME")]
        api_key_env: Option<String>,
        // 尋ねずに進める (プロンプトの言語は UI の言語、設定ファイルは書き換える)
        #[arg(long)]
        yes: bool,
    },
    // 設定・接続・データ根・ジャーナル・退避を、読むだけで点検する
    Doctor,
    // 退避の操作
    Backups {
        #[command(subcommand)]
        action: BackupsAction,
    },
    // 記録を種類ごとの JSON Lines に書き出す (設定は API キーを除いて写す)
    Export {
        // 書き出すフォルダ (空か、まだ無いフォルダ)
        out: PathBuf,
    },
    // 派生データを、会話と明示の操作から作り直す (先に退避する)
    Rebuild {
        // 索引だけを作り直す
        #[arg(long, conflicts_with = "facts")]
        index: bool,
        // 事実も会話から抽出し直す
        #[arg(long)]
        facts: bool,
    },
    // 退避から戻す (その後の会話はジャーナルから足す)
    Restore {
        // 退避のフォルダか、その中の evorag.redb
        backup: PathBuf,
    },
    // 副索引と関係を作り直し、ジャーナルから記録を戻し、忘れる・削除を当て直す (先に退避する)
    Repair,
    // 本体の更新 (新しい版の確認・置き換え・巻き戻し)
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
    // v0.4 のデータ根を新しいデータ根へ取り込み、古い根を改名して残す
    #[command(name = "import-v04")]
    ImportV04 {
        // 取り込む v0.4 のデータ根 (省くと、今のデータ根)
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
    },
}

#[derive(clap::Subcommand)]
enum UpdateAction {
    // 新しい版があるかを確かめる (何も書き換えない)
    Check,
    // 新しい版を取得し、署名を確かめてから置き換える (今の版は evorag.prev に残す)
    Apply {
        // 入れる版 (省くと、いちばん新しい版)
        version: Option<String>,
    },
    // evorag.prev に戻す (通信しない)
    Rollback,
}

#[derive(clap::Subcommand)]
enum BackupsAction {
    // 退避の一覧 (新しい順)
    List,
    // すべての退避を消す
    Purge {
        // 尋ねずに消す
        #[arg(long)]
        yes: bool,
    },
}

/// 引数の命令に、UI の言語の `--help` の文を当てる (07 §5・§6)。clap が足す `--help`・`--version` の文と
/// 見出しは clap の英語のまま。
fn localized_command(c: &Catalog) -> clap::Command {
    use clap::CommandFactory;
    let h = |k: MessageKey| c.key(k);
    Args::command()
        .about(h(MessageKey::CliHelpAbout))
        .mut_arg("data_root", |a| a.help(h(MessageKey::CliHelpDataRoot)))
        .mut_arg("config", |a| a.help(h(MessageKey::CliHelpConfig)))
        .mut_subcommand("setup", |s| {
            s.about(h(MessageKey::CliHelpSetup))
                .mut_arg("url", |a| a.help(h(MessageKey::CliHelpSetupUrl)))
                .mut_arg("api_key_env", |a| {
                    a.help(h(MessageKey::CliHelpSetupApiKeyEnv))
                })
                .mut_arg("yes", |a| a.help(h(MessageKey::CliHelpSetupYes)))
        })
        .mut_subcommand("doctor", |s| s.about(h(MessageKey::CliHelpDoctor)))
        .mut_subcommand("backups", |s| {
            s.about(h(MessageKey::CliHelpBackups))
                .mut_subcommand("list", |s| s.about(h(MessageKey::CliHelpBackupsList)))
                .mut_subcommand("purge", |s| {
                    s.about(h(MessageKey::CliHelpBackupsPurge))
                        .mut_arg("yes", |a| a.help(h(MessageKey::CliHelpBackupsPurgeYes)))
                })
        })
        .mut_subcommand("export", |s| {
            s.about(h(MessageKey::CliHelpExport))
                .mut_arg("out", |a| a.help(h(MessageKey::CliHelpExportOut)))
        })
        .mut_subcommand("rebuild", |s| {
            s.about(h(MessageKey::CliHelpRebuild))
                .mut_arg("index", |a| a.help(h(MessageKey::CliHelpRebuildIndex)))
                .mut_arg("facts", |a| a.help(h(MessageKey::CliHelpRebuildFacts)))
        })
        .mut_subcommand("restore", |s| {
            s.about(h(MessageKey::CliHelpRestore))
                .mut_arg("backup", |a| a.help(h(MessageKey::CliHelpRestoreBackup)))
        })
        .mut_subcommand("repair", |s| s.about(h(MessageKey::CliHelpRepair)))
        .mut_subcommand("update", |s| {
            s.about(h(MessageKey::CliHelpUpdate))
                .mut_subcommand("check", |s| s.about(h(MessageKey::CliHelpUpdateCheck)))
                .mut_subcommand("apply", |s| {
                    s.about(h(MessageKey::CliHelpUpdateApply))
                        .mut_arg("version", |a| {
                            a.help(h(MessageKey::CliHelpUpdateApplyVersion))
                        })
                })
                .mut_subcommand("rollback", |s| {
                    s.about(h(MessageKey::CliHelpUpdateRollback))
                })
        })
        .mut_subcommand("import-v04", |s| {
            s.about(h(MessageKey::CliHelpImportV04))
                .mut_arg("from", |a| a.help(h(MessageKey::CliHelpImportV04From)))
        })
}

/// 引数を読む前に、`--config <PATH>`・`--config=<PATH>` を探す (`--help` を UI の言語で出すため)。
fn config_arg(args: &[OsString]) -> Option<PathBuf> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--config" {
            return it.next().map(PathBuf::from);
        }
        if let Some(p) = a.to_str().and_then(|s| s.strip_prefix("--config=")) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// `rebuild` の作り直す範囲。
enum RebuildScope {
    /// 索引だけ (`--index`)。
    Index,
    /// 派生データの全体。
    Derived,
    /// 事実も抽出し直す (`--facts`)。llama-server の接続先と API キーの環境変数 (案内に示す) と使い方を持つ。
    Facts {
        url: String,
        api_key_env: Option<String>,
        source: evorag_core::rebuild::FactsSource,
    },
}

/// llama-server の失敗のうち、起動と作り直しで同じ案内にするもの (API キーの誤り・ルーターモード、06 §2)。
/// 当たらなければ `None` (呼ぶ側が、それぞれの「繋がらない」の案内を出す)。
fn llm_setup_failure(
    catalog: &Catalog,
    url: &str,
    api_key_env: Option<&str>,
    e: &evorag_llm::LlmError,
) -> Option<ExitCode> {
    use evorag_llm::LlmError;
    Some(match e {
        LlmError::Http { status, .. } if *status == 401 || *status == 403 => {
            // llama-server は動いているが、API キーが違う・設定されていない。
            let unset = catalog.key(MessageKey::CliEnvUnset);
            let env = api_key_env.unwrap_or(&unset);
            fail(
                catalog,
                3,
                MessageKey::CliLlmUnauthorized,
                &[("url", url), ("status", &status.to_string()), ("env", env)],
            )
        }
        // ルーターモードの llama-server に、名前を付けずに繋いだ。
        LlmError::RouterNeedsModel { models } => fail(
            catalog,
            4,
            MessageKey::CliRouterNeedsModel,
            &[("url", url), ("models", &models.join(", "))],
        ),
        LlmError::RouterModel { model, detail } => fail(
            catalog,
            3,
            MessageKey::CliRouterModel,
            &[("url", url), ("model", model), ("detail", detail)],
        ),
        _ => return None,
    })
}

/// 事実を作り直せない理由の文言 (07 §5。値の鍵の理由は、理由ごとの文言を `detail` に入れる)。
fn facts_unavailable(
    catalog: &Catalog,
    why: evorag_core::rebuild::FactsUnavailable,
) -> (MessageKey, Vec<(&'static str, String)>) {
    use evorag_core::forget_key::FactsKeyProblem;
    use evorag_core::rebuild::FactsUnavailable;
    match why {
        FactsUnavailable::NotProbed => (MessageKey::CliRebuildFactsNoProbe, Vec::new()),
        FactsUnavailable::ProbeFailed => (MessageKey::CliRebuildFactsProbeFailed, Vec::new()),
        FactsUnavailable::ContextTooSmall { n_ctx } => (
            MessageKey::CliRebuildFactsContextTooSmall,
            vec![("n_ctx", n_ctx.to_string())],
        ),
        FactsUnavailable::Key(problem) => {
            let cause = match problem {
                FactsKeyProblem::Conflict => MessageKey::CliFactsKeyConflict,
                FactsKeyProblem::MissingWithDirectives => MessageKey::CliFactsKeyMissing,
                FactsKeyProblem::Malformed => MessageKey::CliFactsKeyMalformed,
                FactsKeyProblem::ReadOnly => MessageKey::CliFactsKeyReadOnly,
            };
            (
                MessageKey::CliRebuildFactsKeyUnusable,
                vec![("detail", catalog.key(cause))],
            )
        }
        FactsUnavailable::ForgetPending => (MessageKey::CliRebuildFactsForgetPending, Vec::new()),
        FactsUnavailable::NewerFacts { count } => (
            MessageKey::CliRebuildFactsNewer,
            vec![("count", count.to_string())],
        ),
    }
}

/// `evorag rebuild [--index] [--facts]` (03 §10、FR-212)。
fn rebuild(
    catalog: &Catalog,
    data_root: &std::path::Path,
    config_file: &std::path::Path,
    scope: RebuildScope,
    rt: &tokio::runtime::Runtime,
    clock: Arc<dyn evorag_types::Clock>,
) -> ExitCode {
    use evorag_core::rebuild::{ExtractionStop, RebuildError};

    let ids = Arc::new(UuidV7Gen::new(clock.clone()));
    let index_only = matches!(scope, RebuildScope::Index);
    let (url, api_key_env, facts) = match scope {
        RebuildScope::Facts {
            url,
            api_key_env,
            source,
        } => {
            eprintln!("{}", catalog.key(MessageKey::CliRebuildingFacts));
            (url, api_key_env, Some(source))
        }
        RebuildScope::Index | RebuildScope::Derived => (String::new(), None, None),
    };
    // 繋がらない・断られたときの案内 (API キーとルーターモードは起動と同じ案内にする)。
    let llm_failed = |key: MessageKey, e: &evorag_llm::LlmError| {
        llm_setup_failure(catalog, &url, api_key_env.as_deref(), e).unwrap_or_else(|| {
            fail(
                catalog,
                3,
                key,
                &[("url", &url), ("detail", &e.to_string())],
            )
        })
    };
    match rt.block_on(evorag_core::rebuild::run(
        evorag_core::rebuild::RebuildOptions {
            data_root: data_root.to_path_buf(),
            root_format: RootFormat::V05,
            // semver だけを書く (`VERSION` は作りの情報を含み、`meta` の版として読めない)。
            evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
            config_file: Some(config_file.to_path_buf()),
            index_only,
            facts,
            clock,
            ids,
        },
    )) {
        Ok(r) => {
            let backup = r.backup.display().to_string();
            let m = if let Some(f) = &r.facts {
                catalog.with(
                    MessageKey::CliRebuiltFacts,
                    &[
                        ("backup", &backup),
                        ("removed", &r.removed.to_string()),
                        ("docs", &r.docs.to_string()),
                        ("utterances", &f.utterances.to_string()),
                        ("facts", &f.facts.to_string()),
                        ("old", &f.removed.to_string()),
                        ("skipped", &f.not_sent().to_string()),
                        ("filtered", &f.filtered().to_string()),
                        ("too_long", &f.skipped_for("too_long").to_string()),
                        ("mismatch", &f.skipped_for("forget_mismatch").to_string()),
                        ("corrections", &f.corrections.to_string()),
                    ],
                )
            } else if index_only {
                catalog.with(
                    MessageKey::CliRebuiltIndex,
                    &[("backup", &backup), ("docs", &r.docs.to_string())],
                )
            } else {
                catalog.with(
                    MessageKey::CliRebuilt,
                    &[
                        ("backup", &backup),
                        ("removed", &r.removed.to_string()),
                        ("docs", &r.docs.to_string()),
                    ],
                )
            };
            eprintln!("{m}");
            // 抽出できずに飛ばした発話があれば、作り直しが部分的だったことを理由ごとに示す。
            if let Some(f) = r.facts.as_ref().filter(|f| f.lost() > 0) {
                let n = |reason: &str| f.skipped_for(reason).to_string();
                let m = catalog.with(
                    MessageKey::CliRebuiltFactsPartial,
                    &[
                        ("lost", &f.lost().to_string()),
                        ("truncated", &n("truncated")),
                        ("invalid", &n("invalid_output")),
                    ],
                );
                eprintln!("{}", m.replace("\\n", "\n"));
            }
            ExitCode::SUCCESS
        }
        Err(RebuildError::Open(OpenError::Lock(LockError::InUse { pid }))) => {
            data_root_in_use(catalog, data_root, pid)
        }
        Err(RebuildError::Open(OpenError::NeedsImport { format })) => {
            import::needs_import(catalog, data_root, &format)
        }
        Err(RebuildError::Open(OpenError::NeedsV04Open { format })) => {
            import::needs_v04_open(catalog, data_root, format.as_deref())
        }
        Err(RebuildError::FactsNotInV05) => fail(catalog, 1, MessageKey::CliRebuildFactsV05, &[]),
        // 事実の作り直しを、何も書かずに止めた (07 §5 の終了コード: 3 は llama-server、4 はルーターの設定、6 は
        // モデルの違い)。
        Err(
            RebuildError::LlmUnreachable(e)
            | RebuildError::Extraction(ExtractionStop::Unreachable(e)),
        ) => llm_failed(MessageKey::CliRebuildFactsUnreachable, &e),
        Err(RebuildError::Extraction(ExtractionStop::Rejected(e))) => {
            llm_failed(MessageKey::CliRebuildFactsRejected, &e)
        }
        Err(RebuildError::Extraction(ExtractionStop::Failed { attempts, detail })) => fail(
            catalog,
            3,
            MessageKey::CliRebuildFactsFailed,
            &[
                ("url", &url),
                ("attempts", &attempts.to_string()),
                ("detail", &detail),
            ],
        ),
        Err(RebuildError::AfterFacts { backup, source, .. }) => fail(
            catalog,
            1,
            MessageKey::CliRebuildFailedAfterFacts,
            &[
                ("backup", &backup.display().to_string()),
                ("detail", &source.to_string()),
            ],
        ),
        Err(RebuildError::ModelMismatch {
            recorded,
            connected,
        }) => fail(
            catalog,
            6,
            MessageKey::CliRebuildFactsModelMismatch,
            &[("recorded", &recorded), ("connected", &connected)],
        ),
        Err(RebuildError::FactsUnavailable(why)) => {
            // 新しい版のデータは、データ根が使えないときと同じ終了コード 2 (07 §5)。
            let code = match why {
                evorag_core::rebuild::FactsUnavailable::NewerFacts { .. } => 2,
                _ => 1,
            };
            let (key, args) = facts_unavailable(catalog, why);
            let args: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
            fail(catalog, code, key, &args)
        }
        Err(e) => fail(
            catalog,
            1,
            MessageKey::CliRebuildFailed,
            &[("detail", &e.to_string())],
        ),
    }
}

/// `evorag repair` (03 §10、FR-211)。
fn repair(
    catalog: &Catalog,
    data_root: &std::path::Path,
    config_file: &std::path::Path,
    prompt_language: PromptLanguage,
    clock: Arc<dyn evorag_types::Clock>,
) -> ExitCode {
    use evorag_core::repair::{RepairError, RepairOptions};

    let ids = Arc::new(UuidV7Gen::new(clock.clone()));
    match evorag_core::repair::run(RepairOptions {
        data_root: data_root.to_path_buf(),
        root_format: RootFormat::V05,
        // semver だけを書く (`VERSION` は作りの情報を含み、`meta` の版として読めない)。
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        config_file: Some(config_file.to_path_buf()),
        prompt_language,
        clock,
        ids,
    }) {
        Ok(r) => {
            let s = &r.store;
            let n = |v: usize| v.to_string();
            let m = catalog.with(
                MessageKey::CliRepaired,
                &[
                    ("backup", &r.backup.display().to_string()),
                    ("replayed", &n(r.journal_replayed)),
                    ("deletes", &n(r.journal_deletes)),
                    ("restored", &n(s.restored_total())),
                    ("lower", &n(s.lower_rev_skipped)),
                    ("quarantined", &n(s.quarantined)),
                    ("metas", &n(s.metas_restored)),
                    ("bad_lines", &n(s.journal_bad_lines)),
                    ("links_added", &n(s.links_added)),
                    ("links_removed", &n(s.links_removed)),
                    ("lookups_added", &n(s.lookups_added)),
                    ("lookups_removed", &n(s.lookups_removed)),
                ],
            );
            eprintln!("{}", m.replace("\\n", "\n"));
            let f = &r.forgets;
            if f.patched + f.retracted + f.deleted + f.mismatched > 0 {
                let m = catalog.with(
                    MessageKey::CliRepairedForgets,
                    &[
                        ("patched", &n(f.patched)),
                        ("retracted", &n(f.retracted)),
                        ("deleted", &n(f.deleted)),
                        ("mismatched", &n(f.mismatched)),
                    ],
                );
                eprintln!("{m}");
            }
            if f.journal_pending {
                eprintln!("{}", catalog.key(MessageKey::CliRepairJournalPending));
            }
            if f.key_missing {
                eprintln!("{}", catalog.key(MessageKey::CliRepairKeyMissing));
            }
            ExitCode::SUCCESS
        }
        Err(RepairError::Open(OpenError::Lock(LockError::InUse { pid }))) => {
            data_root_in_use(catalog, data_root, pid)
        }
        Err(RepairError::Open(OpenError::NeedsImport { format })) => {
            import::needs_import(catalog, data_root, &format)
        }
        Err(RepairError::Open(OpenError::NeedsV04Open { format })) => {
            import::needs_v04_open(catalog, data_root, format.as_deref())
        }
        Err(RepairError::NoDataRoot(path)) => fail(
            catalog,
            2,
            MessageKey::CliDataRootMissing,
            &[("path", &path.display().to_string())],
        ),
        // DB のファイルが壊れていたので `broken/` へ移した。repair では直せないので、restore を案内する (03 §8)。
        Err(RepairError::Open(OpenError::Broken { moved_to, reason })) => {
            eprintln!(
                "{}",
                catalog.with(
                    MessageKey::CliBroken,
                    &[
                        ("moved_to", &moved_to.display().to_string()),
                        ("reason", &reason)
                    ]
                )
            );
            fail(catalog, 2, MessageKey::CliRepairBrokenHint, &[])
        }
        Err(RepairError::ReadOnly(why)) => {
            let (key, args) = match &why {
                ReadOnlyReason::NewerFormat { min_write } => (
                    MessageKey::CliReadOnlyNewerFormat,
                    [("min_write", min_write.as_str())],
                ),
                ReadOnlyReason::MigrationFailed { reason } => (
                    MessageKey::CliReadOnlyMigrationFailed,
                    [("reason", reason.as_str())],
                ),
            };
            fail(catalog, 2, key, &args)
        }
        // データ根が使えない (ロック・新しい形式・ほかのプロセスが DB を開いている、07 §5 の終了コード 2)。
        Err(
            e @ RepairError::Open(
                OpenError::Lock(_) | OpenError::DatabaseInUse | OpenError::TooNew { .. },
            ),
        ) => fail(
            catalog,
            2,
            MessageKey::CliDataRootUnusable,
            &[("detail", &e.to_string())],
        ),
        // ストアの層はコミットした後の失敗。戻した記録と作り直した関係を示し、退避の場所を案内する。
        Err(RepairError::AfterStore {
            store,
            backup,
            source,
        }) => {
            let n = |v: usize| v.to_string();
            fail(
                catalog,
                1,
                MessageKey::CliRepairFailedAfterStore,
                &[
                    ("restored", &n(store.restored_total())),
                    ("links_added", &n(store.links_added)),
                    ("links_removed", &n(store.links_removed)),
                    ("lookups_added", &n(store.lookups_added)),
                    ("lookups_removed", &n(store.lookups_removed)),
                    ("detail", &source.to_string()),
                    ("backup", &backup.display().to_string()),
                ],
            )
        }
        Err(e) => fail(
            catalog,
            1,
            MessageKey::CliRepairFailed,
            &[("detail", &e.to_string())],
        ),
    }
}

/// `evorag backups purge [--yes]` (03 §7.3・§10、FR-213)。データ根のロックを取ってから、確認の後に、すべての
/// 退避を消す (TUI が動いていれば断る、03 §9)。
fn purge_backups(
    catalog: &Catalog,
    data_root: &std::path::Path,
    backup_dir: Option<&std::path::Path>,
    yes: bool,
) -> ExitCode {
    use evorag_store::restore::{list_backups, purge_all_backups};

    let _lock = match lock_data_root(catalog, data_root) {
        Ok(l) => l,
        Err(code) => return code,
    };
    // v0.4 の根には書かない (03 §12.4)。退避の整理も、取り込んだ後の古い根の処分 (`reset --old`) に任せる。
    if let Some(code) = import::refuse_v04_root(catalog, data_root) {
        return code;
    }
    let count = list_backups(data_root, backup_dir).len();
    // 消す退避が無くても、作りかけの退避は消す (尋ねない。戻せるものを消さないため)。
    if count > 0 {
        let ask = catalog.with(
            MessageKey::BackupsPurgeAsk,
            &[("count", &count.to_string())],
        );
        if !yes && !ask_yes(&ask) {
            eprintln!("{}", catalog.key(MessageKey::BackupsPurgeDeclined));
            return ExitCode::from(1);
        }
    }
    let purged = purge_all_backups(data_root, backup_dir);
    let removed = purged.removed.to_string();
    if purged.failed.is_empty() {
        let m = if purged.removed == 0 {
            catalog.key(MessageKey::BackupsNone)
        } else {
            catalog.with(MessageKey::BackupsPurged, &[("count", &removed)])
        };
        eprintln!("{m}");
        ExitCode::SUCCESS
    } else {
        fail(
            catalog,
            1,
            MessageKey::BackupsPurgeFailed,
            &[("count", &removed), ("detail", &purged.failed.join("; "))],
        )
    }
}

/// データ根のロックを取る (`backups list`・`backups purge`。TUI など、ほかの evorag が使っていれば終了コード 2 で
/// 断る、03 §9)。データ根がまだ無ければ、使っているプロセスも無いので取らない (何も作らない)。
fn lock_data_root(
    catalog: &Catalog,
    data_root: &std::path::Path,
) -> Result<Option<evorag_store::lock::DataRootLock>, ExitCode> {
    use evorag_store::lock::DataRootLock;

    if !data_root.exists() {
        return Ok(None);
    }
    match DataRootLock::acquire(data_root) {
        Ok(l) => Ok(Some(l)),
        Err(LockError::InUse { pid }) => Err(data_root_in_use(catalog, data_root, pid)),
        Err(e) => Err(fail(
            catalog,
            2,
            MessageKey::CliDataRootUnusable,
            &[("detail", &e.to_string())],
        )),
    }
}

/// y / n を尋ねる。標準入力から 1 行を読み、`y`・`yes` (大文字・小文字を問わない) のときだけ真。ほかの答え・空の
/// 行・入力の終わり (端末でないときなど)・読めないときは偽 (n)。
fn ask_yes(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let answer = line.trim();
            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
        }
    }
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

/// 古い版の設定を書き換えたことを知らせる (03 §12.3、FR-807)。
fn config_migrated(catalog: &Catalog, config_file: &std::path::Path, backup: &std::path::Path) {
    eprintln!(
        "{}",
        catalog.with(
            MessageKey::CliConfigMigrated,
            &[
                ("path", &config_file.display().to_string()),
                ("backup", &backup.display().to_string())
            ]
        )
    );
}

fn fail(catalog: &Catalog, code: u8, key: MessageKey, args: &[(&str, &str)]) -> ExitCode {
    eprintln!("{}", catalog.with(key, args).replace("\\n", "\n"));
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let raw: Vec<OsString> = std::env::args_os().collect();
    let places = config::default_places();
    // `--help` と引数・設定の誤りは、設定を読む前に出すので、設定ファイルの ui.language だけを先に読む。
    let early = config_arg(&raw)
        .or_else(|| places.as_ref().map(|p| p.config_file.clone()))
        .map_or_else(Language::default, |p| config::early_ui_language(&p));
    let catalog = Catalog::new(early);
    let args = match localized_command(&catalog)
        .try_get_matches_from(&raw)
        .and_then(|m| Args::from_arg_matches(&m))
    {
        Ok(a) => a,
        Err(e) => e.exit(),
    };
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
    // update は設定を書き換えない (check は何も書き換えない、rollback は戻した版が読む設定を残す)。
    // この版の読めない新しい形式の設定でも、更新に要るキーだけを読んで動く (12 §7.2)。
    if let Some(Sub::Update { action }) = args.command {
        let ucfg = match config::load_for_update(&config_file) {
            Ok(u) => u,
            Err(e) => return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]),
        };
        for k in &ucfg.ignored {
            eprintln!(
                "{}",
                catalog.with(MessageKey::CliConfigIgnored, &[("key", k)])
            );
        }
        let Some(data_root) = args
            .data_root
            .or(ucfg.data_root)
            .or_else(|| places.as_ref().map(|p| p.data_root.clone()))
        else {
            return fail(
                &catalog,
                1,
                MessageKey::CliError,
                &[("detail", "no home directory")],
            );
        };
        let action = match action {
            UpdateAction::Check => update::Action::Check,
            UpdateAction::Apply { version } => update::Action::Apply { version },
            UpdateAction::Rollback => update::Action::Rollback,
        };
        return update::run(
            &catalog,
            action,
            ucfg.trusted_keys,
            &update::Places {
                data_root: &data_root,
                backup_dir: ucfg.backup_dir.as_deref(),
                config_file: &config_file,
                config_version: ucfg.version,
                state_dir: places.as_ref().map(|p| p.state_dir.as_path()),
            },
        );
    }
    // doctor は読むだけ (設定の移行もしない)。TUI は、更新の片付けの後に移行する (動いている evorag が
    // 置いてあるものと違えば、設定に触れずに止めるため、12 §7.3)。
    let migrate = !matches!(args.command, None | Some(Sub::Doctor));
    let loaded = match config::load(&config_file, migrate) {
        Ok(l) => l,
        Err(e) => {
            // TUI: 設定がこの版の読めない新しい形式の版なら、止まる前に更新の片付けを行う (新しい版から
            // 戻す途中で止まった計画を続けるため、12 §7.3)。データ根は evorag update と同じ読み方で決める。
            if args.command.is_none()
                && let Some(p) = &places
                && let Some(newer) = config::newer_config(&config_file)
            {
                let data_root = args
                    .data_root
                    .clone()
                    .or(newer.data_root)
                    .unwrap_or_else(|| p.data_root.clone());
                let (lines, code) =
                    update::startup_with_newer_config(&catalog, &data_root, &p.state_dir, &e);
                for l in lines {
                    eprintln!("{l}");
                }
                return ExitCode::from(code);
            }
            return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]);
        }
    };
    if let Some(b) = &loaded.migrated {
        config_migrated(&catalog, &config_file, b);
    }
    for k in &loaded.ignored {
        eprintln!(
            "{}",
            catalog.with(MessageKey::CliConfigIgnored, &[("key", k)])
        );
    }
    let cfg = loaded.config;
    let catalog = Catalog::new(cfg.ui_language());
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
    // ルーターモードでは、設定の llm.chat.model をすべての要求に付ける (06 §2)。
    let llm = match HttpClient::new(endpoint, RetryPolicy::default()) {
        Ok(c) => Arc::new(c.with_model(cfg.llm.chat.model.clone())),
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
    // 設定に無ければ UI の言語 (新しいデータ根で setup を通らずに起動したとき、07 §6.1)。
    let prompt_language = cfg.prompt_language();

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
    if let Some(Sub::Backups { action }) = &args.command {
        return match action {
            BackupsAction::List => match lock_data_root(&catalog, &data_root) {
                Ok(_lock) => doctor::list_backups(&catalog, &data_root, backup_dir.as_deref()),
                Err(code) => code,
            },
            BackupsAction::Purge { yes } => {
                purge_backups(&catalog, &data_root, backup_dir.as_deref(), *yes)
            }
        };
    }
    if let Some(Sub::ImportV04 { from }) = &args.command {
        return import::run(
            &catalog,
            from.as_deref().unwrap_or(&data_root),
            cfg.storage.allow_unsafe_location.unwrap_or(false),
            clock.clone(),
        );
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
    // 補助の呼び出しに足す JSON (06 §6)。事実の作り直しの抽出にも使う。
    let aux_request_extra: evorag_llm::RequestExtra = cfg
        .llm
        .chat
        .aux_request_extra
        .as_ref()
        .and_then(|t| serde_json::to_value(t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(Sub::Rebuild { index, facts }) = &args.command {
        let scope = if *facts {
            RebuildScope::Facts {
                url: url.clone(),
                api_key_env: cfg.llm.chat.api_key_env.clone(),
                source: evorag_core::rebuild::FactsSource {
                    llm: llm.clone(),
                    aux_request_extra: aux_request_extra.clone(),
                    prompt_language,
                },
            }
        } else if *index {
            RebuildScope::Index
        } else {
            RebuildScope::Derived
        };
        return rebuild(
            &catalog,
            &data_root,
            &config_file,
            scope,
            &rt,
            clock.clone(),
        );
    }
    if let Some(Sub::Restore { backup }) = &args.command {
        if let Some(code) = import::check_restore(&catalog, &data_root, backup) {
            return code;
        }
        return match evorag_store::restore::restore(
            &data_root,
            backup,
            clock.now().as_millis(),
            Some(&config_file),
            &evorag_memory::carried_on_restore,
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
    if let Some(Sub::Repair) = &args.command {
        return repair(
            &catalog,
            &data_root,
            &config_file,
            prompt_language,
            clock.clone(),
        );
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
    // v0.2 で使わなくなったキー (07 §4.2): 読んで無視し、知らせる。v0.1 の `/model` の候補は、
    // 切り替えを setup でだけ行うようにしたので使わない (ADR-0018)。
    let ignored_config = config::obsolete(&cfg);
    // embed と rerank の役割 (06 §2)。接続先の誤り (ループバック以外など) は設定の誤りとして止める。
    let mut aux: Vec<Option<Arc<dyn evorag_llm::LlmClient>>> = Vec::new();
    for role in ["embed", "rerank"] {
        let client = match cfg.role(role) {
            None => None,
            Some(r) => match Endpoint::new(&r.url, r.api_key, cfg.llm.allow_remote)
                .and_then(|e| HttpClient::new(e, RetryPolicy::default()))
            {
                Ok(c) => Some(Arc::new(c.with_model(r.model)) as Arc<dyn evorag_llm::LlmClient>),
                Err(e) => {
                    return fail(
                        &catalog,
                        4,
                        MessageKey::CliConfigError,
                        &[("detail", &format!("llm.{role}: {e}"))],
                    );
                }
            },
        };
        aux.push(client);
    }
    let (embed, rerank) = (aux[0].clone(), aux[1].clone());
    let model_name = cfg.llm.chat.model.clone().unwrap_or_else(|| url.clone());
    let make_cfg = || CoreConfig {
        data_root: data_root.clone(),
        root_format: RootFormat::V05,
        config_file: Some(config_file.clone()),
        evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
        prompt_language,
        prompt_language_explicit: cfg.prompt_language_explicit(),
        generation: generation.clone(),
        request_extra: request_extra.clone(),
        reserve_for_output: cfg.context.reserve_for_output,
        backup_dir: backup_dir.clone(),
        allow_unsafe_location: cfg.storage.allow_unsafe_location.unwrap_or(false),
        probe: evorag_core::Probe::system(),
        model_name: model_name.clone(),
        ignored_config: ignored_config.clone(),
        embed: embed.clone(),
        rerank: rerank.clone(),
        aux_request_extra: aux_request_extra.clone(),
        probe_models: true,
        sleep: cfg.sleep(),
        memory_enabled: cfg.memory_enabled(),
        precompute: cfg.precompute(),
        clock: clock.clone(),
        ids: Arc::new(UuidV7Gen::new(clock.clone())),
    };
    // ログ (02 §7)。状態のフォルダの logs/。始められなくても会話は続ける。
    if let Some(p) = &places {
        let _ = evorag_core::logging::init(&p.state_dir.join("logs"), clock.now().as_millis());
    }
    // 止まった import-v04 の入れ替えを続け、作りかけを片付ける (03 §12.4)。ストアを開く前に行う (2 つの改名の間で
    // 止まると元の名前のフォルダが無く、開くと空の新しい根を作ってしまうため)。
    if let Err(code) = import::resume_before_start(&catalog, &data_root) {
        return code;
    }
    // 更新の続きと片付け (12 §7.3)。データ根のロックを取って持ったまま、設定の移行とストアを開く前に行う。
    // 動いている evorag が置いてあるものと違えば、設定にもデータにも触れずに止める。片付けに失敗しても起動
    // する。取ったロックは放さずにストアを開くのに渡す。
    let (update_notices, held_lock) = match places
        .as_ref()
        .map(|p| update::startup_cleanup(&catalog, &data_root, &p.state_dir))
    {
        Some(update::StartupCleanup::Restart(m)) => {
            eprintln!("{m}");
            return ExitCode::from(1);
        }
        Some(update::StartupCleanup::Continue { notices, lock }) => (notices, lock),
        None => (Vec::new(), None),
    };
    // 設定の移行 (03 §12.3)。
    match config::migrate(&config_file) {
        Ok(Some(b)) => config_migrated(&catalog, &config_file, &b),
        Ok(None) => {}
        Err(e) => return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]),
    }
    let mut started = rt.block_on(Core::start_locked(make_cfg(), llm.clone(), held_lock));
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
        // 戻す候補は、この根の退避 (と ID の分からない退避) だけ。`storage.backup_dir` を import-v04 の古い根と
        // 共有していても、古い根の退避を戻さない (03 §7.1 の「退避と根の ID」)。
        let owner = evorag_store::restore::BackupOwner::of(&data_root, Some(moved_to));
        // v0.5 より古い形式の退避は戻さない (03 §8。v0.4 の退避は v0.4 で戻してから取り込む)。
        let latest: Vec<_> =
            evorag_store::restore::restore_candidates(&data_root, backup_dir.as_deref(), &owner)
                .into_iter()
                .filter(|b| !import::backup_older_than_v05(&b.dir))
                .collect();
        if let Some(b) = latest.first() {
            if let Err(e) = evorag_store::restore::restore(
                &data_root,
                &b.dir,
                clock.now().as_millis(),
                Some(&config_file),
                &evorag_memory::carried_on_restore,
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
        started = rt.block_on(Core::start(make_cfg(), llm.clone()));
    }
    // v0.4 の根は、在り場所で移行せずに、取り込むかを尋ねる (03 §12.4)。y なら取り込んで、新しい根のロックを
    // 持ったまま新しい根で続ける。y 以外は何も変えずに 2 で終わる。
    if let Err(StartError::Store(OpenError::NeedsImport { format })) = &started {
        let lock = match import::import_at_startup(
            &catalog,
            &data_root,
            format,
            cfg.storage.allow_unsafe_location.unwrap_or(false),
            clock.clone(),
        ) {
            Ok(lock) => lock,
            Err(code) => return code,
        };
        started = rt.block_on(Core::start_locked(make_cfg(), llm, Some(lock)));
    }
    let (core, events, report) = match started {
        Ok(v) => v,
        Err(StartError::LlmUnreachable(e)) => {
            // API キーの誤りとルーターモードは、それぞれの案内にする。ほかは、TUI を開かずに案内を出して終わる
            // (AC-FR-503-1)。
            return llm_setup_failure(&catalog, &url, cfg.llm.chat.api_key_env.as_deref(), &e)
                .unwrap_or_else(|| {
                    fail(
                        &catalog,
                        3,
                        MessageKey::CliLlmUnreachable,
                        &[("url", &url), ("detail", &e.to_string())],
                    )
                });
        }
        Err(StartError::ModelMismatch {
            recorded,
            connected,
        }) => {
            // 画面を開かずに、違いと取れる手を示して終わる (06 §3、終了コード 6)。
            let hint = if evorag_core::same_meta(&recorded, &connected) {
                catalog.key(MessageKey::CliModelMismatchSameMeta)
            } else {
                String::new()
            };
            return fail(
                &catalog,
                6,
                MessageKey::CliModelMismatch,
                &[
                    ("recorded", &recorded.file_name),
                    ("connected", &connected.file_name),
                    ("hint", &hint),
                ],
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
        Err(StartError::Store(OpenError::NeedsV04Open { format })) => {
            return import::needs_v04_open(&catalog, &data_root, format.as_deref());
        }
        Err(StartError::Store(OpenError::ImportPending { new_root })) => {
            return fail(
                &catalog,
                2,
                MessageKey::CliImportPending,
                &[("new", &new_root.display().to_string())],
            );
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
            // 前の点検で止めた機能 (06 §3.3)。まだ点検していなければ、点検の後に知らせが来る。
            limited: core
                .probe_result(evorag_memory::ROLE_CHAT)
                .is_some_and(|p| !evorag_core::probe::issues(&p).is_empty()),
            recall: Some(report.recall),
            background: None,
            background_pending: 0,
            remote_llm: cfg.llm.allow_remote,
            forget: Some(evorag_types::api::ForgetState {
                mismatched: report.forget.mismatched.len(),
                journal_pending: report.forget.journal_pending,
                key_missing: report.forget.key_missing,
            }),
        },
    );
    app.notices.extend(update_notices);
    for w in &report.location_warnings {
        let m = app
            .catalog
            .with(MessageKey::CliLocationWarning, &[("detail", w)]);
        app.notices.push(m);
    }
    if let Some(unusual) = report.chat_model_recorded {
        let key = if unusual {
            MessageKey::CliModelRecordedUnusual
        } else {
            MessageKey::CliModelRecorded
        };
        let file = evorag_llm::file_name(&report.server.model_path).to_owned();
        let m = app.catalog.with(key, &[("file", &file)]);
        app.notices.push(m);
    }
    for role in &report.recorded_roles {
        let file = report
            .roles
            .iter()
            .find(|r| &r.role == role)
            .and_then(|r| r.connected.as_ref())
            .map(|c| c.0.clone())
            .unwrap_or_default();
        let m = app.catalog.with(
            MessageKey::CliRoleRecorded,
            &[("role", role), ("file", &file)],
        );
        app.notices.push(m);
    }
    for r in &report.roles {
        if let Some(m) = evorag_tui::app::role_notice(&app.catalog, r) {
            app.notices.push(m);
        }
    }
    for key in &report.ignored_config {
        let m = app
            .catalog
            .with(MessageKey::CliConfigObsolete, &[("key", key)]);
        app.notices.push(m);
    }
    // 起動時の compact を見送った・できなかった (03 §8)。次の起動でやり直すまで `doctor` も知らせる。
    match &report.store.compaction {
        evorag_store::Compaction::LowSpace { free, need } => {
            let mib = |b: u64| b.div_ceil(1024 * 1024).to_string();
            let m = app.catalog.with(
                MessageKey::CliCompactLowSpace,
                &[
                    ("free", &(free / (1024 * 1024)).to_string()),
                    ("need", &mib(*need)),
                ],
            );
            app.notices.push(m);
        }
        evorag_store::Compaction::Failed { reason } => {
            let m = app
                .catalog
                .with(MessageKey::CliCompactFailed, &[("detail", reason)]);
            app.notices.push(m);
        }
        _ => {}
    }
    // 忘れるの指示と合わないターンと、ジャーナルの書き直しの残り (04 §7.2・§7.8)。
    if !report.forget.mismatched.is_empty() {
        let m = app.catalog.with(
            MessageKey::CliForgetMismatched,
            &[("count", &report.forget.mismatched.len().to_string())],
        );
        app.notices.push(m);
    }
    if report.forget.key_missing {
        let m = app.catalog.key(MessageKey::CliForgetKeyMissing);
        app.notices.push(m);
    }
    if report.forget.journal_pending {
        let m = app.catalog.key(MessageKey::CliForgetJournalPending);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// すべての引数と下位の命令に、どの言語でも `--help` の文がある。英語の文に日本語が混じらない。
    ///
    /// verifies: AC-FR-908-1 (ヘルプの層: `--help` の文を UI の言語のカタログから当てる)
    #[test]
    fn every_argument_and_subcommand_has_help_in_every_language() {
        fn check(cmd: &clap::Command, lang: Language, path: &str, bad: &mut Vec<String>) {
            let about = cmd.get_about().map(ToString::to_string).unwrap_or_default();
            if about.is_empty() || (lang == Language::En && evorag_types::has_cjk(&about)) {
                bad.push(format!("{path}: about {about:?}"));
            }
            for a in cmd.get_arguments() {
                let id = a.get_id().as_str();
                if matches!(id, "help" | "version") {
                    continue;
                }
                let help = a.get_help().map(ToString::to_string).unwrap_or_default();
                if help.is_empty() || (lang == Language::En && evorag_types::has_cjk(&help)) {
                    bad.push(format!("{path} {id}: help {help:?}"));
                }
            }
            for s in cmd.get_subcommands() {
                if s.get_name() != "help" {
                    check(s, lang, &format!("{path} {}", s.get_name()), bad);
                }
            }
        }
        for lang in Language::ALL {
            let mut cmd = localized_command(&Catalog::new(lang));
            cmd.build();
            let mut bad = Vec::new();
            check(&cmd, lang, "evorag", &mut bad);
            assert!(bad.is_empty(), "{}: {bad:#?}", lang.as_str());
        }
        let en = localized_command(&Catalog::new(Language::En))
            .render_long_help()
            .to_string();
        assert!(en.contains("Chat with a local llama-server"), "{en}");
    }

    /// 設定に prompt.language が無く、データ根にも記録が無いとき、最初の起動は尋ねずに UI の言語を記録する。
    /// 記録した後は、設定を変えても記録した言語で動き、明示した prompt.language が記録と違うときだけ知らせる
    /// (07 §6.1)。
    #[test]
    fn a_fresh_data_root_records_the_ui_language_as_the_prompt_language() {
        use evorag_types::{SystemClock, UuidV7Gen};
        let start = |root: &std::path::Path, config: &str| {
            let cfg = config::parse(config).unwrap().config;
            let clock: Arc<dyn evorag_types::Clock> = Arc::new(SystemClock);
            let core_cfg = CoreConfig {
                data_root: root.to_path_buf(),
                root_format: RootFormat::V05,
                config_file: None,
                evorag_version: env!("CARGO_PKG_VERSION").to_owned(),
                prompt_language: cfg.prompt_language(),
                prompt_language_explicit: cfg.prompt_language_explicit(),
                generation: Generation::default(),
                request_extra: Default::default(),
                reserve_for_output: None,
                backup_dir: None,
                allow_unsafe_location: true,
                probe: evorag_core::Probe::system(),
                model_name: "test".into(),
                ignored_config: Vec::new(),
                embed: None,
                rerank: None,
                aux_request_extra: Default::default(),
                probe_models: false,
                sleep: evorag_core::SleepConfig::immediate(),
                memory_enabled: true,
                precompute: false,
                clock: clock.clone(),
                ids: Arc::new(UuidV7Gen::new(clock)),
            };
            let llm = evorag_llm::replay::ReplayClient::from_json(
                r#"{"props": {"build_info": "b1-test", "model_path": "/m/test.gguf", "total_slots": 1,
                              "default_generation_settings": {"n_ctx": 8192}},
                    "models": {"data": [{"id": "test", "meta": {"n_params": 1, "size": 2, "n_vocab": 3}}]},
                    "chats": []}"#,
            )
            .unwrap();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (core, _events, report) = Core::start(core_cfg, Arc::new(llm)).await.unwrap();
                core.shutdown().await;
                (report.prompt_language, report.prompt_language_mismatch)
            })
        };
        let en = "[ui]\nlanguage = \"en\"\n";
        assert_eq!(
            config::parse(en).unwrap().config.prompt_language(),
            PromptLanguage::En
        );
        assert_eq!(
            config::parse("").unwrap().config.prompt_language(),
            PromptLanguage::Ja
        );
        assert_eq!(
            config::parse("[ui]\nlanguage = \"en\"\n[prompt]\nlanguage = \"ja\"\n")
                .unwrap()
                .config
                .prompt_language(),
            PromptLanguage::Ja,
            "prompt.language wins"
        );

        let dir = evorag_store::testutil::TempDir::new("cli-prompt-lang");
        let root = dir.path().join("data");
        assert_eq!(start(&root, en), (PromptLanguage::En, false));
        // 記録した後は、UI の言語を変えても en のまま。UI の言語からの既定は、記録と違っても知らせない。
        assert_eq!(
            start(&root, "[ui]\nlanguage = \"ja\"\n"),
            (PromptLanguage::En, false)
        );
        // 設定に明示した prompt.language が記録と違うときだけ知らせる (記録した en のまま動く)。
        assert_eq!(
            start(&root, "[prompt]\nlanguage = \"ja\"\n"),
            (PromptLanguage::En, true)
        );
        assert_eq!(
            start(&root, "[prompt]\nlanguage = \"en\"\n"),
            (PromptLanguage::En, false)
        );
    }

    #[test]
    fn the_config_path_is_found_before_parsing() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            config_arg(&args(&["evorag", "doctor", "--config", "a.toml"])),
            Some(PathBuf::from("a.toml"))
        );
        assert_eq!(
            config_arg(&args(&["evorag", "--config=b.toml", "--help"])),
            Some(PathBuf::from("b.toml"))
        );
        assert_eq!(config_arg(&args(&["evorag", "--", "--config", "c"])), None);
        assert_eq!(config_arg(&args(&["evorag", "--config"])), None);
    }
}
