//! Issue tabs, details, the ignore list and the read-only selection summary.
//!
//! The read model returns the current issues of a category (bounded at
//! [`READ_LIMIT`]); search, kind filter, "hide recent", sorting and paging are presentation over
//! that set and happen here. Nothing is preselected: every checkbox starts unchecked.

use std::collections::BTreeSet;

use askama::Template;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum_extra::extract::Form;
use axum_extra::extract::FormRejection;
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{IssueCategory, Severity};
use removarr_domain::jobs::JobKind;
use removarr_reporting::{IssueListVm, IssueQuery, IssueRowVm};
use removarr_workflows::plans::IssueAction;

use super::{
    Cmd, Ctx, bytes, category_key, parse_form, render, see_other_flash, severity_look, url_encode,
};
use crate::error::WebError;
use crate::forms::{
    self, IgnoreForm, KindFilter, ListQuery, ListQueryRaw, MAX_REASON_CHARS, SelectionForm, SortKey,
};
use crate::state::AppState;

/// Rows read from the read model per request (its own maximum).
pub const READ_LIMIT: u64 = 1000;
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub id: String,
    pub subject: String,
    pub name: String,
    pub kind: &'static str,
    pub category: &'static str,
    pub age_days: i64,
    pub size: String,
    pub size_bytes: u64,
    pub torrents: usize,
    pub severity: &'static str,
    pub blockers: usize,
    /// Presentation of the severity: css class, glyph id and label key.
    pub level: &'static str,
    pub icon: &'static str,
    pub severity_key: &'static str,
    /// The evidence of this item is incomplete (or its completeness is unknown).
    pub incomplete: bool,
    /// True only on the explicit `select_all=1` render, for the rows of the current page.
    pub checked: bool,
}

#[derive(Debug, Clone)]
pub struct ActionOption {
    pub value: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone)]
pub struct ListView {
    /// `None` for the ignored view.
    pub category: Option<&'static str>,
    pub title: &'static str,
    pub rows: Vec<RowView>,
    pub total_count: usize,
    pub total_size: String,
    pub page: u32,
    pub pages: u32,
    pub q: String,
    pub sort: &'static str,
    pub dir: &'static str,
    pub kind: &'static str,
    pub hide_recent: bool,
    pub hide_recent_days: u32,
    /// The read model holds more issues than one request reads.
    pub truncated: bool,
    pub snapshot_at: Option<String>,
    pub actions: Vec<ActionOption>,
    pub prev_url: Option<String>,
    pub next_url: Option<String>,
    pub ignored_mode: bool,
    pub per_page: u32,
    /// The explicit `select_all=1` render: every row of this page is ticked.
    pub select_all: bool,
    pub select_all_url: String,
    pub clear_url: String,
    /// "N on this page | M in this list (size)".
    pub strip: String,
    /// Initial text of the selection bar (server count; the fragment replaces it).
    pub sel_text: String,
    pub hidden_note: Option<String>,
    /// Which empty state to show when there is no row.
    pub empty_key: Option<&'static str>,
    pub on_page: usize,
    pub filters_active: bool,
    pub clear_filters_url: String,
}

#[derive(Debug, Template)]
#[template(path = "pages/issues.html")]
pub struct IssuesPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a ListView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/issues.html")]
pub struct IssuesFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a ListView,
}

#[derive(Debug, Clone)]
pub struct DetailView {
    pub row: IssueRowVm,
    pub view: RowView,
    pub category_label: &'static str,
    pub actions: Vec<ActionOption>,
    pub outcome: String,
    pub first_seen: String,
    /// Estimate of what could be reclaimed (a potential gain, never a promise).
    pub potential: String,
    pub potential_counted: String,
}

#[derive(Debug, Template)]
#[template(path = "pages/selection_summary.html")]
pub struct SummaryPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a SummaryView,
}

#[derive(Debug, Template)]
#[template(path = "pages/issue_detail.html")]
pub struct DetailPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a DetailView,
}

