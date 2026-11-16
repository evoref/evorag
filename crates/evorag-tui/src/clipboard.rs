//! クリップボードの文字を読む (07 §2.1)。端末が貼り付けを受け持たず、Ctrl+V をキーとして渡すとき
//! (conhost の raw mode) にだけ使う。C を含む依存を使わずに OS から得る (ADR-0015)。
//!
//! Windows 以外では読まない (端末が貼り付けを受け持つ)。

/// クリップボードの文字。無い・読めないときは `None`。CRLF と単独の CR は LF にそろえる。
pub fn text() -> Option<String> {
    imp::text().map(|s| normalize(&s)).filter(|s| !s.is_empty())
}

/// CRLF と単独の CR を LF にそろえる。
pub fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;

    /// `CF_UNICODETEXT` (winuser.h)。
    const CF_UNICODETEXT: u32 = 13;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(h: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h: *mut c_void) -> i32;
        fn GlobalSize(h: *mut c_void) -> usize;
    }

    /// 開いたクリップボードを、抜けるときに必ず閉じる。
    struct Open;

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: この値は OpenClipboard が成功したときだけ作る。
            unsafe { CloseClipboard() };
        }
    }

    pub fn text() -> Option<String> {
        // SAFETY: 窓を持たない (null) で開く。失敗したら何もしない。
        if unsafe { OpenClipboard(core::ptr::null_mut()) } == 0 {
            return None;
        }
        let _open = Open;
        // SAFETY: クリップボードを開いている間だけ呼ぶ。返る handle はクリップボードが持つ。
        let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if h.is_null() {
            return None;
        }
        // SAFETY: `h` は上で得た有効な global memory の handle。
        let p = unsafe { GlobalLock(h) } as *const u16;
        if p.is_null() {
            return None;
        }
        // SAFETY: 同じ handle。大きさはバイトで返る。
        let max = unsafe { GlobalSize(h) } / 2;
        // 終端の NUL までを読む (大きさを超えない)。
        let mut len = 0;
        // SAFETY: `p` から `max` 個の u16 は、lock の間は読める。
        while len < max && unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: 直前で数えた範囲だけを読む。
        let s = String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(p, len) });
        // SAFETY: 上で lock した handle。
        unsafe { GlobalUnlock(h) };
        Some(s)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn text() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lone_cr_become_lf() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    /// 手で確かめる: 先にクリップボードへ「一行目\r\nsecond」を入れてから `--ignored` で走らせる。
    #[test]
    #[ignore = "利用者のクリップボードを読む"]
    #[cfg(windows)]
    fn reads_the_clipboard() {
        assert_eq!(text().as_deref(), Some("一行目\nsecond"));
    }
}
