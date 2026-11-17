//! Login rate limit (architecture 12.3): bounded in-memory fixed windows keyed by client IP and
//! by `(IP, normalized login identifier)`, checked and reserved **before** Argon2.
//!
//! - An attempt is reserved atomically under the caller's lock, so concurrent requests cannot
//!   all pass the check before any of them is counted.
//! - A refused (`429`) attempt is not counted, so `Retry-After` stays stable.
//! - A successful login refunds its reservation: the `(IP, user)` window is cleared and the IP
//!   window gives one attempt back. Nothing is ever keyed by the user name alone, so nobody can
//!   lock the account for everyone (no global account lockout).
//! - State is bounded: each table holds at most `capacity` keys. Expired windows are purged on
//!   every call (deterministic, no background task). A full table evicts the window with the
//!   oldest start (ties: lowest key, `BTreeMap` order).
//! - IPv6 clients are grouped by `/64` (one host usually owns a whole `/64`).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr};

use unicode_normalization::UnicodeNormalization;

/// Length of a window (15 minutes, architecture 11 `auth.login_limit_*`).
pub const WINDOW_MS: i64 = 15 * 60 * 1000;
/// Maximum number of keys per table.
pub const DEFAULT_CAPACITY: usize = 4096;
/// Characters of the normalized identifier kept in a key.
const MAX_KEY_CHARS: usize = 64;

/// Normalized identifier used in the `(IP, user)` key: trimmed, NFKC, lower-case, at most
/// 64 characters. Only a rate-limit key: the credential check compares the raw input.
#[must_use]
pub fn normalize_user(raw: &str) -> String {
    raw.trim()
        .nfkc()
        .flat_map(char::to_lowercase)
        .take(MAX_KEY_CHARS)
        .collect()
}

/// Rate-limit key of an address: IPv4 (and IPv4-mapped IPv6) as is, IPv6 truncated to `/64`.
#[must_use]
pub fn ip_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    start: i64,
    count: u32,
}

/// One bounded table of fixed windows.
#[derive(Debug)]
struct Windows<K: Ord + Clone> {
    map: BTreeMap<K, Window>,
    capacity: usize,
}

impl<K: Ord + Clone> Windows<K> {
    fn new(capacity: usize) -> Self {
        Self {
            map: BTreeMap::new(),
            capacity: capacity.max(1),
        }
    }

    fn purge(&mut self, now: i64) {
        self.map
            .retain(|_, w| now < w.start.saturating_add(WINDOW_MS));
    }

    /// Milliseconds until the window of `k` ends, when it already holds `limit` attempts.
    fn blocked_for(&self, k: &K, limit: u32, now: i64) -> Option<i64> {
        let w = self.map.get(k)?;
        (w.count >= limit).then(|| w.start.saturating_add(WINDOW_MS) - now)
    }

    fn hit(&mut self, k: &K, now: i64) {
        if let Some(w) = self.map.get_mut(k) {
            w.count = w.count.saturating_add(1);
            return;
        }
        if self.map.len() >= self.capacity {
            let oldest = self
                .map
                .iter()
                .min_by_key(|(_, w)| w.start)
                .map(|(k, _)| k.clone());
            if let Some(o) = oldest {
                self.map.remove(&o);
            }
        }
        self.map.insert(
            k.clone(),
            Window {
                start: now,
                count: 1,
            },
        );
    }
}

/// Limits of the two tables (validated by configuration: 1..=1000 per window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_ip: u32,
    pub per_ip_user: u32,
    pub capacity: usize,
}

/// Both login tables. Callers hold it behind a mutex and never across an `await`.
#[derive(Debug)]
pub struct LoginLimiter {
    limits: Limits,
    ip: Windows<IpAddr>,
    ip_user: Windows<(IpAddr, String)>,
}

