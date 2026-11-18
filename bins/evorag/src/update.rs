//! `evorag update check|apply|rollback` と、TUI の起動の更新の片付け (12 §7.2・§7.3)。
//!
//! 流れは evorag-update の `flow` が持ち、ここは文言 (文言キー) と終了コード (07 §5) だけを決める。
//! 失敗の理由は、英語の文言 (`Display`) を差し込まずに、種類ごとの文言キーで示す (FR-907)。OS の失敗の
//! 文字列だけは、整えてから「詳細」の行に添える。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use evorag_store::lock::DataRootLock;
use evorag_tui::catalog::Catalog;
use evorag_types::MessageKey;
use evorag_update::flow::{
    self, ApplyOutcome, CheckOutcome, Context, DataGuidance, DataVerdict, FlowError, Notice,
    ReleaseInfo, RollbackOutcome,
};
use evorag_update::net::{HttpFetcher, NetError, policy};
use evorag_update::swap::{
    self, Build, CheckFailure, CleanupReport, Env, Failure, FileName, FileOp, GapRecovery, Op,
    OwnershipProblem, ProbeError, ProcessProbe, ResumeOutcome, ResumeReport, Startup, SwapError,
};
use evorag_update::{
    Keyring, Severity, SizedInput, TrustedKey, UpdateError, Version, VersionSource,
};

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
    /// 今の設定のファイルの形式の版 (`version`。書いていなければ 0。ファイルが無ければ `None`)。
    pub config_version: Option<u32>,
    /// 状態のフォルダ (`update/` を置く)。決められなければ `None`。
    pub state_dir: Option<&'a Path>,
}

/// cargo で入れ直すコマンド (12 §7.4)。
const CARGO_INSTALL: &str = "cargo install --locked --git https://github.com/evoref/evorag --tag";

/// 「詳細」の行に添える OS の失敗の文字列の長さの上限 (バイト)。
const DETAIL_MAX_BYTES: usize = 512;

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

/// `apply` の版の指定を読む。先頭の `v` は省いてよい (`v0.4.0` と `0.4.0` は同じ)。
fn parse_requested(text: &str) -> Option<Version> {
    text.strip_prefix('v').unwrap_or(text).parse().ok()
}

fn files(names: &[FileName]) -> String {
    names
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn shown(p: &Path) -> String {
    p.display().to_string()
}

/// `evorag update …` を実行する。
pub fn run(
    catalog: &Catalog,
    action: Action,
    trusted_keys: Vec<TrustedKey>,
    places: &Places<'_>,
) -> ExitCode {
    let fail = |code: u8, key: MessageKey, args: &[(&str, &str)]| {
        eprintln!("{}", catalog.with(key, args));
        ExitCode::from(code)
    };
    let build = match current_build() {
        Ok(b) => b,
        Err(e) => return fail(1, MessageKey::CliError, &[("detail", &e)]),
    };
    let requested = match &action {
        Action::Apply {
            version: Some(text),
        } => match parse_requested(text) {
            Some(v) => Some(v),
            None => {
                let shown = evorag_update::sanitize_for_display(text, 64);
                return fail(1, MessageKey::CliUpdateBadVersion, &[("version", &shown)]);
            }
        },
        _ => None,
    };
    let running_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return fail(1, MessageKey::CliError, &[("detail", &e.to_string())]),
    };
    let needs_state = !matches!(action, Action::Check);
    let state_dir = match places.state_dir {
        Some(d) => d.to_path_buf(),
        None if needs_state => return fail(1, MessageKey::CliUpdateNoStateDir, &[]),
        // check は状態のフォルダに触れない。
        None => PathBuf::new(),
    };
    let mut keys = Keyring::embedded();
    keys.extend(trusted_keys);
    let fetcher = HttpFetcher::new(build.version);
    let probe = ProcessProbe::new();
    let channel = env!("EVORAG_CHANNEL");
    let ctx = Context {
        current: &build,
        target: env!("EVORAG_TARGET"),
        channel,
        release_date: option_env!("EVORAG_RELEASE_DATE"),
        keys: &keys,
        fetcher: &fetcher,
        probe: &probe,
        running_exe: &running_exe,
        state_dir: &state_dir,
        data_root: places.data_root,
        backup_dir: places.backup_dir,
        config_file: Some(places.config_file),
        config_version: places.config_version,
    };
    let mut notify = |n: Notice| {
        for line in notice_lines(catalog, &n, &build) {
            eprintln!("{line}");
        }
    };
    let result = match action {
        Action::Check => flow::check(&ctx).map(|o| check_lines(catalog, &o, &build, channel)),
        Action::Apply { .. } => {
            flow::apply(&ctx, requested, &mut notify).map(|o| apply_lines(catalog, &o))
        }
        Action::Rollback => flow::rollback(&ctx, &mut notify).map(|o| rollback_lines(catalog, &o)),
    };
    let state_file = state_dir.join("update").join("state.json");
    let (lines, code) = conclude(catalog, result, &build, &state_file);
    for line in lines {
        eprintln!("{line}");
    }
    ExitCode::from(code)
}

/// 流れの結果を、示す行と終了コード (07 §5) にする。
fn conclude(
    catalog: &Catalog,
    result: Result<Vec<String>, FlowError>,
    build: &Build,
    state_file: &Path,
) -> (Vec<String>, u8) {
    match result {
        Ok(lines) => (lines, 0),
        Err(e) => (
            vec![flow_error(catalog, &e, build, state_file)],
            e.exit_code(),
        ),
    }
}

/// 検証した更新の先 (12 §7.2 の apply の 5)。`notes` は文言の差し込みを通さずに、そのまま示す。
fn release_lines(
    catalog: &Catalog,
    key: MessageKey,
    info: &ReleaseInfo,
    build: &Build,
) -> Vec<String> {
    let version = info.version.to_string();
    let date = info.date.to_string();
    let mut out = vec![catalog.with(key, &[("version", &version), ("date", &date)])];
    match &info.notes {
        Some(notes) => {
            out.push(catalog.key(MessageKey::CliUpdateNotesHeader));
            out.extend(notes.lines().map(|line| format!("  {line}")));
        }
        None => out.push(catalog.with(MessageKey::CliUpdateNotesPage, &[("url", &info.page_url)])),
    }
    if let Some(b) = info.llama_server_min_build {
        out.push(catalog.with(
            MessageKey::CliUpdateLlamaMinBuild,
            &[("build", &b.to_string())],
        ));
    }
    if let Some(min_write) = &info.db_min_write_warning {
        out.push(catalog.with(
            MessageKey::CliUpdateDbMinWrite,
            &[
                ("min_write", min_write),
                ("current", &build.version.to_string()),
            ],
        ));
    }
    if let Some(id) = info.added_key {
        out.push(catalog.with(
            MessageKey::CliUpdateAddedKey,
            &[("key_id", &id.to_string())],
        ));
    }
    out
}

