//! End-to-end tests of the production guard over the real router (`removarr_web::build`) with
//! fake use cases, a controlled clock and fake credentials only (no network, no deployed
//! endpoint, no real secret).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Method, StatusCode, header};
use http_body_util::BodyExt;
use removarr_config::{Secret, TrustedProxies};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::{JobId, PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::jobs::JobKind;
use removarr_domain::plan::ApprovalRequest;
use removarr_domain::ports::{Clock, IgnoreStore};
use removarr_domain::time::Timestamp;
use removarr_jobs::{CancelResult, Enqueued, JobProgress, RecoveryStatus, RuntimeStatus};
use removarr_reporting::{
    DashboardInputs, DashboardVm, HistoryQuery, HistoryVm, IssueListVm, IssueQuery, LogBuffer,
    LogLevel, PlanReportVm, ReportBundle, StatsQuery, StatsVm,
};
use removarr_testkit::clock::FakeClock;
use removarr_web::auth::{Auth, AuthSettings};
use removarr_web::{
    AppState, JobControl, PlanControl, ReadModel, Readiness, UseCaseError, WebSettings,
};
use removarr_workflows::plans::{PlanPreview, PlanRequest};
use tower::ServiceExt;

const USER: &str = "admin";
const PASSWORD: &str = "fake-only-test-password";
const HOST: &str = "removarr.test";
const ORIGIN: &str = "http://removarr.test";
const H: i64 = 3_600_000;
const MIN: i64 = 60_000;

// ---- fakes -------------------------------------------------------------------------------------

fn unavailable() -> PortError {
    PortError::new(ErrorClass::Unavailable, "fake read model")
}

struct FakeRead;

#[async_trait]
impl ReadModel for FakeRead {
    async fn dashboard(&self, _: &DashboardInputs) -> Result<DashboardVm, PortError> {
        Err(unavailable())
    }
    async fn issues(&self, _: &IssueQuery) -> Result<IssueListVm, PortError> {
        Err(unavailable())
    }
    async fn history(&self, _: &HistoryQuery) -> Result<HistoryVm, PortError> {
        Err(unavailable())
    }
    async fn stats(&self, _: &StatsQuery) -> Result<StatsVm, PortError> {
        Err(unavailable())
    }
    async fn plan_report(&self, _: PlanId) -> Result<PlanReportVm, PortError> {
        Err(unavailable())
    }
    async fn report(
        &self,
        _: Option<SessionId>,
        _: &DashboardInputs,
    ) -> Result<ReportBundle, PortError> {
        Err(unavailable())
    }
}

/// Counts every state change requested through the web layer.
#[derive(Default)]
struct Mutations(AtomicUsize);

impl Mutations {
    fn hit(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct FakeJobs(Arc<Mutations>);

#[async_trait]
impl JobControl for FakeJobs {
    async fn submit(&self, _: JobKind) -> Result<Enqueued, UseCaseError> {
        self.0.hit();
        Ok(Enqueued {
            id: JobId::new(RawUuid::from_u128(7)),
            existing: false,
        })
    }
    async fn cancel(&self, _: JobId) -> Result<CancelResult, UseCaseError> {
        self.0.hit();
        Ok(CancelResult::Requested)
    }
    fn retry_recovery(&self) {
        self.0.hit();
    }
    async fn resume_auto(&self) -> Result<bool, UseCaseError> {
        self.0.hit();
        Ok(true)
    }
    fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            jobs: vec![],
            auto_enabled: false,
            auto_paused: None,
            recovery: RecoveryStatus::Clean,
            next_scan: None,
            next_refresh: None,
        }
    }
    fn progress(&self, _: JobId) -> Option<JobProgress> {
        None
    }
}

struct FakePlans(Arc<Mutations>);

fn refused() -> UseCaseError {
    UseCaseError::new(ErrorClass::NotFound, "error.not_found", "fake planner")
}

#[async_trait]
impl PlanControl for FakePlans {
    async fn preview(&self, _: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        self.0.hit();
        Err(refused())
    }
    async fn create(&self, _: &PlanRequest) -> Result<PlanPreview, UseCaseError> {
        self.0.hit();
        Err(refused())
    }
    async fn get(&self, _: PlanId) -> Result<PlanPreview, UseCaseError> {
        Err(refused())
    }
    async fn approve(&self, _: PlanId, _: &ApprovalRequest) -> Result<(), UseCaseError> {
        self.0.hit();
        Err(refused())
    }
    async fn discard(&self, _: PlanId) -> Result<(), UseCaseError> {
        self.0.hit();
        Err(refused())
    }
    async fn check_executable(&self, _: PlanId, _: bool) -> Result<(), UseCaseError> {
        self.0.hit();
        Err(refused())
    }
}

struct FakeIgnores(Arc<Mutations>);

#[async_trait]
impl IgnoreStore for FakeIgnores {
    async fn ignore(
        &self,
        _: &[SubjectKey],
        _: Option<&str>,
        _: Timestamp,
    ) -> Result<(), PortError> {
        self.0.hit();
        Ok(())
    }
    async fn restore(&self, _: &[SubjectKey]) -> Result<(), PortError> {
        self.0.hit();
        Ok(())
    }
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        Ok(BTreeSet::new())
    }
}

