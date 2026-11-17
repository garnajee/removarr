//! Time newtypes. All stored instants are UTC unix milliseconds.

use serde::{Deserialize, Serialize};

/// UTC unix time in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    #[must_use]
    pub fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    #[must_use]
    pub fn as_millis(self) -> i64 {
        self.0
    }

    #[must_use]
    pub fn plus_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }

    #[must_use]
    pub fn plus_hours(self, hours: i64) -> Self {
        self.plus_millis(hours.saturating_mul(3_600_000))
    }

    /// Whole days elapsed from `self` to `now` (negative differences give 0).
    #[must_use]
    pub fn age_days(self, now: Timestamp) -> u64 {
        u64::try_from(now.0.saturating_sub(self.0)).unwrap_or(0) / 86_400_000
    }
}

/// File modification time in nanoseconds since the unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MtimeNs(pub i64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_and_arithmetic() {
        let t = Timestamp(0);
        assert_eq!(
            t.plus_hours(48).age_days(Timestamp(0).plus_hours(48 + 49)),
            2
        );
        assert_eq!(Timestamp(10).age_days(Timestamp(5)), 0);
        assert_eq!(Timestamp(i64::MAX).plus_millis(5), Timestamp(i64::MAX));
    }
}
