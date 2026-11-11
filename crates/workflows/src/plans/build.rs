//! The single planning function (architecture 7.3). Unit and bulk selections, manual and
//! automatic plans, previews and persisted plans all go through [`build_plan`].
//!
//! Order of work: validate the selection (whole request rejected on unknown, stale, ignored or
//! wrong-category ids), expand each issue into a unit of drafted actions with preconditions and
//! verification captured from the stored observation, exclude every blocked unit with its
//! blockers, add companion actions, compute gains once per inode, then validate and seal.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::evidence::{Completeness, EvidenceLevel};
use removarr_domain::fs::{EntryKind, FsEntry, PhysicalId};
use removarr_domain::gain::{GainEstimate, PlanGains, gain_id};
use removarr_domain::ids::{GainId, IssueId, PlanId, SubjectKey};
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Unit};
use removarr_domain::path::{RootId, RootKind, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionPlan, ActionTarget, ArrSnapshotRef, HashedPlan, PlanOrigin, PlannedAction,
    Precondition, VerificationStep,
};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::{MtimeNs, Timestamp};
use removarr_domain::torrent::{TorrentHash, TorrentState};
use serde::{Deserialize, Serialize};

use super::auto::{AutoContext, auto_blockers};
use super::facts::Facts;
use super::input::PlanInput;
use super::request::{IssueAction, PlanError, PlanRequest, PlanSettings, SelectionFault};
use super::validate::check_plan;
use super::{arrplans, fsplans, gate, service};
use crate::issues::{evidence_level, is_ignored};

/// One action under construction. `deps` index the unit's own drafts.
#[derive(Debug)]
pub(crate) struct Draft<'a> {
    pub kind: ActionKind,
    pub target: ActionTarget,
    pub pre: Vec<Precondition>,
    pub verify: Vec<VerificationStep>,
    pub deps: Vec<usize>,
    pub hashes: BTreeSet<TorrentHash>,
    pub level: EvidenceLevel,
    pub completeness: Completeness,
    /// Regular-file links this action removes (gain accounting).
    pub freed: Vec<&'a FsEntry>,
    pub logical: u64,
    pub added: u64,
}

impl<'a> Draft<'a> {
    pub fn new(kind: ActionKind, target: ActionTarget) -> Self {
        Self {
            kind,
            target,
            pre: Vec::new(),
            verify: Vec::new(),
            deps: Vec::new(),
            hashes: BTreeSet::new(),
            level: EvidenceLevel::None,
            completeness: Completeness::Complete,
            freed: Vec::new(),
            logical: 0,
            added: 0,
        }
    }
}

/// The drafted actions of one selected item.
#[derive(Debug, Default)]
pub(crate) struct UnitPlan<'a> {
    pub drafts: Vec<Draft<'a>>,
    /// Samples inside a folder unit: each needs an acknowledgement (OQ-11).
    pub samples: Vec<RootedPath>,
    pub arr: Vec<ArrSnapshotRef>,
    /// Entries whose age counts for A3 (every entry of the unit).
    pub aged: Vec<&'a FsEntry>,
    /// Paths that a second unit may not also claim (CON-25).
    pub claims: Vec<RootedPath>,
}

/// Per-unit builder outcome: drafted actions, or the reasons it cannot be planned.
pub(crate) type UnitResult<'a> = Result<UnitPlan<'a>, Vec<Blocker>>;

/// Clock, identity and origin of one planning run.
#[derive(Debug, Clone)]
pub struct BuildContext<'c> {
    pub id: PlanId,
    pub now: Timestamp,
    pub origin: PlanOrigin,
    /// Required for `PlanOrigin::AutoPolicy`.
    pub auto: Option<&'c AutoContext>,
}

/// Expected gain of one inode, attributed once to the action that removes its last link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedGain {
    pub gain_id: GainId,
    pub phys: PhysicalId,
    pub bytes: u64,
    pub action: u32,
}

/// A sealed plan plus what the preview needs that is not part of the hashed content.
#[derive(Debug, Clone)]
pub struct Built {
    pub plan: HashedPlan,
    /// Per-inode expected gains: each inode appears at most once (no double counting).
    pub expected_gains: Vec<ExpectedGain>,
}