struct FakeReady;

#[async_trait]
impl Readiness for FakeReady {
    async fn ready(&self) -> Result<(), PortError> {
        Ok(())
    }
}

// ---- world -------------------------------------------------------------------------------------

/// Argon2id with cheap test parameters (the PHC string carries them) over a fake password.
fn fake_hash() -> String {
    let a = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(256, 1, 1, None).unwrap(),
    );
    let salt = SaltString::encode_b64(b"fixed-test-salt!").unwrap();
    a.hash_password(PASSWORD.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

fn settings() -> AuthSettings {
    AuthSettings {
        username: USER.into(),
        password_hash: Secret::new(fake_hash()),
        session_key: Secret::new("fake-session-key-for-tests-only-".repeat(3)),
        session_ttl_hours: 12,
        cookie_secure: true,
        login_limit_per_ip: 20,
        login_limit_per_ip_user: 5,
        trusted_proxies: TrustedProxies::none(),
    }
}

struct World {
    auth: Auth,
    app: Router,
    clock: Arc<FakeClock>,
    mutations: Arc<Mutations>,
}

impl World {
    fn new() -> Self {
        Self::with(settings())
    }

    fn with(s: AuthSettings) -> Self {
        let clock = Arc::new(FakeClock::default());
        let auth = Auth::new(s, clock.clone()).unwrap();
        let mutations = Arc::new(Mutations::default());
        let state = AppState {
            read: Arc::new(FakeRead),
            jobs: Arc::new(FakeJobs(mutations.clone())),
            plans: Arc::new(FakePlans(mutations.clone())),
            ignores: Arc::new(FakeIgnores(mutations.clone())),
            readiness: Arc::new(FakeReady),
            logs: LogBuffer::new(100, LogLevel::Info),
            guard: Arc::new(auth.clone()),
            clock: clock.clone() as Arc<dyn Clock>,
            settings: Arc::new(WebSettings {
                dry_run: true,
                auto_mode: false,
                scan_interval_minutes: 1440,
                hide_recent_days: 2,
                configured_services: vec![],
            }),
        };
        Self {
            auth,
            app: removarr_web::build(state),
            clock,
            mutations,
        }
    }

    fn client(&self, peer: &str) -> Client {
        Client {
            app: self.app.clone(),
            peer: peer.parse().unwrap(),
            cookies: BTreeMap::new(),
            xff: None,
        }
    }
}

#[derive(Debug)]
struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    fn set_cookie(&self, name: &str) -> Option<String> {
        self.set_cookies()
            .into_iter()
            .find(|c| c.starts_with(&format!("{name}=")))
    }

    fn location(&self) -> Option<&str> {
        self.headers
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap())
    }

    fn retry_after(&self) -> Option<u64> {
        self.headers
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().parse().unwrap())
    }

    /// The hidden `csrf_token` value of the page.
    fn csrf(&self) -> String {
        let marker = "name=\"csrf_token\" value=\"";
        let start = self.body.find(marker).expect("csrf field") + marker.len();
        let end = self.body[start..].find('"').unwrap() + start;
        self.body[start..end].to_string()
    }
}