#[derive(Debug, Template)]
#[template(path = "fragments/issue_detail.html")]
pub struct DetailFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a DetailView,
}

#[derive(Debug, Clone)]
pub struct SummaryView {
    pub count: usize,
    pub size: String,
    pub unknown: usize,
    /// The read-only sentence shown in the selection bar.
    pub text: String,
}

#[derive(Debug, Template)]
#[template(path = "fragments/selection_summary.html")]
pub struct SummaryFragment<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a SummaryView,
}

fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// Every issue action that applies to `category`, as form options.
pub(crate) fn actions_for(ctx: &Ctx, category: IssueCategory) -> Vec<ActionOption> {
    let all = [
        (IssueAction::Consolidate, "action.consolidate"),
        (IssueAction::Delete, "action.delete"),
        (IssueAction::DeleteReplaced, "action.delete_replaced"),
        (IssueAction::CleanupStalled, "action.cleanup_stalled"),
        (IssueAction::FixMode, "action.fix_mode"),
        (
            IssueAction::ReplaceRelease {
                delete_old_media: false,
            },
            "action.replace_release",
        ),
        (IssueAction::RemoveQueueItem, "action.remove_queue_item"),
    ];
    all.into_iter()
        .filter(|(a, _)| a.accepts(category))
        .map(|(a, k)| ActionOption {
            value: forms::action_value(a),
            label: ctx.tr.t(k),
        })
        .collect()
}

fn row_view(r: &IssueRowVm, now_ms: i64) -> RowView {
    let sev = severity(r.severity);
    let (level, icon, severity_key) = severity_look(sev);
    RowView {
        id: r.id.clone(),
        subject: r.subject.clone(),
        name: r.name.clone(),
        kind: r.unit_kind,
        category: r.category.as_str(),
        age_days: (now_ms.saturating_sub(r.age_basis) / DAY_MS).max(0),
        size: bytes(r.logical_bytes),
        size_bytes: r.logical_bytes,
        torrents: r.torrent_count,
        severity: sev,
        blockers: r.blockers.len(),
        level,
        icon,
        severity_key,
        incomplete: r.completeness.state != "complete",
        checked: false,
    }
}

/// Filters, sorts and pages rows (presentation only). Returns the page, the filtered count,
/// the filtered total size and the page count.
pub(crate) fn select(
    rows: &[IssueRowVm],
    q: &ListQuery,
    ignored_mode: bool,
    now_ms: i64,
    hide_recent_days: u32,
) -> (Vec<RowView>, usize, u64, u32) {
    let needle = q.q.as_ref().map(|s| s.to_lowercase());
    let mut v: Vec<RowView> = rows
        .iter()
        .filter(|r| r.ignored == ignored_mode)
        .filter(|r| match q.kind {
            KindFilter::All => true,
            KindFilter::Folder => r.unit_kind == "folder",
            KindFilter::File => r.unit_kind == "file",
        })
        .filter(|r| {
            needle.as_ref().is_none_or(|n| {
                r.name.to_lowercase().contains(n)
                    || r.subject.to_lowercase().contains(n)
                    || r.paths.iter().any(|p| p.to_lowercase().contains(n))
            })
        })
        .map(|r| row_view(r, now_ms))
        .filter(|r| !q.hide_recent || r.age_days >= i64::from(hide_recent_days))
        .collect();
    v.sort_by(|a, b| {
        let o = match q.sort {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
            SortKey::Age => a.age_days.cmp(&b.age_days),
        };
        let o = if q.desc { o.reverse() } else { o };
        o.then_with(|| a.id.cmp(&b.id))
    });
    let total = v.len();
    let total_bytes = v.iter().fold(0u64, |s, r| s.saturating_add(r.size_bytes));
    let per = q.per_page.max(1) as usize;
    let pages = u32::try_from(total.div_ceil(per).max(1)).unwrap_or(u32::MAX);
    let start = (q.page.saturating_sub(1) as usize).saturating_mul(per);
    let page = v.into_iter().skip(start).take(per).collect();
    (page, total, total_bytes, pages)
}

