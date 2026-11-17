//! `GET /login`, `POST /login`, `POST /logout` (public group; logout runs the full command
//! check itself: session, origin, CSRF).
//!
//! `POST /login` order: same origin -> login CSRF double-submit -> rate-limit reservation ->
//! length checks -> Argon2 (always run for a well-formed attempt, whatever the user name, so
//! timing does not reveal whether the name exists) -> rotate the session. Every credential
//! failure is the same `401` page with the generic key `login.failed`.

use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

use super::password::MAX_PASSWORD_BYTES;
use super::request::{self, CSRF_FIELD};
use super::{
    Auth, LOGIN_COOKIE, LOGIN_TOKEN_MS, MAX_USERNAME_BYTES, SECURITY_TARGET, SESSION_COOKIE, lock,
};
use crate::error::WebError;
use crate::handlers::login::{LoginPage, LoginView};
use crate::handlers::{Chrome, Ctx, render};
use crate::i18n::Tr;
use crate::state::{Access, AppState, Denied};

/// Largest accepted login body.
pub const LOGIN_BODY_LIMIT: usize = 8 * 1024;
/// Generic credential failure key.
pub const LOGIN_FAILED_KEY: &str = "login.failed";

pub(super) fn routes(auth: Auth) -> Router<AppState> {
    Router::new()
        .route("/login", get(page).post(submit))
        .route("/logout", post(logout))
        .with_state(auth)
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("hx-request")
        .is_some_and(|v| v.as_bytes() == b"true")
}

/// 303 to `to`, or `200` + `HX-Redirect` for HTMX.
fn go(to: &'static str, htmx: bool) -> Response {
    if htmx {
        let mut r = StatusCode::OK.into_response();
        r.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static(to));
        r
    } else {
        Redirect::to(to).into_response()
    }
}

impl Auth {
    /// The login form with a fresh pre-session CSRF token (cookie + hidden field).
    fn form(
        &self,
        tr: Tr,
        headers: &HeaderMap,
        status: StatusCode,
        error_key: Option<&'static str>,
        retry_after: Option<u64>,
    ) -> Response {
        let Some(token) = request::random32().map(|b| request::hex(&b)) else {
            return WebError::internal("no randomness for the login token").into_response();
        };
        let cookie = self.cookie(
            LOGIN_COOKIE,
            format!("{token}.{}", self.now()),
            "/login",
            LOGIN_TOKEN_MS / 1000,
        );
        let ctx = Ctx {
            tr,
            htmx: false,
            chrome: Chrome {
                csrf: token,
                user: String::new(),
                dry_run: false,
                lang: tr.locale.as_str(),
            },
            flash: None,
            path: "/login".to_string(),
        };
        let v = LoginView {
            error_key,
            retry_after,
        };
        let page = LoginPage { ctx: &ctx, v: &v };
        let mut resp = render(&ctx, &page, &page);
        if resp.status().is_success() {
            *resp.status_mut() = status;
        }
        if let Some(s) = retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(s));
        }
        (self.jar(headers).add(cookie), resp).into_response()
    }

    /// The login cookie holds `token.issued_ms`; the field must equal the token and the token
    /// must be younger than [`LOGIN_TOKEN_MS`].
    fn login_token_ok(&self, headers: &HeaderMap, presented: Option<&str>, now: i64) -> bool {
        let Some(c) = self.jar(headers).get(LOGIN_COOKIE) else {
            return false;
        };
        let Some((token, issued)) = c.value().split_once('.') else {
            return false;
        };
        let fresh = issued
            .parse::<i64>()
            .is_ok_and(|t| now < t.saturating_add(LOGIN_TOKEN_MS));
        fresh
            && !token.is_empty()
            && presented.is_some_and(|p| request::ct_eq(p.as_bytes(), token.as_bytes()))
    }
}

/// `GET /login`.
async fn page(State(auth): State<Auth>, tr: Tr, headers: HeaderMap) -> Response {
    auth.form(tr, &headers, StatusCode::OK, None, None)
}

