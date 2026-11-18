//! インストール先の実行ファイルの置き換え・巻き戻し・続き・片付け (12 §7.2・§7.3)。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use evorag_store::fsutil;

use super::error::{CheckFailure, Failure, FileOp, SwapError};
use super::names::{EXE_NAME, FileName, Method, Op};
use super::probe::{self, VersionProbe};
use super::state::{self, Plan, Role, StatePaths};
use crate::{Executable, MAX_EXE_BYTES, Sha256Digest, Version};

/// ファイルの操作の後に起きたこと。試験が、そこで強制終了したことにするために見る。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// 新しいファイル (`evorag.new`・写し) を書き切った。
    Wrote(FileName),
    /// 改名した (Linux はフォルダの同期まで)。
    Renamed(FileName, FileName),
    /// 消した。
    Removed(FileName),
    /// 状態のファイルの一時名を書き切った (改名の前)。
    StateWritten,
    /// 状態のファイルを置いた。
    StateSaved,
}

/// 置き換えに渡す道具: `--version` の動かし方と、試験の強制終了の点。
pub struct Env<'a> {
    probe: &'a dyn VersionProbe,
    crash: Option<&'a mut dyn FnMut(Event) -> bool>,
}

impl std::fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("crash_hook", &self.crash.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> Env<'a> {
    /// `--version` を `probe` で動かす。
    #[must_use]
    pub fn new(probe: &'a dyn VersionProbe) -> Self {
        Self { probe, crash: None }
    }

    /// ファイルの操作のたびに `hook` を呼び、真を返したらそこで止める ([`SwapError::Crashed`])。
    /// 止めた後は、ファイルにも状態のファイルにも何もしない (強制終了の模擬)。試験だけで使う。
    #[doc(hidden)]
    #[must_use]
    pub fn with_crash_hook(mut self, hook: &'a mut dyn FnMut(Event) -> bool) -> Self {
        self.crash = Some(hook);
        self
    }

    fn event(&mut self, event: Event) -> Result<(), SwapError> {
        let crashed = self.crash.as_mut().is_some_and(|hook| hook(event));
        if crashed {
            Err(SwapError::Crashed)
        } else {
            Ok(())
        }
    }
}

/// 何のために開くか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// `evorag update apply`・`rollback`: 動いている実行ファイルの名前が `evorag(.exe)` でなければ断る。
    Update,
    /// TUI の起動の片付け: 名前を問わない。
    Startup,
}

/// 名前で片付けて、消せないファイルが残ったときの扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strictness {
    /// 断る (`apply`・`rollback` の始める前の確かめの 5)。
    Strict,
    /// 残して報告する (起動の片付け。次の機会に回す)。
    Lenient,
}

/// 置き換える版の情報 (状態のファイルに、示すためだけに書く)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Build {
    /// 版。
    pub version: Version,
    /// その版が書く DB の形式の版 (分かれば)。
    pub db_format: Option<String>,
}

impl Build {
    /// 版の情報。
    #[must_use]
    pub const fn new(version: Version, db_format: Option<String>) -> Self {
        Self { version, db_format }
    }
}

/// 残った計画の要約 (示すため)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PlanSummary {
    /// 操作。
    pub op: Op,
    /// 済んだ段。
    pub stage: u32,
    /// 元に戻している途中か。
    pub reverting: bool,
}

/// `evorag.prev` の版 (12 §7.2 の rollback の 2)。[`Installation::identify_prev`] だけが作る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevInfo {
    sha256: Sha256Digest,
    version: Version,
    db_format: Option<String>,
    source: PrevSource,
}

/// `evorag.prev` の版をどこから知ったか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrevSource {
    /// 状態のファイルの最後に済んだ操作の記録。
    Record,
    /// `evorag.prev --version` の出力 (DB の形式の版は `db format:` の行か、過去の版の表)。
    Probe,
}

impl PrevInfo {
    /// SHA-256。
    #[must_use]
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// 版。
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// DB の形式の版 (分からなければ `None`)。
    #[must_use]
    pub fn db_format(&self) -> Option<&str> {
        self.db_format.as_deref()
    }

    /// どこから知ったか。
    #[must_use]
    pub const fn source(&self) -> PrevSource {
        self.source
    }
}

/// `apply` の結果。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ApplyReport {
    /// 消せずに残ったファイル (次の片付けで消す)。
    pub left: Vec<FileName>,
}

