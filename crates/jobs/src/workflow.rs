//! Production [`JobHandler`] and [`CycleEngine`] over `removarr-workflows`: scan, refresh,
//! evaluate, verify, execute, recover and the automatic cycle.
//!
//! It keeps the last scan's classification and service observations in memory, because no port
//! persists the ARR library, queue and history (handoff 11). After a restart the automatic
//! cycle therefore waits for the next scheduled scan.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::{IssueId, PlanId, SessionId, SubjectKey};
use removarr_domain::jobs::JobKind;
use removarr_domain::path::RootId;
use removarr_domain::plan::PlanMode;
use removarr_domain::ports::{CancelFlag, ScanControl};
use removarr_domain::session::{ServiceKind, SessionStatus};
use removarr_workflows::actions::{
    self, ActionPorts, ActionSettings, EventSink, ExecutionReport, Mutators, Reporter,
};
use removarr_workflows::evaluate::{VerifyPorts, evaluate_issues, evaluate_media, verify_content};
use removarr_workflows::observe::{
    ObserveConfig, ObserveError, ObservePorts, RefreshTarget, ScanReport, ServiceCache, refresh,
    run_scan,
};
use removarr_workflows::plans::{
    AutoContext, AutoPhase, PlanError, Planner, action_paths, action_subjects,
};

use crate::auto::{
    ActionFacts, ActionHistory, CycleDeps, CycleEngine, PendingAutoPlans, PhaseError, PhasePlan,
    PhasePlans, Refusal, RescanSummary, run_cycle,
};
use crate::handler::{JobCtx, JobError, JobHandler, JobSuccess, RecoverySummary, ScanSummary};
use crate::locks::Scope;

/// Everything the production handler needs, wired by the application.
pub struct WorkflowDeps {
    pub observe: ObservePorts,
    pub observe_cfg: ObserveConfig,
    pub planner: Planner,
    pub actions: ActionPorts,
    pub mutators: Mutators,
    pub action_settings: ActionSettings,
    pub verify: VerifyPorts,
    pub history: Arc<dyn ActionHistory>,
    pub pending: Arc<dyn PendingAutoPlans>,
}

impl std::fmt::Debug for WorkflowDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowDeps").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Memory {
    cache: ServiceCache,
    session: Option<SessionId>,
    issue_ids: Vec<IssueId>,
    /// Perimeter blockers of the last scan or rescan; a "nothing plannable" phase is harmless
    /// only when this is empty (F05).
    uncertainties: Vec<String>,
}

pub struct WorkflowHandler {
    d: WorkflowDeps,
    memory: tokio::sync::Mutex<Memory>,
}

impl std::fmt::Debug for WorkflowHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowHandler").finish_non_exhaustive()
    }
}

fn observe_err(e: ObserveError) -> JobError {
    match e {
        ObserveError::Port(p) => p.into(),
        ObserveError::NoSession => {
            JobError::failed(ErrorClass::NotFound, "no scan session exists yet")
        }
        ObserveError::Classify(c) => JobError::failed(ErrorClass::Conflict, c.to_string()),
    }
}

fn plan_err(e: &PlanError) -> JobError {
    JobError::failed(e.class(), e.to_string())
}

fn port_err(e: &PlanError) -> PortError {
    PortError::new(e.class(), e.to_string())
}

impl WorkflowHandler {
    #[must_use]
    pub fn new(deps: WorkflowDeps) -> Self {
        Self {
            d: deps,
            memory: tokio::sync::Mutex::new(Memory::default()),
        }
    }

    fn scan_control(&self) -> ScanControl {
        ScanControl::default()
    }

    async fn remember(&self, report: &ScanReport) {
        let mut m = self.memory.lock().await;
        m.session = Some(report.session.id);
        m.issue_ids = report
            .classified
            .as_ref()
            .map(|c| c.active().map(|i| i.id.clone()).collect())
            .unwrap_or_default();
        m.cache = report.services.clone();
        m.uncertainties = self.summary(report).uncertainties;
    }