fn page_url(base: &str, q: &ListQuery, page: u32, select_all: bool) -> String {
    let mut url = format!(
        "{base}?page={page}&per_page={}&sort={}&dir={}&kind={}&hide_recent={}",
        q.per_page,
        sort_str(q.sort),
        if q.desc { "desc" } else { "asc" },
        kind_str(q.kind),
        u8::from(q.hide_recent)
    );
    if let Some(s) = &q.q {
        url.push_str("&q=");
        url.push_str(&url_encode(s));
    }
    if select_all {
        url.push_str("&select_all=1");
    }
    url
}

fn sort_str(s: SortKey) -> &'static str {
    match s {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Age => "age",
    }
}

fn kind_str(k: KindFilter) -> &'static str {
    match k {
        KindFilter::All => "all",
        KindFilter::Folder => "folder",
        KindFilter::File => "file",
    }
}

async fn read_all(s: &AppState, category: Option<IssueCategory>) -> Result<IssueListVm, WebError> {
    Ok(s.read
        .issues(&IssueQuery {
            category,
            min_severity: None,
            offset: 0,
            limit: READ_LIMIT,
        })
        .await?)
}

/// Display-only flag of the list routes: tick every row of the current page (server round
/// trip; the default render and every other navigation tick nothing).
#[derive(Debug, Default, serde::Deserialize)]
pub struct SelectAllRaw {
    pub select_all: Option<String>,
}

impl SelectAllRaw {
    fn validate(&self) -> Result<bool, WebError> {
        match self.select_all.as_deref().map(str::trim) {
            None | Some("" | "0") => Ok(false),
            Some("1") => Ok(true),
            Some(_) => Err(WebError::invalid("select_all", "use 0 or 1")),
        }
    }
}

fn build_list(
    ctx: &Ctx,
    s: &AppState,
    listing: &IssueListVm,
    q: &ListQuery,
    category: Option<IssueCategory>,
    select_all: bool,
) -> ListView {
    let ignored_mode = category.is_none();
    let now = s.clock.now().as_millis();
    let days = s.settings.hide_recent_days;
    let (mut rows, total, total_bytes, pages) = select(&listing.rows, q, ignored_mode, now, days);
    let base = match category {
        Some(c) => format!("/issues/{}", c.as_str()),
        None => "/ignored".to_string(),
    };
    // Select all never spans pages: it ticks exactly the rows this page shows.
    let page_bytes = rows
        .iter()
        .fold(0u64, |a, r| a.saturating_add(r.size_bytes));
    let on_page = rows.len();
    if select_all {
        for r in &mut rows {
            r.checked = true;
        }
    }
    let hidden = if q.hide_recent {
        let mut all = q.clone();
        all.hide_recent = false;
        select(&listing.rows, &all, ignored_mode, now, days).1 - total
    } else {
        0
    };
    let truncated = listing.total_count > listing.rows.len() as u64;
    let filters_active = q.q.is_some() || q.kind != KindFilter::All || hidden > 0;
    let empty_key = if on_page > 0 {
        None
    } else if listing.session_finished_at_iso.is_none() {
        Some("empty.no_scan")
    } else if filters_active {
        Some("empty.filtered")
    } else if ignored_mode {
        Some("empty.ignored")
    } else {
        Some("empty.category")
    };
    let size = bytes(total_bytes);
    let sel_text = if select_all {
        ctx.tr.fmt(
            "issues.select_all_banner",
            &[("n", &on_page.to_string()), ("size", &bytes(page_bytes))],
        )
    } else {
        ctx.tr.t("issues.sel_none").to_string()
    };
    ListView {
        category: category.map(IssueCategory::as_str),
        title: category.map_or(ctx.tr.t("nav.ignored"), |c| ctx.tr.t(category_key(c))),
        rows,
        total_count: total,
        total_size: size.clone(),
        page: q.page,
        pages,
        q: q.q.clone().unwrap_or_default(),
        sort: sort_str(q.sort),
        dir: if q.desc { "desc" } else { "asc" },
        kind: kind_str(q.kind),
        hide_recent: q.hide_recent,
        hide_recent_days: days,
        truncated,
        snapshot_at: listing.session_finished_at_iso.clone(),
        actions: category.map(|c| actions_for(ctx, c)).unwrap_or_default(),
        prev_url: (q.page > 1).then(|| page_url(&base, q, q.page - 1, false)),
        next_url: (q.page < pages).then(|| page_url(&base, q, q.page + 1, false)),
        ignored_mode,
        per_page: q.per_page,
        select_all,
        select_all_url: page_url(&base, q, q.page, true),
        clear_url: page_url(&base, q, q.page, false),
        strip: ctx.tr.fmt(
            "issues.strip",
            &[
                ("n", &on_page.to_string()),
                ("m", &total.to_string()),
                ("size", &size),
            ],
        ),
        sel_text,
        hidden_note: (hidden > 0).then(|| {
            ctx.tr.fmt(
                "issues.hidden_recent",
                &[("n", &hidden.to_string()), ("m", &days.to_string())],
            )
        }),
        empty_key,
        on_page,
        filters_active,
        clear_filters_url: if ignored_mode {
            "/ignored".to_string()
        } else {
            format!("{base}?hide_recent=0")
        },
    }
}

