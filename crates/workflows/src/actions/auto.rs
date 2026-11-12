//! Runner-side tier 5 (CP2-tier5 T5-01, T5-03..T5-07, T5-09, T5-13, T5-18). Automatic plans are
//! executed only through these checks, and only when the gate allows tier 5: production builds
//! keep `IMPLEMENTED_TIER = T4`, so `execute` refuses every `AutoPolicy` plan before any of
//! this runs. Tests reach it through `execute_with_gate`, which exists only under the
//! `tier5-test-gate` cargo feature (enabled by this crate's own dev-dependency, never by a
//! normal dependency edge).
//!
//! Two layers, both refusing with `NotExecuted` before any intent:
//! * [`automatic_defects`], pure: kind allowlist (A9), approval by `approve_auto` only, evidence
//!   per kind (A5/A6), no samples, `RemoveTorrentTask` only with `auto_remove_empty_torrents`;
//! * [`Ctx::auto_live_checks`], fresh reads: whole-root perimeter (A2), minimum age (A3), the
//!   ARR replacement and a re-read preview that still rejects the completed file (CP1-F02),
//!   and the ignore list (A4).

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::ArrMediaRef;
use removarr_domain::evidence::EvidenceLevel;
use removarr_domain::fs::{EntryKind, EntrySignature};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionPlan, ActionTarget, Approval, Approver, HashedPlan, PlannedAction,
    Precondition,
};
use removarr_domain::policy::Capability;
use removarr_domain::session::SessionStatus;
use removarr_domain::time::Timestamp;

use super::ActionSettings;
use super::journal::Ctx;
use super::revalidate::{LiveReads, action_paths};

/// Pure checks of an automatic plan. `approval` is `None` only for a simulation of a draft.
pub(crate) fn automatic_defects(
    plan: &ActionPlan,
    hashed: &HashedPlan,
    approval: Option<&Approval>,
    settings: &ActionSettings,
    now: Timestamp,
    simulated: bool,
) -> BTreeMap<u32, Vec<Blocker>> {
    let mut out: BTreeMap<u32, Vec<Blocker>> = BTreeMap::new();
    let mut add = |index: u32, code: BlockerCode, detail: String| {
        out.entry(index)
            .or_default()
            .push(Blocker::new(code, detail));
    };
    // T5-03: only `approve_auto` approves an automatic plan, never a user.
    if !simulated {
        let ok = approval.is_some_and(|a| {
            a.by == Approver::AutoPolicy && &a.plan_hash == hashed.hash() && a.plan_id == plan.id
        });
        if !ok {
            for a in &plan.actions {
                add(
                    a.index,
                    BlockerCode::CapabilityDisabled,
                    "an automatic plan runs only with an automatic-policy approval of its hash"
                        .into(),
                );
            }
        }
    }
    // The domain's own automatic approval rules, re-run at execution (evidence, samples,
    // unrecoverable files, eligibility, expiry).
    if let Err(e) = hashed.approve_auto(now) {
        let only = match &e {
            removarr_domain::error::DomainError::IncompleteEvidence { action_index } => {
                Some(*action_index)
            }
            _ => None,
        };
        for a in &plan.actions {
            if only.is_none_or(|i| i == a.index) {
                add(
                    a.index,
                    BlockerCode::ArrAmbiguous,
                    format!("automatic approval rules no longer hold: {e}"),
                );
            }
        }
    }
    if !plan.samples.is_empty() {
        for a in &plan.actions {
            add(
                a.index,
                BlockerCode::Sample,
                "samples are never automatic".into(),
            );
        }
    }
    for a in &plan.actions {
        // T5-01: the automatic-only kind allowlist, independent of the planner.
        if !a.kind.is_auto_eligible() {
            add(
                a.index,
                BlockerCode::CapabilityDisabled,
                format!("{:?} is never automatic (A9)", a.kind),
            );
        }
        // T5-04: evidence per kind.
        match a.kind {
            ActionKind::DeleteCompletedFile | ActionKind::DeleteCompletedFolder
                if a.evidence_level != EvidenceLevel::ArrValidatedReplacement =>
            {
                add(
                    a.index,
                    BlockerCode::ArrNotUpgradeEvidence,
                    "an automatic deletion needs ARR-validated evidence".into(),
                );
            }
            ActionKind::ConsolidateHardlink
                if !matches!(
                    a.evidence_level,
                    EvidenceLevel::NameSize | EvidenceLevel::Sampled | EvidenceLevel::Full
                ) =>
            {
                add(
                    a.index,
                    BlockerCode::ContentDiffers,
                    "an automatic consolidation needs name+size, sampled or full evidence".into(),
                );
            }
            _ => {}
        }
        // A8.
        if a.kind == ActionKind::RemoveTorrentTask
            && !settings
                .capabilities
                .contains(&Capability::AutoRemoveEmptyTorrents)
        {
            add(
                a.index,
                BlockerCode::CapabilityDisabled,
                "auto_remove_empty_torrents is off".into(),
            );
        }
    }
    out
}

