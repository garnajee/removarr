//! Read side (CQRS): SQL over the storage schema (handoff 04) returning view models.
//!
//! Read-only: the pool is opened with `read_only`, every list is bounded, nothing is written.
//! u64 columns are stored as the same 64 bits in an INTEGER (`from_ne_bytes`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, IdentityProof, Provenance,
};
use removarr_domain::fs::PhysicalId;
use removarr_domain::gain::PlanGains;
use removarr_domain::ids::{PlanId, SessionId};
use removarr_domain::issue::{Blocker, BlockerCode, IssueCategory, Outcome, Severity, Unit};
use removarr_domain::journal::ActionOutcome;
use removarr_domain::ports::Clock;
use removarr_domain::session::{ServiceState, ServiceStatus, SessionMetrics};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{ConnectOptions, Row, SqlitePool};

use crate::explain::{self, PlanFacts, btrfs_caveat, session_explanations};
use crate::model::*;
use crate::redact::{sanitize, truncate};
use crate::timefmt::iso;

const DAY_MS: i64 = 86_400_000;
const MAX_LIST: u64 = 1000;
const MAX_PATHS_PER_ROW: usize = 20;
const MAX_ANNEXES: usize = 200;
const MAX_SERIES_ROWS: i64 = 200_000;
const MAX_POINTS: usize = 730;
const MAX_TEXT: usize = 400;

/// Live facts the read model cannot know (they come from config and the running process).
#[derive(Debug, Clone, Default)]
pub struct DashboardInputs {
    /// `scan.interval_minutes`; 0 means manual scans only (never stale).
    pub scan_interval_minutes: u32,
    pub auto_enabled: bool,
    pub dry_run: bool,
    /// The job runner reports recovery as unresolved (OR-ed with unfinished journal intents).
    pub recovery_pending: bool,
    /// From the action runner's left-stopped set (not persisted).
    pub torrents_left_stopped: u64,
    /// Fresh service statuses; when absent the states recorded by the last scan are shown
    /// with `current = false`.
    pub live_services: Option<Vec<ServiceStatus>>,
}

/// What kind of unit an issue applies to (filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueUnitKind {
    File,
    Folder,
    Torrent,
    Pair,
}

impl IssueUnitKind {
    fn json_key(self) -> &'static str {
        match self {
            IssueUnitKind::File => "File",
            IssueUnitKind::Folder => "Folder",
            IssueUnitKind::Torrent => "Torrent",
            IssueUnitKind::Pair => "Pair",
        }
    }
}

/// Sort key of an issue list. `Severity` is the report order (severity, potential, id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IssueSort {
    #[default]
    Severity,
    Name,
    Size,
    Age,
}

/// Filters, sort and page are applied in SQL; totals cover every match, not only the page.
#[derive(Debug, Clone, Default)]
pub struct IssueQuery {
    pub category: Option<IssueCategory>,
    pub min_severity: Option<Severity>,
    pub offset: u64,
    /// Clamped to 1..=1000; 0 means the default 100.
    pub limit: u64,
    /// Only these issue ids (the detail page and bulk commands resolve any id, whatever its
    /// rank). Empty means no id filter; keep it to a few hundred ids per query.
    pub ids: Vec<String>,
    /// `Some(false)` current issues only, `Some(true)` ignored ones only, `None` both.
    pub ignored: Option<bool>,
    pub unit_kind: Option<IssueUnitKind>,
    /// Case-insensitive (ASCII) substring of the subject, name or rooted paths.
    pub search: Option<String>,
    pub sort: IssueSort,
    /// Reverse the natural direction of `sort` (`Age`: oldest first).
    pub descending: bool,
    /// Only issues whose age basis is at least this many days old.
    pub min_age_days: Option<u32>,
    /// Also fill the torrent list, the annexes and the planned consequences of each row.
    pub include_detail: bool,
}

/// Plan listing page (newest first).
#[derive(Debug, Clone, Default)]
pub struct PlanListQuery {
    pub offset: u64,
    /// Clamped to 1..=1000; 0 means the default 100.
    pub limit: u64,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub offset: u64,
    /// Clamped to 1..=1000; 0 means the default 100.
    pub limit: u64,
    pub plan: Option<PlanId>,
    pub session: Option<SessionId>,
    /// `user` or `auto_policy`.
    pub origin: Option<String>,
    /// Dry-run plans changed nothing; excluded unless asked.
    pub include_dry_run: bool,
}

/// Look-back window of the statistics, counted back from now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatsRange {
    #[default]
    All,
    Days7,
    Days30,
    Days90,
    Days365,
}

impl StatsRange {
    #[must_use]
    pub fn days(self) -> Option<i64> {
        match self {
            StatsRange::All => None,
            StatsRange::Days7 => Some(7),
            StatsRange::Days30 => Some(30),
            StatsRange::Days90 => Some(90),
            StatsRange::Days365 => Some(365),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatsQuery {
    /// Only events at or after this instant (UTC ms).
    pub since: Option<i64>,
    /// Look-back window; the later of `since` and the window start applies.
    pub range: StatsRange,
    /// Daily points kept per series, newest; clamped to 730. 0 means 366.
    pub max_points: usize,
}

#[derive(Clone)]
pub struct Reporter {
    pool: SqlitePool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter").finish_non_exhaustive()
    }
}

fn store_err(_: sqlx::Error) -> PortError {
    // Never forward sqlx text: it can quote bound values.
    PortError::new(ErrorClass::Storage, "reporting read failed")
}

fn corrupt(what: &str) -> PortError {
    PortError::new(
        ErrorClass::Integrity,
        format!("stored {what} is not readable"),
    )
}

fn g<'r, T>(row: &'r SqliteRow, name: &str) -> Result<T, PortError>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(name).map_err(store_err)
}

fn u(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

fn nonneg(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn json<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T, PortError> {
    serde_json::from_str(s).map_err(|_| corrupt(what))
}

/// Enum columns hold either a bare string or JSON.
fn enum_col<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T, PortError> {
    serde_json::from_value(Value::String(s.to_string())).or_else(|_| json(s, what))
}

fn clean(text: &str) -> String {
    truncate(&sanitize(text), MAX_TEXT)
}

/// A row that is neither an error nor a secret: `root:rel`.
fn rooted(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}:{rel}")
    }
}

/// Rooted paths (and torrent hashes) found in a JSON value, in order, bounded.
fn describe_target(v: &Value, out: &mut Vec<String>) {
    if out.len() >= MAX_PATHS_PER_ROW {
        return;
    }
    match v {
        Value::Object(m) => {
            if let (Some(Value::String(r)), Some(Value::String(rel))) =
                (m.get("root"), m.get("rel"))
            {
                out.push(rooted(r, rel));
                return;
            }
            if let Some(Value::String(h)) = m.get("hash") {
                out.push(format!("torrent:{h}"));
            }
            for (k, x) in m {
                if k != "hash" {
                    describe_target(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| describe_target(x, out)),
        _ => {}
    }
}

fn kind_label(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::String(s)) => s,
        Ok(Value::Object(m)) => m.keys().next().cloned().unwrap_or_default(),
        _ => truncate(&sanitize(raw), 40),
    }
}

fn service_vm(service: &str, state: &ServiceState, at: i64, current: bool) -> ServiceVm {
    let (key, version, detail) = match state {
        ServiceState::Ok { version } => ("ok", Some(clean(version)), None),
        ServiceState::Unavailable { reason } => ("unavailable", None, Some(clean(reason))),
        ServiceState::AuthRejected => ("auth_rejected", None, None),
        ServiceState::NotConfigured => ("not_configured", None, None),
        ServiceState::MappingErrors { count } => (
            "mapping_errors",
            None,
            Some(format!("{count} mapping error(s)")),
        ),
    };
    ServiceVm {
        service: service.to_string(),
        state_key: key,
        ok: state.is_ok(),
        problem: !state.is_ok() && !matches!(state, ServiceState::NotConfigured),
        version,
        detail,
        observed_at: at,
        observed_at_iso: iso(at),
        current,
    }
}

/// Facts behind the urgency (architecture 12.2).
#[derive(Debug, Clone, Default)]
pub struct UrgencyFacts {
    pub critical_issues: u64,
    pub warning_issues: u64,
    pub scan_missing: bool,
    pub scan_incomplete: bool,
    pub scan_stale: bool,
    pub services_not_ok: u64,
    pub torrents_left_stopped: u64,
    pub recovery_pending: bool,
    pub auto_paused: bool,
}

/// Red for any critical issue, missing, incomplete or stale scan, a configured service not
/// Ok, torrents left stopped, recovery pending or automatic mode paused; Orange for warnings;
/// Green otherwise. Always carries reasons, an icon and a text label.
#[must_use]
pub fn compute_urgency(f: &UrgencyFacts) -> UrgencyVm {
    let mut red: Vec<ReasonVm> = Vec::new();
    let mut push = |cond: bool, key: &'static str, count: Option<u64>| {
        if cond {
            red.push(ReasonVm { key, count });
        }
    };
    push(
        f.critical_issues > 0,
        "critical_issues",
        Some(f.critical_issues),
    );
    push(f.scan_missing, "scan_missing", None);
    push(f.scan_incomplete, "scan_incomplete", None);
    push(f.scan_stale, "scan_stale", None);
    push(
        f.services_not_ok > 0,
        "service_not_ok",
        Some(f.services_not_ok),
    );
    push(
        f.torrents_left_stopped > 0,
        "torrents_left_stopped",
        Some(f.torrents_left_stopped),
    );
    push(f.recovery_pending, "recovery_pending", None);
    push(f.auto_paused, "auto_paused", None);
    let (level, reasons) = if !red.is_empty() {
        (Urgency::Red, red)
    } else if f.warning_issues > 0 {
        (
            Urgency::Orange,
            vec![ReasonVm {
                key: "warning_issues",
                count: Some(f.warning_issues),
            }],
        )
    } else {
        (Urgency::Green, Vec::new())
    };
    UrgencyVm {
        level,
        label_key: level.label_key(),
        label: level.label_en(),
        icon: level.icon(),
        reasons,
    }
}

