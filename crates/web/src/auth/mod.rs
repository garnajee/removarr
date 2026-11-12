//! Authentication and request security (architecture 12.3, ADR-0006; agent 17).
//!
//! [`Auth`] is the production [`Guard`]:
//! - single-user form login: the configured user name (constant-time comparison) and the
//!   Argon2 PHC hash from `REMOVARR_PASSWORD_HASH` ([`password`]);
//! - sessions: a random 32-byte id in the encrypted `removarr_session` cookie
//!   (`PrivateCookieJar`, AES-GCM, key derived from `REMOVARR_SESSION_KEY`), `HttpOnly`,
//!   `SameSite=Strict`, `Path=/`, `Secure` per `auth.cookie_secure`; a bounded in-memory table
//!   ([`session`]) with absolute and idle expiry; a new id on every login (fixation), removal
//!   on logout, expiry and revocation;
//! - CSRF: a per-session synchronizer token (hidden field `csrf_token` or header
//!   `X-CSRF-Token`) plus an `Origin`/`Referer` check on every command; `/login` uses an
//!   encrypted pre-session double-submit cookie;
//! - login rate limit ([`limiter`]) per client IP and per `(IP, normalized user)`, reserved
//!   before Argon2; generic failures; `429` with `Retry-After`;
//! - client IP from `X-Forwarded-For` only when the peer is in `server.trusted_proxies`;
//! - security events on the `removarr::security` target with the client IP and an event
//!   name only: never a user name, password, cookie, token or hash.
//!
//! [`security_headers`] adds CSP, `nosniff`, `Referrer-Policy` and `X-Frame-Options` to every
//! response.

pub mod limiter;
mod login;
pub mod password;
pub mod request;
pub mod session;

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum_extra::extract::PrivateCookieJar;
use axum_extra::extract::cookie::{Cookie, Key, SameSite};
use removarr_config::{Config, Secret, TrustedProxies};
use removarr_domain::ports::Clock;
use sha2::{Digest, Sha512};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::state::{Access, AppState, Denied, Guard, Principal};
use limiter::{Limits, LoginLimiter};
use password::Verifier;
use session::{Lookup, SessionId, SessionStore};

pub use password::{HashError, hash_password};

/// Session cookie (encrypted session id).
pub const SESSION_COOKIE: &str = "removarr_session";
/// Pre-session login CSRF cookie (encrypted `token.issued_ms`), scoped to `/login`.
pub const LOGIN_COOKIE: &str = "removarr_login_csrf";
/// Lifetime of a login form token.
pub const LOGIN_TOKEN_MS: i64 = 3_600_000;
/// Minimum length of `REMOVARR_SESSION_KEY` (architecture 11).
pub const MIN_SESSION_KEY_CHARS: usize = 64;
/// Maximum accepted user name length in bytes.
pub const MAX_USERNAME_BYTES: usize = 128;
/// Target of security events.
pub const SECURITY_TARGET: &str = "removarr::security";

/// `Content-Security-Policy` of every response (architecture 12.3).
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Everything the guard needs from the configuration.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub username: String,
    /// Argon2 PHC string.
    pub password_hash: Secret,
    pub session_key: Secret,
    pub session_ttl_hours: u32,
    pub cookie_secure: bool,
    pub login_limit_per_ip: u32,
    pub login_limit_per_ip_user: u32,
    pub trusted_proxies: TrustedProxies,
}

impl AuthSettings {
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            username: cfg.auth.username.clone(),
            password_hash: cfg.auth.password_hash.clone(),
            session_key: cfg.auth.session_key.clone(),
            session_ttl_hours: cfg.auth.session_ttl_hours,
            cookie_secure: cfg.auth.cookie_secure,
            login_limit_per_ip: cfg.auth.login_limit_per_ip,
            login_limit_per_ip_user: cfg.auth.login_limit_per_ip_user,
            trusted_proxies: cfg.server.trusted_proxies.clone(),
        }
    }
}

/// The authentication settings cannot be used: the process refuses to start. The message
/// names the setting, never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSetupError {
    InvalidPasswordHash,
    SessionKeyTooShort,
    InvalidUsername,
    InvalidLimit,
}

impl std::error::Error for AuthSetupError {}

impl fmt::Display for AuthSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPasswordHash => {
                "REMOVARR_PASSWORD_HASH is not a valid Argon2 PHC string (use `removarr hash-password`)"
            }
            Self::SessionKeyTooShort => "REMOVARR_SESSION_KEY must be at least 64 characters",
            Self::InvalidUsername => "auth.username must be 1 to 128 bytes",
            Self::InvalidLimit => {
                "auth.session_ttl_hours and auth.login_limit_* must be at least 1"
            }
        })
    }
}

/// Counters for tests and diagnostics (no secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthStats {
    pub sessions: usize,
    pub limiter_keys: usize,
    /// Argon2 verifications started since startup.
    pub password_checks: u64,
}

struct Inner {
    username: String,
    verifier: Verifier,
    key: Key,
    cookie_secure: bool,
    session_ttl_secs: i64,
    proxies: TrustedProxies,
    sessions: Mutex<SessionStore>,
    limiter: Mutex<LoginLimiter>,
    password_checks: std::sync::atomic::AtomicU64,
    clock: Arc<dyn Clock>,
}

/// The production guard. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("cookie_secure", &self.inner.cookie_secure)
            .finish_non_exhaustive()
    }
}

