//! Application state: the explicit interfaces the web layer depends on (architecture 12, task
//! "compose application state through explicit domain and infrastructure interfaces").
//!
//! Handlers only see these traits. Production implementations live in the composition root
//! (`removarr` app crate) over `reporting::Reporter`, `jobs::JobRuntime`, `workflows::plans::Planner`
//! and the storage ports; tests use fakes. Nothing here mutates roots or services: commands
//! become plans or queued jobs.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::Request;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::{JobId, PlanId, SessionId};
use removarr_domain::jobs::JobKind;
use removarr_domain::plan::ApprovalRequest;
use removarr_domain::ports::{Clock, IgnoreStore};
use removarr_domain::session::ServiceKind;
use removarr_jobs::{CancelResult, Enqueued, JobProgress, RuntimeStatus};
use removarr_reporting::{
    DashboardInputs, DashboardVm, HistoryQuery, HistoryVm, IssueListVm, IssueQuery, LogBuffer,
    PlanReportVm, ReportBundle, StatsQuery, StatsVm,
};
use removarr_workflows::plans::{PlanPreview, PlanRequest};

/// A use case refused or failed. `key` is an i18n key shown to the user; `excluded` lists the
/// items a plan request left out (subject keys and blocker codes only); `log_detail` is logged
/// (sanitized, with the correlation id) and **never rendered**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseCaseError {
    pub class: ErrorClass,
    pub key: &'static str,
    pub excluded: Vec<ExcludedItem>,
    pub log_detail: String,
}

impl UseCaseError {
    #[must_use]
    pub fn new(class: ErrorClass, key: &'static str, log_detail: impl Into<String>) -> Self {
        Self {
            class,
            key,
            excluded: Vec::new(),
            log_detail: log_detail.into(),
        }
    }
}

/// One item a plan left out, with its blocker codes (no free text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedItem {
    pub subject: String,
    pub blockers: Vec<String>,
}

/// Read side: the reporting read model (SQLite on the config SSD, never the roots).
#[async_trait]
pub trait ReadModel: Send + Sync {
    async fn dashboard(&self, inputs: &DashboardInputs) -> Result<DashboardVm, PortError>;
    async fn issues(&self, q: &IssueQuery) -> Result<IssueListVm, PortError>;
    async fn history(&self, q: &HistoryQuery) -> Result<HistoryVm, PortError>;
    async fn stats(&self, q: &StatsQuery) -> Result<StatsVm, PortError>;
    async fn plan_report(&self, plan: PlanId) -> Result<PlanReportVm, PortError>;
    async fn report(
        &self,
        session: Option<SessionId>,
        inputs: &DashboardInputs,
    ) -> Result<ReportBundle, PortError>;
}

/// Job commands and memory-only status (job runner, handoff 14).
#[async_trait]
pub trait JobControl: Send + Sync {
    /// Queues a job (scan, refresh, evaluate, verify, execute). Never approves anything.
    async fn submit(&self, kind: JobKind) -> Result<Enqueued, UseCaseError>;
    async fn cancel(&self, id: JobId) -> Result<CancelResult, UseCaseError>;
    fn retry_recovery(&self);
    /// Clears `auto_paused`; `Ok(false)` when it was not paused.
    async fn resume_auto(&self) -> Result<bool, UseCaseError>;
    /// Memory only (no database, no filesystem).
    fn status(&self) -> RuntimeStatus;
    /// Memory only.
    fn progress(&self, id: JobId) -> Option<JobProgress>;
}

/// Plan use cases (plan engine, handoff 12). Unit and bulk selections are the same request.
#[async_trait]
pub trait PlanControl: Send + Sync {
    /// Builds the plan through the real path without storing it.
    async fn preview(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError>;
    /// Builds and stores a `Draft` plan.
    async fn create(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError>;
    /// Read-only view of a stored plan (no status refresh, no transition).
    async fn get(&self, id: PlanId) -> Result<PlanPreview, UseCaseError>;
    /// Explicit approval: hash, typed counts and every acknowledgement must match.
    async fn approve(&self, id: PlanId, req: &ApprovalRequest) -> Result<(), UseCaseError>;
    async fn discard(&self, id: PlanId) -> Result<(), UseCaseError>;
    /// The execution contract (`Planner::ticket`): a live run needs an approved, unexpired live
    /// plan in a live process; a simulation any draft or approved plan.
    async fn check_executable(&self, id: PlanId, simulate: bool) -> Result<(), UseCaseError>;
}

/// Readiness probe: one cheap query on the database (config SSD). Never touches a root.
#[async_trait]
pub trait Readiness: Send + Sync {
    async fn ready(&self) -> Result<(), PortError>;
}

/// What a route needs from the authentication layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// A display route (`GET`): a valid session.
    Page,
    /// A command (`POST`): a valid session, the CSRF token and the same-origin check.
    Command,
}

/// The authenticated user, inserted into the request by the guard middleware. `csrf_token`
/// is rendered as the hidden `csrf_token` field of every form.
#[derive(Clone, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    pub csrf_token: String,
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Principal")
            .field("user", &self.user)
            .field("csrf_token", &"[REDACTED]")
            .finish()
    }
}

/// Why the guard refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// No valid session: pages redirect to `/login`, commands get 401.
    Unauthenticated,
    /// CSRF or origin failure: 403.
    Forbidden,
    RateLimited {
        retry_after_secs: u64,
    },
}

/// The authentication and CSRF extension point implemented by `auth-guard` (17).
///
/// The guard receives the whole request (it may buffer a form body of at most the request
/// limit to read the `csrf_token` field, then rebuild the request) and returns it with the
/// principal. Every non-public route goes through it; command routes additionally require
/// the grant the router adds only after an `Access::Command` approval.
#[async_trait]
pub trait Guard: Send + Sync {
    async fn check(&self, access: Access, req: Request) -> Result<(Request, Principal), Denied>;
}

/// Default guard until `auth-guard` lands: refuses everything (fail closed).
#[derive(Debug, Clone, Copy, Default)]
pub struct FailClosedGuard;

#[async_trait]
impl Guard for FailClosedGuard {
    async fn check(&self, _access: Access, _req: Request) -> Result<(Request, Principal), Denied> {
        Err(Denied::Unauthenticated)
    }
}

/// Values read once at startup (ADR-0003: no UI toggle for `dry_run` or `auto_mode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    pub dry_run: bool,
    pub auto_mode: bool,
    /// `scan.interval_minutes` (dashboard staleness).
    pub scan_interval_minutes: u32,
    /// `thresholds.hide_recent_days` (default of the "hide recent items" filter).
    pub hide_recent_days: u32,
    /// Services with a configured URL; refresh and rescan routes refuse the others.
    pub configured_services: Vec<ServiceKind>,
}

/// Everything a handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub read: Arc<dyn ReadModel>,
    pub jobs: Arc<dyn JobControl>,
    pub plans: Arc<dyn PlanControl>,
    pub ignores: Arc<dyn IgnoreStore>,
    pub readiness: Arc<dyn Readiness>,
    pub logs: LogBuffer,
    pub guard: Arc<dyn Guard>,
    pub clock: Arc<dyn Clock>,
    pub settings: Arc<WebSettings>,
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}
