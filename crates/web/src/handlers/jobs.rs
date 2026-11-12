//! Jobs: list, detail, progress fragment (memory only) and cooperative cancel.

use askama::Template;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use removarr_domain::ids::JobId;
use removarr_domain::jobs::JobOutcome;
use removarr_jobs::{CancelResult, JobProgress};

use super::dashboard::{JobRow, job_row};
use super::{Cmd, Ctx, iso, render, see_other};
use crate::error::WebError;
use crate::forms;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct JobView {
    pub row: JobRow,
    pub lane: String,
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub warnings: Vec<String>,
    pub enqueued_at: String,
    pub started_at: String,
    pub finished_at: String,
    pub cancellable: bool,
    pub cancel_requested: bool,
    pub outcome: String,
    /// Keep polling (`hx-trigger="every 2s"`) only while the job is active.
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct JobsView {
    pub jobs: Vec<JobRow>,
}

#[derive(Debug, Template)]
#[template(path = "pages/jobs.html")]
pub struct JobsPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a JobsView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/jobs.html")]
pub struct JobsFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a JobsView,
}

#[derive(Debug, Template)]
#[template(path = "pages/job.html")]
pub struct JobPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a JobView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/job.html")]
pub struct JobFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a JobView,
}

fn view(p: &JobProgress) -> JobView {
    use removarr_domain::jobs::JobStatus as S;
    let active = matches!(p.status, S::Queued | S::Running);
    JobView {
        row: job_row(p),
        lane: format!("{:?}", p.lane),
        done: p.done,
        total: p.total,
        current: p.current.clone().unwrap_or_default(),
        warnings: p.warnings.clone(),
        enqueued_at: iso(p.enqueued_at.as_millis()),
        started_at: p.started_at.map(|t| iso(t.as_millis())).unwrap_or_default(),
        finished_at: p
            .finished_at
            .map(|t| iso(t.as_millis()))
            .unwrap_or_default(),
        cancellable: active && !p.cancel_requested,
        cancel_requested: p.cancel_requested,
        // The failure message stays in the logs: only the status and class are shown.
        outcome: match &p.outcome {
            Some(JobOutcome::Failed { class, .. }) => format!("Failed ({class:?})"),
            Some(o) => format!("{:?}", o.status()),
            None => String::new(),
        },
        active,
    }
}

fn id_of(path: Result<Path<String>, PathRejection>) -> Result<JobId, WebError> {
    let Path(raw) = path.map_err(|_| WebError::not_found())?;
    forms::job_id(&raw).map_err(|_| WebError::not_found())
}

/// `GET /jobs`: active and recently finished jobs (memory).
pub async fn list(ctx: Ctx, State(s): State<AppState>) -> Result<Response, WebError> {
    let v = JobsView {
        jobs: s.jobs.status().jobs.iter().map(job_row).collect(),
    };
    Ok(render(
        &ctx,
        &JobsPage { ctx: &ctx, v: &v },
        &JobsFragment { ctx: &ctx, v: &v },
    ))
}

/// `GET /jobs/{id}` (memory; jobs older than the in-memory window are not shown).
pub async fn show(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let p = s
        .jobs
        .progress(id_of(path)?)
        .ok_or_else(WebError::not_found)?;
    let v = view(&p);
    Ok(render(
        &ctx,
        &JobPage { ctx: &ctx, v: &v },
        &JobFragment { ctx: &ctx, v: &v },
    ))
}

/// `GET /jobs/{id}/progress`: the progress fragment, always a fragment.
pub async fn progress(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let p = s
        .jobs
        .progress(id_of(path)?)
        .ok_or_else(WebError::not_found)?;
    let v = view(&p);
    let f = JobFragment { ctx: &ctx, v: &v };
    Ok(render(&ctx, &f, &f))
}

/// `POST /jobs/{id}/cancel`: a running job stops at its next safe checkpoint (a mutation step
/// is never dropped).
pub async fn cancel(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    match s.jobs.cancel(id).await? {
        CancelResult::NotFound => Err(WebError::not_found()),
        CancelResult::CancelledQueued | CancelResult::Requested | CancelResult::NotActive => {
            Ok(see_other(&ctx, &format!("/jobs/{id}")))
        }
    }
}
