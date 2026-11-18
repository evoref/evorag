//! Windows: ほかの一般の利用者が使えないファイル・フォルダにする (NFR-503、03 §2)。
//!
//! 今のプロセスの利用者・SYSTEM・Administrators の 3 つに「すべての権限」を許す ACE だけを持つ
//! DACL を、継承を切って (保護して) 付ける。フォルダの ACE は、中に作るファイルとフォルダへ継承させる。
//! (管理者は所有権を取れば読めるので外さない。SYSTEM で動く退避のソフトと、管理者による復旧のため。)
//! C を含む依存を使わず、advapi32 を直接呼ぶ (ADR-0015)。

use std::ffi::c_void;
use std::io;
use std::sync::OnceLock;

type Handle = *mut c_void;
type Psid = *mut c_void;

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER: u32 = 1;
const ACL_REVISION: u32 = 2;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
const SE_FILE_OBJECT: u32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
const ACL_SIZE_INFORMATION: u32 = 2;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const INHERIT_ONLY_ACE: u8 = 0x08;
/// `WELL_KNOWN_SID_TYPE` の WinLocalSystemSid と WinBuiltinAdministratorsSid。
const WIN_LOCAL_SYSTEM_SID: u32 = 22;
const WIN_BUILTIN_ADMINISTRATORS_SID: u32 = 26;
/// 書き込みの類の権限 (12 §7.3 の「インストール先の確かめ」): FILE_WRITE_DATA (ADD_FILE)・
/// FILE_APPEND_DATA (ADD_SUBDIRECTORY)・FILE_DELETE_CHILD・DELETE・WRITE_DAC・WRITE_OWNER・
/// GENERIC_WRITE・GENERIC_ALL。
const WRITE_RIGHTS: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0040
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x4000_0000
    | 0x1000_0000;
/// 「許す」の ACE の型のうち、SID が mask の直後にあるもの (ACCESS_ALLOWED と ACCESS_ALLOWED_CALLBACK)。
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
/// 「許す」の ACE の型のうち、オブジェクトの ACE (SID の位置が変わる。ACCESS_ALLOWED_OBJECT と
/// ACCESS_ALLOWED_CALLBACK_OBJECT)。
const ACCESS_ALLOWED_OBJECT_ACE_TYPES: [u8; 2] = [5, 11];
/// TrustedInstaller (`NT SERVICE\TrustedInstaller`) の SID:
/// S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464。
const TRUSTED_INSTALLER_SUBAUTHORITIES: [u32; 6] = [
    80,
    956_008_885,
    3_418_522_649,
    1_831_038_044,
    1_853_292_631,
    2_271_478_464,
];
/// SECURITY_MAX_SID_SIZE。
const MAX_SID: usize = 68;
const FILE_PERSISTENT_ACLS: u32 = 0x8;
const DRIVE_REMOTE: u32 = 4;

#[repr(C)]
struct SidAndAttributes {
    sid: Psid,
    attributes: u32,
}

#[repr(C)]
#[derive(Default)]
struct AclSizeInformation {
    ace_count: u32,
    bytes_in_use: u32,
    bytes_free: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn LocalFree(p: *mut c_void) -> *mut c_void;
    fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
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
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        info: *mut c_void,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn GetLengthSid(sid: Psid) -> u32;
    fn CreateWellKnownSid(kind: u32, domain: Psid, sid: Psid, size: *mut u32) -> i32;
    fn EqualSid(a: Psid, b: Psid) -> i32;
    fn InitializeAcl(acl: *mut c_void, len: u32, revision: u32) -> i32;
    fn AddAccessAllowedAceEx(
        acl: *mut c_void,
        revision: u32,
        flags: u32,
        mask: u32,
        sid: Psid,
    ) -> i32;
    fn SetNamedSecurityInfoW(
        name: *const u16,
        kind: u32,
        info: u32,
        owner: Psid,
        group: Psid,
        dacl: *const c_void,
        sacl: *const c_void,
    ) -> u32;
    fn GetNamedSecurityInfoW(
        name: *const u16,
        kind: u32,
        info: u32,
        owner: *mut Psid,
        group: *mut Psid,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        sd: *mut *mut c_void,
    ) -> u32;
    fn GetAclInformation(acl: *mut c_void, info: *mut c_void, len: u32, class: u32) -> i32;
    fn GetAce(acl: *mut c_void, index: u32, ace: *mut *mut c_void) -> i32;
}

