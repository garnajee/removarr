//! Exact timestamp formatting. Stored instants are UTC unix milliseconds.

use time::OffsetDateTime;
use time::macros::format_description;

/// `2026-11-12T08:30:00.123Z`. Out-of-range values give `invalid-time` instead of failing.
#[must_use]
pub fn iso(ms: i64) -> String {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| "invalid-time".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn formats_exact_utc_milliseconds() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso(i64::MAX), "invalid-time");
    }
}
