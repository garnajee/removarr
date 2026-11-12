//! View models handed to backend (16) and web (19). All are plain serde data: no secrets, no
//! absolute host paths, every historical observation carries its timestamp.
//!
//! Bytes are always `u64` unless a difference can be negative (`i64`). Text meant for people is
//! offered as a stable `key` (for i18n) plus, where an export needs it, an English `text`.

use std::collections::BTreeMap;

use removarr_domain::gain::PlanGains;
use removarr_domain::issue::{IssueCategory, Outcome, Severity};
use serde::Serialize;

/// Label key shown next to every potential-space number. Potential is never "recovered".
pub const POTENTIAL_LABEL_KEY: &str = "space.potential_not_guaranteed";
/// English fallback of [`POTENTIAL_LABEL_KEY`], used by exports.
pub const POTENTIAL_LABEL_EN: &str = "Potential space (estimate, not guaranteed to be recovered)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Green,
    Orange,
    Red,
}

impl Urgency {
    /// Text label and icon name: urgency is never conveyed by colour alone.
    #[must_use]
    pub fn label_key(self) -> &'static str {
        match self {
            Urgency::Green => "urgency.green",
            Urgency::Orange => "urgency.orange",
            Urgency::Red => "urgency.red",
        }
    }
    #[must_use]
    pub fn label_en(self) -> &'static str {
        match self {
            Urgency::Green => "OK",
            Urgency::Orange => "Attention",
            Urgency::Red => "Action required",
        }
    }
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Urgency::Green => "check-circle",
            Urgency::Orange => "alert-triangle",
            Urgency::Red => "alert-octagon",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UrgencyVm {
    pub level: Urgency,
    pub label_key: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub reasons: Vec<ReasonVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasonVm {
    pub key: &'static str,
    pub count: Option<u64>,
}

/// Four quantities, never merged (INV-21). `potential_bytes` counts each physical inode once
/// across all current issues; `potential_overlap_excluded` is what was left out because
/// another issue already counted the same inodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpaceVm {
    /// Sum of file sizes over every observed path (a hardlinked file counts once per path).
    pub logical_bytes: u64,
    /// Allocated bytes, each physical inode once.
    pub unique_allocated_bytes: u64,
    /// Allocated bytes of inodes reachable through two or more observed paths.
    pub shared_bytes: u64,
    /// Estimated reclaimable bytes of the current issues. Not guaranteed.
    pub potential_bytes: u64,
    pub potential_overlap_excluded_bytes: u64,
    pub potential_label_key: &'static str,
    /// When the inventory behind the first three numbers was observed (last scan end).
    pub observed_at: Option<i64>,
    pub observed_at_iso: Option<String>,
    pub roots: Vec<RootSpaceVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootSpaceVm {
    pub root: String,
    pub logical_bytes: u64,
    pub unique_allocated_bytes: u64,
    pub shared_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryCountVm {
    pub category: IssueCategory,
    pub count: u64,
    pub max_severity: Severity,
    pub info: u64,
    pub warning: u64,
    pub critical: u64,
    pub logical_bytes: u64,
    pub potential_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceVm {
    pub service: String,
    /// `ok`, `unavailable`, `auth_rejected`, `not_configured`, `mapping_errors`, `unknown`.
    pub state_key: &'static str,
    pub ok: bool,
    /// A configured service that is not Ok is a problem; `not_configured` is not.
    pub problem: bool,
    pub version: Option<String>,
    /// Sanitized reason or count text.
    pub detail: Option<String>,
    pub observed_at: i64,
    pub observed_at_iso: String,
    /// False for a state recorded during the last scan: historical, not live.
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionVm {
    pub id: String,
    pub started_at: i64,
    pub started_at_iso: String,
    pub finished_at: Option<i64>,
    pub finished_at_iso: Option<String>,
    pub status: String,
    pub complete: bool,
    pub age_ms: Option<i64>,
    pub stale: bool,
    pub entries_scanned: u64,
    pub scan_errors: u64,
    pub duration_ms: u64,
    pub roots: Vec<SessionRootVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRootVm {
    pub root: String,
    pub scanned: bool,
    pub error_count: u32,
    pub protected_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobVm {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub stage: Option<String>,
    pub enqueued_at: i64,
    pub started_at: Option<i64>,
}

/// Automatic mode status (T5-19): why it is paused is always shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoVm {
    pub enabled: bool,
    pub paused: bool,
    /// Sanitized reason persisted by the job runner under `auto_paused`.
    pub paused_reason: Option<String>,
    pub paused_at: Option<i64>,
    pub paused_at_iso: Option<String>,
    /// An automatic cycle was running when the process last stopped.
    pub cycle_marker_session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DashboardVm {
    pub generated_at: i64,
    pub generated_at_iso: String,
    pub urgency: UrgencyVm,
    pub categories: Vec<CategoryCountVm>,
    pub ignored_issues: u64,
    pub space: SpaceVm,
    pub services: Vec<ServiceVm>,
    pub last_scan: Option<SessionVm>,
    /// Session that produced the current issue set (can differ from `last_scan`).
    pub issues_session: Option<String>,
    pub running_jobs: Vec<JobVm>,
    pub torrents_left_stopped: u64,
    pub recovery_pending: bool,
    pub unfinished_intents: u64,
    pub auto: AutoVm,
    pub dry_run: bool,
    pub explanations: Vec<ExplanationVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompletenessVm {
    /// `complete`, `incomplete` or `unknown`.
    pub state: &'static str,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceVm {
    /// Evidence kind: `physical`, `same_physical`, `content`, `identity`, `quality`,
    /// `torrent_holds`, `service_unavailable`, `note`.
    pub kind: &'static str,
    pub summary: String,
    pub provenance: &'static str,
    pub observed_at: i64,
    pub observed_at_iso: String,
    /// True only for filesystem evidence of the session that produced the issue set and
    /// only while that session is the latest one. Everything else is a dated observation.
    pub current: bool,
    pub completeness: CompletenessVm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueRowVm {
    pub id: String,
    pub category: IssueCategory,
    pub severity: Severity,
    pub outcome: Outcome,
    pub subject: String,
    pub name: String,
    pub unit_kind: &'static str,
    /// `root:relative/path`, bounded. Never an absolute host path.
    pub paths: Vec<String>,
    pub logical_bytes: u64,
    /// Estimate only. See [`POTENTIAL_LABEL_KEY`].
    pub potential_bytes: u64,
    /// Portion of `potential_bytes` counted in the totals (0 when another issue already
    /// counted the same inodes).
    pub potential_counted_bytes: u64,
    pub potential_label_key: &'static str,
    pub torrent_count: usize,
    pub blockers: Vec<String>,
    pub completeness: CompletenessVm,
    pub evidence: Vec<EvidenceVm>,
    /// Inodes as `dev:ino` (inode-by-inode proof).
    pub inodes: Vec<String>,
    pub first_seen_at: i64,
    pub first_seen_at_iso: String,
    pub age_basis: i64,
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueListVm {
    pub rows: Vec<IssueRowVm>,
    pub total_count: u64,
    pub total_logical_bytes: u64,
    pub total_potential_bytes: u64,
    pub offset: u64,
    pub limit: u64,
    /// Issues are a snapshot of this session, observed at `session_finished_at`.
    pub issues_session: Option<String>,
    pub session_finished_at: Option<i64>,
    pub session_finished_at_iso: Option<String>,
    pub explanations: Vec<ExplanationVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutcomeVm {
    /// `verified`, `executed_unverified`, `failed`, `compensated`, `failed_needs_review`,
    /// `simulated`, `not_executed`, or the raw action state when no outcome is stored.
    pub key: String,
    pub success: bool,
    pub needs_attention: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryRowVm {
    pub plan_id: String,
    pub action_index: u32,
    pub action_kind: String,
    /// Exact outcome time (UTC ms) and its ISO form.
    pub at: i64,
    pub at_iso: String,
    pub started_at: Option<i64>,
    pub started_at_iso: Option<String>,
    /// Bounded `root:path` list, sanitized.
    pub target: String,
    pub reason: ReasonOfActionVm,
    pub outcome: OutcomeVm,
    pub gain_ids: Vec<String>,
    pub attributable_bytes: u64,
    pub added_bytes: u64,
    pub plan_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasonOfActionVm {
    /// `user` or `auto_policy`.
    pub origin: String,
    pub approved_by: Option<String>,
    pub approved_at: Option<i64>,
    pub approved_at_iso: Option<String>,
    pub plan_created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryVm {
    pub rows: Vec<HistoryRowVm>,
    pub offset: u64,
    pub limit: u64,
    pub has_more: bool,
    pub unfinished_intents: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesKind {
    /// Estimate of what could be reclaimed (current issue set). Not guaranteed.
    Potential,
    /// What approved or executed plans expected to free.
    Expected,
    /// Space freed only by this tool's removals, counted once per unique gain id.
    Attributable,
    /// New bytes written by actions (for instance an ARR copy instead of a hardlink).
    AddedCost,
    /// Free-space delta measured on the filesystem; may include other writers and snapshots.
    Observed,
}

impl SeriesKind {
    #[must_use]
    pub fn label_key(self) -> &'static str {
        match self {
            SeriesKind::Potential => "series.potential",
            SeriesKind::Expected => "series.expected",
            SeriesKind::Attributable => "series.attributable",
            SeriesKind::AddedCost => "series.added_cost",
            SeriesKind::Observed => "series.observed",
        }
    }
    #[must_use]
    pub fn label_en(self) -> &'static str {
        match self {
            SeriesKind::Potential => "Potential (estimate, not guaranteed)",
            SeriesKind::Expected => "Expected by plans",
            SeriesKind::Attributable => "Attributable (freed by removarr)",
            SeriesKind::AddedCost => "Added cost (new bytes written)",
            SeriesKind::Observed => "Observed free-space delta",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeriesPointVm {
    /// Start of the UTC day bucket (ms) and its ISO form.
    pub at: i64,
    pub at_iso: String,
    /// Bytes in this bucket (observed deltas can be negative).
    pub bytes: i64,
    pub cumulative: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeriesVm {
    pub kind: SeriesKind,
    pub label_key: &'static str,
    pub label: &'static str,
    pub points: Vec<SeriesPointVm>,
    pub total: i64,
    /// Only `Attributable` is measured; the others are estimates or mixed measurements.
    pub guaranteed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GainTotalsVm {
    pub attributable_bytes: u64,
    pub added_bytes: u64,
    pub net_bytes: i64,
    pub unique_gain_ids: u64,
    /// Rows ignored because their gain id was already counted (always 0 with the DB guard).
    pub duplicate_gain_ids_ignored: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedRowVm {
    pub plan_id: Option<String>,
    pub root: String,
    pub free_before: u64,
    pub free_after_local: u64,
    pub free_after_services: Option<u64>,
    pub delta_local: i64,
    pub at: i64,
    pub at_iso: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatsVm {
    pub generated_at: i64,
    pub series: Vec<SeriesVm>,
    pub totals: GainTotalsVm,
    pub observed: Vec<ObservedRowVm>,
    /// Keys of caveats to display next to the charts, for instance
    /// `caveat.btrfs_snapshots_may_retain_space`.
    pub caveats: Vec<ExplanationVm>,
    pub truncated: bool,
}

/// A structured explanation: stable `code`, parameters, and an English `text` for exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplanationVm {
    pub code: &'static str,
    pub params: BTreeMap<String, String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionReportVm {
    pub index: u32,
    pub kind: String,
    pub state: String,
    pub outcome: OutcomeVm,
    pub expected_logical: u64,
    pub expected_attributable: u64,
    pub expected_added: u64,
    pub recorded_attributable: u64,
    pub recorded_added: u64,
    pub gain_ids: Vec<String>,
    pub updated_at: i64,
    pub updated_at_iso: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemainingVm {
    pub index: u32,
    pub kind: String,
    pub state: String,
    /// `wait_or_recover`, `run_or_discard`, `verify_manually`, `review_manually`,
    /// `rescan_and_replan`.
    pub next_step: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReportVm {
    pub plan_id: String,
    pub hash: String,
    pub status: String,
    pub mode: String,
    pub origin: String,
    pub created_at: i64,
    pub created_at_iso: String,
    pub session: String,
    pub planned: PlanGainsVm,
    pub recorded: GainTotalsVm,
    pub observed: Vec<ObservedRowVm>,
    pub actions: Vec<ActionReportVm>,
    pub differences: Vec<ExplanationVm>,
    pub remaining: Vec<RemainingVm>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlanGainsVm {
    pub logical: u64,
    pub attributable: u64,
    pub added: u64,
    pub net: i64,
}

impl From<PlanGains> for PlanGainsVm {
    fn from(g: PlanGains) -> Self {
        Self {
            logical: g.logical,
            attributable: g.attributable,
            added: g.added,
            net: g.net,
        }
    }
}

/// Everything an export contains. Built by [`crate::Reporter::report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportBundle {
    pub generated_at: i64,
    pub generated_at_iso: String,
    pub requested_session: Option<String>,
    pub session: Option<SessionVm>,
    pub dashboard: DashboardVm,
    pub issues: IssueListVm,
    pub history: HistoryVm,
    pub stats: StatsVm,
    pub plans: Vec<PlanReportVm>,
    pub explanations: Vec<ExplanationVm>,
}
