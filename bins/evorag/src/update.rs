//! `evorag update check|apply|rollback` と、TUI の起動の更新の片付け (12 §7.2・§7.3)。
//!
//! 流れは evorag-update の `flow` が持ち、ここは文言 (文言キー) と終了コード (07 §5) だけを決める。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;
use evorag_update::flow::{
    self, ApplyOutcome, CheckOutcome, Context, DataGuidance, DataVerdict, FlowError, Notice,
    ReleaseInfo, RollbackOutcome,
};
use evorag_update::net::{HttpFetcher, NetError, policy};
use evorag_update::swap::{
    self, Build, CleanupReport, Env, FileName, GapRecovery, Op, ProcessProbe, ResumeOutcome,
    ResumeReport, Startup, SwapError,
};
use evorag_update::{Keyring, Severity, TrustedKey, UpdateError, Version};

/// `evorag update` の操作。
pub enum Action {
    /// `check`。
    Check,
    /// `apply [版]`。
    Apply { version: Option<String> },
    /// `rollback`。
    Rollback,
}

/// 設定と置き場所。
pub struct Places<'a> {
    pub data_root: &'a Path,
    pub backup_dir: Option<&'a Path>,
    pub config_file: &'a Path,
    /// 状態のフォルダ (`update/` を置く)。決められなければ `None`。
    pub state_dir: Option<&'a Path>,
}

/// 今の版 (`CARGO_PKG_VERSION`) と、それが書く DB の形式の版。
fn current_build() -> Result<Build, String> {
    let version: Version = env!("CARGO_PKG_VERSION")
        .parse()
        .map_err(|e| format!("CARGO_PKG_VERSION: {e}"))?;
    Ok(Build::new(
        version,
        Some(evorag_store::FORMAT_VERSION.to_owned()),
    ))
}

fn print(catalog: &Catalog, key: MessageKey, args: &[(&str, &str)]) {
    eprintln!("{}", catalog.with(key, args));
}

fn files(names: &[FileName]) -> String {
    names
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `evorag update …` を実行する。
pub fn run(
    catalog: &Catalog,
    action: Action,
    trusted_keys: Vec<TrustedKey>,
    places: &Places<'_>,
) -> ExitCode {
    let build = match current_build() {
        Ok(b) => b,
        Err(e) => return fail(catalog, 1, MessageKey::CliError, &[("detail", &e)]),
    };
    let requested = match &action {
        Action::Apply {
            version: Some(text),
        } => match text.strip_prefix('v').unwrap_or(text).parse::<Version>() {
            Ok(v) => Some(v),
            Err(_) => {
                let shown = evorag_update::sanitize_for_display(text, 64);
                return fail(
                    catalog,
                    1,
                    MessageKey::CliUpdateBadVersion,
                    &[("version", &shown)],
                );
            }
        },
        _ => None,
    };
    let running_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return fail(
                catalog,
                1,
                MessageKey::CliError,
                &[("detail", &e.to_string())],
            );
        }
    };
    let needs_state = !matches!(action, Action::Check);
    let state_dir = match places.state_dir {
        Some(d) => d.to_path_buf(),
        None if needs_state => return fail(catalog, 1, MessageKey::CliUpdateNoStateDir, &[]),
        // check は状態のフォルダに触れない。
        None => PathBuf::new(),
    };
    let mut keys = Keyring::embedded();
    keys.extend(trusted_keys);
    let fetcher = HttpFetcher::new(build.version);
    let probe = ProcessProbe::new();
    let ctx = Context {
        current: &build,
        target: env!("EVORAG_TARGET"),
        channel: env!("EVORAG_CHANNEL"),
        release_date: option_env!("EVORAG_RELEASE_DATE"),
        keys: &keys,
        fetcher: &fetcher,
        probe: &probe,
        running_exe: &running_exe,
        state_dir: &state_dir,
        data_root: places.data_root,
        backup_dir: places.backup_dir,
        config_file: Some(places.config_file),
    };
    let install_dir = running_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut notify = |n: Notice| show_notice(catalog, &n, &install_dir, &build);
    let result = match action {
        Action::Check => flow::check(&ctx).map(|o| show_check(catalog, &o, &build)),
        Action::Apply { .. } => {
            flow::apply(&ctx, requested, &mut notify).map(|o| show_apply(catalog, &o))
        }
        Action::Rollback => flow::rollback(&ctx, &mut notify).map(|o| show_rollback(catalog, &o)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let state_file = state_dir.join("update").join("state.json");
            eprintln!("{}", flow_error(catalog, &e, &build, &state_file));
            ExitCode::from(e.exit_code())
        }
    }
}