    fn summary(&self, report: &ScanReport) -> ScanSummary {
        let complete = report.session.status == SessionStatus::Complete;
        let uncertainties = match &report.classified {
            Some(c) => c
                .perimeter
                .auto_blockers(&self.d.observe_cfg.roots)
                .iter()
                .map(|b| format!("{:?}: {}", b.code, b.detail))
                .collect(),
            None => vec!["the scan did not finish".to_string()],
        };
        ScanSummary {
            session: report.session.id,
            complete,
            uncertainties,
        }
    }

    /// Scan with a memory-only progress ticker (one read of an atomic every 2 s).
    async fn scan(&self, ctx: &JobCtx) -> Result<ScanReport, JobError> {
        let ctl = ScanControl {
            cancel: ctx.cancel.clone(),
            ..self.scan_control()
        };
        let (counter, progress): (Arc<AtomicU64>, _) =
            (Arc::clone(&ctl.progress), ctx.progress.clone());
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                progress.counts(counter.load(Ordering::Relaxed), 0, None);
            }
        });
        let report = run_scan(&self.d.observe, &self.d.observe_cfg, &ctl).await;
        ticker.abort();
        let report = report.map_err(observe_err)?;
        match report.session.status {
            SessionStatus::Cancelled | SessionStatus::Interrupted => Err(JobError::Cancelled),
            SessionStatus::Failed => Err(JobError::failed(ErrorClass::Internal, "the scan failed")),
            _ => Ok(report),
        }
    }

    async fn execute_plan(
        &self,
        plan: PlanId,
        simulate: bool,
        auto_active: bool,
        cancel: &CancelFlag,
        sink: EventSink,
    ) -> Result<ExecutionReport, PortError> {
        // F06: only a cycle run, while automatic mode is enabled and not paused, may execute an
        // automatic plan live; every user job runs with the flag off.
        let settings = ActionSettings {
            auto_mode_active: auto_active,
            ..self.d.action_settings.clone()
        };
        let ticket = self
            .d
            .planner
            .ticket(plan, simulate)
            .await
            .map_err(|e| port_err(&e))?;
        let reporter = Reporter::new().with_sink(sink);
        actions::execute(
            &self.d.actions,
            &self.d.mutators,
            &settings,
            ticket,
            cancel,
            &reporter,
        )
        .await
    }

    async fn recover_inner(&self, sink: EventSink) -> Result<RecoverySummary, PortError> {
        let reporter = Reporter::new().with_sink(sink);
        let report = actions::recover(
            &self.d.actions,
            &self.d.mutators,
            &self.d.action_settings,
            &reporter,
        )
        .await?;
        Ok(RecoverySummary {
            plans: u32::try_from(report.plans.len()).unwrap_or(u32::MAX),
            left_stopped: u32::try_from(
                report
                    .plans
                    .iter()
                    .map(|p| p.left_stopped.len())
                    .sum::<usize>(),
            )
            .unwrap_or(u32::MAX),
            notes: report.plans.iter().flat_map(|p| p.notes.clone()).collect(),
        })
    }
}

#[async_trait]
impl JobHandler for WorkflowHandler {
    async fn scope(&self, kind: &JobKind) -> Result<Scope, JobError> {
        Ok(match kind {
            JobKind::Scan | JobKind::EvaluateIssues { .. } | JobKind::VerifyContent { .. } => {
                Scope::shared_roots(self.d.observe_cfg.roots.iter())
            }
            JobKind::ExecutePlan { plan_id, simulate } => {
                let stored = self
                    .d
                    .planner
                    .get(*plan_id)
                    .await
                    .map_err(|e| plan_err(&e))?;
                Scope::for_plan(stored.plan.plan(), *simulate)
            }
            // Refresh reads APIs only; the automatic cycle locks per plan; media evaluation is HTTP.
            _ => Scope::default(),
        })
    }

