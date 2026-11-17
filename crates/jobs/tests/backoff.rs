#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Backoff with jitter, bounded retries, and the recovery gate of the execute lane.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::jobs::{JobKind, JobOutcome, JobStatus};
use removarr_domain::session::ServiceKind;
use removarr_jobs::backoff::{NoJitter, retry};
use removarr_jobs::config::RetryConfig;
use removarr_jobs::events::JobEventKind;
use removarr_jobs::handler::{JobError, JobSuccess, RecoverySummary};
use removarr_jobs::runtime::RecoveryStatus;
use tokio_util::sync::CancellationToken;

fn unavailable() -> JobError {
    JobError::failed(ErrorClass::Unavailable, "connection refused")
}

fn retries(r: &Rig) -> Vec<u64> {
    r.rt.events_since(0)
        .into_iter()
        .filter_map(|e| match e.kind {
            JobEventKind::Retrying { delay_ms, .. } => Some(delay_ms),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_retry_helper_is_bounded_and_waits_one_two_four_seconds() {
    let cfg = RetryConfig::default();
    let waits = std::sync::Mutex::new(Vec::new());
    let calls = std::sync::atomic::AtomicU32::new(0);
    let t0 = tokio::time::Instant::now();
    let r: Result<(), _> = retry(
        &cfg,
        &NoJitter,
        &CancellationToken::new(),
        |_: &&str| true,
        |n, d, _| waits.lock().unwrap().push((n, d)),
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err("down") }
        },
    )
    .await;
    let f = r.unwrap_err();
    assert_eq!(
        (f.attempts, f.cancelled),
        (4, false),
        "1 call + 3 retries, then give up"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    let w: Vec<u64> = waits
        .lock()
        .unwrap()
        .iter()
        .map(|(_, d)| d.as_secs())
        .collect();
    assert_eq!(w, [1, 2, 4]);
    assert_eq!(t0.elapsed(), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn permanent_errors_are_not_retried_and_cancellation_ends_the_wait() {
    let cfg = RetryConfig::default();
    let calls = std::sync::atomic::AtomicU32::new(0);
    let r: Result<(), _> = retry(
        &cfg,
        &NoJitter,
        &CancellationToken::new(),
        |e: &&str| *e == "down",
        |_, _, _| {},
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err("rejected") }
        },
    )
    .await;
    assert_eq!(r.unwrap_err().attempts, 1);
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        t.cancel();
    });
    let r: Result<(), _> = retry(
        &cfg,
        &NoJitter,
        &token,
        |_: &&str| true,
        |_, _, _| {},
        || async { Err("down") },
    )
    .await;
    let f = r.unwrap_err();
    assert!(f.cancelled);
    assert_eq!(f.attempts, 1);
}

