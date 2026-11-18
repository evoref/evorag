//! ファイルの操作: 調べる・写す・改名する・消す・作る・書く・同期する、状態のファイルの読み書き、
//! `--version` の確かめ (12 §7.2・§7.3)。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use evorag_store::fsutil;

use super::super::env::{Env, Event};
use super::super::error::{CheckFailure, FileOp, SwapError};
use super::super::names::{FileName, Method};
use super::super::probe;
use super::super::state::{self, Plan, Role};
use super::Installation;
use super::converge::Entry;
use crate::{MAX_EXE_BYTES, Sha256Digest};

impl Installation {
    pub(super) fn path(&self, name: FileName) -> PathBuf {
        self.dir.join(name.as_str())
    }

    /// 計画の `dir` に書く、インストール先のパスの文字列。
    ///
    /// [`Path::to_string_lossy`] で作る (UTF-8 でないパスは置き換え文字を含む)。比べるのは同じ
    /// インストール先から作った文字列どうしなので、置き換えても同じものは同じになる。
    pub(super) fn dir_key(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }

    pub(super) fn check_plan_dir(&self, plan: &Plan) -> Result<(), SwapError> {
        if plan.dir == self.dir_key() {
            Ok(())
        } else {
            Err(SwapError::ForeignPlan {
                dir: PathBuf::from(&plan.dir),
            })
        }
    }

    pub(super) fn read_state(&self) -> Result<state::State, SwapError> {
        state::read(&self.state.file).map_err(|e| SwapError::StateUnreadable { reason: e.0 })
    }

    pub(super) fn save(
        &self,
        plan: Option<&Plan>,
        done: Option<&Plan>,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        state::write(&self.state, plan, done, || env.event(Event::StateWritten))?;
        env.event(Event::StateSaved)
    }

    /// 計画を済みにする (計画を最後に済んだ操作に移す)。
    pub(super) fn finish(&self, plan: &Plan, env: &mut Env<'_>) -> Result<(), SwapError> {
        self.save(None, Some(plan), env)
    }

