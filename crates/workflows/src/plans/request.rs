//! Plan requests, settings and errors (architecture 7.3). A unit selection and a bulk selection
//! are the same request with one or many ids: there is exactly one planning function.

use std::collections::BTreeSet;

use removarr_domain::arr::{ArrMediaRef, ReleaseRef};
use removarr_domain::error::{DomainError, ErrorClass, PortError};
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{Blocker, IssueCategory};
use removarr_domain::plan::{PlanMode, PlanStatus};
use removarr_domain::policy::{Capability, RiskTier};
use removarr_domain::session::{ServiceKind, SessionStatus};
use removarr_matching::DurationTolerance;
use serde::{Deserialize, Serialize};

use super::expiry::StaleReason;
use crate::issues::IssueSettings;

/// What to do with the selected issues of one category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueAction {
    /// Replace the media copy by a hardlink to the completed source (`hardlink_missing` with
    /// outcome `Consolidate`, `duplicate`).
    Consolidate,
    /// Plain manual deletion (`orphan_completed`, `junk`, `incomplete_orphan`, and an unlinked
    /// `hardlink_missing` file the user decides to delete without ARR evidence).
    Delete,
    /// Deletion of a completed release that ARR reports as not an upgrade, whose replacement is
    /// physically present in a media root (`hardlink_missing`, `orphan_completed`; CP1-F02).
    DeleteReplaced,
    /// Stop, remove the task (`deleteFiles=false`), then delete the incomplete data (OQ-06).
    CleanupStalled,
    /// Targeted, non-recursive mode fix of an entry owned by the process (`permission`).
    FixMode,
    /// ARR imports the completed release (`importMode=copy`) after coverage and recovery proofs
    /// (`hardlink_missing`; ADR-0010). `delete_old_media` adds the optional `DeleteMediaFile`
    /// steps for old files ARR did not remove.
    ReplaceRelease { delete_old_media: bool },
    /// Remove an ARR queue entry, keeping media and torrent (`arr_queue`).
    RemoveQueueItem,
}

impl IssueAction {
    /// Categories each action applies to. Any other pairing rejects the request.
    #[must_use]
    pub fn accepts(self, category: IssueCategory) -> bool {
        use IssueCategory as C;
        match self {
            IssueAction::Consolidate => matches!(category, C::HardlinkMissing | C::Duplicate),
            IssueAction::Delete => matches!(
                category,
                C::OrphanCompleted | C::Junk | C::IncompleteOrphan | C::HardlinkMissing
            ),
            IssueAction::DeleteReplaced => {
                matches!(category, C::HardlinkMissing | C::OrphanCompleted)
            }
            IssueAction::CleanupStalled => category == C::IncompleteStalled,
            IssueAction::FixMode => category == C::Permission,
            IssueAction::ReplaceRelease { .. } => category == C::HardlinkMissing,
            IssueAction::RemoveQueueItem => category == C::ArrQueue,
        }
    }
}

/// Media-level actions from the media detail page (CP1-F05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    SetMonitored { monitored: bool },
}

/// The four request shapes of architecture 7.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanRequest {
    Issues {
        category: IssueCategory,
        issue_ids: Vec<IssueId>,
        action: IssueAction,
        /// Opt-in companion `RemoveTorrentTask` (CP1-F12). Default false.
        remove_torrent_tasks: bool,
        /// Specials (season 0) count in a coverage proof only when explicitly included.
        include_specials: bool,
    },
    Media {
        media: ArrMediaRef,
        action: MediaAction,
    },
    /// Grab of a listed search result (CP1-F06).
    Release {
        media: ArrMediaRef,
        release: ReleaseRef,
    },
    /// Service-side rescan: `Sonarr`/`Radarr` (`media = None` = whole library) or `Jellyfin`.
    ServiceRescan {
        service: ServiceKind,
        media: Option<ArrMediaRef>,
    },
}

impl PlanRequest {
    /// Convenience constructor for an issue selection with default companion options.
    #[must_use]
    pub fn issues(category: IssueCategory, issue_ids: Vec<IssueId>, action: IssueAction) -> Self {
        PlanRequest::Issues {
            category,
            issue_ids,
            action,
            remove_torrent_tasks: false,
            include_specials: false,
        }
    }
}

