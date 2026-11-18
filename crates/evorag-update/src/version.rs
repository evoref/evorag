//! evorag の版 (`X.Y.Z`)。pre-release (`-`) と build (`+`) の付く版は扱わない (12 §7.2 の apply の 1)。

use std::fmt;
use std::str::FromStr;

/// evorag の版。semver の `X.Y.Z` だけで、大小は semver の順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// 版を作る。
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// タグ (`v<版>`) から版を読む。`v` の無いもの・余分な文字のあるものは断る。
    ///
    /// # Errors
    ///
    /// `tag` が `vX.Y.Z` にちょうど一致しないとき。
    pub fn from_tag(tag: &str) -> Result<Self, ParseVersionError> {
        tag.strip_prefix('v')
            .ok_or(ParseVersionError { _private: () })?
            .parse()
    }

    /// この版のタグ (`v<版>`)。
    #[must_use]
    pub fn tag(&self) -> String {
        format!("v{self}")
    }

    /// 1 つ目の数。
    #[must_use]
    pub const fn major(&self) -> u64 {
        self.major
    }

    /// 2 つ目の数。
    #[must_use]
    pub const fn minor(&self) -> u64 {
        self.minor
    }

    /// 3 つ目の数。
    #[must_use]
    pub const fn patch(&self) -> u64 {
        self.patch
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 版として読めない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a version of the form X.Y.Z")]
pub struct ParseVersionError {
    _private: (),
}

/// semver の数の識別子 (`0` か、`0` で始まらない数字の列)。
fn number(s: &str) -> Option<u64> {
    let ok =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if ok { s.parse().ok() } else { None }
}

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let (Some(a), Some(b), Some(c), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ParseVersionError { _private: () });
        };
        match (number(a), number(b), number(c)) {
            (Some(major), Some(minor), Some(patch)) => Ok(Self::new(major, minor, patch)),
            _ => Err(ParseVersionError { _private: () }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_versions_are_read() {
        assert_eq!("0.3.0".parse(), Ok(Version::new(0, 3, 0)));
        assert_eq!("10.20.30".parse(), Ok(Version::new(10, 20, 30)));
        for bad in [
            "",
            "0.3",
            "0.3.0.1",
            "v0.3.0",
            "0.3.0-rc.1",
            "0.3.0+build",
            "00.3.0",
            "0.03.0",
            " 0.3.0",
            "0.3.0 ",
            "0.3.x",
            "0..0",
            "99999999999999999999.0.0",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tags_are_v_and_the_version() {
        let v = Version::new(0, 3, 1);
        assert_eq!(Version::from_tag("v0.3.1"), Ok(v));
        assert_eq!(v.tag(), "v0.3.1");
        assert!(Version::from_tag("0.3.1").is_err());
        assert!(Version::from_tag("V0.3.1").is_err());
        assert!(Version::from_tag("v0.3.1-rc.1").is_err());
    }

    #[test]
    fn versions_compare_numerically() {
        let v = |s: &str| s.parse::<Version>().expect(s);
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.3.1") > v("0.3.0"));
        assert_eq!(v("0.3.0"), Version::new(0, 3, 0));
    }
}