/// cargo で `version` を入れ直すコマンド。
fn cargo_install(version: Version) -> String {
    format!("{CARGO_INSTALL} {} evorag", version.tag())
}

/// `check` の結果。入れ方が zip でなければ、`apply` ではなく、その入れ方で入れ直す案内を示す (12 §7.4)。
fn check_lines(
    catalog: &Catalog,
    outcome: &CheckOutcome,
    build: &Build,
    channel: &str,
) -> Vec<String> {
    let zip = channel == evorag_update::ZIP_CHANNEL;
    let reinstall = |version: Version| {
        if channel == "cargo" {
            catalog.with(
                MessageKey::CliUpdateReinstall,
                &[("command", &cargo_install(version))],
            )
        } else {
            catalog.with(
                MessageKey::CliUpdateReinstallChannel,
                &[("channel", channel), ("version", &version.to_string())],
            )
        }
    };
    let current = build.version.to_string();
    match outcome {
        CheckOutcome::UpToDate { current } => vec![catalog.with(
            MessageKey::CliUpdateUpToDate,
            &[("current", &current.to_string())],
        )],
        CheckOutcome::Available(info) => {
            let mut out = release_lines(catalog, MessageKey::CliUpdateAvailable, info, build);
            if zip {
                out.push(catalog.key(MessageKey::CliUpdateApplyHint));
            } else {
                out.push(reinstall(info.version));
            }
            out
        }
        CheckOutcome::UpgradeFirst {
            release,
            upgrade_from,
        } => {
            let mut out = release_lines(catalog, MessageKey::CliUpdateAvailable, release, build);
            let first = upgrade_from.to_string();
            if zip {
                out.push(catalog.with(
                    MessageKey::CliUpdateUpgradeFirst,
                    &[("current", &current), ("upgrade_from", &first)],
                ));
            } else {
                out.push(catalog.with(
                    MessageKey::CliUpdateUpgradeFirstReinstall,
                    &[("current", &current), ("upgrade_from", &first)],
                ));
                if channel == "cargo" {
                    // 先に入れる版を先に示す。
                    out.push(catalog.key(MessageKey::CliUpdateReinstallSteps));
                    out.push(format!("  {}", cargo_install(*upgrade_from)));
                    out.push(format!("  {}", cargo_install(release.version)));
                } else {
                    out.push(reinstall(*upgrade_from));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

fn apply_lines(catalog: &Catalog, outcome: &ApplyOutcome) -> Vec<String> {
    match outcome {
        ApplyOutcome::UpToDate { current } => vec![catalog.with(
            MessageKey::CliUpdateUpToDate,
            &[("current", &current.to_string())],
        )],
        ApplyOutcome::Applied { from, to, left } => {
            let mut out = vec![catalog.with(
                MessageKey::CliUpdateApplied,
                &[("from", &from.to_string()), ("to", &to.to_string())],
            )];
            if !left.is_empty() {
                out.push(catalog.with(MessageKey::CliUpdateLeft, &[("files", &files(left))]));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `rollback` の結果 (12 §7.2 の rollback の 3)。データの案内は入れ替えの後に示す。
fn rollback_lines(catalog: &Catalog, outcome: &RollbackOutcome) -> Vec<String> {
    let mut out = vec![catalog.with(
        MessageKey::CliUpdateRolledBack,
        &[
            ("from", &outcome.from.to_string()),
            ("to", &outcome.to.to_string()),
        ],
    )];
    out.extend(guidance_lines(catalog, &outcome.guidance));
    out
}

/// データの案内 (12 §7.2 の rollback の 3)。設定の写しは、DB の案内に関わらず示す。
fn guidance_lines(catalog: &Catalog, g: &DataGuidance) -> Vec<String> {
    let mut out = Vec::new();
    match &g.verdict {
        DataVerdict::NoData => {}
        DataVerdict::Usable => out.push(catalog.key(MessageKey::CliUpdateDataUsable)),
        DataVerdict::ReadOnly { format, min_write } => out.push(catalog.with(
            MessageKey::CliUpdateDataReadOnly,
            &[("format", format), ("min_write", min_write)],
        )),
        DataVerdict::Unopenable { format, min_read } => out.push(catalog.with(
            MessageKey::CliUpdateDataUnopenable,
            &[("format", format), ("min_read", min_read)],
        )),
        DataVerdict::Unknown => out.push(catalog.key(MessageKey::CliUpdateDataUnknown)),
        _ => {}
    }
    if g.needs_restore() {
        if g.pre_op_backups.is_empty() {
            out.push(catalog.key(MessageKey::CliUpdateDataNoBackup));
        } else {
            for b in &g.pre_op_backups {
                out.push(catalog.with(MessageKey::CliUpdateDataBackup, &[("path", &shown(b))]));
            }
            out.push(catalog.key(MessageKey::CliUpdateDataJournal));
        }
    }
    if let Some(c) = &g.config_backup {
        out.push(catalog.with(MessageKey::CliUpdateConfigBackup, &[("path", &shown(c))]));
    }
    out
}

fn notice_lines(catalog: &Catalog, notice: &Notice, build: &Build) -> Vec<String> {
    match notice {
        Notice::Release(info) => release_lines(catalog, MessageKey::CliUpdateTarget, info, build),
        Notice::Downloading { name, size } => vec![catalog.with(
            MessageKey::CliUpdateDownloading,
            &[("name", name), ("size", &size.to_string())],
        )],
        Notice::GapRecovery { recovery, dir } => vec![gap_text(catalog, recovery, dir)],
        Notice::Resumed(r) => resumed_lines(catalog, r),
        Notice::Cleaned(c) => vec![cleaned_text(catalog, c)],
        _ => Vec::new(),
    }
}

/// Windows の隙間の戻し方。`dir` はインストール先 (正規化したフォルダ)。
fn gap_text(catalog: &Catalog, g: &GapRecovery, dir: &Path) -> String {
    let key = match g.op {
        Op::Apply => MessageKey::CliUpdateGapApply,
        Op::Rollback => MessageKey::CliUpdateGapRollback,
    };
    catalog.with(key, &[("dir", &shown(dir))])
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
        let reason = check_reason(catalog, c);
        out.push(catalog.with(MessageKey::CliUpdateReverted, &[("reason", &reason.text)]));
        out.extend(reason.detail_line(catalog));
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

/// 失敗の理由の句と、添える OS の失敗の詳細 (整えたもの)。
struct Reason {
    text: String,
    detail: Option<String>,
}

impl Reason {
    fn new(text: String) -> Self {
        Self { text, detail: None }
    }

    fn key(catalog: &Catalog, key: MessageKey) -> Self {
        Self::new(catalog.key(key))
    }

    fn with(catalog: &Catalog, key: MessageKey, args: &[(&str, &str)]) -> Self {
        Self::new(catalog.with(key, args))
    }

    /// OS の失敗などの文字列を、詳細として添える (制御文字を除き、1 行にして、長さを限る)。
    fn detail(mut self, raw: &str) -> Self {
        let one_line = raw.replace(['\r', '\n'], " ");
        self.detail = Some(evorag_update::sanitize_for_display(
            &one_line,
            DETAIL_MAX_BYTES,
        ));
        self
    }

    /// 入れ子の理由の詳細を、外側の理由に引き継ぐ。
    fn keep_detail(mut self, inner: Option<String>) -> Self {
        if self.detail.is_none() {
            self.detail = inner;
        }
        self
    }

    fn detail_line(&self, catalog: &Catalog) -> Option<String> {
        self.detail
            .as_ref()
            .map(|d| catalog.with(MessageKey::CliUpdateDetail, &[("detail", d)]))
    }
}

/// 文言と、あれば詳細の行をつなぐ。
fn with_detail(catalog: &Catalog, text: String, reason: &Reason) -> String {
    match reason.detail_line(catalog) {
        Some(d) => format!("{text}\n{d}"),
        None => text,
    }
}

/// インストール先を移すときの案内 (OS ごと)。
fn install_advice(catalog: &Catalog) -> String {
    catalog.key(if cfg!(windows) {
        MessageKey::CliUpdateAdviceWindows
    } else {
        MessageKey::CliUpdateAdviceUnix
    })
}

/// 断った・失敗した理由の文言。
fn flow_error(catalog: &Catalog, e: &FlowError, build: &Build, state_file: &Path) -> String {
    let manual = policy::RELEASES_PAGE;
    match e {
        FlowError::NotZipChannel { channel } => {
            catalog.with(MessageKey::CliUpdateNotZip, &[("channel", channel)])
        }
        FlowError::Swap(s) => swap_error(catalog, s, state_file),
        FlowError::RunAgain { op } => {
            catalog.with(MessageKey::CliUpdateRunAgain, &[("op", &op.to_string())])
        }
        FlowError::DataRootInUse { path, pid } => catalog.with(
            MessageKey::CliUpdateDataRootInUse,
            &[
                ("path", &shown(path)),
                (
                    "pid",
                    &pid.map_or_else(|| "?".to_owned(), |p| p.to_string()),
                ),
            ],
        ),
        FlowError::DataRootLock { path, source } => {
            let text = catalog.with(MessageKey::CliUpdateDataRootLock, &[("path", &shown(path))]);
            let reason = Reason::new(String::new()).detail(&source.to_string());
            with_detail(catalog, text, &reason)
        }
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
            let reason = verify_reason(catalog, u);
            with_detail(
                catalog,
                catalog.with(key, &[("reason", &reason.text)]),
                &reason,
            )
        }
        FlowError::ArchiveLongerThanSigned { size } => {
            let reason = Reason::with(
                catalog,
                MessageKey::CliUpdateVerifyArchiveLonger,
                &[("size", &size.to_string())],
            );
            catalog.with(MessageKey::CliUpdateTampered, &[("reason", &reason.text)])
        }
        _ => catalog.with(
            MessageKey::CliUpdateFailed,
            &[("reason", &catalog.key(MessageKey::CliUpdateReasonUnknown))],
        ),
    }
}

/// 検証で断った理由の句。
fn verify_reason(catalog: &Catalog, e: &UpdateError) -> Reason {
    use MessageKey as K;
    let r = |key| Reason::key(catalog, key);
    let w = |key, args: &[(&str, &str)]| Reason::with(catalog, key, args);
    match e {
        UpdateError::SignatureMalformed => r(K::CliUpdateVerifySignatureMalformed),
        UpdateError::LegacySignature => r(K::CliUpdateVerifyLegacySignature),
        UpdateError::UnknownKey { key_id } => w(
            K::CliUpdateVerifyUnknownKey,
            &[("key_id", &key_id.to_string())],
        ),
        UpdateError::BadSignature { key_id } => w(
            K::CliUpdateVerifyBadSignature,
            &[("key_id", &key_id.to_string())],
        ),
        UpdateError::TrustedCommentMismatch => r(K::CliUpdateVerifyTrustedComment),
        UpdateError::UnknownSchema => r(K::CliUpdateVerifyUnknownSchema),
        UpdateError::Malformed { reason } => r(K::CliUpdateVerifyMalformed).detail(reason),
        UpdateError::NameMismatch => r(K::CliUpdateVerifyNameMismatch),
        UpdateError::VersionMismatch {
            source_of,
            found,
            expected,
        } => {
            let source = catalog.key(match source_of {
                VersionSource::TrustedComment => K::CliUpdateVerifySourceComment,
                VersionSource::Tag => K::CliUpdateVerifySourceTag,
                VersionSource::Requested => K::CliUpdateVerifySourceRequested,
            });
            w(
                K::CliUpdateVerifyVersionMismatch,
                &[
                    ("found", found),
                    ("source", &source),
                    ("expected", expected),
                ],
            )
        }
        UpdateError::NotNewer { version, current } => w(
            K::CliUpdateVerifyNotNewer,
            &[
                ("version", &version.to_string()),
                ("current", &current.to_string()),
            ],
        ),
        UpdateError::DateTooOld { date, current } => w(
            K::CliUpdateVerifyDateTooOld,
            &[
                ("date", &date.to_string()),
                ("current", &current.to_string()),
            ],
        ),
        UpdateError::BelowUpgradeFrom { upgrade_from, .. } => w(
            K::CliUpdateVerifyBelowUpgradeFrom,
            &[("upgrade_from", &upgrade_from.to_string())],
        ),
        UpdateError::NoArtifact { target } => {
            w(K::CliUpdateVerifyNoArtifact, &[("target", target)])
        }
        UpdateError::AmbiguousArtifact { target, count } => w(
            K::CliUpdateVerifyAmbiguousArtifact,
            &[("target", target), ("count", &count.to_string())],
        ),
        UpdateError::TooLarge { what, limit } => {
            let what = match what {
                SizedInput::Manifest => "release.json".to_owned(),
                SizedInput::Signature => "release.json.minisig".to_owned(),
                SizedInput::Archive => catalog.key(K::CliUpdateVerifyArchive),
            };
            w(
                K::CliUpdateVerifyTooLarge,
                &[("what", &what), ("limit", &limit.to_string())],
            )
        }
        UpdateError::BuildDateUnreadable => r(K::CliUpdateVerifyBuildDate),
        UpdateError::ArchiveSizeMismatch { expected, actual } => w(
            K::CliUpdateVerifyArchiveSize,
            &[
                ("actual", &actual.to_string()),
                ("expected", &expected.to_string()),
            ],
        ),
        UpdateError::ArchiveDigestMismatch => r(K::CliUpdateVerifyArchiveDigest),
        UpdateError::ArchiveUnreadable { reason } => {
            r(K::CliUpdateVerifyArchiveUnreadable).detail(reason)
        }
        UpdateError::ExeMissing => r(K::CliUpdateVerifyExeMissing),
        UpdateError::ExeNotRegularFile => r(K::CliUpdateVerifyExeNotRegular),
        UpdateError::DuplicateEntries => r(K::CliUpdateVerifyDuplicateEntries),
        UpdateError::ExeTooLarge { limit } => w(
            K::CliUpdateVerifyExeTooLarge,
            &[("limit", &limit.to_string())],
        ),
        UpdateError::UnpackedTooLarge { limit } => w(
            K::CliUpdateVerifyUnpackedTooLarge,
            &[("limit", &limit.to_string())],
        ),
        UpdateError::ExeDigestMismatch => r(K::CliUpdateVerifyExeDigest),
        _ => r(K::CliUpdateReasonUnknown),
    }
}

fn net_error(catalog: &Catalog, n: &NetError) -> String {
    use MessageKey as K;
    let manual = policy::RELEASES_PAGE;
    let reason = match n {
        NetError::CpuUnsupported { missing } => {
            let missing = if missing.is_empty() {
                "?".to_owned()
            } else {
                missing.join(", ")
            };
            return catalog.with(K::CliUpdateCpu, &[("missing", &missing), ("url", manual)]);
        }
        NetError::UnsupportedArch { arch } => {
            return catalog.with(K::CliUpdateArch, &[("arch", arch), ("url", manual)]);
        }
        NetError::NoCertRoots { detail } => {
            let text = catalog.with(K::CliUpdateNoRoots, &[("url", manual)]);
            return with_detail(catalog, text, &Reason::new(String::new()).detail(detail));
        }
        NetError::UnsupportedProxy(p) => {
            return catalog.with(
                K::CliUpdateUnsupportedProxy,
                &[("var", p.var), ("url", manual)],
            );
        }
        NetError::RateLimited { reset: Some(t) } => {
            return catalog.with(
                K::CliUpdateRateLimited,
                &[("reset", &evorag_update::net::format_utc(*t))],
            );
        }
        NetError::RateLimited { reset: None } => {
            return catalog.key(K::CliUpdateRateLimitedUnknown);
        }
        NetError::Refused(_) => Reason::key(catalog, K::CliUpdateNetRefused),
        NetError::TooManyRedirects => Reason::key(catalog, K::CliUpdateNetTooManyRedirects),
        NetError::RedirectWithoutLocation { .. } => {
            Reason::key(catalog, K::CliUpdateNetBadRedirect)
        }
        NetError::Connect { detail } => Reason::key(catalog, K::CliUpdateNetConnect).detail(detail),
        NetError::TimedOut { limit } => Reason::with(
            catalog,
            K::CliUpdateNetTimedOut,
            &[("seconds", &limit.as_secs().to_string())],
        ),
        NetError::TooLarge { limit } => Reason::with(
            catalog,
            K::CliUpdateNetTooLarge,
            &[("limit", &limit.to_string())],
        ),
        NetError::NotFound => Reason::key(catalog, K::CliUpdateNetNotFound),
        NetError::Status { status } => Reason::with(
            catalog,
            K::CliUpdateNetStatus,
            &[("status", &status.to_string())],
        ),
        _ => Reason::key(catalog, K::CliUpdateReasonUnknown),
    };
    let text = catalog.with(
        K::CliUpdateNetwork,
        &[("reason", &reason.text), ("url", manual)],
    );
    with_detail(catalog, text, &reason)
}

/// ファイルの操作の名前。
fn op_name(catalog: &Catalog, op: FileOp) -> String {
    use MessageKey as K;
    catalog.key(match op {
        FileOp::Open => K::CliUpdateOpOpen,
        FileOp::Create => K::CliUpdateOpCreate,
        FileOp::Lock => K::CliUpdateOpLock,
        FileOp::Read => K::CliUpdateOpRead,
        FileOp::Write => K::CliUpdateOpWrite,
        FileOp::Rename => K::CliUpdateOpRename,
        FileOp::Remove => K::CliUpdateOpRemove,
        FileOp::Sync => K::CliUpdateOpSync,
        _ => K::CliUpdateOpOther,
    })
}

/// `--version` を動かせなかった理由の句。
fn probe_reason(catalog: &Catalog, e: &ProbeError) -> Reason {
    use MessageKey as K;
    match e {
        ProbeError::Spawn(io) => {
            Reason::key(catalog, K::CliUpdateReasonProbeSpawn).detail(&io.to_string())
        }
        ProbeError::TimedOut(d) => Reason::with(
            catalog,
            K::CliUpdateReasonProbeTimeout,
            &[("seconds", &d.as_secs().to_string())],
        ),
        ProbeError::Failed(status) => Reason::with(
            catalog,
            K::CliUpdateReasonProbeFailed,
            &[(
                "status",
                &status
                    .code()
                    .map_or_else(|| "?".to_owned(), |c| c.to_string()),
            )],
        ),
        ProbeError::TooMuchOutput => Reason::key(catalog, K::CliUpdateReasonProbeOutput),
        _ => Reason::key(catalog, K::CliUpdateReasonUnknown),
    }
}

/// 置き換えの後の確かめに通らなかった理由の句。
fn check_reason(catalog: &Catalog, c: &CheckFailure) -> Reason {
    match c {
        CheckFailure::WrongVersion {
            first_line,
            expected,
        } => Reason::with(
            catalog,
            MessageKey::CliUpdateReasonCheckVersion,
            &[("line", first_line), ("expected", expected)],
        ),
        CheckFailure::Probe(p) => probe_reason(catalog, p),
        _ => Reason::key(catalog, MessageKey::CliUpdateReasonUnknown),
    }
}

/// 置き換えを元に戻した原因の句。
fn failure_reason(catalog: &Catalog, f: &Failure) -> Reason {
    match f {
        Failure::File { op, name, source } => Reason::with(
            catalog,
            MessageKey::CliUpdateReasonFile,
            &[("name", name.as_str()), ("op", &op_name(catalog, *op))],
        )
        .detail(&source.to_string()),
        Failure::State(inner) => {
            let inner = swap_reason(catalog, inner);
            Reason::with(
                catalog,
                MessageKey::CliUpdateReasonState,
                &[("reason", &inner.text)],
            )
            .keep_detail(inner.detail)
        }
        Failure::Check(c) => check_reason(catalog, c),
        Failure::Other(inner) => swap_reason(catalog, inner),
        _ => Reason::key(catalog, MessageKey::CliUpdateReasonUnknown),
    }
}

/// 置き換え・巻き戻し・片付けの失敗の句 (入れ子の理由と、`failed` の文言に使う)。
fn swap_reason(catalog: &Catalog, e: &SwapError) -> Reason {
    use MessageKey as K;
    let r = |key| Reason::key(catalog, key);
    let w = |key, args: &[(&str, &str)]| Reason::with(catalog, key, args);
    match e {
        SwapError::ExeName { found } => w(
            K::CliUpdateReasonExeName,
            &[("found", found), ("expected", swap::EXE_NAME)],
        ),
        SwapError::Busy => r(K::CliUpdateReasonBusy),
        SwapError::StateUnreadable { reason } => {
            r(K::CliUpdateReasonStateUnreadable).detail(reason)
        }
        SwapError::ForeignPlan { dir } => w(K::CliUpdateReasonForeignPlan, &[("dir", &shown(dir))]),
        SwapError::PlanPending => r(K::CliUpdateReasonPlanPending),
        SwapError::UnknownRole => r(K::CliUpdateReasonUnknownRole),
        SwapError::ForeignFile { name } => {
            w(K::CliUpdateReasonForeignFile, &[("name", name.as_str())])
        }
        SwapError::NothingToRestore => r(K::CliUpdateReasonNothingToRestore),
        SwapError::Leftover { name, source } => {
            w(K::CliUpdateReasonLeftover, &[("name", name.as_str())]).detail(&source.to_string())
        }
        SwapError::UnrelatedLeftover { name } => w(
            K::CliUpdateReasonUnrelatedLeftover,
            &[("name", name.as_str())],
        ),
        SwapError::BlockingFile { path } => {
            w(K::CliUpdateReasonBlockingFile, &[("path", &shown(path))])
        }
        SwapError::WritableByOtherUsers { dir } => {
            w(K::CliUpdateReasonOthersCanWrite, &[("path", &shown(dir))])
        }
        SwapError::OwnedByOtherUser { dir } => {
            w(K::CliUpdateReasonOwnedByOther, &[("path", &shown(dir))])
        }
        SwapError::NotWritable { dir, source } => {
            w(K::CliUpdateReasonNotWritable, &[("dir", &shown(dir))]).detail(&source.to_string())
        }
        SwapError::NotRegularFile { name } => {
            w(K::CliUpdateReasonNotRegularFile, &[("name", name.as_str())])
        }
        SwapError::UnsafeOwnership { path, problem } => match problem {
            OwnershipProblem::WritableByOthers => {
                w(K::CliUpdateReasonOthersCanWrite, &[("path", &shown(path))])
            }
            _ => w(K::CliUpdateReasonUnsafeOwner, &[("path", &shown(path))]),
        },
        SwapError::NoPrev => r(K::CliUpdateReasonNoPrev),
        SwapError::PrevNotEvorag { first_line } => {
            w(K::CliUpdateReasonPrevNotEvorag, &[("line", first_line)])
        }
        SwapError::PrevProbe(p) => {
            let inner = probe_reason(catalog, p);
            w(K::CliUpdateReasonPrevProbe, &[("reason", &inner.text)]).keep_detail(inner.detail)
        }
        SwapError::Changed { name } => w(K::CliUpdateReasonChanged, &[("name", name.as_str())]),
        SwapError::SameFile => r(K::CliUpdateReasonSameFile),
        SwapError::WrittenDigestMismatch => r(K::CliUpdateReasonWrittenDigest),
        SwapError::CopiedDigestMismatch { name } => {
            w(K::CliUpdateReasonCopiedDigest, &[("name", name.as_str())])
        }
        SwapError::Reverted(f) => {
            let inner = failure_reason(catalog, f);
            w(K::CliUpdateReasonReverted, &[("reason", &inner.text)]).keep_detail(inner.detail)
        }
        SwapError::NotRecorded(inner) => {
            let inner = swap_reason(catalog, inner);
            w(K::CliUpdateReasonNotRecorded, &[("reason", &inner.text)]).keep_detail(inner.detail)
        }
        SwapError::RevertedNotRecorded { record, .. } => {
            let inner = swap_reason(catalog, record);
            w(
                K::CliUpdateReasonRevertedNotRecorded,
                &[("reason", &inner.text)],
            )
            .keep_detail(inner.detail)
        }
        SwapError::RevertFailed { cause, revert } => {
            let cause = failure_reason(catalog, cause);
            let revert = swap_reason(catalog, revert);
            w(
                K::CliUpdateReasonRevertFailed,
                &[("cause", &cause.text), ("reason", &revert.text)],
            )
            .keep_detail(revert.detail.or(cause.detail))
        }
        SwapError::File { op, name, source } => w(
            K::CliUpdateReasonFile,
            &[("name", name.as_str()), ("op", &op_name(catalog, *op))],
        )
        .detail(&source.to_string()),
        SwapError::Io { op, path, source } => w(
            K::CliUpdateReasonIo,
            &[("path", &shown(path)), ("op", &op_name(catalog, *op))],
        )
        .detail(&source.to_string()),
        _ => r(K::CliUpdateReasonUnknown),
    }
}

/// 置き換え・巻き戻し・片付けの失敗の文言。利用者が手を打てるものは専用の文言にし、手で戻す手順が要る
/// ものは手順を添える。
fn swap_error(catalog: &Catalog, e: &SwapError, state_file: &Path) -> String {
    use MessageKey as K;
    let reason = swap_reason(catalog, e);
    let text = match e {
        SwapError::ExeName { found } => catalog.with(
            K::CliUpdateExeName,
            &[("found", found), ("expected", swap::EXE_NAME)],
        ),
        SwapError::Busy => catalog.key(K::CliUpdateBusy),
        SwapError::ForeignPlan { dir } => {
            catalog.with(K::CliUpdateForeignPlan, &[("dir", &shown(dir))])
        }
        SwapError::NoPrev => catalog.key(K::CliUpdateNoPrev),
        SwapError::PrevNotEvorag { first_line } => {
            catalog.with(K::CliUpdatePrevNotEvorag, &[("line", first_line)])
        }
        SwapError::Leftover { name, .. } => {
            catalog.with(K::CliUpdateLeftover, &[("name", name.as_str())])
        }
        SwapError::UnrelatedLeftover { name } => {
            catalog.with(K::CliUpdateUnrelatedLeftover, &[("name", name.as_str())])
        }
        SwapError::NotWritable { dir, .. } => catalog.with(
            K::CliUpdateNotWritable,
            &[("dir", &shown(dir)), ("advice", &install_advice(catalog))],
        ),
        SwapError::WritableByOtherUsers { dir } => catalog.with(
            K::CliUpdateOthersCanWrite,
            &[("path", &shown(dir)), ("advice", &install_advice(catalog))],
        ),
        SwapError::OwnedByOtherUser { dir } => catalog.with(
            K::CliUpdateOwnedByOther,
            &[("path", &shown(dir)), ("advice", &install_advice(catalog))],
        ),
        SwapError::UnsafeOwnership { path, problem } => {
            let key = match problem {
                OwnershipProblem::WritableByOthers => K::CliUpdateOthersCanWrite,
                _ => K::CliUpdateUnsafeOwner,
            };
            catalog.with(
                key,
                &[("path", &shown(path)), ("advice", &install_advice(catalog))],
            )
        }
        SwapError::Reverted(f) => {
            let cause = failure_reason(catalog, f);
            catalog.with(K::CliUpdateReverted, &[("reason", &cause.text)])
        }
        SwapError::NotRecorded(inner) => {
            let inner = swap_reason(catalog, inner);
            catalog.with(K::CliUpdateNotRecorded, &[("reason", &inner.text)])
        }
        SwapError::RevertedNotRecorded { cause, record } => {
            let record = swap_reason(catalog, record);
            match cause {
                Some(c) => {
                    let cause = failure_reason(catalog, c);
                    catalog.with(
                        K::CliUpdateRevertedNotRecordedCause,
                        &[("cause", &cause.text), ("reason", &record.text)],
                    )
                }
                None => catalog.with(K::CliUpdateRevertedNotRecorded, &[("reason", &record.text)]),
            }
        }
        SwapError::RevertFailed { cause, revert } => {
            let cause = failure_reason(catalog, cause);
            let revert = swap_reason(catalog, revert);
            catalog.with(
                K::CliUpdateRevertFailed,
                &[("cause", &cause.text), ("reason", &revert.text)],
            )
        }
        _ => catalog.with(K::CliUpdateFailed, &[("reason", &reason.text)]),
    };
    let text = with_detail(catalog, text, &reason);
    if e.needs_manual_recovery() {
        let manual = catalog.with(K::CliUpdateManual, &[("state", &shown(state_file))]);
        format!("{text}\n{manual}")
    } else {
        text
    }
}

/// TUI の起動の更新の片付け (12 §7.3 の「続きと片付け」) の結果。
pub enum StartupCleanup {
    /// 起動を続ける。
    Continue {
        /// 示すこと (TUI の知らせ)。
        notices: Vec<String>,
        /// 片付けの間に持ったデータ根のロック。放さずにストアを開くのに渡す (データ根が無い・ほかの
        /// evorag が使っているときは `None`)。
        lock: Option<DataRootLock>,
    },
    /// 続きの後に、動いているプロセスの実行ファイルが、インストール先のものと違う (どちらの向きでも)。
    /// 設定の移行もストアを開くこともせずに、起動し直すよう案内して終わる (終了コード 1)。
    Restart(String),
}

/// TUI の起動の片付け。データ根のロック (03 §9) を取って持ったまま、設定の移行とストアを開く前に行う。
///
/// - データ根のロックは、入れ方に関わらず取り、[`StartupCleanup::Continue`] で返す (呼ぶ側がそのまま
///   ストアを開くのに渡し、放して取り直す隙間を作らない)。データ根をほかの evorag が使っていれば、
///   片付けずに進む (起動がその理由で止まる)。
/// - 片付けは、入れ方が zip の evorag だけが行う。計画を書く `apply`・`rollback` は zip の evorag だけが
///   動かすので、ほかの入れ方 (cargo・開発のビルド) の置き場所には、続きも残りのファイルも無い。
/// - 更新のロックを取れなければ (ほかの `update` が動いている) 何もしない。失敗しても起動は止めず、
///   理由を示して進む。
pub fn startup_cleanup(catalog: &Catalog, data_root: &Path, state_dir: &Path) -> StartupCleanup {
    let state_file = state_dir.join("update").join("state.json");
    startup_cleanup_with(
        catalog,
        data_root,
        env!("EVORAG_CHANNEL"),
        &state_file,
        || {
            let exe = std::env::current_exe().ok()?;
            let probe = ProcessProbe::new();
            let mut env = Env::new(&probe);
            Some(swap::startup(&exe, state_dir, &mut env))
        },
    )
}

/// [`startup_cleanup`] の、入れ方と片付けの本体 (`cleanup`) を受け取る形。`cleanup` が `None` を返すのは、
/// 動いている実行ファイルが分からないとき (片付けない)。
fn startup_cleanup_with(
    catalog: &Catalog,
    data_root: &Path,
    channel: &str,
    state_file: &Path,
    cleanup: impl FnOnce() -> Option<Result<Startup, SwapError>>,
) -> StartupCleanup {
    let lock = if data_root.is_dir() {
        match DataRootLock::acquire(data_root) {
            Ok(l) => Some(l),
            Err(_) => {
                return StartupCleanup::Continue {
                    notices: Vec::new(),
                    lock: None,
                };
            }
        }
    } else {
        None
    };
    let cont = |notices| StartupCleanup::Continue { notices, lock };
    if channel != evorag_update::ZIP_CHANNEL {
        return cont(Vec::new());
    }
    match cleanup() {
        None | Some(Ok(Startup::Busy)) => cont(Vec::new()),
        Some(Ok(Startup::Resumed(report))) if report.running_replaced => {
            // 何を済ませたかを示してから、起動し直すよう案内する。
            let mut lines = resumed_lines(catalog, &report);
            lines.push(catalog.key(MessageKey::CliUpdateStartupRestart));
            StartupCleanup::Restart(lines.join("\n"))
        }
        Some(Ok(Startup::Resumed(report))) => cont(resumed_lines(catalog, &report)),
        Some(Ok(Startup::Cleaned(c))) => {
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
            cont(out)
        }
        Some(Ok(_)) => cont(Vec::new()),
        Some(Err(e)) => {
            let reason = swap_error(catalog, &e, state_file);
            cont(vec![catalog.with(
                MessageKey::CliUpdateStartupFailed,
                &[("reason", &reason)],
            )])
        }
    }
}

#[cfg(test)]
mod tests {
    use evorag_store::lock::LockError;
    use evorag_store::testutil::TempDir;
    use evorag_update::flow::RollbackOutcome;

    use super::*;

    fn catalog() -> Catalog {
        Catalog::load("ja")
    }

    fn v(s: &str) -> Version {
        s.parse().expect("a test version")
    }

    fn build() -> Build {
        Build::new(v("0.3.0"), Some("0.3".to_owned()))
    }

    /// 試験のデータ根を、権限を決めて作る (umask に頼らない)。
    fn data_root(dir: &TempDir) -> PathBuf {
        let root = dir.path().join("data");
        evorag_store::fsutil::create_private_dir(&root).expect("mkdir");
        root
    }

    fn held_elsewhere(root: &Path) -> bool {
        matches!(DataRootLock::acquire(root), Err(LockError::InUse { .. }))
    }

    /// 起動の片付けは、データ根のロックを取って持ったまま片付け、そのロックを返す。zip でない入れ方では
    /// 片付けない (ロックは取る)。
    #[test]
    fn startup_cleanup_takes_the_lock_and_skips_non_zip_channels() {
        let dir = TempDir::new("cli-startup-lock");
        let root = data_root(&dir);
        let state = dir.path().join("state.json");
        let c = catalog();

        let outcome = startup_cleanup_with(&c, &root, "cargo", &state, || {
            panic!("a non-zip build must not clean up")
        });
        let StartupCleanup::Continue { notices, lock } = outcome else {
            panic!("a non-zip build continues");
        };
        assert!(notices.is_empty());
        assert!(lock.is_some(), "the lock is taken on any channel");
        assert!(held_elsewhere(&root), "the lock is still held");
        drop(lock);

        let mut ran = false;
        let outcome = startup_cleanup_with(&c, &root, "zip", &state, || {
            ran = true;
            assert!(held_elsewhere(&root), "cleanup runs with the lock held");
            Some(Ok(Startup::Busy))
        });
        assert!(ran);
        let StartupCleanup::Continue { lock, .. } = outcome else {
            panic!("busy continues");
        };
        assert!(lock.is_some() && held_elsewhere(&root));
        drop(lock);

        // ほかの evorag が使っていれば、片付けずに進む (起動がその理由で止まる)。
        let other = DataRootLock::acquire(&root).expect("lock");
        let outcome = startup_cleanup_with(&c, &root, "zip", &state, || {
            panic!("must not clean up while another evorag runs")
        });
        assert!(matches!(
            outcome,
            StartupCleanup::Continue { lock: None, .. }
        ));
        drop(other);
    }

    /// 続きの後に、動いている evorag がインストール先のものと違えば (どちらの向きでも)、起動し直すよう
    /// 案内して止まる。同じなら続ける。
    #[test]
    fn startup_cleanup_restarts_when_the_running_evorag_was_replaced() {
        let dir = TempDir::new("cli-startup-restart");
        let root = data_root(&dir);
        let state = dir.path().join("state.json");
        let c = catalog();
        for outcome in [
            ResumeOutcome::Completed,
            ResumeOutcome::Reverted { check: None },
        ] {
            let restart = startup_cleanup_with(&c, &root, "zip", &state, || {
                Some(Ok(Startup::Resumed(ResumeReport::for_test(
                    Op::Apply,
                    outcome,
                    true,
                ))))
            });
            let StartupCleanup::Restart(text) = restart else {
                panic!("a replaced running evorag must restart");
            };
            assert!(text.contains("もう一度起動してください"), "{text}");
            assert!(text.contains("apply"), "{text}");
        }
        let cont = startup_cleanup_with(&c, &root, "zip", &state, || {
            Some(Ok(Startup::Resumed(ResumeReport::for_test(
                Op::Rollback,
                ResumeOutcome::Completed,
                false,
            ))))
        });
        let StartupCleanup::Continue { notices, lock } = cont else {
            panic!("continue");
        };
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(lock.is_some());
    }

    /// verifies: AC-FR-805-1 (戻した版が書けないときに、移行の前の退避のパスと `evorag restore` を示して
    /// 0 で終わる)
    #[test]
    fn rollback_shows_the_pre_op_backup_and_restore_and_ends_with_0() {
        let c = catalog();
        let backup = PathBuf::from("data")
            .join("backups")
            .join("pre-op")
            .join("20261201-000000");
        let outcome = RollbackOutcome::for_test(
            v("0.3.0"),
            v("0.4.0"),
            DataGuidance::for_test(
                DataVerdict::ReadOnly {
                    format: "0.3".into(),
                    min_write: "0.4".into(),
                },
                vec![backup.clone()],
                None,
            ),
        );
        let (lines, code) = conclude(
            &c,
            Ok(rollback_lines(&c, &outcome)),
            &build(),
            Path::new("state.json"),
        );
        assert_eq!(code, 0);
        let text = lines.join("\n");
        assert!(text.contains(&backup.display().to_string()), "{text}");
        assert!(
            text.contains(&format!("evorag restore \"{}\"", backup.display())),
            "{text}"
        );
        assert!(text.contains("0.3.0") && text.contains("0.4.0"), "{text}");
    }

    /// 設定の写しは、DB の案内に関わらず示す。写しが無い (設定の形式の版が変わっていない) ときは示さない。
    #[test]
    fn the_config_backup_is_shown_regardless_of_the_data_guidance() {
        let c = catalog();
        let bak = PathBuf::from("config").join("config.toml.bak-v0");
        for verdict in [DataVerdict::Usable, DataVerdict::NoData] {
            let g = DataGuidance::for_test(verdict.clone(), Vec::new(), Some(bak.clone()));
            let text = guidance_lines(&c, &g).join("\n");
            assert!(
                text.contains(&bak.display().to_string()),
                "{verdict:?}: {text}"
            );
            let g = DataGuidance::for_test(verdict.clone(), Vec::new(), None);
            assert!(
                !guidance_lines(&c, &g)
                    .join("\n")
                    .contains("config.toml.bak"),
                "{verdict:?}"
            );
        }
    }

    /// 入れ方が zip でない evorag の check は、`apply` ではなく、その入れ方で入れ直す案内を示す。
    /// `upgrade_from` を下回るときは、先に入れる版を先に示す。
    #[test]
    fn check_on_a_non_zip_build_shows_how_to_reinstall() {
        let c = catalog();
        let lines = check_lines(&c, &available("0.4.0"), &build(), "cargo").join("\n");
        assert!(
            lines.contains(
                "cargo install --locked --git https://github.com/evoref/evorag --tag v0.4.0 evorag"
            ),
            "{lines}"
        );
        assert!(!lines.contains("evorag update apply"), "{lines}");

        let zip = check_lines(&c, &available("0.4.0"), &build(), "zip").join("\n");
        assert!(zip.contains("evorag update apply"), "{zip}");
        assert!(!zip.contains("cargo install"), "{zip}");

        let first = CheckOutcome::UpgradeFirst {
            release: info("0.5.0"),
            upgrade_from: v("0.4.0"),
        };
        let lines = check_lines(&c, &first, &build(), "cargo").join("\n");
        let a = lines.find("--tag v0.4.0").expect("the first step");
        let b = lines.find("--tag v0.5.0").expect("the second step");
        assert!(a < b, "{lines}");
        assert!(!lines.contains("evorag update apply"), "{lines}");

        let other = check_lines(&c, &first, &build(), "winget").join("\n");
        assert!(
            other.contains("winget") && other.contains("0.4.0"),
            "{other}"
        );
        assert!(!other.contains("evorag update apply"), "{other}");
    }

    fn info(version: &str) -> ReleaseInfo {
        ReleaseInfo::for_test(v(version), "2026-12-01".parse().expect("a date"))
    }

    fn available(version: &str) -> CheckOutcome {
        CheckOutcome::Available(info(version))
    }

    #[test]
    fn a_leading_v_is_accepted_in_the_apply_version() {
        assert_eq!(parse_requested("v0.4.0"), Some(v("0.4.0")));
        assert_eq!(parse_requested("0.4.0"), Some(v("0.4.0")));
        for bad in ["latest", "vv0.4.0", "V0.4.0", "0.4", ""] {
            assert_eq!(parse_requested(bad), None, "{bad}");
        }
    }

    /// 失敗の文言に、英語の理由 (`Display`) を差し込まない (FR-907)。OS の失敗は「詳細」の行に添える。
    #[test]
    fn failures_are_shown_in_japanese_with_os_details_on_their_own_line() {
        let c = catalog();
        let state = Path::new("state.json");
        let os = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access is denied.");
        let e = SwapError::NotWritable {
            dir: PathBuf::from("inst"),
            source: os,
        };
        let text = swap_error(&c, &e, state);
        assert!(text.contains("書き込めません"), "{text}");
        let advice = if cfg!(windows) {
            "%LOCALAPPDATA%\\Programs\\evorag"
        } else {
            "~/.local/bin"
        };
        assert!(text.contains(advice), "{text}");
        let last = text.lines().last().expect("lines");
        assert!(last.contains("詳細: Access is denied."), "{text}");
        assert!(!text.contains("is not writable"), "{text}");

        for e in [
            SwapError::OwnedByOtherUser {
                dir: PathBuf::from("inst"),
            },
            SwapError::WritableByOtherUsers {
                dir: PathBuf::from("inst"),
            },
        ] {
            let text = swap_error(&c, &e, state);
            assert!(text.contains(advice), "{text}");
            assert!(!text.contains("other user"), "{text}");
        }

        let e = SwapError::CopiedDigestMismatch {
            name: FileName::Tmp,
        };
        let text = swap_error(&c, &e, state);
        assert!(
            text.contains("evorag.tmp") && text.contains("写し"),
            "{text}"
        );
        assert!(!text.contains("does not match"), "{text}");

        let e = SwapError::RevertedNotRecorded {
            cause: None,
            record: Box::new(SwapError::Io {
                op: FileOp::Rename,
                path: PathBuf::from("state.json"),
                source: std::io::Error::other("disk full"),
            }),
        };
        let text = swap_error(&c, &e, state);
        assert!(text.contains("元に戻しましたが"), "{text}");
        assert!(text.contains("改名"), "{text}");
        assert!(text.contains("詳細: disk full"), "{text}");
        assert!(!text.contains("recording it failed"), "{text}");

        // 手で戻す手順には、Windows の拡張子の無いファイルの動かし方を含める。
        let text = swap_error(&c, &SwapError::UnknownRole, state);
        assert!(text.contains("evorag.exe という名前で写して"), "{text}");

        let e = FlowError::Verify(UpdateError::ArchiveDigestMismatch);
        let text = flow_error(&c, &e, &build(), state);
        assert!(text.contains("SHA-256 が合いません"), "{text}");
        assert!(!text.contains("does not match"), "{text}");

        let text = net_error(
            &c,
            &NetError::Connect {
                detail: "connection refused\r\nnext".into(),
            },
        );
        assert!(text.contains("接続できませんでした"), "{text}");
        assert!(text.contains("詳細: connection refused  next"), "{text}");
        let text = net_error(
            &c,
            &NetError::UnsupportedProxy(evorag_update::net::ProxyRefused {
                var: "ALL_PROXY",
                problem: evorag_update::net::ProxyProblem::Socks,
            }),
        );
        assert!(
            text.contains("ALL_PROXY") && text.contains("SOCKS"),
            "{text}"
        );
    }

    /// Windows の隙間の案内は、正規化したインストール先を示す。
    #[test]
    fn the_gap_guidance_shows_the_given_folder() {
        let c = catalog();
        let dir = PathBuf::from("canonical").join("install");
        let text = gap_text(&c, &GapRecovery::for_op(Op::Apply), &dir);
        assert!(text.contains(&dir.display().to_string()), "{text}");
    }
}
