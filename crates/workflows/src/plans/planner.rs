//! The planning use case with its ports: loads the stored observation, gathers fresh ARR
//! evidence when the action needs it, builds through [`build_plan`], persists immutably and
//! records approvals. It never mutates a root or a service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use removarr_domain::arr::ArrMediaRef;
use removarr_domain::error::{DomainError, ErrorClass, PortError};
use removarr_domain::ids::{IssueId, PlanId};
use removarr_domain::issue::{IssueCategory, Unit};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{
    ActionPlan, Approval, ApprovalRequest, HashedPlan, PlanMode, PlanOrigin, PlanStatus,
};
use removarr_domain::ports::{
    ArrRead, Clock, IdGen, IgnoreStore, IssueStore, MatchStore, MediaProbe, PlanStore, ScanStore,
    SnapshotStore,
};
use removarr_domain::session::{ServiceKind, ServiceState};

use super::arrplans::identity_of;
use super::auto::{AutoContext, AutoPhase, auto_plan_defects, auto_requests};
use super::build::{BuildContext, Built, build_plan};
use super::expiry::{ArrFingerprints, StaleReason, staleness, verdict};
use super::facts::Facts;
use super::input::{MediaKey, PlanInput};
use super::request::{IssueAction, PlanError, PlanRequest, PlanSettings};
use crate::evaluate::{ArrFacts, GatherRequest, gather};
use crate::issues::{ArrObservation, Observation, QbitObservation};

/// Ports of the planner. Persistence ports are the storage repositories; `arr` and `probe` are
/// read adapters (no mutation is reachable from here).
#[derive(Clone)]
pub struct PlanPorts {
    pub scans: Arc<dyn ScanStore>,
    pub snapshots: Arc<dyn SnapshotStore>,
    pub matches: Arc<dyn MatchStore>,
    pub issues: Arc<dyn IssueStore>,
    pub ignores: Arc<dyn IgnoreStore>,
    pub plans: Arc<dyn PlanStore>,
    pub arr: Vec<Arc<dyn ArrRead>>,
    pub probe: Option<Arc<dyn MediaProbe>>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

impl std::fmt::Debug for PlanPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanPorts")
            .field("arr", &self.arr.len())
            .field("probe", &self.probe.is_some())
            .finish_non_exhaustive()
    }
}

/// ARR reads a request needs: per media, and which files of which issue they concern.
#[derive(Debug, Clone, Default)]
pub struct ArrNeeds {
    pub media: BTreeMap<MediaKey, (ArrMediaRef, GatherRequest)>,
    pub by_issue: BTreeMap<IssueId, (MediaKey, Vec<RootedPath>)>,
}

/// Media and folders to read from ARR for the selected completed files (identity through
/// torrent hashes and library candidates, as the classifier correlates).
#[must_use]
pub fn arr_needs(input: &PlanInput, settings: &PlanSettings, probe: bool) -> ArrNeeds {
    let f = Facts::new(input, settings);
    let mut needs = ArrNeeds::default();
    for issue in &input.issues {
        let videos: Vec<&RootedPath> = match (&issue.category, &issue.unit) {
            (IssueCategory::HardlinkMissing | IssueCategory::OrphanCompleted, Unit::File(p)) => {
                vec![p]
            }
            (IssueCategory::OrphanCompleted, Unit::Folder { members, .. }) => {
                members.iter().collect()
            }
            (IssueCategory::Duplicate, Unit::Pair { a, .. }) => vec![a],
            _ => continue,
        };
        for v in videos {
            let Ok(m) = identity_of(&f, issue, v) else {
                continue;
            };
            let key = (m.service, m.media_id);
            let slot = needs
                .media
                .entry(key)
                .or_insert_with(|| (m.clone(), GatherRequest::default()));
            if let Some(parent) = v.rel.parent() {
                slot.1
                    .folders
                    .insert(RootedPath::new(v.root.clone(), parent));
            }
            if probe {
                slot.1.probe.insert(v.clone());
            }
            let e = needs
                .by_issue
                .entry(issue.id.clone())
                .or_insert_with(|| (key, Vec::new()));
            if e.0 == key {
                e.1.push(v.clone());
            }
        }
    }
    needs
}

