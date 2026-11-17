//! Task 7 (job runtime over the real wiring): execution waits for a scan that shares its root,
//! cancellation of queued and running jobs, and restart of an interrupted observe job.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use removarr_domain::issue::IssueCategory as C;
use removarr_domain::jobs::{JobKind, JobStatus};
use removarr_domain::plan::PlanStatus;
use removarr_integration_tests::*;
use removarr_jobs::CancelResult;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

const CH: &str = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
const MEDIA: &str = "medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv";

async fn approved_charlie(s: &Stack) -> removarr_domain::ids::PlanId {
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{CH}/{CH}.mkv"))
        .await;
    let p = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    s.approve(&p).await;
    p.id
}

#[tokio::test]
async fn an_execution_waits_for_a_running_scan_that_shares_its_root() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let plan = approved_charlie(&s).await;
    let media = s.p(MEDIA);
    let before = ino(&media);

    // Slow the scan down (every torrent listing), start it, then ask for the execution.
    s.qbit
        .set_path_delay("torrents/info", Duration::from_millis(150));
    let scan = s.app.runtime.submit(JobKind::Scan).await.unwrap().id;
    for _ in 0..500 {
        if s.app
            .runtime
            .progress(scan)
            .is_some_and(|p| p.status == JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let exec = s
        .app
        .runtime
        .submit(JobKind::ExecutePlan {
            plan_id: plan,
            simulate: false,
        })
        .await
        .unwrap()
        .id;
    // While the scan runs, nothing of the execution has touched the tree or the client.
    let mut observed_running_scan = 0;
    while s
        .app
        .runtime
        .progress(scan)
        .is_some_and(|p| p.status == JobStatus::Running)
    {
        observed_running_scan += 1;
        assert_eq!(
            ino(&media),
            before,
            "the execution mutated while the scan held the root"
        );
        assert!(s.qbit_mutations().is_empty());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        observed_running_scan > 0,
        "the scan was too fast to observe"
    );
    s.qbit.set_path_delay("torrents/info", Duration::ZERO);
    assert_eq!(s.wait_job(exec).await, JobStatus::Succeeded);
    let (sp, ep) = (
        s.app.runtime.progress(scan).unwrap(),
        s.app.runtime.progress(exec).unwrap(),
    );
    assert!(
        ep.finished_at.unwrap() >= sp.finished_at.unwrap(),
        "the execution ended only after the scan released the root (it may be marked running \
         earlier while it waits for the lock)"
    );
    // The execution used its plan against the old scan: it either verified or refused as stale,
    // but never ran interleaved; the persisted state says which.
    let st = s.plan_status(plan).await;
    assert!(
        matches!(st, PlanStatus::Executed | PlanStatus::Invalidated),
        "{st:?}"
    );
}

#[tokio::test]
async fn cancelling_a_queued_execution_leaves_the_plan_approved_and_the_tree_untouched() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let plan = approved_charlie(&s).await;
    let before = world(&s.fx);
    // Hold the observe lane with a slow scan so that the execution job is still waiting.
    s.qbit
        .set_path_delay("torrents/info", Duration::from_millis(300));
    let scan = s.app.runtime.submit(JobKind::Scan).await.unwrap().id;
    // A second job for the same plan while the first is active is the same job (dedupe).
    let a = s
        .app
        .runtime
        .submit(JobKind::ExecutePlan {
            plan_id: plan,
            simulate: false,
        })
        .await
        .unwrap();
    let b = s
        .app
        .runtime
        .submit(JobKind::ExecutePlan {
            plan_id: plan,
            simulate: false,
        })
        .await
        .unwrap();
    assert!(b.existing && a.id == b.id, "one importer per plan");
    // Cancel it as soon as possible; whichever state it is in, no mutation may result.
    let res = s.app.runtime.cancel(a.id).await.unwrap();
    assert!(
        matches!(res, CancelResult::CancelledQueued | CancelResult::Requested),
        "{res:?}"
    );
    // Cancel the scan too, then let everything settle.
    s.app.runtime.cancel(scan).await.unwrap();
    s.qbit.set_path_delay("torrents/info", Duration::ZERO);
    s.wait_idle().await;
    let st = s.app.runtime.progress(a.id).unwrap().status;
    assert!(
        matches!(
            st,
            JobStatus::Cancelled
                | JobStatus::Interrupted
                | JobStatus::Succeeded
                | JobStatus::Failed
        ),
        "{st:?}"
    );
    if st == JobStatus::Cancelled {
        assert_eq!(world(&s.fx), before);
        assert!(s.qbit_mutations().is_empty());
        assert_eq!(s.plan_status(plan).await, PlanStatus::Approved);
    }
    // The cancelled scan left no half-finished state: a new scan succeeds.
    assert_eq!(
        s.app.runtime.progress(scan).unwrap().status,
        JobStatus::Cancelled
    );
    s.scan().await;
}

#[tokio::test]
async fn cancelling_a_running_scan_ends_it_cancelled_and_the_next_scan_is_clean() {
    let s = Stack::parity(opts()).await;
    s.qbit
        .set_path_delay("torrents/info", Duration::from_millis(200));
    let scan = s.app.runtime.submit(JobKind::Scan).await.unwrap().id;
    for _ in 0..500 {
        if s.app
            .runtime
            .progress(scan)
            .is_some_and(|p| p.status == JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        s.app.runtime.cancel(scan).await.unwrap(),
        CancelResult::Requested
    );
    assert_eq!(s.wait_job(scan).await, JobStatus::Cancelled);
    s.qbit.set_path_delay("torrents/info", Duration::ZERO);
    s.scan().await;
    assert_eq!(s.issues(C::Junk).await.len(), 3);
    assert_eq!(
        s.app.runtime.cancel(scan).await.unwrap(),
        CancelResult::NotActive
    );
}

#[test]
fn an_observe_job_interrupted_by_a_crash_is_requeued_once_and_completes() {
    let mut lab = Lab::parity(opts());
    lab.stack()
        .qbit
        .set_path_delay("torrents/info", Duration::from_secs(3600));
    let scan = lab.app(async {
        lab.stack()
            .app
            .runtime
            .submit(JobKind::Scan)
            .await
            .unwrap()
            .id
    });
    lab.wait_for("the scan to read torrents", |s| {
        !s.qbit.requests_to("torrents/info").is_empty()
    });
    let _ = scan;
    lab.crash_and_restart();
    lab.release();
    lab.stack()
        .qbit
        .set_path_delay("torrents/info", Duration::ZERO);
    lab.app(async {
        let s = lab.stack();
        // The replacement job (re-queued at start) runs to completion by itself.
        for _ in 0..1000 {
            let sess = removarr_domain::ports::ScanStore::latest_session(&s.repos.scans)
                .await
                .unwrap();
            if sess.is_some_and(|x| x.finished_at.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        s.wait_idle().await;
        assert!(
            removarr_domain::ports::ScanStore::latest_session(&s.repos.scans)
                .await
                .unwrap()
                .is_some_and(|x| x.finished_at.is_some()),
            "the interrupted scan was re-queued and finished"
        );
        assert_eq!(s.issues(C::Junk).await.len(), 3);
    });
}
