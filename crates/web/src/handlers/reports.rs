//! History, statistics, logs and exports. Read-only: SQLite (config SSD) and the in-memory log
//! ring; exports are rendered in memory and streamed in the response (nothing written to disk).

use askama::Template;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use removarr_reporting::export::{self, ExportFormat};
use removarr_reporting::{
    DashboardInputs, HistoryQuery, HistoryVm, LogQuery, LogsVm, StatsQuery, StatsVm,
};

use super::{Ctx, bytes, render, signed_bytes};
use crate::error::WebError;
use crate::forms::{self, LogsQueryRaw, PageQueryRaw};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct HistoryView {
    pub vm: HistoryVm,
    pub page: u64,
    pub prev_url: Option<String>,
    pub next_url: Option<String>,
}

#[derive(Debug, Template)]
#[template(path = "pages/history.html")]
pub struct HistoryPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a HistoryView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/history.html")]
pub struct HistoryFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a HistoryView,
}

/// `GET /history`.
pub async fn history(
    ctx: Ctx,
    State(s): State<AppState>,
    query: Result<Query<PageQueryRaw>, QueryRejection>,
) -> Result<Response, WebError> {
    let Query(raw) = query.map_err(|_| WebError::invalid("query", "malformed query"))?;
    let (offset, limit) = raw.validate()?;
    let vm = s
        .read
        .history(&HistoryQuery {
            offset,
            limit,
            ..HistoryQuery::default()
        })
        .await?;
    let page = offset / limit + 1;
    let v = HistoryView {
        prev_url: (page > 1).then(|| format!("/history?page={}&per_page={limit}", page - 1)),
        next_url: vm
            .has_more
            .then(|| format!("/history?page={}&per_page={limit}", page + 1)),
        page,
        vm,
    };
    Ok(render(
        &ctx,
        &HistoryPage { ctx: &ctx, v: &v },
        &HistoryFragment { ctx: &ctx, v: &v },
    ))
}

#[derive(Debug, Clone)]
pub struct SeriesRow {
    pub label: &'static str,
    pub total: String,
    pub guaranteed: bool,
    /// `(day, bytes, cumulative)` for the accessible table (web-ui draws the SVG).
    pub points: Vec<(String, String, String)>,
}

#[derive(Debug, Clone)]
pub struct StatsView {
    pub vm: StatsVm,
    pub series: Vec<SeriesRow>,
    pub attributable: String,
    pub added: String,
    pub net: String,
}

#[derive(Debug, Template)]
#[template(path = "pages/stats.html")]
pub struct StatsPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a StatsView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/stats.html")]
pub struct StatsFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a StatsView,
}

/// `GET /stats`: series data (potential, expected, attributable, added, observed).
pub async fn stats(ctx: Ctx, State(s): State<AppState>) -> Result<Response, WebError> {
    let vm = s.read.stats(&StatsQuery::default()).await?;
    let series = vm
        .series
        .iter()
        .map(|x| SeriesRow {
            label: ctx.tr.t(x.label_key),
            total: signed_bytes(x.total),
            guaranteed: x.guaranteed,
            points: x
                .points
                .iter()
                .map(|p| {
                    (
                        p.at_iso.chars().take(10).collect(),
                        signed_bytes(p.bytes),
                        signed_bytes(p.cumulative),
                    )
                })
                .collect(),
        })
        .collect();
    let v = StatsView {
        attributable: bytes(vm.totals.attributable_bytes),
        added: bytes(vm.totals.added_bytes),
        net: signed_bytes(vm.totals.net_bytes),
        series,
        vm,
    };
    Ok(render(
        &ctx,
        &StatsPage { ctx: &ctx, v: &v },
        &StatsFragment { ctx: &ctx, v: &v },
    ))
}

#[derive(Debug, Clone)]
pub struct LogsView {
    pub vm: LogsVm,
    pub level: &'static str,
    pub q: String,
}

#[derive(Debug, Template)]
#[template(path = "pages/logs.html")]
pub struct LogsPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a LogsView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/logs.html")]
pub struct LogsFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a LogsView,
}

/// `GET /logs`: the sanitized in-memory ring (`level`, `q`).
pub async fn logs(
    ctx: Ctx,
    State(s): State<AppState>,
    query: Result<Query<LogsQueryRaw>, QueryRejection>,
) -> Result<Response, WebError> {
    let Query(raw) = query.map_err(|_| WebError::invalid("query", "malformed query"))?;
    let (min_level, q) = raw.validate()?;
    let vm = s.logs.view(&LogQuery {
        min_level,
        q: q.clone(),
        after_seq: None,
        limit: 200,
    });
    let v = LogsView {
        vm,
        level: match min_level {
            removarr_reporting::LogLevel::Debug => "debug",
            removarr_reporting::LogLevel::Info => "info",
            removarr_reporting::LogLevel::Warn => "warn",
            removarr_reporting::LogLevel::Error => "error",
        },
        q: q.unwrap_or_default(),
    };
    Ok(render(
        &ctx,
        &LogsPage { ctx: &ctx, v: &v },
        &LogsFragment { ctx: &ctx, v: &v },
    ))
}

fn export_format(s: &str) -> Option<ExportFormat> {
    Some(match s {
        "json" => ExportFormat::Json,
        "csv" => ExportFormat::CsvIssues,
        "csv-history" => ExportFormat::CsvHistory,
        "csv-series" => ExportFormat::CsvSeries,
        "md" => ExportFormat::Markdown,
        "html" => ExportFormat::Html,
        _ => return None,
    })
}

/// `GET /export/{session|latest}/{json|csv|csv-history|csv-series|md|html}`.
pub async fn export(
    _ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, WebError> {
    let Path((raw_session, raw_format)) = path.map_err(|_| WebError::not_found())?;
    let session = forms::session(&raw_session).map_err(|_| WebError::not_found())?;
    let format = export_format(&raw_format).ok_or_else(WebError::not_found)?;
    let status = s.jobs.status();
    let inputs = DashboardInputs {
        scan_interval_minutes: s.settings.scan_interval_minutes,
        auto_enabled: status.auto_enabled,
        dry_run: s.settings.dry_run,
        recovery_pending: false,
        torrents_left_stopped: 0,
        live_services: None,
    };
    let bundle = s.read.report(session, &inputs).await?;
    let file = export::render(&bundle, format)?;
    let mut r = file.bytes.into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(file.content_type),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", file.file_name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(r)
}
