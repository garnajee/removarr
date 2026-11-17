//! Adapter settings.

use std::time::Duration;

use removarr_config::{Config, PathMapper, Secret};
use url::Url;

/// Retries apply to reads that fail with `Unavailable` only. Mutations are never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    /// 3 retries, 1 s then 2 s then 4 s, each with +-20 % jitter (architecture 8.4).
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QbitSettings {
    pub url: Url,
    pub username: String,
    pub password: Secret,
    pub paths: PathMapper,
    pub max_response_bytes: u64,
    /// Parallel `torrents/files` calls in `QbitReader::snapshot`.
    pub concurrency: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub retry: RetryPolicy,
    /// After a rejected login, no new login is attempted for this long (avoids IP bans).
    pub login_cooldown: Duration,
}

impl QbitSettings {
    /// Defaults per architecture 8.4: connect 5 s, request 20 s, 32 MiB responses.
    #[must_use]
    pub fn new(url: Url, username: String, password: Secret, paths: PathMapper) -> Self {
        Self {
            url,
            username,
            password,
            paths,
            max_response_bytes: 32 * 1024 * 1024,
            concurrency: 4,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(20),
            retry: RetryPolicy::default(),
            login_cooldown: Duration::from_secs(300),
        }
    }

    /// Builds the settings from the validated configuration. `None` means qBittorrent is not
    /// configured; workflows turn that into `ServiceNotConfigured(Qbittorrent)`.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let svc = cfg.services.qbittorrent.as_ref()?;
        let mut s = Self::new(
            svc.url.clone(),
            svc.username.clone(),
            svc.password.clone(),
            cfg.paths.clone(),
        );
        s.max_response_bytes = cfg.limits.max_response_bytes;
        s.concurrency = usize::try_from(cfg.limits.http_concurrency_per_service)
            .unwrap_or(1)
            .max(1);
        Some(s)
    }
}
