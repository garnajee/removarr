//! Structured logging: a `tracing` layer that writes sanitized JSON lines to stdout (or any
//! writer) and keeps a bounded, sanitized ring buffer for the UI (architecture 10.2).
//!
//! Levels are `DEBUG < INFO < WARN < ERROR` (`TRACE` events are dropped). The capture level is
//! runtime configurable; the UI view filters again by level and substring. Every message,
//! field name and field value is redacted before it is stored or written, and every size is
//! bounded.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::redact::{REDACTED, Redactor, is_sensitive_key, truncate};
use crate::timefmt::iso;

/// Ring buffer capacity of the architecture.
pub const DEFAULT_CAPACITY: usize = 2000;
const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_FIELDS: usize = 16;
const MAX_FIELD_CHARS: usize = 512;
const MAX_TARGET_CHARS: usize = 128;
/// Largest page the UI view returns.
pub const MAX_VIEW_ENTRIES: usize = 500;
const MAX_QUERY_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
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
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => LogLevel::Debug,
            1 => LogLevel::Info,
            2 => LogLevel::Warn,
            _ => LogLevel::Error,
        }
    }

    fn from_tracing(l: &tracing::Level) -> Option<Self> {
        match *l {
            tracing::Level::TRACE => None,
            tracing::Level::DEBUG => Some(LogLevel::Debug),
            tracing::Level::INFO => Some(LogLevel::Info),
            tracing::Level::WARN => Some(LogLevel::Warn),
            tracing::Level::ERROR => Some(LogLevel::Error),
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" | "trace" => Ok(LogLevel::Debug),
            "info" => Ok(LogLevel::Info),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "error" => Ok(LogLevel::Error),
            _ => Err(()),
        }
    }
}

/// One stored log event. `at` is the exact UTC time with milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub at_ms: i64,
    pub at: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

/// Query of the UI view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    pub min_level: LogLevel,
    /// Case-insensitive substring over message, target and field values.
    pub q: Option<String>,
    /// Only entries with `seq` greater than this (incremental polling).
    pub after_seq: Option<u64>,
    pub limit: usize,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Info,
            q: None,
            after_seq: None,
            limit: 200,
        }
    }
}

/// View model of `/logs`. Newest entries last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogsVm {
    pub entries: Vec<LogEntry>,
    /// Entries matching the filter before the page limit.
    pub matched: usize,
    pub buffered: usize,
    pub capacity: usize,
    /// Events pushed out of the ring since start (oldest first eviction).
    pub dropped: u64,
    pub capture_level: LogLevel,
    pub min_level: LogLevel,
    pub truncated: bool,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

struct Inner {
    capacity: usize,
    ring: Mutex<VecDeque<LogEntry>>,
    next_seq: AtomicU64,
    dropped: AtomicU64,
    capture: AtomicU8,
    redactor: Redactor,
    clock: Clock,
}

/// Bounded sanitized in-memory log view. Cheap to clone.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Inner>,
}

impl fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogBuffer")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    })
}

impl LogBuffer {
    /// `capacity` is clamped to at least 1.
    #[must_use]
    pub fn new(capacity: usize, capture: LogLevel) -> Self {
        Self::with_clock(capacity, capture, system_clock())
    }

