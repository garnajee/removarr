//! Runtime settings (architecture 8.4 and section 11). Built by the application from the
//! validated configuration; this crate never reads files or the environment.

use std::time::Duration;

/// Scan schedule: an interval plus a UTC anchor, no cron expressions (C19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleConfig {
    /// `scan.interval_minutes`; 0 = manual scans only.
    pub scan_interval_minutes: u32,
    /// `scan.anchor_utc` as minutes after UTC midnight (03:00 = 180).
    pub anchor_minute_of_day: u32,
    /// `scan.qbit_refresh_minutes`; 0 = off. API only, never touches a disk.
    pub qbit_refresh_minutes: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            scan_interval_minutes: 1440,
            anchor_minute_of_day: 180,
            qbit_refresh_minutes: 60,
        }
    }
}

/// `[limits]` (architecture 8.4). Values above the documented maximum are clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitsConfig {
    pub content_read_concurrency: usize,
    pub ffprobe_concurrency: usize,
    pub http_concurrency_per_service: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            content_read_concurrency: 1,
            ffprobe_concurrency: 1,
            http_concurrency_per_service: 4,
        }
    }
}

/// Retry and backoff policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryConfig {
    /// Retries of a non-mutating job after `Unavailable` (so `1 + read_retries` attempts).
    pub read_retries: u32,
    /// First delay; it doubles per retry (1 s, 2 s, 4 s).
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Symmetric jitter ratio (0.2 = plus or minus 20 percent).
    pub jitter_ratio: f64,
    /// Pause between recovery attempts (5 minutes).
    pub recovery_interval: Duration,
    /// Recovery attempts before a manual retry is required (12).
    pub recovery_attempts: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            read_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            jitter_ratio: 0.2,
            recovery_interval: Duration::from_secs(300),
            recovery_attempts: 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JobsConfig {
    pub schedule: ScheduleConfig,
    pub limits: LimitsConfig,
    pub retry: RetryConfig,
    /// `mode.auto_mode`, read once at startup (ADR-0003): there is no runtime toggle.
    pub auto_mode: bool,
}
