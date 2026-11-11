//! 地方時 (07 §2.1、05 §4.1)。C を含む依存なしで、OS から UTC とのずれを得る (ADR-0015)。
//!
//! - Windows: `SystemTimeToTzSpecificLocalTime` (時刻ごとに夏時間の規則を当てる)。
//! - Linux と macOS: `TZ` か `/etc/localtime` の TZif (遷移の表と、末尾の POSIX TZ の規則)。
//! - 得られなければ `None` (UTC で示し、`UTC` と添える)。

// Windows では TZif と POSIX TZ の読み取りを試験でだけ使う。
#![cfg_attr(windows, allow(dead_code))]

/// UTC のミリ秒から、(年, 月, 日, 時, 分)。
pub fn civil(ms: i64) -> (i64, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, sod / 3600, sod / 60 % 60)
}

/// Howard Hinnant の civil_from_days。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// 年月日から 1970-01-01 からの日数。
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// その時刻の、UTC からのずれ (秒)。分からなければ `None`。
pub fn offset(ms: i64) -> Option<i32> {
    imp::offset(ms)
}

/// `HH:MM` (地方時)。ずれが分からなければ `HH:MM UTC`。
pub fn hhmm(ms: i64) -> String {
    match offset(ms) {
        Some(off) => {
            let (_, _, _, h, m) = civil(ms + i64::from(off) * 1000);
            format!("{h:02}:{m:02}")
        }
        None => {
            let (_, _, _, h, m) = civil(ms);
            format!("{h:02}:{m:02} UTC")
        }
    }
}

/// 発話の先頭に付ける日時 (05 §4.1): `[2026-11-12 (木) 10:02 +09:00]`。`ja` でなければ曜日を英語の
/// 略号にする。ずれが分からなければ `UTC` と書く。
pub fn stamp(ms: i64, ja: bool) -> String {
    const JA: [&str; 7] = ["日", "月", "火", "水", "木", "金", "土"];
    const EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let off = offset(ms);
    let local = ms + i64::from(off.unwrap_or(0)) * 1000;
    let (y, mo, d, h, mi) = civil(local);
    // 1970-01-01 は木曜日。
    let wd = (local.div_euclid(86_400_000) + 4).rem_euclid(7) as usize;
    let w = if ja { JA[wd] } else { EN[wd] };
    let zone = match off {
        Some(o) => {
            let sign = if o < 0 { '-' } else { '+' };
            let a = o.abs();
            format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60)
        }
        None => "UTC".to_owned(),
    };
    format!("[{y:04}-{mo:02}-{d:02} ({w}) {h:02}:{mi:02} {zone}]")
}

/// `YYYY-MM-DD HH:MM (UTC+09:00)` (引用の出典の日時)。分からなければ `… UTC`。
pub fn datetime(ms: i64) -> String {
    match offset(ms) {
        Some(off) => {
            let (y, mo, d, h, mi) = civil(ms + i64::from(off) * 1000);
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.abs();
            format!(
                "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} (UTC{sign}{:02}:{:02})",
                a / 3600,
                a / 60 % 60
            )
        }
        None => {
            let (y, mo, d, h, mi) = civil(ms);
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
        }
    }
}

// ---- POSIX TZ の規則 (例: `JST-9`、`EST5EDT,M3.2.0,M11.1.0`) ----

/// 夏時間の切り替えの日 (`Mm.w.d`、`Jn`、`n`) と時刻 (秒)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleDay {
    /// 月・週 (1〜5、5 は最後)・曜日 (0 は日曜)。
    Month(i64, i64, i64),
    /// 1〜365 の通日 (2 月 29 日を数えない)。
    Julian1(i64),
    /// 0〜365 の通日 (2 月 29 日を数える)。
    Julian0(i64),
}