/// Cookie key: SHA-512 over a domain label and the whole secret (64 bytes: signing half and
/// AES-GCM half), so every character of the configured key contributes.
fn derive_key(secret: &Secret) -> Key {
    let mut h = Sha512::new();
    h.update(b"removarr session cookie key v1\0");
    h.update(secret.expose().as_bytes());
    Key::from(&h.finalize())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Auth {
    /// Validates the settings and builds the guard. Any error refuses startup.
    pub fn new(s: AuthSettings, clock: Arc<dyn Clock>) -> Result<Self, AuthSetupError> {
        if s.username.trim().is_empty() || s.username.len() > MAX_USERNAME_BYTES {
            return Err(AuthSetupError::InvalidUsername);
        }
        if s.session_key.expose().chars().count() < MIN_SESSION_KEY_CHARS {
            return Err(AuthSetupError::SessionKeyTooShort);
        }
        if s.session_ttl_hours == 0 || s.login_limit_per_ip == 0 || s.login_limit_per_ip_user == 0 {
            return Err(AuthSetupError::InvalidLimit);
        }
        let verifier = Verifier::new(s.password_hash.expose())
            .map_err(|_| AuthSetupError::InvalidPasswordHash)?;
        let ttl_ms = i64::from(s.session_ttl_hours) * 3_600_000;
        Ok(Self {
            inner: Arc::new(Inner {
                username: s.username,
                verifier,
                key: derive_key(&s.session_key),
                cookie_secure: s.cookie_secure,
                session_ttl_secs: ttl_ms / 1000,
                proxies: s.trusted_proxies,
                sessions: Mutex::new(SessionStore::new(ttl_ms)),
                limiter: Mutex::new(LoginLimiter::new(Limits {
                    per_ip: s.login_limit_per_ip,
                    per_ip_user: s.login_limit_per_ip_user,
                    capacity: limiter::DEFAULT_CAPACITY,
                })),
                password_checks: std::sync::atomic::AtomicU64::new(0),
                clock,
            }),
        })
    }

    fn now(&self) -> i64 {
        self.inner.clock.now().as_millis()
    }

    /// Revokes every session (they must sign in again). Returns how many were removed.
    pub fn revoke_all(&self) -> usize {
        let n = lock(&self.inner.sessions).clear();
        tracing::info!(target: SECURITY_TARGET, event = "sessions_revoked", count = n, "all sessions revoked");
        n
    }

    #[must_use]
    pub fn stats(&self) -> AuthStats {
        AuthStats {
            sessions: lock(&self.inner.sessions).len(),
            limiter_keys: lock(&self.inner.limiter).len(),
            password_checks: self
                .inner
                .password_checks
                .load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    fn cookie(
        &self,
        name: &'static str,
        value: String,
        path: &'static str,
        max_age_secs: i64,
    ) -> Cookie<'static> {
        Cookie::build((name, value))
            .http_only(true)
            .secure(self.inner.cookie_secure)
            .same_site(SameSite::Strict)
            .path(path)
            .max_age(time::Duration::seconds(max_age_secs))
            .build()
    }

    fn removal(name: &'static str, path: &'static str) -> Cookie<'static> {
        Cookie::build((name, "")).path(path).build()
    }

    fn jar(&self, req_headers: &axum::http::HeaderMap) -> PrivateCookieJar {
        PrivateCookieJar::from_headers(req_headers, self.inner.key.clone())
    }

    fn session_id(&self, headers: &axum::http::HeaderMap) -> Option<SessionId> {
        self.jar(headers)
            .get(SESSION_COOKIE)
            .and_then(|c| request::unhex32(c.value()))
    }

    /// Session, then (commands) origin and CSRF. Returns the session id for logout.
    async fn authorize(
        &self,
        access: Access,
        req: Request,
    ) -> Result<(Request, Principal, SessionId), Denied> {
        let now = self.now();
        let Some(id) = self.session_id(req.headers()) else {
            return Err(Denied::Unauthenticated);
        };
        let csrf = match lock(&self.inner.sessions).lookup(&id, now) {
            Lookup::Valid(t) => t,
            Lookup::Expired => {
                tracing::info!(target: SECURITY_TARGET, event = "session_expired", "session expired");
                return Err(Denied::Unauthenticated);
            }
            Lookup::Unknown => return Err(Denied::Unauthenticated),
        };
        let req = if access == Access::Command {
            let ip = request::client_ip(&req, &self.inner.proxies);
            let trusted = request::peer_is_trusted(&req, &self.inner.proxies);
            if !request::same_origin(req.headers(), req.uri(), trusted) {
                tracing::warn!(target: SECURITY_TARGET, event = "origin_rejected", client_ip = %ip, "cross-origin or unverifiable command refused");
                return Err(Denied::Forbidden);
            }
            let (req, presented) = request::presented_token(req).await;
            if !presented.is_some_and(|t| request::ct_eq(t.as_bytes(), csrf.as_bytes())) {
                tracing::warn!(target: SECURITY_TARGET, event = "csrf_rejected", client_ip = %ip, "command without a valid CSRF token refused");
                return Err(Denied::Forbidden);
            }
            req
        } else {
            req
        };
        let principal = Principal {
            user: self.inner.username.clone(),
            csrf_token: csrf,
        };
        Ok((req, principal, id))
    }
}

#[async_trait]
impl Guard for Auth {
    async fn check(&self, access: Access, req: Request) -> Result<(Request, Principal), Denied> {
        self.authorize(access, req).await.map(|(r, p, _)| (r, p))
    }

    fn routes(&self) -> Option<Router<AppState>> {
        Some(login::routes(self.clone()))
    }
}

/// Adds the security headers of architecture 12.3 to every response that does not set them.
pub fn security_headers<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
}
