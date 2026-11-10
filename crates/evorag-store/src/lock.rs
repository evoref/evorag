//! データ根のロック (03 §9、FR-203)。
//!
//! `lock` に、プロセスが生きている間ずっと OS のロック (`File::try_lock`) を持つ。ファイルが
//! あるかどうかでは判定しない (異常終了の後に残ったファイルで誤判定しないため)。Windows の
//! ロックは強制ロックで、ロックしたファイルは他から読めないので、PID は `lock.pid` に別に書く。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "lock";
pub const PID_FILE: &str = "lock.pid";

/// データ根のロック。落とすと外れる。
#[derive(Debug)]
pub struct DataRootLock {
    _file: File,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// ほかの evorag が使っている。分かれば PID を示す。
    #[error("the data root is in use by another evorag process{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default())]
    InUse { pid: Option<u32> },
    #[error("failed to lock the data root: {0}")]
    Io(#[from] std::io::Error),
}

impl DataRootLock {
    pub fn acquire(data_root: &Path) -> Result<Self, LockError> {
        let path = data_root.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LockError::InUse {
                    pid: read_pid(data_root),
                });
            }
            Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }
        // PID は参考の情報。書けなくてもロックは有効。
        let _ = write_pid(data_root);
        Ok(Self { _file: file, path })
    }

    /// 何も書かずにロックを取る (点検のため、AC-FR-207-1)。ロックのファイルが無ければ、使っている
    /// プロセスも無いので `None`。
    pub fn acquire_quiet(data_root: &Path) -> Result<Option<Self>, LockError> {
        let path = data_root.join(LOCK_FILE);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LockError::Io(e)),
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(TryLockError::WouldBlock) => Err(LockError::InUse {
                pid: read_pid(data_root),
            }),
            Err(TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_pid(data_root: &Path) -> Option<u32> {
    fs::read_to_string(data_root.join(PID_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 一時名に書いてから入れ替える (読み手が空のファイルを読まないように)。
fn write_pid(data_root: &Path) -> std::io::Result<()> {
    let tmp = data_root.join(format!("{PID_FILE}.tmp"));
    let mut f = File::create(&tmp)?;
    writeln!(f, "{}", std::process::id())?;
    drop(f);
    fs::rename(&tmp, data_root.join(PID_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_on_the_same_root_is_refused_with_the_pid() {
        let dir = crate::testutil::TempDir::new("lock");
        let first = DataRootLock::acquire(dir.path()).unwrap();
        match DataRootLock::acquire(dir.path()) {
            Err(LockError::InUse { pid }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected InUse, got {other:?}"),
        }
        drop(first);
        // 外した後は、残った lock のファイルがあっても取れる。
        assert!(dir.path().join(LOCK_FILE).exists());
        DataRootLock::acquire(dir.path()).unwrap();
    }
}
