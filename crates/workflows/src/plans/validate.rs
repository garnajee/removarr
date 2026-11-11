//! Plan-engine rules on top of the domain's structural validation: every action carries
//! evidence, preconditions for everything it touches, and verification steps.

use removarr_domain::evidence::Completeness;
use removarr_domain::fs::DirSnapshot;
use removarr_domain::path::{RelPath, RootKind, RootedPath};
use removarr_domain::plan::{ActionKind, ActionPlan, ActionTarget, PlannedAction, Precondition};
use removarr_domain::policy::is_sample;

fn pins(a: &PlannedAction, path: &RootedPath) -> bool {
    a.preconditions.iter().any(|p| match p {
        Precondition::EntryUnchanged { path: q, .. } => q == path,
        Precondition::DirUnchanged { snapshot } => &snapshot.dir == path,
        _ => false,
    })
}

fn no_error_under(a: &PlannedAction, path: &RootedPath) -> bool {
    a.preconditions
        .iter()
        .any(|p| matches!(p, Precondition::NoScanErrorUnder { path: q } if q == path))
}

fn has<F: Fn(&Precondition) -> bool>(a: &PlannedAction, f: F) -> bool {
    a.preconditions.iter().any(f)
}

/// Returns every violated rule (empty = valid). The domain's `ActionPlan::validate` must pass
/// too; it runs again when the plan is sealed.
#[must_use]
pub fn check_plan(plan: &ActionPlan) -> Vec<String> {
    let mut v = Vec::new();
    if let Err(e) = plan.validate() {
        v.push(e.to_string());
    }
    for a in &plan.actions {
        let i = a.index;
        let mut bad = |m: &str| v.push(format!("action {i} ({:?}): {m}", a.kind));
        if !has(
            a,
            |p| matches!(p, Precondition::ConfigFingerprint(f) if *f == plan.config_fingerprint),
        ) {
            bad("no configuration fingerprint precondition");
        }
        if a.preconditions.len() < 2 {
            bad("no state precondition");
        }
        if a.verification.is_empty() {
            bad("no verification step");
        }
        if a.completeness == Completeness::Unknown {
            bad("no evidence");
        }
        if !a.torrent_hashes.is_empty()
            && a.kind != ActionKind::ArrRemoveQueueItem
            && a.kind != ActionKind::ReplaceRelease
        {
            let covered = a.torrent_hashes.iter().all(|h| {
                has(a, |p| matches!(p, Precondition::TorrentStates { states } if states.contains_key(h)))
            });
            if !covered {
                bad("a touched torrent has no captured state");
            }
        }
        match (&a.kind, &a.target) {
            (ActionKind::ConsolidateHardlink, ActionTarget::Consolidate { source, target }) => {
                if source.root.kind() != RootKind::Completed
                    || target.root.kind() != RootKind::Media
                {
                    bad("repair direction must be completed -> media");
                }
                if !pins(a, source) || !pins(a, target) {
                    bad("source and target must be pinned");
                }
                if !has(a, |p| matches!(p, Precondition::SameDevice { .. })) {
                    bad("no same-device precondition");
                }
            }
            (kind, ActionTarget::Path { path }) => {
                let want = match kind {
                    ActionKind::DeleteIncompleteOrphan => RootKind::Incomplete,
                    ActionKind::DeleteMediaFile => RootKind::Media,
                    _ => RootKind::Completed,
                };
                if path.root.kind() != want || path.rel.is_root() {
                    bad("target is outside the allowed root or is a root");
                }
                if !pins(a, path) || !no_error_under(a, path) {
                    bad("target is not pinned or not guarded against scan errors");
                }
                if is_sample(&path.rel) && !plan.samples.contains(path) {
                    bad("a sample is targeted without an acknowledgement slot");
                }
                for p in &a.preconditions {
                    if let Precondition::DirUnchanged { snapshot } = p
                        && snapshot_paths(snapshot)
                            .iter()
                            .any(|q| is_sample(&q.rel) && !plan.samples.contains(q))
                    {
                        bad("a sample inside the folder has no acknowledgement slot");
                    }
                }
            }
            (ActionKind::CleanupStalledIncomplete, ActionTarget::Torrent { data_paths, .. }) => {
                for p in data_paths {
                    if p.root.kind() != RootKind::Incomplete || !pins(a, p) || !no_error_under(a, p)
                    {
                        bad("incomplete data must be pinned under an incomplete root");
                    }
                }
            }
            (ActionKind::FixMode, ActionTarget::FixMode { path, .. }) => {
                if !pins(a, path) {
                    bad("target is not pinned");
                }
            }
            (ActionKind::ReplaceRelease, ActionTarget::Replace { request }) => {
                if !has(a, |p| matches!(p, Precondition::ArrFingerprint { .. }))
                    || !has(a, |p| {
                        matches!(p, Precondition::OldCoverageRecoverable { .. })
                    })
                {
                    bad("no ARR fingerprint or old-coverage recovery precondition");
                }
                if request.files.iter().any(|(p, _)| !pins(a, p)) {
                    bad("a completed source is not pinned");
                }
            }
            (ActionKind::ArrGrabRelease, _)
                if !has(a, |p| matches!(p, Precondition::ArrFingerprint { .. })) =>
            {
                bad("no ARR fingerprint precondition");
            }
            _ => {}
        }
    }
    v
}

/// Every path a folder snapshot covers, the folder itself first.
pub(crate) fn snapshot_paths(snapshot: &DirSnapshot) -> Vec<RootedPath> {
    snapshot
        .entries
        .keys()
        .filter_map(|rel| {
            let full = if rel.is_root() {
                snapshot.dir.rel.clone()
            } else if snapshot.dir.rel.is_root() {
                rel.clone()
            } else {
                RelPath::new(format!("{}/{}", snapshot.dir.rel, rel)).ok()?
            };
            Some(RootedPath::new(snapshot.dir.root.clone(), full))
        })
        .collect()
}