/// 切り替えの日と、その日の時刻 (秒)。
type Switch = (RuleDay, i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PosixTz {
    /// 標準時の UTC からのずれ (秒、東が正)。
    std: i32,
    /// 夏時間 (ずれ、開始、終了)。
    dst: Option<(i32, Switch, Switch)>,
}

fn take_name(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('<') {
        let end = rest.find('>')?;
        Some(&rest[end + 1..])
    } else {
        let n = s.bytes().take_while(u8::is_ascii_alphabetic).count();
        (n >= 3).then(|| &s[n..])
    }
}

/// `[+-]hh[:mm[:ss]]` を秒に。残りも返す。
fn take_hms(s: &str) -> Option<(i64, &str)> {
    let (sign, s) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(s.len());
    let mut parts = s[..end].split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let sec: i64 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((sign * (h * 3600 + m * 60 + sec), &s[end..]))
}

fn take_rule(s: &str) -> Option<(Switch, &str)> {
    let (day, rest) = if let Some(r) = s.strip_prefix('M') {
        let end = r.find(['/', ',']).unwrap_or(r.len());
        let mut p = r[..end].split('.');
        let m = p.next()?.parse().ok()?;
        let w = p.next()?.parse().ok()?;
        let d = p.next()?.parse().ok()?;
        (RuleDay::Month(m, w, d), &r[end..])
    } else if let Some(r) = s.strip_prefix('J') {
        let end = r.find(['/', ',']).unwrap_or(r.len());
        (RuleDay::Julian1(r[..end].parse().ok()?), &r[end..])
    } else {
        let end = s.find(['/', ',']).unwrap_or(s.len());
        (RuleDay::Julian0(s[..end].parse().ok()?), &s[end..])
    };
    match rest.strip_prefix('/') {
        Some(r) => {
            let (t, r) = take_hms(r)?;
            Some(((day, t), r))
        }
        None => Some(((day, 7200), rest)),
    }
}

impl PosixTz {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let rest = take_name(s)?;
        let (std_west, rest) = take_hms(rest)?;
        let std = i32::try_from(-std_west).ok()?;
        if rest.is_empty() {
            return Some(Self { std, dst: None });
        }
        let rest = take_name(rest)?;
        let (dst_off, rest) = match rest.as_bytes().first() {
            Some(b',') | None => (std + 3600, rest),
            _ => {
                let (w, r) = take_hms(rest)?;
                (i32::try_from(-w).ok()?, r)
            }
        };
        let rest = rest.strip_prefix(',')?;
        let (start, rest) = take_rule(rest)?;
        let rest = rest.strip_prefix(',')?;
        let (end, _) = take_rule(rest)?;
        Some(Self {
            std,
            dst: Some((dst_off, start, end)),
        })
    }

    /// その年の規則の日の、地方時の 0 時からの日数 (1970-01-01 から)。
    fn day_of(year: i64, rule: RuleDay) -> i64 {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        match rule {
            RuleDay::Julian1(n) => {
                let n = if leap && n >= 60 { n + 1 } else { n };
                days_from_civil(year, 1, 1) + n - 1
            }
            RuleDay::Julian0(n) => days_from_civil(year, 1, 1) + n,
            RuleDay::Month(m, w, d) => {
                let first = days_from_civil(year, m, 1);
                // 1970-01-01 は木曜 (4)。
                let wd_first = (first + 4).rem_euclid(7);
                let mut day = first + (d - wd_first).rem_euclid(7) + (w - 1) * 7;
                let next_month = if m == 12 {
                    days_from_civil(year + 1, 1, 1)
                } else {
                    days_from_civil(year, m + 1, 1)
                };
                while day >= next_month {
                    day -= 7;
                }
                day
            }
        }
    }

    pub(crate) fn offset(&self, ms: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else {
            return self.std;
        };
        let utc = ms.div_euclid(1000);
        let (year, ..) = civil((utc + i64::from(self.std)) * 1000);
        // 切り替えの時刻は、開始は標準時、終了は夏時間の地方時で書かれている。
        let at =
            |rule: Switch, off: i32| Self::day_of(year, rule.0) * 86_400 + rule.1 - i64::from(off);
        let (s, e) = (at(start, self.std), at(end, dst));
        let in_dst = if s < e {
            utc >= s && utc < e
        } else {
            // 南半球 (年をまたぐ夏時間)。
            utc >= s || utc < e
        };
        if in_dst { dst } else { self.std }
    }
}

