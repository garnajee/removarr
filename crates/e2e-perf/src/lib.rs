//! End-to-end and performance support (wave 16, agent 23). Tests only.
//!
//! * [`Live`]: the production composition root (via the wave 15 `Stack`) served on
//!   `127.0.0.1:0` with the production `Auth` guard driven by a controllable clock, so expiry and
//!   limiter cleanup are testable over real HTTP.
//! * [`Browser`]: an HTTP client with a cookie jar and form helpers (it follows no redirect).
//! * [`html`]: small markup assertions (no HTML parser dependency).
//! * [`perf`]: synthetic inventory generator and process measurement for the budget benchmarks.
//!
//! Everything binds loopback, writes under a temporary directory, and uses dummy secrets.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

pub mod html;
pub mod perf;

use std::net::SocketAddr;
use std::sync::Arc;

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use removarr_domain::time::Timestamp;
use removarr_integration_tests::{Options, Stack};
use removarr_testkit::clock::FakeClock;
use removarr_web::auth::{Auth, AuthSettings};
use reqwest::{Client, StatusCode, redirect::Policy};

pub const USER: &str = "admin";
pub const PASSWORD: &str = "dummy-only-password-for-tests";

/// Argon2id PHC of [`PASSWORD`] with cheap parameters (fast tests; production parameters are
/// measured separately in `perf_budgets.rs`).
#[must_use]
pub fn cheap_hash() -> String {
    hash_with(Params::new(256, 1, 1, None).unwrap(), PASSWORD)
}

#[must_use]
pub fn hash_with(params: Params, password: &str) -> String {
    let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::encode_b64(b"fixed-test-salt!").unwrap();
    a.hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

/// The production router and guard on a real loopback listener.
pub struct Live {
    pub stack: Stack,
    pub auth: Auth,
    pub clock: Arc<FakeClock>,
    pub base: String,
    _task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").field("base", &self.base).finish()
    }
}

impl Live {
    /// Parity tree, loopback fakes, production wiring, production `Auth` (cheap hash, plain
    /// HTTP cookie) with the given login limits.
    pub async fn start(opts: Options, per_ip: u32, per_ip_user: u32) -> Self {
        let opts = opts
            .env("REMOVARR_PASSWORD_HASH", &cheap_hash())
            .env("REMOVARR_COOKIE_SECURE", "false")
            .env("REMOVARR_LOGIN_LIMIT_PER_IP", &per_ip.to_string())
            .env("REMOVARR_LOGIN_LIMIT_PER_IP_USER", &per_ip_user.to_string())
            .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other");
        let stack = Stack::parity(opts).await;
        Self::over(stack).await
    }

    pub async fn over(stack: Stack) -> Self {
        let clock = Arc::new(FakeClock::new(Timestamp(1_700_000_000_000)));
        let auth = Auth::new(AuthSettings::from_config(&stack.cfg), clock.clone()).unwrap();
        let mut state = stack.app.state.clone();
        state.guard = Arc::new(auth.clone());
        let router = removarr_web::build(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            stack,
            auth,
            clock,
            base: format!("http://{addr}"),
            _task: task,
        }
    }

    #[must_use]
    pub fn browser(&self) -> Browser {
        Browser::new(&self.base)
    }
}

/// A response reduced to what assertions need.
#[derive(Debug, Clone)]
pub struct Page {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}

impl Page {
    #[must_use]
    pub fn location(&self) -> String {
        self.headers
            .get("location")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
    #[must_use]
    pub fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
    /// The whole `Set-Cookie` line of this name (attributes included), if any.
    #[must_use]
    pub fn set_cookie_line(&self, name: &str) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with(&format!("{name}=")))
            .map(ToString::to_string)
    }
    /// The value of a `Set-Cookie` of this name, if any.
    #[must_use]
    pub fn set_cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with(&format!("{name}=")))
            .map(|c| c.split(';').next().unwrap()[name.len() + 1..].to_string())
    }
}

/// One browser: a cookie jar, no redirect following, the loopback origin on every POST.
#[derive(Debug, Clone)]
pub struct Browser {
    pub client: Client,
    pub base: String,
    pub xff: Option<String>,
}

impl Browser {
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            client: Client::builder()
                .redirect(Policy::none())
                .cookie_store(true)
                .no_proxy()
                .build()
                .unwrap(),
            base: base.to_string(),
            xff: None,
        }
    }

    async fn page(r: reqwest::Response) -> Page {
        let status = r.status();
        let headers = r.headers().clone();
        let body = r.text().await.unwrap_or_default();
        Page {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, path: &str) -> Page {
        self.get_with(path, &[]).await
    }

    pub async fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> Page {
        let mut r = self.client.get(format!("{}{path}", self.base));
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        Self::page(r.send().await.unwrap()).await
    }

    /// `POST` with a urlencoded body (given verbatim) and the page's CSRF token as a field.
    pub async fn post(&self, path: &str, body: &str, token: &str) -> Page {
        let sep = if body.is_empty() { "" } else { "&" };
        let body = format!("csrf_token={token}{sep}{body}");
        self.post_raw(path, &body, &[]).await
    }

    pub async fn post_raw(&self, path: &str, body: &str, headers: &[(&str, &str)]) -> Page {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert("Origin", self.base.parse().unwrap());
        map.insert(
            "Content-Type",
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        if let Some(x) = &self.xff {
            map.insert("X-Forwarded-For", x.parse().unwrap());
        }
        for (k, v) in headers {
            map.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .headers(map)
            .body(body.to_string());
        Self::page(r.send().await.unwrap()).await
    }

    /// Fetches `/login`, then posts the credentials with its form token.
    pub async fn login(&self, user: &str, password: &str) -> Page {
        let page = self.get("/login").await;
        let token = html::csrf(&page.body);
        let body = format!("csrf_token={token}&username={user}&password={password}");
        self.post_raw("/login", &body, &[]).await
    }

    /// Polls a job page (`/jobs/<id>`) until it is no longer active.
    pub async fn wait_job(&self, location: &str) {
        let id = location.trim_start_matches("/jobs/");
        let t0 = std::time::Instant::now();
        loop {
            let p = self.get(&format!("/jobs/{id}/progress")).await;
            if !p.body.contains("hx-trigger=\"every 2s\"") {
                return;
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(300),
                "job {id} still active"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// A page's CSRF token (every authenticated page carries it in the "More" sheet).
    pub async fn token(&self) -> String {
        html::csrf(&self.get("/jobs").await.body)
    }
}

/// Askama's HTML escaping of a plain string (to look for translated text in a page).
#[must_use]
pub fn escaped(s: &str) -> String {
    s.replace('&', "&#38;")
        .replace('<', "&#60;")
        .replace('>', "&#62;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}