/// `POST /login`.
async fn submit(State(auth): State<Auth>, tr: Tr, req: Request) -> Response {
    let now = auth.now();
    let ip = request::client_ip(&req, &auth.inner.proxies);
    let trusted = request::peer_is_trusted(&req, &auth.inner.proxies);
    // Misconfiguration hint, logged once: forwarded headers from a peer that is not a trusted
    // proxy are ignored (fail-closed), so behind a reverse proxy every client then shares the
    // proxy's rate-limit bucket. The header is never trusted here.
    if !trusted
        && req.headers().contains_key("x-forwarded-for")
        && !auth.inner.forwarded_warned.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(target: SECURITY_TARGET, event = "forwarded_header_untrusted_peer", peer_ip = %ip, "X-Forwarded-For received from a peer that is not in REMOVARR_TRUSTED_PROXIES and was ignored; behind a reverse proxy, set REMOVARR_TRUSTED_PROXIES to the proxy address or network, otherwise all clients share one login rate-limit bucket");
    }
    let forbidden = |auth: &Auth, headers: &HeaderMap| {
        auth.form(
            tr,
            headers,
            StatusCode::FORBIDDEN,
            Some("error.forbidden"),
            None,
        )
    };
    if !request::same_origin(req.headers(), req.uri(), trusted) {
        tracing::warn!(target: SECURITY_TARGET, event = "origin_rejected", client_ip = %ip, "cross-origin or unverifiable login refused");
        return forbidden(&auth, req.headers());
    }
    let (parts, body) = req.into_parts();
    let headers = parts.headers;
    let Ok(body) = axum::body::to_bytes(body, LOGIN_BODY_LIMIT).await else {
        return WebError::status(StatusCode::PAYLOAD_TOO_LARGE, "error.too_large").into_response();
    };
    let field = |name| request::form_value(&body, name);
    let (Ok(token), Ok(user), Ok(password)) =
        (field(CSRF_FIELD), field("username"), field("password"))
    else {
        tracing::warn!(target: SECURITY_TARGET, event = "csrf_rejected", client_ip = %ip, "malformed login form refused");
        return forbidden(&auth, &headers);
    };
    if !auth.login_token_ok(&headers, token.as_deref(), now) {
        tracing::warn!(target: SECURITY_TARGET, event = "csrf_rejected", client_ip = %ip, "login without a valid form token refused");
        return forbidden(&auth, &headers);
    }
    let user = user.unwrap_or_default();
    let password = password.unwrap_or_default();

    // Rate limit before any hashing; the same for known and unknown names.
    if let Err(retry) = lock(&auth.inner.limiter).reserve(ip, &user, now) {
        tracing::warn!(target: SECURITY_TARGET, event = "login_rate_limited", client_ip = %ip, retry_after_secs = retry, "login attempt rate limited");
        return auth.form(
            tr,
            &headers,
            StatusCode::TOO_MANY_REQUESTS,
            Some("error.rate_limited"),
            Some(retry),
        );
    }

    let user = user.trim();
    let well_formed = !user.is_empty()
        && user.len() <= MAX_USERNAME_BYTES
        && !password.is_empty()
        && password.len() <= MAX_PASSWORD_BYTES;
    let ok = if well_formed {
        auth.inner.password_checks.fetch_add(1, Ordering::SeqCst);
        let password_ok = auth.inner.verifier.verify(password).await;
        let user_ok = request::ct_eq(user.as_bytes(), auth.inner.username.as_bytes());
        password_ok & user_ok
    } else {
        false
    };
    if !ok {
        tracing::warn!(target: SECURITY_TARGET, event = "login_failed", client_ip = %ip, "login failed");
        return auth.form(
            tr,
            &headers,
            StatusCode::UNAUTHORIZED,
            Some(LOGIN_FAILED_KEY),
            None,
        );
    }

    // Success: always a new session id; any id the browser presented is revoked (fixation).
    let (Some(id), Some(csrf)) = (request::random32(), request::random32()) else {
        return WebError::internal("no randomness for the session").into_response();
    };
    let previous = auth.session_id(&headers);
    let evicted = {
        let mut sessions = lock(&auth.inner.sessions);
        if let Some(old) = previous {
            sessions.remove(&old);
        }
        sessions.create(id, request::hex(&csrf), auth.now())
    };
    lock(&auth.inner.limiter).succeeded(ip, user);
    if evicted {
        tracing::info!(target: SECURITY_TARGET, event = "session_evicted", "session table full; least recently used session ended");
    }
    tracing::info!(target: SECURITY_TARGET, event = "login_succeeded", client_ip = %ip, "login succeeded");
    let jar = auth
        .jar(&headers)
        .add(auth.cookie(
            SESSION_COOKIE,
            request::hex(&id),
            "/",
            auth.inner.session_ttl_secs,
        ))
        .remove(Auth::removal(LOGIN_COOKIE, "/login"));
    (jar, go("/", is_htmx(&headers))).into_response()
}

/// `POST /logout`: a command (session + origin + CSRF), then the session is removed.
async fn logout(State(auth): State<Auth>, req: Request) -> Response {
    let headers = req.headers().clone();
    match auth.authorize(Access::Command, req).await {
        Ok((_, _, id)) => {
            lock(&auth.inner.sessions).remove(&id);
            tracing::info!(target: SECURITY_TARGET, event = "logout", "session ended by logout");
            let jar = auth
                .jar(&headers)
                .remove(Auth::removal(SESSION_COOKIE, "/"));
            (jar, go("/login", is_htmx(&headers))).into_response()
        }
        Err(Denied::Unauthenticated) => {
            WebError::status(StatusCode::UNAUTHORIZED, "error.unauthorized").into_response()
        }
        Err(_) => WebError::status(StatusCode::FORBIDDEN, "error.forbidden").into_response(),
    }
}
