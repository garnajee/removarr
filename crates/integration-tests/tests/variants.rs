//! Parity variants of `docs/legacy-parity.md` 7.4 through the production wiring: V2 unreadable
//! directory, V3 qBittorrent down, V4 escaping mapping, V5 wrong expected owner, V7 late-byte
//! difference, V8 qBittorrent 4.x. Every variant asserts "nothing was mutated" and what the
//! database says about the plans.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::issue::IssueCategory as C;
use removarr_integration_tests::*;
use removarr_testkit::fake_qbit::{FakeQbit, parity_escape_torrent};
use removarr_testkit::fixture::ParityFixture;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

fn charlie() -> String {
    format!("completed:{0}/{0}.mkv", fx::CHARLIE)
}

async fn consolidation_refused(s: &Stack, why: &str) {
    let before = world(&s.fx);
    let id = s.issue_id(C::HardlinkMissing, &charlie()).await;
    let r = s
        .try_create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    assert!(r.is_err(), "{why}: a plan was built");
    assert_eq!(world(&s.fx), before, "{why}");
    assert!(s.qbit_mutations().is_empty(), "{why}");
}

#[tokio::test]
async fn v3_qbittorrent_down_scans_but_plans_nothing() {
    let s = Stack::parity(opts()).await;
    s.qbit.force_status("torrents/info", 503);
    let st = s.run_job(removarr_domain::jobs::JobKind::Scan).await;
    // The scan either completes with the service marked unavailable, or fails after bounded
    // retries; in both cases nothing may be planned from unknown torrent ownership.
    let _ = st;
    s.qbit.force_status("torrents/info", 200);
    // Re-scan with the client back to have issues, then take it down for planning.
    s.scan().await;
    s.qbit.force_status("torrents/info", 503);
    s.run_job(removarr_domain::jobs::JobKind::Scan).await;
    if let Some(r) = s.try_issue_at(C::HardlinkMissing, &charlie()).await {
        let _ = r;
        consolidation_refused(&s, "qBittorrent unavailable at the last scan").await;
    }
    assert!(s.qbit_mutations().is_empty());
}

#[tokio::test]
async fn v4_an_escaping_mapping_blocks_planning_even_with_the_external_prefix() {
    let s = Stack::parity(opts()).await;
    let mut t = parity_torrents(now_secs());
    t.push(parity_escape_torrent(now_secs()));
    s.qbit.set_torrents(t);
    s.scan().await;
    consolidation_refused(&s, "V4 mapping error").await;
    let st = removarr_domain::ports::ScanStore::latest_session(&s.repos.scans)
        .await
        .unwrap()
        .unwrap();
    assert!(
        st.services
            .iter()
            .any(|x| format!("{:?}", x.state).contains("MappingErrors")),
        "{:?}",
        st.services
    );
}

#[tokio::test]
async fn v2_an_unreadable_directory_is_a_recorded_scan_error_and_stays_out_of_every_issue() {
    let fx = ParityFixture::build().unwrap();
    fx.apply_v2().unwrap();
    let s = Stack::over(fx, opts()).await;
    let _ = s.run_job(removarr_domain::jobs::JobKind::Scan).await;
    let c = s.inventory(&s.cfg.roots().completed().id).await;
    assert!(
        !c.errors.is_empty(),
        "the unreadable directory is recorded as a scan error"
    );
    assert!(
        c.errors
            .iter()
            .any(|e| e.path.rel.as_str().contains("Lima")),
        "{:?}",
        c.errors
    );
    // Read-only issues are still listed. The scan error is scoped to its subtree: a manual plan
    // elsewhere in the root stays possible (automatic mutation is paused, see `auto_cycle.rs`),
    // and nothing under the unreadable directory is ever listed or plannable.
    let all: Vec<_> = {
        let mut v = Vec::new();
        for c in C::ALL {
            v.extend(s.issues(c).await);
        }
        v
    };
    assert!(
        !all.iter()
            .any(|r| r.paths.iter().any(|p| p.contains("Lima"))),
        "nothing is known about Lima"
    );
    let junk = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let before = world(&s.fx);
    let p = s
        .create_plan(&PlanRequest::issues(
            C::Junk,
            vec![junk],
            IssueAction::Delete,
        ))
        .await;
    assert_eq!(world(&s.fx), before, "planning changes nothing");
    let _ = p;
}