struct IssueCore {
    id: String,
    category: IssueCategory,
    severity: Severity,
    subject: String,
    logical: u64,
    potential: u64,
    phys: Vec<PhysicalId>,
    incomplete: bool,
}

/// Greedy, deterministic de-duplication of potential space: issues are visited by decreasing
/// potential then id; an issue whose inodes were already claimed counts 0.
fn counted_potential(cores: &[&IssueCore]) -> (HashMap<String, u64>, u64) {
    let mut order: Vec<&&IssueCore> = cores.iter().collect();
    order.sort_by(|a, b| b.potential.cmp(&a.potential).then_with(|| a.id.cmp(&b.id)));
    let mut claimed: HashSet<(u64, u64)> = HashSet::new();
    let (mut map, mut excluded) = (HashMap::new(), 0u64);
    for c in order {
        if c.potential == 0 {
            map.insert(c.id.clone(), 0);
            continue;
        }
        let overlaps = c.phys.iter().any(|p| claimed.contains(&(p.dev, p.ino)));
        if overlaps {
            excluded = excluded.saturating_add(c.potential);
            map.insert(c.id.clone(), 0);
        } else {
            claimed.extend(c.phys.iter().map(|p| (p.dev, p.ino)));
            map.insert(c.id.clone(), c.potential);
        }
    }
    (map, excluded)
}

fn completeness_vm(c: &Completeness) -> CompletenessVm {
    match c {
        Completeness::Complete => CompletenessVm {
            state: "complete",
            missing: vec![],
        },
        Completeness::Incomplete { missing } => CompletenessVm {
            state: "incomplete",
            missing: missing.iter().take(20).map(|m| clean(m)).collect(),
        },
        Completeness::Unknown => CompletenessVm {
            state: "unknown",
            missing: vec![],
        },
    }
}

fn evidence_vm(
    e: &Evidence,
    issues_session: Option<&str>,
    latest_session: Option<&str>,
) -> EvidenceVm {
    let (kind, summary) = match &e.kind {
        EvidenceKind::Physical { path, sig } => (
            "physical",
            format!("{path} ({} bytes, {} link(s))", sig.size, sig.nlink),
        ),
        EvidenceKind::SamePhysical { a, b } => (
            "same_physical",
            format!("inode {}:{} = {}:{}", a.dev, a.ino, b.dev, b.ino),
        ),
        EvidenceKind::Content { a, b, proof } => {
            let p = match proof {
                ContentProof::None => "no proof".to_string(),
                ContentProof::NameSize => "name and size only".to_string(),
                ContentProof::Semantic { score_milli } => {
                    format!("semantic score {score_milli}/1000")
                }
                ContentProof::Sampled => "sampled".to_string(),
                ContentProof::Identical { compared_at } => format!(
                    "identical (full compare at {})",
                    iso(compared_at.as_millis())
                ),
                ContentProof::Differs { offset } => format!("differs at byte {offset}"),
            };
            ("content", format!("{a} vs {b}: {p}"))
        }
        EvidenceKind::Identity { subject, proof } => match subject.strip_prefix("note:") {
            Some(code) if matches!(proof, IdentityProof::None) => ("note", code.to_string()),
            _ => ("identity", format!("{subject}: {proof:?}")),
        },
        EvidenceKind::Quality { subject, .. } => ("quality", subject.clone()),
        EvidenceKind::TorrentHolds {
            hash,
            state,
            progress,
            path,
        } => (
            "torrent_holds",
            format!(
                "torrent {} holds {path} ({state:?}, {:.0}%)",
                hash.as_str(),
                progress * 100.0
            ),
        ),
        EvidenceKind::ServiceUnavailable { service, reason } => (
            "service_unavailable",
            format!("{} unavailable: {reason}", service.as_str()),
        ),
    };
    let provenance = match &e.provenance {
        Provenance::Filesystem { .. } => "filesystem",
        Provenance::Qbittorrent { .. } => "qbittorrent",
        Provenance::Arr { .. } => "arr",
        Provenance::Jellyfin { .. } => "jellyfin",
        Provenance::Ffprobe { .. } => "ffprobe",
        Provenance::Computed => "computed",
    };
    let current = match (issues_session, latest_session) {
        (Some(i), Some(l)) if i == l => {
            matches!(&e.provenance, Provenance::Filesystem { session } if session.to_string() == i)
        }
        _ => false,
    };
    EvidenceVm {
        kind,
        summary: clean(&summary),
        provenance,
        observed_at: e.observed_at.as_millis(),
        observed_at_iso: iso(e.observed_at.as_millis()),
        current,
        completeness: completeness_vm(&e.completeness),
    }
}

fn unit_view(unit: &Unit) -> (&'static str, String, Vec<String>) {
    match unit {
        Unit::File(p) => (
            "file",
            p.rel.file_name().unwrap_or(p.root.as_str()).to_string(),
            vec![p.to_string()],
        ),
        Unit::Folder {
            dir,
            members,
            annexes,
        } => {
            let mut v = vec![dir.to_string()];
            v.extend(
                members
                    .iter()
                    .chain(annexes)
                    .take(MAX_PATHS_PER_ROW - 1)
                    .map(ToString::to_string),
            );
            (
                "folder",
                dir.rel.file_name().unwrap_or(dir.root.as_str()).to_string(),
                v,
            )
        }
        Unit::Torrent(h) => ("torrent", format!("torrent {}", h.as_str()), vec![]),
        Unit::Pair { a, b } => (
            "pair",
            a.rel.file_name().unwrap_or(a.root.as_str()).to_string(),
            vec![a.to_string(), b.to_string()],
        ),
    }
}

fn blocker_codes(b: &[Blocker]) -> Vec<String> {
    b.iter()
        .map(|x| match serde_json::to_value(&x.code) {
            Ok(Value::String(s)) => s,
            Ok(Value::Object(m)) => m
                .into_iter()
                .next()
                .map(|(k, v)| format!("{k}:{}", v.as_str().unwrap_or_default()))
                .unwrap_or_default(),
            _ => format!("{:?}", BlockerCode::NotInBuild),
        })
        .collect()
}

fn outcome_vm(state: &str, outcome: Option<&str>) -> OutcomeVm {
    let parsed = outcome.and_then(|s| serde_json::from_str::<ActionOutcome>(s).ok());
    match parsed {
        Some(o) => {
            let (key, detail) = match &o {
                ActionOutcome::Verified => ("verified", None),
                ActionOutcome::ExecutedUnverified { reason } => {
                    ("executed_unverified", Some(clean(reason)))
                }
                ActionOutcome::Failed { class } => ("failed", Some(format!("{class:?}"))),
                ActionOutcome::Compensated => ("compensated", None),
                ActionOutcome::FailedNeedsReview => ("failed_needs_review", None),
                ActionOutcome::Simulated => ("simulated", None),
                ActionOutcome::NotExecuted { blockers } => {
                    ("not_executed", Some(blocker_codes(blockers).join(", ")))
                }
            };
            OutcomeVm {
                key: key.to_string(),
                success: o.is_success(),
                needs_attention: o.needs_attention(),
                detail,
            }
        }
        None => OutcomeVm {
            key: state.to_string(),
            success: state == "verified",
            needs_attention: false,
            detail: None,
        },
    }
}

fn day_bucket(at: i64) -> i64 {
    at.div_euclid(DAY_MS) * DAY_MS
}

