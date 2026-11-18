//! サブコマンドの入口 (07 §5)。v0.1 の M1.4 は、TUI を開く `evorag` だけ。
//!
//! 終了コード: 0 正常、1 一般のエラー、2 データ根が使えない (ロック・新しい形式)、
//! 3 llama-server に繋がらない、4 設定の誤り (07 §5)。

mod config;
mod doctor;
mod export;
mod setup;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, LazyLock};

use clap::Parser;
use evorag_core::{Core, CoreConfig, PromptLanguage, StartError};
use evorag_llm::{Endpoint, Generation, HttpClient, RetryPolicy};
use evorag_store::lock::LockError;
use evorag_store::{OpenError, ReadOnlyReason};
use evorag_tui::app::{App, Status};
use evorag_tui::catalog::Catalog;
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
        evorag_store::FORMAT_VERSION,
    )
});

#[derive(Parser)]
#[command(
    name = "evorag",
    version = VERSION.as_str(),
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
    /// 派生データ (チャンク・ノート・埋め込み・索引・点検の結果) を、会話と明示の操作から作り直す (先に退避する)
    Rebuild {
        /// 索引だけを作り直す
        #[arg(long, conflicts_with = "facts")]
        index: bool,
        /// 事実も会話から抽出し直す (llama-server を使う。事実への指示と検証済みの訂正は当て直す)
        #[arg(long)]
        facts: bool,
    },
    /// 退避から戻す (その後の会話はジャーナルから足す)
    Restore {
        /// 退避のフォルダ (backups/daily/<日付> など) か、その中の evorag.redb
        backup: PathBuf,
    },
    /// 副索引と関係を記録から作り直し、ジャーナルにあって DB に無い記録を戻し、忘れる・削除を当て直す (先に退避する)
    Repair,
    /// 本体の更新 (新しい版の確認・置き換え・巻き戻し)
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
}

#[derive(clap::Subcommand)]
enum UpdateAction {
    /// 新しい版があるかを確かめる (何も書き換えない)
    Check,
    /// 新しい版を取得し、署名を確かめてから置き換える (今の版は evorag.prev に残す)
    Apply {
        /// 入れる版 (省くと、いちばん新しい版)
        version: Option<String>,
    },
    /// evorag.prev に戻す (通信しない)
    Rollback,
}

#[derive(clap::Subcommand)]
enum BackupsAction {
    /// 退避の一覧 (新しい順)
    List,
    /// すべての退避 (日次・週次・移行と作り直しの前の退避) を消す
    Purge {
        /// 尋ねずに消す
        #[arg(long)]
        yes: bool,
    },
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
            let env = api_key_env.unwrap_or("(未設定)");
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
        Err(e) => return fail(&catalog, 4, MessageKey::CliConfigError, &[("detail", &e)]),
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
        let latest = evorag_store::restore::list_backups(&data_root, backup_dir.as_deref());
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
        started = rt.block_on(Core::start(make_cfg(), llm));
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