/// Builds and seals one plan. Pure: no I/O, deterministic for equal inputs.
pub fn build_plan(
    req: &PlanRequest,
    input: &PlanInput,
    settings: &PlanSettings,
    ctx: &BuildContext<'_>,
) -> Result<Built, PlanError> {
    if !input.session.status.allows_planning() {
        return Err(PlanError::SessionNotPlannable(input.session.status));
    }
    if ctx.origin == PlanOrigin::AutoPolicy {
        let auto = ctx
            .auto
            .ok_or_else(|| PlanError::AutoRefused("no automatic context".into()))?;
        auto.check_enabled()?;
    }
    let facts = Facts::new(input, settings);
    let mut units: Vec<(SubjectKey, UnitPlan<'_>)> = Vec::new();
    let mut excluded: Vec<(SubjectKey, Vec<Blocker>)> = Vec::new();
    let mut remove_tasks = false;

    match req {
        PlanRequest::Issues {
            category,
            issue_ids,
            action,
            remove_torrent_tasks,
            include_specials,
        } => {
            remove_tasks = *remove_torrent_tasks;
            // A7: automatic deletions need ARR evidence; plain deletions are manual only.
            if ctx.origin == PlanOrigin::AutoPolicy
                && !matches!(
                    action,
                    IssueAction::Consolidate | IssueAction::DeleteReplaced
                )
            {
                return Err(PlanError::AutoRefused(format!(
                    "{action:?} is never automatic"
                )));
            }
            let issues = select(input, *category, issue_ids, *action)?;
            if let IssueAction::ReplaceRelease { delete_old_media } = action {
                let opts = arrplans::ReplaceOptions {
                    delete_old_media: *delete_old_media,
                    include_specials: *include_specials,
                };
                for (subjects, result) in arrplans::replace_groups(&facts, &issues, opts) {
                    let gated = gate::blockers(ActionKind::ReplaceRelease, ctx.origin, settings);
                    let carried: Vec<Blocker> = subjects
                        .iter()
                        .filter_map(|s| issues.iter().find(|i| &i.subject == s))
                        .flat_map(|i| carried(i, *action, ctx.origin))
                        .collect();
                    accept(
                        &mut units,
                        &mut excluded,
                        subjects,
                        result,
                        [carried, gated].concat(),
                    );
                }
            } else {
                for issue in &issues {
                    let kind = main_kind(*action, issue);
                    let mut blockers = carried(issue, *action, ctx.origin);
                    blockers.extend(gate::blockers(kind, ctx.origin, settings));
                    let result = match action {
                        IssueAction::Consolidate => fsplans::consolidate(&facts, issue),
                        IssueAction::Delete => fsplans::delete(&facts, issue, kind, ctx.origin),
                        IssueAction::DeleteReplaced => {
                            arrplans::delete_replaced(&facts, issue, kind, ctx.origin)
                        }
                        IssueAction::CleanupStalled => fsplans::cleanup_stalled(&facts, issue),
                        IssueAction::FixMode => fsplans::fix_mode(&facts, issue),
                        IssueAction::RemoveQueueItem => service::remove_queue_item(&facts, issue),
                        IssueAction::ReplaceRelease { .. } => unreachable_unit(),
                    };
                    let result = match (result, ctx.auto) {
                        (Ok(u), Some(auto)) if ctx.origin == PlanOrigin::AutoPolicy => {
                            let extra = auto_blockers(&facts, issue, &u, auto, ctx.now);
                            if extra.is_empty() { Ok(u) } else { Err(extra) }
                        }
                        (r, _) => r,
                    };
                    accept(
                        &mut units,
                        &mut excluded,
                        vec![issue.subject.clone()],
                        result,
                        blockers,
                    );
                }
            }
        }
        PlanRequest::Media { media, action } => {
            let (subject, kind, result) = service::media_action(&facts, media, *action);
            let blockers = gate::blockers(kind, ctx.origin, settings);
            accept(&mut units, &mut excluded, vec![subject], result, blockers);
        }
        PlanRequest::Release { media, release } => {
            let (subject, result) = arrplans::grab(&facts, media, release);
            let blockers = gate::blockers(ActionKind::ArrGrabRelease, ctx.origin, settings);
            accept(&mut units, &mut excluded, vec![subject], result, blockers);
        }
        PlanRequest::ServiceRescan { service: s, media } => {
            let (subject, kind, result) = service::rescan(&facts, *s, media.as_ref())?;
            let blockers = gate::blockers(kind, ctx.origin, settings);
            accept(&mut units, &mut excluded, vec![subject], result, blockers);
        }
    }

    excluded.sort_by(|a, b| a.0.cmp(&b.0));
    if units.is_empty() {
        return Err(PlanError::NothingPlannable { excluded });
    }
    assemble(&facts, units, excluded, remove_tasks, ctx)
}

fn unreachable_unit<'a>() -> UnitResult<'a> {
    Err(vec![Blocker::new(
        BlockerCode::NotInBuild,
        "release replacement is planned per media",
    )])
}

/// Adds a unit, or lists it as excluded with every reason (carried, gate and builder).
fn accept<'a>(
    units: &mut Vec<(SubjectKey, UnitPlan<'a>)>,
    excluded: &mut Vec<(SubjectKey, Vec<Blocker>)>,
    subjects: Vec<SubjectKey>,
    result: UnitResult<'a>,
    mut blockers: Vec<Blocker>,
) {
    let claimed: BTreeSet<&RootedPath> = units.iter().flat_map(|(_, u)| &u.claims).collect();
    match result {
        Ok(u) => {
            if let Some(c) = u.claims.iter().find(|c| conflicts(c, &claimed)) {
                blockers.push(Blocker::new(
                    BlockerCode::DuplicateTarget,
                    format!("{c} is already part of this plan"),
                ));
            }
            if blockers.is_empty() {
                let subject = subjects
                    .into_iter()
                    .next()
                    .unwrap_or(SubjectKey(String::new()));
                units.push((subject, u));
                return;
            }
        }
        Err(b) => blockers.extend(b),
    }
    blockers.sort_by(|a, b| (&a.code, &a.detail).cmp(&(&b.code, &b.detail)));
    blockers.dedup();
    for s in subjects {
        excluded.push((s, blockers.clone()));
    }
}

/// A claim conflicts when it equals, contains or lies inside an earlier claim.
fn conflicts(c: &RootedPath, claimed: &BTreeSet<&RootedPath>) -> bool {
    claimed
        .iter()
        .any(|o| o.root == c.root && (o.rel.starts_with(&c.rel) || c.rel.starts_with(&o.rel)))
}

/// Validates the selection: the whole request is rejected on any bad id (CON-32, INV-24).
fn select(
    input: &PlanInput,
    category: IssueCategory,
    ids: &[IssueId],
    action: IssueAction,
) -> Result<Vec<Issue>, PlanError> {
    if ids.is_empty() {
        return Err(PlanError::InvalidRequest(
            "no item selected (there is no implicit selection)".into(),
        ));
    }
    if !action.accepts(category) {
        return Err(PlanError::InvalidRequest(format!(
            "action {action:?} does not apply to category {}",
            category.as_str()
        )));
    }
    let wanted: BTreeSet<&IssueId> = ids.iter().collect();
    let by_id: BTreeMap<&IssueId, &Issue> = input.issues.iter().map(|i| (&i.id, i)).collect();
    let reject = |reason, ids: Vec<IssueId>| {
        if ids.is_empty() {
            Ok(())
        } else {
            Err(PlanError::RejectedSelection { reason, ids })
        }
    };
    reject(
        SelectionFault::UnknownOrResolved,
        wanted
            .iter()
            .filter(|i| !by_id.contains_key(**i))
            .map(|i| (*i).clone())
            .collect(),
    )?;
    let found: Vec<&Issue> = wanted
        .iter()
        .filter_map(|i| by_id.get(*i).copied())
        .collect();
    reject(
        SelectionFault::Stale,
        found
            .iter()
            .filter(|i| i.session != input.session.id || i.expected_id() != i.id)
            .map(|i| i.id.clone())
            .collect(),
    )?;
    reject(
        SelectionFault::WrongCategory,
        found
            .iter()
            .filter(|i| i.category != category)
            .map(|i| i.id.clone())
            .collect(),
    )?;
    reject(
        SelectionFault::Ignored,
        found
            .iter()
            .filter(|i| is_ignored(i) || input.observation.ignored.contains(&i.subject))
            .map(|i| i.id.clone())
            .collect(),
    )?;
    let mut out: Vec<Issue> = found.into_iter().cloned().collect();
    out.sort_by(|a, b| a.subject.cmp(&b.subject));
    Ok(out)
}

/// The main action kind for one issue.
fn main_kind(action: IssueAction, issue: &Issue) -> ActionKind {
    let folder = matches!(issue.unit, Unit::Folder { .. });
    match action {
        IssueAction::Consolidate => ActionKind::ConsolidateHardlink,
        IssueAction::Delete | IssueAction::DeleteReplaced => match issue.category {
            IssueCategory::Junk => ActionKind::DeleteJunk,
            IssueCategory::IncompleteOrphan => ActionKind::DeleteIncompleteOrphan,
            _ if folder => ActionKind::DeleteCompletedFolder,
            _ => ActionKind::DeleteCompletedFile,
        },
        IssueAction::CleanupStalled => ActionKind::CleanupStalledIncomplete,
        IssueAction::FixMode => ActionKind::FixMode,
        IssueAction::ReplaceRelease { .. } => ActionKind::ReplaceRelease,
        IssueAction::RemoveQueueItem => ActionKind::ArrRemoveQueueItem,
    }
}

fn is_arr_service(k: ServiceKind) -> bool {
    matches!(k, ServiceKind::Sonarr | ServiceKind::Radarr)
}

/// Issue blockers that still refuse the requested action. An action may only lift what it
/// re-establishes itself with fresh evidence (for example ARR identity for `DeleteReplaced`),
/// or what the user acknowledges explicitly at approval (samples inside a manual folder plan).
fn carried(issue: &Issue, action: IssueAction, origin: PlanOrigin) -> Vec<Blocker> {
    let arr_identity = |c: &BlockerCode| match c {
        BlockerCode::ArrNotUpgradeEvidence
        | BlockerCode::ArrAmbiguous
        | BlockerCode::ArrMissing => true,
        BlockerCode::ServiceUnavailable(k) | BlockerCode::ServiceNotConfigured(k) => {
            is_arr_service(*k)
        }
        _ => false,
    };
    // Consolidation-only reasons that say nothing about deleting or importing the source.
    let consolidation = |c: &BlockerCode| {
        matches!(
            c,
            BlockerCode::CrossDevice
                | BlockerCode::DuplicateTarget
                | BlockerCode::ContentDiffers
                | BlockerCode::PermissionDenied
        )
    };
    let hardlink = issue.category == IssueCategory::HardlinkMissing;
    let folder = matches!(issue.unit, Unit::Folder { .. });
    issue
        .blockers
        .iter()
        .filter(|b| !b.code.is_warning())
        .filter(|b| {
            let lifted = match action {
                IssueAction::Delete => {
                    (hardlink && (arr_identity(&b.code) || consolidation(&b.code)))
                        || (folder && origin == PlanOrigin::User && b.code == BlockerCode::Sample)
                }
                IssueAction::DeleteReplaced => {
                    arr_identity(&b.code)
                        || (hardlink && consolidation(&b.code))
                        || (folder && origin == PlanOrigin::User && b.code == BlockerCode::Sample)
                }
                IssueAction::ReplaceRelease { .. } => {
                    arr_identity(&b.code) || consolidation(&b.code)
                }
                _ => false,
            };
            !lifted
        })
        .cloned()
        .collect()
}

/// Assigns indices, adds companions, computes gains, validates and seals.
fn assemble<'a>(
    facts: &Facts<'a>,
    units: Vec<(SubjectKey, UnitPlan<'a>)>,
    excluded: Vec<(SubjectKey, Vec<Blocker>)>,
    remove_tasks: bool,
    ctx: &BuildContext<'_>,
) -> Result<Built, PlanError> {
    let settings = facts.settings;
    let mut drafts: Vec<Draft<'a>> = Vec::new();
    let mut samples: BTreeSet<RootedPath> = BTreeSet::new();
    let mut arr: Vec<ArrSnapshotRef> = Vec::new();
    for (_, u) in units {
        let base = drafts.len();
        samples.extend(u.samples);
        for a in u.arr {
            if !arr.contains(&a) {
                arr.push(a);
            }
        }
        for mut d in u.drafts {
            d.deps = d.deps.iter().map(|i| base + i).collect();
            drafts.push(d);
        }
    }
    let companion_origin_ok = |k: ActionKind| gate::blockers(k, ctx.origin, settings).is_empty();
    if companion_origin_ok(ActionKind::RemoveEmptyParent) {
        let parents = fsplans::empty_parents(facts, &drafts, ctx.origin == PlanOrigin::AutoPolicy);
        drafts.extend(parents);
    }
    if remove_tasks && companion_origin_ok(ActionKind::RemoveTorrentTask) {
        let tasks = fsplans::torrent_removals(facts, &drafts, ctx.origin == PlanOrigin::AutoPolicy);
        drafts.extend(tasks);
    }

    let (per_action, expected_gains) = gains(facts, &drafts);
    let mut actions = Vec::with_capacity(drafts.len());
    let mut lock_roots: BTreeSet<RootId> = BTreeSet::new();
    let mut torrent_states: BTreeMap<TorrentHash, (TorrentState, f64)> = BTreeMap::new();
    for (i, d) in drafts.into_iter().enumerate() {
        let index =
            u32::try_from(i).map_err(|_| PlanError::InvalidRequest("too many actions".into()))?;
        lock_roots.extend(d.target.roots());
        let mut pre = d.pre;
        for p in &pre {
            match p {
                Precondition::EntryUnchanged { path, .. } => {
                    lock_roots.insert(path.root.clone());
                }
                Precondition::DirUnchanged { snapshot } => {
                    lock_roots.insert(snapshot.dir.root.clone());
                }
                Precondition::TorrentStates { states } => {
                    torrent_states.extend(states.iter().map(|(h, s)| (h.clone(), s.clone())));
                }
                _ => {}
            }
        }
        pre.push(Precondition::ConfigFingerprint(
            settings.config_fingerprint.clone(),
        ));
        let mut deps: Vec<u32> = d
            .deps
            .iter()
            .filter_map(|x| u32::try_from(*x).ok())
            .collect();
        deps.sort_unstable();
        deps.dedup();
        actions.push(PlannedAction {
            index,
            kind: d.kind,
            target: d.target,
            preconditions: pre,
            verification: d.verify,
            expected_gain: GainEstimate {
                logical: d.logical,
                attributable: per_action.get(i).copied().unwrap_or(0),
                added: d.added,
            },
            depends_on: deps,
            torrent_hashes: d.hashes,
            evidence_level: d.level,
            completeness: d.completeness,
        });
    }
    let totals = actions.iter().fold((0u64, 0u64, 0u64), |acc, a| {
        (
            acc.0.saturating_add(a.expected_gain.logical),
            acc.1.saturating_add(a.expected_gain.attributable),
            acc.2.saturating_add(a.expected_gain.added),
        )
    });
    let ttl = i64::from(settings.plan_ttl_hours.max(1));
    let plan = ActionPlan {
        id: ctx.id,
        created_at: ctx.now,
        expires_at: ctx.now.plus_hours(ttl),
        mode: settings.mode,
        origin: ctx.origin,
        session: facts.input.session.id,
        config_fingerprint: settings.config_fingerprint.clone(),
        actions,
        excluded,
        lock_roots,
        torrent_states,
        arr_snapshots: arr,
        samples: samples.into_iter().collect(),
        gains: PlanGains::new(totals.0, totals.1, totals.2),
    };
    let violations = check_plan(&plan);
    if !violations.is_empty() {
        return Err(PlanError::RuleViolation(violations));
    }
    Ok(Built {
        plan: HashedPlan::seal(plan)?,
        expected_gains,
    })
}

/// Attributable bytes per action and per inode (DR-04, T-03, INV-21). An inode counts once,
/// only when every link seen in the inventories (and at least `nlink`) is removed by the plan,
/// and it is attributed to the action that removes its last link.
fn gains(facts: &Facts<'_>, drafts: &[Draft<'_>]) -> (Vec<u64>, Vec<ExpectedGain>) {
    struct Slot<'e> {
        paths: BTreeSet<&'e RootedPath>,
        last: usize,
        entry: &'e FsEntry,
    }
    let mut slots: BTreeMap<PhysicalId, Slot<'_>> = BTreeMap::new();
    for (i, d) in drafts.iter().enumerate() {
        for e in d.freed.iter().filter(|e| e.sig.kind == EntryKind::File) {
            let s = slots.entry(e.sig.phys()).or_insert(Slot {
                paths: BTreeSet::new(),
                last: i,
                entry: e,
            });
            s.paths.insert(&e.path);
            s.last = s.last.max(i);
        }
    }
    let mut per_action = vec![0u64; drafts.len()];
    let mut lines = Vec::new();
    for (phys, s) in slots {
        let seen = u64::try_from(facts.links_of(&phys).len()).unwrap_or(u64::MAX);
        let needed = s.entry.sig.nlink.max(seen);
        let selected = u64::try_from(s.paths.len()).unwrap_or(0);
        if selected == 0 || selected < needed {
            continue;
        }
        if let Some(slot) = per_action.get_mut(s.last) {
            *slot = slot.saturating_add(s.entry.allocated);
        }
        lines.push(ExpectedGain {
            gain_id: gain_id(phys, s.entry.sig.size, MtimeNs(s.entry.sig.mtime_ns.0)),
            phys,
            bytes: s.entry.allocated,
            action: u32::try_from(s.last).unwrap_or(u32::MAX),
        });
    }
    (per_action, lines)
}

/// Completeness of an issue's evidence, as recorded on its actions.
pub(crate) fn issue_completeness(issue: &Issue) -> Completeness {
    issue.completeness()
}

/// Content-proof level of an issue (strongest `Content` evidence).
pub(crate) fn issue_level(issue: &Issue) -> EvidenceLevel {
    evidence_level(issue)
}

/// True for a root of the given kind.
pub(crate) fn is_kind(path: &RootedPath, kind: RootKind) -> bool {
    path.root.kind() == kind
}
