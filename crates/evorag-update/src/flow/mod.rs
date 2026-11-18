//! `evorag update check`・`apply`・`rollback` の流れ (12 §7.2)。
//!
//! 通信は [`Fetcher`]、`--version` の動かし方は [`VersionProbe`] で受け取る (試験は偽物を渡す)。示す
//! ことは [`Notice`] と結果の型で返し、文言は呼ぶ側 (bin) が文言キーで作る。終了コードは
//! [`FlowError::exit_code`] (07 §5)。
//!
//! - `check`: 通信の前の確かめ (CPU の機能と証明書のルート) だけをして通信する。ロックを取らず、
//!   データ根・インストール先・`update/` に触れない。
//! - `apply`・`rollback`: 始める前の確かめ (1〜9) を順に行い、1〜8 では通信しない。データ根のロックは、
//!   apply は手順 8 から、rollback は手順 3 から終わりまで持つ。

mod data;
mod releases;

use std::path::{Path, PathBuf};

use evorag_store::lock::{DataRootLock, LockError};

pub use self::data::{DataGuidance, DataVerdict};
pub use self::releases::{ListUnreadable, latest_version};
use crate::net::{Fetcher, NetError, Resource, policy};
use crate::swap::{
    Build, CleanupReport, Env, GapRecovery, Installation, Op, Purpose, ResumeReport, Strictness,
    SwapError, VersionProbe,
};
use crate::{
    Expectations, KeyId, KeySource, Keyring, MAX_ARCHIVE_BYTES, Manifest, ReleaseDate, Signer,
    SizedInput, UpdateError, VerifiedRelease, Version, ZIP_CHANNEL, release_date_of_build,
    verify_signature,
};

/// 更新の流れに渡すもの。
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// 今の evorag の版と、それが書く DB の形式の版。
    pub current: &'a Build,
    /// 今の対象 (target triple、`EVORAG_TARGET`)。
    pub target: &'a str,
    /// 入れ方 (`EVORAG_CHANNEL`、12 §7.4)。
    pub channel: &'a str,
    /// 埋め込んだリリースの日付 (`EVORAG_RELEASE_DATE`)。開発のビルドでは `None`。
    pub release_date: Option<&'a str>,
    /// 署名を確かめる鍵 (埋め込んだ鍵と `update.trusted_keys`)。
    pub keys: &'a Keyring,
    /// 取得の口。
    pub fetcher: &'a dyn Fetcher,
    /// `--version` の動かし方。
    pub probe: &'a dyn VersionProbe,
    /// 動いている実行ファイル ([`std::env::current_exe`])。
    pub running_exe: &'a Path,
    /// 状態のフォルダ (`update/` を置く)。
    pub state_dir: &'a Path,
    /// 設定のデータ根 (`--data-root`・`storage.data_root`)。
    pub data_root: &'a Path,
    /// 設定の `storage.backup_dir`。
    pub backup_dir: Option<&'a Path>,
    /// 設定のファイル (`config.toml.bak-v<旧版>` を探す)。
    pub config_file: Option<&'a Path>,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("current", self.current)
            .field("target", &self.target)
            .field("channel", &self.channel)
            .field("running_exe", &self.running_exe)
            .field("state_dir", &self.state_dir)
            .field("data_root", &self.data_root)
            .finish_non_exhaustive()
    }
}

/// 検証した更新の先 (12 §7.2 の apply の 5)。示す文字列は、検証した `release.json` から取ったものだけ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReleaseInfo {
    /// 版。
    pub version: Version,
    /// リリースの日付。
    pub date: ReleaseDate,
    /// 変更の要点 (制御文字と双方向の書式の文字を除き、4 KiB で切ったもの)。
    pub notes: Option<String>,
    /// リリースのページ (`notes` が無いときに示す)。
    pub page_url: String,
    /// 対応する llama-server の build 番号の下限。
    pub llama_server_min_build: Option<u64>,
    /// この版で移行した後の `format.min_write` が、今の版の書く形式より新しいときの、その値 (警告する)。
    pub db_min_write_warning: Option<String>,
    /// 設定で足した鍵で署名が通ったときの、その鍵の ID (警告する、12 §5.2)。
    pub added_key: Option<KeyId>,
}