async fn list_response(
    ctx: &Ctx,
    s: &AppState,
    category: Option<IssueCategory>,
    q: &ListQuery,
    select_all: bool,
) -> Result<Response, WebError> {
    let listing = read_all(s, category).await?;
    let view = build_list(ctx, s, &listing, q, category, select_all);
    Ok(render(
        ctx,
        &IssuesPage { ctx, v: &view },
        &IssuesFragment { ctx, v: &view },
    ))
}

/// `GET /issues/{category}`.
pub async fn list(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<ListQueryRaw>, QueryRejection>,
    all: Result<Query<SelectAllRaw>, QueryRejection>,
) -> Result<Response, WebError> {
    let Path(raw) = path.map_err(|_| WebError::not_found())?;
    let category = forms::category(&raw).map_err(|_| WebError::not_found())?;
    let Query(raw_q) = query.map_err(|_| WebError::invalid("query", "malformed query"))?;
    let q = raw_q.validate(s.settings.hide_recent_days > 0)?;
    let select_all = all
        .map_err(|_| WebError::invalid("query", "malformed query"))?
        .0
        .validate()?;
    list_response(&ctx, &s, Some(category), &q, select_all).await
}

/// `GET /ignored`.
pub async fn ignored(
    ctx: Ctx,
    State(s): State<AppState>,
    query: Result<Query<ListQueryRaw>, QueryRejection>,
    all: Result<Query<SelectAllRaw>, QueryRejection>,
) -> Result<Response, WebError> {
    let Query(raw_q) = query.map_err(|_| WebError::invalid("query", "malformed query"))?;
    let q = raw_q.validate(false)?;
    let select_all = all
        .map_err(|_| WebError::invalid("query", "malformed query"))?
        .0
        .validate()?;
    list_response(&ctx, &s, None, &q, select_all).await
}

