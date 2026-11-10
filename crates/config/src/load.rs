//! Loading: optional TOML file, then `REMOVARR_*` environment overrides, then validation.
//! Secrets come from the environment only. Every problem is collected, none quotes a value.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;

use removarr_domain::policy::Capability;
use removarr_domain::session::ServiceKind;
use url::Url;

use crate::error::{ConfigError, Problem};
use crate::file::{FileConfig, MappingFile};
use crate::mapping::{PathMapper, env_name};
use crate::model::*;
use crate::proxy::TrustedProxies;
use crate::roots::{RootPaths, Roots, validate_data_dir};
use crate::secret::Secret;

/// Environment snapshot (only `REMOVARR_*` variables matter).
pub type Env = BTreeMap<String, String>;

pub const ENV_CONFIG_FILE: &str = "REMOVARR_CONFIG_FILE";
pub const DEFAULT_CONFIG_FILE: &str = "/config/removarr.toml";
const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub const ENV_QBIT_PASSWORD: &str = "REMOVARR_QBIT_PASSWORD";
pub const ENV_SONARR_API_KEY: &str = "REMOVARR_SONARR_API_KEY";
pub const ENV_RADARR_API_KEY: &str = "REMOVARR_RADARR_API_KEY";
pub const ENV_JELLYFIN_API_KEY: &str = "REMOVARR_JELLYFIN_API_KEY";
pub const ENV_PASSWORD_HASH: &str = "REMOVARR_PASSWORD_HASH";
pub const ENV_SESSION_KEY: &str = "REMOVARR_SESSION_KEY";

const DEFAULT_VIDEO: &[&str] = &[".mkv", ".mp4", ".avi", ".m4v", ".ts", ".m2ts", ".wmv"];
const DEFAULT_JUNK: &[&str] = &[
    ".nfo", ".txt", ".sfv", ".md5", ".url", ".rar", ".zip", ".7z", ".par2", ".jpg", ".jpeg", ".png",
];
const DEFAULT_SUBTITLE: &[&str] = &[".srt", ".ass", ".ssa", ".sub", ".idx", ".vtt"];

const ALL_CAPABILITIES: [(Capability, &str); 9] = [
    (Capability::Consolidate, "REMOVARR_CAP_CONSOLIDATE"),
    (Capability::DeleteCompleted, "REMOVARR_CAP_DELETE_COMPLETED"),
    (
        Capability::CleanupIncomplete,
        "REMOVARR_CAP_CLEANUP_INCOMPLETE",
    ),
    (Capability::FixPermissions, "REMOVARR_CAP_FIX_PERMISSIONS"),
    (
        Capability::ArrManualImport,
        "REMOVARR_CAP_ARR_MANUAL_IMPORT",
    ),
    (Capability::ArrQueueRemove, "REMOVARR_CAP_ARR_QUEUE_REMOVE"),
    (
        Capability::ArrSetMonitored,
        "REMOVARR_CAP_ARR_SET_MONITORED",
    ),
    (Capability::ArrGrab, "REMOVARR_CAP_ARR_GRAB"),
    (
        Capability::AutoRemoveEmptyTorrents,
        "REMOVARR_CAP_AUTO_REMOVE_EMPTY_TORRENTS",
    ),
];

struct L<'a> {
    env: &'a Env,
    problems: Vec<Problem>,
}