    /// パスのファイルを調べる。リンク (名前の代理のリパースポイントを含む)・通常のファイルでないもの・
    /// 大きすぎるもの・(Unix) 動いている利用者のものでないものは [`Entry::Other`] (どの役割にも当たらない)。
    /// 中身は少しずつ読んで SHA-256 を求める (主記憶にまとめて読まない)。
    pub(super) fn inspect_path(&self, path: &Path) -> std::io::Result<Entry> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::Absent),
            Err(e) => return Err(e),
        };
        if fsutil::is_link_or_reparse_point(&meta) || !acceptable(&meta) {
            return Ok(Entry::Other);
        }
        fsutil::retry_locked(|| {
            let file = fsutil::open_nofollow(path)?;
            // 開いたもの (調べた後に差し替わっていないか) をもう一度見る。
            if !acceptable(&file.metadata()?) {
                return Ok(Entry::Other);
            }
            let (digest, len) = Sha256Digest::of_reader(file.take(MAX_EXE_BYTES + 1))?;
            Ok(if len > MAX_EXE_BYTES {
                Entry::Other
            } else {
                Entry::File(digest)
            })
        })
    }

    pub(super) fn inspect(&self, name: FileName) -> Result<Entry, SwapError> {
        self.inspect_path(&self.path(name))
            .map_err(SwapError::file(FileOp::Read, name))
    }

    pub(super) fn exists(&self, name: FileName) -> Result<bool, SwapError> {
        match std::fs::symlink_metadata(self.path(name)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SwapError::file(FileOp::Read, name)(e)),
        }
    }

    /// 通常のファイルの SHA-256。無い・通常のファイルでなければ断る。
    pub(super) fn regular(&self, name: FileName) -> Result<Sha256Digest, SwapError> {
        match self.inspect(name)? {
            Entry::File(d) => Ok(d),
            Entry::Absent | Entry::Other => Err(SwapError::NotRegularFile { name }),
        }
    }

    /// `--version` で動かすパス。Windows で拡張子が `.exe` でない名前 (`evorag.prev` など) のときは、
    /// `<名前>.exe` があれば断る (`CreateProcess` が代わりにそれを動かすおそれがあるため)。
    pub(super) fn probe_path(&self, name: FileName) -> Result<PathBuf, SwapError> {
        let path = self.path(name);
        #[cfg(windows)]
        if !name.as_str().to_ascii_lowercase().ends_with(".exe") {
            let shadow = self.dir.join(format!("{}.exe", name.as_str()));
            match std::fs::symlink_metadata(&shadow) {
                Ok(_) => return Err(SwapError::BlockingFile { path: shadow }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(SwapError::io(FileOp::Read, &shadow)(e)),
            }
        }
        Ok(path)
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
            .map_err(SwapError::file(FileOp::Create, name))
    }

    /// `evorag.new` を書いて書き切り、閉じてから開き直して確かめる (12 §7.2 の apply の 8)。
    pub(super) fn write_new(
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
        if self.inspect(name)? != Entry::File(sha256) {
            return Err(SwapError::WrittenDigestMismatch);
        }
        env.event(Event::Wrote(name))
    }

    /// `from` の写しを `to` に書いて書き切る (Unix はフォルダも同期する)。
    pub(super) fn copy(
        &self,
        from: FileName,
        to: FileName,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        let mut src = fsutil::retry_locked(|| fsutil::open_nofollow(&self.path(from)))
            .map_err(SwapError::file(FileOp::Read, from))?;
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
    pub(super) fn rename(
        &self,
        from: FileName,
        to: FileName,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        self.raw_rename(from, to)?;
        env.event(Event::Renamed(from, to))
    }

    /// Windows の隙間の 2 つ目の改名: 上書きしない改名を 1 回だけ試す (再試行しない)。
    #[cfg(windows)]
    pub(super) fn rename_once(
        &self,
        from: FileName,
        to: FileName,
        env: &mut Env<'_>,
    ) -> Result<(), SwapError> {
        fsutil::rename_no_replace(&self.path(from), &self.path(to))
            .map_err(SwapError::file(FileOp::Rename, from))?;
        env.event(Event::Renamed(from, to))
    }

    /// Windows の隙間の前に、`name` をほかのプログラムが掴んでいないことを確かめる (削除の権限・共有
    /// なしで開いて閉じる。ロックは 5 回まで試す)。
    #[cfg(windows)]
    pub(super) fn check_not_open(&self, name: FileName) -> Result<(), SwapError> {
        let path = self.path(name);
        fsutil::retry_locked(|| fsutil::check_not_open(&path))
            .map_err(SwapError::file(FileOp::Open, name))
    }

    pub(super) fn raw_rename(&self, from: FileName, to: FileName) -> Result<(), SwapError> {
        let (f, t) = (self.path(from), self.path(to));
        let r = match self.method {
            #[cfg(windows)]
            Method::TwoStep => fsutil::retry_locked(|| fsutil::rename_no_replace(&f, &t)),
            Method::Replace => fsutil::retry_locked(|| fsutil::rename_replace(&f, &t)),
        };
        r.map_err(SwapError::file(FileOp::Rename, from))?;
        self.sync_dir(to)
    }

    pub(super) fn remove(&self, name: FileName, env: &mut Env<'_>) -> Result<(), SwapError> {
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
    pub(super) fn verify(&self, next: &Role, env: &mut Env<'_>) -> Result<(), CheckFailure> {
        let out = env.probe().run_version(&self.path(FileName::Exe))?;
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
}

/// 役割のファイルになりうるか: 通常のファイルで大きすぎず、(Unix) 動いている利用者のもの。
fn acceptable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != fsutil::effective_uid() {
            return false;
        }
    }
    meta.is_file() && meta.len() <= MAX_EXE_BYTES
}

/// Linux: 動いているイメージ (`/proc/self/exe`) の SHA-256。名前の上のファイルが差し替わっていても、
/// 動いているものを読む。
#[cfg(target_os = "linux")]
pub(super) fn digest_of_running_image() -> Option<Sha256Digest> {
    let file = File::open("/proc/self/exe").ok()?;
    let (digest, len) = Sha256Digest::of_reader(file.take(MAX_EXE_BYTES + 1)).ok()?;
    (len <= MAX_EXE_BYTES).then_some(digest)
}
