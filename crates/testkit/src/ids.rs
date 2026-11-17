//! `SeqIds`: deterministic, sequential UUIDs.

use std::sync::atomic::{AtomicU64, Ordering};

use removarr_domain::ids::RawUuid as Uuid;
use removarr_domain::ports::IdGen;

#[derive(Debug, Default)]
pub struct SeqIds(AtomicU64);

impl SeqIds {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGen for SeqIds {
    /// First call returns `00000000-0000-0000-0000-000000000001`.
    fn uuid(&self) -> Uuid {
        Uuid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst) + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_deterministic_and_unique() {
        let a = SeqIds::new();
        let b = SeqIds::new();
        let xs: Vec<_> = (0..3).map(|_| a.uuid()).collect();
        let ys: Vec<_> = (0..3).map(|_| b.uuid()).collect();
        assert_eq!(xs, ys);
        assert_eq!(xs[0], Uuid::from_u128(1));
        assert!(xs[0] != xs[1] && xs[1] != xs[2]);
    }
}
