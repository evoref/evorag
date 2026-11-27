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

/// `from` を `to` に入れ替える (既にあれば置き換え)。フォルダは書き切らない (呼び手が [`sync_dir`] を別に
/// 呼び、入れ替えの成否と書き切りの成否を分けて扱うため)。Windows は `MOVEFILE_WRITE_THROUGH` を付け、
/// 一時の共有違反を [`retry_transient`] と同じ間隔で試し直す。
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename(from, to)
}

/// ほかのプログラムが一時的にファイルを掴んでいるための失敗 (Windows の共有・ロックの違反とアクセスの拒否) を、
/// 少し待って試し直す (合わせて約 2 秒)。Unix では 1 回だけ呼ぶ。
pub fn retry_transient<T>(f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    imp::retry_transient(f)
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
/// - Windows: 新しく作ったときだけ、今の利用者・SYSTEM・Administrators だけを許す保護した DACL を
///   付け、中に継承させる (03 §2)。
///   既にあったフォルダ (利用者が `--data-root` で指した既存のフォルダなど) は変えない
///   (継承を切ると、その下のすべての権限が変わるため)。
pub fn create_private_dir(p: &Path) -> io::Result<()> {
    imp::create_private_dir(p)
}

/// ほかの一般の利用者が使えないか (Unix は group と other の権限が無い、Windows は許す相手が
/// 今の利用者・SYSTEM・Administrators だけ)。試験のため。
pub fn is_private(p: &Path) -> io::Result<bool> {
    imp::is_private(p)
}

/// 所有者だけの権限を付けられる置き場所か。Windows は、ネットワークの置き場所と ACL を持たない
/// ファイルシステム (FAT・exFAT) では付けない (03 §2)。Unix は常に真。
pub fn acl_applicable(p: &Path) -> io::Result<bool> {
    imp::acl_applicable(p)
}

/// 所有者だけが読み書きできるファイルにする。無ければ空のファイルを作る (中身は消さない)。
/// redb に渡す前に呼び、ほかの利用者が読める権限で作られる間を作らない。
pub fn prepare_private_file(p: &Path) -> io::Result<()> {
    imp::prepare_private_file(p)
}

/// ロックの再試行の待ち (12 §7.3 の「ウイルス対策のロック」)。試すのは合わせて 5 回、待ちは合わせて 2 秒。
pub const LOCK_RETRY_WAITS: [std::time::Duration; 4] = [
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(400),
    std::time::Duration::from_millis(600),
    std::time::Duration::from_millis(800),
];

/// ほかのプログラム (ウイルス対策など) がファイルを掴んでいるための失敗か。Windows はアクセスの拒否・
/// 共有の違反・ロックの違反、Unix は `EBUSY`・`ETXTBSY`。
#[must_use]
pub fn is_lock_error(e: &io::Error) -> bool {
    imp::is_lock_error(e)
}

/// ロックの失敗 ([`is_lock_error`]) を、[`LOCK_RETRY_WAITS`] の間隔で 5 回まで試す (12 §7.3)。
/// ほかの失敗はすぐ返す。
///
/// # Errors
///
/// 最後の試みの失敗か、ロックでない失敗。
pub fn retry_locked<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for wait in LOCK_RETRY_WAITS {
        match f() {
            Err(e) if is_lock_error(&e) => std::thread::sleep(wait),
            r => return r,
        }
    }
    f()
}

/// `from` を `to` に改名する。`to` があれば上書きせずに失敗する (`AlreadyExists`)。Windows だけ。
///
/// `MoveFileExW` に `MOVEFILE_WRITE_THROUGH` だけを付ける (`MOVEFILE_REPLACE_EXISTING` を付けない)。
/// 動いている実行ファイルの改名にも使える (12 §7.3)。再試行はしない ([`retry_locked`] で包む)。
///
/// # Errors
///
/// OS の改名の失敗。
#[cfg(windows)]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_no_replace(from, to)
}

