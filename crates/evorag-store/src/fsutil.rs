//! ファイルとフォルダを書き切る (03 §6 の「書き切る手順」と「入れ替えの rename」)。
//!
//! - Windows: `MoveFileExW` に `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` を付ける。
//!   Windows にはフォルダの書き切りが無い。NTFS はメタデータのジャーナルを順に書くので、電源断の
//!   後は入れ替えの前か後のどちらかになる (どちらでも `migrate::recover` が直す)。
//!   ほかのプログラム (ウイルス対策・索引・同期) が一時的にファイルを掴んでいるときの共有違反は、
//!   少し待って再試行する。長いパスでも動くように `\\?\` を付けて渡す。
//! - Unix: rename の後に、入れ替えたフォルダを書き切る。フォルダの書き切りに対応しない
//!   ファイルシステム (EINVAL / ENOTSUP) では、書き切りを省く。
//!
//! `rename_durable` は同じフォルダの中の入れ替えに使う (書き切るのは `to` の親のフォルダだけ)。

use std::io;
use std::path::Path;

/// `from` を `to` に入れ替え (既にあれば置き換え)、入れ替えをディスクへ書き切る。
pub fn rename_durable(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename(from, to)?;
    if let Some(dir) = to.parent().filter(|d| !d.as_os_str().is_empty()) {
        sync_dir(dir)?;
    }
    Ok(())
}

/// ファイルの中身をディスクへ書き切る (書き込みの権限で開く。Windows の `FlushFileBuffers` は
/// 読み取り専用のハンドルを拒否する)。
pub fn sync_file(path: &Path) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()
}

/// フォルダの中の名前の変更 (作成・rename・削除) を書き切る。Windows では何もしない。
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    imp::sync_dir(dir)
}

/// 所有者だけが読み書きできるフォルダを作る (NFR-503)。
///
/// - Unix: 0700 (既にあったフォルダも締め直す)。
/// - Windows: 新しく作ったときだけ、今の利用者だけを許す保護した DACL を付け、中に継承させる。
///   既にあったフォルダ (利用者が `--data-root` で指した既存のフォルダなど) は変えない
///   (継承を切ると、その下のすべての権限が変わるため)。
pub fn create_private_dir(p: &Path) -> io::Result<()> {
    imp::create_private_dir(p)
}

/// 所有者だけが使えるか (Unix は group と other の権限が無い、Windows は今の利用者だけを許す
/// ACE が 1 つ)。試験のため。
pub fn is_private(p: &Path) -> io::Result<bool> {
    imp::is_private(p)
}

