#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Restart recovery: explicit job states, one re-queue, no automatic restart of anything that
//! mutates, and a real SQLite database across a "crash".

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use removarr_domain::ids::{JobId, SessionId};
use removarr_domain::jobs::{JobKind, JobStatus};
use removarr_domain::ports::{JobStore, StateStore};
use removarr_domain::session::ServiceKind;
use removarr_jobs::auto::{KEY_CYCLE, KEY_PAUSED};
use removarr_jobs::backoff::NoJitter;
use removarr_jobs::runtime::JobRuntime;
use removarr_testkit::clock::FakeClock;

fn session(n: u128) -> SessionId {
    SessionId::new(removarr_domain::ids::RawUuid::from_u128(n))
}

fn auto_cfg() -> removarr_jobs::JobsConfig {
    let mut c = manual_only();
    c.auto_mode = true;
    c
}

#[tokio::test(start_paused = true)]
async fn interrupted_jobs_are_requeued_once_and_execution_never() {
    let r = rig(auto_cfg());
    r.store.insert_raw(JobKind::Scan, JobStatus::Running, 1001);
    r.store.insert_raw(
        JobKind::RefreshService(ServiceKind::Qbittorrent),
        JobStatus::Running,
        1002,
    );
    let exec = exec_kind(9, false);
    r.store.insert_raw(exec.clone(), JobStatus::Running, 1003);
    let cycle = JobKind::AutoCycle {
        session: session(7),
    };
    r.store.insert_raw(cycle.clone(), JobStatus::Running, 1004);
    let queued = r.store.insert_raw(
        JobKind::VerifyContent { issue_ids: vec![] },
        JobStatus::Queued,
        1005,
    );

    let report = r.rt.start().await.unwrap();
    assert_eq!(report.interrupted.len(), 4);
    assert_eq!(report.requeued.len(), 2, "scan and refresh only");

    until("requeued and queued jobs ran", || {
        r.store.status(queued) == Some(JobStatus::Succeeded)
            && report
                .requeued
                .iter()
                .all(|id| r.store.status(*id) == Some(JobStatus::Succeeded))
    })
    .await;
    let scans = r.store.with_key("scan");
    assert_eq!(
        scans.iter().map(|j| j.status).collect::<Vec<_>>(),
        [JobStatus::Interrupted, JobStatus::Succeeded]
    );
    assert_eq!(r.store.with_key(&exec.dedupe_key().unwrap()).len(), 1);
    assert_eq!(
        r.script.calls_of(&exec.dedupe_key().unwrap()),
        0,
        "an interrupted execution never restarts"
    );
    assert_eq!(
        r.script.calls_of(&cycle.dedupe_key().unwrap()),
        0,
        "an interrupted cycle never restarts"
    );
    // T5-16: the interrupted cycle left automatic mode paused, persisted.
    assert!(
        r.rt.status()
            .auto_paused
            .unwrap()
            .contains("interrupted by a restart")
    );
    assert!(!r.state.get(KEY_PAUSED).await.unwrap().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_that_is_itself_a_requeue_is_not_requeued_again() {
    let r = rig(manual_only());
    let id = r.store.insert_raw(JobKind::Scan, JobStatus::Running, 1001);
    // The previous start re-queued the scan as job 1; it was interrupted again.
    r.state.set("requeued:scan", &id.to_string()).await.unwrap();
    let report = r.rt.start().await.unwrap();
    settle().await;
    assert_eq!(report.interrupted, vec![id]);
    assert!(report.requeued.is_empty());
    assert_eq!(r.store.with_key("scan").len(), 1);
    assert_eq!(r.script.calls_of("scan"), 0);
}

#[tokio::test(start_paused = true)]
async fn an_unfinished_cycle_marker_pauses_auto_mode_and_the_pause_survives_restarts() {
    let r = rig(auto_cfg());
    r.state
        .set(KEY_CYCLE, &session(3).to_string())
        .await
        .unwrap();
    let report = r.rt.start().await.unwrap();
    assert!(report.auto_paused.unwrap().contains("never finished"));
    assert_eq!(r.state.get(KEY_CYCLE).await.unwrap().as_deref(), Some(""));
    r.rt.shutdown(Duration::from_secs(1)).await;

    // A second process on the same state: still paused, nothing automatic restarts by itself.
    let script2 = Script::new();
    let rt2 = JobRuntime::new(
        auto_cfg(),
        deps(&r.store, &r.state, &script2, &r.clock, Arc::new(NoJitter)),
    );
    let report2 = rt2.start().await.unwrap();
    assert!(report2.auto_paused.is_some());
    settle().await;
    assert!(script2.log().iter().all(|l| l == "recover"));
    // Only the user clears it.
    assert!(rt2.resume_auto().await.unwrap());
    assert_eq!(rt2.status().auto_paused, None);
    assert_eq!(r.state.get(KEY_PAUSED).await.unwrap().as_deref(), Some(""));
}

#[tokio::test]
async fn real_sqlite_keeps_the_queue_across_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("removarr.db");
    let clock = Arc::new(FakeClock::default());
    let (scan_id, queued_id);
    {
        let db = removarr_storage::Db::open(&path).await.unwrap();
        let repos = removarr_storage::Repos::new(&db, clock.clone());
        let mk = |kind: JobKind, n: u128| {
            removarr_domain::jobs::JobRecord::queued(
                JobId::new(removarr_domain::ids::RawUuid::from_u128(n)),
                kind,
                removarr_domain::time::Timestamp(1),
            )
            .unwrap()
        };
        scan_id = repos.jobs.enqueue(&mk(JobKind::Scan, 1001)).await.unwrap();
        queued_id = repos
            .jobs
            .enqueue(&mk(JobKind::RefreshService(ServiceKind::Qbittorrent), 1002))
            .await
            .unwrap();
        // The scan is claimed (running) when the "process dies": no finish, no shutdown.
        let claimed = repos
            .jobs
            .claim_next(removarr_domain::jobs::Lane::Observe)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, scan_id);
        db.close().await;
    }
    let db = removarr_storage::Db::open(&path).await.unwrap();
    let repos = removarr_storage::Repos::new(&db, clock.clone());
    let script = Script::new();
    let rt = JobRuntime::new(
        manual_only(),
        removarr_jobs::Deps {
            store: Arc::new(repos.jobs.clone()),
            state: Arc::new(repos.state.clone()),
            handler: script.clone(),
            clock: clock.clone(),
            ids: Arc::new(removarr_testkit::ids::SeqIds::new()),
            jitter: Arc::new(NoJitter),
        },
    );
    let report = rt.start().await.unwrap();
    assert_eq!(report.interrupted, vec![scan_id]);
    let new_scan = report.requeued[0];
    for _ in 0..500 {
        let a = repos.jobs.get(queued_id).await.unwrap().unwrap().status;
        let b = repos.jobs.get(new_scan).await.unwrap().unwrap().status;
        if a == JobStatus::Succeeded && b == JobStatus::Succeeded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        repos.jobs.get(scan_id).await.unwrap().unwrap().status,
        JobStatus::Interrupted
    );
    assert_eq!(
        repos.jobs.get(new_scan).await.unwrap().unwrap().status,
        JobStatus::Succeeded
    );
    assert_eq!(
        repos.jobs.get(queued_id).await.unwrap().unwrap().status,
        JobStatus::Succeeded
    );
    rt.shutdown(Duration::from_secs(2)).await;
    db.close().await;
}

#[tokio::test(start_paused = true)]
async fn defect3_interrupted_jobs_of_the_previous_run_stay_readable() {
    let r = rig(manual_only());
    let id = r.store.insert_raw(JobKind::Scan, JobStatus::Running, 1001);
    let report = r.rt.start().await.unwrap();
    settle().await;
    assert_eq!(report.interrupted, vec![id]);
    let p = r.rt.progress(id).expect("the old job is still readable");
    assert_eq!(p.status, JobStatus::Interrupted);
    assert_eq!(
        p.outcome,
        Some(removarr_domain::jobs::JobOutcome::Interrupted)
    );
    assert!(p.finished_at.is_some());
    let listed = r.rt.status().jobs;
    assert!(
        listed
            .iter()
            .any(|j| j.id == id && j.status == JobStatus::Interrupted),
        "listed with an Interrupted status"
    );
    // Recovery semantics are unchanged: the scan was re-queued once under a new id.
    assert_eq!(report.requeued.len(), 1);
    assert_ne!(report.requeued[0], id);
}
