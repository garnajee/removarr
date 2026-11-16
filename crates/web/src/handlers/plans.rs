//! Plans: preview, create, view, confirmation, explicit approval, simulation, execution and
//! discard (architecture 7.3, 12.1). Approval never executes; execution is a queued job that
//! the action runner revalidates. Every rule lives in the plan engine and the runner: these
//! handlers only translate forms into requests.

use askama::Template;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use axum_extra::extract::{Form, FormRejection};
use removarr_domain::ids::PlanId;
use removarr_domain::jobs::JobKind;
use removarr_domain::plan::{ActionTarget, PlanMode, PlanStatus};
use removarr_reporting::PlanReportVm;
use removarr_workflows::plans::{PlanPreview, PlanRequest};

use super::{Cmd, Ctx, bytes, iso, parse_form, render, see_other_flash, signed_bytes};
use crate::error::WebError;
use crate::forms::{self, ApproveForm, PlanForm};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct ActionRow {
    pub index: u32,
    pub kind: String,
    pub tier: String,
    pub irreversible: bool,
    pub companion: bool,
    pub target: String,
    pub annexes: Vec<String>,
    pub torrents: usize,
    pub logical: String,
    pub attributable: String,
}

#[derive(Debug, Clone)]
pub struct ExcludedRow {
    pub subject: String,
    pub blockers: Vec<String>,
}

/// Plan preview and confirmation view (architecture 12.2 `PlanPreviewVm`, `ConfirmVm`).
#[derive(Debug, Clone)]
pub struct PlanView {
    pub id: String,
    pub hash: String,
    /// False for a preview that was not stored (no approval possible).
    pub stored: bool,
    pub status: String,
    pub mode_dry_run: bool,
    pub origin: String,
    pub created_at: String,
    pub expires_at: String,
    pub actions: Vec<ActionRow>,
    pub excluded: Vec<ExcludedRow>,
    pub warnings: Vec<String>,
    pub logical: String,
    pub attributable: String,
    pub added: String,
    pub net: String,
    pub required_count: u32,
    pub required_torrent_removals: u32,
    pub torrent_removals: Vec<String>,
    pub samples: Vec<String>,
    pub unrecoverable: Vec<String>,
    pub irreversible: bool,
    pub max_tier: String,
    pub can_approve: bool,
    pub can_simulate: bool,
    pub can_execute: bool,
    pub can_discard: bool,
    /// Gains recorded after execution (attributable, added) and the observed free-space deltas
    /// per root: kept apart from the expected values above.
    pub recorded_attributable: String,
    pub recorded_added: String,
    pub observed: Vec<(String, String)>,
    /// Translated explanations of planned-versus-recorded differences.
    pub differences: Vec<String>,
    pub report: Option<PlanReportVm>,
}

#[derive(Debug, Template)]
#[template(path = "pages/plan.html")]
pub struct PlanPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a PlanView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/plan.html")]
pub struct PlanFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a PlanView,
}

#[derive(Debug, Template)]
#[template(path = "pages/plan_confirm.html")]
pub struct ConfirmPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a PlanView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/plan_confirm.html")]
pub struct ConfirmFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a PlanView,
}

