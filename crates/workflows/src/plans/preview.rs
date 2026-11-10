//! Plan preview: everything the confirmation screen shows, derived from the sealed plan alone
//! (so a stored plan previews exactly like a fresh one). The dry-run preview is this view of a
//! plan built by the same function as a live plan.

use std::collections::BTreeSet;

use removarr_domain::arr::OldFileRecovery;
use removarr_domain::gain::{GainEstimate, PlanGains};
use removarr_domain::ids::{PlanId, SubjectKey};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{
    ActionKind, ActionTarget, HashedPlan, PlanHash, PlanMode, PlanOrigin, PlanStatus, Precondition,
    VerificationStep,
};
use removarr_domain::policy::{Capability, RiskTier};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::TorrentHash;
use serde::{Deserialize, Serialize};

use super::gate::action_tier;
use super::validate::snapshot_paths;

/// Whether an action can be undone by the application.
#[must_use]
pub fn is_irreversible(kind: ActionKind) -> bool {
    !matches!(
        kind,
        ActionKind::ConsolidateHardlink
            | ActionKind::FixMode
            | ActionKind::ArrSetMonitored
            | ActionKind::ArrRescan
            | ActionKind::JellyfinRefresh
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionPreview {
    pub index: u32,
    pub kind: ActionKind,
    pub tier: RiskTier,
    pub capability: Option<Capability>,
    pub companion: bool,
    pub irreversible: bool,
    pub target: ActionTarget,
    /// Every entry a folder deletion removes besides the folder itself (sidecars, subtitles,
    /// NFO, images, samples, sub-folders, symlinks), from the plan's own folder snapshot.
    pub annexes: Vec<RootedPath>,
    pub torrents: BTreeSet<TorrentHash>,
    pub expected_gain: GainEstimate,
    pub depends_on: Vec<u32>,
    pub verification: Vec<VerificationStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub id: PlanId,
    pub hash: PlanHash,
    pub mode: PlanMode,
    pub dry_run: bool,
    pub origin: PlanOrigin,
    pub status: Option<PlanStatus>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub actions: Vec<ActionPreview>,
    pub excluded: Vec<(SubjectKey, Vec<Blocker>)>,
    pub gains: PlanGains,
    /// Non-refusing warnings: `OldCoverageUnrecoverable` per file, `ArrHardlinksDisabled`.
    pub warnings: Vec<Blocker>,
    /// Typed confirmation (DR-35) and the separate torrent-removal count (CP1-F12).
    pub required_count: u32,
    pub required_torrent_removals: u32,
    pub torrent_removals: Vec<TorrentHash>,
    pub samples_to_acknowledge: Vec<RootedPath>,
    pub unrecoverable_to_acknowledge: Vec<RootedPath>,
    pub irreversible: bool,
    pub max_tier: RiskTier,
}

/// Builds the preview of a sealed plan.
#[must_use]
pub fn preview(plan: &HashedPlan, status: Option<PlanStatus>) -> PlanPreview {
    let p = plan.plan();
    let mut warnings = Vec::new();
    let actions: Vec<ActionPreview> = p
        .actions
        .iter()
        .map(|a| {
            let mut annexes = Vec::new();
            for pre in &a.preconditions {
                match pre {
                    Precondition::DirUnchanged { snapshot } => {
                        annexes.extend(snapshot_paths(snapshot).into_iter().skip(1));
                    }
                    Precondition::OldCoverageRecoverable { old_files } => {
                        for (path, _, r) in old_files {
                            if *r == OldFileRecovery::Unrecoverable {
                                warnings.push(Blocker::new(
                                    BlockerCode::OldCoverageUnrecoverable,
                                    format!("{path}: no other link and no usable recycle bin"),
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if a.kind == ActionKind::ReplaceRelease && a.expected_gain.added > 0 {
                warnings.push(Blocker::new(
                    BlockerCode::ArrHardlinksDisabled,
                    "ARR copies instead of hardlinking: the import adds space and cannot be verified",
                ));
            }
            ActionPreview {
                index: a.index,
                kind: a.kind,
                tier: action_tier(a.kind, p.origin),
                capability: a.kind.capability(),
                companion: a.kind.is_companion(),
                irreversible: is_irreversible(a.kind),
                target: a.target.clone(),
                annexes,
                torrents: a.torrent_hashes.clone(),
                expected_gain: a.expected_gain,
                depends_on: a.depends_on.clone(),
                verification: a.verification.clone(),
            }
        })
        .collect();
    let torrent_removals = p
        .actions
        .iter()
        .filter_map(|a| match (&a.kind, &a.target) {
            (ActionKind::RemoveTorrentTask, ActionTarget::Torrent { hash, .. }) => {
                Some(hash.clone())
            }
            _ => None,
        })
        .collect();
    PlanPreview {
        id: p.id,
        hash: plan.hash().clone(),
        mode: p.mode,
        dry_run: p.mode == PlanMode::DryRun,
        origin: p.origin,
        status,
        created_at: p.created_at,
        expires_at: p.expires_at,
        irreversible: actions.iter().any(|a| a.irreversible),
        max_tier: actions.iter().map(|a| a.tier).max().unwrap_or(RiskTier::T1),
        actions,
        excluded: p.excluded.clone(),
        gains: p.gains,
        warnings,
        required_count: p.required_count(),
        required_torrent_removals: p.required_torrent_removals(),
        torrent_removals,
        samples_to_acknowledge: p.samples.clone(),
        unrecoverable_to_acknowledge: p.unrecoverable_old_files().into_iter().collect(),
    }
}
