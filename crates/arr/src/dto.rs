//! Sonarr/Radarr v3 response shapes. Only the fields the application uses are declared; serde
//! ignores the rest. Download URLs, magnet links, info URLs and history `data` blobs (which can
//! carry tracker passkeys) are deliberately never deserialized (INV-31).

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SystemStatus {
    pub app_name: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeriesDto {
    pub id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub tvdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<i64>,
    pub tv_maze_id: Option<i64>,
    #[serde(default)]
    pub monitored: bool,
    pub quality_profile_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EpisodeDto {
    pub id: i64,
    pub series_id: i64,
    pub season_number: i32,
    pub episode_number: i32,
    pub absolute_episode_number: Option<i32>,
    pub scene_absolute_episode_number: Option<i32>,
    pub scene_season_number: Option<i32>,
    pub scene_episode_number: Option<i32>,
    #[serde(default)]
    pub has_file: bool,
    pub episode_file_id: Option<i64>,
    #[serde(default)]
    pub monitored: bool,
    pub air_date_utc: Option<String>,
}

impl EpisodeDto {
    /// ARR uses 0 for "no file".
    pub fn file_id(&self) -> Option<i64> {
        self.episode_file_id.filter(|id| *id > 0)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QualityWrapper {
    pub quality: Option<Named>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Named {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IdOnly {
    pub id: i64,
}

/// Episode file (Sonarr) or movie file (Radarr).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileDto {
    pub id: i64,
    pub path: Option<String>,
    #[serde(default)]
    pub size: u64,
    pub quality: Option<QualityWrapper>,
    #[serde(default)]
    pub custom_formats: Vec<Named>,
    pub custom_format_score: Option<i64>,
    pub quality_cutoff_not_met: Option<bool>,
    #[serde(default)]
    pub languages: Vec<Named>,
    /// Sonarr v3 has a single language.
    pub language: Option<Named>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MovieDto {
    pub id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub monitored: bool,
    pub quality_profile_id: i64,
    pub movie_file: Option<FileDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileDto {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub upgrade_allowed: bool,
    pub cutoff: i64,
    #[serde(default)]
    pub items: Vec<ProfileItemDto>,
    pub min_format_score: Option<i64>,
    pub cutoff_format_score: Option<i64>,
    #[serde(default)]
    pub format_items: Vec<FormatItemDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProfileItemDto {
    pub id: Option<i64>,
    pub name: Option<String>,
    pub quality: Option<QualityIdName>,
    #[serde(default)]
    pub items: Vec<ProfileItemDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QualityIdName {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FormatItemDto {
    pub format: i64,
    pub name: Option<String>,
    pub score: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MediaManagementDto {
    pub recycle_bin: Option<String>,
    pub recycle_bin_cleanup_days: Option<i64>,
    pub copy_using_hardlinks: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Paged<T> {
    /// Missing means unknown: paging then continues until a short page.
    pub total_records: Option<u64>,
    #[serde(default = "Vec::new")]
    pub records: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueDto {
    pub id: i64,
    pub download_id: Option<String>,
    pub series_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub movie_id: Option<i64>,
    pub tracked_download_status: Option<String>,
    pub tracked_download_state: Option<String>,
    #[serde(default)]
    pub status_messages: Vec<StatusMessageDto>,
    pub error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusMessageDto {
    pub title: Option<String>,
    #[serde(default)]
    pub messages: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryDto {
    pub download_id: Option<String>,
    pub series_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub movie_id: Option<i64>,
    pub event_type: Value,
    pub source_title: Option<String>,
    pub date: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManualImportDto {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    pub series: Option<IdOnly>,
    pub movie: Option<IdOnly>,
    #[serde(default)]
    pub episodes: Vec<IdOnly>,
    /// ARR's own quality object, passed back verbatim on import.
    pub quality: Option<Value>,
    pub languages: Option<Value>,
    pub language: Option<Value>,
    pub release_group: Option<String>,
    pub download_id: Option<String>,
    #[serde(default)]
    pub custom_formats: Vec<Named>,
    pub custom_format_score: Option<i64>,
    #[serde(default)]
    pub rejections: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReleaseDto {
    pub guid: String,
    pub indexer_id: i64,
    pub indexer: Option<String>,
    pub title: String,
    #[serde(default)]
    pub size: u64,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub age: Option<i64>,
    pub protocol: Option<Value>,
    pub quality: Option<QualityWrapper>,
    #[serde(default)]
    pub custom_formats: Vec<Named>,
    pub custom_format_score: Option<i64>,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub rejections: Vec<Value>,
    #[serde(default)]
    pub download_allowed: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CommandDto {
    pub id: i64,
    #[serde(default)]
    pub status: String,
    pub message: Option<String>,
    pub result: Option<String>,
}

pub(crate) fn quality_name(q: Option<&QualityWrapper>) -> String {
    q.and_then(|w| w.quality.as_ref())
        .map(|n| n.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}

pub(crate) fn names(v: &[Named]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|n| n.name.clone()).collect();
    out.sort();
    out.dedup();
    out
}

/// A rejection is a string (release search) or `{ reason, type }` (manual import).
pub(crate) fn rejection_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => match o.get("reason") {
            Some(Value::String(r)) => r.clone(),
            _ => "rejected (no reason given)".to_string(),
        },
        other => other.to_string(),
    }
}

/// String form of a JSON scalar (ARR versions encode some enums as strings or numbers).
pub(crate) fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