impl L<'_> {
    /// Environment value, trimmed. Empty counts as unset (compose `${VAR:-}`).
    fn env(&self, name: &str) -> Option<&str> {
        self.env
            .get(name)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    fn bad(&mut self, key: &str, env: &'static str, reason: &str) {
        self.problems.push(Problem::invalid(key, Some(env), reason));
    }

    fn num<T>(
        &mut self,
        key: &str,
        env: &'static str,
        file: Option<T>,
        default: T,
        min: T,
        max: T,
    ) -> T
    where
        T: FromStr + PartialOrd + Copy + Display,
    {
        let v = match self.env(env) {
            Some(s) => match s.parse::<T>() {
                Ok(v) => v,
                Err(_) => {
                    self.bad(key, env, "not a valid number");
                    return default;
                }
            },
            None => file.unwrap_or(default),
        };
        if !(v >= min && v <= max) {
            self.bad(key, env, &format!("must be between {min} and {max}"));
            return default;
        }
        v
    }

    fn flag(&mut self, key: &str, env: &'static str, file: Option<bool>, default: bool) -> bool {
        match self.env(env) {
            Some(s) => match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => {
                    self.bad(key, env, "not a boolean (true/false)");
                    default
                }
            },
            None => file.unwrap_or(default),
        }
    }

    fn text(&self, env: &str, file: Option<String>) -> Option<String> {
        self.env(env)
            .map(str::to_string)
            .or_else(|| file.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
    }

    fn list(&self, env: &str, file: Option<Vec<String>>) -> Option<Vec<String>> {
        match self.env(env) {
            Some(s) => Some(
                s.split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            None => file,
        }
    }

    fn exts(
        &mut self,
        key: &str,
        env: &'static str,
        file: Option<Vec<String>>,
        default: &[&str],
    ) -> Vec<String> {
        let Some(raw) = self.list(env, file) else {
            return default.iter().map(|s| (*s).to_string()).collect();
        };
        let mut out: Vec<String> = Vec::new();
        for e in raw {
            let e = e.to_ascii_lowercase();
            let ok = e.len() >= 2
                && e.starts_with('.')
                && e[1..].bytes().all(|b| b.is_ascii_alphanumeric());
            if !ok {
                self.bad(
                    key,
                    env,
                    "extensions must be a dot followed by letters or digits",
                );
                return default.iter().map(|s| (*s).to_string()).collect();
            }
            if !out.contains(&e) {
                out.push(e);
            }
        }
        if out.is_empty() {
            self.bad(key, env, "extension list is empty");
        }
        out
    }

    fn url(&mut self, key: &str, env: &'static str, raw: &str) -> Option<Url> {
        let Ok(u) = Url::parse(raw) else {
            self.bad(key, env, "not a valid URL");
            return None;
        };
        if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
            self.bad(key, env, "must be an http or https URL with a host");
            return None;
        }
        if !u.username().is_empty() || u.password().is_some() {
            self.bad(key, env, "URL must not embed credentials");
            return None;
        }
        Some(u)
    }

    fn secret(&self, env: &str) -> Option<Secret> {
        self.env(env).map(Secret::new)
    }

    fn mappings(
        &mut self,
        service: ServiceKind,
        file: Option<Vec<MappingFile>>,
    ) -> Vec<(String, String)> {
        let env = env_name(service);
        let key = format!("path_mappings.{}", service.as_str());
        if let Some(raw) = self.env(env).map(str::to_string) {
            let mut out = Vec::new();
            for (i, entry) in raw
                .split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .enumerate()
            {
                match entry.split_once('=') {
                    Some((s, a)) => out.push((s.trim().to_string(), a.trim().to_string())),
                    None => self.bad(
                        &format!("{key}[{i}]"),
                        env,
                        "entry must be `service_prefix=app_path`",
                    ),
                }
            }
            return out;
        }
        file.unwrap_or_default()
            .into_iter()
            .map(|m| (m.service, m.app))
            .collect()
    }
}

fn parse_file(text: &str) -> Result<FileConfig, Problem> {
    toml::from_str::<FileConfig>(text).map_err(|e| {
        let line = e.span().map_or(0, |s| {
            text[..s.start.min(text.len())].matches('\n').count() + 1
        });
        let hint = if e.message().starts_with("unknown field") {
            e.message().to_string()
        } else {
            "syntax error or wrong value type".to_string()
        };
        Problem::invalid(
            "config_file",
            Some("REMOVARR_CONFIG_FILE"),
            format!("line {line}: {hint}"),
        )
    })
}

fn parse_owner(s: &str) -> Option<(u32, u32)> {
    let (u, g) = s.split_once(':')?;
    Some((u.parse().ok()?, g.parse().ok()?))
}