fn fail(catalog: &Catalog, code: u8, key: MessageKey, args: &[(&str, &str)]) -> ExitCode {
    print(catalog, key, args);
    ExitCode::from(code)
}

/// 検証した更新の先を示す (12 §7.2 の apply の 5)。`notes` は文言の差し込みを通さずに、そのまま示す。
fn show_release(catalog: &Catalog, key: MessageKey, info: &ReleaseInfo, build: &Build) {
    let version = info.version.to_string();
    let date = info.date.to_string();
    print(catalog, key, &[("version", &version), ("date", &date)]);
    match &info.notes {
        Some(notes) => {
            eprintln!("{}", catalog.key(MessageKey::CliUpdateNotesHeader));
            for line in notes.lines() {
                eprintln!("  {line}");
            }
        }
        None => print(
            catalog,
            MessageKey::CliUpdateNotesPage,
            &[("url", &info.page_url)],
        ),
    }
    if let Some(b) = info.llama_server_min_build {
        print(
            catalog,
            MessageKey::CliUpdateLlamaMinBuild,
            &[("build", &b.to_string())],
        );
    }
    if let Some(min_write) = &info.db_min_write_warning {
        print(
            catalog,
            MessageKey::CliUpdateDbMinWrite,
            &[
                ("min_write", min_write),
                ("current", &build.version.to_string()),
            ],
        );
    }
    if let Some(id) = info.added_key {
        print(
            catalog,
            MessageKey::CliUpdateAddedKey,
            &[("key_id", &id.to_string())],
        );
    }
}

fn show_check(catalog: &Catalog, outcome: &CheckOutcome, build: &Build) {
    match outcome {
        CheckOutcome::UpToDate { current } => print(
            catalog,
            MessageKey::CliUpdateUpToDate,
            &[("current", &current.to_string())],
        ),
        CheckOutcome::Available(info) => {
            show_release(catalog, MessageKey::CliUpdateAvailable, info, build);
            eprintln!("{}", catalog.key(MessageKey::CliUpdateApplyHint));
        }
        CheckOutcome::UpgradeFirst {
            release,
            upgrade_from,
        } => {
            show_release(catalog, MessageKey::CliUpdateAvailable, release, build);
            print(
                catalog,
                MessageKey::CliUpdateUpgradeFirst,
                &[
                    ("current", &build.version.to_string()),
                    ("upgrade_from", &upgrade_from.to_string()),
                ],
            );
        }
        _ => {}
    }
}

fn show_apply(catalog: &Catalog, outcome: &ApplyOutcome) {
    match outcome {
        ApplyOutcome::UpToDate { current } => print(
            catalog,
            MessageKey::CliUpdateUpToDate,
            &[("current", &current.to_string())],
        ),
        ApplyOutcome::Applied { from, to, left } => {
            print(
                catalog,
                MessageKey::CliUpdateApplied,
                &[("from", &from.to_string()), ("to", &to.to_string())],
            );
            if !left.is_empty() {
                print(
                    catalog,
                    MessageKey::CliUpdateLeft,
                    &[("files", &files(left))],
                );
            }
        }
        _ => {}
    }
}

fn show_rollback(catalog: &Catalog, outcome: &RollbackOutcome) {
    print(
        catalog,
        MessageKey::CliUpdateRolledBack,
        &[
            ("from", &outcome.from.to_string()),
            ("to", &outcome.to.to_string()),
        ],
    );
    show_guidance(catalog, &outcome.guidance);
}

