//! Bounded in-memory session table (architecture 12.3, ADR-0006).
//!
//! A session id is 32 random bytes carried in the encrypted `removarr_session` cookie. The table
//! holds at most [`MAX_SESSIONS`] entries with an absolute lifetime (`session_ttl_hours`) and an
//! idle timeout ([`IDLE_MS`]). Expired entries are purged on every call; a full table evicts the
//! least recently used session. Nothing is persisted: a restart revokes every session.

use std::collections::HashMap;

/// Maximum number of live sessions.
pub const MAX_SESSIONS: usize = 32;
/// Idle timeout (2 h).
pub const IDLE_MS: i64 = 2 * 3_600_000;

/// A session id (32 random bytes).
pub type SessionId = [u8; 32];

#[derive(Debug, Clone)]
struct Session {
    csrf: String,
    created: i64,
    last_seen: i64,
}

/// Result of a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Valid; the session's CSRF token. The idle clock was reset.
    Valid(String),
    /// Known id past its absolute or idle limit; it was removed.
    Expired,
    Unknown,
}

fn live(ttl_ms: i64, s: &Session, now: i64) -> bool {
    now < s.created.saturating_add(ttl_ms) && now < s.last_seen.saturating_add(IDLE_MS)
}

#[derive(Debug)]
pub struct SessionStore {
    map: HashMap<SessionId, Session>,
    ttl_ms: i64,
}

impl SessionStore {
    #[must_use]
    pub fn new(ttl_ms: i64) -> Self {
        Self {
            map: HashMap::with_capacity(MAX_SESSIONS),
            ttl_ms,
        }
    }

    /// Drops every expired session. Returns how many were removed.
    pub fn purge(&mut self, now: i64) -> usize {
        let before = self.map.len();
        let ttl = self.ttl_ms;
        self.map.retain(|_, s| live(ttl, s, now));
        before - self.map.len()
    }

    /// Inserts a new session. Returns `true` when a live session had to be evicted.
    pub fn create(&mut self, id: SessionId, csrf: String, now: i64) -> bool {
        self.purge(now);
        let mut evicted = false;
        if self.map.len() >= MAX_SESSIONS {
            let lru = self
                .map
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| *k);
            if let Some(k) = lru {
                self.map.remove(&k);
                evicted = true;
            }
        }
        self.map.insert(
            id,
            Session {
                csrf,
                created: now,
                last_seen: now,
            },
        );
        evicted
    }

    /// Validates `id` at `now` and resets its idle clock.
    pub fn lookup(&mut self, id: &SessionId, now: i64) -> Lookup {
        let expired = self.map.get(id).map(|s| !live(self.ttl_ms, s, now));
        self.purge(now);
        match (expired, self.map.get_mut(id)) {
            (Some(false), Some(s)) => {
                s.last_seen = now;
                Lookup::Valid(s.csrf.clone())
            }
            (Some(_), _) => Lookup::Expired,
            (None, _) => Lookup::Unknown,
        }
    }

    /// Logout or rotation. Returns whether the id existed.
    pub fn remove(&mut self, id: &SessionId) -> bool {
        self.map.remove(id).is_some()
    }

    /// Revokes every session. Returns how many were removed.
    pub fn clear(&mut self) -> usize {
        let n = self.map.len();
        self.map.clear();
        n
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;
    const H: i64 = 3_600_000;

    fn id(n: u8) -> SessionId {
        [n; 32]
    }

    #[test]
    fn valid_session_returns_its_token() {
        let mut s = SessionStore::new(12 * H);
        s.create(id(1), "tok".into(), T0);
        assert_eq!(s.lookup(&id(1), T0 + 1), Lookup::Valid("tok".into()));
        assert_eq!(s.lookup(&id(2), T0 + 1), Lookup::Unknown);
    }

    #[test]
    fn idle_timeout_expires_and_activity_extends_it() {
        let mut s = SessionStore::new(12 * H);
        s.create(id(1), "a".into(), T0);
        assert!(matches!(
            s.lookup(&id(1), T0 + IDLE_MS - 1),
            Lookup::Valid(_)
        ));
        // last_seen moved: still valid two idle periods after creation.
        assert!(matches!(
            s.lookup(&id(1), T0 + 2 * IDLE_MS - 2),
            Lookup::Valid(_)
        ));
        s.create(id(2), "b".into(), T0);
        s.create(id(3), "c".into(), T0);
        assert_eq!(s.lookup(&id(2), T0 + IDLE_MS), Lookup::Expired);
        assert_eq!(s.len(), 1, "the idle session 3 was purged by the same call");
        assert_eq!(s.lookup(&id(3), T0 + IDLE_MS), Lookup::Unknown);
    }

    #[test]
    fn absolute_lifetime_wins_over_activity() {
        let mut s = SessionStore::new(3 * H);
        s.create(id(1), "a".into(), T0);
        for k in 1..3 {
            assert!(matches!(s.lookup(&id(1), T0 + k * H), Lookup::Valid(_)));
        }
        assert_eq!(s.lookup(&id(1), T0 + 3 * H), Lookup::Expired);
        assert_eq!(s.lookup(&id(1), T0 + 3 * H), Lookup::Unknown);
        assert!(s.is_empty());
    }

    #[test]
    fn table_is_bounded_and_evicts_the_least_recently_used() {
        let mut s = SessionStore::new(12 * H);
        for n in 0..MAX_SESSIONS as u8 {
            assert!(!s.create(id(n), "t".into(), T0 + i64::from(n)));
        }
        // Touch session 0 so session 1 becomes the least recently used.
        assert!(matches!(s.lookup(&id(0), T0 + 100), Lookup::Valid(_)));
        assert!(s.create(id(200), "t".into(), T0 + 101));
        assert_eq!(s.len(), MAX_SESSIONS);
        assert_eq!(s.lookup(&id(1), T0 + 102), Lookup::Unknown);
        assert!(matches!(s.lookup(&id(0), T0 + 102), Lookup::Valid(_)));
    }

    #[test]
    fn purge_remove_and_clear() {
        let mut s = SessionStore::new(H);
        s.create(id(1), "a".into(), T0);
        s.create(id(2), "b".into(), T0 + H / 2);
        assert_eq!(s.purge(T0 + H), 1);
        assert!(s.remove(&id(2)));
        assert!(!s.remove(&id(2)));
        s.create(id(3), "c".into(), T0 + H);
        s.create(id(4), "d".into(), T0 + H);
        assert_eq!(s.clear(), 2);
        assert!(s.is_empty());
    }
}
