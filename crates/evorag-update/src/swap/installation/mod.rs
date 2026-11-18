//! インストール先の実行ファイルの置き換え・巻き戻し・続き・片付け (12 §7.2・§7.3)。
//!
//! - 開き方・版の情報・操作の結果の型 ([`report`])。
//! - OS ごとの置き換えの手順 (apply・rollback) と、失敗したときに元に戻す流れ ([`sequence`])。
//! - 計画に従って、SHA-256 で役割を見分けて置き終える・元に戻す ([`converge`])。
//! - ファイルの操作 (調べる・写す・改名する・消す・作る・書く・同期する) ([`fileops`])。

mod converge;
mod fileops;
mod report;
mod sequence;

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use evorag_store::fsutil;

pub use self::report::{
    ApplyReport, Build, CleanupReport, PlanSummary, PrevInfo, PrevSource, Purpose, ResumeOutcome,
    ResumeReport, Startup, Strictness,
};
use super::env::Env;
use super::error::{FileOp, SwapError};
use super::names::{EXE_NAME, FileName, Method, Op};
use super::probe;
use super::state::{Role, StatePaths};
use crate::{Executable, Sha256Digest, Version};
use converge::Entry;

/// TUI の起動の片付け (12 §7.3)。更新のロックを取れなければ何もしない。
///
/// 呼ぶ側は、データ根のロック (03 §9) を先に取って持っていること (片付けの間に、ほかの evorag が
/// 起動しないように)。`running_exe` は [`std::env::current_exe`] の結果を渡す。
///
/// # Errors
///
/// 計画が読めない・別のフォルダのもの・役割に当たらないなど ([`Installation::resume`])。
pub fn startup(
    running_exe: &Path,
    state_dir: &Path,
    env: &mut Env<'_>,
) -> Result<Startup, SwapError> {
    let mut inst = match Installation::open(running_exe, state_dir, Purpose::Startup) {
        Err(SwapError::Busy) => return Ok(Startup::Busy),
        r => r?,
    };
    match inst.resume(env)? {
        Some(report) => Ok(Startup::Resumed(report)),
        None => Ok(Startup::Cleaned(inst.cleanup_by_name(Strictness::Lenient)?)),
    }
}

/// 更新のロックを持った、インストール先 (12 §7.3)。落とすとロックが外れる。
///
/// 呼ぶ順 (12 §7.2 の始める前の確かめ): [`Self::open`] (1・3) → [`Self::pending`] (4。計画があれば、
/// 呼ぶ側がデータ根のロックを取ってから [`Self::resume`]) → [`Self::cleanup_by_name`] (5) →
/// [`Self::check_install_dir`] (6) → (呼ぶ側: 7〜9、取得と検証、データ根のロック) →
/// [`Self::apply`] か [`Self::rollback`]。
///
/// [`Self::apply`] と [`Self::rollback`] は、始める前に自分で、計画が残っていないこと・名前での片付け
/// ([`Strictness::Strict`])・インストール先の確かめを、この順でもう一度行う (呼ぶ側が飛ばしても、確かめずに
/// 置き換えない)。先に呼ぶのは、通信の前に断るため。データ根のロックは呼ぶ側が持つ。
#[derive(Debug)]
pub struct Installation {
    dir: PathBuf,
    state: StatePaths,
    method: Method,
    running: Option<Sha256Digest>,
    _lock: File,
}