/// データの案内 (12 §7.2 の rollback の 3)。入れ替えの後に示す。
fn show_guidance(catalog: &Catalog, g: &DataGuidance) {
    match &g.verdict {
        DataVerdict::NoData => {}
        DataVerdict::Usable => eprintln!("{}", catalog.key(MessageKey::CliUpdateDataUsable)),
        DataVerdict::ReadOnly { format, min_write } => print(
            catalog,
            MessageKey::CliUpdateDataReadOnly,
            &[("format", format), ("min_write", min_write)],
        ),
        DataVerdict::Unopenable { format, min_read } => print(
            catalog,
            MessageKey::CliUpdateDataUnopenable,
            &[("format", format), ("min_read", min_read)],
        ),
        DataVerdict::Unknown => eprintln!("{}", catalog.key(MessageKey::CliUpdateDataUnknown)),
        _ => {}
    }
    if g.needs_restore() {
        if g.pre_op_backups.is_empty() {
            eprintln!("{}", catalog.key(MessageKey::CliUpdateDataNoBackup));
        } else {
            for b in &g.pre_op_backups {
                print(
                    catalog,
                    MessageKey::CliUpdateDataBackup,
                    &[("path", &b.display().to_string())],
                );
            }
            eprintln!("{}", catalog.key(MessageKey::CliUpdateDataJournal));
        }
        for c in &g.config_backups {
            print(
                catalog,
                MessageKey::CliUpdateConfigBackup,
                &[("path", &c.display().to_string())],
            );
        }
    }
}

fn show_notice(catalog: &Catalog, notice: &Notice, install_dir: &Path, build: &Build) {
    match notice {
        Notice::Release(info) => show_release(catalog, MessageKey::CliUpdateTarget, info, build),
        Notice::Downloading { name, size } => print(
            catalog,
            MessageKey::CliUpdateDownloading,
            &[("name", name), ("size", &size.to_string())],
        ),
        Notice::GapRecovery(g) => eprintln!("{}", gap_text(catalog, g, install_dir)),
        Notice::Resumed(r) => {
            for line in resumed_lines(catalog, r) {
                eprintln!("{line}");
            }
        }
        Notice::Cleaned(c) => eprintln!("{}", cleaned_text(catalog, c)),
        _ => {}
    }
}

fn gap_text(catalog: &Catalog, g: &GapRecovery, install_dir: &Path) -> String {
    let key = match g.op {
        Op::Apply => MessageKey::CliUpdateGapApply,
        Op::Rollback => MessageKey::CliUpdateGapRollback,
    };
    catalog.with(key, &[("dir", &install_dir.display().to_string())])
}

fn cleaned_text(catalog: &Catalog, c: &CleanupReport) -> String {
    let mut names: Vec<FileName> = c.removed.clone();
    if let Some(r) = c.restored_prev {
        names.push(r);
    }
    catalog.with(MessageKey::CliUpdateCleaned, &[("files", &files(&names))])
}

fn resumed_lines(catalog: &Catalog, r: &ResumeReport) -> Vec<String> {
    let op = r.op.to_string();
    let key = match r.outcome {
        ResumeOutcome::Completed => MessageKey::CliUpdateResumedCompleted,
        _ => MessageKey::CliUpdateResumedReverted,
    };
    let mut out = vec![catalog.with(key, &[("op", &op)])];
    if let ResumeOutcome::Reverted { check: Some(c) } = &r.outcome {
        out.push(catalog.with(MessageKey::CliUpdateReverted, &[("detail", &c.to_string())]));
    }
    if !r.untouched.is_empty() {
        out.push(catalog.with(
            MessageKey::CliUpdateResumeUntouched,
            &[("files", &files(&r.untouched))],
        ));
    }
    if !r.missing.is_empty() {
        out.push(catalog.with(
            MessageKey::CliUpdateResumeMissing,
            &[("files", &files(&r.missing))],
        ));
    }
    if !r.left.is_empty() {
        out.push(catalog.with(MessageKey::CliUpdateLeft, &[("files", &files(&r.left))]));
    }
    out
}

