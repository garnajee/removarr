//! Task 8 (authentication over a real listener): the production guard (`wiring::auth_guard`) on
//! the production router, served by `axum::serve` with connection info on `127.0.0.1:0`, driven
//! by a real HTTP client with a cookie jar. Login, session, CSRF, origin check, rate limit with
//! `Retry-After`, header spoofing and logout, with a dummy password only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use removarr_integration_tests::*;
use reqwest::{Client, StatusCode, redirect::Policy};

const PASSWORD: &str = "dummy-only-password-for-tests";

fn cheap_hash() -> String {
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

struct Served {
    stack: Stack,
    base: String,
    origin: String,
    _task: tokio::task::JoinHandle<()>,
}

async fn serve() -> Served {
    let s = Stack::parity(
        Options::default()
            .env("REMOVARR_PASSWORD_HASH", &cheap_hash())
            .env("REMOVARR_COOKIE_SECURE", "false")
            .env("REMOVARR_LOGIN_LIMIT_PER_IP", "4")
            .env("REMOVARR_LOGIN_LIMIT_PER_IP_USER", "3"),
    )
    .await;
    // The production guard over the same repositories (the stack's own state uses a test guard).
    let guard = removarr::wiring::auth_guard(&s.cfg).unwrap();
    let mut state = s.app.state.clone();
    state.guard = guard;
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
    Served {
        stack: s,
        base: format!("http://{addr}"),
        origin: format!("http://{addr}"),
        _task: task,
    }
}

fn client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap()
}

fn csrf(body: &str) -> String {
    let m = "name=\"csrf_token\" value=\"";
    let a = body.find(m).expect("csrf field") + m.len();
    body[a..a + body[a..].find('"').unwrap()].to_string()
}

async fn login(c: &Client, sv: &Served, password: &str, xff: Option<&str>) -> reqwest::Response {
    let page = c.get(format!("{}/login", sv.base)).send().await.unwrap();
    let token = csrf(&page.text().await.unwrap());
    let mut r = c
        .post(format!("{}/login", sv.base))
        .header("Origin", &sv.origin)
        .form(&[
            ("csrf_token", token.as_str()),
            ("username", "admin"),
            ("password", password),
        ]);
    if let Some(x) = xff {
        r = r.header("X-Forwarded-For", x);
    }
    r.send().await.unwrap()
}

#[tokio::test]
async fn unauthenticated_requests_reach_nothing() {
    let sv = serve().await;
    let c = client();
    let r = c.get(format!("{}/healthz", sv.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = c.get(format!("{}/", sv.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert_eq!(r.headers()["location"], "/login");
    for path in [
        "/scan",
        "/auto/resume",
        "/plans/00000000-0000-0000-0000-000000000001/execute",
    ] {
        let r = c
            .post(format!("{}{path}", sv.base))
            .header("Origin", &sv.origin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    assert!(
        sv.stack.app.runtime.status().jobs.is_empty(),
        "nothing was queued"
    );
    // Security headers on every response, the login page included.
    let r = c.get(format!("{}/login", sv.base)).send().await.unwrap();
    assert!(r.headers().contains_key("content-security-policy"));
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn login_session_csrf_and_origin_gate_a_real_scan() {
    let sv = serve().await;
    let c = client();
    let r = login(&c, &sv, PASSWORD, None).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let set_cookie: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_lowercase())
        .collect();
    let session = set_cookie
        .iter()
        .find(|c| c.starts_with("removarr_session="))
        .unwrap();
    assert!(
        session.contains("httponly") && session.contains("samesite=strict"),
        "{session}"
    );

    let page = c.get(format!("{}/jobs", sv.base)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let token = csrf(&page.text().await.unwrap());

    // A command without the CSRF token or from a foreign origin is refused and queues nothing.
    let r = c
        .post(format!("{}/scan", sv.base))
        .header("Origin", &sv.origin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "missing CSRF token");
    let r = c
        .post(format!("{}/scan", sv.base))
        .header("Origin", "http://evil.invalid")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "foreign origin");
    let r = c
        .post(format!("{}/scan", sv.base))
        .header("Origin", &sv.origin)
        .header("X-CSRF-Token", "0".repeat(64))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "wrong CSRF token");
    assert!(sv.stack.app.runtime.status().jobs.is_empty());

    // The right token and origin: a real scan runs against the real tree and database.
    let r = c
        .post(format!("{}/scan", sv.base))
        .header("Origin", &sv.origin)
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    sv.stack.wait_idle().await;
    assert_eq!(
        sv.stack
            .issues(removarr_domain::issue::IssueCategory::Junk)
            .await
            .len(),
        3
    );

    // Logout revokes the session server-side.
    let r = c
        .post(format!("{}/logout", sv.base))
        .header("Origin", &sv.origin)
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_redirection(), "{}", r.status());
    let r = c.get(format!("{}/jobs", sv.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert_eq!(r.headers()["location"], "/login");
}

#[tokio::test]
async fn brute_force_is_limited_with_retry_after_and_spoofed_headers_do_not_help() {
    let sv = serve().await;
    let c = client();
    let mut statuses = Vec::new();
    let mut retry = None;
    for i in 0..8 {
        // A different spoofed client address each time: ignored without a trusted proxy.
        let r = login(&c, &sv, "wrong-password", Some(&format!("203.0.113.{i}"))).await;
        if r.status() == StatusCode::TOO_MANY_REQUESTS {
            retry = Some(
                r.headers()["retry-after"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
            );
        }
        statuses.push(r.status().as_u16());
    }
    assert_eq!(&statuses[..3], &[401, 401, 401], "{statuses:?}");
    assert!(statuses[3..].iter().all(|s| *s == 429), "{statuses:?}");
    let retry = retry.expect("Retry-After on 429");
    assert!((1..=900).contains(&retry), "{retry}");
    // Even the correct password is refused while the window is open, and costs no hash check.
    let r = login(&c, &sv, PASSWORD, None).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers().contains_key("retry-after"));
    // No session was granted.
    let r = c.get(format!("{}/jobs", sv.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
}
