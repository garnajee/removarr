//! Logging (architecture 10.2): sanitized JSON lines to stdout plus the bounded in-memory ring
//! served at `/logs`. Level from `server.log_level`; `RUST_LOG` overrides. HTTP client crates
//! stay below DEBUG (handoff 09). Every configured secret value is registered with the
//! redactor before the first event.

use removarr_config::Config;
use removarr_reporting::logs::DEFAULT_CAPACITY;
use removarr_reporting::{LogBuffer, LogLayer, LogLevel};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Ring level from the configured log level.
#[must_use]
pub fn level(cfg: &Config) -> LogLevel {
    match cfg.server.log_level {
        removarr_config::LogLevel::Debug => LogLevel::Debug,
        removarr_config::LogLevel::Info => LogLevel::Info,
        removarr_config::LogLevel::Warn => LogLevel::Warn,
        removarr_config::LogLevel::Error => LogLevel::Error,
    }
}

/// The log ring with every configured secret registered for exact-value removal.
#[must_use]
pub fn buffer(cfg: &Config) -> LogBuffer {
    let buf = LogBuffer::new(DEFAULT_CAPACITY, level(cfg));
    let r = buf.redactor();
    r.add_secret(cfg.auth.password_hash.expose());
    r.add_secret(cfg.auth.session_key.expose());
    if let Some(q) = &cfg.services.qbittorrent {
        r.add_secret(q.password.expose());
    }
    for s in [
        &cfg.services.sonarr,
        &cfg.services.radarr,
        &cfg.services.jellyfin,
    ]
    .into_iter()
    .flatten()
    {
        r.add_secret(s.api_key.expose());
    }
    buf
}

/// Default filter: the configured level, HTTP stacks and sqlx kept quiet.
#[must_use]
pub fn default_filter(level: LogLevel) -> String {
    let l = level.as_str().to_ascii_lowercase();
    format!(
        "{l},hyper=info,hyper_util=info,reqwest=info,h2=info,rustls=info,sqlx=warn,tower_http=info"
    )
}

/// Installs the global subscriber (once per process; later calls are ignored).
pub fn install(buf: &LogBuffer) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter(buf.capture_level())));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(LogLayer::stdout(buf.clone()))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_stacks_never_log_below_info_by_default() {
        let f = default_filter(LogLevel::Debug);
        assert!(f.starts_with("debug,"));
        assert!(f.contains("reqwest=info") && f.contains("hyper=info") && f.contains("sqlx=warn"));
        assert!(EnvFilter::try_new(f).is_ok());
    }
}