/// 今のプロセスの利用者の SID (`TOKEN_USER` の入れ物ごと持つ。SID はその中を指す)。
struct UserSid {
    buf: Vec<u64>,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: 今のプロセスの擬似ハンドルと、書き込み先の有効なポインタを渡す。
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut need = 0u32;
        // SAFETY: 大きさを問い合わせるだけ (失敗し、need に要る大きさが入る)。
        unsafe { GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut need) };
        let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
        // SAFETY: buf は need バイト以上あり、8 バイト境界に揃っている。
        let ok = unsafe {
            GetTokenInformation(token, TOKEN_USER, buf.as_mut_ptr().cast(), need, &mut need)
        };
        let err = io::Error::last_os_error();
        // SAFETY: OpenProcessToken が返したハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(err);
        }
        Ok(Self { buf })
    }

    fn sid(&self) -> Psid {
        // SAFETY: buf の先頭は GetTokenInformation が書いた TOKEN_USER。
        unsafe { (*self.buf.as_ptr().cast::<SidAndAttributes>()).sid }
    }
}

/// 今の利用者の SID (プロセスで 1 回だけ問い合わせる)。
fn user() -> io::Result<&'static UserSid> {
    static USER: OnceLock<Result<UserSid, i32>> = OnceLock::new();
    match USER.get_or_init(|| UserSid::current().map_err(|e| e.raw_os_error().unwrap_or(0))) {
        Ok(u) => Ok(u),
        Err(code) => Err(io::Error::from_raw_os_error(*code)),
    }
}

// SAFETY: SID の入れ物は作った後に書き換えない。読むだけなのでスレッドをまたいでよい。
unsafe impl Sync for UserSid {}
unsafe impl Send for UserSid {}

/// よく知られた SID (SYSTEM・Administrators)。
struct WellKnown {
    buf: [u32; MAX_SID / 4],
}

impl WellKnown {
    fn new(kind: u32) -> io::Result<Self> {
        let mut w = Self {
            buf: [0; MAX_SID / 4],
        };
        let mut size = MAX_SID as u32;
        // SAFETY: buf は SECURITY_MAX_SID_SIZE バイトで 4 バイト境界。domain は要らない種類。
        if unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                w.buf.as_mut_ptr().cast(),
                &mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(w)
    }

    fn sid(&self) -> Psid {
        self.buf.as_ptr().cast_mut().cast()
    }
}

/// 許す 3 つの SID (今の利用者・SYSTEM・Administrators)。
struct Allowed {
    system: WellKnown,
    admins: WellKnown,
}

// SAFETY: 作った後に書き換えない。読むだけ。
unsafe impl Sync for Allowed {}
unsafe impl Send for Allowed {}

fn allowed() -> io::Result<(&'static UserSid, &'static Allowed)> {
    static ALLOWED: OnceLock<Result<Allowed, i32>> = OnceLock::new();
    let a = ALLOWED.get_or_init(|| {
        let code = |e: io::Error| e.raw_os_error().unwrap_or(0);
        Ok(Allowed {
            system: WellKnown::new(WIN_LOCAL_SYSTEM_SID).map_err(code)?,
            admins: WellKnown::new(WIN_BUILTIN_ADMINISTRATORS_SID).map_err(code)?,
        })
    });
    match a {
        Ok(a) => Ok((user()?, a)),
        Err(code) => Err(io::Error::from_raw_os_error(*code)),
    }
}

/// ACL を付けられる置き場所か。ネットワークの置き場所 (相手の側では SID が別のアカウントに
/// なり、作った本人が開けなくなるおそれ) と、ACL を持たないファイルシステム (FAT・exFAT。付けても
/// 何も起きない) では付けない。
pub fn applicable(path: &[u16]) -> bool {
    let mut root = vec![0u16; path.len().max(4) + 1];
    // SAFETY: path は NUL で終わる UTF-16。root は path より長い。
    if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return false;
    }
    let end = root.iter().position(|&c| c == 0).unwrap_or(0);
    let s = String::from_utf16_lossy(&root[..end]);
    if s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\")) {
        return false;
    }
    let mut flags = 0u32;
    // SAFETY: root は NUL で終わるボリュームの根。出力は flags だけを受け取る。
    unsafe {
        if GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE {
            return false;
        }
        if GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        ) == 0
        {
            return false;
        }
    }
    flags & FILE_PERSISTENT_ACLS != 0
}

