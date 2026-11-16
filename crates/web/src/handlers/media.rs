//! Media-level release search and the manual grab request (CP1-F06, MASTER section 6).
//!
//! A search is read only: it never grabs and the page shows opaque release tokens (never an
//! indexer guid or URL). "Plan this release" only builds a `PlanRequest::Release` plan through
//! the normal plan, confirm, approve and execute flow; the planner re-searches and gates it,
//! the runner re-searches again before the single grab. These handlers hold no policy: they
//! only decide whether the form is offered.

use askama::Template;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum_extra::extract::{Form, FormRejection};
use removarr_domain::arr::ReleaseSearchResult;
use removarr_workflows::plans::PlanRequest;

use super::{Cmd, Ctx, bytes, parse_form, render, see_other_flash};
use crate::error::WebError;
use crate::forms::{self, GrabPlanForm, MediaSearchForm};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct ResultRow {
    /// Opaque `rel-<hash>` token: the only release identifier that is ever rendered.
    pub token: String,
    pub indexer_id: i64,
    pub indexer: String,
    pub title: String,
    pub size: String,
    pub seeders: String,
    pub age_days: u32,
    pub protocol: String,
    pub quality: String,
    pub custom_formats: Vec<String>,
    pub cf_score: i64,
    pub approved: bool,
    pub rejections: Vec<String>,
    /// Display hint only; the planner decides.
    pub grabbable: bool,
}

#[derive(Debug, Clone)]
pub struct MediaView {
    pub service: &'static str,
    pub media_id: i64,
    pub episode_id: String,
    pub searched: bool,
    pub results: Vec<ResultRow>,
    /// Whether the "plan this release" form is offered, and why not (i18n key) when it is not.
    pub grab_enabled: bool,
    pub grab_note: &'static str,
}

#[derive(Debug, Template)]
#[template(path = "pages/media.html")]
pub struct MediaPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a MediaView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/media.html")]
pub struct MediaFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a MediaView,
}

fn row(r: &ReleaseSearchResult) -> ResultRow {
    ResultRow {
        token: r.release.guid.clone(),
        indexer_id: r.release.indexer_id,
        indexer: r.indexer.clone(),
        title: r.title.clone(),
        size: bytes(r.size),
        seeders: r.seeders.map_or_else(|| "-".to_string(), |n| n.to_string()),
        age_days: r.age_days,
        protocol: r.protocol.clone(),
        quality: r.quality.clone(),
        custom_formats: r.custom_formats.clone(),
        cf_score: r.cf_score,
        approved: r.approved,
        rejections: r.rejections.clone(),
        grabbable: r.approved && r.rejections.is_empty() && r.download_allowed,
    }
}

fn view(
    s: &AppState,
    media: &removarr_domain::arr::ArrMediaRef,
    results: Option<&[ReleaseSearchResult]>,
) -> MediaView {
    let (grab_enabled, grab_note) = if s.settings.dry_run {
        (false, "media.grab_off_dry_run")
    } else if !s.settings.arr_grab {
        (false, "media.grab_off_capability")
    } else {
        (true, "")
    };
    MediaView {
        service: media.service.as_str(),
        media_id: media.media_id,
        episode_id: media
            .episode_ids
            .first()
            .map(ToString::to_string)
            .unwrap_or_default(),
        searched: results.is_some(),
        results: results.unwrap_or_default().iter().map(row).collect(),
        grab_enabled,
        grab_note,
    }
}

fn reply(ctx: &Ctx, v: &MediaView) -> Response {
    render(ctx, &MediaPage { ctx, v }, &MediaFragment { ctx, v })
}

type Ids = Result<Path<(String, String)>, PathRejection>;

fn ids(p: Ids) -> Result<(String, String), WebError> {
    p.map(|Path(v)| v).map_err(|_| WebError::not_found())
}

/// `GET /media/{service}/{id}`: the search entry. Read only; it does not search.
pub async fn page(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Ids,
    query: Result<Query<MediaSearchForm>, QueryRejection>,
) -> Result<Response, WebError> {
    let (svc, id) = ids(path)?;
    let Query(q) = query.map_err(|_| WebError::invalid("query", "malformed query"))?;
    let media = forms::arr_media(&svc, &id, q.episode_id.as_deref())?;
    Ok(reply(&ctx, &view(&s, &media, None)))
}

/// `POST /media/{service}/{id}/search`: interactive search (read only, never grabs). A command
/// route so a prefetch or a crawler cannot query the indexers.
pub async fn search(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Ids,
    f: Result<Form<MediaSearchForm>, FormRejection>,
) -> Result<Response, WebError> {
    let (svc, id) = ids(path)?;
    let form = parse_form(f)?;
    let media = forms::arr_media(&svc, &id, form.episode_id.as_deref())?;
    let found = s.media.search(&media).await?;
    Ok(reply(&ctx, &view(&s, &media, Some(&found))))
}

/// `POST /media/{service}/{id}/grab-plan`: builds and stores a draft grab plan for one release
/// token, then shows its confirmation page. Nothing is grabbed.
pub async fn plan(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    path: Ids,
    f: Result<Form<GrabPlanForm>, FormRejection>,
) -> Result<Response, WebError> {
    let (svc, id) = ids(path)?;
    let form = parse_form(f)?;
    let media = forms::arr_media(&svc, &id, form.episode_id.as_deref())?;
    let release = form.validate()?;
    let built = s
        .plans
        .create(&PlanRequest::Release { media, release })
        .await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/plans/{}/confirm", built.id),
        "plan_created",
    ))
}
