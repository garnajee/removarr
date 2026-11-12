//! Route tests: Axum service calls with fake dependencies only (no database, no filesystem, no
//! network). They cover success paths, validation, the fail-closed guard boundary, "GET never
//! mutates", unit = bulk validation, safe error bodies and cheap health probes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use http_body_util::BodyExt;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::gain::PlanGains;
use removarr_domain::ids::{JobId, PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::issue::{IssueCategory, Outcome, Severity};
use removarr_domain::jobs::{JobKind, JobStatus, Lane};
use removarr_domain::plan::{ApprovalRequest, PlanHash, PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{Clock, IgnoreStore};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use removarr_jobs::{CancelResult, Enqueued, JobProgress, RecoveryStatus, RuntimeStatus};
use removarr_reporting::{
    AutoVm, CompletenessVm, DashboardInputs, DashboardVm, GainTotalsVm, HistoryQuery, HistoryVm,
    IssueListVm, IssueQuery, IssueRowVm, LogBuffer, LogLevel, PlanReportVm, ReportBundle, SpaceVm,
    StatsQuery, StatsVm, Urgency, UrgencyVm,
};
use removarr_workflows::plans::{IssueAction, PlanPreview, PlanRequest};
use tower::ServiceExt;

use crate::state::{
    Access, AppState, Denied, FailClosedGuard, Guard, JobControl, PlanControl, Principal,
    ReadModel, Readiness, UseCaseError, WebSettings,
};

const NOW: i64 = 1_700_000_000_000;
const DAY: i64 = 86_400_000;
const CSRF: &str = "test-csrf-token";
const ID_A: &str = "0123456789abcdef0123456789abcdef";
const ID_B: &str = "fedcba9876543210fedcba9876543210";
const ID_UNKNOWN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECRET_PATH: &str = "/srv/media/very/secret/host/path";
const SECRET_KEY: &str = "apikey=SUPERSECRETVALUE";

fn uuid(n: u128) -> RawUuid {
    RawUuid::from_u128(n)
}

// ---- fakes -------------------------------------------------------------------------------------

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(NOW)
    }
}

fn row(id: &str, name: &str, category: IssueCategory, ignored: bool) -> IssueRowVm {
    IssueRowVm {
        id: id.into(),
        category,
        severity: Severity::Warning,
        outcome: Outcome::KeepManual,
        subject: format!("p:completed:{name}"),
        name: name.into(),
        unit_kind: "file",
        paths: vec![format!("completed:{name}")],
        logical_bytes: 1000,
        potential_bytes: 1000,
        potential_counted_bytes: 1000,
        potential_label_key: "space.potential_not_guaranteed",
        torrent_count: 1,
        blockers: vec![],
        completeness: CompletenessVm {
            state: "complete",
            missing: vec![],
        },
        evidence: vec![],
        inodes: vec!["1:2".into()],
        first_seen_at: NOW - 30 * DAY,
        first_seen_at_iso: "2023-10-15T00:00:00.000Z".into(),
        age_basis: NOW - 30 * DAY,
        ignored,
    }
}

fn dashboard_vm() -> DashboardVm {
    DashboardVm {
        generated_at: NOW,
        generated_at_iso: String::new(),
        urgency: UrgencyVm {
            level: Urgency::Green,
            label_key: Urgency::Green.label_key(),
            label: Urgency::Green.label_en(),
            icon: Urgency::Green.icon(),
            reasons: vec![],
        },
        categories: vec![],
        ignored_issues: 0,
        space: SpaceVm {
            logical_bytes: 0,
            unique_allocated_bytes: 0,
            shared_bytes: 0,
            potential_bytes: 0,
            potential_overlap_excluded_bytes: 0,
            potential_label_key: "space.potential_not_guaranteed",
            observed_at: None,
            observed_at_iso: None,
            roots: vec![],
        },
        services: vec![],
        last_scan: None,
        issues_session: None,
        running_jobs: vec![],
        torrents_left_stopped: 0,
        recovery_pending: false,
        unfinished_intents: 0,
        auto: AutoVm {
            enabled: false,
            paused: false,
            paused_reason: None,
            paused_at: None,
            paused_at_iso: None,
            cycle_marker_session: None,
        },
        dry_run: true,
        explanations: vec![],
    }
}

