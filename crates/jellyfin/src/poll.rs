//! Bounded polling of the asynchronous library scan (`library_refresh_running`).

use std::time::Duration;

use removarr_domain::error::PortError;
use removarr_domain::ports::JellyfinRead;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    /// Hard upper bound for the whole wait.
    pub timeout: Duration,
    pub interval: Duration,
    /// How long to wait for the task to be seen running before assuming it already finished.
    pub start_grace: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            interval: Duration::from_secs(2),
            start_grace: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPoll {
    /// Seen running, then idle.
    Completed,
    /// Never seen running within the grace period: it finished before the first poll or never
    /// started. Weaker than `Completed`; recognition checks decide.
    CompletedUnobserved,
    /// Still running when the timeout elapsed. Verification is incomplete, not failed.
    TimedOut,
}

/// Polls until the scan task is idle or the timeout elapses. A failing poll aborts with the
/// error (dependent verification is then incomplete).
pub async fn wait_for_library_refresh(
    read: &dyn JellyfinRead,
    cfg: PollConfig,
) -> Result<RefreshPoll, PortError> {
    let start = Instant::now();
    let mut seen_running = false;
    loop {
        let running = read.library_refresh_running().await?;
        let elapsed = start.elapsed();
        if running {
            seen_running = true;
        } else if seen_running {
            return Ok(RefreshPoll::Completed);
        } else if elapsed >= cfg.start_grace {
            return Ok(RefreshPoll::CompletedUnobserved);
        }
        if elapsed >= cfg.timeout {
            return Ok(RefreshPoll::TimedOut);
        }
        tokio::time::sleep(cfg.interval).await;
    }
}
