//! Retry with exponential backoff and jitter (architecture 8.4). Retries are always bounded:
//! there is no infinite retry anywhere in this crate.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::RetryConfig;

/// Source of jitter, so tests are deterministic.
pub trait Jitter: Send + Sync {
    /// A value in `[-1.0, 1.0]`.
    fn unit(&self) -> f64;
}

/// Jitter from the operating system's random source (`getrandom`). Falls back to no jitter
/// if the source fails.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomJitter;

impl Jitter for RandomJitter {
    fn unit(&self) -> f64 {
        getrandom::u32().map_or(0.0, |v| f64::from(v) / f64::from(u32::MAX) * 2.0 - 1.0)
    }
}

/// No jitter at all (tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoJitter;

impl Jitter for NoJitter {
    fn unit(&self) -> f64 {
        0.0
    }
}

/// Delay of retry number `retry` (1-based): `base * 2^(retry-1)`, capped, then jittered.
#[must_use]
pub fn delay(cfg: &RetryConfig, retry: u32, jitter: &dyn Jitter) -> Duration {
    let exp = retry.saturating_sub(1).min(20);
    let raw = cfg
        .base_delay
        .saturating_mul(1u32 << exp)
        .min(cfg.max_delay);
    let factor = 1.0 + cfg.jitter_ratio.clamp(0.0, 1.0) * jitter.unit().clamp(-1.0, 1.0);
    raw.mul_f64(factor.max(0.0))
}

/// Why [`retry`] gave up.
#[derive(Debug, PartialEq, Eq)]
pub struct RetryFailure<E> {
    pub error: E,
    /// Calls made, the first one included.
    pub attempts: u32,
    /// True when the caller cancelled while waiting.
    pub cancelled: bool,
}

/// Calls `op` until it succeeds, returns a non-transient error, or `1 + cfg.read_retries`
/// calls were made. `on_retry(retry, delay, &error)` runs before every wait. Cancellation
/// interrupts the wait, never a running call.
pub async fn retry<T, E, F, Fut>(
    cfg: &RetryConfig,
    jitter: &dyn Jitter,
    cancel: &CancellationToken,
    is_transient: impl Fn(&E) -> bool,
    mut on_retry: impl FnMut(u32, Duration, &E),
    mut op: F,
) -> Result<T, RetryFailure<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        let error = match op().await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if !is_transient(&error) || attempts > cfg.read_retries {
            return Err(RetryFailure {
                error,
                attempts,
                cancelled: false,
            });
        }
        let wait = delay(cfg, attempts, jitter);
        on_retry(attempts, wait, &error);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = cancel.cancelled() => {
                return Err(RetryFailure { error, attempts, cancelled: true });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(f64);
    impl Jitter for Fixed {
        fn unit(&self) -> f64 {
            self.0
        }
    }

    #[test]
    fn delays_double_and_are_capped() {
        let cfg = RetryConfig::default();
        let secs = |r| delay(&cfg, r, &NoJitter).as_secs();
        assert_eq!((secs(1), secs(2), secs(3)), (1, 2, 4));
        assert_eq!(secs(30), 60);
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let cfg = RetryConfig::default();
        assert_eq!(delay(&cfg, 2, &Fixed(1.0)), Duration::from_millis(2400));
        assert_eq!(delay(&cfg, 2, &Fixed(-1.0)), Duration::from_millis(1600));
        assert_eq!(delay(&cfg, 2, &Fixed(7.0)), Duration::from_millis(2400));
        let rnd = RandomJitter.unit();
        assert!((-1.0..=1.0).contains(&rnd));
    }
}