/// 断った・失敗した理由の文言。
fn flow_error(catalog: &Catalog, e: &FlowError, build: &Build, state_file: &Path) -> String {
    let manual = policy::RELEASES_PAGE;
    match e {
        FlowError::NotZipChannel { channel } => {
            catalog.with(MessageKey::CliUpdateNotZip, &[("channel", channel)])
        }
        FlowError::Swap(s) => swap_error(catalog, s, state_file),
        FlowError::DataRootInUse { path, pid } => catalog.with(
            MessageKey::CliUpdateDataRootInUse,
            &[
                ("path", &path.display().to_string()),
                (
                    "pid",
                    &pid.map_or_else(|| "?".to_owned(), |p| p.to_string()),
                ),
            ],
        ),
        FlowError::DataRootLock { path, source } => catalog.with(
            MessageKey::CliUpdateDataRootLock,
            &[
                ("path", &path.display().to_string()),
                ("detail", &source.to_string()),
            ],
        ),
        FlowError::NotNewerRequested { requested, current } => catalog.with(
            MessageKey::CliUpdateNotNewer,
            &[
                ("requested", &requested.to_string()),
                ("current", &current.to_string()),
            ],
        ),
        FlowError::Net(n) => net_error(catalog, n),
        FlowError::ListUnreadable(_) => {
            catalog.with(MessageKey::CliUpdateListUnreadable, &[("url", manual)])
        }
        FlowError::ReleaseFileMissing { version, what } => catalog.with(
            MessageKey::CliUpdateMissingFile,
            &[("version", &version.to_string()), ("what", what)],
        ),
        FlowError::Verify(UpdateError::UnknownSchema) => {
            catalog.with(MessageKey::CliUpdateUnknownSchema, &[("url", manual)])
        }
        FlowError::Verify(UpdateError::BelowUpgradeFrom { upgrade_from, .. }) => catalog.with(
            MessageKey::CliUpdateUpgradeFirst,
            &[
                ("current", &build.version.to_string()),
                ("upgrade_from", &upgrade_from.to_string()),
            ],
        ),
        FlowError::Verify(u) => {
            let key = match u.severity() {
                Severity::Tampered => MessageKey::CliUpdateTampered,
                Severity::Unusable => MessageKey::CliUpdateUnusable,
            };
            catalog.with(key, &[("detail", &u.to_string())])
        }
        FlowError::ArchiveLongerThanSigned { .. } => {
            catalog.with(MessageKey::CliUpdateTampered, &[("detail", &e.to_string())])
        }
        _ => catalog.with(MessageKey::CliUpdateFailed, &[("detail", &e.to_string())]),
    }
}

fn net_error(catalog: &Catalog, n: &NetError) -> String {
    let manual = policy::RELEASES_PAGE;
    match n {
        NetError::CpuUnsupported { missing } => catalog.with(
            MessageKey::CliUpdateCpu,
            &[("missing", &missing.join(", ")), ("url", manual)],
        ),
        NetError::UnsupportedArch { arch } => catalog.with(
            MessageKey::CliUpdateArch,
            &[("arch", arch), ("url", manual)],
        ),
        NetError::NoCertRoots { detail } => catalog.with(
            MessageKey::CliUpdateNoRoots,
            &[("detail", detail), ("url", manual)],
        ),
        NetError::RateLimited { reset: Some(t) } => catalog.with(
            MessageKey::CliUpdateRateLimited,
            &[("reset", &evorag_update::net::format_utc(*t))],
        ),
        NetError::RateLimited { reset: None } => {
            catalog.key(MessageKey::CliUpdateRateLimitedUnknown)
        }
        n => catalog.with(
            MessageKey::CliUpdateNetwork,
            &[("detail", &n.to_string()), ("url", manual)],
        ),
    }
}

/// 置き換え・巻き戻し・片付けの失敗の文言。手で戻す手順が要るものは、手順を添える。
fn swap_error(catalog: &Catalog, e: &SwapError, state_file: &Path) -> String {
    let path = |p: &Path| p.display().to_string();
    let text = match e {
        SwapError::ExeName { found } => catalog.with(
            MessageKey::CliUpdateExeName,
            &[("found", found), ("expected", swap::EXE_NAME)],
        ),
        SwapError::Busy => catalog.key(MessageKey::CliUpdateBusy),
        SwapError::ForeignPlan { dir } => {
            catalog.with(MessageKey::CliUpdateForeignPlan, &[("dir", &path(dir))])
        }
        SwapError::NoPrev => catalog.key(MessageKey::CliUpdateNoPrev),
        SwapError::PrevNotEvorag { first_line } => {
            catalog.with(MessageKey::CliUpdatePrevNotEvorag, &[("line", first_line)])
        }
        SwapError::Leftover { name, source } => catalog.with(
            MessageKey::CliUpdateLeftover,
            &[("name", name.as_str()), ("detail", &source.to_string())],
        ),
        SwapError::UnrelatedLeftover { name } => catalog.with(
            MessageKey::CliUpdateUnrelatedLeftover,
            &[("name", name.as_str())],
        ),
        SwapError::NotWritable { dir, source } => catalog.with(
            MessageKey::CliUpdateNotWritable,
            &[("dir", &path(dir)), ("detail", &source.to_string())],
        ),
        SwapError::WritableByOtherUsers { dir } => {
            catalog.with(MessageKey::CliUpdateOthersCanWrite, &[("dir", &path(dir))])
        }
        SwapError::Reverted(f) => {
            catalog.with(MessageKey::CliUpdateReverted, &[("detail", &f.to_string())])
        }
        SwapError::NotRecorded(inner) => catalog.with(
            MessageKey::CliUpdateNotRecorded,
            &[("detail", &inner.to_string())],
        ),
        // `SwapError::RevertedNotRecorded` (元に戻したが記録できなかった) は
        // `MessageKey::CliUpdateRevertedNotRecorded` で示す (PR2 の改訂で足す型)。
        e => catalog.with(MessageKey::CliUpdateFailed, &[("detail", &e.to_string())]),
    };
    if e.needs_manual_recovery() {
        let manual = catalog.with(MessageKey::CliUpdateManual, &[("state", &path(state_file))]);
        format!("{text}\n{manual}")
    } else {
        text
    }
}