// ---- TZif (RFC 8536) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Zone {
    /// 遷移の時刻 (UTC の秒) と、その後のずれ。
    transitions: Vec<(i64, i32)>,
    /// 最初の遷移より前のずれ。
    initial: i32,
    /// 最後の遷移より後の規則。
    footer: Option<PosixTz>,
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl Zone {
    pub(crate) fn parse(b: &[u8]) -> Option<Self> {
        if b.get(..4)? != b"TZif" {
            return None;
        }
        let version = *b.get(4)?;
        let counts = |at: usize| -> Option<[usize; 6]> {
            let mut c = [0usize; 6];
            for (i, v) in c.iter_mut().enumerate() {
                *v = be_u32(b, at + 20 + i * 4)? as usize;
            }
            Some(c)
        };
        let [isut, isstd, leap, timecnt, typecnt, charcnt] = counts(0)?;
        let mut at = 44;
        let mut tsize = 4;
        let mut c = [isut, isstd, leap, timecnt, typecnt, charcnt];
        if version >= b'2' {
            // v1 の部分を飛ばし、64 ビットの部分を読む。
            at += timecnt * 4 + timecnt + typecnt * 6 + charcnt + leap * 8 + isstd + isut;
            c = counts(at)?;
            at += 44;
            tsize = 8;
        }
        let [isut, isstd, leap, timecnt, typecnt, charcnt] = c;
        let mut times = Vec::with_capacity(timecnt);
        for i in 0..timecnt {
            let p = at + i * tsize;
            let t = if tsize == 8 {
                i64::from_be_bytes(b.get(p..p + 8)?.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(b.get(p..p + 4)?.try_into().ok()?))
            };
            times.push(t);
        }
        at += timecnt * tsize;
        let idx: Vec<usize> = b
            .get(at..at + timecnt)?
            .iter()
            .map(|&i| i as usize)
            .collect();
        at += timecnt;
        let mut types = Vec::with_capacity(typecnt);
        for i in 0..typecnt {
            let p = at + i * 6;
            let off = i32::from_be_bytes(b.get(p..p + 4)?.try_into().ok()?);
            let is_dst = *b.get(p + 4)? != 0;
            types.push((off, is_dst));
        }
        at += typecnt * 6 + charcnt + leap * (tsize + 4) + isstd + isut;
        let footer = if version >= b'2' {
            let rest = b.get(at..).unwrap_or_default();
            let s = String::from_utf8_lossy(rest);
            let s = s.trim_matches('\n');
            (!s.is_empty()).then(|| PosixTz::parse(s)).flatten()
        } else {
            None
        };
        let initial = types
            .iter()
            .find(|(_, dst)| !dst)
            .or(types.first())
            .map_or(0, |t| t.0);
        let transitions = times
            .into_iter()
            .zip(idx)
            .filter_map(|(t, i)| types.get(i).map(|ty| (t, ty.0)))
            .collect();
        Some(Self {
            transitions,
            initial,
            footer,
        })
    }

    pub(crate) fn offset(&self, ms: i64) -> i32 {
        let t = ms.div_euclid(1000);
        match self.transitions.iter().rposition(|(at, _)| *at <= t) {
            Some(i) if i + 1 < self.transitions.len() || self.footer.is_none() => {
                self.transitions[i].1
            }
            // 最後の遷移より後は、末尾の規則で決める。
            _ if t >= self.transitions.last().map_or(i64::MIN, |l| l.0) => {
                self.footer.as_ref().map_or_else(
                    || self.transitions.last().map_or(self.initial, |l| l.1),
                    |f| f.offset(ms),
                )
            }
            _ => self.initial,
        }
    }
}

#[cfg(windows)]
mod imp {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SystemTimeToTzSpecificLocalTime(
            tz: *const core::ffi::c_void,
            universal: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }

    pub fn offset(ms: i64) -> Option<i32> {
        let (y, m, d, h, mi) = super::civil(ms);
        if !(1601..=30827).contains(&y) {
            return None;
        }
        let secs = ms.div_euclid(1000).rem_euclid(60);
        let utc = SystemTime {
            year: y as u16,
            month: m as u16,
            day_of_week: 0,
            day: d as u16,
            hour: h as u16,
            minute: mi as u16,
            second: secs as u16,
            milliseconds: 0,
        };
        let mut local = SystemTime::default();
        // SAFETY: tz は null (今の時間帯)、ほかは有効な SYSTEMTIME へのポインタ。
        let ok = unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) };
        if ok == 0 {
            return None;
        }
        let to_secs = |t: &SystemTime| {
            super::days_from_civil(i64::from(t.year), i64::from(t.month), i64::from(t.day)) * 86_400
                + i64::from(t.hour) * 3600
                + i64::from(t.minute) * 60
                + i64::from(t.second)
        };
        i32::try_from(to_secs(&local) - to_secs(&utc)).ok()
    }
}