fn stats_vm() -> StatsVm {
    StatsVm {
        generated_at: NOW,
        series: vec![],
        totals: GainTotalsVm {
            attributable_bytes: 0,
            added_bytes: 0,
            net_bytes: 0,
            unique_gain_ids: 0,
            duplicate_gain_ids_ignored: 0,
        },
        observed: vec![],
        caveats: vec![],
        truncated: false,
    }
}

#[derive(Default)]
struct FakeRead {
    calls: AtomicUsize,
    fail_with: Mutex<Option<PortError>>,
}

impl FakeRead {
    fn hit(&self) -> Result<(), PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.fail_with.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl ReadModel for FakeRead {
    async fn dashboard(&self, _i: &DashboardInputs) -> Result<DashboardVm, PortError> {
        self.hit()?;
        Ok(dashboard_vm())
    }
    async fn issues(&self, q: &IssueQuery) -> Result<IssueListVm, PortError> {
        self.hit()?;
        let all = vec![
            row(ID_A, "Alpha.nfo", IssueCategory::Junk, false),
            row(ID_B, "Beta.txt", IssueCategory::Junk, false),
            row(
                "cccccccccccccccccccccccccccccccc",
                "Ignored.nfo",
                IssueCategory::Junk,
                true,
            ),
        ];
        let rows: Vec<_> = all
            .into_iter()
            .filter(|r| q.category.is_none_or(|c| c == r.category))
            .collect();
        Ok(IssueListVm {
            total_count: rows.len() as u64,
            total_logical_bytes: 0,
            total_potential_bytes: 0,
            offset: 0,
            limit: q.limit,
            issues_session: None,
            session_finished_at: None,
            session_finished_at_iso: None,
            explanations: vec![],
            rows,
        })
    }
    async fn history(&self, q: &HistoryQuery) -> Result<HistoryVm, PortError> {
        self.hit()?;
        Ok(HistoryVm {
            rows: vec![],
            offset: q.offset,
            limit: q.limit,
            has_more: false,
            unfinished_intents: 0,
        })
    }
    async fn stats(&self, _q: &StatsQuery) -> Result<StatsVm, PortError> {
        self.hit()?;
        Ok(stats_vm())
    }
    async fn plan_report(&self, _p: PlanId) -> Result<PlanReportVm, PortError> {
        self.hit()?;
        Err(PortError::new(ErrorClass::NotFound, "no report"))
    }
    async fn report(
        &self,
        _s: Option<SessionId>,
        _i: &DashboardInputs,
    ) -> Result<ReportBundle, PortError> {
        self.hit()?;
        Ok(ReportBundle {
            generated_at: NOW,
            generated_at_iso: "2023-11-14T22:13:20.000Z".into(),
            requested_session: None,
            session: None,
            dashboard: dashboard_vm(),
            issues: self.issues(&IssueQuery::default()).await?,
            history: HistoryVm {
                rows: vec![],
                offset: 0,
                limit: 0,
                has_more: false,
                unfinished_intents: 0,
            },
            stats: stats_vm(),
            plans: vec![],
            explanations: vec![],
        })
    }
}

#[derive(Default)]
struct FakeJobs {
    submitted: Mutex<Vec<JobKind>>,
    cancels: AtomicUsize,
    resumes: AtomicUsize,
    retries: AtomicUsize,
}

fn progress(id: JobId) -> JobProgress {
    JobProgress {
        id,
        label: "scan".into(),
        lane: Lane::Observe,
        status: JobStatus::Running,
        stage: Some("scanning".into()),
        done: 1,
        total: 2,
        current: None,
        warnings: vec![],
        attempt: 1,
        enqueued_at: Timestamp(NOW),
        started_at: None,
        finished_at: None,
        cancel_requested: false,
        outcome: None,
    }
}

impl FakeJobs {
    fn mutations(&self) -> usize {
        self.submitted.lock().unwrap().len()
            + self.cancels.load(Ordering::SeqCst)
            + self.resumes.load(Ordering::SeqCst)
            + self.retries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl JobControl for FakeJobs {
    async fn submit(&self, kind: JobKind) -> Result<Enqueued, UseCaseError> {
        self.submitted.lock().unwrap().push(kind);
        Ok(Enqueued {
            id: JobId::new(uuid(7)),
            existing: false,
        })
    }
    async fn cancel(&self, _id: JobId) -> Result<CancelResult, UseCaseError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(CancelResult::Requested)
    }
    fn retry_recovery(&self) {
        self.retries.fetch_add(1, Ordering::SeqCst);
    }
    async fn resume_auto(&self) -> Result<bool, UseCaseError> {
        self.resumes.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
    fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            jobs: vec![progress(JobId::new(uuid(7)))],
            auto_enabled: false,
            auto_paused: Some("a reason".into()),
            recovery: RecoveryStatus::Clean,
            next_scan: None,
            next_refresh: None,
        }
    }
    fn progress(&self, id: JobId) -> Option<JobProgress> {
        (id == JobId::new(uuid(7))).then(|| progress(id))
    }
}

fn preview(status: Option<PlanStatus>) -> PlanPreview {
    PlanPreview {
        id: PlanId::new(uuid(42)),
        hash: PlanHash("a".repeat(64)),
        mode: PlanMode::DryRun,
        dry_run: true,
        origin: PlanOrigin::User,
        status,
        created_at: Timestamp(NOW),
        expires_at: Timestamp(NOW + DAY),
        actions: vec![],
        excluded: vec![],
        gains: PlanGains::default(),
        warnings: vec![],
        required_count: 2,
        required_torrent_removals: 0,
        torrent_removals: vec![],
        samples_to_acknowledge: vec![],
        unrecoverable_to_acknowledge: vec![],
        irreversible: true,
        max_tier: RiskTier::T3,
    }
}

#[derive(Default)]
struct FakePlans {
    created: Mutex<Vec<PlanRequest>>,
    previewed: Mutex<Vec<PlanRequest>>,
    approvals: Mutex<Vec<(PlanId, ApprovalRequest)>>,
    discards: AtomicUsize,
    checks: Mutex<Vec<(PlanId, bool)>>,
    gets: AtomicUsize,
    fail: Mutex<Option<UseCaseError>>,
}

impl FakePlans {
    fn fail(&self) -> Result<(), UseCaseError> {
        match self.fail.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn mutations(&self) -> usize {
        self.created.lock().unwrap().len()
            + self.approvals.lock().unwrap().len()
            + self.discards.load(Ordering::SeqCst)
            + self.checks.lock().unwrap().len()
    }
}

#[async_trait]
impl PlanControl for FakePlans {
    async fn preview(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        self.fail()?;
        self.previewed.lock().unwrap().push(req.clone());
        Ok(preview(None))
    }
    async fn create(&self, req: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        self.fail()?;
        self.created.lock().unwrap().push(req.clone());
        Ok(preview(Some(PlanStatus::Draft)))
    }
    async fn get(&self, id: PlanId) -> Result<PlanPreview, UseCaseError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if id == PlanId::new(uuid(42)) {
            Ok(preview(Some(PlanStatus::Draft)))
        } else {
            Err(UseCaseError::new(
                ErrorClass::NotFound,
                "error.not_found",
                "unknown plan",
            ))
        }
    }
    async fn approve(&self, id: PlanId, req: &ApprovalRequest) -> Result<(), UseCaseError> {
        self.fail()?;
        self.approvals.lock().unwrap().push((id, req.clone()));
        Ok(())
    }
    async fn discard(&self, _id: PlanId) -> Result<(), UseCaseError> {
        self.fail()?;
        self.discards.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn check_executable(&self, id: PlanId, simulate: bool) -> Result<(), UseCaseError> {
        self.checks.lock().unwrap().push((id, simulate));
        self.fail()
    }
}

#[derive(Default)]
struct FakeIgnores {
    ignored: Mutex<Vec<(Vec<SubjectKey>, Option<String>)>>,
    restored: Mutex<Vec<Vec<SubjectKey>>>,
}

impl FakeIgnores {
    fn mutations(&self) -> usize {
        self.ignored.lock().unwrap().len() + self.restored.lock().unwrap().len()
    }
}

#[async_trait]
impl IgnoreStore for FakeIgnores {
    async fn ignore(
        &self,
        subjects: &[SubjectKey],
        reason: Option<&str>,
        _at: Timestamp,
    ) -> Result<(), PortError> {
        self.ignored
            .lock()
            .unwrap()
            .push((subjects.to_vec(), reason.map(str::to_string)));
        Ok(())
    }
    async fn restore(&self, subjects: &[SubjectKey]) -> Result<(), PortError> {
        self.restored.lock().unwrap().push(subjects.to_vec());
        Ok(())
    }
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        Ok(BTreeSet::new())
    }
}

#[derive(Default)]
struct FakeReady {
    calls: AtomicUsize,
    fail: bool,
}

#[async_trait]
impl Readiness for FakeReady {
    async fn ready(&self) -> Result<(), PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(PortError::new(ErrorClass::Storage, SECRET_PATH))
        } else {
            Ok(())
        }
    }
}