impl ReleaseInfo {
    fn new(manifest: &Manifest, signer: Signer, current: &Build) -> Self {
        let newer = |min_write: &str| {
            let parse = |s: &str| s.parse::<evorag_store::version::Version>().ok();
            match (
                parse(min_write),
                current.db_format.as_deref().and_then(parse),
            ) {
                (Some(m), Some(c)) => m > c,
                // 比べられないときは、警告する側に倒す。
                _ => true,
            }
        };
        Self {
            version: manifest.version(),
            date: manifest.date(),
            notes: manifest.notes().map(str::to_owned),
            page_url: policy::release_page_url(manifest.version()),
            llama_server_min_build: manifest.llama_server_min_build(),
            db_min_write_warning: manifest
                .db_min_write()
                .filter(|m| newer(m))
                .map(str::to_owned),
            added_key: (signer.source() == KeySource::Added).then(|| signer.key_id()),
        }
    }
}

/// 流れの途中で示すこと。
#[derive(Debug)]
#[non_exhaustive]
pub enum Notice {
    /// 残った計画の続きを済ませた (始める前の確かめの 4)。
    Resumed(ResumeReport),
    /// 名前で片付けた (始める前の確かめの 5)。何かを動かした・消したときだけ。
    Cleaned(CleanupReport),
    /// 検証した更新の先 (apply の 5)。配布物を取得する前に示す。
    Release(ReleaseInfo),
    /// 配布物を取得する (apply の 6)。
    Downloading {
        /// 配布物の名前 (検証した `release.json` の値)。
        name: String,
        /// 大きさ (バイト)。
        size: u64,
    },
    /// Windows の隙間の戻し方 (12 §7.3)。インストール先のファイルを動かす前に示す。
    GapRecovery(GapRecovery),
}

/// `check` の結果 (どれも終了コード 0)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckOutcome {
    /// 今の版より新しい版が無い。
    UpToDate {
        /// 今の版。
        current: Version,
    },
    /// 新しい版がある。
    Available(ReleaseInfo),
    /// 新しい版があるが、今の版が `upgrade_from` を下回る。先に `upgrade_from` を入れるよう案内する。
    UpgradeFirst {
        /// 新しい版。
        release: ReleaseInfo,
        /// 先に入れる版。
        upgrade_from: Version,
    },
}

/// `apply` の結果 (どれも終了コード 0)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyOutcome {
    /// 版を省いて、今が最新。
    UpToDate {
        /// 今の版。
        current: Version,
    },
    /// 置き換えた。今の実行ファイルは `evorag.prev` に残した。
    Applied {
        /// 前の版。
        from: Version,
        /// 新しい版。
        to: Version,
        /// 消せずに残ったファイル (次の片付けで消す)。
        left: Vec<crate::swap::FileName>,
    },
}

/// `rollback` の結果 (終了コード 0。データの案内に警告があっても 0)。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RollbackOutcome {
    /// 戻した版。
    pub to: Version,
    /// 戻す前の版 (今は `evorag.prev`)。
    pub from: Version,
    /// データの案内。
    pub guidance: DataGuidance,
}

