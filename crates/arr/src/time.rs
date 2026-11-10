//! Minimal RFC 3339 parsing for ARR timestamps (`2024-05-01T10:20:30.1234567Z`, or with an
//! offset). Returns UTC unix milliseconds.

use removarr_domain::time::Timestamp;

fn num(s: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let part = s.get(range)?;
    if part.bytes().all(|b| b.is_ascii_digit()) {
        part.parse().ok()
    } else {
        None
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn parse_rfc3339(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(s, 0..4)?, num(s, 5..7)?, num(s, 8..10)?);
    let (h, mi, sec) = (num(s, 11..13)?, num(s, 14..16)?, num(s, 17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let frac = &s[start..i.min(start + 3)];
        millis = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
    }
    let offset_min = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = num(s, i + 1..i + 3)?;
            let om = num(s, i + 4..i + 6)?;
            let v = oh * 60 + om;
            if *sign == b'-' { -v } else { v }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3_600 + mi * 60 + sec - offset_min * 60;
    Some(Timestamp(secs * 1_000 + millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_offsets_and_fractions() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(Timestamp(0)));
        assert_eq!(
            parse_rfc3339("2023-11-14T22:13:20Z"),
            Some(Timestamp(1_700_000_000_000))
        );
        assert_eq!(
            parse_rfc3339("2023-11-14T22:13:20.1234567Z"),
            Some(Timestamp(1_700_000_000_123))
        );
        assert_eq!(
            parse_rfc3339("2023-11-15T00:13:20+02:00"),
            Some(Timestamp(1_700_000_000_000))
        );
        assert_eq!(
            parse_rfc3339("2000-02-29T00:00:00Z"),
            Some(Timestamp(951_782_400_000))
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "2023-11-14",
            "2023-11-14T22:13:20",
            "2023-13-14T22:13:20Z",
            "2023-11-14T22:13:20.Z",
            "2023-11-14T22:13:20Zjunk",
            "abcd-11-14T22:13:20Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }
}
