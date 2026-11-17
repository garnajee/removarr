//! `FakeClock`: a clock that only moves when the test advances it.

use std::sync::atomic::{AtomicI64, Ordering};

use removarr_domain::ports::Clock;
use removarr_domain::time::Timestamp;

#[derive(Debug)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    #[must_use]
    pub fn new(start: Timestamp) -> Self {
        Self(AtomicI64::new(start.as_millis()))
    }

    pub fn advance_millis(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn advance_hours(&self, hours: i64) {
        self.advance_millis(hours * 3_600_000);
    }

    pub fn set(&self, t: Timestamp) {
        self.0.store(t.as_millis(), Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(Timestamp(1_700_000_000_000))
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_moves_when_advanced() {
        let c = FakeClock::new(Timestamp(10));
        assert_eq!(c.now(), Timestamp(10));
        assert_eq!(c.now(), Timestamp(10));
        c.advance_millis(5);
        c.advance_hours(1);
        assert_eq!(c.now(), Timestamp(10 + 5 + 3_600_000));
        c.set(Timestamp(1));
        assert_eq!(c.now(), Timestamp(1));
    }
}
