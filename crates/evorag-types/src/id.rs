//! 記録の ID (UUID v7、RFC 9562) と、その生成器 (03 §3.3、02 §6)。

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::time::{Clock, Timestamp};

/// 記録の ID。時刻順に並ぶ UUID v7 の 16 バイト。
///
/// バイト列の辞書順が生成の順になる (同じ生成器の中では単調に増える)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id([u8; 16]);

impl Id {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 生成した時刻 (UNIX 時刻のミリ秒)。UUID v7 の先頭 48 ビット。
    pub fn timestamp(&self) -> Timestamp {
        let mut ms = [0u8; 8];
        ms[2..].copy_from_slice(&self.0[..6]);
        Timestamp::from_millis(i64::from_be_bytes(ms))
    }
}

impl fmt::Display for Id {
    /// 小文字の 8-4-4-4-12 の形。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({self})")
    }
}

/// ID の文字列が 8-4-4-4-12 の 16 進の形でない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIdError;

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid id (expected 8-4-4-4-12 hex)")
    }
}

impl std::error::Error for ParseIdError {}

impl FromStr for Id {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b = s.as_bytes();
        if b.len() != 36 || [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
            return Err(ParseIdError);
        }
        let mut out = [0u8; 16];
        let mut hex = b.iter().filter(|&&c| c != b'-');
        for byte in &mut out {
            let hi = hex.next().and_then(|c| hex_value(*c)).ok_or(ParseIdError)?;
            let lo = hex.next().and_then(|c| hex_value(*c)).ok_or(ParseIdError)?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 文字列 (8-4-4-4-12) として読み書きする。書き出し (03 §11) の JSON でも読める形にするため。
impl serde::Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Id;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an id string (8-4-4-4-12 hex)")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Id, E> {
                v.parse().map_err(E::custom)
            }
        }
        d.deserialize_str(V)
    }
}

/// ID の生成器 (02 §6)。試験では種を固定した実装を注入する。
pub trait IdGen: Send + Sync {
    fn next_id(&self) -> Id;
}

/// UUID v7 の生成器。同じミリ秒の中でも単調に増える (RFC 9562 §6.2 の方法 1: 12 ビットの counter)。
///
/// - 時計が戻ったときは、最後に使った時刻のまま counter を進める。
/// - counter があふれたときは、時刻を 1 ミリ秒進める。
/// - 残りの 62 ビットは擬似乱数 (SplitMix64)。ID は秘密ではないので、暗号論的な乱数は要らない。
pub struct UuidV7Gen {
    clock: Arc<dyn Clock>,
    state: Mutex<GenState>,
}

struct GenState {
    last_ms: i64,
    counter: u16,
    rng: u64,
}

const COUNTER_MAX: u16 = 0x0fff;
const MAX_MS: i64 = (1 << 48) - 1;

impl UuidV7Gen {
    /// OS の乱数で種を決める。
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        use std::hash::{BuildHasher, Hasher};
        // RandomState は OS の乱数で初期化される。
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(0x6576_6f72_6167_5f69); // "evorag_i"
        Self::with_seed(clock, h.finish())
    }

    /// 種を固定する (試験用。同じ時計と種なら同じ列になる)。
    pub fn with_seed(clock: Arc<dyn Clock>, seed: u64) -> Self {
        Self {
            clock,
            state: Mutex::new(GenState {
                last_ms: -1,
                counter: 0,
                rng: seed,
            }),
        }
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl IdGen for UuidV7Gen {
    fn next_id(&self) -> Id {
        let now = self.clock.now().as_millis().clamp(0, MAX_MS);
        // 毒されたロックでも状態は壊れていない (更新は値の代入だけ)。
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if now > st.last_ms {
            st.last_ms = now;
            st.counter = 0;
        } else if st.counter < COUNTER_MAX {
            st.counter += 1;
        } else {
            st.last_ms = (st.last_ms + 1).min(MAX_MS);
            st.counter = 0;
        }
        let ms = st.last_ms as u64;
        let counter = st.counter;
        let rand = splitmix64(&mut st.rng);
        drop(st);

        let mut b = [0u8; 16];
        b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        // 版 7 と counter の上位 4 ビット、counter の下位 8 ビット。
        b[6] = 0x70 | ((counter >> 8) as u8 & 0x0f);
        b[7] = counter as u8;
        // variant (10) と 62 ビットの乱数。
        b[8..].copy_from_slice(&rand.to_be_bytes());
        b[8] = (b[8] & 0x3f) | 0x80;
        Id(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ManualClock;

    fn gen_at(ms: i64) -> (Arc<ManualClock>, UuidV7Gen) {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(ms)));
        let g = UuidV7Gen::with_seed(clock.clone(), 42);
        (clock, g)
    }

    #[test]
    fn ids_are_version_7_and_carry_the_time() {
        let (_, g) = gen_at(1_762_650_000_123);
        let id = g.next_id();
        assert_eq!(id.as_bytes()[6] >> 4, 7);
        assert_eq!(id.as_bytes()[8] >> 6, 0b10);
        assert_eq!(id.timestamp().as_millis(), 1_762_650_000_123);
    }

    #[test]
    fn ids_increase_within_the_same_millisecond_and_when_the_clock_goes_back() {
        let (clock, g) = gen_at(1_000);
        let mut prev = g.next_id();
        for i in 0..10_000 {
            if i == 5_000 {
                clock.set(Timestamp::from_millis(500));
            }
            let id = g.next_id();
            assert!(id > prev, "{prev} !< {id}");
            prev = id;
        }
    }

    #[test]
    fn same_seed_and_clock_give_the_same_sequence() {
        let (_, a) = gen_at(7);
        let (_, b) = gen_at(7);
        for _ in 0..100 {
            assert_eq!(a.next_id(), b.next_id());
        }
    }

    #[test]
    fn display_and_parse_round_trip() {
        let (_, g) = gen_at(1_762_650_000_123);
        let id = g.next_id();
        let s = id.to_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.parse::<Id>(), Ok(id));
        assert_eq!(s.to_uppercase().parse::<Id>(), Ok(id));
        assert!("not-an-id".parse::<Id>().is_err());
        assert!(s.replace('-', "x").parse::<Id>().is_err());
    }
}