/// A plan as stored.
#[derive(Debug, Clone)]
pub struct StoredPlan {
    pub plan: HashedPlan,
    pub status: PlanStatus,
    pub approval: Option<Approval>,
}

/// What the action runner receives (handoff contract): a tamper-checked plan and, for live
/// execution, the approval of exactly this hash.
#[derive(Debug, Clone)]
pub struct ExecutionTicket {
    pub plan: HashedPlan,
    pub status: PlanStatus,
    pub approval: Option<Approval>,
    pub simulate: bool,
}

/// An automatic plan, built and approved by policy.
#[derive(Debug, Clone)]
pub struct AutoPlan {
    pub built: Built,
    pub approval: Approval,
}

/// Outcome of one automatic phase: approved plans and requests the policy refused.
#[derive(Debug, Clone, Default)]
pub struct AutoOutcome {
    pub plans: Vec<AutoPlan>,
    pub refused: Vec<(IssueCategory, String)>,
}

/// Result of a status refresh.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCheck {
    pub status: PlanStatus,
    pub reasons: Vec<StaleReason>,
}

#[derive(Debug, Clone)]
pub struct Planner {
    ports: PlanPorts,
    settings: PlanSettings,
}

fn port(e: PortError) -> PlanError {
    PlanError::Port(e)
}

impl Planner {
    #[must_use]
    pub fn new(ports: PlanPorts, settings: PlanSettings) -> Self {
        Self { ports, settings }
    }

    #[must_use]
    pub fn settings(&self) -> &PlanSettings {
        &self.settings
    }

    #[must_use]
    pub fn ports(&self) -> &PlanPorts {
        &self.ports
    }

    fn arr_for(&self, m: &ArrMediaRef) -> Option<&Arc<dyn ArrRead>> {
        self.ports.arr.iter().find(|a| a.service() == m.service)
    }

    /// Loads the stored observation of the latest session and the selected issues. `arr_obs`
    /// are the in-memory ARR observations (`observe::ServiceCache`); without them no ARR
    /// identity is known and ARR-dependent actions are blocked.
    pub async fn load(
        &self,
        ids: &[IssueId],
        arr_obs: Vec<ArrObservation>,
    ) -> Result<PlanInput, PlanError> {
        let p = &self.ports;
        let session = p
            .scans
            .latest_session()
            .await
            .map_err(port)?
            .ok_or(PlanError::NoSession)?;
        if !session.status.allows_planning() {
            return Err(PlanError::SessionNotPlannable(session.status));
        }
        let mut inventories = Vec::new();
        for r in &session.roots {
            match p.scans.load_inventory(&r.root).await {
                Ok(inv) => inventories.push(inv),
                Err(e) if e.class == ErrorClass::NotFound => {}
                Err(e) => return Err(port(e)),
            }
        }
        let qbit_state = session
            .services
            .iter()
            .find(|s| s.service == ServiceKind::Qbittorrent)
            .map(|s| s.state.clone());
        let qbit = match qbit_state {
            Some(ServiceState::NotConfigured) => QbitObservation::NotConfigured,
            Some(ServiceState::Ok { .. }) => {
                match p.snapshots.latest_torrents().await.map_err(port)? {
                    Some(s) => QbitObservation::Available(s),
                    None => QbitObservation::Unavailable {
                        reason: "no stored torrent snapshot".into(),
                    },
                }
            }
            _ => QbitObservation::Unavailable {
                reason: "qBittorrent was not available in the latest session".into(),
            },
        };
        let mut wanted: Vec<IssueId> = ids.to_vec();
        wanted.sort();
        wanted.dedup();
        let issues = if wanted.is_empty() {
            Vec::new()
        } else {
            p.issues.get_many(&wanted).await.map_err(port)?
        };
        let observation = Observation {
            session: session.id,
            session_status: session.status,
            observed_at: session.finished_at.unwrap_or(session.started_at),
            now: p.clock.now(),
            configured_roots: session.roots.iter().map(|r| r.root.clone()).collect(),
            inventories,
            qbit,
            arr: arr_obs,
            matches: p.matches.current().await.map_err(port)?,
            ignored: p.ignores.ignored().await.map_err(port)?,
        };
        Ok(PlanInput {
            session,
            observation,
            issues,
            arr: BTreeMap::new(),
            search: None,
        })
    }

