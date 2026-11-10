//! Automatic policy (architecture 13.3, rules A1-A9). An automatic plan contains a unit only if
//! every rule holds; anything uncertain is excluded with its reason. Automatic plans are built
//! by the same [`super::build_plan`] as manual ones, with `PlanOrigin::AutoPolicy`.

use std::collections::BTreeSet;

use removarr_domain::evidence::EvidenceLevel;
use removarr_domain::fs::FsEntry;
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Outcome, Unit};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{ActionKind, ActionTarget, Precondition};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use serde::{Deserialize, Serialize};

use super::build::UnitPlan;
use super::facts::Facts;
use super::input::PlanInput;
use super::request::{IssueAction, PlanError, PlanRequest};
use crate::issues::is_ignored;

/// Runtime state of automatic mode, supplied by the job runner (14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoContext {
    /// `mode.auto_mode` (A1). Off by default.
    pub auto_mode: bool,
    /// `auto_paused` reason from the state store (A1). Set = nothing automatic.
    pub auto_paused: Option<String>,
    /// Subjects with a failed action in the last 7 days (A4).
    pub failed_subjects: BTreeSet<SubjectKey>,
    /// Paths involved in a failed or unresolved consolidation (A4, T-20).
    pub failed_paths: BTreeSet<RootedPath>,
}

impl AutoContext {
    /// A1.
    pub fn check_enabled(&self) -> Result<(), PlanError> {
        if !self.auto_mode {
            return Err(PlanError::AutoRefused("auto_mode is off".into()));
        }
        if let Some(r) = &self.auto_paused {
            return Err(PlanError::AutoRefused(format!("auto mode is paused: {r}")));
        }
        Ok(())
    }
}

/// The two phases of the automatic cycle (architecture 7.6, DR-34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoPhase {
    /// Exact-copy consolidations.
    Consolidations,
    /// Deletions under completed with ARR evidence (`DeleteReplaced` only).
    Deletions,
}

/// The requests an automatic phase plans: every active issue of the eligible categories. There
/// is no user selection here; the policy considers each candidate and excludes the uncertain.
#[must_use]
pub fn auto_requests(phase: AutoPhase, input: &PlanInput) -> Vec<PlanRequest> {
    let ids = |cat: IssueCategory, pred: &dyn Fn(&Issue) -> bool| -> Vec<IssueId> {
        input
            .issues
            .iter()
            .filter(|i| i.category == cat && !is_ignored(i) && pred(i))
            .map(|i| i.id.clone())
            .collect()
    };
    let mut out = Vec::new();
    let mut push = |cat, action, ids: Vec<IssueId>| {
        if !ids.is_empty() {
            out.push(PlanRequest::Issues {
                category: cat,
                issue_ids: ids,
                action,
                remove_torrent_tasks: true,
                include_specials: false,
            });
        }
    };
    match phase {
        AutoPhase::Consolidations => {
            push(
                IssueCategory::HardlinkMissing,
                IssueAction::Consolidate,
                ids(IssueCategory::HardlinkMissing, &|i| {
                    i.outcome == Outcome::Consolidate
                }),
            );
            push(
                IssueCategory::Duplicate,
                IssueAction::Consolidate,
                ids(IssueCategory::Duplicate, &|_| true),
            );
        }
        AutoPhase::Deletions => {
            push(
                IssueCategory::HardlinkMissing,
                IssueAction::DeleteReplaced,
                ids(IssueCategory::HardlinkMissing, &|i| {
                    i.outcome == Outcome::KeepManual
                }),
            );
            push(
                IssueCategory::OrphanCompleted,
                IssueAction::DeleteReplaced,
                ids(IssueCategory::OrphanCompleted, &|_| true),
            );
        }
    }
    out
}

/// Roots an action touches: its targets and every path its preconditions pin.
fn touched_roots(u: &UnitPlan<'_>) -> Vec<RootId> {
    let mut roots: BTreeSet<RootId> = BTreeSet::new();
    for d in &u.drafts {
        roots.extend(d.target.roots());
        for p in &d.pre {
            match p {
                Precondition::EntryUnchanged { path, .. }
                | Precondition::NoScanErrorUnder { path }
                | Precondition::NoTorrentReferences { path } => {
                    roots.insert(path.root.clone());
                }
                Precondition::DirUnchanged { snapshot } => {
                    roots.insert(snapshot.dir.root.clone());
                }
                _ => {}
            }
        }
    }
    roots.into_iter().collect()
}

