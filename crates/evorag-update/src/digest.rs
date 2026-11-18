//! SHA-256 の値 (`release.json` では小文字の 16 進 64 桁、12 §5.3)。

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

/// SHA-256 の値。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// バイト列の SHA-256。
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// 32 バイトの値。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

/// 小文字の 16 進 64 桁でない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a SHA-256 of 64 lowercase hex digits")]
pub struct ParseDigestError {
    _private: (),
}

impl FromStr for Sha256Digest {
    type Err = ParseDigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        let b = s.as_bytes();
        if b.len() != 64 {
            return Err(ParseDigestError { _private: () });
        }
        let mut out = [0u8; 32];
        let (pairs, _) = b.as_chunks::<2>();
        for (o, pair) in out.iter_mut().zip(pairs) {
            match (hex(pair[0]), hex(pair[1])) {
                (Some(hi), Some(lo)) => *o = hi << 4 | lo,
                _ => return Err(ParseDigestError { _private: () }),
            }
        }
        Ok(Self(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_round_trip_as_lowercase_hex() {
        let d = Sha256Digest::of(b"abc");
        let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(d.to_string(), hex);
        assert_eq!(hex.parse(), Ok(d));
        assert!(hex.to_ascii_uppercase().parse::<Sha256Digest>().is_err());
        assert!(hex[..63].parse::<Sha256Digest>().is_err());
        assert!(format!("{hex}0").parse::<Sha256Digest>().is_err());
        assert!(format!("g{}", &hex[1..]).parse::<Sha256Digest>().is_err());
    }
}