    async fn run(&self, kind: &JobKind, ctx: &JobCtx) -> Result<JobSuccess, JobError> {
        match kind {
            JobKind::Scan => {
                ctx.progress.stage("scanning roots and services").await;
                let report = self.scan(ctx).await?;
                self.remember(&report).await;
                let m = &report.session.metrics;
                let mut ok = JobSuccess::default()
                    .with_metric("entries_scanned", m.entries_scanned)
                    .with_metric("scan_errors", m.scan_errors);
                ok.scan = Some(self.summary(&report));
                Ok(ok)
            }
            JobKind::RefreshService(service) => {
                ctx.progress
                    .stage("refreshing from the API (no disk access)")
                    .await;
                let target = match service {
                    ServiceKind::Qbittorrent => RefreshTarget::Qbittorrent,
                    ServiceKind::Sonarr | ServiceKind::Radarr => RefreshTarget::Arr,
                    ServiceKind::Jellyfin => RefreshTarget::Jellyfin,
                };
                let mut m = self.memory.lock().await;
                let classified =
                    refresh(&self.d.observe, &self.d.observe_cfg, target, &mut m.cache)
                        .await
                        .map_err(observe_err)?;
                m.issue_ids = classified.active().map(|i| i.id.clone()).collect();
                Ok(JobSuccess::default().with_metric("issues", m.issue_ids.len() as u64))
            }
            JobKind::EvaluateIssues { issue_ids } => {
                ctx.progress.stage("evaluating with ARR").await;
                let arr = self
                    .memory
                    .lock()
                    .await
                    .cache
                    .arr
                    .clone()
                    .unwrap_or_default();
                let r = evaluate_issues(&self.d.planner, issue_ids, arr).await?;
                Ok(JobSuccess::default()
                    .with_metric("evaluated", r.evaluated.len() as u64)
                    .with_metric("skipped", r.skipped.len() as u64))
            }
            JobKind::EvaluateMedia { media, search } => {
                ctx.progress.stage("reading ARR quality data").await;
                let arr = self
                    .d
                    .observe
                    .arr
                    .iter()
                    .find(|a| a.service() == media.service)
                    .ok_or_else(|| {
                        JobError::failed(ErrorClass::Config, "that ARR service is not configured")
                    })?;
                let e = evaluate_media(&**arr, media, *search).await?;
                Ok(JobSuccess::default()
                    .with_metric("episodes", e.episodes.len() as u64)
                    .with_metric("search_results", e.search.map_or(0, |s| s.len() as u64)))
            }
            JobKind::VerifyContent { issue_ids } => {
                ctx.progress.stage("comparing content").await;
                let ctl = ScanControl {
                    cancel: ctx.cancel.clone(),
                    ..self.scan_control()
                };
                let r = verify_content(
                    &self.d.verify,
                    &self.d.action_settings.verifier,
                    issue_ids,
                    &ctl,
                )
                .await?;
                ctx.checkpoint()?;
                Ok(JobSuccess::default()
                    .with_metric("identical", u64::from(r.identical))
                    .with_metric("differs", u64::from(r.differs))
                    .with_metric("inconclusive", u64::from(r.inconclusive)))
            }
            JobKind::ExecutePlan { plan_id, simulate } => {
                ctx.progress
                    .stage(if *simulate {
                        "simulating the plan"
                    } else {
                        "executing the plan"
                    })
                    .await;
                let report = self
                    .execute_plan(
                        *plan_id,
                        *simulate,
                        false,
                        &ctx.cancel,
                        ctx.progress.action_sink(),
                    )
                    .await?;
                for a in report.actions.iter().filter(|a| !a.outcome.is_success()) {
                    ctx.progress.warn(&format!(
                        "action {} ({:?}): {:?}",
                        a.index, a.kind, a.outcome
                    ));
                }
                if report.interrupted {
                    return Err(JobError::Cancelled);
                }
                Ok(JobSuccess::default()
                    .with_metric("verified", report.verified() as u64)
                    .with_metric("actions", report.actions.len() as u64)
                    .with_metric("left_stopped", report.left_stopped.len() as u64))
            }
            JobKind::AutoCycle { session } => {
                let deps = CycleDeps {
                    engine: self,
                    history: &*self.d.history,
                    ctx,
                };
                run_cycle(&deps, *session).await
            }
            JobKind::Recover => Err(JobError::failed(
                ErrorClass::Validation,
                "recovery is run by the runtime",
            )),
        }
    }

    async fn discard_pending_auto(&self) -> Result<u32, PortError> {
        let mut n = 0;
        for id in self.d.pending.approved_automatic().await? {
            self.d.planner.discard(id).await.map_err(|e| port_err(&e))?;
            n += 1;
        }
        Ok(n)
    }

    async fn recover(&self, sink: EventSink) -> Result<RecoverySummary, PortError> {
        self.recover_inner(sink).await
    }
}

