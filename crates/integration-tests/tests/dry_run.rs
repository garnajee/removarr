//! Task 4: a dry-run process performs no filesystem and no service mutation, end to end:
//! scan, plans of every filesystem kind, approval, execution requests (over the real router and
//! through the job runtime). Assertions are on the whole tree, on the request logs of the HTTP
//! fakes and on the persisted journal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::ids::IssueId;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::PlanStatus;
use removarr_integration_tests::*;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options {
        radarr: true,
        sonarr: true,
        jellyfin: true,
        ..Options::default()
    }
    .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

#[tokio::test]
async fn dry_run_pipeline_mutates_neither_the_tree_nor_any_service() {
    let s = Stack::parity(opts()).await;
    assert!(s.app.state.settings.dry_run && !s.app.report.live_adapters);
    let before = world(&s.fx);
    s.scan().await;
    s.run_job(removarr_domain::jobs::JobKind::RefreshService(
        removarr_domain::session::ServiceKind::Qbittorrent,
    ))
    .await;

    let charlie = format!("completed:{}/{}.mkv", fx::CHARLIE, fx::CHARLIE);
    let delta = "completed:Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
    let stalled = IssueId(s.issues(C::IncompleteStalled).await[0].id.clone());
    let reqs = vec![
        PlanRequest::issues(
            C::HardlinkMissing,
            vec![s.issue_id(C::HardlinkMissing, &charlie).await],
            IssueAction::Consolidate,
        ),
        PlanRequest::issues(
            C::Junk,
            vec![s.issue_id(C::Junk, "completed:stray/readme.txt").await],
            IssueAction::Delete,
        ),
        PlanRequest::Issues {
            category: C::HardlinkMissing,
            issue_ids: vec![s.issue_id(C::HardlinkMissing, delta).await],
            action: IssueAction::Delete,
            remove_torrent_tasks: true,
            include_specials: false,
        },
        PlanRequest::issues(
            C::IncompleteStalled,
            vec![stalled],
            IssueAction::CleanupStalled,
        ),
    ];
    let mut plans = Vec::new();
    for r in &reqs {
        let p = s.create_plan(r).await;
        assert!(p.dry_run, "plans of a dry-run process are dry-run plans");
        s.approve(&p).await;
        plans.push(p);
    }

    for p in &plans {
        // A live execution is refused in a dry-run process, before any job exists.
        let (st, ..) = s.post(&format!("/plans/{}/execute", p.id.0), "").await;
        assert!(
            st.is_client_error(),
            "live execute must be refused, got {st}"
        );
        // A simulation runs the whole pipeline on the recording set.
        let (st, ..) = s.post(&format!("/plans/{}/simulate", p.id.0), "").await;
        assert!(st.is_redirection(), "{st}");
        s.wait_idle().await;
    }

    assert_eq!(world(&s.fx), before, "not one entry of any root changed");
    assert!(s.qbit_mutations().is_empty(), "{:?}", s.qbit_mutations());
    assert_eq!(s.radarr.as_ref().unwrap().mutating_count(), 0);
    assert_eq!(s.sonarr.as_ref().unwrap().mutating_count(), 0);
    assert_eq!(s.jellyfin.as_ref().unwrap().post_count(), 0);
    for p in &plans {
        assert_eq!(
            s.plan_status(p.id).await,
            PlanStatus::Approved,
            "a simulation changes no status"
        );
        let j = s.journal(p.id).await;
        assert!(!j.is_empty(), "the simulation is journaled");
        assert!(
            j.iter().all(|(_, r)| r.as_deref() == Some("Simulated")),
            "{j:?}"
        );
    }
    // No hidden state change either: a fresh scan finds the same issues.
    s.scan().await;
    assert_eq!(s.issues(C::Junk).await.len(), 3);
}

#[tokio::test]
async fn simulated_execution_still_revalidates_against_the_real_tree() {
    // A simulation reads the real filesystem: a changed target refuses the plan even though
    // nothing would be mutated.
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let p = s
        .create_plan(&PlanRequest::issues(C::Junk, vec![id], IssueAction::Delete))
        .await;
    s.approve(&p).await;
    std::fs::write(s.p("completed/stray/readme.txt"), b"changed after planning").unwrap();
    let before = world(&s.fx);
    s.execute(p.id, true).await;
    assert_eq!(world(&s.fx), before);
    // Refused before any step: nothing journaled, no outcome, the plan is not consumed.
    assert!(s.journal(p.id).await.is_empty());
    assert_eq!(s.action_outcomes(p.id).await, vec![None]);
    assert_eq!(s.plan_status(p.id).await, PlanStatus::Approved);
    assert!(s.p("completed/stray/readme.txt").exists());
}