/// Stands in for auth-guard: a session for every request, CSRF checked on commands.
struct AllowGuard;

#[async_trait]
impl Guard for AllowGuard {
    async fn check(&self, access: Access, req: Request) -> Result<(Request, Principal), Denied> {
        if access == Access::Command
            && req.headers().get("x-csrf-token").map(|v| v.as_bytes()) != Some(CSRF.as_bytes())
        {
            return Err(Denied::Forbidden);
        }
        Ok((
            req,
            Principal {
                user: "admin".into(),
                csrf_token: CSRF.into(),
            },
        ))
    }
}

/// Grants pages and refuses every command.
struct PageOnlyGuard;

#[async_trait]
impl Guard for PageOnlyGuard {
    async fn check(&self, access: Access, req: Request) -> Result<(Request, Principal), Denied> {
        match access {
            Access::Page => Ok((
                req,
                Principal {
                    user: "admin".into(),
                    csrf_token: CSRF.into(),
                },
            )),
            Access::Command => Err(Denied::Forbidden),
        }
    }
}

struct World {
    read: Arc<FakeRead>,
    jobs: Arc<FakeJobs>,
    plans: Arc<FakePlans>,
    ignores: Arc<FakeIgnores>,
    ready: Arc<FakeReady>,
    app: Router,
}

