//! 版の比較 (03 §3.1.1)。形式の版と evorag の版は、`.` で区切った整数の並びとして比べる。

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// `.` で区切った整数の並び (例: 形式の版 `0.1`、evorag の版 `0.0.1`)。
///
/// 足りない桁は 0 とみなして比べる (`0.1` と `0.1.0` は等しい)。evorag の版の `-` 以降
/// (pre-release) と `+` 以降 (build) は比べない。
#[derive(Debug, Clone, Eq)]
pub struct Version(Vec<u64>);

impl Version {
    pub fn new(parts: &[u64]) -> Self {
        Self(parts.to_vec())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid version `{0}`")]
pub struct ParseVersionError(pub String);

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let core = s.split(['-', '+']).next().unwrap_or_default();
        let parts: Result<Vec<u64>, _> = core.split('.').map(str::parse).collect();
        match parts {
            Ok(p) if !p.is_empty() => Ok(Self(p)),
            _ => Err(ParseVersionError(s.to_owned())),
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        (0..n)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{p}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn compares_numerically_not_as_strings() {
        assert!(v("0.10") > v("0.9"));
        assert!(v("1") > v("0.99"));
        assert_eq!(v("0.1"), v("0.1.0"));
        assert!(v("0.0.2") > v("0.0.1"));
        assert_eq!(v("1.2.3-beta.1+abc"), v("1.2.3"));
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "a", "1..2", "1.x", "-1"] {
            assert!(s.parse::<Version>().is_err(), "{s}");
        }
    }

    #[test]
    fn displays_its_parts() {
        assert_eq!(v("0.1").to_string(), "0.1");
    }
}
