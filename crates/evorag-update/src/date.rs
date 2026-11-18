//! リリースの日付 (`YYYY-MM-DD`、UTC。12 §5.3 の `date` と `EVORAG_RELEASE_DATE`)。

use std::fmt;
use std::str::FromStr;

/// リリースの日付。大小は日付の前後。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseDate {
    year: u16,
    month: u8,
    day: u8,
}

const fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl ReleaseDate {
    /// 日付を作る。無い日付なら `None`。
    #[must_use]
    pub const fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        if year == 0 || month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            None
        } else {
            Some(Self { year, month, day })
        }
    }

    /// UNIX 時刻 (秒) の UTC の日付。`git log -1 --format=%ct` の値から求める (12 §5.3)。
    ///
    /// 9999-12-31 を超える時刻は 9999-12-31 にする。
    #[must_use]
    pub fn from_unix_time(seconds: u64) -> Self {
        // H. Hinnant の civil_from_days。
        let days = i64::try_from(seconds / 86_400)
            .unwrap_or(i64::MAX)
            .min(2_932_896);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        let narrow = |v: i64| u8::try_from(v).expect("month and day fit in u8");
        Self {
            year: u16::try_from(year).unwrap_or(9999).min(9999),
            month: narrow(month),
            day: narrow(day),
        }
    }
}

impl fmt::Display for ReleaseDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// 日付として読めない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a date of the form YYYY-MM-DD")]
pub struct ParseDateError {
    _private: (),
}

impl FromStr for ReleaseDate {
    type Err = ParseDateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = ParseDateError { _private: () };
        let b = s.as_bytes();
        let shape = b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
        if !shape {
            return Err(err);
        }
        let year = s[0..4].parse().map_err(|_| err.clone())?;
        let month = s[5..7].parse().map_err(|_| err.clone())?;
        let day = s[8..10].parse().map_err(|_| err.clone())?;
        Self::new(year, month, day).ok_or(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_read_strictly() {
        assert_eq!(
            "2026-11-18".parse(),
            Ok(ReleaseDate::new(2026, 11, 18).expect("valid"))
        );
        assert!("2024-02-29".parse::<ReleaseDate>().is_ok());
        for bad in [
            "2026-1-18",
            "2026/11/18",
            "2026-11-18 ",
            "2026-13-01",
            "2026-00-10",
            "2026-02-29",
            "2026-04-31",
            "0000-01-01",
            "２０２６-11-18",
            "+026-11-18",
        ] {
            assert!(bad.parse::<ReleaseDate>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unix_time_is_turned_into_the_utc_date() {
        let d = |s: u64| ReleaseDate::from_unix_time(s).to_string();
        assert_eq!(d(0), "1970-01-01");
        assert_eq!(d(86_399), "1970-01-01");
        assert_eq!(d(951_782_400), "2000-02-29");
        assert_eq!(d(1_700_000_000), "2023-11-14");
        assert_eq!(d(u64::MAX), "9999-12-31");
    }

    #[test]
    fn dates_order_by_time() {
        let d = |s: &str| s.parse::<ReleaseDate>().expect(s);
        assert!(d("2026-11-18") > d("2026-11-17"));
        assert!(d("2027-01-01") > d("2026-12-31"));
    }
}
