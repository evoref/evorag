//! Windows: 所有者だけが使えるファイル・フォルダにする (NFR-503)。
//!
//! 今のプロセスの利用者の SID に「すべての権限」を許す ACE を 1 つだけ持つ DACL を、継承を切って
//! (保護して) 付ける。フォルダの ACE は、中に作るファイルとフォルダへ継承させる。
//! C を含む依存を使わず、advapi32 を直接呼ぶ (ADR-0015)。

use std::ffi::c_void;
use std::io;
use std::path::Path;

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
const SE_DACL_PROTECTED: u16 = 0x1000;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

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
    fn GetSecurityDescriptorControl(sd: *mut c_void, control: *mut u16, revision: *mut u32) -> i32;
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

/// `path` に、今の利用者だけを許す保護した DACL を付ける。`dir` ならフォルダの中へ継承させる。
pub fn restrict(path: &[u16], dir: bool) -> io::Result<()> {
    let user = UserSid::current()?;
    let sid = user.sid();
    // SAFETY: sid は有効な SID を指す。
    let sid_len = unsafe { GetLengthSid(sid) };
    // ACL (8) + ACCESS_ALLOWED_ACE (12) − SidStart (4) + SID。
    let acl_len = 8 + 12 - 4 + sid_len;
    let mut acl = vec![0u32; (acl_len as usize).div_ceil(4)];
    let flags = if dir {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    // SAFETY: acl は acl_len バイト以上で 4 バイト境界。sid は有効。
    unsafe {
        if InitializeAcl(acl.as_mut_ptr().cast(), acl_len, ACL_REVISION) == 0
            || AddAccessAllowedAceEx(
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

/// 所有者だけが使えるか (DACL の ACE が、今の利用者を許す 1 つだけで、継承を切っているか、
/// 継承を切ったフォルダから継承しているか)。試験と doctor のため。
pub fn is_owner_only(path: &[u16], _p: &Path) -> io::Result<bool> {
    let user = UserSid::current()?;
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
                || info.ace_count != 1
            {
                return false;
            }
            let mut ace: *mut c_void = std::ptr::null_mut();
            if GetAce(dacl, 0, &mut ace) == 0 {
                return false;
            }
            // ACE_HEADER (型 1・flags 1・大きさ 2) + mask (4) + SidStart。
            let kind = *ace.cast::<u8>();
            let sid = ace.cast::<u8>().add(8).cast::<c_void>();
            if kind != ACCESS_ALLOWED_ACE_TYPE || EqualSid(sid, user.sid()) == 0 {
                return false;
            }
            let mut control = 0u16;
            let mut rev = 0u32;
            if GetSecurityDescriptorControl(sd, &mut control, &mut rev) == 0 {
                return false;
            }
            // 継承を切っているか、ACE が継承したものか (継承を切ったフォルダの中)。
            let inherited = *ace.cast::<u8>().add(1) & 0x10 != 0;
            control & SE_DACL_PROTECTED != 0 || inherited
        }
    })();
    // SAFETY: GetNamedSecurityInfoW が割り当てた記述子を 1 回だけ解放する。
    unsafe { LocalFree(sd) };
    Ok(result)
}