fn parse_anchor(s: &str) -> Option<AnchorUtc> {
    let b = s.as_bytes();
    if b.len() != 5
        || b[2] != b':'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 2 || c.is_ascii_digit())
    {
        return None;
    }
    let hour: u8 = s[..2].parse().ok()?;
    let minute: u8 = s[3..].parse().ok()?;
    (hour < 24 && minute < 60).then_some(AnchorUtc { hour, minute })
}

impl Config {
    /// Reads the process environment (`REMOVARR_*`, UTF-8 values) and the config file.
    pub fn load_from_process() -> Result<Self, ConfigError> {
        let env: Env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| k.starts_with("REMOVARR_"))
            .collect();
        Self::load_with_env(&env)
    }

    /// Resolves `REMOVARR_CONFIG_FILE` (default `/config/removarr.toml`, optional when not set
    /// explicitly), reads it, then calls [`Config::from_sources`].
    pub fn load_with_env(env: &Env) -> Result<Self, ConfigError> {
        let explicit = env
            .get(ENV_CONFIG_FILE)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let path = Path::new(explicit.unwrap_or(DEFAULT_CONFIG_FILE));
        let fail = |reason: &str| ConfigError {
            problems: vec![Problem::invalid(
                "config_file",
                Some(ENV_CONFIG_FILE),
                reason,
            )],
        };
        let text = match std::fs::metadata(path) {
            Ok(m) if m.is_file() && m.len() <= MAX_FILE_BYTES => {
                match std::fs::read_to_string(path) {
                    Ok(t) => Some(t),
                    Err(_) => return Err(fail("cannot be read as UTF-8 text")),
                }
            }
            Ok(_) => return Err(fail("is not a regular file or exceeds 1 MiB")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => None,
            Err(_) => return Err(fail("cannot be opened")),
        };
        Self::from_sources(text.as_deref(), env)
    }

    /// Pure merge of file text and environment, then validation (roots are checked on disk).
    pub fn from_sources(file_text: Option<&str>, env: &Env) -> Result<Self, ConfigError> {
        let mut l = L {
            env,
            problems: Vec::new(),
        };
        let file = match file_text.map(parse_file) {
            Some(Ok(f)) => f,
            Some(Err(p)) => return Err(ConfigError { problems: vec![p] }),
            None => FileConfig::default(),
        };

        // Roots and mappings.
        let media = l
            .list("REMOVARR_MEDIA_ROOTS", file.roots.media)
            .unwrap_or_default();
        let root_paths = RootPaths {
            completed: l.text("REMOVARR_COMPLETED_ROOT", file.roots.completed),
            incomplete: l.text("REMOVARR_INCOMPLETE_ROOT", file.roots.incomplete),
            media,
        };
        for (key, env_n, p) in [
            (
                "roots.completed",
                "REMOVARR_COMPLETED_ROOT",
                root_paths.completed.as_deref(),
            ),
            (
                "roots.incomplete",
                "REMOVARR_INCOMPLETE_ROOT",
                root_paths.incomplete.as_deref(),
            ),
        ] {
            if p.is_some_and(|p| p.contains([',', '='])) {
                l.bad(key, env_n, "paths must not contain `,` or `=`");
            }
        }
        if root_paths.media.iter().any(|p| p.contains([',', '='])) {
            l.bad(
                "roots.media",
                "REMOVARR_MEDIA_ROOTS",
                "paths must not contain `,` or `=`",
            );
        }
        let roots = match Roots::validate(&root_paths) {
            Ok(r) => Some(r),
            Err(p) => {
                l.problems.extend(p);
                None
            }
        };

        let q = file.services.qbittorrent;
        let external = l
            .list("REMOVARR_QBIT_EXTERNAL_PREFIXES", q.external_prefixes)
            .unwrap_or_default();
        let mut raw_rules: BTreeMap<ServiceKind, Vec<(String, String)>> = BTreeMap::new();
        for (svc, f) in [
            (ServiceKind::Qbittorrent, file.path_mappings.qbittorrent),
            (ServiceKind::Sonarr, file.path_mappings.sonarr),
            (ServiceKind::Radarr, file.path_mappings.radarr),
            (ServiceKind::Jellyfin, file.path_mappings.jellyfin),
        ] {
            let m = l.mappings(svc, f);
            raw_rules.insert(svc, m);
        }
        let mapper = roots
            .as_ref()
            .and_then(|r| match PathMapper::new(r, &raw_rules, &external) {
                Ok(m) => Some(m),
                Err(p) => {
                    l.problems.extend(p);
                    None
                }
            });

        // Server.
        let bind = match l.text("REMOVARR_BIND", file.server.bind) {
            Some(s) => s.parse::<SocketAddr>().unwrap_or_else(|_| {
                l.bad("server.bind", "REMOVARR_BIND", "not a valid socket address");
                SocketAddr::from(([0, 0, 0, 0], 8080))
            }),
            None => SocketAddr::from(([0, 0, 0, 0], 8080)),
        };
        let (raw, r) = (
            l.text("REMOVARR_DATA_DIR", file.server.data_dir)
                .unwrap_or_else(|| "/config".into()),
            roots.as_ref(),
        );
        let data_dir = match validate_data_dir(&raw, r) {
            Ok(p) => Some(p),
            Err(p) => {
                l.problems.extend(p);
                None
            }
        };
        let log_level = match l
            .text("REMOVARR_LOG_LEVEL", file.server.log_level)
            .as_deref()
        {
            None | Some("info") => LogLevel::Info,
            Some("debug") => LogLevel::Debug,
            Some("warn") => LogLevel::Warn,
            Some("error") => LogLevel::Error,
            Some(_) => {
                l.bad(
                    "server.log_level",
                    "REMOVARR_LOG_LEVEL",
                    "must be debug, info, warn or error",
                );
                LogLevel::Info
            }
        };
        let trusted_proxies = match l.list("REMOVARR_TRUSTED_PROXIES", file.server.trusted_proxies)
        {
            None => TrustedProxies::none(),
            Some(v) => TrustedProxies::parse(&v).unwrap_or_else(|why| {
                l.bad("server.trusted_proxies", "REMOVARR_TRUSTED_PROXIES", why);
                TrustedProxies::none()
            }),
        };
        let shutdown_grace_seconds = l.num(
            "server.shutdown_grace_seconds",
            "REMOVARR_SHUTDOWN_GRACE_SECONDS",
            file.server.shutdown_grace_seconds,
            45,
            5,
            300,
        );

        // Owner.
        let owner = match l.text("REMOVARR_EXPECTED_OWNER", file.owner.expected) {
            Some(s) => match parse_owner(&s) {
                Some((uid, gid)) => Owner {
                    uid,
                    gid,
                    from_process: false,
                },
                None => {
                    l.bad(
                        "owner.expected",
                        "REMOVARR_EXPECTED_OWNER",
                        "must be `uid:gid` (numbers)",
                    );
                    process_owner()
                }
            },
            None => process_owner(),
        };

        // Thresholds.
        let t = file.thresholds;
        let thresholds = Thresholds {
            incomplete_stalled_days: l.num(
                "thresholds.incomplete_stalled_days",
                "REMOVARR_INCOMPLETE_STALLED_DAYS",
                t.incomplete_stalled_days,
                14,
                1,
                3650,
            ),
            auto_min_age_hours: l.num(
                "thresholds.auto_min_age_hours",
                "REMOVARR_AUTO_MIN_AGE_HOURS",
                t.auto_min_age_hours,
                48,
                0,
                8760,
            ),
            hide_recent_days: l.num(
                "thresholds.hide_recent_days",
                "REMOVARR_HIDE_RECENT_DAYS",
                t.hide_recent_days,
                2,
                0,
                365,
            ),
            recheck_timeout_seconds: l.num(
                "thresholds.recheck_timeout_seconds",
                "REMOVARR_RECHECK_TIMEOUT_SECONDS",
                t.recheck_timeout_seconds,
                600,
                30,
                7200,
            ),
            plan_ttl_hours: l.num(
                "thresholds.plan_ttl_hours",
                "REMOVARR_PLAN_TTL_HOURS",
                t.plan_ttl_hours,
                24,
                1,
                168,
            ),
        };

        // File extensions.
        let files = FileRules {
            video_extensions: l.exts(
                "files.video_extensions",
                "REMOVARR_VIDEO_EXTENSIONS",
                file.files.video_extensions,
                DEFAULT_VIDEO,
            ),
            junk_extensions: l.exts(
                "files.junk_extensions",
                "REMOVARR_JUNK_EXTENSIONS",
                file.files.junk_extensions,
                DEFAULT_JUNK,
            ),
            subtitle_extensions: l.exts(
                "files.subtitle_extensions",
                "REMOVARR_SUBTITLE_EXTENSIONS",
                file.files.subtitle_extensions,
                DEFAULT_SUBTITLE,
            ),
        };
        let overlap = |a: &[String], b: &[String]| a.iter().any(|x| b.contains(x));
        if overlap(&files.junk_extensions, &files.video_extensions)
            || files.video_extensions.iter().any(|v| files.is_junk(v))
        {
            l.bad(
                "files.junk_extensions",
                "REMOVARR_JUNK_EXTENSIONS",
                "must not intersect video extensions",
            );
        }
        if overlap(&files.subtitle_extensions, &files.video_extensions)
            || overlap(&files.subtitle_extensions, &files.junk_extensions)
        {
            l.bad(
                "files.subtitle_extensions",
                "REMOVARR_SUBTITLE_EXTENSIONS",
                "must not intersect video or junk extensions",
            );
        }

        // Scan.
        let s = file.scan;
        let interval_minutes = l.num(
            "scan.interval_minutes",
            "REMOVARR_SCAN_INTERVAL_MINUTES",
            s.interval_minutes,
            1440,
            0,
            10080,
        );
        if interval_minutes != 0 && interval_minutes < 60 {
            l.bad(
                "scan.interval_minutes",
                "REMOVARR_SCAN_INTERVAL_MINUTES",
                "must be 0 (manual only) or at least 60",
            );
        }
        let qbit_refresh_minutes = l.num(
            "scan.qbit_refresh_minutes",
            "REMOVARR_QBIT_REFRESH_MINUTES",
            s.qbit_refresh_minutes,
            60,
            0,
            1440,
        );
        if qbit_refresh_minutes != 0 && qbit_refresh_minutes < 5 {
            l.bad(
                "scan.qbit_refresh_minutes",
                "REMOVARR_QBIT_REFRESH_MINUTES",
                "must be 0 (off) or at least 5",
            );
        }
        let anchor_utc = match l.text("REMOVARR_SCAN_ANCHOR_UTC", s.anchor_utc) {
            None => AnchorUtc { hour: 3, minute: 0 },
            Some(a) => parse_anchor(&a).unwrap_or_else(|| {
                l.bad(
                    "scan.anchor_utc",
                    "REMOVARR_SCAN_ANCHOR_UTC",
                    "must be HH:MM (UTC, 24 h)",
                );
                AnchorUtc { hour: 3, minute: 0 }
            }),
        };
        let scan = ScanConfig {
            interval_minutes,
            anchor_utc,
            qbit_refresh_minutes,
        };

        // Limits and matching.
        let li = file.limits;
        let limits = Limits {
            content_read_concurrency: l.num(
                "limits.content_read_concurrency",
                "REMOVARR_CONTENT_READ_CONCURRENCY",
                li.content_read_concurrency,
                1,
                1,
                4,
            ),
            ffprobe_concurrency: l.num(
                "limits.ffprobe_concurrency",
                "REMOVARR_FFPROBE_CONCURRENCY",
                li.ffprobe_concurrency,
                1,
                1,
                4,
            ),
            http_concurrency_per_service: l.num(
                "limits.http_concurrency_per_service",
                "REMOVARR_HTTP_CONCURRENCY",
                li.http_concurrency_per_service,
                4,
                1,
                16,
            ),
            max_response_bytes: l.num(
                "limits.max_response_bytes",
                "REMOVARR_MAX_RESPONSE_BYTES",
                li.max_response_bytes,
                32 << 20,
                1 << 20,
                512 << 20,
            ),
        };
        let m = file.matching;
        let ffprobe_path = l
            .text("REMOVARR_FFPROBE_PATH", m.ffprobe_path)
            .unwrap_or_else(|| "ffprobe".into());
        if ffprobe_path.contains('\0')
            || (ffprobe_path.contains('/') && !ffprobe_path.starts_with('/'))
        {
            l.bad(
                "matching.ffprobe_path",
                "REMOVARR_FFPROBE_PATH",
                "must be a bare command name or an absolute path",
            );
        }
        let matching = MatchingConfig {
            similarity_threshold: l.num(
                "matching.similarity_threshold",
                "REMOVARR_SIMILARITY_THRESHOLD",
                m.similarity_threshold,
                0.60,
                0.0,
                1.0,
            ),
            sample_size_bytes: l.num(
                "matching.sample_size_bytes",
                "REMOVARR_SAMPLE_SIZE_BYTES",
                m.sample_size_bytes,
                1 << 20,
                1,
                1 << 30,
            ),
            sample_count: l.num(
                "matching.sample_count",
                "REMOVARR_SAMPLE_COUNT",
                m.sample_count,
                5,
                1,
                1000,
            ),
            compare_chunk_bytes: l.num(
                "matching.compare_chunk_bytes",
                "REMOVARR_COMPARE_CHUNK_BYTES",
                m.compare_chunk_bytes,
                16 << 20,
                65_536,
                1 << 30,
            ),
            duration_tolerance_seconds: l.num(
                "matching.duration_tolerance_seconds",
                "REMOVARR_DURATION_TOLERANCE_SECONDS",
                m.duration_tolerance_seconds,
                5.0,
                0.0,
                86_400.0,
            ),
            duration_tolerance_ratio: l.num(
                "matching.duration_tolerance_ratio",
                "REMOVARR_DURATION_TOLERANCE_RATIO",
                m.duration_tolerance_ratio,
                0.002,
                0.0,
                1.0,
            ),
            ffprobe_path,
        };

        // Services and their secrets.
        let qbit = match l.text("REMOVARR_QBIT_URL", q.url) {
            None => None,
            Some(raw) => {
                let url = l.url("services.qbittorrent.url", "REMOVARR_QBIT_URL", &raw);
                let password = l.secret(ENV_QBIT_PASSWORD);
                if password.is_none() {
                    l.problems.push(Problem::missing(
                        "services.qbittorrent.password",
                        Some(ENV_QBIT_PASSWORD),
                    ));
                }
                let username = l
                    .text("REMOVARR_QBIT_USERNAME", q.username)
                    .unwrap_or_else(|| "admin".into());
                url.zip(password).map(|(url, password)| QbitService {
                    url,
                    username,
                    password,
                    external_prefixes: external.clone(),
                })
            }
        };
        let api = |l: &mut L<'_>,
                   name: &str,
                   env_url: &'static str,
                   env_key: &'static str,
                   file_url: Option<String>| {
            let raw = l.text(env_url, file_url)?;
            let url = l.url(&format!("services.{name}.url"), env_url, &raw);
            let key = l.secret(env_key);
            if key.is_none() {
                l.problems.push(Problem::missing(
                    format!("services.{name}.api_key"),
                    Some(env_key),
                ));
            }
            url.zip(key)
                .map(|(url, api_key)| ApiService { url, api_key })
        };
        let services = Services {
            qbittorrent: qbit,
            sonarr: api(
                &mut l,
                "sonarr",
                "REMOVARR_SONARR_URL",
                ENV_SONARR_API_KEY,
                file.services.sonarr.url,
            ),
            radarr: api(
                &mut l,
                "radarr",
                "REMOVARR_RADARR_URL",
                ENV_RADARR_API_KEY,
                file.services.radarr.url,
            ),
            jellyfin: api(
                &mut l,
                "jellyfin",
                "REMOVARR_JELLYFIN_URL",
                ENV_JELLYFIN_API_KEY,
                file.services.jellyfin.url,
            ),
        };

        // Mode and capabilities.
        let mode = Mode {
            dry_run: l.flag("mode.dry_run", "REMOVARR_DRY_RUN", file.mode.dry_run, true),
            auto_mode: l.flag(
                "mode.auto_mode",
                "REMOVARR_AUTO_MODE",
                file.mode.auto_mode,
                false,
            ),
        };
        let c = &file.capabilities;
        let mut caps = BTreeMap::new();
        for (cap, env_n) in ALL_CAPABILITIES {
            let from_file = match cap {
                Capability::Consolidate => c.consolidate,
                Capability::DeleteCompleted => c.delete_completed,
                Capability::CleanupIncomplete => c.cleanup_incomplete,
                Capability::FixPermissions => c.fix_permissions,
                Capability::ArrManualImport => c.arr_manual_import,
                Capability::ArrQueueRemove => c.arr_queue_remove,
                Capability::ArrSetMonitored => c.arr_set_monitored,
                Capability::ArrGrab => c.arr_grab,
                Capability::AutoRemoveEmptyTorrents => c.auto_remove_empty_torrents,
            };
            let name = cap.as_str();
            let v = l.flag(
                &format!("capabilities.{name}"),
                env_n,
                from_file,
                cap.default_enabled(),
            );
            caps.insert(cap, v);
        }
        let capabilities = Capabilities::from_map(caps);

        // Auth.
        let a = file.auth;
        let username = l
            .text("REMOVARR_USERNAME", a.username)
            .unwrap_or_else(|| "admin".into());
        let password_hash = match l.env(ENV_PASSWORD_HASH) {
            None => {
                l.problems.push(Problem::missing(
                    "auth.password_hash",
                    Some(ENV_PASSWORD_HASH),
                ));
                None
            }
            Some(h) => {
                // Compose files double `$`; a PHC string never contains an empty field.
                let h = h.replace("$$", "$");
                if h.starts_with("$argon2") {
                    Some(Secret::new(h))
                } else {
                    l.bad(
                        "auth.password_hash",
                        ENV_PASSWORD_HASH,
                        "must be an Argon2 PHC string",
                    );
                    None
                }
            }
        };
        let session_key = match l.env(ENV_SESSION_KEY) {
            None => {
                l.problems
                    .push(Problem::missing("auth.session_key", Some(ENV_SESSION_KEY)));
                None
            }
            Some(k) if k.chars().count() < 64 => {
                l.bad(
                    "auth.session_key",
                    ENV_SESSION_KEY,
                    "must be at least 64 characters",
                );
                None
            }
            Some(k) => Some(Secret::new(k)),
        };
        let session_ttl_hours = l.num(
            "auth.session_ttl_hours",
            "REMOVARR_SESSION_TTL_HOURS",
            a.session_ttl_hours,
            12,
            1,
            720,
        );
        let cookie_secure = l.flag(
            "auth.cookie_secure",
            "REMOVARR_COOKIE_SECURE",
            a.cookie_secure,
            true,
        );
        let login_limit_per_ip = l.num(
            "auth.login_limit_per_ip",
            "REMOVARR_LOGIN_LIMIT_PER_IP",
            a.login_limit_per_ip,
            20,
            1,
            1000,
        );
        let login_limit_per_ip_user = l.num(
            "auth.login_limit_per_ip_user",
            "REMOVARR_LOGIN_LIMIT_PER_IP_USER",
            a.login_limit_per_ip_user,
            5,
            1,
            1000,
        );

        let (Some(paths), Some(data_dir), Some(password_hash), Some(session_key)) =
            (mapper, data_dir, password_hash, session_key)
        else {
            return Err(ConfigError {
                problems: l.problems,
            });
        };
        if !l.problems.is_empty() {
            return Err(ConfigError {
                problems: l.problems,
            });
        }
        Ok(Config {
            server: ServerConfig {
                bind,
                data_dir,
                log_level,
                trusted_proxies,
                shutdown_grace_seconds,
            },
            paths,
            owner,
            thresholds,
            files,
            scan,
            limits,
            matching,
            services,
            mode,
            capabilities,
            auth: AuthConfig {
                username,
                password_hash,
                session_key,
                session_ttl_hours,
                cookie_secure,
                login_limit_per_ip,
                login_limit_per_ip_user,
            },
        })
    }
}

fn process_owner() -> Owner {
    Owner {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        from_process: true,
    }
}
