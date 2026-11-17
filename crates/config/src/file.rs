//! Schema of the optional TOML file. No secret fields exist, and unknown keys are rejected.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    pub server: ServerFile,
    pub roots: RootsFile,
    pub path_mappings: PathMappingsFile,
    pub owner: OwnerFile,
    pub thresholds: ThresholdsFile,
    pub files: FilesFile,
    pub scan: ScanFile,
    pub limits: LimitsFile,
    pub matching: MatchingFile,
    pub services: ServicesFile,
    pub mode: ModeFile,
    pub capabilities: CapabilitiesFile,
    pub auth: AuthFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerFile {
    pub bind: Option<String>,
    pub data_dir: Option<String>,
    pub log_level: Option<String>,
    pub trusted_proxies: Option<Vec<String>>,
    pub shutdown_grace_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RootsFile {
    pub completed: Option<String>,
    pub incomplete: Option<String>,
    pub media: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingFile {
    pub service: String,
    pub app: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathMappingsFile {
    pub qbittorrent: Option<Vec<MappingFile>>,
    pub sonarr: Option<Vec<MappingFile>>,
    pub radarr: Option<Vec<MappingFile>>,
    pub jellyfin: Option<Vec<MappingFile>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OwnerFile {
    pub expected: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ThresholdsFile {
    pub incomplete_stalled_days: Option<u32>,
    pub auto_min_age_hours: Option<u32>,
    pub hide_recent_days: Option<u32>,
    pub recheck_timeout_seconds: Option<u32>,
    pub plan_ttl_hours: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FilesFile {
    pub video_extensions: Option<Vec<String>>,
    pub junk_extensions: Option<Vec<String>>,
    pub subtitle_extensions: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScanFile {
    pub interval_minutes: Option<u32>,
    pub anchor_utc: Option<String>,
    pub qbit_refresh_minutes: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsFile {
    pub content_read_concurrency: Option<u32>,
    pub ffprobe_concurrency: Option<u32>,
    pub http_concurrency_per_service: Option<u32>,
    pub max_response_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MatchingFile {
    pub similarity_threshold: Option<f64>,
    pub sample_size_bytes: Option<u64>,
    pub sample_count: Option<u32>,
    pub compare_chunk_bytes: Option<u64>,
    pub duration_tolerance_seconds: Option<f64>,
    pub duration_tolerance_ratio: Option<f64>,
    pub ffprobe_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QbitFile {
    pub url: Option<String>,
    pub username: Option<String>,
    pub external_prefixes: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UrlFile {
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServicesFile {
    pub qbittorrent: QbitFile,
    pub sonarr: UrlFile,
    pub radarr: UrlFile,
    pub jellyfin: UrlFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModeFile {
    pub dry_run: Option<bool>,
    pub auto_mode: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CapabilitiesFile {
    pub consolidate: Option<bool>,
    pub delete_completed: Option<bool>,
    pub cleanup_incomplete: Option<bool>,
    pub fix_permissions: Option<bool>,
    pub arr_manual_import: Option<bool>,
    pub arr_queue_remove: Option<bool>,
    pub arr_set_monitored: Option<bool>,
    pub arr_grab: Option<bool>,
    pub auto_remove_empty_torrents: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthFile {
    pub username: Option<String>,
    pub session_ttl_hours: Option<u32>,
    pub cookie_secure: Option<bool>,
    pub login_limit_per_ip: Option<u32>,
    pub login_limit_per_ip_user: Option<u32>,
}