/// 続きの向き。
#[derive(Debug)]
#[non_exhaustive]
pub enum ResumeOutcome {
    /// 置き換えを終えた (`evorag(.exe)` が新しいもの)。
    Completed,
    /// 元に戻した (`evorag(.exe)` が置き換えの前のもの)。操作はやり直さない。
    Reverted {
        /// 置き換えの後の確かめに通らなかったときは、その理由。
        check: Option<CheckFailure>,
    },
}

/// 計画の続きの結果 (12 §7.3 の「続きと片付け」)。
#[derive(Debug)]
#[non_exhaustive]
pub struct ResumeReport {
    /// 計画の操作。
    pub op: Op,
    /// 向き。
    pub outcome: ResumeOutcome,
    /// 役割に当たらないので触れなかったファイル。
    pub untouched: Vec<FileName>,
    /// 置くべき役割のファイルが見つからなかった名前 (`evorag.prev` など)。
    pub missing: Vec<FileName>,
    /// 消せずに残したファイル。
    pub left: Vec<FileName>,
}

/// 名前での片付けの結果。
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct CleanupReport {
    /// `evorag.prev` に戻したファイル。
    pub restored_prev: Option<FileName>,
    /// 消したファイル。
    pub removed: Vec<FileName>,
    /// 消せずに残したファイル (起動の片付けのとき)。
    pub left: Vec<FileName>,
    /// `evorag(.exe)` が無いので、何もしなかった。
    pub skipped_without_exe: bool,
}

/// 起動の片付けの結果。
#[derive(Debug)]
#[non_exhaustive]
pub enum Startup {
    /// ほかの `update` が動いているので、片付けずに進む。
    Busy,
    /// 計画の続きを進めた。
    Resumed(ResumeReport),
    /// 計画が無いので、名前で片付けた。
    Cleaned(CleanupReport),
}

/// TUI の起動の片付け (12 §7.3)。更新のロックを取れなければ何もしない。
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

/// ファイルの中身の見分け。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Absent,
    File(Sha256Digest),
    /// リンク・フォルダ・大きすぎるファイルなど (どの役割にも当たらない)。
    Other,
}

/// 名前ごとの中身。操作のたびに直す。
#[derive(Debug, Default)]
struct Snapshot(BTreeMap<FileName, Entry>);

impl Snapshot {
    fn get(&self, name: FileName) -> Entry {
        self.0.get(&name).copied().unwrap_or(Entry::Absent)
    }

    fn holds(&self, name: FileName, digest: Sha256Digest) -> bool {
        self.get(name) == Entry::File(digest)
    }

    fn moved(&mut self, from: FileName, to: FileName) {
        let e = self.0.remove(&from).unwrap_or(Entry::Absent);
        self.0.insert(to, e);
    }

    fn copied(&mut self, from: FileName, to: FileName) {
        let e = self.get(from);
        self.0.insert(to, e);
    }

    fn removed(&mut self, name: FileName) {
        self.0.remove(&name);
    }
}

/// 置く先に置いた結果。
enum Placed {
    Done,
    /// 置くべき中身のファイルが無い。
    Missing,
}