/// A browser-like client: a cookie jar with path scoping, a fixed TCP peer.
#[derive(Clone)]
struct Client {
    app: Router,
    peer: SocketAddr,
    /// name -> (value, path)
    cookies: BTreeMap<String, (String, String)>,
    xff: Option<String>,
}

impl Client {
    fn cookie_header(&self, path: &str) -> Option<String> {
        let v: Vec<String> = self
            .cookies
            .iter()
            .filter(|(_, (_, p))| path.starts_with(p.as_str()))
            .map(|(k, (v, _))| format!("{k}={v}"))
            .collect();
        (!v.is_empty()).then(|| v.join("; "))
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies.get(name).map(|(v, _)| v.clone())
    }

    async fn send(&mut self, mut req: Request) -> Resp {
        req.extensions_mut().insert(ConnectInfo(self.peer));
        let path = req.uri().path().to_string();
        if let Some(c) = self.cookie_header(&path) {
            req.headers_mut().insert(header::COOKIE, c.parse().unwrap());
        }
        if let Some(x) = &self.xff {
            req.headers_mut()
                .insert("x-forwarded-for", x.parse().unwrap());
        }
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let r = Resp {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        for c in r.set_cookies() {
            let first = c.split(';').next().unwrap();
            let (name, value) = first.split_once('=').unwrap();
            let path = c
                .split(';')
                .map(str::trim)
                .find_map(|a| a.strip_prefix("Path="))
                .unwrap_or("/")
                .to_string();
            if value.is_empty() || c.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.into(), (value.into(), path));
            }
        }
        r
    }

    async fn get(&mut self, path: &str) -> Resp {
        let req = Request::get(path)
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }

    /// A same-origin form post; `extra` headers are added (or override `Origin`).
    async fn post(&mut self, path: &str, form: &str, extra: &[(&str, &str)]) -> Resp {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("origin")) {
            b = b.header(header::ORIGIN, ORIGIN);
        }
        for (k, v) in extra {
            if !v.is_empty() {
                b = b.header(*k, *v);
            }
        }
        self.send(b.body(Body::from(form.to_string())).unwrap())
            .await
    }

    /// Fetches the login form and posts the credentials with its token.
    async fn login(&mut self, user: &str, password: &str) -> Resp {
        let form = self.get("/login").await;
        assert_eq!(form.status, StatusCode::OK);
        let body = format!(
            "csrf_token={}&username={}&password={}",
            form.csrf(),
            user,
            password
        );
        self.post("/login", &body, &[]).await
    }

    /// Logs in and returns the session CSRF token read from a page.
    async fn session(&mut self) -> String {
        let r = self.login(USER, PASSWORD).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let page = self.get("/jobs").await;
        assert_eq!(page.status, StatusCode::OK);
        page.csrf()
    }
}

/// Body with the per-response token removed, to compare error pages.
fn normalized(r: &Resp) -> String {
    r.body.replace(&r.csrf(), "TOKEN")
}

