//! ファイルの入れ替えを書き切る (03 §6 の「書き切る手順」と「入れ替えの rename」)。
//!
//! - Windows: `MoveFileExW` に `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` を付ける
//!   (std の `fs::rename` は WRITE_THROUGH を付けない。Windows にはフォルダの書き切りが無いので、この旗で代える)。
//! - Unix: rename の後に、入れ替えたフォルダを書き切る。

use std::io;
use std::path::Path;

/// `from` を `to` に入れ替え (既にあれば置き換え)、入れ替えをディスクへ書き切る。
pub fn rename_durable(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_durable(from, to)
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

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub fn rename_durable(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from), wide(to));
        // SAFETY: 両方とも NUL で終わる UTF-16 の列で、呼び出しの間は生きている。
        let ok = unsafe {
            MoveFileExW(
                f.as_ptr(),
                t.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::Path;

    pub fn rename_durable(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)?;
        let dir = to
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::File::open(dir)?.sync_all()
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
}
