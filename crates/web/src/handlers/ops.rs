//! Operational commands: scan, service refresh and rescan, recovery retry, automatic mode
//! status and resume, locale.
//!
//! A scan or refresh only observes (it never plans or acts). A service rescan is a one-action
//! plan that still needs approval and a live process (architecture 7.3); it never implies any
//! other action.

use askama::Template;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum_extra::extract::{Form, FormRejection};
use removarr_domain::jobs::JobKind;
use removarr_domain::session::ServiceKind;
use removarr_workflows::plans::PlanRequest;

use super::{Cmd, Ctx, iso, parse_form, render, see_other, see_other_flash};
use crate::error::WebError;
use crate::forms::{self, LocaleForm};
use crate::i18n::{LOCALE_COOKIE, Locale};
use crate::state::AppState;

fn configured_service(
    s: &AppState,
    path: Result<Path<String>, PathRejection>,
) -> Result<ServiceKind, WebError> {
    let Path(raw) = path.map_err(|_| WebError::not_found())?;
    let svc = forms::service(&raw).map_err(|_| WebError::not_found())?;
    if !s.settings.configured_services.contains(&svc) {
        return Err(WebError::from_class(
            &removarr_domain::error::ErrorClass::Conflict,
            Some("error.service_not_configured"),
            "service not configured",
        ));
    }
    Ok(svc)
}

/// `POST /scan`: queues a scan (deduplicated with a running one).
pub async fn scan(Cmd(ctx): Cmd, State(s): State<AppState>) -> Result<Response, WebError> {
    let job = s.jobs.submit(JobKind::Scan).await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/jobs/{}", job.id),
        "job_queued",
    ))
}

/// `POST /services/{service}/refresh`: API-only re-read and reclassification (allowed in
/// dry-run; no disk access).
pub async fn refresh(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let svc = configured_service(&s, path)?;
    let job = s.jobs.submit(JobKind::RefreshService(svc)).await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/jobs/{}", job.id),
        "job_queued",
    ))
}

/// `POST /services/{sonarr|radarr|jellyfin}/rescan`: builds a one-action `ServiceRescan` plan
/// (journaled like any plan, executable only live after approval).
pub async fn rescan(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, WebError> {
    let svc = configured_service(&s, path)?;
    if svc == ServiceKind::Qbittorrent {
        return Err(WebError::invalid(
            "service",
            "qBittorrent has no rescan command; use refresh",
        ));
    }
    let plan = s
        .plans
        .create(&PlanRequest::ServiceRescan {
            service: svc,
            media: None,
        })
        .await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/plans/{}", plan.id),
        "plan_created",
    ))
}

/// `POST /recovery/retry`: re-arms recovery after the automatic attempts are exhausted.
pub async fn retry_recovery(Cmd(ctx): Cmd, State(s): State<AppState>) -> Response {
    s.jobs.retry_recovery();
    see_other(&ctx, "/")
}

#[derive(Debug, Clone)]
pub struct AutoView {
    pub enabled: bool,
    pub paused: Option<String>,
    pub dry_run: bool,
    pub next_scan: Option<String>,
}

#[derive(Debug, Template)]
#[template(path = "pages/auto.html")]
pub struct AutoPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a AutoView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/auto.html")]
pub struct AutoFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a AutoView,
}

/// `GET /auto`: automatic mode status (read once at startup; no toggle) and pause reason.
pub async fn auto_status(ctx: Ctx, State(s): State<AppState>) -> Response {
    let st = s.jobs.status();
    let v = AutoView {
        enabled: st.auto_enabled,
        paused: st.auto_paused,
        dry_run: s.settings.dry_run,
        next_scan: st.next_scan.map(|t| iso(t.as_millis())),
    };
    render(
        &ctx,
        &AutoPage { ctx: &ctx, v: &v },
        &AutoFragment { ctx: &ctx, v: &v },
    )
}

/// `POST /auto/resume`: the only way to clear `auto_paused`.
pub async fn resume_auto(Cmd(ctx): Cmd, State(s): State<AppState>) -> Result<Response, WebError> {
    s.jobs.resume_auto().await?;
    Ok(see_other_flash(&ctx, "/auto", "resumed"))
}

/// `POST /locale`: sets the locale cookie (not part of the session).
pub async fn locale(
    Cmd(ctx): Cmd,
    f: Result<Form<LocaleForm>, FormRejection>,
) -> Result<Response, WebError> {
    let f = parse_form(f)?;
    let l = f
        .locale
        .as_deref()
        .and_then(Locale::parse)
        .ok_or_else(|| WebError::invalid("locale", "use en or fr"))?;
    let mut r = see_other(&ctx, "/");
    let cookie = format!(
        "{LOCALE_COOKIE}={}; Path=/; Max-Age=31536000; SameSite=Strict",
        l.as_str()
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    Ok(r)
}
