//! Structural dry-run enforcement (ADR-0003).

/// Proof that the process runs with `dry_run = false`. The field is private, so the only
/// constructor is [`crate::Config::live_mode`]. Every live mutating adapter takes `&LiveMode`.
#[derive(Debug, Clone)]
pub struct LiveMode(());

impl LiveMode {
    pub(crate) fn new() -> Self {
        Self(())
    }

    /// Test-only constructor. The feature is enabled through dev-dependencies only.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn for_tests() -> Self {
        Self(())
    }
}
