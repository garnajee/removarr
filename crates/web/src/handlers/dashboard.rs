//! `GET /`: dashboard (architecture 12.2 `DashboardVm`). Reads the read model and the job
//! runner's memory; touches no root.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use removarr_jobs::{JobProgress, RecoveryStatus};
use removarr_reporting::{DashboardInputs, DashboardVm};

use super::{Ctx, bytes, category_key, iso, render, severity_look};
use crate::error::WebError;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct CategoryRow {
    pub slug: &'static str,
    pub label: &'static str,
    pub count: u64,
    pub severity: &'static str,
    pub size: String,
    pub level: &'static str,
    pub icon: &'static str,
    pub severity_key: &'static str,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub label: String,
    pub status: String,
    pub stage: String,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct ServiceRow {
    pub name: String,
    pub state: &'static str,
    pub version: Option<String>,
    pub current: bool,
    /// css class of the state badge and its glyph (never colour alone).
    pub level: &'static str,
    pub icon: &'static str,
    pub observed_at: String,
}

fn service_state_key(k: &str) -> &'static str {
    match k {
        "ok" => "service.ok",
        "unavailable" => "service.unavailable",
        "auth_rejected" => "service.auth_rejected",
        "not_configured" => "service.not_configured",
        "mapping_errors" => "service.mapping_errors",
        _ => "service.unknown",
    }
}

fn reason_key(k: &str) -> &'static str {
    match k {
        "critical_issues" => "reason.critical_issues",
        "warning_issues" => "reason.warning_issues",
        "auto_paused" => "reason.auto_paused",
        "recovery_pending" => "reason.recovery_pending",
        "scan_incomplete" => "reason.scan_incomplete",
        "scan_missing" => "reason.scan_missing",
        "scan_stale" => "reason.scan_stale",
        "service_not_ok" => "reason.service_not_ok",
        "torrents_left_stopped" => "reason.torrents_left_stopped",
        _ => "reason.other",
    }
}

/// Dashboard view: the read model plus formatted values and memory-only runtime facts.
#[derive(Debug, Clone)]
pub struct DashboardView {
    pub vm: DashboardVm,
    pub categories: Vec<CategoryRow>,
    pub reasons: Vec<(&'static str, Option<u64>)>,
    pub services: Vec<ServiceRow>,
    pub potential: String,
    pub shared: String,
    pub logical: String,
    pub allocated: String,
    pub overlap: String,
    /// Configured services that accept a refresh / that accept a rescan plan.
    pub refreshable: Vec<&'static str>,
    pub rescannable: Vec<&'static str>,
    pub last_scan: Option<String>,
    pub jobs: Vec<JobRow>,
    pub auto_paused: Option<String>,
    /// i18n key of the recovery state.
    pub recovery_key: &'static str,
    pub recovery_needs_manual: bool,
    pub next_scan: Option<String>,
    /// css class of the urgency badge: `ok`, `warn` or `crit`.
    pub urgency_class: &'static str,
}

#[derive(Debug, Template)]
#[template(path = "pages/dashboard.html")]
pub struct DashboardPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a DashboardView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/dashboard.html")]
pub struct DashboardFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a DashboardView,
}

fn sev(s: removarr_domain::issue::Severity) -> &'static str {
    match s {
        removarr_domain::issue::Severity::Critical => "critical",
        removarr_domain::issue::Severity::Warning => "warning",
        removarr_domain::issue::Severity::Info => "info",
    }
}

pub(crate) fn job_row(p: &JobProgress) -> JobRow {
    JobRow {
        id: p.id.to_string(),
        label: p.label.clone(),
        status: format!("{:?}", p.status),
        stage: p.stage.clone().unwrap_or_default(),
        done: p.done,
        total: p.total,
    }
}

pub async fn show(ctx: Ctx, State(s): State<AppState>) -> Result<Response, WebError> {
    let status = s.jobs.status();
    let recovery_pending = matches!(
        status.recovery,
        RecoveryStatus::Pending { .. } | RecoveryStatus::NeedsManual
    );
    let inputs = DashboardInputs {
        scan_interval_minutes: s.settings.scan_interval_minutes,
        auto_enabled: status.auto_enabled,
        dry_run: s.settings.dry_run,
        recovery_pending,
        // The runner keeps no persisted left-stopped set (handoff 15 gap); the read model adds
        // unfinished intents itself.
        torrents_left_stopped: 0,
        live_services: None,
    };
    let vm = s.read.dashboard(&inputs).await?;
    let categories = vm
        .categories
        .iter()
        .map(|c| CategoryRow {
            slug: c.category.as_str(),
            label: ctx.tr.t(category_key(c.category)),
            count: c.count,
            severity: sev(c.max_severity),
            size: bytes(c.logical_bytes),
            level: severity_look(sev(c.max_severity)).0,
            icon: severity_look(sev(c.max_severity)).1,
            severity_key: severity_look(sev(c.max_severity)).2,
        })
        .collect();
    let reasons = vm
        .urgency
        .reasons
        .iter()
        .map(|r| (ctx.tr.t(reason_key(r.key)), r.count))
        .collect();
    let services = vm
        .services
        .iter()
        .map(|x| ServiceRow {
            name: x.service.clone(),
            state: ctx.tr.t(service_state_key(x.state_key)),
            version: x.version.clone(),
            current: x.current,
            level: match x.state_key {
                "ok" => "ok",
                "not_configured" | "unknown" => "info",
                _ => "warn",
            },
            icon: match x.state_key {
                "ok" => "check-circle",
                "not_configured" | "unknown" => "info-circle",
                _ => "alert-triangle",
            },
            observed_at: x.observed_at_iso.clone(),
        })
        .collect();
    let view = DashboardView {
        reasons,
        services,
        potential: bytes(vm.space.potential_bytes),
        shared: bytes(vm.space.shared_bytes),
        logical: bytes(vm.space.logical_bytes),
        allocated: bytes(vm.space.unique_allocated_bytes),
        overlap: bytes(vm.space.potential_overlap_excluded_bytes),
        refreshable: s
            .settings
            .configured_services
            .iter()
            .map(|k| k.as_str())
            .collect(),
        rescannable: s
            .settings
            .configured_services
            .iter()
            .filter(|k| **k != removarr_domain::session::ServiceKind::Qbittorrent)
            .map(|k| k.as_str())
            .collect(),
        last_scan: vm
            .last_scan
            .as_ref()
            .and_then(|s| s.finished_at_iso.clone()),
        categories,
        jobs: status.jobs.iter().map(job_row).collect(),
        auto_paused: status.auto_paused.clone(),
        recovery_key: match status.recovery {
            RecoveryStatus::Clean => "recovery.clean",
            RecoveryStatus::Unknown => "recovery.unknown",
            RecoveryStatus::Pending { .. } => "recovery.pending",
            RecoveryStatus::NeedsManual => "recovery.needs_manual",
        },
        recovery_needs_manual: status.recovery == RecoveryStatus::NeedsManual,
        next_scan: status.next_scan.map(|t| iso(t.as_millis())),
        urgency_class: match vm.urgency.level {
            removarr_reporting::Urgency::Green => "ok",
            removarr_reporting::Urgency::Orange => "warn",
            removarr_reporting::Urgency::Red => "crit",
        },
        vm,
    };
    Ok(render(
        &ctx,
        &DashboardPage {
            ctx: &ctx,
            v: &view,
        },
        &DashboardFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}
