//! 時刻と時計 (02 §6)。時刻は UTC の UNIX 時刻のミリ秒 (03 §3.2)。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// UTC の UNIX 時刻 (ミリ秒)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

/// 時計 (02 §6)。試験では固定の時計を注入する。
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// OS の時計。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            // 1970 年より前に合わせた時計。
            Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
        };
        Timestamp(ms)
    }
}

/// 試験用の時計。`set` と `advance` でだけ動く。
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(at: Timestamp) -> Self {
        Self(AtomicI64::new(at.0))
    }

    pub fn set(&self, at: Timestamp) {
        self.0.store(at.0, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(Timestamp::from_millis(10));
        assert_eq!(c.now().as_millis(), 10);
        c.advance(5);
        assert_eq!(c.now().as_millis(), 15);
        c.set(Timestamp::from_millis(3));
        assert_eq!(c.now().as_millis(), 3);
    }

    #[test]
    fn system_clock_is_after_2026() {
        assert!(SystemClock.now().as_millis() > 1_767_225_600_000);
    }
}