impl LoginLimiter {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            ip: Windows::new(limits.capacity),
            ip_user: Windows::new(limits.capacity),
        }
    }

    /// Reserves one attempt for `(ip, user)` at `now` (ms). `Err(retry_after_secs)` when either
    /// window is full; nothing is counted then.
    pub fn reserve(&mut self, ip: IpAddr, user: &str, now: i64) -> Result<(), u64> {
        self.purge(now);
        let ip = ip_key(ip);
        let pair = (ip, normalize_user(user));
        let blocked = self
            .ip
            .blocked_for(&ip, self.limits.per_ip, now)
            .into_iter()
            .chain(
                self.ip_user
                    .blocked_for(&pair, self.limits.per_ip_user, now),
            )
            .max();
        if let Some(ms) = blocked {
            return Err(retry_secs(ms));
        }
        self.ip.hit(&ip, now);
        self.ip_user.hit(&pair, now);
        Ok(())
    }

    /// A successful login: clears the `(ip, user)` window and gives one IP attempt back.
    pub fn succeeded(&mut self, ip: IpAddr, user: &str) {
        let ip = ip_key(ip);
        self.ip_user.map.remove(&(ip, normalize_user(user)));
        if let Some(w) = self.ip.map.get_mut(&ip) {
            w.count = w.count.saturating_sub(1);
        }
    }

    /// Drops every expired window.
    pub fn purge(&mut self, now: i64) {
        self.ip.purge(now);
        self.ip_user.purge(now);
    }

    /// Number of keys held (both tables).
    #[must_use]
    pub fn len(&self) -> usize {
        self.ip.map.len() + self.ip_user.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whole seconds, rounded up, at least 1 (`Retry-After` is never 0).
fn retry_secs(ms: i64) -> u64 {
    u64::try_from(ms.max(1)).map_or(1, |ms| ms.div_ceil(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn limiter(per_ip: u32, per_ip_user: u32, capacity: usize) -> LoginLimiter {
        LoginLimiter::new(Limits {
            per_ip,
            per_ip_user,
            capacity,
        })
    }

    #[test]
    fn per_ip_user_window_blocks_with_exact_retry_after_and_expires() {
        let mut l = limiter(20, 5, 100);
        let a = ip("192.0.2.1");
        for _ in 0..5 {
            assert_eq!(l.reserve(a, "admin", T0), Ok(()));
        }
        assert_eq!(l.reserve(a, "admin", T0), Err(900));
        assert_eq!(l.reserve(a, "admin", T0 + 600_000), Err(300));
        assert_eq!(l.reserve(a, "admin", T0 + WINDOW_MS - 1), Err(1));
        assert_eq!(l.reserve(a, "admin", T0 + WINDOW_MS), Ok(()));
    }

    #[test]
    fn retry_after_rounds_up_and_is_never_zero() {
        assert_eq!(retry_secs(1), 1);
        assert_eq!(retry_secs(0), 1);
        assert_eq!(retry_secs(-5), 1);
        assert_eq!(retry_secs(1000), 1);
        assert_eq!(retry_secs(1001), 2);
    }

    #[test]
    fn refused_attempts_are_not_counted() {
        let mut l = limiter(20, 2, 100);
        let a = ip("192.0.2.1");
        l.reserve(a, "u", T0).unwrap();
        l.reserve(a, "u", T0).unwrap();
        for _ in 0..50 {
            assert!(l.reserve(a, "u", T0 + 1).is_err());
        }
        // The IP window only holds the two counted attempts.
        for i in 0..18 {
            assert_eq!(l.reserve(a, &format!("other{i}"), T0 + 2), Ok(()));
        }
        assert!(l.reserve(a, "fresh", T0 + 3).is_err());
    }

    #[test]
    fn identifiers_are_independent_but_share_the_ip_budget() {
        let mut l = limiter(6, 3, 100);
        let a = ip("192.0.2.1");
        for _ in 0..3 {
            l.reserve(a, "alice", T0).unwrap();
        }
        assert!(l.reserve(a, "alice", T0).is_err());
        for _ in 0..3 {
            assert_eq!(l.reserve(a, "bob", T0), Ok(()));
        }
        // IP budget (6) exhausted for any identifier.
        assert!(l.reserve(a, "carol", T0).is_err());
        // Another address is unaffected, for every identifier (no account-wide lock).
        let b = ip("198.51.100.2");
        assert_eq!(l.reserve(b, "alice", T0), Ok(()));
    }

    #[test]
    fn identifier_normalization_shares_one_key() {
        let mut l = limiter(100, 2, 100);
        let a = ip("192.0.2.1");
        l.reserve(a, "Admin", T0).unwrap();
        l.reserve(a, "  admin ", T0).unwrap();
        assert!(l.reserve(a, "ＡＤＭＩＮ", T0).is_err(), "NFKC + lower-case");
        assert_eq!(normalize_user(&"x".repeat(500)).len(), 64);
    }

    #[test]
    fn ipv6_is_grouped_by_64_and_mapped_v4_is_canonical() {
        assert_eq!(ip_key(ip("2001:db8:1:2:aaaa::1")), ip("2001:db8:1:2::"));
        assert_eq!(ip_key(ip("::ffff:192.0.2.7")), ip("192.0.2.7"));
        let mut l = limiter(2, 100, 100);
        l.reserve(ip("2001:db8:1:2::1"), "a", T0).unwrap();
        l.reserve(ip("2001:db8:1:2::ffff"), "b", T0).unwrap();
        assert!(l.reserve(ip("2001:db8:1:2:1:2:3:4"), "c", T0).is_err());
        assert_eq!(l.reserve(ip("2001:db8:1:3::1"), "c", T0), Ok(()));
    }

    #[test]
    fn success_refunds_the_reservation() {
        let mut l = limiter(3, 2, 100);
        let a = ip("192.0.2.1");
        l.reserve(a, "admin", T0).unwrap();
        l.reserve(a, "admin", T0).unwrap();
        l.succeeded(a, "ADMIN");
        assert_eq!(l.reserve(a, "admin", T0), Ok(()));
        assert_eq!(l.reserve(a, "admin", T0), Ok(()));
        assert!(l.reserve(a, "admin", T0).is_err());
    }

    #[test]
    fn expired_windows_are_purged_deterministically() {
        let mut l = limiter(20, 5, 1000);
        for i in 0..200u32 {
            let a = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            l.reserve(a, "admin", T0 + i64::from(i)).unwrap();
        }
        assert_eq!(l.len(), 400);
        l.purge(T0 + WINDOW_MS + 100);
        assert_eq!(l.len(), 198, "windows started at T0..=T0+100 expired");
        l.purge(T0 + WINDOW_MS + 200);
        assert!(l.is_empty());
    }

    #[test]
    fn tables_are_bounded_and_evict_the_oldest_window() {
        let mut l = limiter(20, 5, 8);
        for i in 0..100u8 {
            l.reserve(IpAddr::from([10, 0, 0, i]), "admin", T0 + i64::from(i))
                .unwrap();
            assert!(l.ip.map.len() <= 8 && l.ip_user.map.len() <= 8);
        }
        let kept: Vec<_> = l.ip.map.keys().copied().collect();
        let expected: Vec<IpAddr> = (92..100u8).map(|i| IpAddr::from([10, 0, 0, i])).collect();
        assert_eq!(kept, expected, "the newest windows survive");
    }
}