/// 更新のロックを持った、インストール先 (12 §7.3)。落とすとロックが外れる。
///
/// 呼ぶ順 (12 §7.2 の始める前の確かめ): [`Self::open`] (1・3) → [`Self::pending`] (4。計画があれば、
/// 呼ぶ側がデータ根のロックを取ってから [`Self::resume`]) → [`Self::cleanup_by_name`] (5) →
/// [`Self::check_install_dir`] (6) → (呼ぶ側: 7〜9、取得と検証、データ根のロック) →
/// [`Self::apply`] か [`Self::rollback`]。
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
    /// # Errors
    ///
    /// [`Purpose::Update`] で名前が `evorag(.exe)` でない ([`SwapError::ExeName`])、ほかの `update` が
    /// 動いている ([`SwapError::Busy`])、フォルダを開けない。
    pub fn open(running_exe: &Path, state_dir: &Path, purpose: Purpose) -> Result<Self, SwapError> {
        let name = running_exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if purpose == Purpose::Update && name != EXE_NAME {
            return Err(SwapError::ExeName { found: name });
        }
        let parent = running_exe
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let dir = canonical_dir(parent).map_err(|source| SwapError::Io {
            what: "open",
            path: parent.to_path_buf(),
            source,
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
        inst.running = match inst.inspect_path(&inst.dir.join(&name)) {
            Ok(Entry::File(d)) => Some(d),
            _ => None,
        };
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
    /// [`Self::pending`] の理由のほか、`evorag(.exe)` が役割に当たらない ([`SwapError::UnknownRole`]。
    /// 何も動かさず、計画を残す)、置く先に役割に当たらないファイルがある、ファイルの操作の失敗
    /// (計画を残す。次の機会に続ける)。
    pub fn resume(&mut self, env: &mut Env<'_>) -> Result<Option<ResumeReport>, SwapError> {
        let st = self.read_state()?;
        let Some(plan) = st.plan else {
            return Ok(None);
        };
        self.check_plan_dir(&plan)?;
        self.converge(plan, st.done, env).map(Some)
    }

    /// 計画が無いときに、名前で片付ける (12 §7.3)。`evorag.prev` が無く `evorag.prev.old` か
    /// `evorag.tmp` があれば `evorag.prev` に戻し、残りのファイルを消す。`evorag(.exe)` が無ければ
    /// 何もしない。
    ///
    /// # Errors
    ///
    /// 計画が残っている、[`Strictness::Strict`] で消せないファイルがある ([`SwapError::Leftover`])。
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
                if self.exists(from)? {
                    self.raw_rename(from, FileName::Prev)?;
                    report.restored_prev = Some(from);
                    break;
                }
            }
        }
        for name in FileName::LEFTOVERS {
            if !self.exists(name)? {
                continue;
            }
            match fsutil::retry_locked(|| std::fs::remove_file(self.path(name))) {
                Ok(()) => report.removed.push(name),
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
    /// `evorag(.exe)` がシンボリックリンク・リパースポイント・通常のファイルでない、Unix で持ち主か
    /// 権限が条件に合わない、書き込めない。
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
            let dir_meta = std::fs::metadata(&self.dir).map_err(|source| SwapError::Io {
                what: "read",
                path: self.dir.clone(),
                source,
            })?;
            unix_ownership(&self.dir, &dir_meta)?;
            unix_ownership(&exe, &meta)?;
        }
        let not_writable = |source| SwapError::NotWritable {
            dir: self.dir.clone(),
            source,
        };
        let probe = self.path(FileName::Probe);
        match std::fs::remove_file(&probe) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(not_writable(e)),
            _ => {}
        }
        drop(fsutil::create_new_file(&probe, 0o600).map_err(not_writable)?);
        std::fs::remove_file(&probe).map_err(not_writable)
    }

    /// `evorag.prev` の版を決める (12 §7.2 の rollback の 1・2)。最後に済んだ操作の `current` の
    /// SHA-256 が合えばその記録を、合わなければ `--version` の出力を使う。
    ///
    /// # Errors
    ///
    /// `evorag.prev` が無い・通常のファイルでない、動かせない、1 行目が `evorag <版>` でない。
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
            .probe
            .run_version(&self.path(FileName::Prev))
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
    /// # Errors
    ///
    /// 始める前に断ったとき (インストール先は変わらない)、置き換えに失敗して元に戻したとき
    /// ([`SwapError::Reverted`]。確かめに通らないときを含む)、元に戻せなかったとき
    /// ([`SwapError::RevertFailed`]。計画を残す)。
    pub fn apply(
        &mut self,
        exe: &Executable,
        current: &Build,
        next: &Build,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        self.apply_bytes(exe.as_bytes(), exe.sha256(), current, next, env)
    }

    pub(crate) fn apply_bytes(
        &mut self,
        bytes: &[u8],
        sha256: Sha256Digest,
        current: &Build,
        next: &Build,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        let st = self.read_state()?;
        if st.plan.is_some() {
            return Err(SwapError::PlanPending);
        }
        let current_sha = self.regular(FileName::Exe)?;
        if current_sha == sha256 {
            return Err(SwapError::SameFile);
        }
        let old_prev = match self.inspect(FileName::Prev)? {
            Entry::Absent => None,
            Entry::File(d) => Some(Role {
                sha256: d,
                version: None,
                db_format: None,
            }),
            Entry::Other => {
                return Err(SwapError::NotRegularFile {
                    name: FileName::Prev,
                });
            }
        };
        // 1: evorag.new を書いて書き切り、読み直して確かめる。
        if let Err(e) = self.write_new(bytes, sha256, env) {
            if !matches!(e, SwapError::Crashed) {
                let _ = std::fs::remove_file(self.path(FileName::New));
            }
            return Err(e);
        }
        let mut plan = Plan {
            op: Op::Apply,
            dir: self.dir_key(),
            stage: 1,
            reverting: false,
            current: role(current_sha, current),
            next: role(sha256, next),
            old_prev,
        };
        let done = st.done;
        if let Err(e) = self.save(Some(&plan), done.as_ref(), env) {
            if !matches!(e, SwapError::Crashed) {
                let _ = std::fs::remove_file(self.path(FileName::New));
            }
            return Err(e);
        }
        match self.method {
            #[cfg(windows)]
            Method::TwoStep => self.apply_two_step(&mut plan, done.as_ref(), env),
            Method::Replace => self.apply_replace(&mut plan, done.as_ref(), env),
        }
    }

    #[cfg(windows)]
    fn apply_two_step(
        &mut self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        use FileName::{Exe, New, Prev, PrevOld};
        // 2: 前回の evorag.prev を evorag.prev.old に (消さない)。
        if plan.old_prev.is_some() {
            if let Err(e) = self.rename(Prev, PrevOld, env) {
                return Err(self.abort(plan, done, e, env));
            }
            plan.stage = 2;
            if let Err(e) = self.save(Some(plan), done, env) {
                return Err(self.abort(plan, done, e, env));
            }
        }
        // 3・4: 自分を evorag.prev に、evorag.new を evorag.exe に。間にほかの処理を入れない。
        if let Err(e) = self.rename(Exe, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        if let Err(e) = self.rename(New, Exe, env) {
            if matches!(e, SwapError::Crashed) {
                return Err(e);
            }
            // evorag.exe が無い間を短くするため、まず自分を戻す。
            if let Err(SwapError::Crashed) = self.rename(Prev, Exe, env) {
                return Err(SwapError::Crashed);
            }
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 4;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 5: 置き換えの後の確かめ。
        if let Err(f) = self.verify(&plan.next, env) {
            return Err(self.abort_check(plan, done, f, env));
        }
        plan.stage = 5;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 6: evorag.prev.old を消す。消せなければ残し、次の片付けで消す。
        let mut report = ApplyReport::default();
        if plan.old_prev.is_some() {
            match self.remove(PrevOld, env) {
                Ok(()) => {}
                Err(SwapError::Crashed) => return Err(SwapError::Crashed),
                Err(_) => report.left.push(PrevOld),
            }
        }
        plan.stage = 6;
        self.finish(plan, env)?;
        Ok(report)
    }

    fn apply_replace(
        &mut self,
        plan: &mut Plan,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<ApplyReport, SwapError> {
        use FileName::{Exe, New, Prev, Tmp};
        // 2: 今の evorag の写しを evorag.tmp に。
        if let Err(e) = self.copy(Exe, Tmp, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 2;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 3: evorag.new を evorag の上に。
        if let Err(e) = self.rename(New, Exe, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 3;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 4: 置き換えの後の確かめ。
        if let Err(f) = self.verify(&plan.next, env) {
            return Err(self.abort_check(plan, done, f, env));
        }
        plan.stage = 4;
        if let Err(e) = self.save(Some(plan), done, env) {
            return Err(self.abort(plan, done, e, env));
        }
        // 5: evorag.tmp を evorag.prev に (前回の evorag.prev を上書きする)。
        if let Err(e) = self.rename(Tmp, Prev, env) {
            return Err(self.abort(plan, done, e, env));
        }
        plan.stage = 5;
        self.finish(plan, env)?;
        Ok(ApplyReport::default())
    }

    /// `evorag.prev` に戻す (12 §7.2 の rollback の 4・5、§7.3)。今の `evorag(.exe)` は `evorag.prev`
    /// になる。`prev` は [`Self::identify_prev`] の結果。データ根のロックは呼ぶ側が持つ。
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
        use FileName::{Exe, Prev, Tmp};
        let st = self.read_state()?;
        if st.plan.is_some() {
            return Err(SwapError::PlanPending);
        }
        let current_sha = self.regular(Exe)?;
        // Windows の 1: evorag.prev の SHA-256 を求め、確かめたものと同じであること。
        let prev_sha = match self.inspect(Prev)? {
            Entry::Absent => return Err(SwapError::NoPrev),
            Entry::Other => return Err(SwapError::NotRegularFile { name: Prev }),
            Entry::File(d) => d,
        };
        if prev_sha != prev.sha256 {
            return Err(SwapError::Changed { name: Prev });
        }
        if prev_sha == current_sha {
            return Err(SwapError::SameFile);
        }
        let mut plan = Plan {
            op: Op::Rollback,
            dir: self.dir_key(),
            stage: 1,
            reverting: false,
            current: role(current_sha, current),
            next: Role {
                sha256: prev_sha,
                version: Some(prev.version.to_string()),
                db_format: prev.db_format.clone(),
            },
            old_prev: None,
        };
        let done = st.done;
        let done = done.as_ref();
        match self.method {
            #[cfg(windows)]
            Method::TwoStep => {
                self.save(Some(&plan), done, env)?;
                // 2・3: 自分を evorag.tmp に、evorag.prev を evorag.exe に。間にほかの処理を入れない。
                if let Err(e) = self.rename(Exe, Tmp, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                if let Err(e) = self.rename(Prev, Exe, env) {
                    if matches!(e, SwapError::Crashed) {
                        return Err(e);
                    }
                    if let Err(SwapError::Crashed) = self.rename(Tmp, Exe, env) {
                        return Err(SwapError::Crashed);
                    }
                    return Err(self.abort(&mut plan, done, e, env));
                }
                plan.stage = 3;
                if let Err(e) = self.save(Some(&plan), done, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                // 4: evorag.tmp を evorag.prev に。
                if let Err(e) = self.rename(Tmp, Prev, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                plan.stage = 4;
            }
            Method::Replace => {
                // 1: 今の evorag の写しを evorag.tmp に。計画はその後に書く。
                if let Err(e) = self.copy(Exe, Tmp, env) {
                    if !matches!(e, SwapError::Crashed) {
                        let _ = std::fs::remove_file(self.path(Tmp));
                    }
                    return Err(e);
                }
                self.save(Some(&plan), done, env)?;
                // 2: evorag.prev を evorag の上に。
                if let Err(e) = self.rename(Prev, Exe, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                plan.stage = 2;
                if let Err(e) = self.save(Some(&plan), done, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                // 3: evorag.tmp を evorag.prev に。
                if let Err(e) = self.rename(Tmp, Prev, env) {
                    return Err(self.abort(&mut plan, done, e, env));
                }
                plan.stage = 3;
            }
        }
        self.finish(&plan, env)
    }

    // ---- 元に戻す・続き ----

    /// 置き換えの途中の失敗: 計画を `reverting` にしてから元に戻す。
    fn abort(
        &mut self,
        plan: &mut Plan,
        done: Option<&Plan>,
        error: SwapError,
        env: &mut Env<'_>,
    ) -> SwapError {
        if matches!(error, SwapError::Crashed) {
            return error;
        }
        self.revert_after(plan, done, error.into_failure(), env)
    }

    /// 置き換えの後の確かめに通らなかった: 元に戻す (Windows は `evorag.rejected` を経る)。
    fn abort_check(
        &mut self,
        plan: &mut Plan,
        done: Option<&Plan>,
        check: CheckFailure,
        env: &mut Env<'_>,
    ) -> SwapError {
        self.revert_after(plan, done, Failure::Check(check), env)
    }

    fn revert_after(
        &mut self,
        plan: &mut Plan,
        done: Option<&Plan>,
        cause: Failure,
        env: &mut Env<'_>,
    ) -> SwapError {
        plan.reverting = true;
        // 状態のファイルを書けなくても、ファイルは元に戻す (計画は次の機会に、SHA-256 で見分けて続ける)。
        if let Err(SwapError::Crashed) = self.save(Some(plan), done, env) {
            return SwapError::Crashed;
        }
        match self.converge(plan.clone(), done.cloned(), env) {
            Ok(_) => SwapError::Reverted(cause),
            Err(SwapError::Crashed) => SwapError::Crashed,
            Err(e) => SwapError::RevertFailed {
                cause,
                revert: Box::new(e),
            },
        }
    }

    /// 計画に従って、役割のファイルを置き終える (続き) か、元に戻す。役割は SHA-256 で見分ける。
    fn converge(
        &mut self,
        mut plan: Plan,
        done: Option<Plan>,
        env: &mut Env<'_>,
    ) -> Result<ResumeReport, SwapError> {
        use FileName::{Exe, New, Prev, PrevOld, Rejected, Tmp};
        let mut check = None;
        let mut snap = self.scan()?;
        let (current, next) = (plan.current.sha256, plan.next.sha256);
        let mut roles = vec![current, next];
        roles.extend(plan.old_prev.as_ref().map(|r| r.sha256));
        let anywhere = |snap: &Snapshot, d| FileName::ROLE_NAMES.iter().any(|&n| snap.holds(n, d));
        let exe_was_current = snap.holds(Exe, current);
        let mut forward = if plan.reverting {
            false
        } else {
            match snap.get(Exe) {
                Entry::File(d) if d == next => true,
                Entry::File(d) if d == current => false,
                Entry::Absent if anywhere(&snap, next) => true,
                Entry::Absent if anywhere(&snap, current) => false,
                Entry::Absent => return Err(SwapError::NothingToRestore),
                Entry::File(_) | Entry::Other => return Err(SwapError::UnknownRole),
            }
        };
        let done = done.as_ref();
        let mut missing = Vec::new();
        if forward {
            let targets = [(Exe, next), (Prev, current)];
            if let Placed::Missing = self.place(&mut snap, &targets, &roles, Exe, next, env)? {
                return Err(SwapError::NothingToRestore);
            }
            let verified = plan.op == Op::Rollback
                || plan.stage >= self.method.verify_stage()
                || self.running == Some(next);
            if !verified {
                match self.verify(&plan.next, env) {
                    Ok(()) => {
                        plan.stage = self.method.verify_stage();
                        self.save(Some(&plan), done, env)?;
                    }
                    Err(f) => {
                        check = Some(f);
                        forward = false;
                    }
                }
            }
        }
        if !forward && !plan.reverting {
            plan.reverting = true;
            self.save(Some(&plan), done, env)?;
        }
        let targets: Vec<(FileName, Sha256Digest)> = if forward {
            vec![(Exe, next), (Prev, current)]
        } else {
            let prev = match plan.op {
                Op::Apply => plan.old_prev.as_ref().map(|r| r.sha256),
                Op::Rollback => Some(next),
            };
            std::iter::once((Exe, current))
                .chain(prev.map(|d| (Prev, d)))
                .collect()
        };
        for &(name, want) in &targets {
            match self.place(&mut snap, &targets, &roles, name, want, env)? {
                Placed::Done => {}
                Placed::Missing if name == Exe => return Err(SwapError::NothingToRestore),
                Placed::Missing => missing.push(name),
            }
        }
        // 残りを片付ける: 役割に当たるものは消し、当たらないものには触れない。
        let (mut untouched, mut left) = (Vec::new(), Vec::new());
        for name in [New, Tmp, PrevOld, Rejected, Prev] {
            if targets.iter().any(|&(n, _)| n == name) {
                continue;
            }
            let disposable = match snap.get(name) {
                Entry::Absent => continue,
                Entry::File(d) if roles.contains(&d) => true,
                // 置き換えの前 (段 1) で止まった計画の evorag.new と、元に戻すときの書きかけの写し。
                _ if name == New && plan.stage == 1 => true,
                _ if name == Tmp && !forward && exe_was_current => true,
                _ => false,
            };
            if !disposable {
                untouched.push(name);
                continue;
            }
            match self.remove(name, env) {
                Ok(()) => snap.removed(name),
                Err(SwapError::Crashed) => return Err(SwapError::Crashed),
                Err(_) => left.push(name),
            }
        }
        let outcome = if forward {
            plan.reverting = false;
            self.finish(&plan, env)?;
            ResumeOutcome::Completed
        } else {
            self.save(None, done, env)?;
            ResumeOutcome::Reverted { check }
        };
        Ok(ResumeReport {
            op: plan.op,
            outcome,
            untouched,
            missing,
            left,
        })
    }

    /// `target` に、SHA-256 が `want` のファイルを置く。
    ///
    /// 置く先にあるファイルは、ほかの置く先で要る唯一のものならそこへ移し (Unix は写す)、要らない
    /// ものなら、Windows は `evorag.exe` なら `evorag.rejected` に改名し、ほかは消す。Unix は上から置く。
    fn place(
        &mut self,
        snap: &mut Snapshot,
        targets: &[(FileName, Sha256Digest)],
        roles: &[Sha256Digest],
        target: FileName,
        want: Sha256Digest,
        env: &mut Env<'_>,
    ) -> Result<Placed, SwapError> {
        const SOURCES: [FileName; 5] = [
            FileName::New,
            FileName::Tmp,
            FileName::Rejected,
            FileName::PrevOld,
            FileName::Prev,
        ];
        if snap.holds(target, want) {
            return Ok(Placed::Done);
        }
        let wanted_at = |n: FileName| targets.iter().find(|&&(t, _)| t == n).map(|&(_, d)| d);
        let source = SOURCES.into_iter().find(|&n| {
            n != target && snap.holds(n, want) && wanted_at(n).is_none_or(|d| d != want)
        });
        let Some(source) = source else {
            return Ok(Placed::Missing);
        };
        // evorag.exe を空けたとき、その中身を移した先 (置くのに失敗したら戻す)。
        #[cfg_attr(
            not(windows),
            expect(unused_mut, reason = "only Windows moves the file away")
        )]
        let mut vacated = None;
        match snap.get(target) {
            Entry::Absent => {}
            Entry::Other => return Err(SwapError::ForeignFile { name: target }),
            Entry::File(d) if !roles.contains(&d) => {
                return Err(SwapError::ForeignFile { name: target });
            }
            Entry::File(d) => {
                let needed_at = targets
                    .iter()
                    .find(|&&(n, w)| n != target && w == d && !snap.holds(n, d))
                    .map(|&(n, _)| n);
                let other_copy = FileName::ROLE_NAMES
                    .iter()
                    .any(|&n| n != target && snap.holds(n, d));
                match needed_at {
                    Some(dest) if !other_copy => {
                        if snap.get(dest) != Entry::Absent {
                            return Err(SwapError::ForeignFile { name: dest });
                        }
                        match self.method {
                            #[cfg(windows)]
                            Method::TwoStep => {
                                self.rename(target, dest, env)?;
                                snap.moved(target, dest);
                                vacated = Some(dest);
                            }
                            Method::Replace => {
                                self.copy(target, dest, env)?;
                                snap.copied(target, dest);
                            }
                        }
                    }
                    _ => match self.method {
                        #[cfg(windows)]
                        Method::TwoStep if target == FileName::Exe => {
                            let rejected = FileName::Rejected;
                            match snap.get(rejected) {
                                Entry::Absent => {}
                                Entry::File(r) if roles.contains(&r) => {
                                    self.remove(rejected, env)?;
                                    snap.removed(rejected);
                                }
                                _ => return Err(SwapError::ForeignFile { name: rejected }),
                            }
                            self.rename(target, rejected, env)?;
                            snap.moved(target, rejected);
                            vacated = Some(rejected);
                        }
                        #[cfg(windows)]
                        Method::TwoStep => {
                            self.remove(target, env)?;
                            snap.removed(target);
                        }
                        // 上から置く (rename が置き換える)。
                        Method::Replace => {}
                    },
                }
            }
        }
        match self.rename(source, target, env) {
            Ok(()) => {
                snap.moved(source, target);
                Ok(Placed::Done)
            }
            Err(SwapError::Crashed) => Err(SwapError::Crashed),
            Err(e) => {
                // evorag.exe を空けたままにしない。
                if let Some(v) = vacated
                    && let Err(SwapError::Crashed) = self.rename(v, target, env)
                {
                    return Err(SwapError::Crashed);
                }
                Err(e)
            }
        }
    }

    // ---- ファイルの操作 ----

    fn path(&self, name: FileName) -> PathBuf {
        self.dir.join(name.as_str())
    }

    fn dir_key(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }

    fn check_plan_dir(&self, plan: &Plan) -> Result<(), SwapError> {
        if plan.dir == self.dir_key() {
            Ok(())
        } else {
            Err(SwapError::ForeignPlan {
                dir: PathBuf::from(&plan.dir),
            })
        }
    }

    fn read_state(&self) -> Result<state::State, SwapError> {
        state::read(&self.state.file).map_err(|e| SwapError::StateUnreadable { reason: e.0 })
    }

    fn save(
        &self,
        plan: Option<&Plan>,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        state::write(&self.state, plan, done, || env.event(Event::StateWritten))?;
        env.event(Event::StateSaved)
    }

    /// 計画を済みにする (計画を最後に済んだ操作に移す)。
    fn finish(&self, plan: &Plan, env: &mut Env<'_>) -> Result<(), SwapError> {
        self.save(None, Some(plan), env)
    }

    fn inspect_path(&self, path: &Path) -> Result<Entry, std::io::Error> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::Absent),
            Err(e) => return Err(e),
        };
        if fsutil::is_link_or_reparse_point(&meta) || !meta.is_file() || meta.len() > MAX_EXE_BYTES
        {
            return Ok(Entry::Other);
        }
        let bytes = fsutil::retry_locked(|| std::fs::read(path))?;
        Ok(Entry::File(Sha256Digest::of(&bytes)))
    }

    fn inspect(&self, name: FileName) -> Result<Entry, SwapError> {
        self.inspect_path(&self.path(name))
            .map_err(SwapError::file(FileOp::Read, name))
    }

    fn exists(&self, name: FileName) -> Result<bool, SwapError> {
        match std::fs::symlink_metadata(self.path(name)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SwapError::file(FileOp::Read, name)(e)),
        }
    }

    /// 通常のファイルの SHA-256。無い・通常のファイルでなければ断る。
    fn regular(&self, name: FileName) -> Result<Sha256Digest, SwapError> {
        match self.inspect(name)? {
            Entry::File(d) => Ok(d),
            Entry::Absent | Entry::Other => Err(SwapError::NotRegularFile { name }),
        }
    }

    fn scan(&self) -> Result<Snapshot, SwapError> {
        let mut snap = Snapshot::default();
        for name in FileName::ROLE_NAMES {
            snap.0.insert(name, self.inspect(name)?);
        }
        Ok(snap)
    }

    /// 新しいファイルの Unix の権限: 今の `evorag` の権限から group と other の書き込みを除く。
    fn new_file_mode(&self) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(self.path(FileName::Exe))
                .map_or(0o700, |m| m.permissions().mode() & 0o755)
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    /// 同じ名前のファイルを先に消してから、排他の作成で作る (12 §7.3 の「ファイルの作り方」)。
    fn create_fresh(&self, name: FileName) -> Result<File, SwapError> {
        let path = self.path(name);
        match fsutil::retry_locked(|| std::fs::remove_file(&path)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(SwapError::file(FileOp::Remove, name)(e));
            }
            _ => {}
        }
        fsutil::create_new_file(&path, self.new_file_mode())
            .map_err(SwapError::file(FileOp::Write, name))
    }

    fn write_new(
        &self,
        bytes: &[u8],
        sha256: Sha256Digest,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        let name = FileName::New;
        let mut file = self.create_fresh(name)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(SwapError::file(FileOp::Write, name))?;
        drop(file);
        // 閉じてから開き直して確かめる (12 §7.2 の apply の 8)。
        if self.inspect(name)? != Entry::File(sha256) {
            return Err(SwapError::WrittenDigestMismatch);
        }
        env.event(Event::Wrote(name))
    }

    fn copy(&self, from: FileName, to: FileName, env: &mut Env<'_>) -> Result<(), SwapError> {
        let mut src = File::open(self.path(from)).map_err(SwapError::file(FileOp::Read, from))?;
        let mut dst = self.create_fresh(to)?;
        std::io::copy(&mut src, &mut dst)
            .and_then(|_| dst.sync_all())
            .map_err(SwapError::file(FileOp::Write, to))?;
        drop(dst);
        self.sync_dir(to)?;
        env.event(Event::Wrote(to))
    }

    /// 方式に従って改名する (Windows は上書きしない、Unix は上から置いてフォルダを同期する)。
    /// ロックは 5 回まで試す。
    fn rename(&self, from: FileName, to: FileName, env: &mut Env<'_>) -> Result<(), SwapError> {
        self.raw_rename(from, to)?;
        env.event(Event::Renamed(from, to))
    }

    fn raw_rename(&self, from: FileName, to: FileName) -> Result<(), SwapError> {
        let (f, t) = (self.path(from), self.path(to));
        let r = match self.method {
            #[cfg(windows)]
            Method::TwoStep => fsutil::retry_locked(|| fsutil::rename_no_replace(&f, &t)),
            Method::Replace => fsutil::retry_locked(|| fsutil::rename_replace(&f, &t)),
        };
        r.map_err(SwapError::file(FileOp::Rename, from))?;
        self.sync_dir(to)
    }

    fn remove(&self, name: FileName, env: &mut Env<'_>) -> Result<(), SwapError> {
        match fsutil::retry_locked(|| std::fs::remove_file(self.path(name))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(SwapError::file(FileOp::Remove, name)(e));
            }
            _ => {}
        }
        env.event(Event::Removed(name))
    }

    fn sync_dir(&self, name: FileName) -> Result<(), SwapError> {
        if self.method == Method::Replace {
            fsutil::sync_dir(&self.dir).map_err(SwapError::file(FileOp::Sync, name))?;
        }
        Ok(())
    }

    /// 置き換えの後の確かめ (12 §7.2 の apply の 10): 1 行目が `evorag <版>` であること。
    fn verify(&self, next: &Role, env: &mut Env<'_>) -> Result<(), CheckFailure> {
        let out = env.probe.run_version(&self.path(FileName::Exe))?;
        let expected = next.version.clone().unwrap_or_default();
        let line = out.split('\n').next().unwrap_or_default();
        if line.trim_end_matches('\r') == format!("evorag {expected}") {
            Ok(())
        } else {
            Err(CheckFailure::WrongVersion {
                first_line: probe::first_line(&out),
                expected,
            })
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

/// フォルダを正規化する。Windows では、普通のパスで表せるものは `\\?\` を外す (表示と子のプロセスのため)。
fn canonical_dir(p: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(c)
}

/// 状態のフォルダの `update/lock` を取る (12 §7.3 の「更新のロック」)。
fn lock_update(paths: &StatePaths) -> Result<File, SwapError> {
    let io = |what, path: &Path| {
        let path = path.to_path_buf();
        move |source| SwapError::Io { what, path, source }
    };
    fsutil::create_private_dir(&paths.dir).map_err(io("create", &paths.dir))?;
    fsutil::prepare_private_file(&paths.lock).map_err(io("create", &paths.lock))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .map_err(io("open", &paths.lock))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(SwapError::Busy),
        Err(TryLockError::Error(e)) => Err(io("lock", &paths.lock)(e)),
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
