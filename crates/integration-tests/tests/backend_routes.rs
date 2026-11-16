//! Task 8 (routes): the real router over real repositories, the real job runtime and the
//! loopback fakes. The whole user flow runs through HTTP forms; assertions also read the
//! database and the tree, not only status codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::PlanStatus;
use removarr_integration_tests::*;

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

fn location(h: &axum::http::HeaderMap) -> String {
    h.get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn the_user_flow_scan_plan_approve_execute_over_http_changes_the_tree_and_the_database() {
    let s = Stack::parity(opts()).await;
    let (st, h, _) = s.post("/scan", "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(location(&h).starts_with("/jobs/"));
    s.wait_idle().await;

    let (st, _, body) = s.get("/issues/junk").await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.contains("readme.txt") && body.contains("archive.rar"),
        "listed"
    );
    assert!(!body.contains("checked"), "nothing is preselected");
    assert!(
        s.p("completed/stray/readme.txt").exists(),
        "viewing a list deletes nothing"
    );

    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let (st, h, body) = s
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={}", id.0),
        )
        .await;
    assert_eq!(st, StatusCode::SEE_OTHER, "{body}");
    let plan_url = location(&h);
    let plan_id =
        removarr_domain::ids::PlanId(plan_url.rsplit('/').next().unwrap().parse().unwrap());
    assert_eq!(s.plan_status(plan_id).await, PlanStatus::Draft);
    let (st, _, body) = s.get(&plan_url).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("readme.txt"));
    assert!(
        s.p("completed/stray/readme.txt").exists(),
        "creating a plan deletes nothing"
    );

    // Executing a draft is refused; nothing queued, nothing deleted.
    let (st, ..) = s.post(&format!("/plans/{}/execute", plan_id.0), "").await;
    assert!(st.is_client_error(), "{st}");
    assert!(s.p("completed/stray/readme.txt").exists());

    // Approval needs the exact hash and the typed count.
    let hash = s.stored_hash(plan_id).await;
    let (st, ..) = s
        .post(
            &format!("/plans/{}/approve", plan_id.0),
            &format!("plan_hash={hash}&confirmed_count=7"),
        )
        .await;
    assert!(st.is_client_error(), "wrong count refused: {st}");
    assert_eq!(s.plan_status(plan_id).await, PlanStatus::Draft);
    let (st, ..) = s
        .post(
            &format!("/plans/{}/approve", plan_id.0),
            &format!("plan_hash={}&confirmed_count=1", "0".repeat(64)),
        )
        .await;
    assert!(st.is_client_error(), "wrong hash refused: {st}");
    let (st, ..) = s
        .post(
            &format!("/plans/{}/approve", plan_id.0),
            &format!("plan_hash={hash}&confirmed_count=1"),
        )
        .await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(s.plan_status(plan_id).await, PlanStatus::Approved);
    assert!(
        s.p("completed/stray/readme.txt").exists(),
        "approving executes nothing"
    );

    let (st, h, _) = s.post(&format!("/plans/{}/execute", plan_id.0), "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(location(&h).starts_with("/jobs/"));
    s.wait_idle().await;

    assert!(!s.p("completed/stray/readme.txt").exists());
    assert!(
        s.p("completed/stray/archive.rar").exists(),
        "only the selected file went"
    );
    assert_eq!(s.plan_status(plan_id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan_id).await,
        vec![Some(removarr_domain::journal::ActionOutcome::Verified)]
    );
    assert!(
        s.journal(plan_id)
            .await
            .iter()
            .all(|(_, r)| r.as_deref() == Some("Done"))
    );

    // History, statistics and exports read what was persisted.
    let (st, _, body) = s.get("/history").await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.contains(&plan_id.0.to_string()) || body.contains("verified"),
        "{body}"
    );
    let (st, _, body) = s.get("/plans").await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(&plan_id.0.to_string()));
    let (st, _, _) = s.get("/stats").await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn pages_never_leak_host_paths_or_secrets_and_get_routes_mutate_nothing() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let before = world(&s.fx);
    let base = s.fx.base().to_str().unwrap().to_string();
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let mut uris = vec![
        "/".to_string(),
        "/jobs".into(),
        "/auto".into(),
        "/history".into(),
        "/plans".into(),
        "/stats".into(),
        "/logs".into(),
        "/ignored".into(),
        format!("/issues/junk/{}", id.0),
        "/export/latest/json".into(),
        "/export/latest/csv".into(),
        "/export/latest/md".into(),
        "/export/latest/html".into(),
    ];
    for c in C::ALL {
        uris.push(format!("/issues/{}", c.as_str()));
    }
    for u in uris {
        let (st, _, body) = s.get(&u).await;
        assert_eq!(st, StatusCode::OK, "{u}: {body}");
        assert!(!body.contains(&base), "{u} leaks a host path");
        for secret in [
            removarr_testkit::fake_qbit::FAKE_PASSWORD,
            removarr_testkit::fake_arr::FAKE_API_KEY,
            JELLYFIN_KEY,
            DUMMY_HASH,
            DUMMY_SESSION_KEY,
        ] {
            assert!(!body.contains(secret), "{u} leaks a secret");
        }
    }
    assert_eq!(world(&s.fx), before);
    assert!(s.qbit_mutations().is_empty());
}

#[tokio::test]
async fn an_ignored_item_is_never_planned_and_restoring_it_makes_it_plannable_again() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let (st, ..) = s
        .post("/issues/ignore", &format!("issue_ids={}&reason=keep", id.0))
        .await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let (_, _, body) = s.get("/issues/junk").await;
    assert!(
        !body.contains("readme.txt"),
        "ignored items are hidden from the list"
    );
    let (st, _, body) = s.get("/ignored").await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains("readme.txt"));
    let (st, ..) = s
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={}", id.0),
        )
        .await;
    assert!(
        st.is_client_error(),
        "an ignored item cannot be planned: {st}"
    );
    let (st, ..) = s
        .post("/issues/restore", &format!("issue_ids={}", id.0))
        .await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let (st, ..) = s
        .post(
            "/plans",
            &format!("category=junk&action=delete&issue_ids={}", id.0),
        )
        .await;
    assert_eq!(st, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn malformed_requests_are_rejected_without_side_effects() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let before = world(&s.fx);
    for (uri, form) in [
        ("/plans", "category=junk&action=delete&issue_ids=zz"),
        (
            "/plans",
            "category=nope&action=delete&issue_ids=00000000000000000000000000000000",
        ),
        (
            "/plans",
            "category=junk&action=consolidate&issue_ids=00000000000000000000000000000000",
        ),
        ("/plans", ""),
        ("/plans/not-a-uuid/execute", ""),
        ("/plans/00000000-0000-0000-0000-000000000000/execute", ""),
        ("/jobs/00000000-0000-0000-0000-000000000000/cancel", ""),
        ("/services/qbittorrent/refresh/../x", ""),
    ] {
        let (st, ..) = s.post(uri, form).await;
        assert!(st.is_client_error(), "{uri} {form}: {st}");
    }
    assert_eq!(world(&s.fx), before);
    assert!(s.qbit_mutations().is_empty());
}
