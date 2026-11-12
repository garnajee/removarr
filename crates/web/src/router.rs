//! Route table and cross-cutting layers (architecture 12.1, 12.3).
//!
//! Three groups:
//! - public: `/healthz`, `/readyz`, `/login`;
//! - display (`GET`): behind the guard with [`Access::Page`]; they never mutate;
//! - commands (`POST`): behind the guard with [`Access::Command`] (session, CSRF, origin) and
//!   the [`CommandGrant`] the handlers require. With the default [`crate::state::FailClosedGuard`]
//!   every protected route is refused.
//!
//! Layers: body limit (1 MiB), request-target limit, 30 s timeout, panic catcher (generic 500),
//! security headers ([`crate::auth::security_headers`]). The login routes come from the guard
//! ([`crate::state::Guard::routes`]); the session/CSRF implementation is [`crate::auth`] (17).

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::error::WebError;
use crate::forms::{MAX_BODY_BYTES, MAX_URI_BYTES};
use crate::handlers::{self, CommandGrant};
use crate::health;
use crate::state::{Access, AppState, Denied};

/// Whole-request timeout (architecture 12.3).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn denied(d: Denied, access: Access, htmx: bool) -> Response {
    match (d, access) {
        (Denied::Unauthenticated, Access::Page) => {
            if htmx {
                let mut r = StatusCode::UNAUTHORIZED.into_response();
                r.headers_mut()
                    .insert("hx-redirect", HeaderValue::from_static("/login"));
                r
            } else {
                Redirect::to("/login").into_response()
            }
        }
        (Denied::Unauthenticated, Access::Command) => {
            WebError::status(StatusCode::UNAUTHORIZED, "error.unauthorized").into_response()
        }
        (Denied::Forbidden, _) => {
            WebError::status(StatusCode::FORBIDDEN, "error.forbidden").into_response()
        }
        (Denied::RateLimited { retry_after_secs }, _) => {
            let mut e = WebError::status(StatusCode::TOO_MANY_REQUESTS, "error.rate_limited");
            e.retry_after = Some(retry_after_secs);
            e.into_response()
        }
    }
}

async fn guarded(s: &AppState, access: Access, req: Request, next: Next) -> Response {
    let htmx = req
        .headers()
        .get("hx-request")
        .is_some_and(|v| v.as_bytes() == b"true");
    match s.guard.check(access, req).await {
        Ok((mut req, principal)) => {
            req.extensions_mut().insert(principal);
            if access == Access::Command {
                req.extensions_mut().insert(CommandGrant::new());
            } else {
                // A display route can never carry a command grant, whatever the guard did.
                req.extensions_mut().remove::<CommandGrant>();
            }
            next.run(req).await
        }
        Err(d) => denied(d, access, htmx),
    }
}

async fn page_guard(State(s): State<AppState>, req: Request, next: Next) -> Response {
    guarded(&s, Access::Page, req, next).await
}

async fn command_guard(State(s): State<AppState>, req: Request, next: Next) -> Response {
    guarded(&s, Access::Command, req, next).await
}

/// Refuses oversized request targets before any routing (414).
async fn uri_limit(req: Request, next: Next) -> Response {
    let len = req.uri().path_and_query().map_or(0, |pq| pq.as_str().len());
    if len > MAX_URI_BYTES {
        return WebError::status(StatusCode::URI_TOO_LONG, "error.too_large").into_response();
    }
    next.run(req).await
}

async fn not_found() -> WebError {
    WebError::not_found()
}

fn panic_response(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a request handler panicked");
    let mut r =
        WebError::status(StatusCode::INTERNAL_SERVER_ERROR, "error.internal").into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The application router.
pub fn build(state: AppState) -> Router {
    use handlers::{dashboard, issues, jobs, login, ops, plans, reports};

    let login_routes = state
        .guard
        .routes()
        .unwrap_or_else(|| Router::new().route("/login", get(login::page)));
    let public = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .merge(login_routes);

    let pages = Router::new()
        .route("/", get(dashboard::show))
        .route("/issues/{category}", get(issues::list))
        .route("/issues/{category}/{issue_id}", get(issues::detail))
        .route("/ignored", get(issues::ignored))
        .route("/plans/{id}", get(plans::show))
        .route("/plans/{id}/confirm", get(plans::confirm))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::show))
        .route("/jobs/{id}/progress", get(jobs::progress))
        .route("/auto", get(ops::auto_status))
        .route("/history", get(reports::history))
        .route("/stats", get(reports::stats))
        .route("/logs", get(reports::logs))
        .route("/export/{session}/{format}", get(reports::export))
        .route_layer(middleware::from_fn_with_state(state.clone(), page_guard));

    let commands = Router::new()
        .route("/scan", post(ops::scan))
        .route("/services/{service}/refresh", post(ops::refresh))
        .route("/services/{service}/rescan", post(ops::rescan))
        .route("/issues/selection-summary", post(issues::selection_summary))
        .route("/issues/ignore", post(issues::ignore))
        .route("/issues/restore", post(issues::restore))
        .route("/issues/evaluate", post(issues::evaluate))
        .route("/issues/verify-content", post(issues::verify_content))
        .route("/plans", post(plans::create))
        .route("/plans/preview", post(plans::preview))
        .route("/plans/{id}/approve", post(plans::approve))
        .route("/plans/{id}/execute", post(plans::execute))
        .route("/plans/{id}/simulate", post(plans::simulate))
        .route("/plans/{id}/discard", post(plans::discard))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/recovery/retry", post(ops::retry_recovery))
        .route("/auto/resume", post(ops::resume_auto))
        .route("/locale", post(ops::locale))
        .route_layer(middleware::from_fn_with_state(state.clone(), command_guard));

    let app = public
        .merge(pages)
        .merge(commands)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(middleware::from_fn(uri_limit))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(CatchPanicLayer::custom(panic_response))
        .with_state(state);
    crate::auth::security_headers(app)
}