/// Daily series with running totals; keeps the newest `max_points` buckets.
fn build_series(
    kind: SeriesKind,
    events: &[(i64, i64)],
    guaranteed: bool,
    max_points: usize,
) -> (SeriesVm, bool) {
    let mut buckets: BTreeMap<i64, i64> = BTreeMap::new();
    for (at, bytes) in events {
        let e = buckets.entry(day_bucket(*at)).or_insert(0);
        *e = e.saturating_add(*bytes);
    }
    let mut cumulative = 0i64;
    let mut points: Vec<SeriesPointVm> = buckets
        .into_iter()
        .map(|(at, bytes)| {
            cumulative = cumulative.saturating_add(bytes);
            SeriesPointVm {
                at,
                at_iso: iso(at),
                bytes,
                cumulative,
            }
        })
        .collect();
    let truncated = points.len() > max_points;
    if truncated {
        points.drain(..points.len() - max_points);
    }
    (
        SeriesVm {
            kind,
            label_key: kind.label_key(),
            label: kind.label_en(),
            points,
            total: cumulative,
            guaranteed,
        },
        truncated,
    )
}

impl Reporter {
    /// Opens a read-only pool on the database file (2 connections, WAL friendly).
    pub async fn open(db_path: &Path, clock: Arc<dyn Clock>) -> Result<Self, PortError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_millis(5000))
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts)
            .await
            .map_err(store_err)?;
        Ok(Self { pool, clock })
    }

    #[must_use]
    pub fn from_pool(pool: SqlitePool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    async fn state_value(&self, key: &str) -> Result<Option<(String, i64)>, PortError> {
        let row = sqlx::query("SELECT value, updated_at FROM app_state WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_err)?;
        row.map(|r| Ok((g::<String>(&r, "value")?, g::<i64>(&r, "updated_at")?)))
            .transpose()
    }

    async fn meta_value(&self, key: &str) -> Result<Option<String>, PortError> {
        let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_err)?;
        row.map(|r| g::<String>(&r, "value")).transpose()
    }

    /// Automatic mode status including why it paused (T5-19).
    pub async fn auto_status(&self, enabled: bool) -> Result<AutoVm, PortError> {
        let paused = self
            .state_value("auto_paused")
            .await?
            .filter(|(v, _)| !v.is_empty());
        let marker = self
            .state_value("auto_cycle_running")
            .await?
            .filter(|(v, _)| !v.is_empty());
        Ok(AutoVm {
            enabled,
            paused: paused.is_some(),
            paused_reason: paused.as_ref().map(|(v, _)| clean(v)),
            paused_at: paused.as_ref().map(|(_, t)| *t),
            paused_at_iso: paused.as_ref().map(|(_, t)| iso(*t)),
            cycle_marker_session: marker.map(|(v, _)| clean(&v)),
        })
    }

    async fn session_row(
        &self,
        row: &SqliteRow,
        interval_minutes: u32,
    ) -> Result<SessionVm, PortError> {
        let id: String = g(row, "id")?;
        let started: i64 = g(row, "started_at")?;
        let finished: Option<i64> = g(row, "finished_at")?;
        let status: String = g(row, "status")?;
        let metrics: SessionMetrics = json(&g::<String>(row, "metrics")?, "session metrics")?;
        let roots = sqlx::query(
            "SELECT root, scanned, error_count, protected_count FROM session_roots WHERE session_id = ? ORDER BY root",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let roots = roots
            .iter()
            .map(|r| {
                Ok(SessionRootVm {
                    root: g::<String>(r, "root")?,
                    scanned: g::<i64>(r, "scanned")? == 1,
                    error_count: u32::try_from(g::<i64>(r, "error_count")?).unwrap_or(u32::MAX),
                    protected_count: u32::try_from(g::<i64>(r, "protected_count")?)
                        .unwrap_or(u32::MAX),
                })
            })
            .collect::<Result<Vec<_>, PortError>>()?;
        let now = self.clock.now().as_millis();
        let age_ms = finished.map(|f| now.saturating_sub(f));
        let stale = interval_minutes > 0
            && age_ms.is_some_and(|a| a > i64::from(interval_minutes) * 60_000 * 2);
        Ok(SessionVm {
            complete: status == "complete",
            id,
            started_at: started,
            started_at_iso: iso(started),
            finished_at: finished,
            finished_at_iso: finished.map(iso),
            status,
            age_ms,
            stale,
            entries_scanned: metrics.entries_scanned,
            scan_errors: metrics.scan_errors,
            duration_ms: metrics.duration_ms,
            roots,
        })
    }

    /// The most recent finished scan session, or the one asked for.
    pub async fn session(
        &self,
        id: Option<SessionId>,
        interval_minutes: u32,
    ) -> Result<Option<SessionVm>, PortError> {
        let row = match id {
            Some(id) => sqlx::query("SELECT id, started_at, finished_at, status, metrics FROM scan_sessions WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await,
            None => sqlx::query(
                "SELECT id, started_at, finished_at, status, metrics FROM scan_sessions \
                 WHERE finished_at IS NOT NULL ORDER BY finished_at DESC, started_at DESC LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await,
        }
        .map_err(store_err)?;
        match row {
            Some(r) => Ok(Some(self.session_row(&r, interval_minutes).await?)),
            None => Ok(None),
        }
    }

    async fn services(
        &self,
        session: Option<&SessionVm>,
        inputs: &DashboardInputs,
    ) -> Result<Vec<ServiceVm>, PortError> {
        if let Some(live) = &inputs.live_services {
            return Ok(live
                .iter()
                .map(|s| {
                    service_vm(
                        s.service.as_str(),
                        &s.state,
                        s.observed_at.as_millis(),
                        true,
                    )
                })
                .collect());
        }
        let Some(s) = session else { return Ok(vec![]) };
        let rows = sqlx::query("SELECT service, state, observed_at FROM session_services WHERE session_id = ? ORDER BY service")
            .bind(&s.id)
            .fetch_all(&self.pool)
            .await
            .map_err(store_err)?;
        rows.iter()
            .map(|r| {
                let name: String = g(r, "service")?;
                let at: i64 = g(r, "observed_at")?;
                let state: Option<ServiceState> =
                    serde_json::from_str(&g::<String>(r, "state")?).ok();
                Ok(match state {
                    Some(st) => service_vm(&name, &st, at, false),
                    None => ServiceVm {
                        service: name,
                        state_key: "unknown",
                        ok: false,
                        problem: true,
                        version: None,
                        detail: None,
                        observed_at: at,
                        observed_at_iso: iso(at),
                        current: false,
                    },
                })
            })
            .collect()
    }

    async fn jobs(&self) -> Result<Vec<JobVm>, PortError> {
        let rows = sqlx::query(
            "SELECT id, kind, status, stage, enqueued_at, started_at FROM jobs \
             WHERE status IN ('queued','running') ORDER BY enqueued_at, rowid LIMIT 50",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.iter()
            .map(|r| {
                Ok(JobVm {
                    id: g(r, "id")?,
                    kind: kind_label(&g::<String>(r, "kind")?),
                    status: g(r, "status")?,
                    stage: g::<Option<String>>(r, "stage")?.map(|s| clean(&s)),
                    enqueued_at: g(r, "enqueued_at")?,
                    started_at: g(r, "started_at")?,
                })
            })
            .collect()
    }

    async fn unfinished_intents(&self) -> Result<u64, PortError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journal_intents i LEFT JOIN journal_results r ON r.intent_id = i.id \
             WHERE i.mode = 'live' AND r.intent_id IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(nonneg(n))
    }

    async fn ignored_subjects(&self) -> Result<HashSet<String>, PortError> {
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT subject_key FROM ignores WHERE restored_at IS NULL")
                .fetch_all(&self.pool)
                .await
                .map_err(store_err)?;
        Ok(rows.into_iter().collect())
    }

    async fn issue_cores(&self) -> Result<Vec<IssueCore>, PortError> {
        let rows = sqlx::query(
            "SELECT id, category, severity, subject, logical_bytes, potential_bytes, physical_ids, \
             (evidence = '[]' OR evidence LIKE '%\"completeness\":\"Unknown\"%' OR evidence LIKE '%\"Incomplete\":%') AS incomplete \
             FROM issues WHERE resolved_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.iter()
            .map(|r| {
                Ok(IssueCore {
                    id: g(r, "id")?,
                    category: enum_col(&g::<String>(r, "category")?, "issue category")?,
                    severity: enum_col(&g::<String>(r, "severity")?, "severity")?,
                    subject: g(r, "subject")?,
                    logical: u(g(r, "logical_bytes")?),
                    potential: u(g(r, "potential_bytes")?),
                    phys: json(&g::<String>(r, "physical_ids")?, "physical ids")?,
                    incomplete: g::<i64>(r, "incomplete")? == 1,
                })
            })
            .collect()
    }

    async fn space(
        &self,
        observed_at: Option<i64>,
        potential: u64,
        excluded: u64,
    ) -> Result<SpaceVm, PortError> {
        let rows = sqlx::query(
            "SELECT root, SUM(sz) AS logical, SUM(a) AS alloc, SUM(CASE WHEN c > 1 THEN a ELSE 0 END) AS shared \
             FROM (SELECT root, dev, ino, SUM(size) AS sz, MAX(allocated) AS a, COUNT(*) AS c \
                   FROM fs_entries WHERE kind = 'file' AND gone_session IS NULL GROUP BY root, dev, ino) \
             GROUP BY root ORDER BY root",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let roots = rows
            .iter()
            .map(|r| {
                Ok(RootSpaceVm {
                    root: g(r, "root")?,
                    logical_bytes: nonneg(g(r, "logical")?),
                    unique_allocated_bytes: nonneg(g(r, "alloc")?),
                    shared_bytes: nonneg(g(r, "shared")?),
                })
            })
            .collect::<Result<Vec<_>, PortError>>()?;
        // Totals are computed over distinct inodes across roots, not by adding the roots.
        let t = sqlx::query(
            "SELECT COALESCE(SUM(sz),0) AS logical, COALESCE(SUM(a),0) AS alloc, \
             COALESCE(SUM(CASE WHEN c > 1 THEN a ELSE 0 END),0) AS shared \
             FROM (SELECT dev, ino, SUM(size) AS sz, MAX(allocated) AS a, COUNT(*) AS c \
                   FROM fs_entries WHERE kind = 'file' AND gone_session IS NULL GROUP BY dev, ino)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(SpaceVm {
            logical_bytes: nonneg(g(&t, "logical")?),
            unique_allocated_bytes: nonneg(g(&t, "alloc")?),
            shared_bytes: nonneg(g(&t, "shared")?),
            potential_bytes: potential,
            potential_overlap_excluded_bytes: excluded,
            potential_label_key: POTENTIAL_LABEL_KEY,
            observed_at,
            observed_at_iso: observed_at.map(iso),
            roots,
        })
    }

    /// Dashboard aggregates: issues by category and severity, urgency, space (four separate
    /// quantities), service health, scan status, jobs, recovery and automatic mode.
    pub async fn dashboard(&self, inputs: &DashboardInputs) -> Result<DashboardVm, PortError> {
        let now = self.clock.now().as_millis();
        let ignored = self.ignored_subjects().await?;
        let all = self.issue_cores().await?;
        let (active, ignored_cores): (Vec<&IssueCore>, Vec<&IssueCore>) =
            all.iter().partition(|c| !ignored.contains(&c.subject));
        let (counted, excluded) = counted_potential(&active);
        let session = self.session(None, inputs.scan_interval_minutes).await?;
        let issues_session = self.meta_value("issues_session").await?;
        let services = self.services(session.as_ref(), inputs).await?;
        let auto = self.auto_status(inputs.auto_enabled).await?;
        let unfinished = self.unfinished_intents().await?;
        let jobs = self.jobs().await?;

        let categories = IssueCategory::ALL
            .iter()
            .map(|cat| {
                let mine: Vec<&&IssueCore> = active.iter().filter(|c| c.category == *cat).collect();
                let n = |s: Severity| mine.iter().filter(|c| c.severity == s).count() as u64;
                CategoryCountVm {
                    category: *cat,
                    count: mine.len() as u64,
                    max_severity: mine
                        .iter()
                        .map(|c| c.severity)
                        .max()
                        .unwrap_or(Severity::Info),
                    info: n(Severity::Info),
                    warning: n(Severity::Warning),
                    critical: n(Severity::Critical),
                    logical_bytes: mine.iter().fold(0u64, |a, c| a.saturating_add(c.logical)),
                    potential_bytes: mine.iter().fold(0u64, |a, c| {
                        a.saturating_add(counted.get(&c.id).copied().unwrap_or(0))
                    }),
                }
            })
            .collect::<Vec<_>>();
        let potential = categories
            .iter()
            .fold(0u64, |a, c| a.saturating_add(c.potential_bytes));
        let observed_at = session.as_ref().and_then(|s| s.finished_at);
        let space = self.space(observed_at, potential, excluded).await?;
        let incomplete = active.iter().filter(|c| c.incomplete).count() as u64;

        let recovery_pending = inputs.recovery_pending || unfinished > 0;
        let urgency = compute_urgency(&UrgencyFacts {
            critical_issues: categories.iter().map(|c| c.critical).sum(),
            warning_issues: categories.iter().map(|c| c.warning).sum(),
            scan_missing: session.is_none(),
            scan_incomplete: session.as_ref().is_some_and(|s| !s.complete),
            scan_stale: session.as_ref().is_some_and(|s| s.stale),
            services_not_ok: services.iter().filter(|s| s.problem).count() as u64,
            torrents_left_stopped: inputs.torrents_left_stopped,
            recovery_pending,
            auto_paused: auto.paused,
        });
        let mut explanations = session_explanations(
            session.as_ref(),
            issues_session.as_deref(),
            &services,
            incomplete,
        );
        if auto.paused {
            explanations.push(explain::ex(
                "auto.paused",
                &[("reason", auto.paused_reason.clone().unwrap_or_default())],
                format!(
                    "Automatic mode is paused until a user resumes it. Reason: {}",
                    auto.paused_reason.clone().unwrap_or_default()
                ),
            ));
        }
        Ok(DashboardVm {
            generated_at: now,
            generated_at_iso: iso(now),
            urgency,
            categories,
            ignored_issues: ignored_cores.len() as u64,
            space,
            services,
            last_scan: session,
            issues_session,
            running_jobs: jobs,
            torrents_left_stopped: inputs.torrents_left_stopped,
            recovery_pending,
            unfinished_intents: unfinished,
            auto,
            dry_run: inputs.dry_run,
            explanations,
        })
    }

    /// SQL selection of the issues matching `q`: ids and sizes in the requested order.
    async fn select_issues(
        &self,
        q: &IssueQuery,
        now: i64,
    ) -> Result<Vec<(String, u64)>, PortError> {
        enum Bind {
            Text(String),
            Int(i64),
        }
        let enum_text = |v: Value| match v {
            Value::String(s) => s,
            _ => String::new(),
        };
        let path = "COALESCE(json_extract(unit,'$.File.rel'), json_extract(unit,'$.Folder.dir.rel'), json_extract(unit,'$.Pair.a.rel'), '')";
        let root = "COALESCE(json_extract(unit,'$.File.root'), json_extract(unit,'$.Folder.dir.root'), json_extract(unit,'$.Pair.a.root'), '')";
        let name = format!(
            "CASE WHEN json_type(unit,'$.Torrent') IS NOT NULL THEN 'torrent ' || json_extract(unit,'$.Torrent') \
             ELSE COALESCE(NULLIF(replace({path}, rtrim({path}, replace({path}, '/', '')), ''), ''), {root}) END"
        );
        let mut sql =
            String::from("SELECT id, logical_bytes FROM issues WHERE resolved_at IS NULL");
        let mut binds: Vec<Bind> = Vec::new();
        if let Some(c) = q.category {
            sql.push_str(" AND category = ?");
            binds.push(Bind::Text(enum_text(
                serde_json::to_value(c).map_err(|_| corrupt("category"))?,
            )));
        }
        if let Some(min) = q.min_severity {
            let allowed: Vec<String> = [Severity::Info, Severity::Warning, Severity::Critical]
                .into_iter()
                .filter(|s| *s >= min)
                .map(|s| serde_json::to_value(s).map(enum_text))
                .collect::<Result<_, _>>()
                .map_err(|_| corrupt("severity"))?;
            sql.push_str(" AND severity IN (");
            sql.push_str(&vec!["?"; allowed.len()].join(","));
            sql.push(')');
            binds.extend(allowed.into_iter().map(Bind::Text));
        }
        if !q.ids.is_empty() {
            sql.push_str(" AND id IN (");
            sql.push_str(&vec!["?"; q.ids.len()].join(","));
            sql.push(')');
            binds.extend(q.ids.iter().cloned().map(Bind::Text));
        }
        match q.ignored {
            Some(true) => sql.push_str(
                " AND subject IN (SELECT subject_key FROM ignores WHERE restored_at IS NULL)",
            ),
            Some(false) => sql.push_str(
                " AND subject NOT IN (SELECT subject_key FROM ignores WHERE restored_at IS NULL)",
            ),
            None => {}
        }
        if let Some(k) = q.unit_kind {
            sql.push_str(&format!(
                " AND json_type(unit,'$.{}') IS NOT NULL",
                k.json_key()
            ));
        }
        if let Some(days) = q.min_age_days {
            sql.push_str(" AND age_basis <= ?");
            binds.push(Bind::Int(now.saturating_sub(i64::from(days) * DAY_MS)));
        }
        if let Some(needle) = q.search.as_deref().filter(|n| !n.is_empty()) {
            let needle = needle.to_ascii_lowercase();
            sql.push_str(&format!(
                " AND (instr(lower(subject), ?) > 0 OR instr(lower({name}), ?) > 0 \
                 OR instr(lower({path}), ?) > 0 \
                 OR instr(lower(COALESCE(json_extract(unit,'$.Pair.b.rel'), '')), ?) > 0 \
                 OR EXISTS (SELECT 1 FROM json_each(unit,'$.Folder.members') m WHERE instr(lower(json_extract(m.value,'$.rel')), ?) > 0) \
                 OR EXISTS (SELECT 1 FROM json_each(unit,'$.Folder.annexes') m WHERE instr(lower(json_extract(m.value,'$.rel')), ?) > 0))"
            ));
            for _ in 0..6 {
                binds.push(Bind::Text(needle.clone()));
            }
        }
        let dir = if q.descending { "DESC" } else { "ASC" };
        match q.sort {
            IssueSort::Severity => {
                sql.push_str(
                    " ORDER BY CASE severity WHEN 'critical' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, potential_bytes DESC, id",
                );
            }
            IssueSort::Name => sql.push_str(&format!(" ORDER BY lower({name}) {dir}, id")),
            IssueSort::Size => sql.push_str(&format!(" ORDER BY logical_bytes {dir}, id")),
            // Older means a smaller age basis: ascending age shows the newest first.
            IssueSort::Age => sql.push_str(&format!(
                " ORDER BY age_basis {}, id",
                if q.descending { "ASC" } else { "DESC" }
            )),
        }
        let mut query = sqlx::query(&sql);
        for b in binds {
            query = match b {
                Bind::Text(t) => query.bind(t),
                Bind::Int(i) => query.bind(i),
            };
        }
        let rows = query.fetch_all(&self.pool).await.map_err(store_err)?;
        rows.iter()
            .map(|r| Ok((g::<String>(r, "id")?, u(g(r, "logical_bytes")?))))
            .collect()
    }

    /// Torrents (hash, name, state) of an issue, from the current snapshot.
    async fn torrents_of(&self, hashes_json: &str) -> Result<Vec<TorrentVm>, PortError> {
        let rows = sqlx::query(
            "SELECT hash, name, state FROM torrents WHERE hash IN (SELECT value FROM json_each(?)) ORDER BY hash LIMIT 50",
        )
        .bind(hashes_json)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.iter()
            .map(|r| {
                Ok(TorrentVm {
                    hash: g(r, "hash")?,
                    name: clean(&g::<String>(r, "name")?),
                    state: clean(&g::<String>(r, "state")?),
                })
            })
            .collect()
    }

    /// Actions of stored plans whose target is one of `paths` (rooted paths of the issue unit):
    /// what each plan would do, or did, to it. Discarded, expired and invalidated plans are left out.
    async fn planned_for(&self, paths: &[String]) -> Result<Vec<PlannedActionVm>, PortError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT p.id AS plan_id, p.status, p.mode, pa.idx, pa.kind, pa.state, \
             json_extract(p.body, '$.actions[' || pa.idx || '].target') AS target, \
             json_extract(p.body, '$.actions[' || pa.idx || '].expected_gain') AS expected, \
             COALESCE(json_array_length(p.body, '$.actions[' || pa.idx || '].torrent_hashes'), 0) AS torrents \
             FROM plan_actions pa JOIN plans p ON p.id = pa.plan_id \
             WHERE p.status NOT IN ('discarded','expired','invalidated') \
             ORDER BY p.created_at DESC, pa.idx LIMIT 5000",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let mut out = Vec::new();
        for r in &rows {
            let Some(target) = g::<Option<String>>(r, "target")? else {
                continue;
            };
            let target: Value = json(&target, "action target")?;
            let mut touched = Vec::new();
            describe_target(&target, &mut touched);
            if !touched.iter().any(|t| paths.contains(t)) {
                continue;
            }
            let expected: removarr_domain::gain::GainEstimate = g::<Option<String>>(r, "expected")?
                .map(|s| json(&s, "expected gain"))
                .transpose()?
                .unwrap_or_default();
            out.push(PlannedActionVm {
                plan_id: g(r, "plan_id")?,
                plan_status: g(r, "status")?,
                mode: g(r, "mode")?,
                index: u32::try_from(g::<i64>(r, "idx")?).unwrap_or(0),
                kind: clean(&kind_label(&g::<String>(r, "kind")?)),
                state: g(r, "state")?,
                expected_logical: expected.logical,
                expected_attributable: expected.attributable,
                expected_added: expected.added,
                torrents: usize::try_from(g::<i64>(r, "torrents")?).unwrap_or(0),
            });
            if out.len() >= 20 {
                break;
            }
        }
        Ok(out)
    }

    /// One bounded page of current issues with evidence, completeness and inode proof.
    pub async fn issues(&self, q: &IssueQuery) -> Result<IssueListVm, PortError> {
        let now = self.clock.now().as_millis();
        let ignored = self.ignored_subjects().await?;
        let cores = self.issue_cores().await?;
        let active: Vec<&IssueCore> = cores
            .iter()
            .filter(|c| !ignored.contains(&c.subject))
            .collect();
        let (counted, _) = counted_potential(&active);
        let by_id: HashMap<&str, &IssueCore> = cores.iter().map(|c| (c.id.as_str(), c)).collect();
        let matches = self.select_issues(q, now).await?;
        let limit = if q.limit == 0 {
            100
        } else {
            q.limit.min(MAX_LIST)
        };
        let total_count = matches.len() as u64;
        let total_logical = matches.iter().fold(0u64, |a, (_, b)| a.saturating_add(*b));
        let total_potential = matches.iter().fold(0u64, |a, (id, _)| {
            a.saturating_add(counted.get(id).copied().unwrap_or(0))
        });
        let page: Vec<&IssueCore> = matches
            .iter()
            .filter_map(|(id, _)| by_id.get(id.as_str()).copied())
            .skip(usize::try_from(q.offset).unwrap_or(usize::MAX))
            .take(usize::try_from(limit).unwrap_or(100))
            .collect();

        let issues_session = self.meta_value("issues_session").await?;
        let latest = self.session(None, 0).await?;
        let mut rows = Vec::with_capacity(page.len());
        for c in page {
            let r = sqlx::query(
                "SELECT unit, outcome, evidence, blockers, torrent_hashes, first_seen_at, age_basis FROM issues WHERE id = ?",
            )
            .bind(&c.id)
            .fetch_one(&self.pool)
            .await
            .map_err(store_err)?;
            let unit: Unit = json(&g::<String>(&r, "unit")?, "unit")?;
            let evidence: Vec<Evidence> = json(&g::<String>(&r, "evidence")?, "evidence")?;
            let blockers: Vec<Blocker> = json(&g::<String>(&r, "blockers")?, "blockers")?;
            let hashes_raw: String = g(&r, "torrent_hashes")?;
            let hashes: Vec<Value> = json(&hashes_raw, "torrent hashes")?;
            let (unit_kind, name, paths) = unit_view(&unit);
            let (torrents, annexes, planned) = if q.include_detail {
                let (annexes, primary): (Vec<String>, Vec<String>) = match &unit {
                    Unit::Folder { dir, annexes, .. } => (
                        annexes
                            .iter()
                            .take(MAX_ANNEXES)
                            .map(|p| clean(&rooted(p.root.as_str(), p.rel.as_str())))
                            .collect(),
                        vec![rooted(dir.root.as_str(), dir.rel.as_str())],
                    ),
                    Unit::File(p) => (vec![], vec![rooted(p.root.as_str(), p.rel.as_str())]),
                    Unit::Pair { a, b } => (
                        vec![],
                        vec![
                            rooted(a.root.as_str(), a.rel.as_str()),
                            rooted(b.root.as_str(), b.rel.as_str()),
                        ],
                    ),
                    Unit::Torrent(h) => (vec![], vec![format!("torrent:{}", h.as_str())]),
                };
                (
                    self.torrents_of(&hashes_raw).await?,
                    annexes,
                    self.planned_for(&primary).await?,
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
            let first_seen: i64 = g(&r, "first_seen_at")?;
            let combined = removarr_domain::evidence::combined_completeness(&evidence);
            rows.push(IssueRowVm {
                id: c.id.clone(),
                category: c.category,
                severity: c.severity,
                outcome: enum_col::<Outcome>(&g::<String>(&r, "outcome")?, "outcome")?,
                subject: clean(&c.subject),
                name: clean(&name),
                unit_kind,
                paths: paths.iter().map(|p| clean(p)).collect(),
                logical_bytes: c.logical,
                potential_bytes: c.potential,
                potential_counted_bytes: counted.get(&c.id).copied().unwrap_or(0),
                potential_label_key: POTENTIAL_LABEL_KEY,
                torrent_count: hashes.len(),
                torrents,
                annexes,
                planned,
                blockers: blocker_codes(&blockers),
                completeness: completeness_vm(&combined),
                evidence: evidence
                    .iter()
                    .take(50)
                    .map(|e| {
                        evidence_vm(
                            e,
                            issues_session.as_deref(),
                            latest.as_ref().map(|s| s.id.as_str()),
                        )
                    })
                    .collect(),
                inodes: c
                    .phys
                    .iter()
                    .take(100)
                    .map(|p| format!("{}:{}", p.dev, p.ino))
                    .collect(),
                first_seen_at: first_seen,
                first_seen_at_iso: iso(first_seen),
                age_basis: g(&r, "age_basis")?,
                ignored: ignored.contains(&c.subject),
            });
        }
        let session_vm = match &issues_session {
            Some(id) => {
                let sid = SessionId::new(
                    removarr_domain::ids::RawUuid::parse_str(id)
                        .map_err(|_| corrupt("session id"))?,
                );
                self.session(Some(sid), 0).await?
            }
            None => None,
        };
        let incomplete = active.iter().filter(|c| c.incomplete).count() as u64;
        let explanations = session_explanations(
            session_vm.as_ref(),
            issues_session.as_deref(),
            &[],
            incomplete,
        );
        Ok(IssueListVm {
            rows,
            total_count,
            total_logical_bytes: total_logical,
            total_potential_bytes: total_potential,
            offset: q.offset,
            limit,
            issues_session,
            session_finished_at: session_vm.as_ref().and_then(|s| s.finished_at),
            session_finished_at_iso: session_vm.as_ref().and_then(|s| s.finished_at_iso.clone()),
            explanations,
        })
    }

    /// Action history, newest first: exact time, target, reason (origin and approval),
    /// outcome and unique gain ids.
    pub async fn history(&self, q: &HistoryQuery) -> Result<HistoryVm, PortError> {
        let limit = if q.limit == 0 {
            100
        } else {
            q.limit.min(MAX_LIST)
        };
        let mut sql = String::from(
            "SELECT pa.plan_id, pa.idx, pa.kind, pa.state, pa.outcome, pa.updated_at, \
             p.mode, p.origin, p.status, p.created_at, ap.approved_by, ap.approved_at, \
             json_extract(p.body, '$.actions[' || pa.idx || '].target') AS target, \
             (SELECT MIN(ji.created_at) FROM journal_intents ji WHERE ji.plan_id = pa.plan_id AND ji.action_idx = pa.idx AND ji.mode = 'live') AS started_at, \
             (SELECT COALESCE(SUM(g.bytes),0) FROM gains g WHERE g.plan_id = pa.plan_id AND g.action_idx = pa.idx AND g.kind <> 'added_cost') AS attributable, \
             (SELECT COALESCE(SUM(g.bytes),0) FROM gains g WHERE g.plan_id = pa.plan_id AND g.action_idx = pa.idx AND g.kind = 'added_cost') AS added, \
             (SELECT group_concat(g.gain_id, ',') FROM gains g WHERE g.plan_id = pa.plan_id AND g.action_idx = pa.idx) AS gain_ids \
             FROM plan_actions pa JOIN plans p ON p.id = pa.plan_id LEFT JOIN approvals ap ON ap.plan_id = pa.plan_id \
             WHERE pa.state <> 'pending'",
        );
        if !q.include_dry_run {
            sql.push_str(" AND p.mode = 'live'");
        }
        if q.plan.is_some() {
            sql.push_str(" AND pa.plan_id = ?");
        }
        if q.session.is_some() {
            sql.push_str(" AND p.session_id = ?");
        }
        if q.origin.is_some() {
            sql.push_str(" AND p.origin = ?");
        }
        sql.push_str(" ORDER BY pa.updated_at DESC, pa.plan_id, pa.idx LIMIT ? OFFSET ?");
        let mut query = sqlx::query(&sql);
        if let Some(p) = q.plan {
            query = query.bind(p.to_string());
        }
        if let Some(s) = q.session {
            query = query.bind(s.to_string());
        }
        if let Some(o) = &q.origin {
            query = query.bind(o.clone());
        }
        let fetched = query
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .bind(i64::try_from(q.offset).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_err)?;
        let has_more = fetched.len() as u64 > limit;
        let mut rows = Vec::new();
        for r in fetched.iter().take(usize::try_from(limit).unwrap_or(100)) {
            let at: i64 = g(r, "updated_at")?;
            let started: Option<i64> = g(r, "started_at")?;
            let approved_at: Option<i64> = g(r, "approved_at")?;
            let created: i64 = g(r, "created_at")?;
            let mut target = Vec::new();
            if let Some(t) = g::<Option<String>>(r, "target")?
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                describe_target(&t, &mut target);
            }
            let outcome_raw: Option<String> = g(r, "outcome")?;
            let gain_ids = g::<Option<String>>(r, "gain_ids")?
                .map(|s| s.split(',').map(str::to_string).collect::<BTreeSet<_>>())
                .unwrap_or_default();
            rows.push(HistoryRowVm {
                plan_id: g(r, "plan_id")?,
                action_index: u32::try_from(g::<i64>(r, "idx")?).unwrap_or(0),
                action_kind: clean(&g::<String>(r, "kind")?),
                at,
                at_iso: iso(at),
                started_at: started,
                started_at_iso: started.map(iso),
                target: clean(&target.join(", ")),
                reason: ReasonOfActionVm {
                    origin: g(r, "origin")?,
                    approved_by: g(r, "approved_by")?,
                    approved_at,
                    approved_at_iso: approved_at.map(iso),
                    plan_created_at: created,
                },
                outcome: outcome_vm(&g::<String>(r, "state")?, outcome_raw.as_deref()),
                gain_ids: gain_ids.into_iter().collect(),
                attributable_bytes: u(g(r, "attributable")?),
                added_bytes: u(g(r, "added")?),
                plan_status: g(r, "status")?,
            });
        }
        Ok(HistoryVm {
            rows,
            offset: q.offset,
            limit,
            has_more,
            unfinished_intents: self.unfinished_intents().await?,
        })
    }

    async fn gain_rows(&self, plan: Option<&str>) -> Result<(Vec<GainRow>, bool), PortError> {
        let base = "SELECT gain_id, plan_id, action_idx, kind, bytes, recorded_at FROM gains";
        let rows =
            match plan {
                Some(p) => sqlx::query(&format!(
                    "{base} WHERE plan_id = ? ORDER BY recorded_at, gain_id LIMIT {MAX_SERIES_ROWS}"
                ))
                .bind(p)
                .fetch_all(&self.pool)
                .await,
                None => {
                    sqlx::query(&format!(
                        "{base} ORDER BY recorded_at, gain_id LIMIT {}",
                        MAX_SERIES_ROWS + 1
                    ))
                    .fetch_all(&self.pool)
                    .await
                }
            }
            .map_err(store_err)?;
        let truncated = plan.is_none() && rows.len() as i64 > MAX_SERIES_ROWS;
        let out = rows
            .iter()
            .take(usize::try_from(MAX_SERIES_ROWS).unwrap_or(usize::MAX))
            .map(|r| {
                Ok(GainRow {
                    gain_id: g(r, "gain_id")?,
                    action_idx: u32::try_from(g::<i64>(r, "action_idx")?).unwrap_or(0),
                    added: g::<String>(r, "kind")? == "added_cost",
                    bytes: u(g(r, "bytes")?),
                    at: g(r, "recorded_at")?,
                })
            })
            .collect::<Result<Vec<_>, PortError>>()?;
        Ok((out, truncated))
    }

    async fn observed_rows(
        &self,
        plan: Option<&str>,
        since: Option<i64>,
    ) -> Result<Vec<ObservedRowVm>, PortError> {
        let mut sql = String::from(
            "SELECT plan_id, root, free_before, free_after_local, free_after_services, recorded_at FROM observed_deltas WHERE recorded_at >= ?",
        );
        if plan.is_some() {
            sql.push_str(" AND plan_id = ?");
        }
        sql.push_str(" ORDER BY recorded_at DESC, id DESC LIMIT 5000");
        let mut q = sqlx::query(&sql).bind(since.unwrap_or(i64::MIN));
        if let Some(p) = plan {
            q = q.bind(p.to_string());
        }
        let rows = q.fetch_all(&self.pool).await.map_err(store_err)?;
        let mut out = rows
            .iter()
            .map(|r| {
                let before = u(g(r, "free_before")?);
                let after = u(g(r, "free_after_local")?);
                let at: i64 = g(r, "recorded_at")?;
                Ok(ObservedRowVm {
                    plan_id: g(r, "plan_id")?,
                    root: g(r, "root")?,
                    free_before: before,
                    free_after_local: after,
                    free_after_services: g::<Option<i64>>(r, "free_after_services")?.map(u),
                    delta_local: i64::try_from(after)
                        .unwrap_or(i64::MAX)
                        .saturating_sub(i64::try_from(before).unwrap_or(i64::MAX)),
                    at,
                    at_iso: iso(at),
                })
            })
            .collect::<Result<Vec<_>, PortError>>()?;
        out.reverse();
        Ok(out)
    }

    /// Time series of the four gain notions (plus added cost), daily buckets, and the unique
    /// gain-id totals. Attributable and added bytes are summed only through unique gain ids.
    pub async fn stats(&self, q: &StatsQuery) -> Result<StatsVm, PortError> {
        let now = self.clock.now().as_millis();
        let max_points = if q.max_points == 0 {
            366
        } else {
            q.max_points.min(MAX_POINTS)
        };
        let since = match q.range.days() {
            Some(d) => Some(
                q.since
                    .map_or(now - d * DAY_MS, |s| s.max(now - d * DAY_MS)),
            ),
            None => q.since,
        };
        let q = &StatsQuery { since, ..q.clone() };
        let (gains, gains_truncated) = self.gain_rows(None).await?;
        let (totals, attributable_events, added_events) = aggregate_gains(&gains, q.since);

        let expected_rows = sqlx::query(
            "SELECT created_at, json_extract(body, '$.gains.attributable') AS expected FROM plans \
             WHERE mode = 'live' AND status IN ('draft','approved','executing','executed','partially_executed','interrupted') \
             AND created_at >= ? ORDER BY created_at LIMIT 100000",
        )
        .bind(q.since.unwrap_or(i64::MIN))
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let expected_events = expected_rows
            .iter()
            .map(|r| {
                Ok((
                    g::<i64>(r, "created_at")?,
                    g::<Option<i64>>(r, "expected")?
                        .map_or(0, |v| i64::try_from(u(v)).unwrap_or(i64::MAX)),
                ))
            })
            .collect::<Result<Vec<_>, PortError>>()?;

        let observed = self.observed_rows(None, q.since).await?;
        let observed_events: Vec<(i64, i64)> =
            observed.iter().map(|o| (o.at, o.delta_local)).collect();

        let ignored = self.ignored_subjects().await?;
        let cores = self.issue_cores().await?;
        let active: Vec<&IssueCore> = cores
            .iter()
            .filter(|c| !ignored.contains(&c.subject))
            .collect();
        let (counted, _) = counted_potential(&active);
        let potential: u64 = counted.values().fold(0u64, |a, v| a.saturating_add(*v));
        let issues_session = self.meta_value("issues_session").await?;
        let potential_at = match &issues_session {
            Some(s) => match removarr_domain::ids::RawUuid::parse_str(s) {
                Ok(raw) => self
                    .session(Some(SessionId::new(raw)), 0)
                    .await?
                    .and_then(|s| s.finished_at),
                Err(_) => None,
            },
            None => None,
        };
        let potential_events: Vec<(i64, i64)> = potential_at
            .map(|t| vec![(t, i64::try_from(potential).unwrap_or(i64::MAX))])
            .unwrap_or_default();

        let mut truncated = gains_truncated;
        let mut series = Vec::new();
        for (kind, events, guaranteed) in [
            (SeriesKind::Potential, potential_events, false),
            (SeriesKind::Expected, expected_events, false),
            (SeriesKind::Attributable, attributable_events, true),
            (SeriesKind::AddedCost, added_events, true),
            (SeriesKind::Observed, observed_events, false),
        ] {
            let (s, t) = build_series(kind, &events, guaranteed, max_points);
            truncated |= t;
            series.push(s);
        }
        let observed_tail = observed.len().saturating_sub(100);
        Ok(StatsVm {
            generated_at: now,
            series,
            totals,
            observed: observed[observed_tail..].to_vec(),
            caveats: vec![btrfs_caveat()],
            truncated,
        })
    }

    /// Stored plans, newest first: status, mode, action counts and gains. Read-only.
    pub async fn plans(&self, q: &PlanListQuery) -> Result<PlanListVm, PortError> {
        let limit = if q.limit == 0 {
            100
        } else {
            q.limit.min(MAX_LIST)
        };
        let rows = sqlx::query(
            "SELECT p.id, p.status, p.mode, p.origin, p.created_at, \
             COALESCE(json_array_length(p.body, '$.actions'), 0) AS n, \
             (SELECT COUNT(*) FROM plan_actions a WHERE a.plan_id = p.id AND a.state IN ('verified','executed_unverified')) AS done, \
             (SELECT COUNT(*) FROM plan_actions a WHERE a.plan_id = p.id AND a.state IN ('failed','failed_needs_review','compensated')) AS failed, \
             COALESCE(json_extract(p.body, '$.gains.attributable'), 0) AS planned_a, \
             COALESCE(json_extract(p.body, '$.gains.added'), 0) AS planned_d, \
             COALESCE((SELECT SUM(bytes) FROM gains x WHERE x.plan_id = p.id AND x.kind != 'added_cost'), 0) AS rec_a, \
             COALESCE((SELECT SUM(bytes) FROM gains x WHERE x.plan_id = p.id AND x.kind = 'added_cost'), 0) AS rec_d \
             FROM plans p ORDER BY p.created_at DESC, p.id LIMIT ? OFFSET ?",
        )
        .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
        .bind(i64::try_from(q.offset).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let has_more = rows.len() as u64 > limit;
        let rows = rows
            .iter()
            .take(usize::try_from(limit).unwrap_or(100))
            .map(|r| {
                let created: i64 = g(r, "created_at")?;
                Ok(PlanRowVm {
                    id: g(r, "id")?,
                    status: g(r, "status")?,
                    mode: g(r, "mode")?,
                    origin: g(r, "origin")?,
                    created_at: created,
                    created_at_iso: iso(created),
                    actions: nonneg(g(r, "n")?),
                    actions_done: nonneg(g(r, "done")?),
                    actions_failed: nonneg(g(r, "failed")?),
                    planned_attributable: nonneg(g(r, "planned_a")?),
                    planned_added: nonneg(g(r, "planned_d")?),
                    recorded_attributable: nonneg(g(r, "rec_a")?),
                    recorded_added: nonneg(g(r, "rec_d")?),
                })
            })
            .collect::<Result<Vec<_>, PortError>>()?;
        Ok(PlanListVm {
            rows,
            offset: q.offset,
            limit,
            has_more,
        })
    }

    /// Plan versus result: planned and recorded gains, observed deltas, differences and the
    /// actions that still need something.
    pub async fn plan_report(&self, plan: PlanId) -> Result<PlanReportVm, PortError> {
        let pid = plan.to_string();
        let p = sqlx::query(
            "SELECT id, hash, status, mode, origin, session_id, created_at, json_extract(body, '$.gains') AS gains FROM plans WHERE id = ?",
        )
        .bind(&pid)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?
        .ok_or_else(|| PortError::new(ErrorClass::NotFound, "plan not found"))?;
        let planned: PlanGains = g::<Option<String>>(&p, "gains")?
            .map(|s| json(&s, "plan gains"))
            .transpose()?
            .unwrap_or_default();
        let (status, mode): (String, String) = (g(&p, "status")?, g(&p, "mode")?);
        let created: i64 = g(&p, "created_at")?;

        let (gains, _) = self.gain_rows(Some(&pid)).await?;
        let (recorded, _, _) = aggregate_gains(&gains, None);
        let rows = sqlx::query(
            "SELECT pa.idx, pa.kind, pa.state, pa.outcome, pa.updated_at, \
             json_extract(p.body, '$.actions[' || pa.idx || '].expected_gain') AS expected \
             FROM plan_actions pa JOIN plans p ON p.id = pa.plan_id WHERE pa.plan_id = ? ORDER BY pa.idx",
        )
        .bind(&pid)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let mut seen = HashSet::new();
        let mut actions = Vec::with_capacity(rows.len());
        for r in &rows {
            let idx = u32::try_from(g::<i64>(r, "idx")?).unwrap_or(0);
            let expected: removarr_domain::gain::GainEstimate = g::<Option<String>>(r, "expected")?
                .map(|s| json(&s, "expected gain"))
                .transpose()?
                .unwrap_or_default();
            let mine: Vec<&GainRow> = gains
                .iter()
                .filter(|x| x.action_idx == idx && seen.insert(x.gain_id.clone()))
                .collect();
            let at: i64 = g(r, "updated_at")?;
            let state: String = g(r, "state")?;
            let outcome_raw: Option<String> = g(r, "outcome")?;
            actions.push(ActionReportVm {
                index: idx,
                kind: clean(&g::<String>(r, "kind")?),
                outcome: outcome_vm(&state, outcome_raw.as_deref()),
                state,
                expected_logical: expected.logical,
                expected_attributable: expected.attributable,
                expected_added: expected.added,
                recorded_attributable: mine
                    .iter()
                    .filter(|x| !x.added)
                    .fold(0u64, |a, x| a.saturating_add(x.bytes)),
                recorded_added: mine
                    .iter()
                    .filter(|x| x.added)
                    .fold(0u64, |a, x| a.saturating_add(x.bytes)),
                gain_ids: mine.iter().map(|x| x.gain_id.clone()).collect(),
                updated_at: at,
                updated_at_iso: iso(at),
            });
        }
        let observed = self.observed_rows(Some(&pid), None).await?;
        let unfinished: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journal_intents i LEFT JOIN journal_results r ON r.intent_id = i.id \
             WHERE i.plan_id = ? AND i.mode = 'live' AND r.intent_id IS NULL",
        )
        .bind(&pid)
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        let observed_delta = (!observed.is_empty()).then(|| {
            observed
                .iter()
                .fold(0i64, |a, o| a.saturating_add(o.delta_local))
        });
        let planned_vm = PlanGainsVm::from(planned);
        let differences = explain::plan_differences(&PlanFacts {
            status: &status,
            mode: &mode,
            planned: planned_vm,
            recorded: &recorded,
            actions: &actions,
            observed_delta,
            unfinished_intents: nonneg(unfinished),
        });
        let remaining = explain::remaining_actions(&actions, &mode);
        Ok(PlanReportVm {
            plan_id: pid,
            hash: g(&p, "hash")?,
            status,
            mode,
            origin: g(&p, "origin")?,
            created_at: created,
            created_at_iso: iso(created),
            session: g(&p, "session_id")?,
            planned: planned_vm,
            recorded,
            observed,
            actions,
            differences,
            remaining,
        })
    }

    /// Everything an export needs for one session (default: the latest finished scan).
    /// Issues are always the current set: when they come from another session the bundle says
    /// so in its explanations instead of presenting them as that session's state.
    pub async fn report(
        &self,
        session: Option<SessionId>,
        inputs: &DashboardInputs,
    ) -> Result<ReportBundle, PortError> {
        let now = self.clock.now().as_millis();
        let dashboard = self.dashboard(inputs).await?;
        let session_vm = match session {
            Some(id) => Some(
                self.session(Some(id), inputs.scan_interval_minutes)
                    .await?
                    .ok_or_else(|| PortError::new(ErrorClass::NotFound, "session not found"))?,
            ),
            None => dashboard.last_scan.clone(),
        };
        let issues = self
            .issues(&IssueQuery {
                limit: MAX_LIST,
                ..IssueQuery::default()
            })
            .await?;
        let sid = session_vm.as_ref().and_then(|s| {
            removarr_domain::ids::RawUuid::parse_str(&s.id)
                .ok()
                .map(SessionId::new)
        });
        let history = self
            .history(&HistoryQuery {
                limit: MAX_LIST,
                session: sid,
                ..HistoryQuery::default()
            })
            .await?;
        let stats = self.stats(&StatsQuery::default()).await?;
        let plan_ids: Vec<String> = match &session_vm {
            Some(s) => sqlx::query_scalar(
                "SELECT id FROM plans WHERE session_id = ? ORDER BY created_at DESC LIMIT 50",
            )
            .bind(&s.id)
            .fetch_all(&self.pool)
            .await
            .map_err(store_err)?,
            None => vec![],
        };
        let mut plans = Vec::new();
        for id in plan_ids {
            let raw =
                removarr_domain::ids::RawUuid::parse_str(&id).map_err(|_| corrupt("plan id"))?;
            plans.push(self.plan_report(PlanId::new(raw)).await?);
        }
        let mut explanations = session_explanations(
            session_vm.as_ref(),
            issues.issues_session.as_deref(),
            &dashboard.services,
            dashboard
                .explanations
                .iter()
                .filter(|e| e.code == "issues.incomplete_evidence")
                .count() as u64,
        );
        for e in &dashboard.explanations {
            if e.code == "issues.incomplete_evidence" || e.code == "auto.paused" {
                explanations.push(e.clone());
            }
        }
        explanations.push(btrfs_caveat());
        Ok(ReportBundle {
            generated_at: now,
            generated_at_iso: iso(now),
            requested_session: session.map(|s| s.to_string()),
            session: session_vm,
            dashboard,
            issues,
            history,
            stats,
            plans,
            explanations,
        })
    }
}

type Events = Vec<(i64, i64)>;

struct GainRow {
    gain_id: String,
    action_idx: u32,
    added: bool,
    bytes: u64,
    at: i64,
}

/// The only place gain bytes are summed: each gain id counts once.
fn aggregate_gains(rows: &[GainRow], since: Option<i64>) -> (GainTotalsVm, Events, Events) {
    let mut seen: HashSet<&str> = HashSet::new();
    let (mut attr, mut added, mut dup) = (0u64, 0u64, 0u64);
    let (mut attr_ev, mut added_ev) = (Vec::new(), Vec::new());
    for r in rows {
        if !seen.insert(r.gain_id.as_str()) {
            dup += 1;
            continue;
        }
        let b = i64::try_from(r.bytes).unwrap_or(i64::MAX);
        if r.added {
            added = added.saturating_add(r.bytes);
        } else {
            attr = attr.saturating_add(r.bytes);
        }
        if since.is_none_or(|s| r.at >= s) {
            if r.added {
                added_ev.push((r.at, b))
            } else {
                attr_ev.push((r.at, b))
            }
        }
    }
    let net = i64::try_from(attr)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(added).unwrap_or(i64::MAX));
    (
        GainTotalsVm {
            attributable_bytes: attr,
            added_bytes: added,
            net_bytes: net,
            unique_gain_ids: seen.len() as u64,
            duplicate_gain_ids_ignored: dup,
        },
        attr_ev,
        added_ev,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn row(id: &str, added: bool, bytes: u64, at: i64) -> GainRow {
        GainRow {
            gain_id: id.into(),
            action_idx: 0,
            added,
            bytes,
            at,
        }
    }

    #[test]
    fn duplicate_gain_ids_are_counted_once() {
        let rows = [
            row("a", false, 100, 1),
            row("a", false, 100, 2),
            row("b", false, 50, 3),
            row("c", true, 30, 4),
            row("c", true, 30, 5),
        ];
        let (t, attr, added) = aggregate_gains(&rows, None);
        assert_eq!(
            (t.attributable_bytes, t.added_bytes, t.net_bytes),
            (150, 30, 120)
        );
        assert_eq!((t.unique_gain_ids, t.duplicate_gain_ids_ignored), (3, 2));
        assert_eq!(attr.len(), 2);
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn potential_is_counted_once_per_inode() {
        let mk = |id: &str, pot, ino| IssueCore {
            id: id.into(),
            category: IssueCategory::Junk,
            severity: Severity::Info,
            subject: id.into(),
            logical: pot,
            potential: pot,
            phys: vec![PhysicalId { dev: 1, ino }],
            incomplete: false,
        };
        let (a, b, c) = (mk("a", 100, 1), mk("b", 100, 1), mk("c", 40, 2));
        let (map, excl) = counted_potential(&[&a, &b, &c]);
        assert_eq!(map["a"] + map["b"] + map["c"], 140);
        assert_eq!(excl, 100);
    }

    #[test]
    fn urgency_rules_and_labels() {
        let u = compute_urgency(&UrgencyFacts::default());
        assert_eq!(u.level, Urgency::Green);
        assert!(!u.label.is_empty() && !u.icon.is_empty());
        let o = compute_urgency(&UrgencyFacts {
            warning_issues: 2,
            ..Default::default()
        });
        assert_eq!(o.level, Urgency::Orange);
        for f in [
            UrgencyFacts {
                critical_issues: 1,
                ..Default::default()
            },
            UrgencyFacts {
                scan_incomplete: true,
                ..Default::default()
            },
            UrgencyFacts {
                scan_stale: true,
                ..Default::default()
            },
            UrgencyFacts {
                scan_missing: true,
                ..Default::default()
            },
            UrgencyFacts {
                services_not_ok: 1,
                ..Default::default()
            },
            UrgencyFacts {
                torrents_left_stopped: 1,
                ..Default::default()
            },
            UrgencyFacts {
                recovery_pending: true,
                ..Default::default()
            },
            UrgencyFacts {
                auto_paused: true,
                warning_issues: 5,
                ..Default::default()
            },
        ] {
            let r = compute_urgency(&f);
            assert_eq!(r.level, Urgency::Red);
            assert!(!r.reasons.is_empty());
        }
    }

    #[test]
    fn series_are_daily_cumulative_and_bounded() {
        let ev = [(DAY_MS + 5, 10), (DAY_MS + 9, 5), (3 * DAY_MS, -4)];
        let (s, t) = build_series(SeriesKind::Observed, &ev, false, 10);
        assert!(!t);
        assert_eq!(
            s.points
                .iter()
                .map(|p| (p.bytes, p.cumulative))
                .collect::<Vec<_>>(),
            [(15, 15), (-4, 11)]
        );
        let (s, t) = build_series(SeriesKind::Observed, &ev, false, 1);
        assert!(t);
        assert_eq!(s.points.len(), 1);
        assert_eq!(s.total, 11);
    }

    #[test]
    fn target_description_keeps_only_rooted_paths_and_hashes() {
        let v = serde_json::json!({"Consolidate": {"source": {"root": "completed", "rel": "a/b.mkv"}, "target": {"root": "media-aabbccdd", "rel": "x.mkv"}}});
        let mut out = vec![];
        describe_target(&v, &mut out);
        assert_eq!(out, ["completed:a/b.mkv", "media-aabbccdd:x.mkv"]);
        let t =
            serde_json::json!({"Torrent": {"hash": "abc", "data_paths": [], "url": "http://t/x"}});
        let mut out = vec![];
        describe_target(&t, &mut out);
        assert_eq!(out, ["torrent:abc"]);
    }
}
