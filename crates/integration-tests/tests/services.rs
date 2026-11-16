//! Task 3 (Jellyfin and ARR rescans) and dry-run guard on the service side: a rescan is a plan the
//! user approves; refreshes read only; nothing is sent in a dry-run process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::plan::PlanStatus;
use removarr_integration_tests::*;

fn opts(dry: bool) -> Options {
    Options {
        dry_run: dry,
        radarr: true,
        jellyfin: true,
        ..Options::default()
    }
    .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

fn plan_id(h: &axum::http::HeaderMap) -> removarr_domain::ids::PlanId {
    let loc = h
        .get("location")
        .expect("a redirect to the plan")
        .to_str()
        .unwrap();
    removarr_domain::ids::PlanId(loc.rsplit('/').next().unwrap().parse().unwrap())
}

#[tokio::test]
async fn refreshes_only_read_and_a_jellyfin_rescan_needs_a_plan_and_an_approval() {
    let s = Stack::parity(opts(false)).await;
    s.scan().await;
    for svc in ["qbittorrent", "radarr", "jellyfin"] {
        let (st, ..) = s.post(&format!("/services/{svc}/refresh"), "").await;
        assert!(st.is_redirection(), "{svc} {st}");
        s.wait_idle().await;
    }
    assert_eq!(
        s.jellyfin.as_ref().unwrap().post_count(),
        0,
        "a refresh only reads"
    );
    assert_eq!(s.radarr.as_ref().unwrap().mutating_count(), 0);
    assert!(s.qbit_mutations().is_empty());

    let (st, h, body) = s.post("/services/jellyfin/rescan", "").await;
    assert_eq!(st, axum::http::StatusCode::SEE_OTHER, "{body}");
    let id = plan_id(&h);
    assert_eq!(s.plan_status(id).await, PlanStatus::Draft);
    assert_eq!(
        s.jellyfin.as_ref().unwrap().post_count(),
        0,
        "a draft sends nothing"
    );
    // A draft cannot be executed; approval then execution sends exactly one refresh.
    let (st, ..) = s.post(&format!("/plans/{}/execute", id.0), "").await;
    assert!(st.is_client_error(), "{st}");
    let p = s.app.state.plans.get(id).await.unwrap();
    s.approve(&p).await;
    assert_eq!(s.jellyfin.as_ref().unwrap().post_count(), 0);
    s.execute(id, false).await;
    assert_eq!(s.jellyfin.as_ref().unwrap().post_count(), 1);
    assert_eq!(s.plan_status(id).await, PlanStatus::Executed);
    let j = s.journal(id).await;
    assert!(
        j.iter()
            .any(|(st, r)| st == "JellyfinRefresh" && r.as_deref() == Some("Done")),
        "{j:?}"
    );
}

#[tokio::test]
async fn a_radarr_rescan_is_one_command_after_approval_and_cannot_even_be_planned_in_a_dry_run() {
    let live = Stack::parity(opts(false)).await;
    live.scan().await;
    let (st, h, b) = live.post("/services/radarr/rescan", "").await;
    assert_eq!(st, axum::http::StatusCode::SEE_OTHER, "{b}");
    let id = plan_id(&h);
    let p = live.app.state.plans.get(id).await.unwrap();
    live.approve(&p).await;
    live.execute(id, false).await;
    let cmds: Vec<_> = live
        .radarr
        .as_ref()
        .unwrap()
        .commands()
        .into_values()
        .collect();
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    assert!(
        cmds[0].get("importMode").is_none(),
        "a rescan carries no import mode"
    );

    let dry = Stack::parity(opts(true)).await;
    dry.scan().await;
    // A dry-run process refuses to plan a rescan at all ("simulation only"): no plan, no call.
    let (st, h, _) = dry.post("/services/radarr/rescan", "").await;
    assert_eq!(st, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(h.get("location").is_none());
    let (st, ..) = dry.post("/services/jellyfin/rescan", "").await;
    assert_eq!(st, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(dry.radarr.as_ref().unwrap().mutating_count(), 0);
    assert_eq!(dry.jellyfin.as_ref().unwrap().post_count(), 0);
    let plans = dry
        .app
        .state
        .read
        .plans(&Default::default())
        .await
        .unwrap()
        .rows;
    assert!(plans.is_empty(), "{plans:?}");
}
