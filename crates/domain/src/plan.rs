//! Immutable action plans, ordered actions, approvals and expiration.
//!
//! An [`ActionPlan`] is hashed over its canonical JSON. [`HashedPlan`] seals a validated plan
//! with its hash, so a plan that was approved cannot silently change. A potential gain is never
//! an authorization: approval looks at evidence, counts and acknowledgements, not at bytes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::arr::{ArrMediaRef, ArrService, ManualImportRequest, OldFileRecovery, ReleaseRef};
use crate::canonical::{canonical_json, sha256_hex};
use crate::error::DomainError;
use crate::evidence::{Completeness, EvidenceLevel};
use crate::fs::{DirSnapshot, EntryKind, EntrySignature, PhysicalId};
use crate::gain::{GainEstimate, PlanGains};
use crate::ids::{PlanId, SessionId, SubjectKey};
use crate::issue::Blocker;
use crate::path::{RootId, RootKind, RootedPath};
use crate::policy::{Capability, RiskTier};
use crate::session::ServiceKind;
use crate::time::Timestamp;
use crate::torrent::{TorrentHash, TorrentState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    DryRun,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOrigin {
    User,
    AutoPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    ConsolidateHardlink,
    DeleteCompletedFile,
    DeleteCompletedFolder,
    DeleteJunk,
    RemoveEmptyParent,
    RemoveTorrentTask,
    CleanupStalledIncomplete,
    DeleteIncompleteOrphan,
    FixMode,
    ReplaceRelease,
    /// Valid only inside a `ReplaceRelease` plan, after the verified import.
    DeleteMediaFile,
    ArrGrabRelease,
    ArrRemoveQueueItem,
    ArrSetMonitored,
    ArrRescan,
    JellyfinRefresh,
}

impl ActionKind {
    /// Risk tier at which the kind becomes available (section 13.1). Tier 5 is the automatic
    /// mode, not an action tier; see [`ActionKind::is_auto_eligible`].
    #[must_use]
    pub fn tier(self) -> RiskTier {
        use ActionKind::*;
        match self {
            ConsolidateHardlink | ArrRescan | JellyfinRefresh => RiskTier::T2,
            DeleteCompletedFile
            | DeleteCompletedFolder
            | DeleteJunk
            | RemoveEmptyParent
            | RemoveTorrentTask
            | CleanupStalledIncomplete
            | DeleteIncompleteOrphan
            | FixMode => RiskTier::T3,
            ReplaceRelease | DeleteMediaFile | ArrGrabRelease | ArrRemoveQueueItem
            | ArrSetMonitored => RiskTier::T4,
        }
    }

    /// Capability flag required to plan and execute the kind; `None` = always allowed in live mode
    /// (never in dry-run).
    #[must_use]
    pub fn capability(self) -> Option<Capability> {
        use ActionKind::*;
        match self {
            ConsolidateHardlink => Some(Capability::Consolidate),
            DeleteCompletedFile
            | DeleteCompletedFolder
            | DeleteJunk
            | RemoveEmptyParent
            | RemoveTorrentTask => Some(Capability::DeleteCompleted),
            CleanupStalledIncomplete | DeleteIncompleteOrphan => {
                Some(Capability::CleanupIncomplete)
            }
            FixMode => Some(Capability::FixPermissions),
            ReplaceRelease | DeleteMediaFile => Some(Capability::ArrManualImport),
            ArrRemoveQueueItem => Some(Capability::ArrQueueRemove),
            ArrSetMonitored => Some(Capability::ArrSetMonitored),
            ArrGrabRelease => Some(Capability::ArrGrab),
            ArrRescan | JellyfinRefresh => None,
        }
    }

    /// Only these kinds may ever be part of an automatic plan (section 13.1 tier 5, rule A9).
    #[must_use]
    pub fn is_auto_eligible(self) -> bool {
        use ActionKind::*;
        matches!(
            self,
            ConsolidateHardlink
                | DeleteCompletedFile
                | DeleteCompletedFolder
                | RemoveEmptyParent
                | RemoveTorrentTask
        )
    }

    /// Companion actions are added automatically and do not count in the typed confirmation.
    #[must_use]
    pub fn is_companion(self) -> bool {
        matches!(
            self,
            ActionKind::RemoveEmptyParent | ActionKind::RemoveTorrentTask
        )
    }

    /// Whether the kind is consistent with the target shape.
    #[must_use]
    pub fn accepts(self, target: &ActionTarget) -> bool {
        use ActionKind::*;
        use ActionTarget as T;
        matches!(
            (self, target),
            (ConsolidateHardlink, T::Consolidate { .. })
                | (
                    DeleteCompletedFile
                        | DeleteCompletedFolder
                        | DeleteJunk
                        | RemoveEmptyParent
                        | DeleteIncompleteOrphan
                        | DeleteMediaFile,
                    T::Path { .. }
                )
                | (FixMode, T::FixMode { .. })
                | (
                    RemoveTorrentTask | CleanupStalledIncomplete,
                    T::Torrent { .. }
                )
                | (ReplaceRelease, T::Replace { .. })
                | (ArrGrabRelease, T::Release { .. })
                | (ArrRemoveQueueItem, T::Queue { .. })
                | (ArrSetMonitored, T::Monitor { .. })
                | (ArrRescan, T::Rescan { .. })
                | (JellyfinRefresh, T::JellyfinRefresh { .. })
        )
    }
}

/// What an action operates on. Every path is root-relative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionTarget {
    /// `source` is in completed; `target` (a media path) is replaced by a hardlink to it.
    Consolidate {
        source: RootedPath,
        target: RootedPath,
    },
    Path {
        path: RootedPath,
    },
    FixMode {
        path: RootedPath,
        mode: u32,
    },
    /// `data_paths` is empty for `RemoveTorrentTask` and lists the incomplete data for
    /// `CleanupStalledIncomplete`.
    Torrent {
        hash: TorrentHash,
        data_paths: Vec<RootedPath>,
    },
    /// The import request has no mode field (the adapter always copies).
    Replace {
        request: ManualImportRequest,
    },
    Release {
        media: ArrMediaRef,
        release: ReleaseRef,
    },
    Queue {
        service: ArrService,
        id: i64,
    },
    Monitor {
        media: ArrMediaRef,
        monitored: bool,
    },
    Rescan {
        service: ArrService,
        media: Option<ArrMediaRef>,
    },
    JellyfinRefresh {
        item_id: Option<String>,
    },
}

