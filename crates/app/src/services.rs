//! Production implementations of the web interfaces (`removarr_web::state`) and of the inputs
//! the job runner and the action runner need from storage (handoffs 13, 14).

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ids::{JobId, PlanId, SessionId};
use removarr_domain::jobs::JobKind;
use removarr_domain::plan::{ApprovalRequest, PlanOrigin, PlanStatus};
use removarr_domain::ports::{ArrRead, Clock, IssueStore, JournalStore, PlanStore, StateStore};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::TorrentHash;
use removarr_jobs::{
    ActionHistory, CancelResult, Enqueued, ExecutedAction, JobProgress, JobRuntime,
    PendingAutoPlans, RuntimeStatus,
};
use removarr_reporting::{
    DashboardInputs, DashboardVm, HistoryQuery, HistoryVm, IssueListVm, IssueQuery, PlanReportVm,
    ReportBundle, Reporter, StatsQuery, StatsVm,
};
use removarr_storage::{JournalRepo, PlanRepo, StateRepo};
use removarr_web::error::{job_failure, plan_failure, port_failure};
use removarr_web::{JobControl, PlanControl, ReadModel, Readiness, UseCaseError};
use removarr_workflows::actions::ExecutingPlans;
use removarr_workflows::issues::ArrObservation;
use removarr_workflows::observe::read_arr;
use removarr_workflows::plans::{PlanPreview, PlanRequest, Planner, preview};

// ---- read model --------------------------------------------------------------------------------

/// `ReadModel` over the reporting read pool.
#[derive(Debug, Clone)]
pub struct ReporterModel(pub Reporter);

#[async_trait]
impl ReadModel for ReporterModel {
    async fn dashboard(&self, i: &DashboardInputs) -> Result<DashboardVm, PortError> {
        self.0.dashboard(i).await
    }
    async fn issues(&self, q: &IssueQuery) -> Result<IssueListVm, PortError> {
        self.0.issues(q).await
    }
    async fn history(&self, q: &HistoryQuery) -> Result<HistoryVm, PortError> {
        self.0.history(q).await
    }
    async fn stats(&self, q: &StatsQuery) -> Result<StatsVm, PortError> {
        self.0.stats(q).await
    }
    async fn plan_report(&self, plan: PlanId) -> Result<PlanReportVm, PortError> {
        self.0.plan_report(plan).await
    }
    async fn report(
        &self,
        session: Option<SessionId>,
        inputs: &DashboardInputs,
    ) -> Result<ReportBundle, PortError> {
        self.0.report(session, inputs).await
    }
}

// ---- jobs --------------------------------------------------------------------------------------

/// `JobControl` over the job runtime. Status and progress read memory only.
#[derive(Debug, Clone)]
pub struct RuntimeJobs(pub Arc<JobRuntime>);

#[async_trait]
impl JobControl for RuntimeJobs {
    async fn submit(&self, kind: JobKind) -> Result<Enqueued, UseCaseError> {
        self.0.submit(kind).await.map_err(|e| job_failure(&e))
    }
    async fn cancel(&self, id: JobId) -> Result<CancelResult, UseCaseError> {
        self.0.cancel(id).await.map_err(|e| port_failure(&e))
    }
    fn retry_recovery(&self) {
        self.0.retry_recovery();
    }
    async fn resume_auto(&self) -> Result<bool, UseCaseError> {
        self.0.resume_auto().await.map_err(|e| port_failure(&e))
    }
    fn status(&self) -> RuntimeStatus {
        self.0.status()
    }
    fn progress(&self, id: JobId) -> Option<JobProgress> {
        self.0.progress(id)
    }
}

// ---- plans -------------------------------------------------------------------------------------

/// `PlanControl` over a [`Planner`].
///
/// The planner needs ARR observations (library, queue, history) to identify media; the job
/// runner keeps its own in memory and exposes none (handoff 14). A user request therefore
/// re-reads ARR through the same read-only observe function, limited to the history of the
/// torrents of the selected issues. Requests other than issue selections, approval and
/// staleness checks do not use them.
pub struct PlannerControl {
    pub planner: Planner,
    pub issues: Arc<dyn IssueStore>,
    pub arr: Vec<Arc<dyn ArrRead>>,
    pub clock: Arc<dyn Clock>,
    pub http_concurrency: usize,
}

impl std::fmt::Debug for PlannerControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannerControl")
            .field("arr", &self.arr.len())
            .finish_non_exhaustive()
    }
}

impl PlannerControl {
    async fn arr_observations(
        &self,
        req: &PlanRequest,
    ) -> Result<Vec<ArrObservation>, UseCaseError> {
        let PlanRequest::Issues { issue_ids, .. } = req else {
            return Ok(Vec::new());
        };
        if self.arr.is_empty() || issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        let issues = self
            .issues
            .get_many(issue_ids)
            .await
            .map_err(|e| port_failure(&e))?;
        let hashes: BTreeSet<TorrentHash> = issues
            .iter()
            .flat_map(|i| i.torrent_hashes.iter().cloned())
            .collect();
        let now = self.clock.now();
        let mut out = Vec::with_capacity(self.arr.len());
        for a in &self.arr {
            out.push(read_arr(a, &hashes, now, self.http_concurrency).await.0);
        }
        Ok(out)
    }
}