#[tokio::test]
async fn v5_a_wrong_expected_owner_flags_the_tree_and_mutates_nothing() {
    let fx = ParityFixture::build().unwrap();
    let (u, g) = fx.apply_v5_expected_owner().unwrap();
    let s = Stack::over(
        fx,
        opts().env("REMOVARR_EXPECTED_OWNER", &format!("{u}:{g}")),
    )
    .await;
    s.scan().await;
    let perm = s.issues(C::Permission).await;
    assert!(
        perm.len() > 1,
        "owner mismatch is reported broadly: {}",
        perm.len()
    );
    let before = world(&s.fx);
    // A consolidation needs a writable, correctly owned target: refused.
    let id = s.issue_id(C::HardlinkMissing, &charlie()).await;
    let r = s
        .try_create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    if let Ok(p) = r {
        s.approve(&p).await;
        s.execute(p.id, false).await;
    }
    assert_eq!(
        world(&s.fx),
        before,
        "nothing was mutated under a wrong owner"
    );
    assert!(s.qbit_mutations().iter().all(|m| !m.starts_with("delete ")));
}

#[tokio::test]
async fn v7_a_difference_outside_the_sample_windows_is_rejected_by_the_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    fx.apply_v7().unwrap();
    let s = Stack::over(fx, opts()).await;
    s.scan().await;
    // The pair survives the name+size funnel but the content differs: never an exact duplicate.
    assert!(
        s.try_issue_at(C::Duplicate, &charlie()).await.is_none(),
        "no duplicate issue for a pair whose bytes differ"
    );
    let id = s.issue_id(C::HardlinkMissing, &charlie()).await;
    let before = world(&s.fx);
    let r = s
        .try_create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    if let Ok(p) = r {
        s.approve(&p).await;
        s.execute(p.id, false).await;
    }
    let media = s.p("medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv");
    assert_ne!(
        ino(&media),
        ino(&s.p(&format!("completed/{0}/{0}.mkv", fx::CHARLIE)))
    );
    assert_eq!(world(&s.fx), before);
}

#[tokio::test]
async fn v8_qbittorrent_4x_uses_pause_and_resume_and_accepts_paused_up() {
    let fx = ParityFixture::build().unwrap();
    let qbit = FakeQbit::spawn("v4.6.7").await;
    qbit.set_torrents(parity_torrents(now_secs()));
    let s = Stack::assemble(fx, qbit, None, None, None, opts()).await;
    s.scan().await;
    // T_B2 is `pausedUP`, a valid 4.x state: the Bravo consolidation is no longer blocked.
    let bravo = s
        .issue_at(C::HardlinkMissing, &format!("completed:{}.mkv", fx::BRAVO))
        .await;
    assert!(
        !bravo.blockers.iter().any(|b| b == "TorrentProtectedState"),
        "{:?}",
        bravo.blockers
    );
    let id = removarr_domain::ids::IssueId(bravo.id);
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;
    let m = s.qbit_mutations();
    assert!(m.iter().any(|r| r.starts_with("pause ")), "{m:?}");
    assert!(m.iter().any(|r| r.starts_with("resume ")), "{m:?}");
    assert!(
        !m.iter()
            .any(|r| r.starts_with("stop ") || r.starts_with("start ")),
        "{m:?}"
    );
    assert_eq!(
        ino(&s.p(&fx::bravo_completed())),
        ino(&s.p(&fx::bravo_media()))
    );
}