/// Entry signatures pinned by an action (files and every entry of a folder snapshot).
fn pinned_sigs(a: &PlannedAction) -> Vec<(RootedPath, EntrySignature)> {
    let mut out = Vec::new();
    for p in &a.preconditions {
        match p {
            Precondition::EntryUnchanged { path, sig, .. } => out.push((path.clone(), *sig)),
            Precondition::DirUnchanged { snapshot } => {
                for (rel, sig) in &snapshot.entries {
                    if let Some(full) = super::batch::join(&snapshot.dir, rel.as_str()) {
                        out.push((full, *sig));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn subjects_of(a: &PlannedAction) -> Vec<SubjectKey> {
    let mut out: Vec<SubjectKey> = pinned_sigs(a)
        .iter()
        .map(|(p, _)| SubjectKey::rooted(p))
        .collect();
    match &a.target {
        ActionTarget::Torrent { hash, .. } => out.push(SubjectKey::torrent(hash)),
        ActionTarget::Consolidate { source, target } => {
            let sig = |p: &RootedPath| {
                pinned_sigs(a)
                    .into_iter()
                    .find(|(q, _)| q == p)
                    .map(|(_, s)| s.phys())
            };
            if let (Some(x), Some(y)) = (sig(source), sig(target)) {
                out.push(SubjectKey::pair(x, y));
            }
        }
        _ => {}
    }
    out
}

impl Ctx<'_> {
    /// Fresh-read checks of an automatic plan; `None` map entries mean the action holds.
    pub(crate) async fn auto_live_checks(
        &self,
        actions: &[&PlannedAction],
    ) -> BTreeMap<u32, Vec<Blocker>> {
        let mut out: BTreeMap<u32, Vec<Blocker>> = BTreeMap::new();
        let mut add = |index: u32, code: BlockerCode, detail: String| {
            out.entry(index)
                .or_default()
                .push(Blocker::new(code, detail));
        };
        let now = self.ports.clock.now();

        // T5-07: whole-root perimeter, not only "around the path".
        let mut live = LiveReads::default();
        let session = match self.ports.scans.latest_session().await {
            Ok(s) => s,
            Err(e) => {
                for a in actions {
                    add(
                        a.index,
                        BlockerCode::ScanIncomplete,
                        format!("session: {}", e.detail),
                    );
                }
                return out;
            }
        };
        let session_ok = session
            .as_ref()
            .is_some_and(|s| s.status == SessionStatus::Complete);
        let roots: BTreeSet<RootId> = actions
            .iter()
            .flat_map(|a| {
                let mut r: Vec<RootId> = action_paths(a).into_iter().map(|p| p.root).collect();
                r.extend(pinned_sigs(a).into_iter().map(|(p, _)| p.root));
                r
            })
            .collect();
        let mut unclean: Vec<String> = Vec::new();
        for root in &roots {
            match self.inventory(&mut live, root).await {
                Ok(Some(inv)) if inv.is_clean() => {}
                _ => unclean.push(root.to_string()),
            }
        }
        let qbit_ok = match &self.ports.qbit {
            Some(q) => q.version().await.is_ok(),
            None => false,
        };
        if !session_ok || !unclean.is_empty() || !qbit_ok {
            let why = if !session_ok {
                "the latest scan session is not complete".to_string()
            } else if !unclean.is_empty() {
                format!("scan errors or protected zones in {}", unclean.join(", "))
            } else {
                "qBittorrent is not reachable".to_string()
            };
            for a in actions {
                add(a.index, BlockerCode::ScanIncomplete, why.clone());
            }
            return out;
        }

        // T5-09: the ignore list is read now, not at planning.
        let ignored = match &self.ports.ignores {
            Some(store) => store.ignored().await.map_err(|e| e.detail),
            None => Err("no ignore list is configured".to_string()),
        };
        // T5-06: minimum age of every pinned entry.
        let min_ms = i64::from(self.settings.auto_min_age_hours).saturating_mul(3_600_000);
        for a in actions {
            match &ignored {
                Err(why) => add(
                    a.index,
                    BlockerCode::Ignored,
                    format!("ignore list unreadable: {why}"),
                ),
                Ok(set) => {
                    if subjects_of(a).iter().any(|s| set.contains(s)) {
                        add(
                            a.index,
                            BlockerCode::Ignored,
                            "the subject is on the ignore list".into(),
                        );
                    }
                }
            }
            for (path, sig) in pinned_sigs(a) {
                let mtime_ms = sig.mtime_ns.0 / 1_000_000;
                if now.as_millis().saturating_sub(mtime_ms) < min_ms {
                    add(
                        a.index,
                        BlockerCode::Recent,
                        format!("{path} is younger than the minimum age"),
                    );
                    break;
                }
            }
            if matches!(
                a.kind,
                ActionKind::DeleteCompletedFile | ActionKind::DeleteCompletedFolder
            ) {
                for b in self.arr_validated_blockers(a).await {
                    add(a.index, b.code, b.detail);
                }
            }
        }
        out
    }

    /// T5-05 (CP1-F02) at execution for one automatic deletion.
    async fn arr_validated_blockers(&self, a: &PlannedAction) -> Vec<Blocker> {
        let amb = |m: String| Blocker::new(BlockerCode::ArrAmbiguous, m);
        let mut out = Vec::new();
        let replacements: Vec<(&ArrMediaRef, &RootedPath, u64)> = a
            .preconditions
            .iter()
            .filter_map(|p| match p {
                Precondition::ArrReplacementPresent { media, path, size } => {
                    Some((media, path, *size))
                }
                _ => None,
            })
            .collect();
        if replacements.is_empty() {
            return vec![amb("no ARR replacement is pinned".into())];
        }
        let replacement_paths: BTreeSet<&RootedPath> = replacements.iter().map(|r| r.1).collect();
        let targets: Vec<(RootedPath, EntrySignature)> = pinned_sigs(a)
            .into_iter()
            .filter(|(p, _)| !replacement_paths.contains(p))
            .collect();
        let mut live_replacement_phys = Vec::new();
        for (media, path, size) in &replacements {
            let current = match self.arr_current_files(media.service, media).await {
                Ok(m) => m,
                Err(e) => {
                    out.push(amb(format!(
                        "ARR's current files cannot be read: {}",
                        e.detail
                    )));
                    continue;
                }
            };
            let in_media: Vec<&RootedPath> = current
                .iter()
                .filter(|(ep, _)| media.episode_ids.is_empty() || media.episode_ids.contains(ep))
                .map(|(_, p)| p)
                .collect();
            if !in_media.contains(path) {
                out.push(amb(format!("ARR's current file is no longer {path}")));
                continue;
            }
            match self.stat_path(path).await {
                Ok(Some(s)) if s.kind == EntryKind::File && s.size == *size => {
                    live_replacement_phys.push(s.phys());
                }
                _ => out.push(amb(format!("{path} is not the planned regular file"))),
            }
            // The preview is not in the fingerprint: re-read it. Every completed file the
            // action deletes must still be rejected as not an upgrade.
            let Some(reader) = self.ports.arr.iter().find(|r| r.service() == media.service) else {
                out.push(amb("no ARR read adapter".into()));
                continue;
            };
            for (target, sig) in targets.iter().filter(|(_, s)| s.kind == EntryKind::File) {
                let Some(parent) = target.rel.parent() else {
                    continue;
                };
                let folder = RootedPath::new(target.root.clone(), parent);
                match reader.manual_import_preview(&folder, Some(media)).await {
                    Ok(decisions) => {
                        let subject = target.to_string();
                        match decisions.iter().find(|d| d.subject == subject) {
                            Some(d) if !d.approved || !d.rejections.is_empty() => {}
                            _ => out.push(Blocker::new(
                                BlockerCode::ArrNotUpgradeEvidence,
                                format!("ARR no longer rejects {target} as not an upgrade"),
                            )),
                        }
                    }
                    Err(e) => out.push(amb(format!("ARR preview failed: {}", e.detail))),
                }
                let _ = sig;
            }
        }
        let target_phys: BTreeSet<_> = targets.iter().map(|(_, s)| s.phys()).collect();
        if live_replacement_phys
            .iter()
            .any(|p| target_phys.contains(p))
        {
            out.push(amb(
                "a replacement shares an inode with a deletion target".into()
            ));
        }
        out
    }
}

/// Test-only view of the pure automatic checks (feature `tier5-test-gate`): lets a test feed
/// plans that `HashedPlan::seal` would never accept (a manual-only kind in an automatic plan).
#[cfg(feature = "tier5-test-gate")]
#[must_use]
pub fn automatic_defects_for_test(
    plan: &ActionPlan,
    hashed: &HashedPlan,
    approval: Option<&Approval>,
    settings: &ActionSettings,
    now: Timestamp,
    simulated: bool,
) -> BTreeMap<u32, Vec<Blocker>> {
    automatic_defects(plan, hashed, approval, settings, now, simulated)
}