const COMMANDS: &[(&str, &str)] = &[
    ("/scan", ""),
    ("/services/qbittorrent/refresh", ""),
    ("/services/sonarr/rescan", ""),
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

// ---- cookies and headers -----------------------------------------------------------------------

#[tokio::test]
async fn login_sets_hardened_cookies_and_security_headers() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let form = c.get("/login").await;
    assert_eq!(form.status, StatusCode::OK);
    let login_cookie = form.set_cookie("removarr_login_csrf").unwrap();
    for attr in [
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/login",
        "Max-Age=3600",
    ] {
        assert!(login_cookie.contains(attr), "{attr}: {login_cookie}");
    }
    assert!(
        !login_cookie.contains(&form.csrf()),
        "the cookie is encrypted, not the raw token"
    );
    let h = &form.headers;
    assert_eq!(
        h.get(header::CONTENT_SECURITY_POLICY).unwrap(),
        removarr_web::auth::CSP
    );
    assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    assert_eq!(h.get(header::REFERRER_POLICY).unwrap(), "same-origin");
    assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-store");

    let ok = c
        .post(
            "/login",
            &format!(
                "csrf_token={}&username={USER}&password={PASSWORD}",
                form.csrf()
            ),
            &[],
        )
        .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(ok.location(), Some("/"));
    let session = ok.set_cookie("removarr_session").unwrap();
    for attr in [
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
        "Max-Age=43200",
    ] {
        assert!(session.contains(attr), "{attr}: {session}");
    }
    assert!(!session.contains("Domain"), "host-only cookie");
    let removal = ok.set_cookie("removarr_login_csrf").unwrap();
    assert!(
        removal.contains("Max-Age=0"),
        "login token consumed: {removal}"
    );
    // Every response carries the headers, including errors and the health probe.
    let probe = c.get("/healthz").await;
    assert_eq!(probe.headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    let missing = c.get("/no/such/page").await;
    assert!(
        missing
            .headers
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some()
    );
}

#[tokio::test]
async fn cookie_secure_can_be_disabled_for_plain_http_lan() {
    let mut s = settings();
    s.cookie_secure = false;
    let w = World::with(s);
    let mut c = w.client("192.0.2.10:5000");
    let r = c.login(USER, PASSWORD).await;
    let session = r.set_cookie("removarr_session").unwrap();
    assert!(!session.contains("Secure"), "{session}");
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Strict"));
}

// ---- sessions and CSRF ---------------------------------------------------------------------------

#[tokio::test]
async fn session_gives_pages_and_commands_with_the_csrf_header_or_field() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    assert_eq!(c.get("/jobs").await.location(), Some("/login"));
    let csrf = c.session().await;
    assert_eq!(csrf.len(), 64);

    let r = c.post("/scan", "", &[("x-csrf-token", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = c.post("/scan", &format!("csrf_token={csrf}"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // Referer is accepted when Origin is absent.
    let r = c
        .post(
            "/scan",
            &format!("csrf_token={csrf}"),
            &[("origin", ""), ("referer", "http://removarr.test/jobs")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // The handler still reads the rebuilt body.
    let r = c
        .post("/locale", &format!("locale=fr&csrf_token={csrf}"), &[])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        r.set_cookies()
            .iter()
            .any(|c| c.starts_with("removarr_locale=fr")),
        "form body reached the handler"
    );
    assert_eq!(w.mutations.count(), 3);
}

#[tokio::test]
async fn destructive_routes_are_refused_without_session_or_csrf() {
    let w = World::new();
    let mut anon = w.client("192.0.2.10:5000");
    let mut c = w.client("192.0.2.11:5000");
    let csrf = c.session().await;
    let mut other = w.client("192.0.2.12:5000");
    let other_csrf = other.session().await;
    assert_ne!(csrf, other_csrf);

    for (uri, form) in COMMANDS {
        let with_token = if form.is_empty() {
            format!("csrf_token={csrf}")
        } else {
            format!("{form}&csrf_token={csrf}")
        };
        // No session at all, even with a token.
        let r = anon
            .post(uri, &with_token, &[("x-csrf-token", &csrf)])
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
        // Session, no token.
        assert_eq!(
            c.post(uri, form, &[]).await.status,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
        // Wrong token, and the token of another session.
        let r = c
            .post(uri, form, &[("x-csrf-token", "0".repeat(64).as_str())])
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        let r = c.post(uri, form, &[("x-csrf-token", &other_csrf)]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        // The header wins over a valid field (no fallback).
        let r = c.post(uri, &with_token, &[("x-csrf-token", "bad")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        // Valid token but cross-origin, or no Origin/Referer at all.
        let r = c
            .post(uri, &with_token, &[("origin", "http://evil.test")])
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        let r = c.post(uri, &with_token, &[("origin", "")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{uri}");
        // A GET on a command path never reaches a handler either.
        assert_ne!(c.get(uri).await.status, StatusCode::OK, "{uri}");
    }
    assert_eq!(
        w.mutations.count(),
        0,
        "nothing was queued, planned or ignored"
    );

    let r = c.post("/scan", "", &[("x-csrf-token", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(w.mutations.count(), 1);
}

#[tokio::test]
async fn logout_requires_csrf_and_ends_the_session() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let csrf = c.session().await;
    assert_eq!(
        c.post("/logout", "", &[]).await.status,
        StatusCode::FORBIDDEN
    );
    let r = c
        .post(
            "/logout",
            "",
            &[("origin", "http://evil.test"), ("x-csrf-token", &csrf)],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        c.get("/jobs").await.status,
        StatusCode::OK,
        "still signed in"
    );

    let stolen = c.clone();
    let r = c.post("/logout", &format!("csrf_token={csrf}"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), Some("/login"));
    assert!(
        r.set_cookie("removarr_session")
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(c.get("/jobs").await.location(), Some("/login"));
    // A copy of the old cookie is dead server-side.
    let mut stolen = stolen;
    assert_eq!(stolen.get("/jobs").await.location(), Some("/login"));
    assert_eq!(w.auth.stats().sessions, 0);
    // HTMX logout answers with HX-Redirect.
    let csrf = c.session().await;
    let r = c
        .post(
            "/logout",
            "",
            &[("x-csrf-token", &csrf), ("hx-request", "true")],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers.get("hx-redirect").unwrap(), "/login");
}

#[tokio::test]
async fn login_form_csrf_is_required() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let creds = format!("username={USER}&password={PASSWORD}");

    // No login cookie.
    let r = c
        .post(
            "/login",
            &format!("csrf_token={}&{creds}", "a".repeat(64)),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Cookie present, field missing or different.
    let token = c.get("/login").await.csrf();
    assert_eq!(
        c.post("/login", &creds, &[]).await.status,
        StatusCode::FORBIDDEN
    );
    let token2 = c.get("/login").await.csrf();
    let r = c
        .post("/login", &format!("csrf_token={token}&{creds}"), &[])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "superseded token");
    // Repeated field is ambiguous.
    let token3 = c.get("/login").await.csrf();
    let r = c
        .post(
            "/login",
            &format!("csrf_token={token3}&csrf_token={token3}&{creds}"),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Expired token.
    let token4 = c.get("/login").await.csrf();
    w.clock.advance_millis(H);
    let r = c
        .post("/login", &format!("csrf_token={token4}&{creds}"), &[])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Cross-origin login.
    let token5 = c.get("/login").await.csrf();
    let r = c
        .post(
            "/login",
            &format!("csrf_token={token5}&{creds}"),
            &[("origin", "http://evil.test")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let _ = token2;
    assert_eq!(
        w.auth.stats().password_checks,
        0,
        "no hashing without a valid form"
    );
    assert_eq!(w.auth.stats().sessions, 0);
    // None of those consumed the rate limit.
    let r = c.login(USER, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn login_rotates_the_session_id_and_revokes_the_presented_one() {
    let w = World::new();
    // An attacker obtains a valid session and plants its cookie in the victim's browser.
    let mut attacker = w.client("198.51.100.66:4000");
    attacker.session().await;
    let planted = attacker.cookie("removarr_session").unwrap();

    let mut victim = w.client("192.0.2.10:5000");
    victim
        .cookies
        .insert("removarr_session".into(), (planted.clone(), "/".into()));
    let r = victim.login(USER, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let fresh = victim.cookie("removarr_session").unwrap();
    assert_ne!(fresh, planted, "a new id is issued on login");
    assert_eq!(victim.get("/jobs").await.status, StatusCode::OK);
    assert_eq!(
        attacker.get("/jobs").await.location(),
        Some("/login"),
        "the presented id was revoked"
    );

    // Forged or tampered cookies are ignored.
    let mut forger = w.client("192.0.2.20:5000");
    forger
        .cookies
        .insert("removarr_session".into(), ("a".repeat(64), "/".into()));
    assert_eq!(forger.get("/jobs").await.location(), Some("/login"));
    let mut tampered = fresh.clone();
    tampered.replace_range(..1, if fresh.starts_with('A') { "B" } else { "A" });
    forger
        .cookies
        .insert("removarr_session".into(), (tampered, "/".into()));
    assert_eq!(forger.get("/jobs").await.location(), Some("/login"));
}

#[tokio::test]
async fn sessions_expire_on_idle_and_on_absolute_lifetime() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    c.session().await;
    w.clock.advance_millis(2 * H - 1);
    assert_eq!(c.get("/jobs").await.status, StatusCode::OK);
    w.clock.advance_millis(2 * H);
    assert_eq!(c.get("/jobs").await.location(), Some("/login"), "idle 2 h");
    assert_eq!(w.auth.stats().sessions, 0);

    // Activity keeps it alive until the absolute lifetime (12 h).
    let mut c = w.client("192.0.2.11:5000");
    c.session().await;
    for _ in 0..11 {
        w.clock.advance_millis(H);
        assert_eq!(c.get("/jobs").await.status, StatusCode::OK);
    }
    w.clock.advance_millis(H);
    assert_eq!(
        c.get("/jobs").await.location(),
        Some("/login"),
        "12 h absolute"
    );
    // HTMX requests get 401 + HX-Redirect instead of a 303.
    let r = c
        .send(
            Request::get("/jobs")
                .header(header::HOST, HOST)
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers.get("hx-redirect").unwrap(), "/login");
}

#[tokio::test]
async fn revocation_ends_every_session() {
    let w = World::new();
    let mut a = w.client("192.0.2.10:5000");
    let mut b = w.client("192.0.2.11:5000");
    a.session().await;
    b.session().await;
    assert_eq!(w.auth.revoke_all(), 2);
    assert_eq!(a.get("/jobs").await.location(), Some("/login"));
    assert_eq!(b.get("/jobs").await.location(), Some("/login"));
}

// ---- generic errors and rate limits ------------------------------------------------------------

#[tokio::test]
async fn authentication_failures_are_generic() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let wrong_password = c.login(USER, "nope").await;
    let unknown_user = c.login("nobody", PASSWORD).await;
    let both = c.login("nobody", "nope").await;
    let empty = c.login("", "").await;
    for r in [&wrong_password, &unknown_user, &both, &empty] {
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(r.body.contains("Sign-in failed"), "{}", r.body);
        assert!(r.set_cookie("removarr_session").is_none());
        assert!(!r.body.contains("nobody") && !r.body.contains(PASSWORD));
    }
    let reference = normalized(&wrong_password);
    assert_eq!(normalized(&unknown_user), reference);
    assert_eq!(normalized(&both), reference);
    assert_eq!(normalized(&empty), reference);
    assert_eq!(
        w.auth.stats().password_checks,
        3,
        "Argon2 runs for unknown names too (no timing oracle)"
    );
}

#[tokio::test]
async fn rate_limit_per_ip_and_user_answers_429_with_retry_after_before_hashing() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    for _ in 0..5 {
        assert_eq!(
            c.login(USER, "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let checks = w.auth.stats().password_checks;
    // Even the right password is refused while limited, without hashing.
    let r = c.login(USER, PASSWORD).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.retry_after(), Some(900));
    assert!(r.body.contains("Too many attempts") && r.body.contains("900"));
    assert!(r.set_cookie("removarr_session").is_none());
    assert_eq!(w.auth.stats().password_checks, checks);
    // Case and spacing variants share the key.
    assert_eq!(
        c.login(" ADMIN ", "x").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    w.clock.advance_millis(10 * MIN);
    assert_eq!(c.login(USER, PASSWORD).await.retry_after(), Some(300));
    w.clock.advance_millis(5 * MIN);
    assert_eq!(c.login(USER, PASSWORD).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn limits_are_per_ip_and_independent_across_ips_and_users() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    for user in ["a", "b", "c", "d"] {
        for _ in 0..5 {
            assert_eq!(
                c.login(user, "wrong").await.status,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    // The IP budget (20) is spent, for any identifier.
    let r = c.login("e", "wrong").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.retry_after().unwrap() >= 1);
    // The account is not locked: another client signs in as the same user.
    let mut other = w.client("198.51.100.7:6000");
    assert_eq!(
        other.login(USER, PASSWORD).await.status,
        StatusCode::SEE_OTHER
    );
    // A different user name on a fresh IP is unaffected by another IP's failures.
    let mut third = w.client("203.0.113.5:6000");
    assert_eq!(
        third.login("a", "wrong").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn forwarded_for_is_ignored_from_untrusted_peers() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    for i in 0..5 {
        c.xff = Some(format!("203.0.113.{i}"));
        assert_eq!(
            c.login(USER, "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    c.xff = Some("203.0.113.99".into());
    assert_eq!(
        c.login(USER, PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "a rotating X-Forwarded-For does not escape the peer's bucket"
    );
}

#[tokio::test]
async fn forwarded_for_from_a_trusted_proxy_identifies_clients() {
    let mut s = settings();
    s.trusted_proxies = TrustedProxies::parse(&["10.0.0.0/8"]).unwrap();
    let w = World::with(s);
    let mut proxy = w.client("10.0.0.1:443");
    proxy.xff = Some("6.6.6.6, 198.51.100.7".into());
    for _ in 0..5 {
        assert_eq!(
            proxy.login(USER, "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    // Client-controlled left entries change; the proxy-appended address does not.
    proxy.xff = Some("1.1.1.1, 198.51.100.7".into());
    assert_eq!(
        proxy.login(USER, PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Another client behind the same proxy is independent.
    proxy.xff = Some("198.51.100.8".into());
    assert_eq!(
        proxy.login(USER, PASSWORD).await.status,
        StatusCode::SEE_OTHER
    );
    // A trusted proxy sets X-Forwarded-Host; Origin is checked against it.
    let csrf = proxy.get("/jobs").await.csrf();
    let r = proxy
        .post(
            "/scan",
            "",
            &[
                ("x-csrf-token", &csrf),
                ("x-forwarded-host", "media.example.test"),
                ("origin", "https://media.example.test"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    // The same header from an untrusted peer is ignored.
    let mut direct = w.client("192.0.2.30:5000");
    let csrf = direct.session().await;
    let r = direct
        .post(
            "/scan",
            "",
            &[
                ("x-csrf-token", &csrf),
                ("x-forwarded-host", "media.example.test"),
                ("origin", "https://media.example.test"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

// ---- concurrency and bounded state -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_attempts_cannot_exceed_the_limit() {
    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let token = c.get("/login").await.csrf();
    let mut tasks = Vec::new();
    for _ in 0..40 {
        let mut c = c.clone();
        let body = format!("csrf_token={token}&username={USER}&password=wrong");
        tasks.push(tokio::spawn(async move {
            c.post("/login", &body, &[]).await.status
        }));
    }
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for t in tasks {
        *counts.entry(t.await.unwrap().as_u16()).or_default() += 1;
    }
    assert_eq!(counts.get(&401), Some(&5), "{counts:?}");
    assert_eq!(counts.get(&429), Some(&35), "{counts:?}");
    assert_eq!(w.auth.stats().password_checks, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_logins_keep_the_session_table_bounded() {
    let w = World::new();
    let mut tasks = Vec::new();
    for i in 0..40u8 {
        let mut c = w.client(&format!("192.0.2.{}:5000", i + 1));
        tasks.push(tokio::spawn(async move {
            let r = c.login(USER, PASSWORD).await;
            (r.status, c.cookie("removarr_session"))
        }));
    }
    let mut ids = BTreeSet::new();
    for t in tasks {
        let (status, cookie) = t.await.unwrap();
        assert_eq!(status, StatusCode::SEE_OTHER);
        ids.insert(cookie.unwrap());
    }
    assert_eq!(ids.len(), 40, "every login got its own id");
    assert_eq!(
        w.auth.stats().sessions,
        32,
        "bounded table (least recently used evicted)"
    );
}

#[tokio::test]
async fn limiter_state_is_cleaned_after_the_window() {
    let w = World::new();
    for i in 0..50u8 {
        let mut c = w.client(&format!("198.51.100.{i}:5000"));
        assert_eq!(
            c.login(USER, "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(w.auth.stats().limiter_keys, 100);
    w.clock.advance_millis(15 * MIN);
    let mut c = w.client("192.0.2.1:5000");
    assert_eq!(
        c.login(USER, "wrong").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        w.auth.stats().limiter_keys,
        2,
        "expired windows were purged by the next attempt"
    );
}

// ---- logs ----------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn security_events_never_contain_credentials_or_cookies() {
    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    // Process-wide (a thread-local default races with the callsite interest cache of tests
    // running in parallel). Every test of this binary then logs here too, which only widens
    // the "no secret" check below.
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let w = World::new();
    let mut c = w.client("192.0.2.10:5000");
    let form = c.get("/login").await;
    let login_token = form.csrf();
    let login_cookie = c.cookie("removarr_login_csrf").unwrap();
    c.login("probe-user-name", "probe-wrong-password").await;
    let csrf = c.session().await;
    let session_cookie = c.cookie("removarr_session").unwrap();
    c.post("/scan", "", &[("x-csrf-token", "forged-token-value")])
        .await;
    c.post(
        "/scan",
        "",
        &[("x-csrf-token", &csrf), ("origin", "http://evil.test")],
    )
    .await;
    for _ in 0..6 {
        c.login(USER, "probe-wrong-password").await;
    }
    c.post("/logout", "", &[("x-csrf-token", &csrf)]).await;

    let logs = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    for event in [
        "login_failed",
        "login_succeeded",
        "csrf_rejected",
        "origin_rejected",
        "login_rate_limited",
        "logout",
    ] {
        assert!(logs.contains(event), "missing event {event}:\n{logs}");
    }
    assert!(logs.contains("192.0.2.10"), "events carry the client IP");
    for secret in [
        PASSWORD,
        "probe-wrong-password",
        "probe-user-name",
        "forged-token-value",
        login_token.as_str(),
        login_cookie.as_str(),
        csrf.as_str(),
        session_cookie.as_str(),
        fake_hash().as_str(),
        "fake-session-key-for-tests-only-",
    ] {
        assert!(!logs.contains(secret), "secret leaked into logs: {secret}");
    }
}

#[test]
fn invalid_settings_refuse_to_start() {
    use removarr_web::auth::AuthSetupError;
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::default());
    let mut s = settings();
    s.password_hash = Secret::new("not-a-phc-string");
    assert_eq!(
        Auth::new(s, clock.clone()).unwrap_err(),
        AuthSetupError::InvalidPasswordHash
    );
    let mut s = settings();
    s.session_key = Secret::new("short");
    let e = Auth::new(s, clock.clone()).unwrap_err();
    assert_eq!(e, AuthSetupError::SessionKeyTooShort);
    assert!(
        !e.to_string().contains("short"),
        "names the setting, not the value"
    );
    let mut s = settings();
    s.username = " ".into();
    assert_eq!(
        Auth::new(s, clock.clone()).unwrap_err(),
        AuthSetupError::InvalidUsername
    );
    let mut s = settings();
    s.login_limit_per_ip_user = 0;
    assert_eq!(
        Auth::new(s, clock).unwrap_err(),
        AuthSetupError::InvalidLimit
    );
}