/// 所有者だけが読み書きできるファイルにする。無ければ空のファイルを作る (中身は消さない)。
/// redb に渡す前に呼び、ほかの利用者が読める権限で作られる間を作らない。
pub fn prepare_private_file(p: &Path) -> io::Result<()> {
    imp::prepare_private_file(p)
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Path, Prefix};
    use std::time::Duration;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    /// `\\?\` を付けた NUL 終端の UTF-16 (MAX_PATH を超えるパスのため)。
    fn wide(p: &Path) -> io::Result<Vec<u16>> {
        let abs = std::path::absolute(p)?;
        let mut s: Vec<u16> = Vec::new();
        if let Some(Component::Prefix(pre)) = abs.components().next() {
            match pre.kind() {
                Prefix::Verbatim(_) | Prefix::VerbatimUNC(..) | Prefix::VerbatimDisk(_) => {}
                Prefix::UNC(..) => {
                    // \\server\share\... → \\?\UNC\server\share\...
                    s.extend("\\\\?\\UNC".encode_utf16());
                    s.extend(abs.as_os_str().encode_wide().skip(1));
                    s.push(0);
                    return Ok(s);
                }
                _ => s.extend("\\\\?\\".encode_utf16()),
            }
        }
        s.extend(abs.as_os_str().encode_wide());
        s.push(0);
        Ok(s)
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from)?, wide(to)?);
        // 合わせて約 2 秒まで待つ。
        let mut wait = Duration::from_millis(20);
        for attempt in 0.. {
            // SAFETY: 両方とも NUL で終わる UTF-16 の列で、呼び出しの間は生きている。
            let ok = unsafe {
                MoveFileExW(
                    f.as_ptr(),
                    t.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if ok != 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            let transient = matches!(
                e.raw_os_error(),
                Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            );
            if !transient || attempt == 7 {
                return Err(e);
            }
            std::thread::sleep(wait);
            wait *= 2;
        }
        unreachable!()
    }

    pub fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }

    pub fn create_private_dir(p: &Path) -> io::Result<()> {
        let existed = p.is_dir();
        std::fs::create_dir_all(p)?;
        if !existed {
            crate::winacl::restrict(&wide(p)?, true)?;
        }
        Ok(())
    }

    pub fn prepare_private_file(p: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)?;
        crate::winacl::restrict(&wide(p)?, false)
    }

    pub fn is_private(p: &Path) -> io::Result<bool> {
        crate::winacl::is_owner_only(&wide(p)?, p)
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    // EINVAL と ENOTSUP / EOPNOTSUPP (Linux は 95、macOS は 45 と 102)。
    fn unsupported(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(22 | 45 | 95 | 102))
            || e.kind() == io::ErrorKind::Unsupported
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    pub fn sync_dir(dir: &Path) -> io::Result<()> {
        match std::fs::File::open(dir)?.sync_all() {
            Err(e) if unsupported(&e) => Ok(()),
            r => r,
        }
    }

    pub fn create_private_dir(p: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
        // 既にあったフォルダも締め直す。
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
    }

    pub fn prepare_private_file(p: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(p)?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
    }

    pub fn is_private(p: &Path) -> io::Result<bool> {
        Ok(std::fs::metadata(p)?.permissions().mode() & 0o077 == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_file() {
        let dir = crate::testutil::TempDir::new("rename");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        sync_file(&a).unwrap();
        rename_durable(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
    }

    #[test]
    fn reports_a_missing_source() {
        let dir = crate::testutil::TempDir::new("rename-missing");
        assert!(rename_durable(&dir.path().join("nope"), &dir.path().join("x")).is_err());
    }

    #[test]
    fn works_with_paths_longer_than_max_path() {
        let dir = crate::testutil::TempDir::new("rename-long");
        let mut deep = dir.path().to_path_buf();
        for i in 0..12 {
            deep.push(format!("{i:02}-long-directory-name-for-max-path"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        assert!(deep.as_os_str().len() > 300);
        let (a, b) = (deep.join("a"), deep.join("b"));
        std::fs::write(&a, b"x").unwrap();
        rename_durable(&a, &b).unwrap();
        assert!(b.exists());
    }

    #[test]
    fn prepare_private_file_keeps_existing_content() {
        let dir = crate::testutil::TempDir::new("private-file");
        let f = dir.path().join("f");
        std::fs::write(&f, b"keep").unwrap();
        prepare_private_file(&f).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"keep");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// verifies: AC-NFR-503-1 (作ったフォルダとファイル、その中に作ったファイル)
    #[test]
    fn private_dirs_and_files_are_owner_only() {
        let dir = crate::testutil::TempDir::new("private");
        let root = dir.path().join("made").join("here");
        create_private_dir(&root).unwrap();
        assert!(is_private(&root).unwrap());
        // 中に普通に作ったファイルも、継承で所有者だけになる。
        let inner = root.join("inner.txt");
        std::fs::write(&inner, b"x").unwrap();
        assert!(is_private(&inner).unwrap() || cfg!(unix));
        // 外のフォルダのファイルも、prepare_private_file で所有者だけにする。
        let outside = dir.path().join("file.bin");
        std::fs::write(&outside, b"y").unwrap();
        prepare_private_file(&outside).unwrap();
        assert!(is_private(&outside).unwrap());
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"y",
            "the content is kept"
        );
    }

    /// Windows: 既にあったフォルダの権限は変えない (利用者のフォルダの継承を壊さない)。
    #[cfg(windows)]
    #[test]
    fn existing_dirs_are_left_alone_on_windows() {
        let dir = crate::testutil::TempDir::new("private-existing");
        let before = is_private(dir.path()).unwrap();
        create_private_dir(dir.path()).unwrap();
        assert_eq!(is_private(dir.path()).unwrap(), before);
    }
}