#[async_trait]
impl CycleEngine for WorkflowHandler {
    fn simulate(&self) -> bool {
        self.d.mutators.mode == PlanMode::DryRun
    }

    async fn plan_phase(
        &self,
        phase: AutoPhase,
        ctx: &AutoContext,
        session: SessionId,
    ) -> Result<PhasePlans, PhaseError> {
        let (ids, arr, uncertainties) = {
            let m = self.memory.lock().await;
            if m.session != Some(session) {
                return Err(PhaseError::Failed(
                    "no in-memory scan result for this session".into(),
                ));
            }
            (
                m.issue_ids.clone(),
                m.cache.arr.clone().unwrap_or_default(),
                m.uncertainties.clone(),
            )
        };
        let out = self
            .d
            .planner
            .create_auto(phase, ctx, &ids, arr)
            .await
            .map_err(|e| match e {
                PlanError::AutoRefused(m) => PhaseError::Refused(m),
                e => PhaseError::Failed(e.to_string()),
            })?;
        let plans = out
            .plans
            .iter()
            .map(|ap| {
                let plan = ap.built.plan.plan();
                PhasePlan {
                    plan: plan.id,
                    live_scope: Scope::for_plan(plan, false),
                    read_scope: Scope::for_plan(plan, true),
                    actions: plan
                        .actions
                        .iter()
                        .map(|a| ActionFacts {
                            index: a.index,
                            kind: a.kind,
                            paths: action_paths(a),
                            subjects: action_subjects(a),
                        })
                        .collect(),
                }
            })
            .collect();
        // The planner reports a normal policy exclusion as "<n> item(s) excluded"; any other
        // text is a defect or an approval error.
        // The text carries no blocker codes, so "nothing plannable" is harmless only while the
        // last observation has no perimeter uncertainty (F05); an unknown text always pauses.
        let refused = out
            .refused
            .into_iter()
            .map(|(_, reason)| Refusal {
                benign: reason.ends_with("item(s) excluded") && uncertainties.is_empty(),
                reason,
                blockers: Vec::new(),
            })
            .collect();
        Ok(PhasePlans { plans, refused })
    }

    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        self.d.observe.ignores.ignored().await
    }

    async fn discard(&self, plan: PlanId) -> Result<(), PortError> {
        self.d.planner.discard(plan).await.map_err(|e| port_err(&e))
    }

    async fn discard_pending(&self) -> Result<u32, PortError> {
        self.discard_pending_auto().await
    }

    async fn recover(&self, sink: EventSink) -> Result<RecoverySummary, PortError> {
        self.recover_inner(sink).await
    }

    async fn execute(
        &self,
        plan: PlanId,
        simulate: bool,
        auto_active: bool,
        cancel: &CancelFlag,
        sink: EventSink,
    ) -> Result<ExecutionReport, PortError> {
        self.execute_plan(plan, simulate, auto_active, cancel, sink)
            .await
    }

    /// Deviation: `run_scan` has no root restriction (architecture 7.6 names
    /// `observe::run(roots = Some(R))`), so the rescan observes every configured root. The
    /// cycle itself computes R and only rescans when R is not empty.
    async fn rescan(
        &self,
        _roots: &BTreeSet<RootId>,
        cancel: &CancelFlag,
    ) -> Result<RescanSummary, PortError> {
        let ctl = ScanControl {
            cancel: cancel.clone(),
            ..self.scan_control()
        };
        let report = run_scan(&self.d.observe, &self.d.observe_cfg, &ctl)
            .await
            .map_err(|e| match observe_err(e) {
                JobError::Failed { class, message } => PortError::new(class, message),
                JobError::Cancelled => PortError::new(ErrorClass::Cancelled, "cancelled"),
            })?;
        match report.session.status {
            SessionStatus::Cancelled | SessionStatus::Interrupted => {
                return Err(PortError::new(
                    ErrorClass::Cancelled,
                    "the rescan was cancelled",
                ));
            }
            SessionStatus::Failed => {
                return Err(PortError::new(ErrorClass::Internal, "the rescan failed"));
            }
            _ => {}
        }
        self.remember(&report).await;
        let s = self.summary(&report);
        Ok(RescanSummary {
            session: s.session,
            complete: s.complete,
            uncertainties: s.uncertainties,
        })
    }
}