    /// Fresh ARR reads for every needed media (sequential: bounded by design).
    pub async fn gather_all(
        &self,
        needs: &ArrNeeds,
    ) -> BTreeMap<MediaKey, Result<ArrFacts, PortError>> {
        let mut out = BTreeMap::new();
        for (key, (media, req)) in &needs.media {
            let r = match self.arr_for(media) {
                Some(a) => gather(&**a, self.ports.probe.as_deref(), media, req).await,
                None => Err(PortError::new(
                    ErrorClass::Config,
                    format!("{} is not configured", media.service.as_str()),
                )),
            };
            out.insert(*key, r);
        }
        out
    }

    /// The full input of one request: stored observation plus the fresh ARR reads it needs.
    async fn input_for(
        &self,
        req: &PlanRequest,
        arr_obs: Vec<ArrObservation>,
    ) -> Result<PlanInput, PlanError> {
        let ids: &[IssueId] = match req {
            PlanRequest::Issues { issue_ids, .. } => issue_ids,
            _ => &[],
        };
        let mut input = self.load(ids, arr_obs).await?;
        self.add_arr(req, &mut input).await;
        Ok(input)
    }

    async fn add_arr(&self, req: &PlanRequest, input: &mut PlanInput) {
        let needs = match req {
            PlanRequest::Issues {
                action: IssueAction::DeleteReplaced,
                ..
            } => arr_needs(input, &self.settings, false),
            PlanRequest::Issues {
                action: IssueAction::ReplaceRelease { .. },
                ..
            } => arr_needs(input, &self.settings, true),
            PlanRequest::Media { media, .. } | PlanRequest::Release { media, .. } => {
                let mut n = ArrNeeds::default();
                n.media.insert(
                    (media.service, media.media_id),
                    (media.clone(), GatherRequest::default()),
                );
                n
            }
            _ => ArrNeeds::default(),
        };
        input.arr = self.gather_all(&needs).await;
        if let PlanRequest::Release { media, .. } = req {
            input.search = Some(match self.arr_for(media) {
                Some(a) => a.search_releases(media).await,
                None => Err(PortError::new(ErrorClass::Config, "service not configured")),
            });
        }
    }