#[tokio::test(start_paused = true)]
async fn a_transient_failure_of_an_observe_job_is_retried_with_jitter_then_succeeds() {
    let r = rig_with(manual_only(), Arc::new(FixedJitter(1.0)), 1_700_000_000_000);
    r.script.on_run(|_, _, n| {
        Box::pin(async move {
            if n < 3 {
                Err(unavailable())
            } else {
                Ok(JobSuccess::default())
            }
        })
    });
    r.rt.start().await.unwrap();
    let job =
        r.rt.submit(JobKind::RefreshService(ServiceKind::Qbittorrent))
            .await
            .unwrap();
    until("succeeded", || {
        r.store.status(job.id) == Some(JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.script.calls_of("refresh:qbittorrent"), 3);
    assert_eq!(
        retries(&r),
        [1200, 2400],
        "1 s and 2 s, plus 20 percent jitter"
    );
}

#[tokio::test(start_paused = true)]
async fn retries_stop_after_three_and_the_job_fails_with_its_class() {
    let r = rig(manual_only());
    r.script
        .on_run(|_, _, _| Box::pin(async { Err(unavailable()) }));
    r.rt.start().await.unwrap();
    let job = r.rt.rescan().await.unwrap();
    until("failed", || {
        r.store.status(job.id) == Some(JobStatus::Failed)
    })
    .await;
    assert_eq!(r.script.calls_of("scan"), 4, "no infinite retry");
    assert_eq!(retries(&r), [1000, 2000, 4000]);
    assert!(matches!(
        r.store.all()[0].outcome,
        Some(JobOutcome::Failed {
            class: ErrorClass::Unavailable,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn rejections_and_mutating_jobs_are_never_retried() {
    let r = rig(manual_only());
    r.script.on_run(|k, _, _| {
        Box::pin(async move {
            Err(if matches!(k, JobKind::Scan) {
                JobError::failed(ErrorClass::Rejected, "no")
            } else {
                unavailable()
            })
        })
    });
    r.rt.start().await.unwrap();
    r.rt.rescan().await.unwrap();
    let exec = r.rt.submit(exec_kind(1, false)).await.unwrap();
    until("both failed", || {
        r.store.all().iter().all(|j| j.status == JobStatus::Failed)
    })
    .await;
    assert_eq!(r.script.calls_of("scan"), 1);
    assert_eq!(
        r.script
            .calls_of(&exec_kind(1, false).dedupe_key().unwrap()),
        1,
        "REJ-21"
    );
    assert_eq!(r.store.status(exec.id), Some(JobStatus::Failed));
    assert!(retries(&r).is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_while_waiting_to_retry_ends_the_job() {
    let r = rig(manual_only());
    r.script
        .on_run(|_, _, _| Box::pin(async { Err(unavailable()) }));
    r.rt.start().await.unwrap();
    let job = r.rt.rescan().await.unwrap();
    until("first retry scheduled", || !retries(&r).is_empty()).await;
    r.rt.cancel(job.id).await.unwrap();
    until("cancelled", || {
        r.store.status(job.id) == Some(JobStatus::Cancelled)
    })
    .await;
    assert_eq!(r.script.calls_of("scan"), 1);
}

#[tokio::test(start_paused = true)]
async fn nothing_executes_while_recovery_is_unresolved_and_attempts_follow_the_schedule() {
    let r = rig(manual_only());
    {
        let mut q = r.script.recover_results.lock().unwrap();
        q.push_back(Err(PortError::new(
            ErrorClass::Unavailable,
            "qBittorrent down",
        )));
        q.push_back(Err(PortError::new(
            ErrorClass::Unavailable,
            "qBittorrent down",
        )));
    }
    r.rt.start().await.unwrap();
    let exec = r.rt.submit(exec_kind(1, false)).await.unwrap();
    let scan = r.rt.rescan().await.unwrap();
    until("first attempt", || {
        r.script.recover_calls.load(Ordering::SeqCst) == 1
    })
    .await;
    // The observe lane is unaffected.
    until("scan done", || {
        r.store.status(scan.id) == Some(JobStatus::Succeeded)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(299)).await;
    assert_eq!(r.script.recover_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        r.rt.status().recovery,
        RecoveryStatus::Pending { attempt: 1, .. }
    ));
    assert_eq!(r.store.status(exec.id), Some(JobStatus::Queued));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(r.script.recover_calls.load(Ordering::SeqCst), 2);
    assert_eq!(r.store.status(exec.id), Some(JobStatus::Queued));
    tokio::time::sleep(Duration::from_secs(300)).await;
    until("executed", || {
        r.store.status(exec.id) == Some(JobStatus::Succeeded)
    })
    .await;
    let log = r.script.log();
    let first_exec = log
        .iter()
        .position(|l| l.starts_with("start:exec"))
        .unwrap();
    assert_eq!(
        log[..first_exec].iter().filter(|l| *l == "recover").count(),
        3
    );
    assert_eq!(
        log[first_exec - 1],
        "recover",
        "recovery runs right before the execute job"
    );
}

#[tokio::test(start_paused = true)]
async fn twelve_failed_recoveries_need_a_manual_retry_and_never_loop_forever() {
    let r = rig(manual_only());
    {
        let mut q = r.script.recover_results.lock().unwrap();
        for _ in 0..12 {
            q.push_back(Err(PortError::new(ErrorClass::Unavailable, "down")));
        }
    }
    r.rt.start().await.unwrap();
    let exec = r.rt.submit(exec_kind(1, false)).await.unwrap();
    until("needs manual retry", || {
        r.rt.status().recovery == RecoveryStatus::NeedsManual
    })
    .await;
    assert_eq!(r.script.recover_calls.load(Ordering::SeqCst), 12);
    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    assert_eq!(
        r.script.recover_calls.load(Ordering::SeqCst),
        12,
        "no attempt without the user"
    );
    assert_eq!(r.store.status(exec.id), Some(JobStatus::Queued));
    r.rt.retry_recovery();
    until("executed after manual retry", || {
        r.store.status(exec.id) == Some(JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.rt.status().recovery, RecoveryStatus::Clean);
}

#[tokio::test(start_paused = true)]
async fn recover_runs_before_every_execute_job_and_after_an_execute_error() {
    let r = rig(manual_only());
    r.script.on_run(|k, _, _| {
        Box::pin(async move {
            if k == exec_kind(1, false) {
                Err(JobError::failed(ErrorClass::Internal, "boom"))
            } else {
                Ok(JobSuccess::default())
            }
        })
    });
    r.rt.start().await.unwrap();
    let a = r.rt.submit(exec_kind(1, false)).await.unwrap();
    until("a failed", || {
        r.store.status(a.id) == Some(JobStatus::Failed)
    })
    .await;
    settle().await;
    let b = r.rt.submit(exec_kind(2, false)).await.unwrap();
    until("b done", || {
        r.store.status(b.id) == Some(JobStatus::Succeeded)
    })
    .await;
    let log: Vec<String> = r
        .script
        .log()
        .into_iter()
        .filter(|l| !l.starts_with("end:"))
        .collect();
    let pos = |needle: &str| log.iter().position(|l| l.starts_with(needle)).unwrap();
    assert_eq!(
        log[pos("start:exec:00000000-0000-0000-0000-000000000001") - 1],
        "recover"
    );
    assert_eq!(
        log[pos("start:exec:00000000-0000-0000-0000-000000000001") + 1],
        "recover",
        "after the error"
    );
    assert_eq!(
        log[pos("start:exec:00000000-0000-0000-0000-000000000002") - 1],
        "recover"
    );
}

#[tokio::test(start_paused = true)]
async fn a_recovery_that_resolved_a_plan_pauses_automatic_mode() {
    let mut cfg = manual_only();
    cfg.auto_mode = true;
    let r = rig(cfg);
    r.script
        .recover_results
        .lock()
        .unwrap()
        .push_back(Ok(RecoverySummary {
            plans: 1,
            left_stopped: 0,
            notes: vec![],
        }));
    r.rt.start().await.unwrap();
    until("paused", || r.rt.status().auto_paused.is_some()).await;
    assert!(
        r.rt.status()
            .auto_paused
            .unwrap()
            .contains("recovery resolved 1")
    );
    assert_eq!(r.rt.status().recovery, RecoveryStatus::Clean);
}