/// Planning configuration, built by the application from the validated configuration
/// (architecture 2.2: this crate never reads configuration itself).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSettings {
    /// `DryRun` in a dry-run process, `Live` otherwise (ADR-0003).
    pub mode: PlanMode,
    /// `Config::fingerprint()` (covers roots and path mappings).
    pub config_fingerprint: String,
    /// `thresholds.plan_ttl_hours`.
    pub plan_ttl_hours: u32,
    /// Enabled capability flags (section 13.2).
    pub capabilities: BTreeSet<Capability>,
    /// Highest tier the action runner implements (`workflows::actions::IMPLEMENTED_TIER`,
    /// raised by 13 after CP2). Kinds above it carry `NotInBuild`. Tier 5 enables auto plans.
    pub implemented_tier: RiskTier,
    /// Effective uid of the process: `FixMode` only changes entries it owns.
    pub process_uid: u32,
    /// `thresholds.auto_min_age_hours` (A3).
    pub auto_min_age_hours: u32,
    /// `matching.duration_tolerance_*` (replacement checks).
    pub duration_tolerance: DurationTolerance,
    /// File classes and expected owner (shared with the classifier).
    pub files: IssueSettings,
}

impl PlanSettings {
    /// Architecture defaults: dry-run, capabilities at their default values, 24 h TTL.
    #[must_use]
    pub fn new(config_fingerprint: impl Into<String>, files: IssueSettings) -> Self {
        Self {
            mode: PlanMode::DryRun,
            config_fingerprint: config_fingerprint.into(),
            plan_ttl_hours: 24,
            capabilities: ALL_CAPABILITIES
                .into_iter()
                .filter(|c| c.default_enabled())
                .collect(),
            implemented_tier: RiskTier::T1,
            process_uid: files.expected_owner.0,
            auto_min_age_hours: 48,
            duration_tolerance: DurationTolerance::default(),
            files,
        }
    }
}

/// Every capability flag, in table order.
pub const ALL_CAPABILITIES: [Capability; 9] = [
    Capability::Consolidate,
    Capability::DeleteCompleted,
    Capability::CleanupIncomplete,
    Capability::FixPermissions,
    Capability::ArrManualImport,
    Capability::ArrQueueRemove,
    Capability::ArrSetMonitored,
    Capability::ArrGrab,
    Capability::AutoRemoveEmptyTorrents,
];

/// Why a request produced no plan.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error("invalid plan request: {0}")]
    InvalidRequest(String),
    /// Unknown or resolved ids, ids of an older session, ignored ids, or ids of another
    /// category: the whole request is rejected with the offending ids (CON-32, INV-24).
    #[error("rejected selection ({reason}): {} id(s)", ids.len())]
    RejectedSelection {
        reason: SelectionFault,
        ids: Vec<IssueId>,
    },
    /// Plans are never built from a session that did not finish (OQ-05).
    #[error("session status {0:?} does not allow planning")]
    SessionNotPlannable(SessionStatus),
    #[error("no completed scan session")]
    NoSession,
    /// Every selected item is blocked. The blocked items and their reasons are listed.
    #[error("nothing plannable: {} item(s) blocked", excluded.len())]
    NothingPlannable {
        excluded: Vec<(SubjectKey, Vec<Blocker>)>,
    },
    /// Automatic mode is off or paused (A1).
    #[error("automatic planning refused: {0}")]
    AutoRefused(String),
    #[error(transparent)]
    Domain(#[from] DomainError),
    #[error(transparent)]
    Port(#[from] PortError),
    /// The plan no longer matches the world: it was moved to `status` (never adapted).
    #[error("plan is stale ({status:?}): {} reason(s)", reasons.len())]
    Stale {
        status: PlanStatus,
        reasons: Vec<StaleReason>,
    },
    /// A built plan broke a plan-engine rule (a bug, never shown as a valid plan).
    #[error("internal plan rule violation: {0:?}")]
    RuleViolation(Vec<String>),
}

/// Why a selection was rejected as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionFault {
    UnknownOrResolved,
    Stale,
    Ignored,
    WrongCategory,
}

impl std::fmt::Display for SelectionFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SelectionFault::UnknownOrResolved => "unknown or resolved",
            SelectionFault::Stale => "stale",
            SelectionFault::Ignored => "ignored",
            SelectionFault::WrongCategory => "wrong category",
        })
    }
}

impl PlanError {
    /// Architecture error class (section 10.1).
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            PlanError::InvalidRequest(_) | PlanError::NothingPlannable { .. } => {
                ErrorClass::Validation
            }
            PlanError::RejectedSelection {
                reason: SelectionFault::UnknownOrResolved,
                ..
            } => ErrorClass::NotFound,
            PlanError::RejectedSelection { .. } => ErrorClass::Validation,
            PlanError::SessionNotPlannable(_) | PlanError::NoSession | PlanError::Stale { .. } => {
                ErrorClass::Conflict
            }
            PlanError::AutoRefused(_) => ErrorClass::CapabilityDisabled,
            PlanError::Domain(e) => e.class(),
            PlanError::Port(e) => e.class.clone(),
            PlanError::RuleViolation(_) => ErrorClass::Internal,
        }
    }
}
