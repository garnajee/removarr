//! System clock and id generator (architecture 2.1: owned by the composition root).

use removarr_domain::ports::{Clock, IdGen};
use removarr_domain::time::Timestamp;

/// Wall clock, UTC milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Timestamp(ms)
    }
}

/// Time-ordered UUID v7 ids.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidV7;

impl IdGen for UuidV7 {
    fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_and_ids_are_monotonic_enough() {
        let a = SystemClock.now();
        let b = SystemClock.now();
        assert!(b >= a && a.as_millis() > 1_600_000_000_000);
        assert_ne!(UuidV7.uuid(), UuidV7.uuid());
    }
}
