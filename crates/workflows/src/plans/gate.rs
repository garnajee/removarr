//! Risk tiers and capability flags (architecture 6.7, 13.1, 13.2). Checked at planning time; the
//! live adapters check the capability again at execution time.

use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::plan::{ActionKind, PlanMode, PlanOrigin};
use removarr_domain::policy::{Capability, RiskTier};

use super::request::PlanSettings;

/// Effective tier of an action: its kind's tier, or tier 5 inside an automatic plan.
#[must_use]
pub fn action_tier(kind: ActionKind, origin: PlanOrigin) -> RiskTier {
    match origin {
        PlanOrigin::AutoPolicy => RiskTier::T5,
        PlanOrigin::User => kind.tier(),
    }
}

/// Kinds that run only in a live process (service commands and grabs, sections 7.4 and 13.2):
/// a dry-run process cannot even plan them.
#[must_use]
pub fn live_only(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::ArrGrabRelease
            | ActionKind::ArrRemoveQueueItem
            | ActionKind::ArrSetMonitored
            | ActionKind::ArrRescan
            | ActionKind::JellyfinRefresh
    )
}

/// Every reason the configuration forbids planning `kind` with `origin`. Empty = allowed.
#[must_use]
pub fn blockers(kind: ActionKind, origin: PlanOrigin, settings: &PlanSettings) -> Vec<Blocker> {
    let mut out = Vec::new();
    let tier = action_tier(kind, origin);
    if tier > settings.implemented_tier || kind.tier() > settings.implemented_tier {
        out.push(Blocker::new(
            BlockerCode::NotInBuild,
            format!(
                "{kind:?} needs tier {tier:?}; this build enables up to {:?}",
                settings.implemented_tier
            ),
        ));
    }
    if let Some(c) = kind.capability()
        && !settings.capabilities.contains(&c)
    {
        out.push(Blocker::new(
            BlockerCode::CapabilityDisabled,
            format!("capability {} is disabled", c.as_str()),
        ));
    }
    if origin == PlanOrigin::AutoPolicy {
        if !kind.is_auto_eligible() {
            out.push(Blocker::new(
                BlockerCode::CapabilityDisabled,
                format!("{kind:?} is never automatic (A9)"),
            ));
        }
        if kind == ActionKind::RemoveTorrentTask
            && !settings
                .capabilities
                .contains(&Capability::AutoRemoveEmptyTorrents)
        {
            out.push(Blocker::new(
                BlockerCode::CapabilityDisabled,
                "capability auto_remove_empty_torrents is disabled (A8)",
            ));
        }
    }
    if live_only(kind) && settings.mode == PlanMode::DryRun {
        out.push(Blocker::new(
            BlockerCode::DryRunOnly,
            format!("{kind:?} runs only in a live process"),
        ));
    }
    out
}