impl World {
    fn new(guard: Arc<dyn Guard>) -> Self {
        Self::with(guard, false, true)
    }

    fn with(guard: Arc<dyn Guard>, ready_fails: bool, dry_run: bool) -> Self {
        let read = Arc::new(FakeRead::default());
        let jobs = Arc::new(FakeJobs::default());
        let plans = Arc::new(FakePlans::default());
        let ignores = Arc::new(FakeIgnores::default());
        let ready = Arc::new(FakeReady {
            calls: AtomicUsize::new(0),
            fail: ready_fails,
        });
        let state = AppState {
            read: read.clone(),
            jobs: jobs.clone(),
            plans: plans.clone(),
            ignores: ignores.clone(),
            readiness: ready.clone(),
            logs: LogBuffer::new(100, LogLevel::Info),
            guard,
            clock: Arc::new(FixedClock),
            settings: Arc::new(WebSettings {
                dry_run,
                auto_mode: false,
                scan_interval_minutes: 1440,
                hide_recent_days: 2,
                configured_services: vec![ServiceKind::Qbittorrent, ServiceKind::Sonarr],
            }),
        };
        Self {
            read,
            jobs,
            plans,
            ignores,
            ready,
            app: crate::build(state),
        }
    }

    fn mutations(&self) -> usize {
        self.jobs.mutations() + self.plans.mutations() + self.ignores.mutations()
    }

    async fn call(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(&self, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        self.call(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    async fn post(&self, uri: &str, form: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        self.call(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("x-csrf-token", CSRF)
                .body(Body::from(form.to_string()))
                .unwrap(),
        )
        .await
    }
}

const GET_ROUTES: &[&str] = &[
    "/",
    "/issues/junk",
    "/issues/junk?q=alpha&sort=name&dir=asc&kind=file&hide_recent=1&page=1&per_page=10",
    "/issues/junk/0123456789abcdef0123456789abcdef",
    "/ignored",
    "/plans/00000000-0000-0000-0000-00000000002a",
    "/plans/00000000-0000-0000-0000-00000000002a/confirm",
    "/jobs",
    "/jobs/00000000-0000-0000-0000-000000000007",
    "/jobs/00000000-0000-0000-0000-000000000007/progress",
    "/auto",
    "/history",
    "/stats",
    "/logs?level=warn&q=scan",
    "/export/latest/json",
    "/export/latest/csv",
    "/export/latest/md",
];

const POST_ROUTES: &[(&str, &str)] = &[
    ("/scan", ""),
    ("/services/sonarr/refresh", ""),
    ("/services/sonarr/rescan", ""),
    (
        "/issues/selection-summary",
        "issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/issues/ignore",
        "issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/issues/restore",
        "issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/issues/evaluate",
        "issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/issues/verify-content",
        "issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/plans",
        "category=junk&action=delete&issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/plans/preview",
        "category=junk&action=delete&issue_ids=0123456789abcdef0123456789abcdef",
    ),
    (
        "/plans/00000000-0000-0000-0000-00000000002a/approve",
        "plan_hash=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&confirmed_count=2",
    ),
    ("/plans/00000000-0000-0000-0000-00000000002a/execute", ""),
    ("/plans/00000000-0000-0000-0000-00000000002a/simulate", ""),
    ("/plans/00000000-0000-0000-0000-00000000002a/discard", ""),
    ("/jobs/00000000-0000-0000-0000-000000000007/cancel", ""),
    ("/recovery/retry", ""),
    ("/auto/resume", ""),
    ("/locale", "locale=fr"),
];

// ---- health ------------------------------------------------------------------------------------

#[tokio::test]
async fn health_probes_are_public_cheap_and_detail_free() {
    let w = World::new(Arc::new(FailClosedGuard));
    let (s, _, body) = w.get("/healthz").await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "ok"));
    assert_eq!(
        w.ready.calls.load(Ordering::SeqCst),
        0,
        "liveness touches nothing"
    );
    let (s, _, body) = w.get("/readyz").await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "ready"));
    assert_eq!(w.ready.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        w.read.calls.load(Ordering::SeqCst),
        0,
        "no read model query"
    );
    assert_eq!(w.mutations(), 0, "no job, no scan, no plan");

    let w = World::with(Arc::new(FailClosedGuard), true, true);
    let (s, _, body) = w.get("/readyz").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "not ready");
    assert!(!body.contains(SECRET_PATH));
}

