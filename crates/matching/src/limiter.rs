//! Blocking limit on concurrent content reads issued through this crate.
//!
//! The filesystem adapter already bounds its own readers; this second bound keeps the matching
//! crate honest with any `ContentReader` implementation (architecture 8.4: at most 4).

use std::sync::{Arc, Condvar, Mutex, PoisonError};

#[derive(Debug, Default)]
struct State {
    active: usize,
    peak: usize,
}

/// Counting semaphore for blocking threads (callers run in `spawn_blocking`).
#[derive(Debug, Clone)]
pub struct ReadLimiter {
    inner: Arc<(Mutex<State>, Condvar)>,
    max: usize,
}

impl ReadLimiter {
    /// `max` is clamped to `1..=4` (architecture 8.4).
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            inner: Arc::new((Mutex::new(State::default()), Condvar::new())),
            max: max.clamp(1, 4),
        }
    }

    #[must_use]
    pub fn max(&self) -> usize {
        self.max
    }

    /// Highest number of reads observed at the same time.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.inner
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .peak
    }

    /// Blocks until a slot is free. The slot is released when the permit drops.
    #[must_use]
    pub fn acquire(&self) -> ReadPermit<'_> {
        let (lock, cv) = &*self.inner;
        let mut st = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while st.active >= self.max {
            st = cv.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        st.active += 1;
        st.peak = st.peak.max(st.active);
        ReadPermit { limiter: self }
    }
}

impl Default for ReadLimiter {
    fn default() -> Self {
        Self::new(1)
    }
}

/// RAII slot of a [`ReadLimiter`].
#[derive(Debug)]
pub struct ReadPermit<'a> {
    limiter: &'a ReadLimiter,
}

impl Drop for ReadPermit<'_> {
    fn drop(&mut self) {
        let (lock, cv) = &*self.limiter.inner;
        let mut st = lock.lock().unwrap_or_else(PoisonError::into_inner);
        st.active = st.active.saturating_sub(1);
        cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn bounds_concurrent_holders() {
        let lim = ReadLimiter::new(2);
        let current = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lim = lim.clone();
                let current = current.clone();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let _p = lim.acquire();
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    seen.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    current.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(seen.load(Ordering::SeqCst) <= 2);
        assert!(lim.peak() <= 2);
        assert_eq!(ReadLimiter::new(0).max(), 1);
        assert_eq!(ReadLimiter::new(99).max(), 4);
    }
}