    #[must_use]
    pub fn with_clock(capacity: usize, capture: LogLevel, clock: Clock) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Inner {
                capacity,
                ring: Mutex::new(VecDeque::with_capacity(capacity.min(DEFAULT_CAPACITY))),
                next_seq: AtomicU64::new(1),
                dropped: AtomicU64::new(0),
                capture: AtomicU8::new(capture as u8),
                redactor: Redactor::new(),
                clock,
            }),
        }
    }

    /// Redactor shared with the layer: register configured secret values here at startup.
    #[must_use]
    pub fn redactor(&self) -> &Redactor {
        &self.inner.redactor
    }

    #[must_use]
    pub fn capture_level(&self) -> LogLevel {
        LogLevel::from_u8(self.inner.capture.load(Ordering::Relaxed))
    }

    /// Events below this level are neither stored nor written to stdout.
    pub fn set_capture_level(&self, level: LogLevel) {
        self.inner.capture.store(level as u8, Ordering::Relaxed);
    }

    /// Sanitizes, bounds and stores one event. Returns it (for the stdout sink), or `None`
    /// when it is below the capture level.
    pub fn push(
        &self,
        level: LogLevel,
        target: &str,
        message: &str,
        fields: Vec<(String, String)>,
    ) -> Option<LogEntry> {
        if level < self.capture_level() {
            return None;
        }
        let r = &self.inner.redactor;
        let mut map = BTreeMap::new();
        for (k, v) in fields.into_iter().take(MAX_FIELDS) {
            let k = truncate(&k, 64);
            let v = if is_sensitive_key(&k) {
                REDACTED.to_string()
            } else {
                truncate(&r.sanitize(&v), MAX_FIELD_CHARS)
            };
            map.insert(k, v);
        }
        let at_ms = (self.inner.clock)();
        let entry = LogEntry {
            seq: self.inner.next_seq.fetch_add(1, Ordering::Relaxed),
            at_ms,
            at: iso(at_ms),
            level,
            target: truncate(&r.sanitize(target), MAX_TARGET_CHARS),
            message: truncate(&r.sanitize(message), MAX_MESSAGE_CHARS),
            fields: map,
        };
        let mut ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len() >= self.inner.capacity {
            ring.pop_front();
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(entry.clone());
        Some(entry)
    }

    /// Bounded, filtered view for the UI. Never returns more than [`MAX_VIEW_ENTRIES`].
    #[must_use]
    pub fn view(&self, q: &LogQuery) -> LogsVm {
        let needle =
            q.q.as_deref()
                .map(|s| truncate(s.trim(), MAX_QUERY_CHARS).to_lowercase())
                .filter(|s| !s.is_empty());
        let limit = q.limit.clamp(1, MAX_VIEW_ENTRIES);
        let ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let buffered = ring.len();
        let mut matched: Vec<&LogEntry> = ring
            .iter()
            .filter(|e| e.level >= q.min_level)
            .filter(|e| q.after_seq.is_none_or(|s| e.seq > s))
            .filter(|e| {
                needle.as_deref().is_none_or(|n| {
                    e.message.to_lowercase().contains(n)
                        || e.target.to_lowercase().contains(n)
                        || e.fields.values().any(|v| v.to_lowercase().contains(n))
                })
            })
            .collect();
        let total = matched.len();
        let skip = total.saturating_sub(limit);
        let entries: Vec<LogEntry> = matched.drain(skip..).cloned().collect();
        LogsVm {
            entries,
            matched: total,
            buffered,
            capacity: self.inner.capacity,
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            capture_level: self.capture_level(),
            min_level: q.min_level,
            truncated: total > limit,
        }
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl FieldVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else if self.fields.len() < MAX_FIELDS {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

struct SinkBox(Box<dyn Write + Send>);

impl fmt::Debug for SinkBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SinkBox")
    }
}

type Sink = Arc<Mutex<SinkBox>>;

/// `tracing` layer: capture into the buffer and stream JSON lines to the sink.
#[derive(Debug)]
pub struct LogLayer {
    buffer: LogBuffer,
    sink: Option<Sink>,
}

impl LogLayer {
    /// Buffer only (tests, or when stdout is handled elsewhere).
    #[must_use]
    pub fn buffer_only(buffer: LogBuffer) -> Self {
        Self { buffer, sink: None }
    }

    /// Buffer plus JSON lines on stdout.
    #[must_use]
    pub fn stdout(buffer: LogBuffer) -> Self {
        Self::with_writer(buffer, Box::new(std::io::stdout()))
    }

    #[must_use]
    pub fn with_writer(buffer: LogBuffer, writer: Box<dyn Write + Send>) -> Self {
        Self {
            buffer,
            sink: Some(Arc::new(Mutex::new(SinkBox(writer)))),
        }
    }
}

/// One JSON line (no trailing newline) of an already sanitized entry.
#[must_use]
pub fn json_line(e: &LogEntry) -> String {
    serde_json::json!({
        "ts": e.at,
        "level": e.level.as_str(),
        "target": e.target,
        "message": e.message,
        "fields": e.fields,
    })
    .to_string()
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let Some(level) = LogLevel::from_tracing(meta.level()) else {
            return;
        };
        if level < self.buffer.capture_level() {
            return;
        }
        let mut v = FieldVisitor::default();
        event.record(&mut v);
        let Some(entry) = self.buffer.push(
            level,
            meta.target(),
            &v.message.unwrap_or_default(),
            v.fields,
        ) else {
            return;
        };
        if let Some(sink) = &self.sink {
            let mut line = json_line(&entry);
            line.push('\n');
            let mut w = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A broken stdout must never take the application down.
            let _ = w.0.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn buf(cap: usize, level: LogLevel) -> LogBuffer {
        LogBuffer::with_clock(cap, level, Arc::new(|| 1_700_000_000_000))
    }

    #[test]
    fn levels_parse_and_order() {
        assert_eq!("warning".parse::<LogLevel>(), Ok(LogLevel::Warn));
        assert_eq!("ERROR".parse::<LogLevel>(), Ok(LogLevel::Error));
        assert!("loud".parse::<LogLevel>().is_err());
        assert!(LogLevel::Debug < LogLevel::Info && LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn ring_is_bounded_and_counts_drops() {
        let b = buf(3, LogLevel::Debug);
        for i in 0..10 {
            b.push(LogLevel::Info, "t", &format!("m{i}"), vec![]);
        }
        let v = b.view(&LogQuery {
            min_level: LogLevel::Debug,
            limit: 100,
            ..LogQuery::default()
        });
        assert_eq!(v.buffered, 3);
        assert_eq!(v.dropped, 7);
        assert_eq!(
            v.entries
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>(),
            ["m7", "m8", "m9"]
        );
    }

    #[test]
    fn view_filters_by_level_text_and_page() {
        let b = buf(100, LogLevel::Debug);
        b.push(LogLevel::Debug, "scan", "per file detail", vec![]);
        b.push(LogLevel::Info, "scan", "session done", vec![]);
        b.push(
            LogLevel::Warn,
            "qbit",
            "slow response",
            vec![("job".into(), "J1".into())],
        );
        b.push(LogLevel::Error, "arr", "failed", vec![]);
        let q = |l, s: Option<&str>| LogQuery {
            min_level: l,
            q: s.map(str::to_string),
            after_seq: None,
            limit: 100,
        };
        assert_eq!(b.view(&q(LogLevel::Debug, None)).entries.len(), 4);
        assert_eq!(b.view(&q(LogLevel::Info, None)).entries.len(), 3);
        assert_eq!(b.view(&q(LogLevel::Warn, None)).entries.len(), 2);
        assert_eq!(b.view(&q(LogLevel::Error, None)).entries.len(), 1);
        assert_eq!(b.view(&q(LogLevel::Debug, Some("QBIT"))).entries.len(), 1);
        assert_eq!(b.view(&q(LogLevel::Debug, Some("j1"))).entries.len(), 1);
        let v = b.view(&LogQuery {
            min_level: LogLevel::Debug,
            q: None,
            after_seq: Some(2),
            limit: 1,
        });
        assert_eq!((v.matched, v.entries.len(), v.truncated), (2, 1, true));
        assert_eq!(v.entries[0].message, "failed");
    }

    #[test]
    fn view_limit_is_capped() {
        let b = buf(2000, LogLevel::Debug);
        for _ in 0..800 {
            b.push(LogLevel::Info, "t", "m", vec![]);
        }
        let v = b.view(&LogQuery {
            limit: 100_000,
            ..LogQuery::default()
        });
        assert_eq!(v.entries.len(), MAX_VIEW_ENTRIES);
    }

    #[test]
    fn capture_level_is_configurable_at_runtime() {
        let b = buf(10, LogLevel::Warn);
        assert!(b.push(LogLevel::Info, "t", "dropped", vec![]).is_none());
        b.set_capture_level(LogLevel::Debug);
        assert!(b.push(LogLevel::Debug, "t", "kept", vec![]).is_some());
    }

    #[test]
    fn sizes_are_bounded_and_secrets_never_stored() {
        let b = buf(10, LogLevel::Debug);
        b.redactor().add_secret("topsecretvalue");
        let long = "x".repeat(10_000);
        let e = b
            .push(
                LogLevel::Error,
                "t",
                &format!("{long} topsecretvalue http://tracker/announce?passkey=p Bearer abc"),
                (0..40)
                    .map(|i| (format!("f{i}"), "v".repeat(2000)))
                    .chain([
                        ("api_key".to_string(), "zzz".to_string()),
                        ("note".to_string(), "see magnet:?xt=1".to_string()),
                    ])
                    .collect(),
            )
            .unwrap();
        assert!(e.message.chars().count() <= MAX_MESSAGE_CHARS + 1);
        assert!(e.fields.len() <= MAX_FIELDS);
        let all = serde_json::to_string(&e).unwrap();
        for bad in ["topsecretvalue", "tracker", "passkey=p", "zzz", "magnet"] {
            assert!(!all.contains(bad), "{bad}");
        }
    }

    #[test]
    fn secrets_beyond_the_truncation_point_cannot_survive_a_cut() {
        let b = buf(10, LogLevel::Debug);
        b.redactor().add_secret("topsecretvalue");
        let msg = format!("{}topsecretvalue", "a ".repeat(MAX_MESSAGE_CHARS / 2 - 3));
        let e = b.push(LogLevel::Info, "t", &msg, vec![]).unwrap();
        assert!(!e.message.contains("topsecret"));
    }

    #[test]
    fn layer_streams_json_lines_and_fills_the_buffer() {
        let b = buf(10, LogLevel::Info);
        let cap = Capture::default();
        let layer = LogLayer::with_writer(b.clone(), Box::new(cap.clone()));
        let sub = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(sub, || {
            tracing::debug!("below capture");
            tracing::info!(job = "J9", "scan started");
            tracing::warn!(
                password = "hunter2",
                "bad call to http://x.example/a?apikey=k"
            );
        });
        let out = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v["ts"].as_str().unwrap().ends_with('Z'));
        }
        assert!(
            !out.contains("hunter2") && !out.contains("x.example") && !out.contains("apikey=k")
        );
        assert!(lines[0].contains("scan started") && lines[0].contains("J9"));
        let v = b.view(&LogQuery::default());
        assert_eq!(v.entries.len(), 2);
        assert_eq!(v.entries[1].level, LogLevel::Warn);
    }
}