#[cfg(unix)]
mod imp {
    use std::sync::OnceLock;

    use super::{PosixTz, Zone};

    enum Source {
        Zone(Zone),
        Rule(PosixTz),
    }

    fn load() -> Option<Source> {
        let read = |p: &str| std::fs::read(p).ok().and_then(|b| Zone::parse(&b));
        match std::env::var("TZ").ok().filter(|v| !v.is_empty()) {
            Some(tz) => {
                let name = tz.strip_prefix(':').unwrap_or(&tz);
                if name.starts_with('/') {
                    read(name).map(Source::Zone)
                } else if let Some(z) = read(&format!("/usr/share/zoneinfo/{name}")) {
                    Some(Source::Zone(z))
                } else {
                    PosixTz::parse(name).map(Source::Rule)
                }
            }
            None => read("/etc/localtime").map(Source::Zone),
        }
    }

    pub fn offset(ms: i64) -> Option<i32> {
        static SOURCE: OnceLock<Option<Source>> = OnceLock::new();
        match SOURCE.get_or_init(load).as_ref()? {
            Source::Zone(z) => Some(z.offset(ms)),
            Source::Rule(r) => Some(r.offset(ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i64, m: i64, d: i64, h: i64) -> i64 {
        (days_from_civil(y, m, d) * 86_400 + h * 3600) * 1000
    }

    #[test]
    fn civil_round_trips() {
        for days in [-1000, 0, 19_000, 20_401, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn posix_rules_without_and_with_daylight_saving() {
        assert_eq!(
            PosixTz::parse("JST-9").unwrap().offset(ms(2026, 7, 1, 0)),
            9 * 3600
        );
        let ny = PosixTz::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(ny.offset(ms(2026, 1, 15, 12)), -5 * 3600);
        assert_eq!(ny.offset(ms(2026, 7, 1, 12)), -4 * 3600);
        // 2026-03-08 02:00 EST (= 07:00 UTC) に夏時間が始まる。
        assert_eq!(ny.offset(ms(2026, 3, 8, 6)), -5 * 3600);
        assert_eq!(ny.offset(ms(2026, 3, 8, 7)), -4 * 3600);
        let syd = PosixTz::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(syd.offset(ms(2026, 1, 15, 0)), 11 * 3600);
        assert_eq!(syd.offset(ms(2026, 7, 1, 0)), 10 * 3600);
        assert_eq!(
            PosixTz::parse("<+0530>-5:30").unwrap().offset(0),
            5 * 3600 + 1800
        );
        assert!(PosixTz::parse("nonsense").is_none());
    }

    /// 遷移の無い v2 の TZif と末尾の規則 (`JST-9`)。
    #[test]
    fn tzif_with_a_footer() {
        let mut b = Vec::new();
        let header = |b: &mut Vec<u8>, typecnt: u32, charcnt: u32| {
            b.extend_from_slice(b"TZif2");
            b.extend_from_slice(&[0; 15]);
            for c in [0u32, 0, 0, 0, typecnt, charcnt] {
                b.extend_from_slice(&c.to_be_bytes());
            }
        };
        header(&mut b, 1, 4);
        b.extend_from_slice(&32_400i32.to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(b"JST\0");
        header(&mut b, 1, 4);
        b.extend_from_slice(&32_400i32.to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(b"JST\0");
        b.extend_from_slice(b"\nJST-9\n");
        let z = Zone::parse(&b).unwrap();
        assert_eq!(z.offset(ms(2026, 11, 9, 0)), 9 * 3600);
        assert!(Zone::parse(b"not a tzif").is_none());
    }

    #[test]
    fn formats_have_a_utc_marker_only_without_an_offset() {
        let t = ms(2026, 11, 9, 12);
        let h = hhmm(t);
        match offset(t) {
            Some(_) => assert!(!h.ends_with("UTC") && datetime(t).contains("(UTC")),
            None => assert!(h.ends_with("UTC")),
        }
    }
}
