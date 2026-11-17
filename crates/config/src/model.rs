//! Typed, validated configuration (architecture section 11). Secrets are [`Secret`] values.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use removarr_domain::canonical::{canonical_json_string, sha256_hex};
use removarr_domain::policy::Capability;
use serde::Serialize;
use url::Url;

use crate::live::LiveMode;
use crate::mapping::PathMapper;
use crate::proxy::TrustedProxies;
use crate::roots::Roots;
use crate::secret::Secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// Canonical, outside every root. Holds the SQLite database and nothing else of ours.
    pub data_dir: PathBuf,
    pub log_level: LogLevel,
    pub trusted_proxies: TrustedProxies,
    pub shutdown_grace_seconds: u64,
}

/// Expected owner of files under the roots. Defaults to the process effective uid:gid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    /// True when no explicit value was configured (log it at startup, OQ-09).
    pub from_process: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thresholds {
    pub incomplete_stalled_days: u32,
    pub auto_min_age_hours: u32,
    pub hide_recent_days: u32,
    pub recheck_timeout_seconds: u32,
    pub plan_ttl_hours: u32,
}

/// Lower-case, dot-prefixed extension lists (pairwise disjoint).
#[derive(Debug, Clone, Serialize)]
pub struct FileRules {
    pub video_extensions: Vec<String>,
    pub junk_extensions: Vec<String>,
    pub subtitle_extensions: Vec<String>,
}

/// `.r00` to `.r99` (split RAR volumes), junk in addition to the configured list (OQ-02).
fn is_rar_volume(ext: &str) -> bool {
    let b = ext.as_bytes();
    b.len() == 4 && b[0] == b'.' && b[1] == b'r' && b[2].is_ascii_digit() && b[3].is_ascii_digit()
}

impl FileRules {
    fn has(list: &[String], ext_with_dot: &str) -> bool {
        let e = ext_with_dot.to_ascii_lowercase();
        list.contains(&e)
    }

    /// `ext` includes the leading dot; case-insensitive.
    #[must_use]
    pub fn is_video(&self, ext: &str) -> bool {
        Self::has(&self.video_extensions, ext)
    }

    #[must_use]
    pub fn is_junk(&self, ext: &str) -> bool {
        Self::has(&self.junk_extensions, ext) || is_rar_volume(&ext.to_ascii_lowercase())
    }