fn paths_of<'u>(u: &'u UnitPlan<'_>) -> Vec<&'u RootedPath> {
    let mut out: Vec<&RootedPath> = u.claims.iter().collect();
    for d in &u.drafts {
        if let ActionTarget::Consolidate { source, target } = &d.target {
            out.extend([source, target]);
        }
    }
    out
}

fn mtime_ms(e: &FsEntry) -> i64 {
    e.sig.mtime_ns.0.div_euclid(1_000_000)
}

/// Rules A2-A8 for one drafted unit (A1 is checked before planning, A9 by the gate and the
/// domain). Empty = the unit may be part of an automatic plan.
pub(crate) fn auto_blockers(
    f: &Facts<'_>,
    issue: &Issue,
    u: &UnitPlan<'_>,
    ctx: &AutoContext,
    now: Timestamp,
) -> Vec<Blocker> {
    let mut out = Vec::new();
    // A2: whole-root perimeter and every service the action depends on.
    out.extend(f.perimeter.auto_blockers(&touched_roots(u)));
    for d in &u.drafts {
        for p in &d.pre {
            if let Precondition::ServiceAvailable(k) = p
                && !f.service_ok(*k)
            {
                out.push(Blocker::new(
                    BlockerCode::ServiceUnavailable(*k),
                    format!("{} is not Ok in the latest session", k.as_str()),
                ));
            }
        }
    }
    if !f.service_ok(ServiceKind::Qbittorrent) {
        out.push(Blocker::new(
            BlockerCode::ServiceUnavailable(ServiceKind::Qbittorrent),
            "qBittorrent is not Ok in the latest session",
        ));
    }
    // A3: every entry of the unit is old enough (newest mtime).
    let min_ms = i64::from(f.settings.auto_min_age_hours).saturating_mul(3_600_000);
    if let Some(newest) = u.aged.iter().map(|e| mtime_ms(e)).max()
        && now.0.saturating_sub(newest) < min_ms
    {
        out.push(Blocker::new(
            BlockerCode::Recent,
            format!(
                "an entry is younger than {} hours",
                f.settings.auto_min_age_hours
            ),
        ));
    }
    // A4: ignored, samples, failed or unresolved consolidations.
    if is_ignored(issue) || f.input.observation.ignored.contains(&issue.subject) {
        out.push(Blocker::new(BlockerCode::Ignored, "subject is ignored"));
    }
    if !u.samples.is_empty() {
        out.push(Blocker::new(
            BlockerCode::Sample,
            "samples need a manual acknowledgement",
        ));
    }
    if ctx.failed_subjects.contains(&issue.subject) {
        out.push(Blocker::new(
            BlockerCode::FailedConsolidation,
            "the same subject failed in the last 7 days",
        ));
    }
    for p in paths_of(u) {
        if ctx.failed_paths.contains(p) {
            out.push(Blocker::new(
                BlockerCode::FailedConsolidation,
                format!("{p} is involved in a failed or unresolved consolidation"),
            ));
        }
    }
    for d in &u.drafts {
        // A6: an exact-copy candidate is name+size, sampled or fully compared.
        if d.kind == ActionKind::ConsolidateHardlink
            && !matches!(
                d.level,
                EvidenceLevel::NameSize | EvidenceLevel::Sampled | EvidenceLevel::Full
            )
        {
            out.push(Blocker::new(
                BlockerCode::ScanIncomplete,
                "content samples were not checked for this candidate",
            ));
        }
        // A7 / completeness: automatic plans hold complete evidence only.
        if !d.completeness.is_complete() {
            out.push(Blocker::new(
                BlockerCode::ScanIncomplete,
                "evidence is incomplete",
            ));
        }
    }
    // A7: deletions in automatic mode are `DeleteReplaced` units only (checked by the request);
    // a folder must hold no ambiguous member (all-or-nothing, enforced by the builder).
    if let Unit::Folder { .. } = issue.unit
        && u.arr.is_empty()
    {
        out.push(Blocker::new(
            BlockerCode::ArrNotUpgradeEvidence,
            "an automatic folder deletion needs ARR evidence for every member",
        ));
    }
    out.sort_by(|a, b| (&a.code, &a.detail).cmp(&(&b.code, &b.detail)));
    out.dedup();
    out
}