// ---- guard boundary ----------------------------------------------------------------------------

#[tokio::test]
async fn without_auth_guard_every_protected_route_is_refused_and_nothing_runs() {
    let w = World::new(Arc::new(FailClosedGuard));
    for uri in GET_ROUTES {
        let (s, h, _) = w.get(uri).await;
        assert_eq!(s, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(h.get(header::LOCATION).unwrap(), "/login", "{uri}");
    }
    for (uri, form) in POST_ROUTES {
        let (s, _, body) = w.post(uri, form).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(body.contains("Sign-in required"), "{uri}");
    }
    assert_eq!(w.mutations(), 0);
    assert_eq!(w.read.calls.load(Ordering::SeqCst), 0);
    assert_eq!(w.plans.gets.load(Ordering::SeqCst), 0);
    let (s, _, body) = w.get("/login").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("not available"));
}

#[tokio::test]
async fn commands_need_the_command_grant_and_the_csrf_check() {
    let w = World::new(Arc::new(PageOnlyGuard));
    assert_eq!(w.get("/").await.0, StatusCode::OK);
    for (uri, form) in POST_ROUTES {
        assert_eq!(w.post(uri, form).await.0, StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(w.mutations(), 0);

    // With a guard that checks CSRF, a command without the token is refused.
    let w = World::new(Arc::new(AllowGuard));
    let (s, ..) = w
        .call(
            Request::post("/scan")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(w.mutations(), 0);
    // A GET to a command path never runs the command (the command guard answers first).
    assert!(w.get("/scan").await.0.is_client_error());
    assert_eq!(w.mutations(), 0);
}

// ---- GET never mutates -------------------------------------------------------------------------

#[tokio::test]
async fn display_routes_render_and_never_mutate() {
    let w = World::new(Arc::new(AllowGuard));
    for uri in GET_ROUTES {
        let (s, _, body) = w.get(uri).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {body}");
    }
    assert_eq!(w.mutations(), 0, "a display route changed state");
    assert!(w.read.calls.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn htmx_requests_get_fragments_and_plain_requests_full_pages() {
    let w = World::new(Arc::new(AllowGuard));
    let (_, _, page) = w.get("/").await;
    assert!(page.contains("<html"));
    assert!(page.contains(CSRF), "forms carry the CSRF token");
    let (s, h, frag) = w
        .call(
            Request::get("/")
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!frag.contains("<html"));
    assert!(frag.contains("id=\"dashboard\""));
    assert_eq!(h.get(header::VARY).unwrap(), "HX-Request");
}

#[tokio::test]
async fn issue_lists_preselect_nothing_and_hide_ignored_items() {
    let w = World::new(Arc::new(AllowGuard));
    let (_, _, body) = w.get("/issues/junk?hide_recent=0").await;
    assert!(body.contains("Alpha.nfo") && body.contains("Beta.txt"));
    assert!(!body.contains("Ignored.nfo"));
    assert!(!body.contains(" checked"), "nothing is preselected");
    let (_, _, ignored) = w.get("/ignored").await;
    assert!(ignored.contains("Ignored.nfo") && !ignored.contains("Alpha.nfo"));
    let (_, _, searched) = w.get("/issues/junk?q=beta&hide_recent=0").await;
    assert!(searched.contains("Beta.txt") && !searched.contains("Alpha.nfo"));
}

// ---- validation --------------------------------------------------------------------------------

#[tokio::test]
async fn inputs_are_validated_with_consistent_errors() {
    let w = World::new(Arc::new(AllowGuard));
    for uri in [
        "/issues/nope",
        "/issues/junk/not-an-id",
        "/issues/junk/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "/plans/42",
        "/plans/00000000-0000-0000-0000-000000000099",
        "/jobs/xyz",
        "/export/latest/exe",
        "/export/not-a-session/json",
        "/nowhere",
    ] {
        assert_eq!(w.get(uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
    for uri in [
        "/issues/junk?sort=inode",
        "/issues/junk?per_page=500",
        "/issues/junk?page=0",
        "/issues/junk?dir=sideways",
        "/history?page=-1",
        "/logs?level=trace",
    ] {
        let (s, _, body) = w.get(uri).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert!(body.contains("The request is invalid"), "{uri}");
    }
    let (s, ..) = w.post("/locale", "locale=de").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, ..) = w.post("/services/radarr/refresh", "").await;
    assert_eq!(s, StatusCode::CONFLICT, "unconfigured service");
    let (s, ..) = w.post("/services/plex/refresh", "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, ..) = w.post("/services/qbittorrent/rescan", "").await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "qBittorrent has no rescan"
    );
    let (s, ..) = w
        .post(
            "/issues/ignore",
            &format!("issue_ids={ID_A}&reason={}", "r".repeat(501)),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, ..) = w
        .post(
            "/plans/00000000-0000-0000-0000-00000000002a/approve",
            "plan_hash=short&confirmed_count=2",
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(w.mutations(), 0);
}

#[tokio::test]
async fn request_sizes_are_bounded() {
    let w = World::new(Arc::new(AllowGuard));
    let big = format!(
        "issue_ids={}",
        "a".repeat(crate::forms::MAX_BODY_BYTES + 10)
    );
    let (s, ..) = w.post("/issues/evaluate", &big).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    let long = format!("/issues/junk?q={}", "a".repeat(crate::forms::MAX_URI_BYTES));
    let (s, ..) = w.get(&long).await;
    assert_eq!(s, StatusCode::URI_TOO_LONG);
    let many: Vec<String> = (0..=crate::forms::MAX_SELECTION)
        .map(|i| format!("issue_ids={i:032x}"))
        .collect();
    let (s, ..) = w.post("/issues/evaluate", &many.join("&")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(w.mutations(), 0);
}

// ---- unit and bulk share one path --------------------------------------------------------------

#[tokio::test]
async fn one_item_and_bulk_selections_take_the_same_validation_path() {
    let w = World::new(Arc::new(AllowGuard));
    let one = format!("category=junk&action=delete&issue_ids={ID_A}");
    let bulk =
        format!("category=junk&action=delete&issue_ids={ID_B}&issue_ids={ID_A}&issue_ids={ID_A}");
    let (s1, h1, _) = w.post("/plans", &one).await;
    let (s2, h2, _) = w.post("/plans", &bulk).await;
    assert_eq!((s1, s2), (StatusCode::SEE_OTHER, StatusCode::SEE_OTHER));
    assert_eq!(h1.get(header::LOCATION), h2.get(header::LOCATION));
    let created = w.plans.created.lock().unwrap().clone();
    let ids = |r: &PlanRequest| match r {
        PlanRequest::Issues {
            category,
            issue_ids,
            action,
            remove_torrent_tasks,
            ..
        } => {
            assert_eq!(*category, IssueCategory::Junk);
            assert_eq!(*action, IssueAction::Delete);
            assert!(!remove_torrent_tasks, "companions are opt-in");
            issue_ids.iter().map(|i| i.0.clone()).collect::<Vec<_>>()
        }
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(ids(&created[0]), vec![ID_A.to_string()]);
    assert_eq!(ids(&created[1]), vec![ID_A.to_string(), ID_B.to_string()]);

    // The same refusal for one bad id alone or inside a bulk selection, and no implicit selection.
    let (bad_one, _, b1) = w
        .post("/plans", "category=junk&action=delete&issue_ids=../x")
        .await;
    let (bad_bulk, _, b2) = w
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={ID_A}&issue_ids=../x"),
        )
        .await;
    let (empty, _, b3) = w.post("/plans", "category=junk&action=delete").await;
    for (s, b) in [(bad_one, &b1), (bad_bulk, &b2), (empty, &b3)] {
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(b.contains("issue_ids"));
    }
    // An action that does not fit the category is refused before planning.
    let (s, ..) = w
        .post(
            "/plans",
            &format!("category=junk&action=consolidate&issue_ids={ID_A}"),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(w.plans.created.lock().unwrap().len(), 2);

    // Evaluate: same selection rules, one job per request.
    w.post(
        "/issues/evaluate",
        &format!("issue_ids={ID_A}&issue_ids={ID_B}"),
    )
    .await;
    assert!(matches!(
        w.jobs.submitted.lock().unwrap().as_slice(),
        [JobKind::EvaluateIssues { issue_ids }] if issue_ids.len() == 2
    ));
}

#[tokio::test]
async fn preview_stores_nothing_and_renders_the_plan() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, _, body) = w
        .post(
            "/plans/preview",
            &format!("category=junk&action=delete&issue_ids={ID_A}"),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Preview only"));
    assert_eq!(w.plans.previewed.lock().unwrap().len(), 1);
    assert_eq!(w.mutations(), 0);
}

// ---- plans: approval and execution -------------------------------------------------------------

#[tokio::test]
async fn approval_is_explicit_and_execution_is_a_checked_job() {
    let w = World::new(Arc::new(AllowGuard));
    let (_, _, confirm) = w
        .get("/plans/00000000-0000-0000-0000-00000000002a/confirm")
        .await;
    assert!(confirm.contains("name=\"confirmed_count\""));
    assert!(confirm.contains(&"a".repeat(64)));
    let (s, h, _) = w
        .post(
            "/plans/00000000-0000-0000-0000-00000000002a/approve",
            &format!(
                "plan_hash={}&confirmed_count=2&acknowledged_samples=completed:Rel/Sample/s.mkv",
                "a".repeat(64)
            ),
        )
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).unwrap(),
        "/plans/00000000-0000-0000-0000-00000000002a"
    );
    let (_, req) = w.plans.approvals.lock().unwrap()[0].clone();
    assert_eq!(req.confirmed_count, 2);
    assert_eq!(
        req.acknowledged_samples[0].to_string(),
        "completed:Rel/Sample/s.mkv"
    );
    assert!(
        w.jobs.submitted.lock().unwrap().is_empty(),
        "approval never executes"
    );

    let (s, h, _) = w
        .post("/plans/00000000-0000-0000-0000-00000000002a/execute", "")
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).unwrap(),
        "/jobs/00000000-0000-0000-0000-000000000007"
    );
    w.post("/plans/00000000-0000-0000-0000-00000000002a/simulate", "")
        .await;
    let plan = PlanId::new(uuid(42));
    assert_eq!(
        *w.jobs.submitted.lock().unwrap(),
        vec![
            JobKind::ExecutePlan {
                plan_id: plan,
                simulate: false
            },
            JobKind::ExecutePlan {
                plan_id: plan,
                simulate: true
            }
        ]
    );
    assert_eq!(
        *w.plans.checks.lock().unwrap(),
        vec![(plan, false), (plan, true)]
    );
}

#[tokio::test]
async fn a_refused_execution_queues_nothing_and_answers_safely() {
    let w = World::new(Arc::new(AllowGuard));
    *w.plans.fail.lock().unwrap() = Some(UseCaseError::new(
        ErrorClass::CapabilityDisabled,
        "error.capability_disabled",
        format!("a dry-run process can only simulate; {SECRET_PATH} {SECRET_KEY}"),
    ));
    let (s, _, body) = w
        .post("/plans/00000000-0000-0000-0000-00000000002a/execute", "")
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(body.contains("disabled"));
    assert!(body.contains("Reference"));
    assert!(!body.contains(SECRET_PATH) && !body.contains("SUPERSECRET"));
    assert!(w.jobs.submitted.lock().unwrap().is_empty());
}

// ---- searches, rescans and refreshes never imply actions ---------------------------------------

#[tokio::test]
async fn rescans_and_refreshes_never_imply_actions() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, _) = w.post("/services/sonarr/rescan", "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).unwrap(),
        "/plans/00000000-0000-0000-0000-00000000002a"
    );
    assert_eq!(
        *w.plans.created.lock().unwrap(),
        vec![PlanRequest::ServiceRescan {
            service: ServiceKind::Sonarr,
            media: None
        }]
    );
    assert!(
        w.jobs.submitted.lock().unwrap().is_empty(),
        "a rescan plan is not executed"
    );
    assert!(w.plans.approvals.lock().unwrap().is_empty());

    w.post("/services/qbittorrent/refresh", "").await;
    w.post("/scan", "").await;
    assert_eq!(
        *w.jobs.submitted.lock().unwrap(),
        vec![
            JobKind::RefreshService(ServiceKind::Qbittorrent),
            JobKind::Scan
        ]
    );
    assert_eq!(w.plans.created.lock().unwrap().len(), 1);
}

// ---- ignore list -------------------------------------------------------------------------------

#[tokio::test]
async fn ignore_and_restore_store_subjects_with_their_reason() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, _) = w
        .post(
            "/issues/ignore",
            &format!("issue_ids={ID_A}&reason=keep+this"),
        )
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h.get(header::LOCATION).unwrap(), "/ignored");
    assert_eq!(
        *w.ignores.ignored.lock().unwrap(),
        vec![(
            vec![SubjectKey("p:completed:Alpha.nfo".into())],
            Some("keep this".into())
        )]
    );
    let (s, ..) = w
        .post(
            "/issues/ignore",
            &format!("issue_ids={ID_A}&issue_ids={ID_UNKNOWN}"),
        )
        .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "one unknown id rejects the request"
    );
    assert_eq!(w.ignores.ignored.lock().unwrap().len(), 1);
    w.post("/issues/restore", "subjects=p:completed:Alpha.nfo")
        .await;
    assert_eq!(w.ignores.restored.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_selection_summary_is_read_only() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, _, body) = w
        .post(
            "/issues/selection-summary",
            &format!("issue_ids={ID_A}&issue_ids={ID_B}&issue_ids={ID_UNKNOWN}"),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains(": 2 (2.0 KiB)"), "{body}");
    assert!(body.contains("Not found: 1"));
    assert_eq!(w.mutations(), 0);
}