#[async_trait]
impl PlanControl for PlannerControl {
    async fn preview(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        let obs = self.arr_observations(req).await?;
        let built = self
            .planner
            .preview(req, obs)
            .await
            .map_err(|e| plan_failure(&e))?;
        Ok(preview(&built.plan, None))
    }

    async fn create(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        let obs = self.arr_observations(req).await?;
        let built = self
            .planner
            .create(req, obs)
            .await
            .map_err(|e| plan_failure(&e))?;
        Ok(preview(&built.plan, Some(PlanStatus::Draft)))
    }

    async fn get(&self, id: PlanId) -> Result<PlanPreview, UseCaseError> {
        let stored = self.planner.get(id).await.map_err(|e| plan_failure(&e))?;
        Ok(preview(&stored.plan, Some(stored.status)))
    }

    async fn approve(&self, id: PlanId, req: &ApprovalRequest) -> Result<(), UseCaseError> {
        // Staleness compares stored state and fresh ARR fingerprints; it reads no observation.
        self.planner
            .approve(id, req, Vec::new())
            .await
            .map(|_| ())
            .map_err(|e| plan_failure(&e))
    }

    async fn discard(&self, id: PlanId) -> Result<(), UseCaseError> {
        self.planner.discard(id).await.map_err(|e| plan_failure(&e))
    }

    async fn check_executable(&self, id: PlanId, simulate: bool) -> Result<(), UseCaseError> {
        self.planner
            .ticket(id, simulate)
            .await
            .map(|_| ())
            .map_err(|e| plan_failure(&e))
    }
}

// ---- readiness ---------------------------------------------------------------------------------

/// One indexed read on the database (reader pool on the config SSD).
#[derive(Debug, Clone)]
pub struct DbReadiness(pub StateRepo);

#[async_trait]
impl Readiness for DbReadiness {
    async fn ready(&self) -> Result<(), PortError> {
        StateStore::get(&self.0, "readiness_probe")
            .await
            .map(|_| ())
    }
}

// ---- job-runner and action-runner inputs -------------------------------------------------------

/// `ActionHistory` over storage (reference implementation of handoff 14): every action of the
/// plans that started executing, keyed on when their outcome was recorded.
#[derive(Debug, Clone)]
pub struct StoreHistory {
    pub plans: PlanRepo,
    pub journal: JournalRepo,
}

#[async_trait]
impl ActionHistory for StoreHistory {
    async fn executed_since(&self, since: Timestamp) -> Result<Vec<ExecutedAction>, PortError> {
        let mut out = Vec::new();
        for status in [
            PlanStatus::Executing,
            PlanStatus::Executed,
            PlanStatus::PartiallyExecuted,
            PlanStatus::Failed,
            PlanStatus::Interrupted,
            PlanStatus::Invalidated,
        ] {
            for id in self.plans.ids_with_status(status).await? {
                let Some((plan, ..)) = PlanStore::get(&self.plans, id).await? else {
                    continue;
                };
                let at = JournalStore::for_plan(&self.journal, id)
                    .await?
                    .iter()
                    .flat_map(|(i, r)| [Some(i.created_at), r.as_ref().map(|r| r.at)])
                    .flatten()
                    .max()
                    .unwrap_or(plan.created_at);
                if at < since {
                    continue;
                }
                let rows = self.plans.actions(id).await?;
                for a in plan.actions {
                    let outcome = rows
                        .iter()
                        .find(|r| r.index == a.index)
                        .and_then(|r| r.outcome.clone());
                    out.push(ExecutedAction {
                        plan: id,
                        at,
                        action: a,
                        outcome,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// `PendingAutoPlans`: approved plans built by the automatic policy.
#[derive(Debug, Clone)]
pub struct PendingAuto(pub PlanRepo);

#[async_trait]
impl PendingAutoPlans for PendingAuto {
    async fn approved_automatic(&self) -> Result<Vec<PlanId>, PortError> {
        let mut out = Vec::new();
        for id in self.0.ids_with_status(PlanStatus::Approved).await? {
            if let Some((plan, ..)) = PlanStore::get(&self.0, id).await?
                && plan.origin == PlanOrigin::AutoPolicy
            {
                out.push(id);
            }
        }
        Ok(out)
    }
}

/// `ExecutingPlans` for recovery.
#[derive(Debug, Clone)]
pub struct Executing(pub PlanRepo);

#[async_trait]
impl ExecutingPlans for Executing {
    async fn executing(&self) -> Result<Vec<PlanId>, PortError> {
        self.0.ids_with_status(PlanStatus::Executing).await
    }
}