impl Installation {
    /// インストール先 (動いている実行ファイルのフォルダ) を開き、状態のフォルダの `update/lock` を取る。
    ///
    /// `running_exe` は正規化してから (シンボリックリンクをたどって) フォルダと名前を取る。名前は Windows と
    /// macOS では大文字と小文字を区別せずに比べる。
    ///
    /// 動いている実行ファイルの SHA-256 (続きで「続きを進めているのが `next` 自身か」を見る) は、
    /// `running_exe` のファイルから求める。Linux で `running_exe` が [`std::env::current_exe`] と同じ
    /// ファイルなら、`/proc/self/exe` (動いているイメージそのもの) から求める (名前の上のファイルが
    /// 差し替わっていても、動いているものを見る)。試験は別のパスを渡すので、ファイルから求める。
    ///
    /// # Errors
    ///
    /// [`Purpose::Update`] で名前が `evorag(.exe)` でない ([`SwapError::ExeName`])、ほかの `update` が
    /// 動いている ([`SwapError::Busy`])、フォルダを開けない。
    pub fn open(running_exe: &Path, state_dir: &Path, purpose: Purpose) -> Result<Self, SwapError> {
        let running_path =
            canonical_exe_path(running_exe).map_err(SwapError::io(FileOp::Open, running_exe))?;
        let name = running_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if purpose == Purpose::Update && !is_exe_name(&name) {
            return Err(SwapError::ExeName { found: name });
        }
        let dir = running_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| SwapError::ExeName {
                found: name.clone(),
            })?;
        let state = StatePaths::new(state_dir);
        let lock = lock_update(&state)?;
        let mut inst = Self {
            dir,
            state,
            method: Method::HOST,
            running: None,
            _lock: lock,
        };
        inst.running = inst.running_digest(running_exe, &running_path);
        Ok(inst)
    }

    /// インストール先 (正規化したパス)。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 状態のファイルのパス (手で戻す手順の 4 で示す)。
    #[must_use]
    pub fn state_file(&self) -> &Path {
        &self.state.file
    }

    /// 残った計画 (12 §7.2 の始める前の確かめの 4)。
    ///
    /// # Errors
    ///
    /// 計画が読めない ([`SwapError::StateUnreadable`]、手で戻す手順を示す)、計画がほかのフォルダの
    /// もの ([`SwapError::ForeignPlan`])。どちらもインストール先に触れない。
    pub fn pending(&self) -> Result<Option<PlanSummary>, SwapError> {
        let st = self.read_state()?;
        let Some(plan) = st.plan else {
            return Ok(None);
        };
        self.check_plan_dir(&plan)?;
        Ok(Some(PlanSummary {
            op: plan.op,
            stage: plan.stage,
            reverting: plan.reverting,
        }))
    }

    /// 残った計画の続きを進める (12 §7.3 の「続きと片付け」)。計画が無ければ `None`。
    ///
    /// 役割はファイルの名前ではなく SHA-256 で見分ける。データ根のロックは呼ぶ側が先に取る。
    ///
    /// # Errors
    ///
    /// [`Self::pending`] の理由のほか、次のときは何も動かさず、計画を残す (どれも手で戻す手順を示す):
    /// `evorag(.exe)` が役割に当たらない ([`SwapError::UnknownRole`])、置く先に役割に当たらないファイルが
    /// ある ([`SwapError::ForeignFile`])、`evorag(.exe)` に置けるファイルが無い
    /// ([`SwapError::NothingToRestore`])。ファイルの操作の失敗 (計画を残す。次の機会に続ける)。
    pub fn resume(&mut self, env: &mut Env<'_>) -> Result<Option<ResumeReport>, SwapError> {
        let st = self.read_state()?;
        let Some(plan) = st.plan else {
            return Ok(None);
        };
        self.check_plan_dir(&plan)?;
        self.converge(plan, st.done, env).map(Some)
    }

    /// 計画が無いときに、名前で片付ける (12 §7.3)。`evorag(.exe)` が無ければ何もしない。
    ///
    /// - `evorag.prev` が無ければ、`evorag.prev.old` を、無ければ `evorag.tmp` を `evorag.prev` に戻す
    ///   (通常のファイルのときだけ)。
    /// - `evorag.tmp` は、中身が `evorag(.exe)` か (戻した後の) `evorag.prev` と同じときだけ消す。違えば
    ///   消さずに報告する (利用者のファイルかもしれない)。
    /// - 残りの `evorag.new`・`evorag.prev.old`・`evorag.rejected`・`evorag.probe` は消す。
    ///
    /// # Errors
    ///
    /// 計画が残っている、[`Strictness::Strict`] で消せないファイルがある ([`SwapError::Leftover`]) か
    /// 消さない `evorag.tmp` がある ([`SwapError::UnrelatedLeftover`])。
    pub fn cleanup_by_name(&mut self, strictness: Strictness) -> Result<CleanupReport, SwapError> {
        if self.read_state()?.plan.is_some() {
            return Err(SwapError::PlanPending);
        }
        let mut report = CleanupReport::default();
        if !self.exists(FileName::Exe)? {
            report.skipped_without_exe = true;
            return Ok(report);
        }
        if !self.exists(FileName::Prev)? {
            for from in [FileName::PrevOld, FileName::Tmp] {
                if matches!(self.inspect(from)?, Entry::File(_)) {
                    self.raw_rename(from, FileName::Prev)?;
                    report.restored_prev = Some(from);
                    break;
                }
            }
        }
        let keep_tmp = match self.inspect(FileName::Tmp)? {
            Entry::Absent => false,
            Entry::File(d) => ![FileName::Exe, FileName::Prev]
                .into_iter()
                .map(|n| self.inspect(n))
                .collect::<Result<Vec<_>, _>>()?
                .contains(&Entry::File(d)),
            Entry::Other => true,
        };
        if keep_tmp {
            match strictness {
                Strictness::Strict => {
                    return Err(SwapError::UnrelatedLeftover {
                        name: FileName::Tmp,
                    });
                }
                Strictness::Lenient => report.kept.push(FileName::Tmp),
            }
        }
        for name in FileName::LEFTOVERS {
            if (name == FileName::Tmp && keep_tmp) || !self.exists(name)? {
                continue;
            }
            match fsutil::retry_locked(|| std::fs::remove_file(self.path(name))) {
                Ok(()) => report.removed.push(name),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) if strictness == Strictness::Strict => {
                    return Err(SwapError::Leftover { name, source });
                }
                Err(_) => report.left.push(name),
            }
        }
        Ok(report)
    }

    /// インストール先を確かめる (12 §7.3 の「インストール先の確かめ」)。何も変えない
    /// (`evorag.probe` を作って消すだけ)。
    ///
    /// # Errors
    ///
    /// `evorag(.exe)` がリンク・名前の代理のリパースポイント・通常のファイルでない、Unix で持ち主か
    /// 権限が条件に合わない、Windows でほかの利用者がフォルダに書き込める
    /// ([`SwapError::WritableByOtherUsers`])、書き込めない。
    pub fn check_install_dir(&self) -> Result<(), SwapError> {
        let exe = self.path(FileName::Exe);
        let meta = std::fs::symlink_metadata(&exe)
            .map_err(SwapError::file(FileOp::Read, FileName::Exe))?;
        if fsutil::is_link_or_reparse_point(&meta) || !meta.is_file() {
            return Err(SwapError::NotRegularFile {
                name: FileName::Exe,
            });
        }
        #[cfg(unix)]
        {
            let dir_meta =
                std::fs::metadata(&self.dir).map_err(SwapError::io(FileOp::Read, &self.dir))?;
            unix_ownership(&self.dir, &dir_meta)?;
            unix_ownership(&exe, &meta)?;
        }
        #[cfg(windows)]
        if fsutil::writable_by_others(&self.dir).map_err(SwapError::io(FileOp::Read, &self.dir))? {
            return Err(SwapError::WritableByOtherUsers {
                dir: self.dir.clone(),
            });
        }
        let not_writable = |source| SwapError::NotWritable {
            dir: self.dir.clone(),
            source,
        };
        let probe = self.path(FileName::Probe);
        match fsutil::retry_locked(|| std::fs::remove_file(&probe)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(not_writable(e)),
            _ => {}
        }
        drop(fsutil::create_new_file(&probe, 0o600).map_err(not_writable)?);
        fsutil::retry_locked(|| std::fs::remove_file(&probe)).map_err(not_writable)
    }

    /// `evorag.prev` の版を決める (12 §7.2 の rollback の 1・2)。最後に済んだ操作の `current` の
    /// SHA-256 が合えばその記録を、合わなければ `--version` の出力を使う。
    ///
    /// # Errors
    ///
    /// `evorag.prev` が無い・通常のファイルでない・ほかの利用者のもの、動かせない、1 行目が
    /// `evorag <版>` でない、Windows で `evorag.prev.exe` がある ([`SwapError::BlockingFile`])。
    pub fn identify_prev(&self, env: &mut Env<'_>) -> Result<PrevInfo, SwapError> {
        let sha256 = match self.inspect(FileName::Prev)? {
            Entry::Absent => return Err(SwapError::NoPrev),
            Entry::Other => {
                return Err(SwapError::NotRegularFile {
                    name: FileName::Prev,
                });
            }
            Entry::File(d) => d,
        };
        if let Some(done) = self.read_state()?.done
            && done.current.sha256 == sha256
            && let Some(version) = done.current.version.as_deref().and_then(|v| v.parse().ok())
        {
            return Ok(PrevInfo {
                sha256,
                version,
                db_format: done.current.db_format,
                source: PrevSource::Record,
            });
        }
        let out = env
            .probe()
            .run_version(&self.probe_path(FileName::Prev)?)
            .map_err(SwapError::PrevProbe)?;
        let parsed = probe::parse_version_output(&out).ok_or_else(|| SwapError::PrevNotEvorag {
            first_line: probe::first_line(&out),
        })?;
        let db_format = parsed
            .db_format
            .or_else(|| past_db_format(parsed.version).map(str::to_owned));
        Ok(PrevInfo {
            sha256,
            version: parsed.version,
            db_format,
            source: PrevSource::Probe,
        })
    }

    /// 検証した新しい実行ファイルで置き換える (12 §7.2 の apply の 8〜11、§7.3)。今の
    /// `evorag(.exe)` は `evorag.prev` になる。データ根のロックは呼ぶ側が持つ。
    ///
    /// 始める前に、計画が残っていないこと・名前での片付け ([`Strictness::Strict`])・インストール先の
    /// 確かめを、この順で行う。
    ///
    /// # Errors
    ///
    /// 始める前に断ったとき (インストール先は変わらない)、置き換えに失敗して元に戻したとき
    /// ([`SwapError::Reverted`]。確かめに通らないときを含む)、元に戻せなかったとき
    /// ([`SwapError::RevertFailed`]。計画を残す)、入れ替えは済んだが記録できなかったとき
    /// ([`SwapError::NotRecorded`])。
    pub fn apply(
        &mut self,
        exe: &Executable,
        current: &Build,
        next: &Build,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        self.apply_bytes(exe.as_bytes(), exe.sha256(), current, next, env)
    }

    /// `evorag.prev` に戻す (12 §7.2 の rollback の 4・5、§7.3)。今の `evorag(.exe)` は `evorag.prev`
    /// になる。`prev` は [`Self::identify_prev`] の結果。データ根のロックは呼ぶ側が持つ。
    ///
    /// 始める前の確かめは [`Self::apply`] と同じ。
    ///
    /// # Errors
    ///
    /// `evorag.prev` が無い・確かめた後に変わった、置き換えに失敗した ([`Self::apply`] と同じ)。
    pub fn rollback(
        &mut self,
        prev: &PrevInfo,
        current: &Build,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        self.rollback_files(prev, current, env)
    }

    /// apply・rollback の始める前の確かめ (12 §7.2 の 4〜6): 計画が残っていない、名前で片付ける、
    /// インストール先を確かめる。読んだ状態のファイルを返す。
    fn precheck(&mut self) -> Result<super::state::State, SwapError> {
        let st = self.read_state()?;
        if let Some(plan) = &st.plan {
            self.check_plan_dir(plan)?;
            return Err(SwapError::PlanPending);
        }
        self.cleanup_by_name(Strictness::Strict)?;
        self.check_install_dir()?;
        Ok(st)
    }

    /// 動いている実行ファイルの SHA-256 ([`Self::open`] を参照)。求められなければ `None`。
    fn running_digest(&self, running_exe: &Path, running_path: &Path) -> Option<Sha256Digest> {
        #[cfg(target_os = "linux")]
        {
            let is_self = std::env::current_exe().is_ok_and(|c| {
                c == running_exe || canonical_exe_path(&c).is_ok_and(|c| c == running_path)
            });
            if is_self {
                return fileops::digest_of_running_image();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = running_exe;
        match self.inspect_path(running_path) {
            Ok(Entry::File(d)) => Some(d),
            _ => None,
        }
    }

    #[cfg(all(test, windows))]
    pub(crate) fn set_method(&mut self, method: Method) {
        self.method = method;
    }
}

fn role(sha256: Sha256Digest, build: &Build) -> Role {
    Role {
        sha256,
        version: Some(build.version.to_string()),
        db_format: build.db_format.clone(),
    }
}

/// `db format:` の行を出さない過去の版の DB の形式の版 (12 §7.2 の rollback の 2)。
fn past_db_format(v: Version) -> Option<&'static str> {
    match (v.major(), v.minor()) {
        (0, 1) => Some("0.1"),
        (0, 2) => Some("0.2"),
        _ => None,
    }
}

