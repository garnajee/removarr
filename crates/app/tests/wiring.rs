//! Composition-root tests: real configuration, real SQLite in a temp dir, no network, no
//! service. A dry-run process builds no mutating adapter (architecture 6.8(a)); the default
//! guard refuses every protected route; health probes answer from the wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use http_body_util::BodyExt;
use removarr::wiring::{self, App};
use removarr_config::{Config, Env};
use removarr_domain::plan::PlanMode;
use removarr_domain::policy::Capability;
use removarr_reporting::{LogBuffer, LogLevel};
use removarr_web::{Access, Denied, Guard, Principal};
use tempfile::TempDir;
use tower::ServiceExt;

const DUMMY_SESSION_KEY: &str =
    "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";

struct Fixture {
    _tmp: TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for d in ["completed", "media", "data"] {
            fs::create_dir(base.join(d)).unwrap();
        }
        Self { _tmp: tmp, base }
    }

    fn env(&self) -> Env {
        let p = |d: &str| self.base.join(d).to_str().unwrap().to_string();
        let mut e = Env::new();
        e.insert("REMOVARR_COMPLETED_ROOT".into(), p("completed"));
        e.insert("REMOVARR_MEDIA_ROOTS".into(), p("media"));
        e.insert("REMOVARR_DATA_DIR".into(), p("data"));
        e.insert("REMOVARR_PASSWORD_HASH".into(), DUMMY_HASH.into());
        e.insert("REMOVARR_SESSION_KEY".into(), DUMMY_SESSION_KEY.into());
        e
    }

    fn tree(&self) -> Vec<(PathBuf, std::time::SystemTime)> {
        let mut out = Vec::new();
        for d in ["completed", "media"] {
            let p = self.base.join(d);
            out.push((p.clone(), fs::metadata(&p).unwrap().modified().unwrap()));
        }
        out
    }
}

struct AllowGuard;

#[async_trait]
impl Guard for AllowGuard {
    async fn check(&self, _a: Access, req: Request) -> Result<(Request, Principal), Denied> {
        Ok((
            req,
            Principal {
                user: "admin".into(),
                csrf_token: "t".into(),
            },
        ))
    }
}

async fn build(env: &Env, guard: Arc<dyn Guard>) -> App {
    let cfg = Config::from_sources(None, env).unwrap();
    wiring::build(&cfg, LogBuffer::new(100, LogLevel::Info), guard)
        .await
        .unwrap()
}

async fn call(app: &App, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = removarr_web::build(app.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let (s, h) = (resp.status(), resp.headers().clone());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (s, h, String::from_utf8_lossy(&body).into_owned())
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, form: &str) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

#[tokio::test]
async fn a_dry_run_process_builds_no_mutating_adapter() {
    let f = Fixture::new();
    let app = build(&f.env(), wiring::default_guard()).await;
    assert_eq!(app.report.mode, PlanMode::DryRun);
    assert!(!app.report.live_adapters);
    assert!(!app.report.auto_mode, "automatic mode is off by default");
    assert!(app.report.services.is_empty(), "no service configured");
    assert!(app.report.capabilities.contains(&Capability::Consolidate));
    assert!(!app.report.capabilities.contains(&Capability::ArrGrab));
    assert!(app.state.settings.dry_run);
    assert!(f.base.join("data").join(wiring::DB_FILE).exists());
}

#[tokio::test]
async fn a_live_configuration_builds_the_live_set_and_touches_no_root() {
    let f = Fixture::new();
    let before = f.tree();
    let mut env = f.env();
    env.insert("REMOVARR_DRY_RUN".into(), "false".into());
    let app = build(&env, wiring::default_guard()).await;
    assert_eq!(app.report.mode, PlanMode::Live);
    assert!(app.report.live_adapters);
    assert_eq!(
        f.tree(),
        before,
        "building adapters writes nothing in a root"
    );
    assert!(
        fs::read_dir(f.base.join("completed"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn the_production_wiring_fails_closed_and_answers_health_probes() {
    let f = Fixture::new();
    let app = build(&f.env(), wiring::default_guard()).await;
    let (s, _, body) = call(&app, get("/healthz")).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "ok"));
    let (s, _, body) = call(&app, get("/readyz")).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "ready"));
    let (s, h, _) = call(&app, get("/")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h.get(header::LOCATION).unwrap(), "/login");
    for (uri, form) in [
        ("/scan", ""),
        (
            "/plans",
            "category=junk&action=delete&issue_ids=0123456789abcdef0123456789abcdef",
        ),
        ("/plans/00000000-0000-0000-0000-000000000001/execute", ""),
        ("/auto/resume", ""),
    ] {
        assert_eq!(
            call(&app, post(uri, form)).await.0,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    assert!(
        app.state.jobs.status().jobs.is_empty(),
        "nothing was queued"
    );
}

#[tokio::test]
async fn real_routes_over_an_empty_database_are_safe() {
    let f = Fixture::new();
    let app = build(&f.env(), Arc::new(AllowGuard)).await;
    for uri in [
        "/",
        "/issues/junk",
        "/ignored",
        "/history",
        "/stats",
        "/logs",
        "/jobs",
        "/auto",
    ] {
        let (s, _, body) = call(&app, get(uri)).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {body}");
        assert!(
            !body.contains(f.base.to_str().unwrap()),
            "{uri} leaks a host path"
        );
    }
    // No scan yet: planning is refused as a conflict, safely.
    let (s, _, body) = call(
        &app,
        post(
            "/plans",
            "category=junk&action=delete&issue_ids=0123456789abcdef0123456789abcdef",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("No completed scan"));
    // Unknown plans are not found; a dry-run process never executes live.
    let (s, ..) = call(
        &app,
        post("/plans/00000000-0000-0000-0000-000000000001/execute", ""),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, ..) = call(&app, get("/plans/00000000-0000-0000-0000-000000000001")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(app.state.jobs.status().jobs.is_empty());
}

#[test]
fn invalid_configuration_exits_with_code_two_and_names_keys_only() {
    let f = Fixture::new();
    let mut env = f.env();
    env.remove("REMOVARR_COMPLETED_ROOT");
    env.insert("REMOVARR_SESSION_KEY".into(), "short-secret-value".into());
    let e = Config::from_sources(None, &env).unwrap_err();
    assert_eq!(e.exit_code(), 2);
    let text = e.to_string();
    assert!(text.contains("roots.completed"));
    assert!(!text.contains("short-secret-value"));
}