/// フォルダ `from` を、同じボリュームの `to` に改名する。`to` が既にあれば改名せずに `AlreadyExists` で失敗する
/// (データ根を入れ替える改名で、ほかのフォルダを置き換えないため、03 §10.2・§12.4)。1 回だけ試し、親のフォルダは
/// 書き切らない (呼び手が [`sync_dir`] を別に呼び、改名の成否と書き切りの成否を分けて扱うため)。
///
/// - Windows: `MoveFileExW` に `MOVEFILE_WRITE_THROUGH` だけを付ける (置き換えない)。
/// - Linux: `renameat2(RENAME_NOREPLACE)`。ファイルシステムが対応しない (`EINVAL`・`ENOSYS`) ときだけ、下の
///   確かめてからの改名に戻す。
/// - macOS: `renamex_np(RENAME_EXCL)`。対応しない (`ENOTSUP`) ときだけ、確かめてからの改名に戻す。
/// - ほかの Unix: `to` が無いことを確かめてから `rename(2)` する (確かめと改名の間に `to` を作られる隙は残る)。
///
/// # Errors
///
/// `to` がある (`AlreadyExists`)、OS の改名の失敗。
pub fn rename_dir_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the rename target already exists",
        ));
    }
    imp::rename_dir_no_replace(from, to)
}

/// 所有者だけが読み書きできるフォルダを、新しく 1 段だけ作る (親は作らない)。既にあれば `AlreadyExists` で失敗する
/// (同じ名前のフォルダをほかが先に作った隙を、作った後に気付かずに使わないため、03 §12.4 の「書き方」)。権限は
/// [`create_private_dir`] と同じ。
///
/// # Errors
///
/// 既にある (`AlreadyExists`)・親が無い・作れない・権限を付けられないとき。
pub fn create_private_dir_new(p: &Path) -> io::Result<()> {
    imp::create_private_dir_new(p)
}

/// リンクか: シンボリックリンクと、Windows の名前の代理のリパースポイント (ジャンクションなど、
/// タグに `IsReparseTagNameSurrogate` の 0x2000_0000 が立つもの)。名前の代理でないリパースポイント
/// (OneDrive のクラウドのファイルなど) は通常のファイルとして扱う (12 §7.3)。
/// `meta` は [`std::fs::symlink_metadata`] で取ったもの。
#[must_use]
pub fn is_link_or_reparse_point(meta: &std::fs::Metadata) -> bool {
    imp::is_link_or_reparse_point(meta)
}

/// 読むためにファイルを開く。Unix では `O_NOFOLLOW` を付け、名前がシンボリックリンクなら失敗する
/// (確かめた後に差し替えられたリンクの先を読まないため)。
///
/// # Errors
///
/// 開けないとき。
pub fn open_nofollow(path: &Path) -> io::Result<std::fs::File> {
    imp::open_nofollow(path)
}

/// 新しいファイルを排他で作る (`create_new`。既にあれば `AlreadyExists`)。読み書きで開く。
///
/// Unix では `O_NOFOLLOW` を付け、権限を `unix_mode` にする (umask に関わらず)。Windows では
/// `unix_mode` を使わない (権限はフォルダから継承する)。名前にシンボリックリンクがあっても、その先に
/// 書かない (12 §7.3 の「ファイルの作り方」)。
///
/// # Errors
///
/// 作れないとき。
pub fn create_new_file(path: &Path, unix_mode: u32) -> io::Result<std::fs::File> {
    imp::create_new_file(path, unix_mode)
}

/// ほかのプログラムがファイルを開いていないかを、削除の権限 (`DELETE`) と共有なし (`share_mode(0)`) で
/// 開いて、すぐ閉じて確かめる。Windows だけ。再試行はしない ([`retry_locked`] で包む)。
///
/// 自分を改名してから置く置き換えで、`evorag.exe` を空ける前に、置くファイルを掴まれていないことを
/// 確かめるために使う (12 §7.3 の「Windows の隙間」)。
///
/// # Errors
///
/// 開けないとき (ほかが開いていれば共有の違反)。
#[cfg(windows)]
pub fn check_not_open(path: &Path) -> io::Result<()> {
    imp::check_not_open(path)
}

/// インストール先のフォルダに、今の利用者・SYSTEM・Administrators・TrustedInstaller のほかの相手へ
/// 書き込みの類の権限を許す ACE があるか。Windows だけ。ACL を持たない置き場所・ネットワークの置き場所
/// ([`acl_applicable`] が偽) では偽 (確かめられない)。継承だけの ACE は、中のファイルへ継承するもの
/// (`OBJECT_INHERIT_ACE`) だけを見る (相手が CREATOR OWNER・CREATOR GROUP なら許す)。そうした ACE は、
/// フォルダの中に新しく作るファイル (`evorag.new`・`evorag.tmp`・`evorag.probe`) に効く。同じボリュームの
/// 中の改名ではセキュリティ記述子が変わらない (改名したファイルは継承し直さない) ので、改名で置くファイルの
/// 権限は、作ったときのまま。
///
/// # Errors
///
/// DACL を読めないとき。
#[cfg(windows)]
pub fn writable_by_others(dir: &Path) -> io::Result<bool> {
    imp::writable_by_others(dir)
}