// ---- safe errors -------------------------------------------------------------------------------

#[tokio::test]
async fn storage_and_plan_failures_render_no_paths_or_secrets() {
    let w = World::new(Arc::new(AllowGuard));
    *w.read.fail_with.lock().unwrap() = Some(PortError::new(
        ErrorClass::Storage,
        format!("sqlite at {SECRET_PATH}/removarr.db failed; {SECRET_KEY}"),
    ));
    let (s, h, body) = w.get("/").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert!(!body.contains(SECRET_PATH) && !body.contains("SUPERSECRET"));
    assert!(body.contains("database is unavailable"));

    let w = World::new(Arc::new(AllowGuard));
    *w.plans.fail.lock().unwrap() = Some(UseCaseError {
        class: ErrorClass::Validation,
        key: "error.plan.nothing_plannable",
        excluded: vec![crate::state::ExcludedItem {
            subject: "p:completed:Rel/a.mkv".into(),
            blockers: vec!["Sample".into()],
        }],
        log_detail: format!("{SECRET_PATH} {SECRET_KEY}"),
    });
    let (s, _, body) = w
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={ID_A}"),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("p:completed:Rel/a.mkv: Sample"));
    assert!(!body.contains(SECRET_PATH) && !body.contains("SUPERSECRET"));
}

#[tokio::test]
async fn jobs_cancel_resume_and_retry_are_commands() {
    let w = World::new(Arc::new(AllowGuard));
    assert_eq!(
        w.post("/jobs/00000000-0000-0000-0000-000000000007/cancel", "")
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    w.post("/auto/resume", "").await;
    w.post("/recovery/retry", "").await;
    assert_eq!(w.jobs.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(w.jobs.resumes.load(Ordering::SeqCst), 1);
    assert_eq!(w.jobs.retries.load(Ordering::SeqCst), 1);
    let (_, h, _) = w.post("/locale", "locale=fr").await;
    assert!(
        h.get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("removarr_locale=fr;")
    );
    let (_, _, auto) = w.get("/auto").await;
    assert!(auto.contains("a reason"), "the pause reason is shown");
}

#[tokio::test]
async fn exports_are_attachments_rendered_in_memory() {
    let w = World::new(Arc::new(AllowGuard));
    let (s, h, body) = w.get("/export/latest/csv").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        h.get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"removarr-")
    );
    assert!(body.contains("Alpha.nfo"));
}
