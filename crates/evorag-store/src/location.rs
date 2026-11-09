//! 置き場所の検査と空きの量 (03 §2.2、§8)。
//!
//! - 危ない置き場所 (ネットワーク・同期フォルダ) は、同期による書き換えやロックの無効化で壊れや
//!   すいので、既定で起動を拒否する (設定で警告だけにできる、FR-216)。判定は経験則で、漏れはありうる。
//! - Windows は kernel32 の関数を直接宣言し、Linux は `statfs` を使う (C を含む依存を足さない、ADR-0015)。

use std::path::Path;

/// 置き場所の判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// 使えるが注意が要る (FAT / exFAT など)。理由。
    Warn(String),
    /// 危ない (ネットワーク・同期フォルダ)。理由。
    Unsafe(String),
}

/// 置き場所を調べる。まだ無いフォルダは、あるところまで遡って調べる。
pub fn check(path: &Path) -> Verdict {
    let mut p = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    while !p.exists() {
        match p.parent() {
            Some(parent) => p = parent.to_path_buf(),
            None => break,
        }
    }
    imp::check(&p)
}

/// `path` のある場所の空きのバイト数。分からなければ `None`。
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = std::path::absolute(path).ok()?;
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    imp::free_bytes(&p)
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use super::Verdict;

    const DRIVE_REMOTE: u32 = 4;
    const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    const FILE_ATTRIBUTE_PINNED: u32 = 0x8_0000;
    const FILE_ATTRIBUTE_UNPINNED: u32 = 0x10_0000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_len: u32,
        ) -> i32;
        fn GetFileAttributesW(path: *const u16) -> u32;
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn volume_root(p: &Path) -> Option<Vec<u16>> {
        let w = wide(p);
        let mut out = vec![0u16; 1024];
        // SAFETY: `w` は NUL で終わる UTF-16、`out` は長さを渡したバッファ。
        let ok = unsafe { GetVolumePathNameW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
        if ok == 0 {
            return None;
        }
        let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
        out.truncate(end + 1);
        Some(out)
    }

    fn file_system(root: &[u16]) -> Option<String> {
        let mut fs = vec![0u16; 64];
        // SAFETY: `root` は NUL で終わる UTF-16。出力のポインタは null か、長さを渡したバッファ。
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        (ok != 0).then(|| from_wide(&fs))
    }

    /// 同期フォルダ (OneDrive) の場所。既知のフォルダを OneDrive へ移した場合もここに入る。
    fn onedrive_roots() -> Vec<std::path::PathBuf> {
        ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
            .iter()
            .filter_map(std::env::var_os)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .collect()
    }

    pub fn check(p: &Path) -> Verdict {
        let s = p.to_string_lossy();
        if s.starts_with(r"\\") && !s.starts_with(r"\\?\") || s.starts_with(r"\\?\UNC\") {
            return Verdict::Unsafe(format!("{s} is a network (UNC) path"));
        }
        let lower = s.to_lowercase();
        for root in onedrive_roots() {
            let r = root.to_string_lossy().to_lowercase();
            if lower.starts_with(&r) {
                return Verdict::Unsafe(format!(
                    "{s} is inside the OneDrive folder {}",
                    root.display()
                ));
            }
        }
        // そのフォルダと上のフォルダの属性 (クラウドファイル・リパースポイント)。
        let mut cur = Some(p);
        while let Some(dir) = cur {
            let w = wide(dir);
            // SAFETY: `w` は NUL で終わる UTF-16。
            let a = unsafe { GetFileAttributesW(w.as_ptr()) };
            if a != INVALID_FILE_ATTRIBUTES {
                let cloud = FILE_ATTRIBUTE_RECALL_ON_OPEN
                    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                    | FILE_ATTRIBUTE_PINNED
                    | FILE_ATTRIBUTE_UNPINNED
                    | FILE_ATTRIBUTE_OFFLINE;
                if a & cloud != 0 {
                    return Verdict::Unsafe(format!(
                        "{} is a cloud (synced) folder",
                        dir.display()
                    ));
                }
                // ドライブの根 (C:\ など) のリパースポイントは数えない。
                if a & FILE_ATTRIBUTE_REPARSE_POINT != 0 && dir.parent().is_some() {
                    return Verdict::Unsafe(format!(
                        "{} is a reparse point (a link or a synced folder)",
                        dir.display()
                    ));
                }
            }
            cur = dir.parent();
        }
        let Some(root) = volume_root(p) else {
            return Verdict::Ok;
        };
        // SAFETY: `root` は NUL で終わる UTF-16。
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
            return Verdict::Unsafe(format!("{s} is on a network drive"));
        }
        match file_system(&root).as_deref() {
            Some(fs @ ("FAT" | "FAT32" | "exFAT")) => Verdict::Warn(format!(
                "{s} is on {fs}, which has no journaling and is weak against power loss"
            )),
            _ => Verdict::Ok,
        }
    }

    pub fn free_bytes(p: &Path) -> Option<u64> {
        let w = wide(p);
        let mut avail = 0u64;
        // SAFETY: `w` は NUL で終わる UTF-16。出力は u64 へのポインタか null。
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                w.as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(avail)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::Verdict;

    fn cpath(p: &Path) -> Option<CString> {
        CString::new(p.as_os_str().as_bytes()).ok()
    }

    #[cfg(target_os = "linux")]
    pub fn check(p: &Path) -> Verdict {
        const NFS: i64 = 0x6969;
        const SMB: i64 = 0x517B;
        const CIFS: i64 = 0xFF53_4D42;
        const SMB2: i64 = 0xFE53_4D42;
        const FUSE: i64 = 0x6573_5546;
        const MSDOS: i64 = 0x4d44;
        const EXFAT: i64 = 0x2011_BAB0;
        let Some(c) = cpath(p) else {
            return Verdict::Ok;
        };
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` は NUL で終わるパス、`st` は書き込める statfs。
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return Verdict::Ok;
        }
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
        let kind = st.f_type as i64 & 0xFFFF_FFFF;
        let s = p.display();
        match kind {
            NFS | SMB | CIFS | SMB2 => Verdict::Unsafe(format!("{s} is on a network file system")),
            FUSE => Verdict::Unsafe(format!("{s} is on a FUSE file system (sync tools use it)")),
            MSDOS | EXFAT => Verdict::Warn(format!(
                "{s} is on FAT/exFAT, which has no journaling and is weak against power loss"
            )),
            _ => Verdict::Ok,
        }
    }

    /// macOS の判定は、macOS を出荷するとき (v0.6) に入れる。
    #[cfg(not(target_os = "linux"))]
    pub fn check(_p: &Path) -> Verdict {
        Verdict::Ok
    }

    pub fn free_bytes(p: &Path) -> Option<u64> {
        let c = cpath(p)?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` は NUL で終わるパス、`st` は書き込める statvfs。
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_dir_is_not_unsafe_and_has_free_space() {
        let dir = crate::testutil::TempDir::new("location");
        assert!(
            !matches!(check(dir.path()), Verdict::Unsafe(_)),
            "{:?}",
            check(dir.path())
        );
        // まだ無いフォルダも、あるところまで遡って調べる。
        assert!(!matches!(
            check(&dir.path().join("a/b/c")),
            Verdict::Unsafe(_)
        ));
        assert!(free_bytes(dir.path()).is_some_and(|n| n > 0));
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_are_unsafe() {
        assert!(matches!(
            check(Path::new(r"\\server\share\evorag")),
            Verdict::Unsafe(_)
        ));
    }
}