/// フォルダの持ち主が、今の利用者・SYSTEM・Administrators・TrustedInstaller のどれでもないか。Windows
/// だけ。持ち主は DACL を書き換えられる (`WRITE_DAC` を暗に持つ) ため、インストール先の確かめで見る。
/// ACL を持たない置き場所・ネットワークの置き場所 ([`acl_applicable`] が偽) では偽 (確かめられない)。
///
/// # Errors
///
/// 持ち主を読めないとき。
#[cfg(windows)]
pub fn owned_by_others(dir: &Path) -> io::Result<bool> {
    imp::owned_by_others(dir)
}

/// 動いているプロセスの実効の利用者の ID。Unix だけ。
#[cfg(unix)]
#[must_use]
pub fn effective_uid() -> u32 {
    imp::effective_uid()
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
                Prefix::Verbatim(_)
                | Prefix::VerbatimUNC(..)
                | Prefix::VerbatimDisk(_)
                | Prefix::DeviceNS(_) => {}
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

    pub fn retry_transient<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        // 合わせて約 2 秒まで待つ。
        let mut wait = Duration::from_millis(20);
        let mut attempt = 0;
        loop {
            let e = match f() {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if !is_lock_error(&e) || attempt == 7 {
                return Err(e);
            }
            std::thread::sleep(wait);
            wait *= 2;
            attempt += 1;
        }
    }

    pub fn is_lock_error(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        )
    }

    pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from)?, wide(to)?);
        // SAFETY: 両方とも NUL で終わる UTF-16 の列で、呼び出しの間は生きている。
        let ok = unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn rename_dir_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        rename_no_replace(from, to)
    }

    pub fn create_private_dir_new(p: &Path) -> io::Result<()> {
        std::fs::create_dir(p)?;
        crate::winacl::restrict(&wide(&std::path::absolute(p)?)?, true)
    }

    pub fn is_link_or_reparse_point(meta: &std::fs::Metadata) -> bool {
        // std の is_symlink は、名前の代理のリパースポイント (タグの 0x2000_0000 が立つもの: シンボリック
        // リンク・ジャンクションなど) で真になる。OneDrive のクラウドのファイルなどほかのリパースポイントは
        // 通常のファイルとして扱う。
        meta.file_type().is_symlink()
    }

    pub fn open_nofollow(path: &Path) -> io::Result<std::fs::File> {
        std::fs::File::open(path)
    }

    pub fn check_not_open(path: &Path) -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        const DELETE: u32 = 0x0001_0000;
        std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(0)
            .open(path)
            .map(drop)
    }

    pub fn writable_by_others(dir: &Path) -> io::Result<bool> {
        let w = wide(dir)?;
        if !crate::winacl::applicable(&w) {
            return Ok(false);
        }
        crate::winacl::grants_write_to_others(&w)
    }

    pub fn owned_by_others(dir: &Path) -> io::Result<bool> {
        let w = wide(dir)?;
        if !crate::winacl::applicable(&w) {
            return Ok(false);
        }
        crate::winacl::owner_is_allowed(&w).map(|allowed| !allowed)
    }

    pub fn create_new_file(path: &Path, _unix_mode: u32) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from)?, wide(to)?);
        retry_transient(|| {
            // SAFETY: 両方とも NUL で終わる UTF-16 の列で、呼び出しの間は生きている。
            let ok = unsafe {
                MoveFileExW(
                    f.as_ptr(),
                    t.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if ok != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
    }

    pub fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }

    pub fn create_private_dir(p: &Path) -> io::Result<()> {
        // 新しく作る段のうち、いちばん上 (ここから下は継承で所有者だけになる)。
        let abs = std::path::absolute(p)?;
        let top = abs
            .ancestors()
            .take_while(|a| !a.exists())
            .last()
            .map(Path::to_path_buf);
        std::fs::create_dir_all(p)?;
        if let Some(top) = top {
            crate::winacl::restrict(&wide(&top)?, true)?;
        }
        Ok(())
    }

    pub fn prepare_private_file(p: &Path) -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        // 新しく作るときは、ほかから開けないように (共有なしで) 開いたまま権限を付ける。
        // 付ける前の継承した権限で、ほかのプロセスが開いておく間を作らないため
        // (権限の変更は共有の検査を受けないので、開いたままで付けられる)。
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(p)
        {
            Ok(handle) => {
                let r = crate::winacl::restrict(&wide(p)?, false);
                drop(handle);
                r
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                crate::winacl::restrict(&wide(p)?, false)
            }
            Err(e) => Err(e),
        }
    }

    pub fn is_private(p: &Path) -> io::Result<bool> {
        crate::winacl::is_owner_only(&wide(p)?)
    }

    pub fn acl_applicable(p: &Path) -> io::Result<bool> {
        Ok(crate::winacl::applicable(&wide(p)?))
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

    pub fn retry_transient<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        f()
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    pub fn rename_dir_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        match rename_exclusive(from, to) {
            Some(r) => r,
            // 置き換えない改名に対応しないファイルシステム: 確かめてから改名する。
            None => {
                if std::fs::symlink_metadata(to).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "the rename target already exists",
                    ));
                }
                std::fs::rename(from, to)
            }
        }
    }

    fn c_path(p: &Path) -> io::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"))
    }

    /// 置き換えない改名。対応しない (呼び手が確かめてからの改名に戻す) ときは `None`。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn rename_exclusive(from: &Path, to: &Path) -> Option<io::Result<()>> {
        let (f, t) = match (c_path(from), c_path(to)) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(e), _) | (_, Err(e)) => return Some(Err(e)),
        };
        // SAFETY: 両方とも NUL で終わる C の文字列で、呼び出しの間は生きている。AT_FDCWD は相対パスを今の
        // フォルダから解く (渡すのは絶対パス)。
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE as _,
            )
        };
        if rc == 0 {
            return Some(Ok(()));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINVAL | libc::ENOSYS) => None,
            Some(libc::EEXIST) => Some(Err(io::Error::new(io::ErrorKind::AlreadyExists, e))),
            _ => Some(Err(e)),
        }
    }

    /// 置き換えない改名。対応しない (呼び手が確かめてからの改名に戻す) ときは `None`。
    #[cfg(target_os = "macos")]
    fn rename_exclusive(from: &Path, to: &Path) -> Option<io::Result<()>> {
        let (f, t) = match (c_path(from), c_path(to)) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(e), _) | (_, Err(e)) => return Some(Err(e)),
        };
        // SAFETY: 両方とも NUL で終わる C の文字列で、呼び出しの間は生きている。
        let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Some(Ok(()));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOTSUP) => None,
            Some(libc::EEXIST) => Some(Err(io::Error::new(io::ErrorKind::AlreadyExists, e))),
            _ => Some(Err(e)),
        }
    }

    /// 置き換えない改名を持たない Unix: 常に確かめてからの改名に戻す。
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn rename_exclusive(_from: &Path, _to: &Path) -> Option<io::Result<()>> {
        None
    }

    pub fn create_private_dir_new(p: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new().mode(0o700).create(p)?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
    }

    pub fn is_lock_error(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::EBUSY | libc::ETXTBSY))
    }

    pub fn is_link_or_reparse_point(meta: &std::fs::Metadata) -> bool {
        meta.file_type().is_symlink()
    }

    pub fn open_nofollow(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    pub fn create_new_file(path: &Path, unix_mode: u32) -> io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(unix_mode)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(unix_mode))?;
        Ok(file)
    }

    pub fn effective_uid() -> u32 {
        // SAFETY: geteuid は引数を取らず、常に成功し、プロセスの状態を変えない (POSIX)。
        unsafe { libc::geteuid() }
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

    pub fn acl_applicable(_p: &Path) -> io::Result<bool> {
        Ok(true)
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
        if !acl_applicable(dir.path()).unwrap() {
            eprintln!("skipped: the temp folder is on a volume without ACLs");
            return;
        }
        let root = dir.path().join("made").join("here");
        create_private_dir(&root).unwrap();
        assert!(is_private(&root).unwrap());
        // 途中に作った段も所有者だけ (Unix の 0700 と同じ)。
        assert!(is_private(&dir.path().join("made")).unwrap());
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

    #[test]
    fn retry_locked_retries_only_lock_errors() {
        let lock = || {
            if cfg!(windows) {
                io::Error::from_raw_os_error(32)
            } else {
                io::Error::from_raw_os_error(16)
            }
        };
        assert!(is_lock_error(&lock()));
        let mut n = 0;
        let start = std::time::Instant::now();
        let r = retry_locked(|| {
            n += 1;
            if n < 3 { Err(lock()) } else { Ok(n) }
        });
        assert_eq!(r.unwrap(), 3);
        assert!(start.elapsed() >= std::time::Duration::from_millis(600));
        let mut n = 0;
        let r: io::Result<()> = retry_locked(|| {
            n += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert!(r.is_err());
        assert_eq!(n, 1, "other errors are not retried");
    }

    #[test]
    fn create_new_file_refuses_existing_names() {
        let dir = crate::testutil::TempDir::new("create-new");
        let p = dir.path().join("f");
        drop(create_new_file(&p, 0o640).unwrap());
        assert_eq!(
            create_new_file(&p, 0o640).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o640
            );
            // 名前にあるシンボリックリンクの先には書かない。
            let target = dir.path().join("target");
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(create_new_file(&link, 0o600).is_err());
            assert!(!target.exists());
            let meta = std::fs::symlink_metadata(&link).unwrap();
            assert!(is_link_or_reparse_point(&meta));
        }
        let meta = std::fs::symlink_metadata(&p).unwrap();
        assert!(!is_link_or_reparse_point(&meta));
    }

    #[cfg(windows)]
    #[test]
    fn rename_no_replace_keeps_an_existing_target() {
        let dir = crate::testutil::TempDir::new("rename-noreplace");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        assert_eq!(
            rename_no_replace(&a, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&b).unwrap(), b"old");
        std::fs::remove_file(&b).unwrap();
        rename_no_replace(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"new");
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

    /// Windows: ほかが開いている (削除の共有なし) と、開けない。閉じれば開ける。
    #[cfg(windows)]
    #[test]
    fn check_not_open_sees_other_handles() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = crate::testutil::TempDir::new("not-open");
        let p = dir.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        check_not_open(&p).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(&p)
            .unwrap();
        assert!(is_lock_error(&check_not_open(&p).unwrap_err()));
        drop(held);
        check_not_open(&p).unwrap();
    }

    /// Windows: 保護した DACL のフォルダは、ほかの利用者が書けない。Users に書き込みを許すと書ける。
    /// TrustedInstaller と、継承だけの ACE は数えない。
    #[cfg(windows)]
    #[test]
    fn writable_by_others_reads_the_dacl() {
        let dir = crate::testutil::TempDir::new("writable-by-others");
        if !acl_applicable(dir.path()).unwrap() {
            eprintln!("skipped: the temp folder is on a volume without ACLs");
            return;
        }
        let d = dir.path().join("install");
        create_private_dir(&d).unwrap();
        assert!(!writable_by_others(&d).unwrap());
        assert!(!owned_by_others(&d).unwrap(), "a folder we create is ours");
        let icacls = |arg: &str| {
            std::process::Command::new("icacls")
                .arg(&d)
                .args(["/grant", arg])
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(icacls(
            "*S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464:(F)"
        ));
        assert!(icacls("*S-1-3-0:(OI)(CI)(IO)(F)"));
        assert!(icacls("*S-1-3-1:(OI)(IO)(M)"));
        assert!(icacls("*S-1-5-32-545:(CI)(IO)(F)"));
        assert!(icacls("*S-1-5-32-545:(RX)"));
        assert!(!writable_by_others(&d).unwrap());
        assert!(icacls("*S-1-5-32-545:(AD)"));
        assert!(writable_by_others(&d).unwrap());
        // 中に作るファイルへ継承する ACE (継承だけ) で、Users に変更を許す。
        let d = dir.path().join("inherit");
        create_private_dir(&d).unwrap();
        let icacls = |arg: &str| {
            std::process::Command::new("icacls")
                .arg(&d)
                .args(["/grant", arg])
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(icacls("*S-1-5-32-545:(OI)(IO)(M)"));
        assert!(writable_by_others(&d).unwrap());
    }
}
