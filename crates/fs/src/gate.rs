//! A tiny counting semaphore for blocking threads (scans and content reads).
//!
//! The filesystem port is synchronous and runs inside `spawn_blocking`. Several callers could
//! otherwise walk or read the same spinning disk at once. The gate bounds that concurrency, and
//! records the peak so tests and performance runs can prove the bound.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

#[derive(Debug)]
pub(crate) struct Gate {
    permits: Mutex<usize>,
    cv: Condvar,
    active: AtomicUsize,
    peak: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct Permit<'a>(&'a Gate);

impl Gate {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits.max(1)),
            cv: Condvar::new(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    pub(crate) fn acquire(&self) -> Permit<'_> {
        let mut n = self.permits.lock().unwrap_or_else(PoisonError::into_inner);
        while *n == 0 {
            n = self.cv.wait(n).unwrap_or_else(PoisonError::into_inner);
        }
        *n -= 1;
        drop(n);
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        Permit(self)
    }

    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        let mut n = self
            .0
            .permits
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *n += 1;
        drop(n);
        self.0.cv.notify_one();
    }
}