/// `path` に、今の利用者だけを許す保護した DACL を付ける。`dir` ならフォルダの中へ継承させる。
/// 付けられない置き場所 ([`applicable`]) では何もしない。
pub fn restrict(path: &[u16], dir: bool) -> io::Result<()> {
    if !applicable(path) {
        return Ok(());
    }
    let (user, others) = allowed()?;
    let sids = [user.sid(), others.system.sid(), others.admins.sid()];
    // ACL (8) + 各 ACE (ACCESS_ALLOWED_ACE 12 − SidStart 4 + SID)。
    // SAFETY: どれも有効な SID を指す。
    let acl_len: u32 = 8 + sids
        .iter()
        .map(|s| 8 + unsafe { GetLengthSid(*s) })
        .sum::<u32>();
    let mut acl = vec![0u32; (acl_len as usize).div_ceil(4)];
    let flags = if dir {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    // SAFETY: acl は acl_len バイト以上で 4 バイト境界。SID は有効。
    unsafe {
        if InitializeAcl(acl.as_mut_ptr().cast(), acl_len, ACL_REVISION) == 0 {
            return Err(io::Error::last_os_error());
        }
        for sid in sids {
            if AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                flags,
                FILE_ALL_ACCESS,
                sid,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
    }
    // SAFETY: path は NUL で終わる UTF-16。acl は上で作った有効な ACL。
    let r = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl.as_ptr().cast(),
            std::ptr::null(),
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    Ok(())
}

/// TrustedInstaller の SID (SID の構造: 版 1・下位の数 6・権威 5・下位の値)。
fn trusted_installer() -> [u32; 8] {
    let [a, b, c, d, e, f] = TRUSTED_INSTALLER_SUBAUTHORITIES;
    [
        u32::from_ne_bytes([1, 6, 0, 0]),
        u32::from_ne_bytes([0, 0, 0, 5]),
        a,
        b,
        c,
        d,
        e,
        f,
    ]
}

/// CREATOR OWNER (S-1-3-0) と CREATOR GROUP (S-1-3-1) の SID (継承した先では作った者に置き換わる)。
fn creator(sub_authority: u32) -> [u32; 3] {
    [
        u32::from_ne_bytes([1, 1, 0, 0]),
        u32::from_ne_bytes([0, 0, 0, 3]),
        sub_authority,
    ]
}

/// `path` の DACL が、今の利用者・SYSTEM・Administrators・TrustedInstaller のほかの相手に、書き込みの類の
/// 権限 ([`WRITE_RIGHTS`]) を許すか。DACL が無ければ (誰でも使える) 真。「拒む」の ACE は見ない (許す相手を
/// 狭めるだけ)。
///
/// 継承だけの ACE (このものには効かない) は、ファイルへ継承するもの (`OBJECT_INHERIT_ACE`) だけを見る。
/// 中に作るファイル (`evorag.new` など) と、改名で継承し直すファイルに効くため。相手が CREATOR OWNER・
/// CREATOR GROUP なら作った者に置き換わるので許す。フォルダへだけ継承するものは見ない。
pub fn grants_write_to_others(path: &[u16]) -> io::Result<bool> {
    let (user, others) = allowed()?;
    let installer = trusted_installer();
    let installer_sid: Psid = installer.as_ptr().cast_mut().cast();
    let (creator_owner, creator_group) = (creator(0), creator(1));
    let creator_sids: [Psid; 2] = [
        creator_owner.as_ptr().cast_mut().cast(),
        creator_group.as_ptr().cast_mut().cast(),
    ];
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: path は NUL で終わる UTF-16。出力のポインタはすべて有効。
    let r = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let result = (|| {
        if dacl.is_null() {
            return Ok(true); // DACL が無い = 誰でも使える。
        }
        let mut info = AclSizeInformation::default();
        // SAFETY: dacl と sd は GetNamedSecurityInfoW が返したもので、LocalFree の前まで有効。ACE は
        // GetAce が返した、ACL の中を指すポインタ。
        unsafe {
            if GetAclInformation(
                dacl,
                (&raw mut info).cast(),
                std::mem::size_of::<AclSizeInformation>() as u32,
                ACL_SIZE_INFORMATION,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            for index in 0..info.ace_count {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(dacl, index, &mut ace) == 0 {
                    return Err(io::Error::last_os_error());
                }
                // ACE_HEADER (型 1・flags 1・大きさ 2) + mask (4) + SidStart。
                let kind = *ace.cast::<u8>();
                let ace_flags = *ace.cast::<u8>().add(1);
                let inherit_only = ace_flags & INHERIT_ONLY_ACE != 0;
                if inherit_only && u32::from(ace_flags) & OBJECT_INHERIT_ACE == 0 {
                    continue; // フォルダへだけ継承する。このものにも中のファイルにも効かない。
                }
                let mask = ace.cast::<u8>().add(4).cast::<u32>().read_unaligned();
                if mask & WRITE_RIGHTS == 0 {
                    continue;
                }
                if ACCESS_ALLOWED_OBJECT_ACE_TYPES.contains(&kind) {
                    return Ok(true); // 相手を読み分けない (ファイルには使われない形)。安全な側に倒す。
                }
                if kind != ACCESS_ALLOWED_ACE_TYPE && kind != ACCESS_ALLOWED_CALLBACK_ACE_TYPE {
                    continue; // 拒む・監査の ACE。
                }
                let sid = ace.cast::<u8>().add(8).cast::<c_void>();
                let known = [
                    user.sid(),
                    others.system.sid(),
                    others.admins.sid(),
                    installer_sid,
                ];
                let is_known = known.iter().any(|&k| EqualSid(sid, k) != 0)
                    || (inherit_only && creator_sids.iter().any(|&k| EqualSid(sid, k) != 0));
                if !is_known {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    })();
    // SAFETY: GetNamedSecurityInfoW が割り当てた記述子を 1 回だけ解放する。
    unsafe { LocalFree(sd) };
    result
}

/// ほかの一般の利用者が使えないか: 効く ACE (継承だけのものを除く) がすべて「許す」で、相手が
/// 今の利用者・SYSTEM・Administrators のどれかで、今の利用者にはすべての権限を許すものがある。
/// 試験のため。
pub fn is_owner_only(path: &[u16]) -> io::Result<bool> {
    let (user, others) = allowed()?;
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: path は NUL で終わる UTF-16。出力のポインタはすべて有効。
    let r = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let result = (|| {
        if dacl.is_null() {
            return false; // DACL が無い = 誰でも使える。
        }
        let mut info = AclSizeInformation::default();
        // SAFETY: dacl と sd は GetNamedSecurityInfoW が返したもので、LocalFree の前まで有効。
        unsafe {
            if GetAclInformation(
                dacl,
                (&raw mut info).cast(),
                std::mem::size_of::<AclSizeInformation>() as u32,
                ACL_SIZE_INFORMATION,
            ) == 0
            {
                return false;
            }
            let mut user_full = false;
            for index in 0..info.ace_count {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(dacl, index, &mut ace) == 0 {
                    return false;
                }
                // ACE_HEADER (型 1・flags 1・大きさ 2) + mask (4) + SidStart。
                let kind = *ace.cast::<u8>();
                let ace_flags = *ace.cast::<u8>().add(1);
                if ace_flags & INHERIT_ONLY_ACE != 0 {
                    continue; // このものには効かない。
                }
                if kind != ACCESS_ALLOWED_ACE_TYPE {
                    return false;
                }
                let mask = ace.cast::<u8>().add(4).cast::<u32>().read_unaligned();
                let sid = ace.cast::<u8>().add(8).cast::<c_void>();
                if EqualSid(sid, user.sid()) != 0 {
                    user_full |= mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS;
                } else if EqualSid(sid, others.system.sid()) == 0
                    && EqualSid(sid, others.admins.sid()) == 0
                {
                    return false; // ほかの利用者・グループを許している。
                }
            }
            user_full
        }
    })();
    // SAFETY: GetNamedSecurityInfoW が割り当てた記述子を 1 回だけ解放する。
    unsafe { LocalFree(sd) };
    Ok(result)
}