    fn ctx<'c>(&self, origin: PlanOrigin, auto: Option<&'c AutoContext>) -> BuildContext<'c> {
        BuildContext {
            id: PlanId::new(self.ports.ids.uuid()),
            now: self.ports.clock.now(),
            origin,
            auto,
        }
    }

    /// Dry-run preview: the complete plan through the same path, not persisted.
    pub async fn preview(
        &self,
        req: &PlanRequest,
        arr_obs: Vec<ArrObservation>,
    ) -> Result<Built, PlanError> {
        let input = self.input_for(req, arr_obs).await?;
        build_plan(
            req,
            &input,
            &self.settings,
            &self.ctx(PlanOrigin::User, None),
        )
    }

    /// Builds and persists a user plan (status `Draft`). The commit is the "plan written and
    /// synced" step; ARR snapshots used by the plan are stored too.
    pub async fn create(
        &self,
        req: &PlanRequest,
        arr_obs: Vec<ArrObservation>,
    ) -> Result<Built, PlanError> {
        let input = self.input_for(req, arr_obs).await?;
        let built = build_plan(
            req,
            &input,
            &self.settings,
            &self.ctx(PlanOrigin::User, None),
        )?;
        self.persist(&built, &input).await?;
        Ok(built)
    }

    async fn persist(&self, built: &Built, input: &PlanInput) -> Result<(), PlanError> {
        for r in &built.plan.plan().arr_snapshots {
            if let Some(Ok(a)) = input.arr.get(&(r.media.service, r.media.media_id)) {
                self.ports
                    .snapshots
                    .save_arr(&a.snapshot)
                    .await
                    .map_err(port)?;
            }
        }
        let (plan, hash) = (built.plan.plan(), built.plan.hash());
        self.ports.plans.insert(plan, hash).await.map_err(port)
    }

    /// One automatic phase (A1-A9): plans every eligible candidate, keeps only units that
    /// satisfy every automatic rule, and approves as policy. `candidates` are the active issue
    /// ids of the triggering session (the job runner holds the classification).
    pub async fn create_auto(
        &self,
        phase: AutoPhase,
        ctx: &AutoContext,
        candidates: &[IssueId],
        arr_obs: Vec<ArrObservation>,
    ) -> Result<AutoOutcome, PlanError> {
        ctx.check_enabled()?;
        let base = self.load(candidates, arr_obs).await?;
        let mut out = AutoOutcome::default();
        for req in auto_requests(phase, &base) {
            let PlanRequest::Issues { category, .. } = &req else {
                continue;
            };
            let category = *category;
            let mut input = base.clone();
            self.add_arr(&req, &mut input).await;
            let built = match build_plan(
                &req,
                &input,
                &self.settings,
                &self.ctx(PlanOrigin::AutoPolicy, Some(ctx)),
            ) {
                Ok(b) => b,
                Err(PlanError::NothingPlannable { excluded }) => {
                    out.refused
                        .push((category, format!("{} item(s) excluded", excluded.len())));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let defects = auto_plan_defects(built.plan.plan(), &self.settings);
            if !defects.is_empty() {
                out.refused.push((category, defects.join("; ")));
                continue;
            }
            let approval = match built.plan.approve_auto(self.ports.clock.now()) {
                Ok(a) => a,
                Err(e) => {
                    out.refused.push((category, e.to_string()));
                    continue;
                }
            };
            self.persist(&built, &input).await?;
            self.ports.plans.approve(&approval).await.map_err(port)?;
            out.plans.push(AutoPlan { built, approval });
        }
        Ok(out)
    }

    /// Loads a stored plan; a tampered plan fails (`PlanHashMismatch`).
    pub async fn get(&self, id: PlanId) -> Result<StoredPlan, PlanError> {
        let (plan, hash, status, approval) = self
            .ports
            .plans
            .get(id)
            .await
            .map_err(port)?
            .ok_or_else(|| PlanError::Port(PortError::new(ErrorClass::NotFound, "unknown plan")))?;
        Ok(StoredPlan {
            plan: HashedPlan::from_parts(plan, hash)?,
            status,
            approval,
        })
    }

    async fn arr_fingerprints(&self, plan: &ActionPlan) -> ArrFingerprints {
        let mut out = BTreeMap::new();
        let medias: BTreeSet<(removarr_domain::arr::ArrService, i64)> = plan
            .arr_snapshots
            .iter()
            .map(|r| (r.media.service, r.media.media_id))
            .collect();
        for (service, id) in medias {
            let media = ArrMediaRef {
                service,
                media_id: id,
                episode_ids: vec![],
            };
            let r = match self.arr_for(&media) {
                Some(a) => a.quality_snapshot(&media).await.map(|s| s.fingerprint),
                None => Err(PortError::new(ErrorClass::Config, "service not configured")),
            };
            out.insert((service, id), r);
        }
        out
    }

    /// Compares a plan with the current stored state and fresh ARR fingerprints; a stale
    /// `Draft`/`Approved` plan moves to `Expired` or `Invalidated` (it never adapts).
    pub async fn refresh_status(
        &self,
        id: PlanId,
        arr_obs: Vec<ArrObservation>,
    ) -> Result<PlanCheck, PlanError> {
        let stored = self.get(id).await?;
        let reasons = match self.load(&[], arr_obs).await {
            Ok(current) => {
                let fps = self.arr_fingerprints(stored.plan.plan()).await;
                staleness(
                    stored.plan.plan(),
                    self.ports.clock.now(),
                    &current,
                    &self.settings,
                    &fps,
                )
            }
            Err(PlanError::SessionNotPlannable(_) | PlanError::NoSession) => {
                vec![StaleReason::SessionNotPlannable]
            }
            Err(e) => return Err(e),
        };
        let mut status = stored.status;
        if matches!(status, PlanStatus::Draft | PlanStatus::Approved)
            && let Some(next) = verdict(&reasons)
        {
            self.ports
                .plans
                .transition(id, status, next)
                .await
                .map_err(port)?;
            status = next;
        }
        Ok(PlanCheck { status, reasons })
    }

    /// Manual approval (architecture 7.3): the hash, the typed counts and every per-file
    /// acknowledgement must match, the plan must be a `Draft` that is neither expired nor stale.
    /// Approval never enqueues execution.
    pub async fn approve(
        &self,
        id: PlanId,
        req: &ApprovalRequest,
        arr_obs: Vec<ArrObservation>,
    ) -> Result<Approval, PlanError> {
        let check = self.refresh_status(id, arr_obs).await?;
        if check.status != PlanStatus::Draft {
            return Err(if check.reasons.is_empty() {
                PlanError::Domain(DomainError::InvalidTransition {
                    entity: "plan",
                    from: format!("{:?}", check.status),
                    to: "Approved".into(),
                })
            } else {
                PlanError::Stale {
                    status: check.status,
                    reasons: check.reasons,
                }
            });
        }
        let stored = self.get(id).await?;
        let approval = stored.plan.approve(req, self.ports.clock.now())?;
        self.ports.plans.approve(&approval).await.map_err(port)?;
        Ok(approval)
    }

    /// Discards a `Draft` or `Approved` plan.
    pub async fn discard(&self, id: PlanId) -> Result<(), PlanError> {
        let stored = self.get(id).await?;
        self.ports
            .plans
            .transition(id, stored.status, PlanStatus::Discarded)
            .await
            .map_err(port)
    }

    /// The execution contract for the action runner. A simulation accepts any `Draft` or
    /// `Approved` plan; a live execution needs an `Approved` live plan whose approval names this
    /// exact hash, before its expiry.
    pub async fn ticket(&self, id: PlanId, simulate: bool) -> Result<ExecutionTicket, PlanError> {
        let stored = self.get(id).await?;
        let now = self.ports.clock.now();
        let wrong = |s: PlanStatus| {
            PlanError::Domain(DomainError::InvalidTransition {
                entity: "plan",
                from: format!("{s:?}"),
                to: "Executing".into(),
            })
        };
        if simulate {
            if !matches!(stored.status, PlanStatus::Draft | PlanStatus::Approved) {
                return Err(wrong(stored.status));
            }
        } else {
            let plan = stored.plan.plan();
            if stored.status != PlanStatus::Approved {
                return Err(wrong(stored.status));
            }
            if plan.mode != PlanMode::Live || self.settings.mode != PlanMode::Live {
                return Err(PlanError::Port(PortError::new(
                    ErrorClass::CapabilityDisabled,
                    "a dry-run plan or process can only simulate",
                )));
            }
            if plan.is_expired(now) {
                self.ports
                    .plans
                    .transition(id, stored.status, PlanStatus::Expired)
                    .await
                    .map_err(port)?;
                return Err(PlanError::Domain(DomainError::PlanExpired {
                    expires_at: plan.expires_at,
                    now,
                }));
            }
            match &stored.approval {
                Some(a) if &a.plan_hash == stored.plan.hash() && a.plan_id == plan.id => {}
                _ => {
                    return Err(PlanError::Domain(DomainError::PlanHashMismatch {
                        expected: stored.plan.hash().0.clone(),
                        actual: stored
                            .approval
                            .as_ref()
                            .map(|a| a.plan_hash.0.clone())
                            .unwrap_or_default(),
                    }));
                }
            }
        }
        Ok(ExecutionTicket {
            plan: stored.plan,
            status: stored.status,
            approval: stored.approval,
            simulate,
        })
    }
}