/// 動いている実行ファイルの名前が `evorag(.exe)` か。Windows と macOS は大文字と小文字を区別しない
/// (ファイルシステムが区別しないのが既定のため)。
fn is_exe_name(name: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        name.eq_ignore_ascii_case(EXE_NAME)
    } else {
        name == EXE_NAME
    }
}

/// 実行ファイルのパスを正規化する。ファイルが無いとき (Windows の隙間で `evorag.exe` が無い) は、
/// フォルダを正規化して名前を付ける。
fn canonical_exe_path(p: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(p) {
        Ok(c) => Ok(strip_verbatim(c)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = p
                .file_name()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
            let parent = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(strip_verbatim(std::fs::canonicalize(parent)?).join(name))
        }
        Err(e) => Err(e),
    }
}

/// Windows では、普通のパスで表せるものは `\\?\` を外す (表示と子のプロセスのため)。
fn strip_verbatim(c: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    c
}

/// 状態のフォルダの `update/lock` を取る (12 §7.3 の「更新のロック」)。
fn lock_update(paths: &StatePaths) -> Result<File, SwapError> {
    fsutil::create_private_dir(&paths.dir).map_err(SwapError::io(FileOp::Create, &paths.dir))?;
    fsutil::prepare_private_file(&paths.lock)
        .map_err(SwapError::io(FileOp::Create, &paths.lock))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .map_err(SwapError::io(FileOp::Open, &paths.lock))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(SwapError::Busy),
        Err(TryLockError::Error(e)) => Err(SwapError::io(FileOp::Lock, &paths.lock)(e)),
    }
}

/// Unix: 動いている利用者のもので、group と other が書けない (sticky のフォルダは除く)。
#[cfg(unix)]
fn unix_ownership(path: &Path, meta: &std::fs::Metadata) -> Result<(), SwapError> {
    use std::os::unix::fs::MetadataExt;

    use super::error::OwnershipProblem;
    let problem = if meta.uid() != fsutil::effective_uid() {
        Some(OwnershipProblem::NotOwnedByUser)
    } else if meta.mode() & 0o022 != 0 && !(meta.is_dir() && meta.mode() & 0o1000 != 0) {
        Some(OwnershipProblem::WritableByOthers)
    } else {
        None
    };
    problem.map_or(Ok(()), |problem| {
        Err(SwapError::UnsafeOwnership {
            path: path.to_path_buf(),
            problem,
        })
    })
}

/// 操作 `op` の計画を作る (段 1)。
fn new_plan(
    op: Op,
    dir: String,
    current: Role,
    next: Role,
    old_prev: Option<Role>,
) -> super::state::Plan {
    super::state::Plan {
        op,
        dir,
        stage: 1,
        reverting: false,
        current,
        next,
        old_prev,
    }
}