impl ActionTarget {
    /// Roots the action touches on disk.
    #[must_use]
    pub fn roots(&self) -> BTreeSet<RootId> {
        let mut roots = BTreeSet::new();
        match self {
            ActionTarget::Consolidate { source, target } => {
                roots.insert(source.root.clone());
                roots.insert(target.root.clone());
            }
            ActionTarget::Path { path } | ActionTarget::FixMode { path, .. } => {
                roots.insert(path.root.clone());
            }
            ActionTarget::Torrent { data_paths, .. } => {
                roots.extend(data_paths.iter().map(|p| p.root.clone()));
            }
            ActionTarget::Replace { request } => {
                roots.extend(request.files.iter().map(|(p, _)| p.root.clone()));
            }
            _ => {}
        }
        roots
    }
}

/// Conditions revalidated before the first mutation. Any difference invalidates the action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Precondition {
    EntryUnchanged {
        path: RootedPath,
        sig: EntrySignature,
        compare_nlink: bool,
    },
    DirUnchanged {
        snapshot: DirSnapshot,
    },
    NoScanErrorUnder {
        path: RootedPath,
    },
    TorrentStates {
        states: BTreeMap<TorrentHash, (TorrentState, f64)>,
    },
    NoTorrentReferences {
        path: RootedPath,
    },
    ArrFingerprint {
        media: ArrMediaRef,
        fingerprint: String,
    },
    NoArrImport {
        media: ArrMediaRef,
        hashes: BTreeSet<TorrentHash>,
    },
    SameDevice {
        a: RootedPath,
        b: RootedPath,
    },
    ServiceAvailable(ServiceKind),
    /// ARR's current file for `media` is `path`, with `size` bytes (CP1-F02, "replacement
    /// present"). It must be paired with an `EntryUnchanged` on the same path.
    ArrReplacementPresent {
        media: ArrMediaRef,
        path: RootedPath,
        size: u64,
    },
    ConfigFingerprint(String),
    /// Every old media file that ARR's upgrade import will delete, with where its content
    /// survives (CP1-F20).
    OldCoverageRecoverable {
        old_files: Vec<(RootedPath, PhysicalId, OldFileRecovery)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationStep {
    InodeEquals {
        path: RootedPath,
        phys: PhysicalId,
    },
    Absent {
        path: RootedPath,
    },
    RecheckVerified {
        hashes: BTreeSet<TorrentHash>,
    },
    TorrentAbsent {
        hashes: BTreeSet<TorrentHash>,
    },
    ArrCurrentFile {
        media: ArrMediaRef,
        path: RootedPath,
    },
    JellyfinRecognized {
        path: RootedPath,
    },
    ModeEquals {
        path: RootedPath,
        mode: u32,
    },
    /// The completed source is unchanged after an ARR import (a move would be detected).
    PathStillHasInode {
        path: RootedPath,
        phys: PhysicalId,
    },
    ArrQueueHasRelease {
        service: ArrService,
        release: ReleaseRef,
    },
    /// The ARR queue item is gone on re-read.
    ArrQueueItemAbsent {
        service: ArrService,
        id: i64,
    },
    /// The re-read monitored flag of the media equals the requested one.
    ArrMonitored {
        media: ArrMediaRef,
        monitored: bool,
    },
    /// The ARR rescan command was observed completed.
    ArrCommandCompleted {
        service: ArrService,
        media: Option<ArrMediaRef>,
    },
    /// The Jellyfin refresh task was observed completed.
    JellyfinRefreshed {
        item_id: Option<String>,
    },
    /// Where a replaced old media file went (recycle bin, other link, or nowhere).
    OldFileAccounted {
        path: RootedPath,
        recovery: OldFileRecovery,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedAction {
    pub index: u32,
    pub kind: ActionKind,
    pub target: ActionTarget,
    pub preconditions: Vec<Precondition>,
    pub verification: Vec<VerificationStep>,
    pub expected_gain: GainEstimate,
    /// Indexes of earlier actions that must be verified first.
    pub depends_on: Vec<u32>,
    pub torrent_hashes: BTreeSet<TorrentHash>,
    /// Strength of the content proof behind this action.
    pub evidence_level: EvidenceLevel,
    /// `Complete` only when every needed observation was actually obtained.
    pub completeness: Completeness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrSnapshotRef {
    pub media: ArrMediaRef,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionPlan {
    pub id: PlanId,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub mode: PlanMode,
    pub origin: PlanOrigin,
    pub session: SessionId,
    pub config_fingerprint: String,
    pub actions: Vec<PlannedAction>,
    pub excluded: Vec<(SubjectKey, Vec<Blocker>)>,
    pub lock_roots: BTreeSet<RootId>,
    pub torrent_states: BTreeMap<TorrentHash, (TorrentState, f64)>,
    pub arr_snapshots: Vec<ArrSnapshotRef>,
    /// Samples inside folder units of this plan. Each needs an explicit acknowledgement (OQ-11).
    pub samples: Vec<RootedPath>,
    pub gains: PlanGains,
}

impl ActionPlan {
    /// Checks the structural invariants of a plan. Does not look at evidence or approval.
    pub fn validate(&self) -> Result<(), DomainError> {
        let bad = |m: String| Err(DomainError::InvalidPlan(m));
        if self.actions.is_empty() {
            return bad("a plan needs at least one action".into());
        }
        if self.expires_at <= self.created_at {
            return bad("expires_at must be after created_at".into());
        }
        let has_replace = self
            .actions
            .iter()
            .any(|a| a.kind == ActionKind::ReplaceRelease);
        for (pos, a) in self.actions.iter().enumerate() {
            if usize::try_from(a.index).ok() != Some(pos) {
                return bad(format!("action at position {pos} has index {}", a.index));
            }
            if !a.kind.accepts(&a.target) {
                return bad(format!(
                    "action {pos}: target does not fit kind {:?}",
                    a.kind
                ));
            }
            if a.depends_on.iter().any(|d| *d >= a.index) {
                return bad(format!(
                    "action {pos}: depends_on must reference earlier actions"
                ));
            }
            if self.origin == PlanOrigin::AutoPolicy && !a.kind.is_auto_eligible() {
                return bad(format!("action {pos}: {:?} can never be automatic", a.kind));
            }
            if a.kind == ActionKind::DeleteMediaFile {
                let after_import = a.depends_on.iter().any(|d| {
                    self.actions
                        .get(usize::try_from(*d).unwrap_or(usize::MAX))
                        .is_some_and(|x| x.kind == ActionKind::ReplaceRelease)
                });
                if !has_replace || !after_import {
                    return bad(format!(
                        "action {pos}: DeleteMediaFile must depend on a ReplaceRelease"
                    ));
                }
            }
            if let Some(missing) = a
                .target
                .roots()
                .into_iter()
                .find(|r| !self.lock_roots.contains(r))
            {
                return bad(format!("action {pos}: root {missing} is not in lock_roots"));
            }
        }
        Ok(())
    }

    /// Number of items the user must confirm by typing: every action except companions.
    #[must_use]
    pub fn required_count(&self) -> u32 {
        let n = self
            .actions
            .iter()
            .filter(|a| !a.kind.is_companion())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Torrent task removals, confirmed separately (DR-35).
    #[must_use]
    pub fn required_torrent_removals(&self) -> u32 {
        let n = self
            .actions
            .iter()
            .filter(|a| a.kind == ActionKind::RemoveTorrentTask)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Old media files with no surviving copy that need a per-file acknowledgement (CP1-F20).
    #[must_use]
    pub fn unrecoverable_old_files(&self) -> BTreeSet<RootedPath> {
        let mut out = BTreeSet::new();
        for a in &self.actions {
            for p in &a.preconditions {
                if let Precondition::OldCoverageRecoverable { old_files } = p {
                    out.extend(
                        old_files
                            .iter()
                            .filter(|(_, _, r)| *r == OldFileRecovery::Unrecoverable)
                            .map(|(path, _, _)| path.clone()),
                    );
                }
            }
        }
        out
    }

    #[must_use]
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at
    }
}

/// SHA-256 hex of the canonical JSON of an [`ActionPlan`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanHash(pub String);

impl PlanHash {
    pub fn compute(plan: &ActionPlan) -> Result<Self, DomainError> {
        Ok(Self(sha256_hex(&canonical_json(plan)?)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated plan sealed with its hash. There are no mutable accessors.
#[derive(Debug, Clone, PartialEq)]
pub struct HashedPlan {
    plan: ActionPlan,
    hash: PlanHash,
}

impl HashedPlan {
    /// Validates the plan and computes its hash.
    pub fn seal(plan: ActionPlan) -> Result<Self, DomainError> {
        plan.validate()?;
        let hash = PlanHash::compute(&plan)?;
        Ok(Self { plan, hash })
    }

    /// Rebuilds a plan loaded from storage. Fails if the stored hash does not match the content.
    pub fn from_parts(plan: ActionPlan, hash: PlanHash) -> Result<Self, DomainError> {
        plan.validate()?;
        let actual = PlanHash::compute(&plan)?;
        if actual != hash {
            return Err(DomainError::PlanHashMismatch {
                expected: hash.0,
                actual: actual.0,
            });
        }
        Ok(Self { plan, hash })
    }

    #[must_use]
    pub fn plan(&self) -> &ActionPlan {
        &self.plan
    }

    #[must_use]
    pub fn hash(&self) -> &PlanHash {
        &self.hash
    }

    #[must_use]
    pub fn into_parts(self) -> (ActionPlan, PlanHash) {
        (self.plan, self.hash)
    }

    /// Manual approval. Rejects a different hash, an expired plan, a wrong typed count, and any
    /// sample or unrecoverable old file that is not individually acknowledged.
    pub fn approve(&self, req: &ApprovalRequest, now: Timestamp) -> Result<Approval, DomainError> {
        if req.plan_hash != self.hash {
            return Err(DomainError::PlanHashMismatch {
                expected: self.hash.0.clone(),
                actual: req.plan_hash.0.clone(),
            });
        }
        self.check_not_expired(now)?;
        let (count, removals) = (
            self.plan.required_count(),
            self.plan.required_torrent_removals(),
        );
        if req.confirmed_count != count {
            return Err(DomainError::ApprovalMismatch {
                field: "confirmed_count",
                expected: count.to_string(),
                actual: req.confirmed_count.to_string(),
            });
        }
        if req.confirmed_torrent_removals != removals {
            return Err(DomainError::ApprovalMismatch {
                field: "confirmed_torrent_removals",
                expected: removals.to_string(),
                actual: req.confirmed_torrent_removals.to_string(),
            });
        }
        let samples = acknowledged(
            self.plan.samples.iter().cloned().collect(),
            &req.acknowledged_samples,
        )
        .map_err(DomainError::SamplesNotAcknowledged)?;
        let unrecoverable = acknowledged(
            self.plan.unrecoverable_old_files(),
            &req.acknowledged_unrecoverable,
        )
        .map_err(DomainError::UnrecoverableNotAcknowledged)?;
        Ok(Approval {
            plan_id: self.plan.id,
            plan_hash: self.hash.clone(),
            approved_at: now,
            by: Approver::User,
            confirmed_count: count,
            confirmed_torrent_removals: removals,
            acknowledged_samples: samples,
            acknowledged_unrecoverable: unrecoverable,
        })
    }

    /// Automatic approval by policy. Refused unless the plan was built by the auto policy, holds
    /// only auto-eligible kinds, needs no acknowledgement, and **every action has complete
    /// evidence of a known level**. Gains are never consulted.
    pub fn approve_auto(&self, now: Timestamp) -> Result<Approval, DomainError> {
        let forbid = |m: &str| Err(DomainError::AutoApprovalForbidden(m.to_string()));
        if self.plan.origin != PlanOrigin::AutoPolicy {
            return forbid("plan was not built by the auto policy");
        }
        self.check_not_expired(now)?;
        if !self.plan.samples.is_empty() {
            return forbid("samples need a manual acknowledgement");
        }
        if !self.plan.unrecoverable_old_files().is_empty() {
            return forbid("unrecoverable old files need a manual acknowledgement");
        }
        for a in &self.plan.actions {
            if !a.kind.is_auto_eligible() {
                return forbid("action kind is not auto-eligible");
            }
            if !a.completeness.is_complete() || a.evidence_level == EvidenceLevel::None {
                return Err(DomainError::IncompleteEvidence {
                    action_index: a.index,
                });
            }
            if a.evidence_level == EvidenceLevel::ArrValidatedReplacement {
                check_arr_validated_deletion(a)?;
            }
        }
        Ok(Approval {
            plan_id: self.plan.id,
            plan_hash: self.hash.clone(),
            approved_at: now,
            by: Approver::AutoPolicy,
            confirmed_count: self.plan.required_count(),
            confirmed_torrent_removals: self.plan.required_torrent_removals(),
            acknowledged_samples: vec![],
            acknowledged_unrecoverable: vec![],
        })
    }

    fn check_not_expired(&self, now: Timestamp) -> Result<(), DomainError> {
        if self.plan.is_expired(now) {
            Err(DomainError::PlanExpired {
                expires_at: self.plan.expires_at,
                now,
            })
        } else {
            Ok(())
        }
    }
}

/// The `ArrValidatedReplacement` level is a deletion-only proof (A7, CP1-F02): the target is
/// under the completed root and every replacement ARR reports is pinned, physically present
/// in a media root with ARR's size, and a different inode from every deletion target.
fn check_arr_validated_deletion(a: &PlannedAction) -> Result<(), DomainError> {
    let forbid = |m: &str| Err(DomainError::AutoApprovalForbidden(m.to_string()));
    let missing = || {
        Err(DomainError::IncompleteEvidence {
            action_index: a.index,
        })
    };
    let target = match (&a.kind, &a.target) {
        (
            ActionKind::DeleteCompletedFile | ActionKind::DeleteCompletedFolder,
            ActionTarget::Path { path },
        ) => path,
        _ => return forbid("ARR-validated evidence only covers completed deletions"),
    };
    if target.root != RootId::completed() {
        return forbid("ARR-validated deletion is limited to the completed root");
    }
    let replacements: Vec<(&ArrMediaRef, &RootedPath, u64)> = a
        .preconditions
        .iter()
        .filter_map(|p| match p {
            Precondition::ArrReplacementPresent { media, path, size } => Some((media, path, *size)),
            _ => None,
        })
        .collect();
    if replacements.is_empty() {
        return missing();
    }
    let mut replacement_phys = Vec::new();
    for (media, path, size) in &replacements {
        if path.root.kind() != RootKind::Media {
            return missing();
        }
        let pinned = a.preconditions.iter().find_map(|p| match p {
            Precondition::EntryUnchanged {
                path: q,
                sig,
                compare_nlink: false,
            } if q == *path => Some(sig),
            _ => None,
        });
        let Some(sig) = pinned else {
            return missing();
        };
        let fingerprinted = a
            .preconditions
            .iter()
            .any(|p| matches!(p, Precondition::ArrFingerprint { media: m, .. } if m == *media));
        if sig.kind != EntryKind::File || sig.size != *size || !fingerprinted {
            return missing();
        }
        replacement_phys.push(sig.phys());
    }
    let replacement_paths: BTreeSet<&RootedPath> = replacements.iter().map(|r| r.1).collect();
    let mut target_phys = Vec::new();
    for p in &a.preconditions {
        match p {
            Precondition::EntryUnchanged { path, sig, .. } if !replacement_paths.contains(path) => {
                target_phys.push(sig.phys());
            }
            Precondition::DirUnchanged { snapshot } => {
                target_phys.extend(snapshot.entries.values().map(EntrySignature::phys));
            }
            _ => {}
        }
    }
    if target_phys.is_empty() || replacement_phys.iter().any(|r| target_phys.contains(r)) {
        return missing();
    }
    Ok(())
}

/// Returns the required paths in sorted order, or the ones missing from `given`.
fn acknowledged(
    required: BTreeSet<RootedPath>,
    given: &[RootedPath],
) -> Result<Vec<RootedPath>, Vec<RootedPath>> {
    let given: BTreeSet<&RootedPath> = given.iter().collect();
    let missing: Vec<RootedPath> = required
        .iter()
        .filter(|p| !given.contains(p))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(required.into_iter().collect())
    } else {
        Err(missing)
    }
}

/// Plan lifecycle (the stored, enforced state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Approved,
    Executing,
    Executed,
    PartiallyExecuted,
    Failed,
    Expired,
    Invalidated,
    Discarded,
    Interrupted,
}

impl PlanStatus {
    /// Explicit table. Simulations never change the status (section 6). `Interrupted` plans are
    /// never resumed: the user re-plans.
    #[must_use]
    pub fn can_transition_to(self, next: PlanStatus) -> bool {
        use PlanStatus::*;
        matches!(
            (self, next),
            (Draft, Approved | Expired | Invalidated | Discarded)
                | (Approved, Executing | Expired | Invalidated | Discarded)
                | (
                    Executing,
                    Executed | PartiallyExecuted | Failed | Interrupted | Invalidated
                )
        )
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(
            self,
            PlanStatus::Draft | PlanStatus::Approved | PlanStatus::Executing
        )
    }

    pub fn transition(self, next: PlanStatus) -> Result<PlanStatus, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "plan",
                from: format!("{self:?}"),
                to: format!("{next:?}"),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approver {
    User,
    AutoPolicy,
}

/// What the approver submits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub plan_hash: PlanHash,
    pub confirmed_count: u32,
    pub confirmed_torrent_removals: u32,
    pub acknowledged_samples: Vec<RootedPath>,
    pub acknowledged_unrecoverable: Vec<RootedPath>,
}

/// A recorded approval of exactly one plan hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub plan_id: PlanId,
    pub plan_hash: PlanHash,
    pub approved_at: Timestamp,
    pub by: Approver,
    pub confirmed_count: u32,
    pub confirmed_torrent_removals: u32,
    pub acknowledged_samples: Vec<RootedPath>,
    pub acknowledged_unrecoverable: Vec<RootedPath>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{action, p, plan, rooted_hash};

    fn consolidate(i: u32) -> PlannedAction {
        action(
            i,
            ActionKind::ConsolidateHardlink,
            ActionTarget::Consolidate {
                source: p("completed", &format!("rel{i}.mkv")),
                target: p("media-00000000", &format!("lib/rel{i}.mkv")),
            },
        )
    }

    fn req(h: &HashedPlan, count: u32) -> ApprovalRequest {
        ApprovalRequest {
            plan_hash: h.hash().clone(),
            confirmed_count: count,
            confirmed_torrent_removals: 0,
            acknowledged_samples: vec![],
            acknowledged_unrecoverable: vec![],
        }
    }

    #[test]
    fn plan_status_table_is_exact() {
        use PlanStatus::*;
        let all = [
            Draft,
            Approved,
            Executing,
            Executed,
            PartiallyExecuted,
            Failed,
            Expired,
            Invalidated,
            Discarded,
            Interrupted,
        ];
        let allowed = [
            (Draft, Approved),
            (Draft, Expired),
            (Draft, Invalidated),
            (Draft, Discarded),
            (Approved, Executing),
            (Approved, Expired),
            (Approved, Invalidated),
            (Approved, Discarded),
            (Executing, Executed),
            (Executing, PartiallyExecuted),
            (Executing, Failed),
            (Executing, Interrupted),
            (Executing, Invalidated),
        ];
        for a in all {
            for b in all {
                assert_eq!(
                    a.can_transition_to(b),
                    allowed.contains(&(a, b)),
                    "{a:?} -> {b:?}"
                );
            }
        }
        assert!(
            Draft.transition(Executing).is_err(),
            "no execution without approval"
        );
        assert!(
            Interrupted.transition(Executing).is_err(),
            "interrupted plans never resume"
        );
        assert!(Executed.is_terminal() && !Approved.is_terminal());
    }

    #[test]
    fn hash_is_stable_across_serialization_round_trips() {
        let h = HashedPlan::seal(plan(vec![consolidate(0), consolidate(1)])).unwrap();
        let json = serde_json::to_string(h.plan()).unwrap();
        let back: ActionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, h.plan());
        assert_eq!(PlanHash::compute(&back).unwrap(), *h.hash());
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(json, again, "serialization is deterministic");
        assert_eq!(h.hash().as_str().len(), 64);
        // Re-sealing an identical plan gives the same hash.
        assert_eq!(
            HashedPlan::seal(plan(vec![consolidate(0), consolidate(1)]))
                .unwrap()
                .hash(),
            h.hash()
        );
    }

    #[test]
    fn golden_hash_of_plan_without_patch2_variants_is_pinned() {
        let mut a = consolidate(0);
        a.preconditions
            .push(Precondition::ServiceAvailable(ServiceKind::Sonarr));
        a.verification.push(VerificationStep::Absent {
            path: p("completed", "x"),
        });
        let h = HashedPlan::seal(plan(vec![a])).unwrap();
        assert_eq!(
            h.hash().as_str(),
            "6a25e2ec99cd303b05876950333bebf3bd0804e191b8e3ca56b5843b32d362a4"
        );
    }

    #[test]
    fn hash_changes_when_any_content_changes() {
        let base = HashedPlan::seal(plan(vec![consolidate(0)])).unwrap();
        let mut p2 = plan(vec![consolidate(0)]);
        p2.actions[0].expected_gain.attributable += 1;
        assert_ne!(HashedPlan::seal(p2).unwrap().hash(), base.hash());
        let mut p3 = plan(vec![consolidate(0)]);
        p3.expires_at = p3.expires_at.plus_hours(1);
        assert_ne!(HashedPlan::seal(p3).unwrap().hash(), base.hash());
        let mut p4 = plan(vec![consolidate(0)]);
        p4.actions[0]
            .preconditions
            .push(Precondition::ConfigFingerprint("x".into()));
        assert_ne!(HashedPlan::seal(p4).unwrap().hash(), base.hash());
    }

    #[test]
    fn hash_does_not_depend_on_map_insertion_order() {
        let h1 = TorrentHash::new("a".repeat(40)).unwrap();
        let h2 = TorrentHash::new("b".repeat(40)).unwrap();
        let mut a = plan(vec![consolidate(0)]);
        a.torrent_states
            .insert(h1.clone(), (TorrentState::Uploading, 1.0));
        a.torrent_states
            .insert(h2.clone(), (TorrentState::PausedUp, 1.0));
        let mut b = plan(vec![consolidate(0)]);
        b.torrent_states.insert(h2, (TorrentState::PausedUp, 1.0));
        b.torrent_states.insert(h1, (TorrentState::Uploading, 1.0));
        assert_eq!(
            PlanHash::compute(&a).unwrap(),
            PlanHash::compute(&b).unwrap()
        );
    }

    #[test]
    fn loading_a_tampered_plan_fails() {
        let h = HashedPlan::seal(plan(vec![consolidate(0)])).unwrap();
        let (mut p, hash) = h.into_parts();
        p.config_fingerprint = "changed".into();
        assert!(matches!(
            HashedPlan::from_parts(p, hash),
            Err(DomainError::PlanHashMismatch { .. })
        ));
    }

    #[test]
    fn validation_rejects_malformed_plans() {
        assert!(plan(vec![]).validate().is_err());
        let mut a = consolidate(0);
        a.index = 3;
        assert!(plan(vec![a]).validate().is_err());
        let mut dep = consolidate(1);
        dep.depends_on = vec![1];
        assert!(plan(vec![consolidate(0), dep]).validate().is_err());
        let mut wrong = consolidate(0);
        wrong.kind = ActionKind::DeleteCompletedFile;
        assert!(plan(vec![wrong]).validate().is_err());
        let mut late = plan(vec![consolidate(0)]);
        late.expires_at = late.created_at;
        assert!(late.validate().is_err());
        let mut roots = plan(vec![consolidate(0)]);
        roots.lock_roots.clear();
        assert!(roots.validate().is_err());
    }

    #[test]
    fn delete_media_file_only_after_a_replace_release() {
        let del = |deps: Vec<u32>, i: u32| {
            let mut a = action(
                i,
                ActionKind::DeleteMediaFile,
                ActionTarget::Path {
                    path: p("media-00000000", "lib/old.mkv"),
                },
            );
            a.depends_on = deps;
            a
        };
        assert!(plan(vec![del(vec![], 0)]).validate().is_err());
        let replace = action(
            0,
            ActionKind::ReplaceRelease,
            ActionTarget::Replace {
                request: ManualImportRequest {
                    files: vec![(
                        p("completed", "new.mkv"),
                        ArrMediaRef {
                            service: ArrService::Radarr,
                            media_id: 1,
                            episode_ids: vec![],
                        },
                    )],
                },
            },
        );
        let ok = plan(vec![replace.clone(), del(vec![0], 1)]);
        assert!(ok.validate().is_ok());
        let unlinked = plan(vec![replace, del(vec![], 1)]);
        assert!(unlinked.validate().is_err());
    }

    #[test]
    fn auto_plans_cannot_hold_manual_only_kinds() {
        let mut a = action(
            0,
            ActionKind::FixMode,
            ActionTarget::FixMode {
                path: p("completed", "a"),
                mode: 0o644,
            },
        );
        a.evidence_level = EvidenceLevel::Full;
        let mut pl = plan(vec![a]);
        pl.origin = PlanOrigin::AutoPolicy;
        assert!(pl.validate().is_err());
    }

    #[test]
    fn tiers_and_capabilities_follow_the_table() {
        use ActionKind::*;
        assert_eq!(ConsolidateHardlink.tier(), RiskTier::T2);
        assert_eq!(ArrRescan.tier(), RiskTier::T2);
        assert_eq!(DeleteCompletedFile.tier(), RiskTier::T3);
        assert_eq!(FixMode.tier(), RiskTier::T3);
        assert_eq!(ReplaceRelease.tier(), RiskTier::T4);
        assert_eq!(ArrGrabRelease.tier(), RiskTier::T4);
        assert_eq!(
            ConsolidateHardlink.capability(),
            Some(Capability::Consolidate)
        );
        for k in [
            DeleteCompletedFile,
            DeleteCompletedFolder,
            DeleteJunk,
            RemoveEmptyParent,
            RemoveTorrentTask,
        ] {
            assert_eq!(k.capability(), Some(Capability::DeleteCompleted));
        }
        assert_eq!(
            CleanupStalledIncomplete.capability(),
            Some(Capability::CleanupIncomplete)
        );
        assert_eq!(
            DeleteMediaFile.capability(),
            Some(Capability::ArrManualImport)
        );
        assert_eq!(ArrGrabRelease.capability(), Some(Capability::ArrGrab));
        assert_eq!(ArrRescan.capability(), None);
        assert_eq!(JellyfinRefresh.capability(), None);
        let auto: Vec<_> = [
            ConsolidateHardlink,
            DeleteCompletedFile,
            DeleteCompletedFolder,
            DeleteJunk,
            RemoveEmptyParent,
            RemoveTorrentTask,
            CleanupStalledIncomplete,
            DeleteIncompleteOrphan,
            FixMode,
            ReplaceRelease,
            DeleteMediaFile,
            ArrGrabRelease,
            ArrRemoveQueueItem,
            ArrSetMonitored,
            ArrRescan,
            JellyfinRefresh,
        ]
        .into_iter()
        .filter(|k| k.is_auto_eligible())
        .collect();
        assert_eq!(
            auto,
            [
                ConsolidateHardlink,
                DeleteCompletedFile,
                DeleteCompletedFolder,
                RemoveEmptyParent,
                RemoveTorrentTask
            ]
        );
    }

    #[test]
    fn manual_approval_checks_hash_count_and_expiry() {
        let h = HashedPlan::seal(plan(vec![consolidate(0), consolidate(1)])).unwrap();
        let now = h.plan().created_at;
        let ok = h.approve(&req(&h, 2), now).unwrap();
        assert_eq!(
            (ok.by, ok.confirmed_count, &ok.plan_hash),
            (Approver::User, 2, h.hash())
        );
        assert!(matches!(
            h.approve(&req(&h, 1), now),
            Err(DomainError::ApprovalMismatch {
                field: "confirmed_count",
                ..
            })
        ));
        let mut wrong = req(&h, 2);
        wrong.plan_hash = rooted_hash("other");
        assert!(matches!(
            h.approve(&wrong, now),
            Err(DomainError::PlanHashMismatch { .. })
        ));
        let late = h.plan().expires_at;
        assert!(matches!(
            h.approve(&req(&h, 2), late),
            Err(DomainError::PlanExpired { .. })
        ));
        assert!(h.approve(&req(&h, 2), Timestamp(late.0 - 1)).is_ok());
    }

    #[test]
    fn samples_and_unrecoverable_files_need_explicit_acknowledgement() {
        let sample = p("completed", "Rel/Sample/s.mkv");
        let old = p("media-00000000", "lib/old.mkv");
        let mut a = action(
            0,
            ActionKind::DeleteCompletedFolder,
            ActionTarget::Path {
                path: p("completed", "Rel"),
            },
        );
        a.preconditions.push(Precondition::OldCoverageRecoverable {
            old_files: vec![
                (
                    old.clone(),
                    PhysicalId { dev: 1, ino: 2 },
                    OldFileRecovery::Unrecoverable,
                ),
                (
                    p("media-00000000", "lib/ok.mkv"),
                    PhysicalId { dev: 1, ino: 3 },
                    OldFileRecovery::RecycleBin,
                ),
            ],
        });
        let mut pl = plan(vec![a]);
        pl.samples = vec![sample.clone()];
        let h = HashedPlan::seal(pl).unwrap();
        let now = h.plan().created_at;
        assert_eq!(
            h.plan().unrecoverable_old_files(),
            BTreeSet::from([old.clone()])
        );
        let mut r = req(&h, 1);
        assert!(
            matches!(h.approve(&r, now), Err(DomainError::SamplesNotAcknowledged(v)) if v == [sample.clone()])
        );
        r.acknowledged_samples = vec![sample.clone()];
        assert!(
            matches!(h.approve(&r, now), Err(DomainError::UnrecoverableNotAcknowledged(v)) if v == [old.clone()])
        );
        r.acknowledged_unrecoverable = vec![old.clone(), p("completed", "unrelated")];
        let ok = h.approve(&r, now).unwrap();
        assert_eq!(ok.acknowledged_samples, [sample]);
        assert_eq!(
            ok.acknowledged_unrecoverable,
            [old],
            "only plan-relevant acknowledgements are recorded"
        );
    }

    #[test]
    fn torrent_removals_are_confirmed_separately_and_companions_are_not_counted() {
        let hash = TorrentHash::new("c".repeat(40)).unwrap();
        let del = action(
            0,
            ActionKind::DeleteCompletedFile,
            ActionTarget::Path {
                path: p("completed", "a.mkv"),
            },
        );
        let parent = action(
            1,
            ActionKind::RemoveEmptyParent,
            ActionTarget::Path {
                path: p("completed", "dir"),
            },
        );
        let task = action(
            2,
            ActionKind::RemoveTorrentTask,
            ActionTarget::Torrent {
                hash,
                data_paths: vec![],
            },
        );
        let h = HashedPlan::seal(plan(vec![del, parent, task])).unwrap();
        assert_eq!(
            (
                h.plan().required_count(),
                h.plan().required_torrent_removals()
            ),
            (1, 1)
        );
        let now = h.plan().created_at;
        assert!(
            h.approve(&req(&h, 1), now).is_err(),
            "removals not confirmed"
        );
        let mut r = req(&h, 1);
        r.confirmed_torrent_removals = 1;
        assert!(h.approve(&r, now).is_ok());
    }

    fn auto_plan(level: EvidenceLevel, completeness: Completeness) -> HashedPlan {
        let mut a = consolidate(0);
        a.evidence_level = level;
        a.completeness = completeness;
        let mut pl = plan(vec![a]);
        pl.origin = PlanOrigin::AutoPolicy;
        HashedPlan::seal(pl).unwrap()
    }

    #[test]
    fn incomplete_evidence_cannot_be_automatically_approved() {
        let now = Timestamp(0);
        for c in [
            Completeness::Unknown,
            Completeness::Incomplete {
                missing: vec!["scan".into()],
            },
        ] {
            let h = auto_plan(EvidenceLevel::Sampled, c);
            assert!(matches!(
                h.approve_auto(now),
                Err(DomainError::IncompleteEvidence { action_index: 0 })
            ));
        }
        let none = auto_plan(EvidenceLevel::None, Completeness::Complete);
        assert!(matches!(
            none.approve_auto(now),
            Err(DomainError::IncompleteEvidence { .. })
        ));
        let ok = auto_plan(EvidenceLevel::Sampled, Completeness::Complete)
            .approve_auto(now)
            .unwrap();
        assert_eq!(ok.by, Approver::AutoPolicy);
    }

    #[test]
    fn potential_gain_never_authorizes_anything() {
        let now = Timestamp(0);
        let mut a = consolidate(0);
        a.completeness = Completeness::Unknown;
        a.expected_gain = GainEstimate {
            logical: u64::MAX,
            attributable: u64::MAX,
            added: 0,
        };
        let mut pl = plan(vec![a]);
        pl.gains = PlanGains::new(u64::MAX, u64::MAX, 0);
        pl.origin = PlanOrigin::AutoPolicy;
        assert!(HashedPlan::seal(pl).unwrap().approve_auto(now).is_err());
    }

    #[test]
    fn auto_approval_is_refused_for_user_plans_samples_and_expired_plans() {
        let user = HashedPlan::seal(plan(vec![consolidate(0)])).unwrap();
        assert!(matches!(
            user.approve_auto(Timestamp(0)),
            Err(DomainError::AutoApprovalForbidden(_))
        ));
        let mut sample_plan = auto_plan(EvidenceLevel::Sampled, Completeness::Complete)
            .into_parts()
            .0;
        sample_plan.samples.push(p("completed", "Sample/s.mkv"));
        assert!(
            HashedPlan::seal(sample_plan)
                .unwrap()
                .approve_auto(Timestamp(0))
                .is_err()
        );
        let exp = auto_plan(EvidenceLevel::Sampled, Completeness::Complete);
        let late = exp.plan().expires_at;
        assert!(matches!(
            exp.approve_auto(late),
            Err(DomainError::PlanExpired { .. })
        ));
    }

    #[test]
    fn auto_plans_refuse_unrecoverable_old_files() {
        let mut a = consolidate(0);
        a.evidence_level = EvidenceLevel::Sampled;
        a.completeness = Completeness::Complete;
        a.preconditions.push(Precondition::OldCoverageRecoverable {
            old_files: vec![(
                p("media-00000000", "x"),
                PhysicalId { dev: 1, ino: 1 },
                OldFileRecovery::Unrecoverable,
            )],
        });
        let mut pl = plan(vec![a]);
        pl.origin = PlanOrigin::AutoPolicy;
        assert!(matches!(
            HashedPlan::seal(pl).unwrap().approve_auto(Timestamp(0)),
            Err(DomainError::AutoApprovalForbidden(_))
        ));
    }

    // ---- Patch 2: automatic approval of ARR-validated deletions ----

    fn sig(ino: u64, size: u64) -> EntrySignature {
        EntrySignature {
            kind: EntryKind::File,
            dev: 1,
            ino,
            size,
            nlink: 1,
            mtime_ns: crate::time::MtimeNs(0),
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }

    fn media_ref() -> ArrMediaRef {
        ArrMediaRef {
            service: ArrService::Radarr,
            media_id: 9,
            episode_ids: vec![],
        }
    }

    /// A valid auto deletion plan with ARR-validated evidence; `tweak` breaks one condition.
    fn arr_auto(
        tweak: impl FnOnce(&mut PlannedAction, &mut ActionPlan),
    ) -> Result<Approval, DomainError> {
        let target = p("completed", "rel/a.mkv");
        let repl = p("media-00000000", "lib/a.mkv");
        let mut a = action(
            0,
            ActionKind::DeleteCompletedFile,
            ActionTarget::Path {
                path: target.clone(),
            },
        );
        a.evidence_level = EvidenceLevel::ArrValidatedReplacement;
        a.preconditions = vec![
            Precondition::EntryUnchanged {
                path: target,
                sig: sig(10, 100),
                compare_nlink: true,
            },
            Precondition::EntryUnchanged {
                path: repl.clone(),
                sig: sig(20, 500),
                compare_nlink: false,
            },
            Precondition::ArrReplacementPresent {
                media: media_ref(),
                path: repl,
                size: 500,
            },
            Precondition::ArrFingerprint {
                media: media_ref(),
                fingerprint: "f".into(),
            },
        ];
        let mut pl = plan(vec![]);
        pl.origin = PlanOrigin::AutoPolicy;
        tweak(&mut a, &mut pl);
        pl.actions = vec![a];
        HashedPlan::seal(pl)?.approve_auto(Timestamp(0))
    }

    #[test]
    fn arr_validated_deletion_is_auto_approved_when_every_rule_holds() {
        assert_eq!(arr_auto(|_, _| {}).unwrap().by, Approver::AutoPolicy);
    }

    #[test]
    fn arr_validated_deletion_needs_every_pinned_condition() {
        type Tweak = Box<dyn Fn(&mut PlannedAction)>;
        let cases: Vec<(&str, Tweak)> = vec![
            (
                "no replacement evidence",
                Box::new(|a| {
                    a.preconditions
                        .retain(|p| !matches!(p, Precondition::ArrReplacementPresent { .. }))
                }),
            ),
            (
                "no replacement pin",
                Box::new(|a| {
                    a.preconditions.retain(|p| {
                        !matches!(
                            p,
                            Precondition::EntryUnchanged {
                                compare_nlink: false,
                                ..
                            }
                        )
                    })
                }),
            ),
            (
                "no fingerprint",
                Box::new(|a| {
                    a.preconditions
                        .retain(|p| !matches!(p, Precondition::ArrFingerprint { .. }))
                }),
            ),
            (
                "size differs",
                Box::new(|a| {
                    for p in &mut a.preconditions {
                        if let Precondition::ArrReplacementPresent { size, .. } = p {
                            *size = 501;
                        }
                    }
                }),
            ),
            (
                "same inode as target",
                Box::new(|a| {
                    for p in &mut a.preconditions {
                        if let Precondition::EntryUnchanged {
                            sig: s,
                            compare_nlink: false,
                            ..
                        } = p
                        {
                            s.ino = 10;
                        }
                    }
                }),
            ),
            (
                "replacement not in a media root",
                Box::new(|a| {
                    for p in &mut a.preconditions {
                        match p {
                            Precondition::EntryUnchanged {
                                path,
                                compare_nlink: false,
                                ..
                            }
                            | Precondition::ArrReplacementPresent { path, .. } => {
                                *path = p_in_incomplete();
                            }
                            _ => {}
                        }
                    }
                }),
            ),
            (
                "target inode unknown",
                Box::new(|a| {
                    a.preconditions.retain(|p| {
                        !matches!(
                            p,
                            Precondition::EntryUnchanged {
                                compare_nlink: true,
                                ..
                            }
                        )
                    })
                }),
            ),
            (
                "incomplete",
                Box::new(|a| {
                    a.completeness = Completeness::Incomplete {
                        missing: vec!["x".into()],
                    }
                }),
            ),
            (
                "unknown",
                Box::new(|a| a.completeness = Completeness::Unknown),
            ),
            (
                "no evidence",
                Box::new(|a| a.evidence_level = EvidenceLevel::None),
            ),
        ];
        for (name, f) in cases {
            let r = arr_auto(|a, _| f(a));
            assert!(
                matches!(r, Err(DomainError::IncompleteEvidence { action_index: 0 })),
                "{name}: {r:?}"
            );
        }
    }

    fn p_in_incomplete() -> RootedPath {
        p("incomplete", "x.mkv")
    }

    #[test]
    fn arr_validated_level_is_refused_outside_completed_deletions() {
        let r = arr_auto(|a, _| {
            a.target = ActionTarget::Path {
                path: p("media-00000000", "lib/a.mkv"),
            };
        });
        assert!(
            matches!(r, Err(DomainError::AutoApprovalForbidden(_))),
            "{r:?}"
        );
        let r = arr_auto(|a, _| a.kind = ActionKind::DeleteJunk);
        assert!(r.is_err());
        let r = arr_auto(|a, pl| {
            a.kind = ActionKind::ConsolidateHardlink;
            a.target = ActionTarget::Consolidate {
                source: p("completed", "a"),
                target: p("media-00000000", "b"),
            };
            let _ = pl;
        });
        assert!(
            matches!(r, Err(DomainError::AutoApprovalForbidden(_))),
            "{r:?}"
        );
        let r = arr_auto(|_, pl| pl.samples.push(p("completed", "rel/Sample/s.mkv")));
        assert!(matches!(r, Err(DomainError::AutoApprovalForbidden(_))));
        // A user plan never auto-approves.
        let r = arr_auto(|_, pl| pl.origin = PlanOrigin::User);
        assert!(r.is_err());
    }

    #[test]
    fn arr_validated_folder_deletion_compares_every_snapshot_entry() {
        let r = arr_auto(|a, _| {
            a.kind = ActionKind::DeleteCompletedFolder;
            a.preconditions.retain(|p| {
                !matches!(
                    p,
                    Precondition::EntryUnchanged {
                        compare_nlink: true,
                        ..
                    }
                )
            });
            let mut entries = BTreeMap::new();
            entries.insert(crate::path::RelPath::new("a.mkv").unwrap(), sig(20, 500));
            a.preconditions.push(Precondition::DirUnchanged {
                snapshot: DirSnapshot {
                    dir: p("completed", "rel/a.mkv"),
                    entries,
                },
            });
        });
        assert!(
            matches!(r, Err(DomainError::IncompleteEvidence { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn patch2_variants_roundtrip_deterministically() {
        let mut a = action(
            0,
            ActionKind::ArrRemoveQueueItem,
            ActionTarget::Queue {
                service: ArrService::Sonarr,
                id: 3,
            },
        );
        a.verification = vec![
            VerificationStep::ArrQueueItemAbsent {
                service: ArrService::Sonarr,
                id: 3,
            },
            VerificationStep::ArrMonitored {
                media: media_ref(),
                monitored: true,
            },
            VerificationStep::ArrCommandCompleted {
                service: ArrService::Radarr,
                media: None,
            },
            VerificationStep::JellyfinRefreshed {
                item_id: Some("x".into()),
            },
        ];
        a.evidence_level = EvidenceLevel::ArrValidatedReplacement;
        let h = HashedPlan::seal(plan(vec![a])).unwrap();
        let json = serde_json::to_string(h.plan()).unwrap();
        let back: ActionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, h.plan());
        assert_eq!(PlanHash::compute(&back).unwrap(), *h.hash());
    }
}