/// 更新の流れで断った・失敗した理由。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FlowError {
    /// 入れ方が zip でない (12 §7.4)。その管理の仕組みでの更新を案内する。
    #[error("evorag was installed via {channel}, not the zip archive")]
    NotZipChannel {
        /// 入れ方。
        channel: String,
    },
    /// 置き換え・巻き戻し・片付けの失敗 (始める前の確かめを含む)。
    #[error(transparent)]
    Swap(#[from] SwapError),
    /// データ根をほかの evorag が使っている。
    #[error("the data root {} is in use by another evorag", path.display())]
    DataRootInUse {
        /// データ根。
        path: PathBuf,
        /// 使っているプロセス (分かれば)。
        pid: Option<u32>,
    },
    /// データ根のロックを取れない (OS の失敗)。
    #[error("cannot lock the data root {}: {source}", path.display())]
    DataRootLock {
        /// データ根。
        path: PathBuf,
        /// OS の失敗。
        #[source]
        source: std::io::Error,
    },
    /// 指定した版が今と同じか古い (通信の前に断る。古い版に戻すのは `rollback`)。
    #[error("the requested version {requested} is not newer than the current version {current}")]
    NotNewerRequested {
        /// 指定した版。
        requested: Version,
        /// 今の版。
        current: Version,
    },
    /// 通信の失敗 (通信の前の確かめを含む)。
    #[error(transparent)]
    Net(#[from] NetError),
    /// リリースの一覧を読めない。
    #[error(transparent)]
    ListUnreadable(#[from] ListUnreadable),
    /// その版の `release.json` か `.minisig` が無い (無い版・取り下げた版)。
    #[error("version {version} has no {what}")]
    ReleaseFileMissing {
        /// 版。
        version: Version,
        /// 無いファイル (`release.json` か `release.json.minisig`)。
        what: &'static str,
    },
    /// 検証で断った。
    #[error(transparent)]
    Verify(#[from] UpdateError),
    /// 配布物が、署名した `size` を超えて届いた (大きさの不一致として扱う)。
    #[error("the archive is longer than its signed size {size}")]
    ArchiveLongerThanSigned {
        /// `release.json` の `size`。
        size: u64,
    },
}

impl FlowError {
    /// 07 §5 の終了コード。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::DataRootInUse { .. } | Self::DataRootLock { .. } => 2,
            Self::Verify(e) => e.exit_code(),
            Self::ArchiveLongerThanSigned { .. } => 5,
            Self::Swap(e) => e.exit_code(),
            Self::Net(e) => e.exit_code(),
            Self::NotZipChannel { .. }
            | Self::NotNewerRequested { .. }
            | Self::ListUnreadable(_)
            | Self::ReleaseFileMissing { .. } => 1,
        }
    }
}

/// `evorag update check` (12 §7.2)。ロックを取らず、データ根・インストール先・`update/` に触れない。
///
/// # Errors
///
/// 通信の失敗、一覧を読めない、検証で断った (`upgrade_from` を下回るときは断らずに
/// [`CheckOutcome::UpgradeFirst`])。
pub fn check(ctx: &Context<'_>) -> Result<CheckOutcome, FlowError> {
    ctx.fetcher.preflight()?;
    let current = ctx.current.version;
    let Some(version) = newest(ctx)? else {
        return Ok(CheckOutcome::UpToDate { current });
    };
    match fetch_release(ctx, version, None) {
        Ok(release) => Ok(CheckOutcome::Available(ReleaseInfo::new(
            release.manifest(),
            release.signer(),
            ctx.current,
        ))),
        Err(FlowError::Verify(UpdateError::BelowUpgradeFrom {
            upgrade_from,
            release,
            signer,
        })) => Ok(CheckOutcome::UpgradeFirst {
            release: ReleaseInfo::new(&release, signer, ctx.current),
            upgrade_from,
        }),
        Err(e) => Err(e),
    }
}

/// `evorag update apply [版]` (12 §7.2)。
///
/// # Errors
///
/// 始める前の確かめで断った、通信の失敗、検証で断った、置き換えに失敗した ([`FlowError`])。
pub fn apply(
    ctx: &Context<'_>,
    requested: Option<Version>,
    notify: &mut dyn FnMut(Notice),
) -> Result<ApplyOutcome, FlowError> {
    let current = ctx.current.version;
    let mut inst = begin(ctx, notify)?;
    // 8: 指定した版が今と同じか古ければ、通信の前に断る。
    if let Some(requested) = requested
        && requested <= current
    {
        return Err(FlowError::NotNewerRequested { requested, current });
    }
    // 9: CPU の機能と証明書のルート。ここまでは通信しない。
    ctx.fetcher.preflight()?;
    // 1: 版を決める。
    let version = match requested {
        Some(v) => v,
        None => match newest(ctx)? {
            Some(v) => v,
            None => return Ok(ApplyOutcome::UpToDate { current }),
        },
    };
    // 2〜4: 取得して検証する。
    let release = fetch_release(ctx, version, requested)?;
    // 5: 示す。
    notify(Notice::Release(ReleaseInfo::new(
        release.manifest(),
        release.signer(),
        ctx.current,
    )));
    // 6: 配布物を取得し、大きさと SHA-256 を確かめる。
    let artifact = release.artifact();
    let size = artifact.size();
    if size > MAX_ARCHIVE_BYTES {
        return Err(UpdateError::TooLarge {
            what: SizedInput::Archive,
            limit: MAX_ARCHIVE_BYTES,
        }
        .into());
    }
    notify(Notice::Downloading {
        name: artifact.name().to_owned(),
        size,
    });
    let bytes = ctx
        .fetcher
        .fetch(
            &policy::download_url(version, artifact.name()),
            Resource::Archive { size },
        )
        .map_err(|e| match e {
            NetError::TooLarge { .. } => FlowError::ArchiveLongerThanSigned { size },
            e => FlowError::Net(e),
        })?;
    let archive = release.verify_archive(bytes)?;
    // 7: 実行ファイルを取り出す。
    let exe = archive.extract_exe()?;
    // 8〜11: データ根のロックを取り、終わりまで持って置き換える。
    let _data_lock = lock_data_root(ctx)?;
    let next = Build::new(version, release.manifest().db_format().map(str::to_owned));
    notify_gap(Op::Apply, notify);
    let mut env = Env::new(ctx.probe);
    let report = inst.apply(&exe, ctx.current, &next, &mut env)?;
    Ok(ApplyOutcome::Applied {
        from: current,
        to: version,
        left: report.left,
    })
}

/// `evorag update rollback` (12 §7.2)。通信しない。
///
/// # Errors
///
/// 始める前の確かめで断った、`evorag.prev` が無い・evorag の版を出さない、データ根のロックを取れない、
/// 入れ替えに失敗した ([`FlowError`])。
pub fn rollback(
    ctx: &Context<'_>,
    notify: &mut dyn FnMut(Notice),
) -> Result<RollbackOutcome, FlowError> {
    let mut inst = begin(ctx, notify)?;
    // 1・2: 戻す先を確かめる。
    let mut env = Env::new(ctx.probe);
    let prev = inst.identify_prev(&mut env)?;
    // 3: データの案内を、データ根のロックを持ったまま求める。
    let data_lock = lock_data_root(ctx)?;
    let guidance = data::guidance(
        data_lock.as_ref(),
        ctx.data_root,
        ctx.backup_dir,
        ctx.config_file,
        prev.db_format(),
    );
    // 4・5: 入れ替えて、計画を済みにする。
    notify_gap(Op::Rollback, notify);
    inst.rollback(&prev, ctx.current, &mut env)?;
    drop(data_lock);
    Ok(RollbackOutcome {
        to: prev.version(),
        from: ctx.current.version,
        guidance,
    })
}

/// 始める前の確かめの 1〜7 (12 §7.2)。どれかに当たれば、インストール先を変えずに断る。
fn begin(ctx: &Context<'_>, notify: &mut dyn FnMut(Notice)) -> Result<Installation, FlowError> {
    // 1: 名前。
    crate::swap::check_exe_name(ctx.running_exe)?;
    // 2: 入れ方。
    if ctx.channel != ZIP_CHANNEL {
        return Err(FlowError::NotZipChannel {
            channel: ctx.channel.to_owned(),
        });
    }
    // 3: 更新のロック。
    let mut inst = Installation::open(ctx.running_exe, ctx.state_dir, Purpose::Update)?;
    // 4: 残った計画は、データ根のロックを取ってから続きを済ませる。
    if let Some(plan) = inst.pending()? {
        let _data_lock = lock_data_root(ctx)?;
        notify_gap(plan.op, notify);
        let mut env = Env::new(ctx.probe);
        if let Some(report) = inst.resume(&mut env)? {
            notify(Notice::Resumed(report));
        }
    }
    // 5: 名前で片付ける。残ったファイルを消せなければ断る。
    let cleaned = inst.cleanup_by_name(Strictness::Strict)?;
    if cleaned.restored_prev.is_some() || !cleaned.removed.is_empty() {
        notify(Notice::Cleaned(cleaned));
    }
    // 6: インストール先。
    inst.check_install_dir()?;
    // 7: データ根を使っている evorag が無いことを確かめ、すぐ放す。
    if ctx.data_root.is_dir() {
        DataRootLock::acquire_quiet(ctx.data_root).map_err(|e| data_root_error(ctx, e))?;
    }
    Ok(inst)
}

/// Windows の隙間の戻し方を示す (12 §7.3)。隙間の無い OS では示さない。
fn notify_gap(op: Op, notify: &mut dyn FnMut(Notice)) {
    if cfg!(windows) {
        notify(Notice::GapRecovery(GapRecovery::for_op(op)));
    }
}

/// データ根のロックを取る。データ根がまだ無ければ取らない (`None`)。
fn lock_data_root(ctx: &Context<'_>) -> Result<Option<DataRootLock>, FlowError> {
    if !ctx.data_root.is_dir() {
        return Ok(None);
    }
    DataRootLock::acquire(ctx.data_root)
        .map(Some)
        .map_err(|e| data_root_error(ctx, e))
}

fn data_root_error(ctx: &Context<'_>, e: LockError) -> FlowError {
    match e {
        LockError::InUse { pid } => FlowError::DataRootInUse {
            path: ctx.data_root.to_path_buf(),
            pid,
        },
        LockError::Io(source) => FlowError::DataRootLock {
            path: ctx.data_root.to_path_buf(),
            source,
        },
    }
}

/// リリースの一覧を 1 回引き、今の版より大きい、いちばん大きい版を返す (12 §7.2 の apply の 1)。
fn newest(ctx: &Context<'_>) -> Result<Option<Version>, FlowError> {
    let list = ctx
        .fetcher
        .fetch(policy::RELEASES_URL, Resource::ReleaseList)?;
    Ok(latest_version(&list)?.filter(|v| *v > ctx.current.version))
}

/// `release.json` と `.minisig` を取得し、署名と中身を確かめる (12 §7.2 の apply の 2〜4)。
fn fetch_release(
    ctx: &Context<'_>,
    version: Version,
    requested: Option<Version>,
) -> Result<VerifiedRelease, FlowError> {
    let get = |name: &'static str, resource| {
        ctx.fetcher
            .fetch(&policy::download_url(version, name), resource)
            .map_err(|e| match e {
                NetError::NotFound => FlowError::ReleaseFileMissing {
                    version,
                    what: name,
                },
                e => FlowError::Net(e),
            })
    };
    // 配布物の版で日付が読めなければ、取得の前に断る。
    let release_date = release_date_of_build(ctx.channel, ctx.release_date)?;
    let json = get("release.json", Resource::Manifest)?;
    let minisig = get("release.json.minisig", Resource::Signature)?;
    let mut expect = Expectations::new(ctx.current.version, ctx.target).with_tag(version);
    if let Some(requested) = requested {
        expect = expect.with_requested_version(requested);
    }
    if let Some(date) = release_date {
        expect = expect.with_release_date(date);
    }
    Ok(verify_signature(&json, &minisig, ctx.keys)?.verify(&expect)?)
}