    #[must_use]
    pub fn is_subtitle(&self, ext: &str) -> bool {
        Self::has(&self.subtitle_extensions, ext)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AnchorUtc {
    pub hour: u8,
    pub minute: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanConfig {
    /// 0 = manual only, otherwise 60..=10080.
    pub interval_minutes: u32,
    pub anchor_utc: AnchorUtc,
    /// 0 = off, otherwise 5..=1440. API-only refresh, no disk I/O.
    pub qbit_refresh_minutes: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub content_read_concurrency: u32,
    pub ffprobe_concurrency: u32,
    pub http_concurrency_per_service: u32,
    /// Per response body; connectors stream with a counting limit.
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchingConfig {
    pub similarity_threshold: f64,
    pub sample_size_bytes: u64,
    pub sample_count: u32,
    pub compare_chunk_bytes: u64,
    pub duration_tolerance_seconds: f64,
    pub duration_tolerance_ratio: f64,
    pub ffprobe_path: String,
}

#[derive(Debug, Clone)]
pub struct QbitService {
    pub url: Url,
    pub username: String,
    pub password: Secret,
    /// Normalized service-side prefixes declared outside every root (OQ-07).
    pub external_prefixes: Vec<String>,
}

/// Sonarr, Radarr and Jellyfin: base URL plus API key.
#[derive(Debug, Clone)]
pub struct ApiService {
    pub url: Url,
    pub api_key: Secret,
}

#[derive(Debug, Clone, Default)]
pub struct Services {
    pub qbittorrent: Option<QbitService>,
    pub sonarr: Option<ApiService>,
    pub radarr: Option<ApiService>,
    pub jellyfin: Option<ApiService>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Mode {
    pub dry_run: bool,
    pub auto_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    enabled: BTreeMap<Capability, bool>,
}

impl Capabilities {
    pub(crate) fn from_map(enabled: BTreeMap<Capability, bool>) -> Self {
        Self { enabled }
    }

    #[must_use]
    pub fn is_enabled(&self, c: Capability) -> bool {
        self.enabled
            .get(&c)
            .copied()
            .unwrap_or_else(|| c.default_enabled())
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub username: String,
    /// Argon2 PHC string.
    pub password_hash: Secret,
    pub session_key: Secret,
    pub session_ttl_hours: u32,
    pub cookie_secure: bool,
    pub login_limit_per_ip: u32,
    pub login_limit_per_ip_user: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub paths: PathMapper,
    pub owner: Owner,
    pub thresholds: Thresholds,
    pub files: FileRules,
    pub scan: ScanConfig,
    pub limits: Limits,
    pub matching: MatchingConfig,
    pub services: Services,
    pub mode: Mode,
    pub capabilities: Capabilities,
    pub auth: AuthConfig,
}

impl Config {
    #[must_use]
    pub fn roots(&self) -> &Roots {
        self.paths.roots()
    }

    /// `Some` only when `dry_run = false`. The single constructor of [`LiveMode`].
    #[must_use]
    pub fn live_mode(&self) -> Option<LiveMode> {
        (!self.mode.dry_run).then(LiveMode::new)
    }

    /// sha256 of the canonical non-secret policy view: roots, mappings, owner, thresholds,
    /// files, matching, capabilities and mode. Stored in plans (`Precondition::ConfigFingerprint`).
    /// Secrets, URLs, bind address and auth settings are not part of it.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        #[derive(Serialize)]
        struct View<'a> {
            roots: &'a Roots,
            mappings: BTreeMap<&'static str, &'a [crate::mapping::MapRule]>,
            external_prefixes: &'a [String],
            owner: Owner,
            thresholds: &'a Thresholds,
            files: &'a FileRules,
            matching: &'a MatchingConfig,
            capabilities: &'a Capabilities,
            mode: Mode,
        }
        use removarr_domain::session::ServiceKind::*;
        let mappings = [Qbittorrent, Sonarr, Radarr, Jellyfin]
            .into_iter()
            .map(|s| (s.as_str(), self.paths.rules(s)))
            .collect();
        let view = View {
            roots: self.paths.roots(),
            mappings,
            external_prefixes: self.paths.external_prefixes(),
            owner: self.owner,
            thresholds: &self.thresholds,
            files: &self.files,
            matching: &self.matching,
            capabilities: &self.capabilities,
            mode: self.mode,
        };
        // Serialization of plain data cannot fail; fall back to a distinct constant if it did.
        let json = canonical_json_string(&view).unwrap_or_default();
        sha256_hex(json.as_bytes())
    }

    fn secret_values(&self) -> Vec<&Secret> {
        let mut v = vec![&self.auth.password_hash, &self.auth.session_key];
        if let Some(q) = &self.services.qbittorrent {
            v.push(&q.password);
        }
        for s in [
            &self.services.sonarr,
            &self.services.radarr,
            &self.services.jellyfin,
        ]
        .into_iter()
        .flatten()
        {
            v.push(&s.api_key);
        }
        v
    }

    /// Replaces every configured secret value, then applies [`crate::redact_text`]. Use it on
    /// any text that may echo upstream error bodies.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in self.secret_values() {
            if s.expose().len() >= 4 {
                out = out.replace(s.expose(), crate::secret::REDACTED);
            }
        }
        crate::redact_text(&out)
    }

    /// Non-secret startup lines for the log. Never contains a secret value.
    #[must_use]
    pub fn startup_summary(&self) -> Vec<String> {
        let r = self.roots();
        let svc = |name: &str, on: bool| {
            format!(
                "service {name}: {}",
                if on { "configured" } else { "not configured" }
            )
        };
        vec![
            format!(
                "dry_run={} auto_mode={}",
                self.mode.dry_run, self.mode.auto_mode
            ),
            format!(
                "roots: completed=1 incomplete={} media={}",
                usize::from(r.incomplete().is_some()),
                r.media().len()
            ),
            format!(
                "expected owner {}:{} ({})",
                self.owner.uid,
                self.owner.gid,
                if self.owner.from_process {
                    "process default"
                } else {
                    "configured"
                }
            ),
            svc("qbittorrent", self.services.qbittorrent.is_some()),
            svc("sonarr", self.services.sonarr.is_some()),
            svc("radarr", self.services.radarr.is_some()),
            svc("jellyfin", self.services.jellyfin.is_some()),
            format!(
                "trusted proxy networks: {}",
                self.server.trusted_proxies.networks().len()
            ),
            format!("config fingerprint {}", &self.fingerprint()[..12]),
        ]
    }
}
