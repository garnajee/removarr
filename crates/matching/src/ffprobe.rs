//! Bounded `ffprobe` adapter implementing `MediaProbe` (architecture 2.1, 8.4; API-12, INV-50).
//!
//! - argv only, never a shell, with a cleared environment (only a minimal `PATH`); the input is passed as `file:<absolute path>` with
//!   `-protocol_whitelist file`, so a crafted file cannot make ffprobe open network URLs;
//! - the path is built from a configured root and a validated `RelPath`; every component is
//!   checked with `symlink_metadata` (no symlink, final entry a regular file);
//! - concurrency bounded by a semaphore (`ffprobe_concurrency`, 1..=4), a timeout per call with
//!   `kill_on_drop`, stdout capped (4 MiB by default), stderr drained and truncated;
//! - errors never contain the absolute host path (the rooted display is used instead).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::IoKind;
use removarr_domain::media::{AudioStream, ProbeReport, SubtitleStream, VideoStream};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::ports::MediaProbe;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// The only environment variable the child gets (a bare `ffprobe` is resolved through it).
const CHILD_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
use tokio::sync::Semaphore;

/// Settings filled by `app` from the configuration (`matching.ffprobe_path`,
/// `limits.ffprobe_concurrency`, root paths).
#[derive(Debug, Clone)]
pub struct FfprobeSettings {
    pub program: PathBuf,
    pub timeout: Duration,
    pub root_paths: BTreeMap<RootId, PathBuf>,
    pub concurrency: usize,
    pub max_output_bytes: usize,
}

impl FfprobeSettings {
    /// Architecture defaults: 120 s timeout, 4 MiB of output, one process at a time.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, root_paths: BTreeMap<RootId, PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: Duration::from_secs(120),
            root_paths,
            concurrency: 1,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }
}

/// `MediaProbe` backed by the ffprobe binary.
#[derive(Debug, Clone)]
pub struct FfprobeProbe {
    settings: Arc<FfprobeSettings>,
    permits: Arc<Semaphore>,
}

const STDERR_KEEP: usize = 4096;

impl FfprobeProbe {
    #[must_use]
    pub fn new(settings: FfprobeSettings) -> Self {
        let n = settings.concurrency.clamp(1, 4);
        Self {
            settings: Arc::new(settings),
            permits: Arc::new(Semaphore::new(n)),
        }
    }

    /// Runs `ffprobe -version` once (startup or first use). `Config` when the program cannot be
    /// started, so `app` can report a service-state problem instead of failing later.
    pub async fn check_available(&self) -> Result<String, PortError> {
        let out = self
            .run(vec!["-version".into()], "ffprobe -version", None)
            .await?;
        let text = String::from_utf8_lossy(&out);
        Ok(text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect())
    }

    fn resolve(&self, path: &RootedPath) -> Result<PathBuf, PortError> {
        let root = self.settings.root_paths.get(&path.root).ok_or_else(|| {
            PortError::new(
                ErrorClass::Confinement,
                format!("root not configured for {path}"),
            )
        })?;
        let mut abs = root.clone();
        for c in path.rel.components() {
            abs.push(c);
        }
        Ok(abs)
    }

