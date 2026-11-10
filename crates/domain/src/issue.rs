//! Issues: classified findings with evidence and blockers.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::evidence::{Completeness, Evidence, combined_completeness};
use crate::fs::PhysicalId;
use crate::ids::{IssueId, SessionId, SubjectKey};
use crate::path::RootedPath;
use crate::session::ServiceKind;
use crate::time::Timestamp;
use crate::torrent::TorrentHash;

/// Stable category names (URLs, database and i18n keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCategory {
    IncompleteStalled,
    IncompleteOrphan,
    HardlinkMissing,
    Duplicate,
    OrphanCompleted,
    TorrentMissingFiles,
    MediaWithoutSource,
    Permission,
    Junk,
    TorrentProtected,
    UnmappedTorrent,
    Symlink,
    ArrQueue,
}

impl IssueCategory {
    pub const ALL: [IssueCategory; 13] = [
        IssueCategory::IncompleteStalled,
        IssueCategory::IncompleteOrphan,
        IssueCategory::HardlinkMissing,
        IssueCategory::Duplicate,
        IssueCategory::OrphanCompleted,
        IssueCategory::TorrentMissingFiles,
        IssueCategory::MediaWithoutSource,
        IssueCategory::Permission,
        IssueCategory::Junk,
        IssueCategory::TorrentProtected,
        IssueCategory::UnmappedTorrent,
        IssueCategory::Symlink,
        IssueCategory::ArrQueue,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCategory::IncompleteStalled => "incomplete_stalled",
            IssueCategory::IncompleteOrphan => "incomplete_orphan",
            IssueCategory::HardlinkMissing => "hardlink_missing",
            IssueCategory::Duplicate => "duplicate",
            IssueCategory::OrphanCompleted => "orphan_completed",
            IssueCategory::TorrentMissingFiles => "torrent_missing_files",
            IssueCategory::MediaWithoutSource => "media_without_source",
            IssueCategory::Permission => "permission",
            IssueCategory::Junk => "junk",
            IssueCategory::TorrentProtected => "torrent_protected",
            IssueCategory::UnmappedTorrent => "unmapped_torrent",
            IssueCategory::Symlink => "symlink",
            IssueCategory::ArrQueue => "arr_queue",
        }
    }

    pub fn parse(s: &str) -> Result<Self, DomainError> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| DomainError::InvalidIssueCategory(s.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Consolidate,
    DeleteReplaced,
    KeepManual,
    KeepSample,
}

/// What an issue (and a plan action) applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unit {
    File(RootedPath),
    Folder {
        dir: RootedPath,
        members: Vec<RootedPath>,
        annexes: Vec<RootedPath>,
    },
    Torrent(TorrentHash),
    Pair {
        a: RootedPath,
        b: RootedPath,
    },
}

/// Why an item cannot be planned or executed (architecture section 4.3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BlockerCode {
    ScanIncomplete,
    ScanErrorUnder,
    ServiceUnavailable(ServiceKind),
    ServiceNotConfigured(ServiceKind),
    MappingError,
    UnmappedTorrents,
    TorrentProtectedState,
    TorrentReferencesTarget,
    Sample,
    Recent,
    Ignored,
    ArrMissing,
    ArrAmbiguous,
    ArrRejected,
    ArrNotUpgradeEvidence,
    CoverageIncomplete,
    DurationMismatch,
    AudioLanguageMissing,
    EditionMismatch,
    PermissionDenied,
    CrossDevice,
    FailedConsolidation,
    ImportInProgress,
    ContentDiffers,
    DuplicateTarget,
    MediaRootDeletion,
    CapabilityDisabled,
    NotInBuild,
    DryRunOnly,
    ProtectedZone,
    Symlink,
    NumberingMismatch,
    QueueItemHidesError,
    ReleaseNotInSearch,
    ArrImportMoved,
    OldCoverageUnrecoverable,
    ArrHardlinksDisabled,
}

impl BlockerCode {
    /// `ArrHardlinksDisabled` is a warning shown to the user, not a refusal (CP1-F20).
    #[must_use]
    pub fn is_warning(&self) -> bool {
        matches!(self, BlockerCode::ArrHardlinksDisabled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub code: BlockerCode,
    pub detail: String,
}

impl Blocker {
    #[must_use]
    pub fn new(code: BlockerCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// True when at least one blocker is a refusal (warnings do not count).
#[must_use]
pub fn has_refusal(blockers: &[Blocker]) -> bool {
    blockers.iter().any(|b| !b.code.is_warning())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: IssueId,
    pub category: IssueCategory,
    pub subject: SubjectKey,
    pub unit: Unit,
    pub severity: Severity,
    pub outcome: Outcome,
    pub evidence: Vec<Evidence>,
    pub blockers: Vec<Blocker>,
    pub torrent_hashes: BTreeSet<TorrentHash>,
    pub logical_bytes: u64,
    /// Potentially recoverable bytes. A label for display, never a guarantee and never an
    /// authorization to delete (INV-21).
    pub potential_bytes: u64,
    pub age_basis: Timestamp,
    pub first_seen_at: Timestamp,
    pub session: SessionId,
    /// Physical identities behind `potential_bytes`, for cross-category deduplication (OQ-04).
    pub physical_ids: BTreeSet<PhysicalId>,
}

impl Issue {
    /// True when nothing refuses the item. This is a precondition for planning, not an
    /// authorization: plans also need evidence, ARR checks and approval.
    #[must_use]
    pub fn is_unblocked(&self) -> bool {
        !has_refusal(&self.blockers)
    }

    #[must_use]
    pub fn completeness(&self) -> Completeness {
        combined_completeness(&self.evidence)
    }

    /// Recomputes the id from category and subject. Used to validate ids from the database.
    #[must_use]
    pub fn expected_id(&self) -> IssueId {
        IssueId::derive(self.category, &self.subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_names_round_trip_and_match_serde() {
        for c in IssueCategory::ALL {
            assert_eq!(IssueCategory::parse(c.as_str()).unwrap(), c);
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
        }
        assert!(IssueCategory::parse("Junk").is_err());
        assert_eq!(IssueCategory::ALL.len(), 13);
    }

    #[test]
    fn warnings_do_not_refuse() {
        let w = Blocker::new(BlockerCode::ArrHardlinksDisabled, "copy mode");
        let r = Blocker::new(BlockerCode::OldCoverageUnrecoverable, "no recycle bin");
        assert!(!has_refusal(std::slice::from_ref(&w)));
        assert!(has_refusal(&[w, r]));
    }

    #[test]
    fn blocker_codes_serialize_with_service() {
        let j =
            serde_json::to_string(&BlockerCode::ServiceUnavailable(ServiceKind::Radarr)).unwrap();
        assert_eq!(j, r#"{"ServiceUnavailable":"radarr"}"#);
        assert_eq!(
            serde_json::to_string(&BlockerCode::Sample).unwrap(),
            "\"Sample\""
        );
    }
}