/// `GET /issues/{category}/{issue_id}`.
pub async fn detail(
    ctx: Ctx,
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, WebError> {
    let Path((raw_cat, raw_id)) = path.map_err(|_| WebError::not_found())?;
    let category = forms::category(&raw_cat).map_err(|_| WebError::not_found())?;
    let id = forms::issue_id(&raw_id).map_err(|_| WebError::not_found())?;
    let listing = read_all(&s, Some(category)).await?;
    let row = listing
        .rows
        .into_iter()
        .find(|r| r.id == id.0)
        .ok_or_else(WebError::not_found)?;
    let view = DetailView {
        view: row_view(&row, s.clock.now().as_millis()),
        category_label: ctx.tr.t(category_key(category)),
        actions: actions_for(&ctx, category),
        outcome: format!("{:?}", row.outcome),
        first_seen: row.first_seen_at_iso.clone(),
        potential: bytes(row.potential_bytes),
        potential_counted: bytes(row.potential_counted_bytes),
        row,
    };
    Ok(render(
        &ctx,
        &DetailPage {
            ctx: &ctx,
            v: &view,
        },
        &DetailFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}

/// `POST /issues/selection-summary`: count and size of a selection. Read-only (a `POST` only
/// because selections can be large).
pub async fn selection_summary(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<SelectionForm>, FormRejection>,
) -> Result<Response, WebError> {
    let ids: BTreeSet<IssueId> = forms::issue_ids(&parse_form(f)?.issue_ids)?
        .into_iter()
        .collect();
    let listing = read_all(&s, None).await?;
    let found: Vec<&IssueRowVm> = listing
        .rows
        .iter()
        .filter(|r| ids.contains(&IssueId(r.id.clone())))
        .collect();
    let size = bytes(
        found
            .iter()
            .fold(0u64, |a, r| a.saturating_add(r.logical_bytes)),
    );
    let unknown = ids.len() - found.len();
    let mut text = format!(
        "{} - {}: {}",
        ctx.tn("issues.sel_count", found.len()),
        ctx.tr.t("command.selection_total"),
        size
    );
    if unknown > 0 {
        text.push_str(" - ");
        text.push_str(&ctx.tn("issues.sel_unknown_count", unknown));
    }
    let view = SummaryView {
        count: found.len(),
        size,
        unknown,
        text,
    };
    Ok(render(
        &ctx,
        &SummaryPage {
            ctx: &ctx,
            v: &view,
        },
        &SummaryFragment {
            ctx: &ctx,
            v: &view,
        },
    ))
}

/// Subjects of an ignore or restore request: explicit `subjects`, or the subjects of the
/// selected `issue_ids` (all must be current issues; one unknown id rejects the request).
async fn resolve_subjects(s: &AppState, f: &IgnoreForm) -> Result<Vec<SubjectKey>, WebError> {
    if f.issue_ids.is_empty() {
        return forms::subjects(&f.subjects);
    }
    let ids: BTreeSet<IssueId> = forms::issue_ids(&f.issue_ids)?.into_iter().collect();
    let listing = read_all(s, None).await?;
    let subjects: Vec<String> = listing
        .rows
        .iter()
        .filter(|r| ids.contains(&IssueId(r.id.clone())))
        .map(|r| r.subject.clone())
        .collect();
    if subjects.len() != ids.len() {
        return Err(WebError::from_class(
            &removarr_domain::error::ErrorClass::NotFound,
            Some("error.plan.selection_unknown"),
            "ignore selection names unknown issues",
        ));
    }
    forms::subjects(&subjects)
}

/// `POST /issues/ignore`: stored in the database with reason and date (never browser-only).
pub async fn ignore(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<IgnoreForm>, FormRejection>,
) -> Result<Response, WebError> {
    let f = parse_form(f)?;
    let reason = forms::text(f.reason.as_deref(), "reason", MAX_REASON_CHARS)?;
    let subjects = resolve_subjects(&s, &f).await?;
    s.ignores
        .ignore(&subjects, reason.as_deref(), s.clock.now())
        .await?;
    Ok(see_other_flash(&ctx, "/ignored", "ignored"))
}

/// `POST /issues/restore`.
pub async fn restore(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<IgnoreForm>, FormRejection>,
) -> Result<Response, WebError> {
    let f = parse_form(f)?;
    let subjects = resolve_subjects(&s, &f).await?;
    s.ignores.restore(&subjects).await?;
    Ok(see_other_flash(&ctx, "/ignored", "restored"))
}

/// `POST /issues/evaluate`: queues an `Evaluate` job (ARR and ffprobe reads only).
pub async fn evaluate(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<SelectionForm>, FormRejection>,
) -> Result<Response, WebError> {
    let issue_ids = forms::issue_ids(&parse_form(f)?.issue_ids)?;
    let job = s.jobs.submit(JobKind::EvaluateIssues { issue_ids }).await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/jobs/{}", job.id),
        "job_queued",
    ))
}

/// `POST /issues/verify-content`: queues a `VerifyContent` job (bounded full comparison).
pub async fn verify_content(
    Cmd(ctx): Cmd,
    State(s): State<AppState>,
    f: Result<Form<SelectionForm>, FormRejection>,
) -> Result<Response, WebError> {
    let issue_ids = forms::issue_ids(&parse_form(f)?.issue_ids)?;
    let job = s.jobs.submit(JobKind::VerifyContent { issue_ids }).await?;
    Ok(see_other_flash(
        &ctx,
        &format!("/jobs/{}", job.id),
        "job_queued",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use removarr_domain::issue::Outcome;
    use removarr_reporting::CompletenessVm;

    pub(crate) fn row(
        id: &str,
        name: &str,
        kind: &'static str,
        bytes: u64,
        age_days: i64,
        ignored: bool,
    ) -> IssueRowVm {
        IssueRowVm {
            id: id.into(),
            category: IssueCategory::Junk,
            severity: Severity::Warning,
            outcome: Outcome::KeepManual,
            subject: format!("p:completed:{name}"),
            name: name.into(),
            unit_kind: kind,
            paths: vec![format!("completed:{name}")],
            logical_bytes: bytes,
            potential_bytes: bytes,
            potential_counted_bytes: bytes,
            potential_label_key: "space.potential_not_guaranteed",
            torrent_count: 0,
            blockers: vec![],
            completeness: CompletenessVm {
                state: "complete",
                missing: vec![],
            },
            evidence: vec![],
            inodes: vec![],
            first_seen_at: 0,
            first_seen_at_iso: String::new(),
            age_basis: 100 * DAY_MS - age_days * DAY_MS,
            ignored,
        }
    }

    fn q() -> ListQuery {
        ListQueryRaw::default().validate(false).unwrap()
    }

    #[test]
    fn filters_sort_and_page_without_preselection() {
        let rows = vec![
            row("a", "Alpha.nfo", "file", 10, 1, false),
            row("b", "beta", "folder", 30, 9, false),
            row("c", "Gamma.txt", "file", 20, 5, false),
            row("d", "Delta", "file", 99, 50, true),
        ];
        let now = 100 * DAY_MS;
        let (page, total, size, pages) = select(&rows, &q(), false, now, 2);
        assert_eq!((total, size, pages), (3, 60, 1));
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );

        let mut by_name = q();
        by_name.sort = SortKey::Name;
        by_name.desc = false;
        let (page, ..) = select(&rows, &by_name, false, now, 2);
        assert_eq!(page[0].name, "Alpha.nfo");

        let mut recent = q();
        recent.hide_recent = true;
        let (page, total, ..) = select(&rows, &recent, false, now, 2);
        assert_eq!(total, 2);
        assert!(page.iter().all(|r| r.age_days >= 2));

        let mut search = q();
        search.q = Some("GAMMA".into());
        assert_eq!(select(&rows, &search, false, now, 2).1, 1);

        let mut folders = q();
        folders.kind = KindFilter::Folder;
        assert_eq!(select(&rows, &folders, false, now, 2).1, 1);

        let mut paged = q();
        paged.per_page = 2;
        paged.page = 2;
        let (page, total, _, pages) = select(&rows, &paged, false, now, 2);
        assert_eq!((page.len(), total, pages), (1, 3, 2));

        let (page, ..) = select(&rows, &q(), true, now, 2);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "d");
    }
}