    async fn run(
        &self,
        args: Vec<OsString>,
        display: &str,
        hide: Option<&Path>,
    ) -> Result<Vec<u8>, PortError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| PortError::new(ErrorClass::Internal, "ffprobe limiter closed"))?;
        let mut child = Command::new(&self.settings.program)
            .args(&args)
            // The child never sees the process environment (REMOVARR secrets, tokens).
            .env_clear()
            .env("PATH", CHILD_PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                PortError::new(
                    if e.kind() == std::io::ErrorKind::NotFound {
                        ErrorClass::Config
                    } else {
                        ErrorClass::Unavailable
                    },
                    format!("cannot start ffprobe: {}", e.kind()),
                )
            })?;
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            return Err(PortError::new(
                ErrorClass::Internal,
                "ffprobe pipes missing",
            ));
        };
        let cap = self.settings.max_output_bytes;
        let child_ref = &mut child;
        let work = async {
            let out = async {
                let r = read_capped(&mut stdout, cap, false).await;
                if matches!(r, Ok((_, true))) {
                    // Stop the producer at once so stderr reaches EOF.
                    let _ = child_ref.start_kill();
                }
                r
            };
            tokio::join!(out, read_capped(&mut stderr, STDERR_KEEP, true))
        };
        let Ok((out, err)) = tokio::time::timeout(self.settings.timeout, work).await else {
            let _ = child.start_kill();
            return Err(PortError::new(
                ErrorClass::Rejected,
                format!("ffprobe timed out for {display}"),
            ));
        };
        let (out, overflow) = out.map_err(|e| io_error(&e, display))?;
        if overflow {
            let _ = child.start_kill();
            return Err(PortError::new(
                ErrorClass::Rejected,
                format!("ffprobe output exceeds {cap} bytes for {display}"),
            ));
        }
        let status = tokio::time::timeout(self.settings.timeout, child.wait())
            .await
            .map_err(|_| {
                PortError::new(
                    ErrorClass::Rejected,
                    format!("ffprobe timed out for {display}"),
                )
            })?
            .map_err(|e| io_error(&e, display))?;
        if !status.success() {
            let (err, _) = err.unwrap_or_default();
            return Err(PortError::new(
                ErrorClass::Rejected,
                format!(
                    "ffprobe failed for {display} ({status}): {}",
                    sanitize(&err, hide, display)
                ),
            ));
        }
        Ok(out)
    }
}

fn io_kind(k: std::io::ErrorKind) -> IoKind {
    match k {
        std::io::ErrorKind::NotFound => IoKind::NotFound,
        std::io::ErrorKind::PermissionDenied => IoKind::PermissionDenied,
        std::io::ErrorKind::NotADirectory => IoKind::NotADirectory,
        _ => IoKind::Other,
    }
}

fn io_error(e: &std::io::Error, display: &str) -> PortError {
    PortError::new(
        ErrorClass::Unavailable,
        format!("ffprobe I/O error for {display}: {}", e.kind()),
    )
}

/// Reads up to `cap` bytes. With `drain`, keeps reading (and discarding) so the child never
/// blocks on a full pipe; without it, stops and reports an overflow.
async fn read_capped<R: AsyncRead + Unpin>(
    r: &mut R,
    cap: usize,
    drain: bool,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok((buf, false));
        }
        let room = cap.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..n.min(room)]);
        if n > room && !drain {
            return Ok((buf, true));
        }
    }
}

/// First line of stderr, host path replaced by the rooted display, at most 200 characters.
fn sanitize(stderr: &[u8], hide: Option<&Path>, display: &str) -> String {
    let mut text = String::from_utf8_lossy(stderr)
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    if let Some(p) = hide.and_then(Path::to_str) {
        text = text
            .replace(&format!("file:{p}"), display)
            .replace(p, display);
    }
    text.chars().take(200).collect()
}