/// The data part of an action target (rooted paths, hashes, ids). The action kind label is
/// translated by the template; no English words are built here.
fn target(t: &ActionTarget) -> String {
    match t {
        ActionTarget::Consolidate { source, target } => format!("{source} -> {target}"),
        ActionTarget::Path { path } | ActionTarget::FixMode { path, .. } => path.to_string(),
        ActionTarget::Torrent { hash, data_paths } => {
            let mut s = hash.as_str().to_string();
            for p in data_paths.iter().take(20) {
                s.push_str(&format!(", {p}"));
            }
            s
        }
        ActionTarget::Replace { request } => request
            .files
            .iter()
            .map(|(p, _)| p.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        ActionTarget::Release { media, .. } | ActionTarget::Monitor { media, .. } => {
            format!("{} #{}", media.service.as_str(), media.media_id)
        }
        ActionTarget::Queue { service, id } => format!("{} #{id}", service.as_str()),
        ActionTarget::Rescan { service, .. } => service.as_str().to_string(),
        ActionTarget::JellyfinRefresh { .. } => "Jellyfin".to_string(),
    }
}

pub(crate) fn plan_view(
    p: &PlanPreview,
    stored: bool,
    process_dry_run: bool,
    report: Option<PlanReportVm>,
) -> PlanView {
    let status = p.status;
    PlanView {
        id: p.id.to_string(),
        hash: p.hash.0.clone(),
        stored,
        status: status.map_or_else(|| "Preview".to_string(), |s| format!("{s:?}")),
        mode_dry_run: p.mode == PlanMode::DryRun,
        origin: format!("{:?}", p.origin),
        created_at: iso(p.created_at.as_millis()),
        expires_at: iso(p.expires_at.as_millis()),
        actions: p
            .actions
            .iter()
            .map(|a| ActionRow {
                index: a.index,
                kind: format!("{:?}", a.kind),
                tier: format!("{:?}", a.tier),
                irreversible: a.irreversible,
                companion: a.companion,
                target: target(&a.target),
                annexes: a.annexes.iter().map(ToString::to_string).collect(),
                torrents: a.torrents.len(),
                logical: bytes(a.expected_gain.logical),
                attributable: bytes(a.expected_gain.attributable),
            })
            .collect(),
        excluded: p
            .excluded
            .iter()
            .map(|(s, bs)| ExcludedRow {
                subject: s.as_str().to_string(),
                blockers: bs.iter().map(|b| format!("{:?}", b.code)).collect(),
            })
            .collect(),
        warnings: p.warnings.iter().map(|b| format!("{:?}", b.code)).collect(),
        logical: bytes(p.gains.logical),
        attributable: bytes(p.gains.attributable),
        added: bytes(p.gains.added),
        net: signed_bytes(p.gains.net),
        required_count: p.required_count,
        required_torrent_removals: p.required_torrent_removals,
        torrent_removals: p
            .torrent_removals
            .iter()
            .map(|h| h.as_str().to_string())
            .collect(),
        samples: p
            .samples_to_acknowledge
            .iter()
            .map(ToString::to_string)
            .collect(),
        unrecoverable: p
            .unrecoverable_to_acknowledge
            .iter()
            .map(ToString::to_string)
            .collect(),
        irreversible: p.irreversible,
        max_tier: format!("{:?}", p.max_tier),
        can_approve: stored && status == Some(PlanStatus::Draft),
        can_simulate: stored && matches!(status, Some(PlanStatus::Draft | PlanStatus::Approved)),
        can_execute: stored
            && status == Some(PlanStatus::Approved)
            && p.mode == PlanMode::Live
            && !process_dry_run,
        can_discard: stored && matches!(status, Some(PlanStatus::Draft | PlanStatus::Approved)),
        recorded_attributable: report
            .as_ref()
            .map_or_else(String::new, |r| bytes(r.recorded.attributable_bytes)),
        recorded_added: report
            .as_ref()
            .map_or_else(String::new, |r| bytes(r.recorded.added_bytes)),
        observed: report
            .as_ref()
            .map(|r| {
                r.observed
                    .iter()
                    .map(|o| (o.root.clone(), signed_bytes(o.delta_local)))
                    .collect()
            })
            .unwrap_or_default(),
        differences: Vec::new(),
        report,
    }
}

fn request(f: &PlanForm) -> Result<PlanRequest, WebError> {
    let i = f.validate()?;
    Ok(PlanRequest::Issues {
        category: i.category,
        issue_ids: i.issue_ids,
        action: i.action,
        remove_torrent_tasks: i.remove_torrent_tasks,
        include_specials: i.include_specials,
    })
}

fn id_of(path: Result<Path<String>, PathRejection>) -> Result<PlanId, WebError> {
    let Path(raw) = path.map_err(|_| WebError::not_found())?;
    forms::plan_id(&raw).map_err(|_| WebError::not_found())
}

/// `POST /plans`: builds and stores a draft plan from an explicit selection (one or many ids,
/// the same request), then shows it.
pub async fn create(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<PlanForm>, FormRejection>,
) -> Result<Response, WebError> {
    let req = request(&parse_form(f)?)?;
    let built = s.plans.create(&req).await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/plans/{}", built.id),
        "plan_created",
    ))
}

/// `POST /plans/preview`: the same plan through the same path, not stored.
pub async fn preview(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<PlanForm>, FormRejection>,
) -> Result<Response, WebError> {
    let req = request(&parse_form(f)?)?;
    let p = s.plans.preview(&req).await?;
    let view = plan_view(&p, false, s.settings.dry_run, None);
    Ok(render(
        &ctx,
        &PlanPage {
            ctx: &ctx,
            v: &view,
        },
        &PlanFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}

/// `GET /plans/{id}`: read-only (no status refresh, no transition).
pub async fn show(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    let p = s.plans.get(id).await?;
    let report = s.read.plan_report(id).await.ok();
    let mut view = plan_view(&p, true, s.settings.dry_run, report);
    if let Some(r) = &view.report {
        view.differences = r.differences.iter().map(|d| ctx.explanation(d)).collect();
    }
    Ok(render(
        &ctx,
        &PlanPage {
            ctx: &ctx,
            v: &view,
        },
        &PlanFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}

/// `GET /plans/{id}/confirm`: the explicit confirmation (target count, space impact,
/// irreversible effects, typed counts, per-file acknowledgements; nothing pre-checked).
pub async fn confirm(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    let p = s.plans.get(id).await?;
    let view = plan_view(&p, true, s.settings.dry_run, None);
    Ok(render(
        &ctx,
        &ConfirmPage {
            ctx: &ctx,
            v: &view,
        },
        &ConfirmFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}

/// `POST /plans/{id}/approve`.
pub async fn approve(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    f: Result<Form<ApproveForm>, FormRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    let req = parse_form(f)?.validate()?;
    s.plans.approve(id, &req).await?;
    Ok(see_other_flash(&ctx, &format!("/plans/{id}"), "approved"))
}

async fn enqueue(
    ctx: &Ctx,
    s: &AppState,
    id: PlanId,
    simulate: bool,
) -> Result<Response, WebError> {
    s.plans.check_executable(id, simulate).await?;
    let job = s
        .jobs
        .submit(JobKind::ExecutePlan {
            plan_id: id,
            simulate,
        })
        .await?;
    Ok(see_other_flash(
        ctx,
        &format!("/jobs/{}", job.id),
        "job_queued",
    ))
}

/// `POST /plans/{id}/execute`: approved, unexpired live plan in a live process only.
pub async fn execute(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    enqueue(&ctx, &s, id, false).await
}

/// `POST /plans/{id}/simulate`: any draft or approved plan, any process; never mutates.
pub async fn simulate(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    enqueue(&ctx, &s, id, true).await
}

/// `POST /plans/{id}/discard`.
pub async fn discard(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let id = id_of(path)?;
    s.plans.discard(id).await?;
    Ok(see_other_flash(&ctx, &format!("/plans/{id}"), "discarded"))
}