/// TUI の起動の更新の片付け (12 §7.3 の「続きと片付け」) の結果。
pub enum StartupCleanup {
    /// 起動を続ける。示すこと (TUI の知らせ) を持つ。
    Continue(Vec<String>),
    /// 続きで、動いているプロセスの実行ファイルが入れ替わった。移行も書き込みもせずに、起動し直すよう
    /// 案内して終わる (終了コード 1)。
    Restart(String),
}

/// TUI の起動の片付け。データ根のロック (03 §9) を取って持ったまま、ストアを書き込みで開く前に行う。
/// 更新のロックを取れなければ (ほかの `update` が動いている) 何もしない。失敗しても起動は止めず、
/// 理由を示して進む。データ根をほかの evorag が使っていれば、片付けずに進む (起動がその理由で止まる)。
///
/// 入れ方が zip でない evorag (cargo・開発のビルド) は片付けない。計画を書く `apply`・`rollback` は zip の
/// evorag だけが動かすので、ほかの入れ方の evorag の置き場所には、続きも残りのファイルも無い。
pub fn startup_cleanup(catalog: &Catalog, data_root: &Path, state_dir: &Path) -> StartupCleanup {
    if env!("EVORAG_CHANNEL") != evorag_update::ZIP_CHANNEL {
        return StartupCleanup::Continue(Vec::new());
    }
    let _lock = if data_root.is_dir() {
        match evorag_store::lock::DataRootLock::acquire(data_root) {
            Ok(l) => Some(l),
            Err(_) => return StartupCleanup::Continue(Vec::new()),
        }
    } else {
        None
    };
    let Ok(exe) = std::env::current_exe() else {
        return StartupCleanup::Continue(Vec::new());
    };
    let probe = ProcessProbe::new();
    let mut env = Env::new(&probe);
    let state_file = state_dir.join("update").join("state.json");
    match swap::startup(&exe, state_dir, &mut env) {
        Ok(Startup::Resumed(report)) => {
            if running_replaced(&report) {
                return StartupCleanup::Restart(catalog.key(MessageKey::CliUpdateStartupRestart));
            }
            StartupCleanup::Continue(resumed_lines(catalog, &report))
        }
        Ok(Startup::Cleaned(c)) => {
            let mut out = Vec::new();
            if c.restored_prev.is_some() {
                out.push(cleaned_text(catalog, &c));
            }
            if !c.kept.is_empty() {
                out.push(catalog.with(
                    MessageKey::CliUpdateStartupKept,
                    &[("files", &files(&c.kept))],
                ));
            }
            StartupCleanup::Continue(out)
        }
        Ok(_) => StartupCleanup::Continue(Vec::new()),
        Err(e) => StartupCleanup::Continue(vec![catalog.with(
            MessageKey::CliUpdateStartupFailed,
            &[("detail", &swap_error(catalog, &e, &state_file))],
        )]),
    }
}

/// 続きで、動いているプロセスの実行ファイルが入れ替わったか (12 §7.3)。
// TODO(M3.5): 置き換えの核に `ResumeReport::running_replaced` が入ったら、その値を返す。それまでは偽。
const fn running_replaced(report: &ResumeReport) -> bool {
    let _ = report;
    false
}