/// Every component under the root is checked without following symlinks; the final entry must
/// be a regular file. (A race after this check is possible but ffprobe only reads.)
async fn check_no_symlink(root: &Path, abs: &Path, display: &str) -> Result<(), PortError> {
    let rel = abs.strip_prefix(root).map_err(|_| {
        PortError::new(
            ErrorClass::Confinement,
            format!("{display} escapes its root"),
        )
    })?;
    let mut cur = root.to_path_buf();
    let n = rel.components().count();
    for (i, c) in rel.components().enumerate() {
        cur.push(c);
        let md = tokio::fs::symlink_metadata(&cur).await.map_err(|e| {
            PortError::new(
                ErrorClass::Io(io_kind(e.kind())),
                format!("cannot stat {display}: {}", e.kind()),
            )
        })?;
        if md.file_type().is_symlink() {
            return Err(PortError::new(
                ErrorClass::Confinement,
                format!("refusing to probe through a symlink: {display}"),
            ));
        }
        if i + 1 == n && !md.is_file() {
            return Err(PortError::new(
                ErrorClass::Rejected,
                format!("not a regular file: {display}"),
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl MediaProbe for FfprobeProbe {
    async fn probe(&self, path: &RootedPath) -> Result<ProbeReport, PortError> {
        let display = path.to_string();
        let abs = self.resolve(path)?;
        let root = self
            .settings
            .root_paths
            .get(&path.root)
            .cloned()
            .unwrap_or_default();
        check_no_symlink(&root, &abs, &display).await?;
        let mut input = OsString::from("file:");
        input.push(abs.as_os_str());
        let args: Vec<OsString> = [
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-print_format",
            "json",
            "-show_entries",
            "format=duration:stream=codec_type,codec_name,width,height,channels:stream_tags=language:stream_disposition=attached_pic",
        ]
        .iter()
        .map(OsString::from)
        .chain(std::iter::once(input))
        .collect();
        let out = self.run(args, &display, Some(&abs)).await?;
        parse_report(&out).map_err(|e| {
            PortError::new(
                ErrorClass::Rejected,
                format!("unreadable ffprobe output for {display}: {e}"),
            )
        })
    }
}

#[derive(Deserialize)]
struct RawOutput {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    disposition: BTreeMap<String, i64>,
}

impl RawStream {
    fn language(&self) -> Option<String> {
        self.tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("language"))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
    }
    fn codec(&self) -> String {
        self.codec_name.clone().unwrap_or_else(|| "unknown".into())
    }
}

/// Parses ffprobe JSON. A missing, unparsable, non-finite or non-positive duration is `None`
/// (unknown, never assumed equal). Cover art (`attached_pic`) is not a video stream.
pub fn parse_report(json: &[u8]) -> Result<ProbeReport, serde_json::Error> {
    let raw: RawOutput = serde_json::from_slice(json)?;
    let duration_s = raw
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);
    let mut report = ProbeReport {
        duration_s,
        video: vec![],
        audio: vec![],
        subtitles: vec![],
    };
    for s in raw.streams {
        match s.codec_type.as_deref() {
            Some("video") if s.disposition.get("attached_pic").copied().unwrap_or(0) == 0 => {
                report.video.push(VideoStream {
                    codec: s.codec(),
                    width: s.width,
                    height: s.height,
                });
            }
            Some("audio") => report.audio.push(AudioStream {
                codec: s.codec(),
                language: s.language(),
                channels: s.channels,
            }),
            Some("subtitle") => report.subtitles.push(SubtitleStream {
                codec: s.codec(),
                language: s.language(),
            }),
            _ => {}
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tracks_languages_and_ignores_cover_art() {
        let json = br#"{
          "streams": [
            {"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"disposition":{"attached_pic":0}},
            {"codec_type":"video","codec_name":"mjpeg","width":600,"height":900,"disposition":{"attached_pic":1}},
            {"codec_type":"audio","codec_name":"eac3","channels":6,"tags":{"language":"fre"}},
            {"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"LANGUAGE":"ENG"}},
            {"codec_type":"audio","codec_name":"aac","channels":2},
            {"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"fre"}},
            {"codec_type":"data"}
          ],
          "format": {"duration":"7200.040000"}
        }"#;
        let r = parse_report(json).unwrap();
        assert_eq!(r.duration_s, Some(7200.04));
        assert_eq!(r.video.len(), 1);
        assert_eq!(r.video[0].height, Some(1080));
        let langs: Vec<_> = r.audio.iter().map(|a| a.language.clone()).collect();
        assert_eq!(langs, [Some("fre".into()), Some("eng".into()), None]);
        assert_eq!(r.audio[0].channels, Some(6));
        assert_eq!(r.subtitles.len(), 1);
    }

    #[test]
    fn bad_durations_are_unknown_and_bad_json_is_an_error() {
        for d in [r#""N/A""#, r#""-1""#, r#""0""#, r#""inf""#, "null"] {
            let json = format!(r#"{{"streams":[],"format":{{"duration":{d}}}}}"#);
            assert_eq!(
                parse_report(json.as_bytes()).unwrap().duration_s,
                None,
                "{d}"
            );
        }
        assert_eq!(parse_report(b"{}").unwrap().duration_s, None);
        assert!(parse_report(b"not json").is_err());
    }

    #[test]
    fn sanitize_hides_the_host_path() {
        let p = Path::new("/srv/media/films/Movie.mkv");
        let msg = b"\nfile:/srv/media/films/Movie.mkv: Invalid data found when processing input\n";
        let s = sanitize(msg, Some(p), "media-1:films/Movie.mkv");
        assert_eq!(
            s,
            "media-1:films/Movie.mkv: Invalid data found when processing input"
        );
        assert!(!s.contains("/srv"));
    }
}
