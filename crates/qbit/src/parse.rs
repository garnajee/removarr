//! JSON decoding and path candidates. Nothing here talks to the network.

use removarr_config::PathMapper;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentState};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionInfo {
    pub raw: String,
    pub major: u8,
}

fn rejected(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::Rejected, detail)
}

/// Accepts `v5.0.4`, `4.6.7`, `v4.6.0beta1`. Anything else (an HTML page, a banner) is refused:
/// an unknown server must not be driven.
pub(crate) fn parse_version(body: &[u8]) -> Result<VersionInfo, PortError> {
    let raw = std::str::from_utf8(body)
        .map_err(|_| rejected("qbittorrent version is not text"))?
        .trim();
    let digits = raw.trim_start_matches(['v', 'V']);
    let shape_ok = !raw.is_empty()
        && raw.len() <= 64
        && digits.starts_with(|c: char| c.is_ascii_digit())
        && digits
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'));
    let major = digits
        .split(['.', '-', '+', '_'])
        .next()
        .and_then(|m| m.parse::<u8>().ok());
    match (shape_ok, major) {
        (true, Some(major)) => Ok(VersionInfo {
            raw: raw.to_string(),
            major,
        }),
        _ => Err(rejected("unrecognized qbittorrent version string")),
    }
}

#[derive(Deserialize)]
struct RawTorrent {
    hash: String,
    #[serde(default)]
    name: String,
    state: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    added_on: i64,
    #[serde(default)]
    completion_on: i64,
    #[serde(default)]
    category: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    download_path: Option<String>,
    #[serde(default)]
    content_path: Option<String>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    index: Option<u32>,
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    priority: i32,
}

fn json_err(e: &serde_json::Error) -> PortError {
    // The serde message may echo values, so only the position is reported.
    rejected(format!(
        "invalid JSON from qbittorrent (line {}, column {})",
        e.line(),
        e.column()
    ))
}

fn map_required(paths: &PathMapper, raw: &str, what: &str) -> MappedPath {
    if raw.trim().is_empty() {
        MappedPath::Invalid {
            raw: String::new(),
            reason: format!("{what} is missing"),
        }
    } else {
        paths.to_app(ServiceKind::Qbittorrent, raw)
    }
}

pub(crate) fn parse_torrents(
    body: &[u8],
    paths: &PathMapper,
) -> Result<Vec<TorrentInfo>, PortError> {
    let raw: Vec<RawTorrent> = serde_json::from_slice(body).map_err(|e| json_err(&e))?;
    raw.into_iter()
        .map(|r| {
            let hash = TorrentHash::new(&r.hash)
                .map_err(|_| rejected("qbittorrent returned an invalid torrent hash"))?;
            let download_path = r
                .download_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .map(|p| paths.to_app(ServiceKind::Qbittorrent, p));
            Ok(TorrentInfo {
                hash,
                name: r.name,
                state: TorrentState::parse(&r.state),
                progress: r.progress,
                added_on: Timestamp(r.added_on.saturating_mul(1000)),
                completion_on: (r.completion_on > 0)
                    .then(|| Timestamp(r.completion_on.saturating_mul(1000))),
                category: r.category,
                size: u64::try_from(r.size).unwrap_or(0),
                save_path: map_required(paths, &r.save_path, "save_path"),
                download_path,
                content_path: map_required(
                    paths,
                    r.content_path.as_deref().unwrap_or(""),
                    "content_path",
                ),
            })
        })
        .collect()
}

/// A mapping error on any path of the torrent makes mutation on it uncertain.
pub(crate) fn info_mapping_uncertain(t: &TorrentInfo) -> bool {
    t.save_path.is_mapping_error()
        || t.content_path.is_mapping_error()
        || t.download_path
            .as_ref()
            .is_some_and(MappedPath::is_mapping_error)
}

fn dirname(p: &str) -> Option<&str> {
    let t = p.trim_end_matches('/');
    t.rsplit_once('/')
        .map(|(d, _)| if d.is_empty() { "/" } else { d })
}

fn join(base: &str, name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), name)
}

/// Every plausible location of one file: from `save_path`, `download_path`, then the directory
/// of `content_path`, each followed by its `.!qB` variant. Never derived from the torrent name
/// (the file name comes from the torrent's file list; matching is by path).
fn candidates(t: &TorrentInfo, name: &str, paths: &PathMapper) -> Vec<MappedPath> {
    let mut bases: Vec<String> = vec![t.save_path.raw().to_string()];
    if let Some(d) = &t.download_path {
        bases.push(d.raw().to_string());
    }
    if let Some(d) = dirname(t.content_path.raw()) {
        bases.push(d.to_string());
    }
    let mut out: Vec<MappedPath> = Vec::new();
    for base in bases.iter().filter(|b| !b.trim().is_empty()) {
        let plain = join(base, name);
        let working = format!("{plain}.!qB");
        for raw in [plain, working] {
            let m = paths.to_app(ServiceKind::Qbittorrent, &raw);
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    if out.is_empty() {
        out.push(MappedPath::Invalid {
            raw: String::new(),
            reason: "torrent has no usable base path".to_string(),
        });
    }
    out
}

pub(crate) fn parse_files(
    body: &[u8],
    info: &TorrentInfo,
    paths: &PathMapper,
) -> Result<Vec<TorrentFile>, PortError> {
    let raw: Vec<RawFile> = serde_json::from_slice(body).map_err(|e| json_err(&e))?;
    raw.into_iter()
        .enumerate()
        .map(|(i, f)| {
            let index = match f.index {
                Some(n) => n,
                None => u32::try_from(i).map_err(|_| rejected("too many files"))?,
            };
            Ok(TorrentFile {
                index,
                candidates: candidates(info, &f.name, paths),
                name: f.name,
                size: f.size,
                priority: f.priority,
                progress: f.progress,
            })
        })
        .collect()
}

pub(crate) fn files_mapping_uncertain(files: &[TorrentFile]) -> bool {
    files
        .iter()
        .any(|f| f.candidates.iter().any(MappedPath::is_mapping_error))
}
