//! Authentication over a real listener on the production router, production `Auth` and the
//! full stack (wave 16). Complements wave 15 `auth_listener.rs` (login, CSRF, brute force
//! basics) with: session rotation and fixation, idle and absolute expiry (controlled clock),
//! HTMX expiry, the two login limits with `429` + `Retry-After`, window recovery, limiter
//! cleanup and password handling. A scan queued by a rejected request must never exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_e2e_perf::*;
use removarr_integration_tests::Options;
use reqwest::StatusCode;

const H: i64 = 3_600_000;

async fn live(per_ip: u32, per_user: u32) -> Live {
    Live::start(Options::default(), per_ip, per_user).await
}

#[tokio::test]
async fn login_logout_and_session_rotation_over_http() {
    let lv = live(20, 10).await;
    let b = lv.browser();
    // Public: health, login page; protected: everything else.
    assert_eq!(b.get("/healthz").await.status, StatusCode::OK);
    let p = b.get("/").await;
    assert_eq!(
        (p.status, p.location().as_str()),
        (StatusCode::SEE_OTHER, "/login")
    );

    let r = b.login(USER, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/");
    let first = r.set_cookie("removarr_session").expect("session cookie");
    let c = r.set_cookie_line("removarr_session").unwrap();
    assert!(c.to_lowercase().contains("httponly") && c.to_lowercase().contains("samesite=strict"));
    assert_eq!(b.get("/").await.status, StatusCode::OK);
    assert_eq!(lv.auth.stats().sessions, 1);

    // Logging in again from a browser that already holds a session rotates the id and the
    // old cookie value stops working (fixation defence).
    let r = b.login(USER, PASSWORD).await;
    let second = r.set_cookie("removarr_session").expect("rotated cookie");
    assert_ne!(first, second);
    assert_eq!(lv.auth.stats().sessions, 1, "the old session was removed");
    let stale = b
        .get_with("/", &[("Cookie", &format!("removarr_session={first}"))])
        .await;
    // Both the jar's cookie and the stale header are sent; use a clean client for certainty.
    let clean = Browser::new(&lv.base);
    let p = clean
        .get_with("/", &[("Cookie", &format!("removarr_session={first}"))])
        .await;
    assert_eq!(
        p.status,
        StatusCode::SEE_OTHER,
        "stale id refused (jar: {})",
        stale.status
    );
    assert_eq!(p.location(), "/login");

    // A forged or truncated cookie is refused without a panic.
    for v in ["", "x", &"A".repeat(300), &first[..first.len() / 2]] {
        let p = clean
            .get_with("/", &[("Cookie", &format!("removarr_session={v}"))])
            .await;
        assert_eq!(p.status, StatusCode::SEE_OTHER, "cookie {v:.10}");
    }

    // Logout: the CSRF token and origin are required, the session dies server-side.
    let token = b.token().await;
    let p = b.post_raw("/logout", "", &[("X-CSRF-Token", "bad")]).await;
    assert_eq!(p.status, StatusCode::FORBIDDEN);
    assert_eq!(lv.auth.stats().sessions, 1);
    let p = b.post("/logout", "", &token).await;
    assert!(p.status.is_redirection(), "{}", p.status);
    assert_eq!(p.location(), "/login");
    assert_eq!(lv.auth.stats().sessions, 0);
    // Replaying the old cookie after logout does not resume the session.
    let p = clean
        .get_with("/", &[("Cookie", &format!("removarr_session={second}"))])
        .await;
    assert_eq!(p.location(), "/login");
}

#[tokio::test]
async fn idle_and_absolute_expiry_with_htmx_and_commands() {
    let lv = live(20, 10).await;
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    assert_eq!(b.get("/jobs").await.status, StatusCode::OK);

    // Idle limit (2 h): activity every 90 minutes keeps the session alive...
    for _ in 0..3 {
        lv.clock.advance_millis(H * 3 / 2);
        assert_eq!(b.get("/jobs").await.status, StatusCode::OK);
    }
    // ... and 3 idle hours end it, for pages (redirect), fragments (401 + HX-Redirect) and
    // commands (401). Nothing is queued by the refused command.
    lv.clock.advance_millis(3 * H);
    let p = b.get("/jobs").await;
    assert_eq!(
        (p.status, p.location().as_str()),
        (StatusCode::SEE_OTHER, "/login")
    );
    assert_eq!(lv.auth.stats().sessions, 0, "expired session purged");
    let p = b.get_with("/jobs", &[("HX-Request", "true")]).await;
    assert_eq!(p.status, StatusCode::UNAUTHORIZED);
    assert_eq!(p.header("hx-redirect"), "/login");
    let p = b.post("/scan", "", &token).await;
    assert_eq!(p.status, StatusCode::UNAUTHORIZED);
    assert!(lv.stack.app.runtime.status().jobs.is_empty());

    // Absolute lifetime (session_ttl_hours, default 12) wins over continuous activity.
    b.login(USER, PASSWORD).await;
    assert_eq!(b.get("/jobs").await.status, StatusCode::OK);
    let ttl = i64::from(lv.stack.cfg.auth.session_ttl_hours);
    for i in 1..=ttl + 1 {
        lv.clock.advance_millis(H);
        let st = b.get("/jobs").await.status;
        if i < ttl {
            assert_eq!(st, StatusCode::OK, "hour {i} of {ttl}");
        }
    }
    assert_eq!(
        b.get("/jobs").await.status,
        StatusCode::SEE_OTHER,
        "absolute expiry"
    );
}

#[tokio::test]
async fn csrf_is_required_on_every_command_and_htmx_uses_the_header() {
    let lv = live(20, 10).await;
    let b = lv.browser();
    b.login(USER, PASSWORD).await;
    let token = b.token().await;
    let id = "00000000-0000-0000-0000-000000000001";
    let commands = [
        "/scan",
        "/auto/resume",
        "/recovery/retry",
        "/locale",
        "/issues/ignore",
        "/issues/restore",
        "/issues/evaluate",
        "/issues/verify-content",
        "/issues/selection-summary",
        "/plans",
        "/plans/preview",
        "/services/qbittorrent/refresh",
        "/services/jellyfin/rescan",
    ];
    let more = [
        format!("/plans/{id}/approve"),
        format!("/plans/{id}/execute"),
        format!("/plans/{id}/simulate"),
        format!("/plans/{id}/discard"),
        format!("/jobs/{id}/cancel"),
    ];
    for path in commands.iter().map(ToString::to_string).chain(more) {
        // No token, wrong token, foreign origin: 403 and no effect.
        let p = b.post_raw(&path, "", &[]).await;
        assert_eq!(p.status, StatusCode::FORBIDDEN, "{path} without token");
        let p = b.post_raw(&path, "csrf_token=wrong", &[]).await;
        assert_eq!(p.status, StatusCode::FORBIDDEN, "{path} wrong field token");
        let p = b.post_raw(&path, "", &[("X-CSRF-Token", "wrong")]).await;
        assert_eq!(p.status, StatusCode::FORBIDDEN, "{path} wrong header token");
        let p = b
            .post_raw(
                &path,
                &format!("csrf_token={token}"),
                &[("Origin", "http://evil.invalid")],
            )
            .await;
        assert_eq!(p.status, StatusCode::FORBIDDEN, "{path} foreign origin");
    }
    assert!(lv.stack.app.runtime.status().jobs.is_empty());
    // A GET never mutates, whatever the query.
    for q in ["/scan", "/plans/x/execute", "/locale?locale=fr"] {
        let p = b.get(q).await;
        assert!(
            [
                StatusCode::METHOD_NOT_ALLOWED,
                StatusCode::NOT_FOUND,
                StatusCode::FORBIDDEN
            ]
            .contains(&p.status),
            "GET {q}: {}",
            p.status
        );
    }
    // HTMX-style: the header replaces the field.
    let p = b
        .post_raw(
            "/issues/selection-summary",
            "category=junk",
            &[("X-CSRF-Token", &token), ("HX-Request", "true")],
        )
        .await;
    // The header token passed the guard: the refusal (if any) is the handler's validation.
    assert!(
        ![StatusCode::FORBIDDEN, StatusCode::UNAUTHORIZED].contains(&p.status),
        "{}",
        p.status
    );
}

#[tokio::test]
async fn both_login_limits_answer_429_with_retry_after_then_recover_and_clean_up() {
    // Per IP: 6 attempts per window, per (IP, user): 3.
    let lv = live(6, 3).await;
    let b = lv.browser();
    let mut codes = Vec::new();
    for _ in 0..4 {
        codes.push(b.login(USER, "wrong").await.status.as_u16());
    }
    assert_eq!(codes, [401, 401, 401, 429], "(IP, identifier) limit");
    let hashed = lv.auth.stats().password_checks;
    let r = b.login(USER, "wrong").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let ra: i64 = r.header("retry-after").parse().unwrap();
    assert!((1..=900).contains(&ra), "{ra}");
    assert_eq!(
        lv.auth.stats().password_checks,
        hashed,
        "refused before Argon2"
    );
    // The correct password is refused too while the (IP, user) window is open.
    assert_eq!(
        b.login(USER, PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Other identifiers still share the per-IP budget: 3 used, 3 left.
    let mut codes = Vec::new();
    for i in 0..5 {
        codes.push(b.login(&format!("other{i}"), "wrong").await.status.as_u16());
    }
    assert_eq!(codes, [401, 401, 401, 429, 429], "IP limit: {codes:?}");
    let r = b.login("other9", "wrong").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.body.contains("retry") || r.header("retry-after").parse::<i64>().is_ok());

    // Retry-After shrinks with time and the budget returns after the window; the table is
    // cleaned (no key retained for an expired window).
    lv.clock.advance_millis(10 * 60_000);
    let r = b.login(USER, "wrong").await;
    let ra2: i64 = r.header("retry-after").parse().unwrap();
    assert!(ra2 <= 300 + 1 && ra2 < ra, "{ra2} vs {ra}");
    lv.clock.advance_millis(6 * 60_000);
    let r = b.login(USER, PASSWORD).await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "window elapsed: {}",
        r.body
    );
    let keys = lv.auth.stats().limiter_keys;
    assert!(keys <= 2, "expired windows purged, {keys} keys left");
    // A success refunds its reservation: repeated good logins never lock the owner out.
    for _ in 0..8 {
        assert_eq!(b.login(USER, PASSWORD).await.status, StatusCode::SEE_OTHER);
    }
}

#[tokio::test]
async fn limiter_tables_stay_bounded_under_a_flood_of_distinct_identifiers() {
    let lv = live(1000, 1000).await;
    let b = lv.browser();
    for i in 0..300 {
        let _ = b.login(&format!("flood-{i}"), "x").await;
    }
    let s = lv.auth.stats();
    assert!(s.limiter_keys <= 2 * 4096, "bounded: {}", s.limiter_keys);
    // The owner can still sign in after the flood (the IP budget of 1000 is not exhausted).
    assert_eq!(b.login(USER, PASSWORD).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn oversized_and_malformed_login_bodies_are_refused_cheaply() {
    let lv = live(50, 50).await;
    let b = lv.browser();
    let page = b.get("/login").await;
    let token = html::csrf(&page.body);
    let before = lv.auth.stats().password_checks;
    let huge = format!(
        "csrf_token={token}&username={USER}&password={}",
        "p".repeat(20_000)
    );
    let r = b.post_raw("/login", &huge, &[]).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    let long_pw = format!(
        "csrf_token={token}&username={USER}&password={}",
        "p".repeat(2000)
    );
    let r = b.post_raw("/login", &long_pw, &[]).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    let r = b.post_raw("/login", "garbage", &[]).await;
    assert!(r.status.is_client_error());
    assert_eq!(
        lv.auth.stats().password_checks,
        before,
        "no Argon2 for malformed input"
    );
}
