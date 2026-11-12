//! Scan scheduling: interval plus UTC anchor (architecture 8.5). Pure arithmetic; the runtime
//! sleeps until the next due instant, so there is no polling loop.

use removarr_domain::time::Timestamp;

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// `HH:MM` to minutes after midnight.
#[must_use]
pub fn parse_anchor(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Next run strictly after `now`: `anchor + k * interval` on the UTC day of `now`. A run missed
/// while the process was down is not replayed (a restart never wakes the disks by itself).
#[must_use]
pub fn next_anchored(
    now: Timestamp,
    anchor_minute: u32,
    interval_minutes: u32,
) -> Option<Timestamp> {
    if interval_minutes == 0 {
        return None;
    }
    let day_start = now.0.div_euclid(DAY_MS) * DAY_MS;
    let a0 = day_start + i64::from(anchor_minute) * MINUTE_MS;
    let step = i64::from(interval_minutes) * MINUTE_MS;
    let k = (now.0 - a0).div_euclid(step) + 1;
    Some(Timestamp(a0 + k * step))
}

/// `from + minutes`, `None` when the feature is off (0).
#[must_use]
pub fn next_every(from: Timestamp, minutes: u32) -> Option<Timestamp> {
    (minutes != 0).then(|| from.plus_millis(i64::from(minutes) * MINUTE_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    #[test]
    fn anchor_parsing() {
        assert_eq!(parse_anchor("03:00"), Some(180));
        assert_eq!(parse_anchor("23:59"), Some(1439));
        assert_eq!(parse_anchor("24:00"), None);
        assert_eq!(parse_anchor("3"), None);
        assert_eq!(parse_anchor("03:60"), None);
    }

    #[test]
    fn daily_anchor_is_the_next_occurrence_strictly_after_now() {
        let day = 20_000 * DAY_MS;
        let anchor = 180;
        assert_eq!(
            next_anchored(Timestamp(day + H), anchor, 1440),
            Some(Timestamp(day + 3 * H))
        );
        assert_eq!(
            next_anchored(Timestamp(day + 3 * H), anchor, 1440),
            Some(Timestamp(day + 27 * H))
        );
        assert_eq!(
            next_anchored(Timestamp(day + 5 * H), anchor, 1440),
            Some(Timestamp(day + 27 * H))
        );
    }

    #[test]
    fn shorter_intervals_follow_the_anchor_grid() {
        let day = 20_000 * DAY_MS;
        // every 6 hours from 03:00: 03, 09, 15, 21, then 03 the next day.
        assert_eq!(
            next_anchored(Timestamp(day + 10 * H), 180, 360),
            Some(Timestamp(day + 15 * H))
        );
        assert_eq!(
            next_anchored(Timestamp(day + 22 * H), 180, 360),
            Some(Timestamp(day + 27 * H))
        );
        assert_eq!(
            next_anchored(Timestamp(day + H), 180, 360),
            Some(Timestamp(day + 3 * H))
        );
    }

    #[test]
    fn zero_means_off() {
        assert_eq!(next_anchored(Timestamp(5), 180, 0), None);
        assert_eq!(next_every(Timestamp(5), 0), None);
        assert_eq!(
            next_every(Timestamp(5), 60),
            Some(Timestamp(5 + 60 * MINUTE_MS))
        );
    }
}
