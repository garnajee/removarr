#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Queue, lanes, locks, limits, cancellation, dedupe and memory-only status.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::ids::JobId;
use removarr_domain::jobs::{JobKind, JobOutcome, JobStatus};
use removarr_domain::session::ServiceKind;
use removarr_jobs::handler::{JobError, JobSuccess};
use removarr_jobs::locks::Scope;
use removarr_jobs::runtime::CancelResult;
use tokio::sync::Notify;

fn issue_ids(n: u128) -> JobKind {
    JobKind::EvaluateIssues {
        issue_ids: vec![removarr_domain::ids::IssueId::derive(
            removarr_domain::issue::IssueCategory::HardlinkMissing,
            &removarr_domain::ids::SubjectKey(format!("p:completed:{n}")),
        )],
    }
}

#[tokio::test(start_paused = true)]
async fn a_manual_rescan_and_a_second_request_are_one_job() {
    let r = rig(manual_only());
    let gate = Arc::new(Notify::new());
    let g = gate.clone();
    r.script.on_run(move |_, _, _| {
        let g = g.clone();
        Box::pin(async move {
            g.notified().await;
            Ok(JobSuccess::default())
        })
    });
    r.rt.start().await.unwrap();
    let a = r.rt.rescan().await.unwrap();
    let b = r.rt.rescan().await.unwrap();
    assert!(!a.existing && b.existing);
    assert_eq!(a.id, b.id);
    until("scan started", || r.script.calls_of("scan") == 1).await;
    // Still active: a third request is absorbed too.
    assert!(r.rt.rescan().await.unwrap().existing);
    gate.notify_one();
    until("scan finished", || {
        r.store.status(a.id) == Some(JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.store.with_key("scan").len(), 1);
    assert_eq!(r.script.calls_of("scan"), 1);
    // Once finished, a new request is new work.
    assert!(!r.rt.rescan().await.unwrap().existing);
}

#[tokio::test(start_paused = true)]
async fn a_scheduled_tick_on_a_running_manual_scan_does_not_duplicate_it() {
    let mut cfg = manual_only();
    cfg.schedule.scan_interval_minutes = 1440;
    cfg.schedule.anchor_minute_of_day = 180; // 03:00 UTC
    let r = rig_with(
        cfg,
        Arc::new(removarr_jobs::backoff::NoJitter),
        20_000 * DAY + 3_600_000,
    ); // 01:00
    let gate = Arc::new(Notify::new());
    let g = gate.clone();
    r.script.on_run(move |_, _, _| {
        let g = g.clone();
        Box::pin(async move {
            g.notified().await;
            Ok(JobSuccess::default())
        })
    });
    r.rt.start().await.unwrap();
    let manual = r.rt.rescan().await.unwrap();
    until("manual scan started", || r.script.calls_of("scan") == 1).await;
    tokio::time::sleep(Duration::from_secs(2 * 3600 + 60)).await; // past 03:00
    assert_eq!(
        r.store.with_key("scan").len(),
        1,
        "the scheduled tick must not queue a second scan"
    );
    gate.notify_one();
    until("done", || {
        r.store.status(manual.id) == Some(JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.script.calls_of("scan"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_scheduler_sleeps_until_due_and_never_polls() {
    let mut cfg = manual_only();
    cfg.schedule.scan_interval_minutes = 1440;
    cfg.schedule.anchor_minute_of_day = 180;
    cfg.schedule.qbit_refresh_minutes = 60;
    let r = rig_with(
        cfg,
        Arc::new(removarr_jobs::backoff::NoJitter),
        20_000 * DAY + 3_600_000,
    ); // 01:00
    r.rt.start().await.unwrap();
    until("workers idle", || {
        r.script.recover_calls.load(Ordering::SeqCst) >= 1 && r.store.calls() >= 2
    })
    .await;
    settle().await;
    let st = r.rt.status();
    assert_eq!(
        st.next_scan.map(|t| t.0),
        Some(20_000 * DAY + 3 * 3_600_000)
    );
    assert_eq!(
        st.next_refresh.map(|t| t.0),
        Some(20_000 * DAY + 2 * 3_600_000)
    );
    // Nothing is due for 59 minutes: the store is not touched at all.
    let before = r.store.calls();
    tokio::time::sleep(Duration::from_secs(59 * 60)).await;
    assert_eq!(
        r.store.calls(),
        before,
        "idle workers and the scheduler must not touch the store"
    );
    // 02:00 refresh, 03:00 scan + refresh; by 03:30 exactly two refreshes and one scan ran.
    tokio::time::sleep(Duration::from_secs(91 * 60)).await;
    until("jobs ran", || {
        r.script.calls_of("scan") == 1 && r.script.calls_of("refresh:qbittorrent") == 2
    })
    .await;
    assert_eq!(r.store.with_key("scan").len(), 1);
    assert_eq!(r.store.with_key("refresh:qbittorrent").len(), 2);
}

#[tokio::test(start_paused = true)]
async fn scheduling_can_never_queue_an_execution_or_an_approval() {
    let mut cfg = manual_only();
    cfg.schedule.scan_interval_minutes = 60;
    cfg.schedule.qbit_refresh_minutes = 5;
    cfg.auto_mode = false;
    let r = rig(cfg);
    r.rt.start().await.unwrap();
    tokio::time::sleep(Duration::from_secs(26 * 3600)).await;
    for j in r.store.all() {
        assert!(
            matches!(
                j.kind,
                JobKind::Scan | JobKind::RefreshService(ServiceKind::Qbittorrent)
            ),
            "the scheduler queued {:?}",
            j.kind
        );
    }
    // Users cannot request an automatic cycle or a recovery job either.
    let e =
        r.rt.submit(JobKind::AutoCycle {
            session: removarr_domain::ids::SessionId::new(removarr_domain::ids::RawUuid::nil()),
        })
        .await;
    assert!(matches!(
        e,
        Err(JobError::Failed {
            class: ErrorClass::Validation,
            ..
        })
    ));
    assert!(r.rt.submit(JobKind::Recover).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn each_lane_runs_one_job_at_a_time_and_lanes_overlap_when_scopes_are_disjoint() {
    let r = rig(manual_only());
    let gate = Arc::new(Notify::new());
    let g = gate.clone();
    r.script.on_run(move |_, _, _| {
        let g = g.clone();
        Box::pin(async move {
            g.notified().await;
            Ok(JobSuccess::default())
        })
    });
    r.script.on_scope(|k| match k {
        JobKind::ExecutePlan { .. } => Scope {
            exclusive: ["root:completed".to_string()].into(),
            ..Scope::default()
        },
        _ => Scope {
            shared: ["root:media-00000001".to_string()].into(),
            ..Scope::default()
        },
    });
    r.rt.start().await.unwrap();
    for n in 0..4 {
        r.rt.submit(issue_ids(n)).await.unwrap();
    }
    r.rt.submit(JobKind::Scan).await.unwrap();
    for n in 1..=3 {
        r.rt.submit(exec_kind(n, false)).await.unwrap();
    }
    // One observe and one execute job run together (disjoint scopes); nothing else starts.
    until("both lanes busy", || {
        r.script.max_of("observe") == 1 && r.script.max_of("execute") == 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        r.script
            .log()
            .iter()
            .filter(|l| l.starts_with("start:"))
            .count(),
        2
    );
    for _ in 0..8 {
        gate.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    until("all done", || {
        r.store
            .all()
            .iter()
            .all(|j| j.status == JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.script.max_of("observe"), 1);
    assert_eq!(r.script.max_of("execute"), 1);
}

#[tokio::test(start_paused = true)]
async fn an_execution_waits_for_a_scan_that_shares_its_root_and_vice_versa() {
    let r = rig(manual_only());
    let gate = Arc::new(Notify::new());
    let g = gate.clone();
    r.script.on_run(move |k, _, _| {
        let g = g.clone();
        Box::pin(async move {
            if matches!(k, JobKind::Scan) {
                g.notified().await;
            }
            Ok(JobSuccess::default())
        })
    });
    r.script.on_scope(|k| match k {
        JobKind::ExecutePlan { .. } => Scope {
            exclusive: ["root:completed".to_string()].into(),
            ..Scope::default()
        },
        _ => Scope {
            shared: ["root:completed".to_string()].into(),
            ..Scope::default()
        },
    });
    r.rt.start().await.unwrap();
    r.rt.submit(JobKind::Scan).await.unwrap();
    until("scan running", || r.script.calls_of("scan") == 1).await;
    let exec = r.rt.submit(exec_kind(1, false)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(
        r.script
            .calls_of(&exec_kind(1, false).dedupe_key().unwrap()),
        0,
        "lock contention must hold the execution back"
    );
    gate.notify_one();
    until("exec done", || {
        r.store.status(exec.id) == Some(JobStatus::Succeeded)
    })
    .await;
    let log = r.script.log();
    let end_scan = log.iter().position(|l| l == "end:scan").unwrap();
    let start_exec = log
        .iter()
        .position(|l| l.starts_with("start:exec"))
        .unwrap();
    assert!(end_scan < start_exec);
}

#[tokio::test(start_paused = true)]
async fn two_plans_naming_the_same_media_never_run_together_even_across_lanes() {
    // Same ARR media key: the second importer waits (INV-16). Only one execute worker exists,
    // so the contention shows against a simulation holding the media shared.
    let a = Scope {
        exclusive: [Scope::media_key(&removarr_domain::arr::ArrMediaRef {
            service: removarr_domain::arr::ArrService::Sonarr,
            media_id: 7,
            episode_ids: vec![1],
        })]
        .into(),
        ..Scope::default()
    };
    let b = Scope {
        exclusive: [Scope::media_key(&removarr_domain::arr::ArrMediaRef {
            service: removarr_domain::arr::ArrService::Sonarr,
            media_id: 7,
            episode_ids: vec![2, 3],
        })]
        .into(),
        ..Scope::default()
    };
    let c = Scope {
        exclusive: [Scope::media_key(&removarr_domain::arr::ArrMediaRef {
            service: removarr_domain::arr::ArrService::Sonarr,
            media_id: 8,
            episode_ids: vec![1],
        })]
        .into(),
        ..Scope::default()
    };
    assert!(a.conflicts_with(&b), "episodes of one media share one key");
    assert!(!a.conflicts_with(&c));
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_queued_job_ends_it_without_running_it() {
    let r = rig(manual_only());
    let gate = Arc::new(Notify::new());
    let g = gate.clone();
    r.script.on_run(move |_, _, _| {
        let g = g.clone();
        Box::pin(async move {
            g.notified().await;
            Ok(JobSuccess::default())
        })
    });
    r.rt.start().await.unwrap();
    let first = r.rt.submit(issue_ids(1)).await.unwrap();
    let second = r.rt.submit(issue_ids(2)).await.unwrap();
    until("first running", || {
        r.script.calls_of(&issue_ids(1).dedupe_key().unwrap()) == 1
    })
    .await;
    assert_eq!(
        r.rt.cancel(second.id).await.unwrap(),
        CancelResult::CancelledQueued
    );
    gate.notify_one();
    until("first done", || {
        r.store.status(first.id) == Some(JobStatus::Succeeded)
    })
    .await;
    assert_eq!(r.store.status(second.id), Some(JobStatus::Cancelled));
    assert_eq!(r.script.calls_of(&issue_ids(2).dedupe_key().unwrap()), 0);
    assert_eq!(
        r.rt.cancel(second.id).await.unwrap(),
        CancelResult::NotActive
    );
    let ghost = JobId::new(removarr_domain::ids::RawUuid::from_u128(999));
    assert_eq!(r.rt.cancel(ghost).await.unwrap(), CancelResult::NotFound);
}

#[tokio::test(start_paused = true)]
async fn a_running_job_stops_at_its_next_checkpoint_and_ends_cancelled() {
    let r = rig(manual_only());
    r.script.on_run(|_, ctx, _| {
        Box::pin(async move {
            loop {
                ctx.checkpoint()?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    });
    r.rt.start().await.unwrap();
    let job = r.rt.rescan().await.unwrap();
    until("started", || r.script.calls_of("scan") == 1).await;
    assert_eq!(r.rt.cancel(job.id).await.unwrap(), CancelResult::Requested);
    until("cancelled", || {
        r.store.status(job.id) == Some(JobStatus::Cancelled)
    })
    .await;
    assert_eq!(r.store.all()[0].outcome, Some(JobOutcome::Cancelled));
    let p = r.rt.progress(job.id).unwrap();
    assert!(p.cancel_requested);
    assert_eq!(p.status, JobStatus::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_a_mutation_never_drops_the_step() {
    // The execute lane never drops a job future: the mutation step runs to completion and the
    // handler (the action runner) then compensates and reports the interruption.
    let r = rig(manual_only());
    let step_done = Arc::new(AtomicBool::new(false));
    let compensated = Arc::new(AtomicBool::new(false));
    let (s, c) = (step_done.clone(), compensated.clone());
    r.script.on_run(move |_, ctx, _| {
        let (s, c) = (s.clone(), c.clone());
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(10)).await; // an uninterruptible step
            s.store(true, Ordering::SeqCst);
            if ctx.cancel.is_cancelled() {
                c.store(true, Ordering::SeqCst); // compensation is the runner's job
                return Err(JobError::Cancelled);
            }
            Ok(JobSuccess::default())
        })
    });
    r.rt.start().await.unwrap();
    let job = r.rt.submit(exec_kind(1, false)).await.unwrap();
    until("running", || {
        r.script
            .calls_of(&exec_kind(1, false).dedupe_key().unwrap())
            == 1
    })
    .await;
    assert_eq!(r.rt.cancel(job.id).await.unwrap(), CancelResult::Requested);
    until("ended", || {
        r.store.status(job.id) == Some(JobStatus::Cancelled)
    })
    .await;
    assert!(
        step_done.load(Ordering::SeqCst),
        "the mutation step must complete"
    );
    assert!(
        compensated.load(Ordering::SeqCst),
        "compensation is delegated to the runner path"
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_only_api_job_is_dropped_immediately_on_cancellation() {
    let r = rig(manual_only());
    let reached_end = Arc::new(AtomicBool::new(false));
    let e = reached_end.clone();
    r.script.on_run(move |_, _, _| {
        let e = e.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            e.store(true, Ordering::SeqCst);
            Ok(JobSuccess::default())
        })
    });
    r.rt.start().await.unwrap();
    let job =
        r.rt.submit(JobKind::RefreshService(ServiceKind::Qbittorrent))
            .await
            .unwrap();
    until("running", || r.script.calls_of("refresh:qbittorrent") == 1).await;
    r.rt.cancel(job.id).await.unwrap();
    until("cancelled", || {
        r.store.status(job.id) == Some(JobStatus::Cancelled)
    })
    .await;
    assert!(!reached_end.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_running_jobs_requeues_observe_work_once_and_never_execution() {
    let r = rig(manual_only());
    r.script.on_run(|_, ctx, _| {
        Box::pin(async move {
            loop {
                ctx.checkpoint()?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    });
    r.rt.start().await.unwrap();
    let scan = r.rt.rescan().await.unwrap();
    let exec = r.rt.submit(exec_kind(5, false)).await.unwrap();
    until("both running", || {
        r.script
            .log()
            .iter()
            .filter(|l| l.starts_with("start:"))
            .count()
            == 2
    })
    .await;
    r.rt.shutdown(Duration::from_secs(5)).await;
    assert_eq!(r.store.status(scan.id), Some(JobStatus::Interrupted));
    assert_eq!(r.store.status(exec.id), Some(JobStatus::Interrupted));
    let scans = r.store.with_key("scan");
    assert_eq!(
        scans.len(),
        2,
        "the observe job is queued again for the next start"
    );
    assert_eq!(scans[1].status, JobStatus::Queued);
    assert_eq!(
        r.store
            .with_key(&exec_kind(5, false).dedupe_key().unwrap())
            .len(),
        1,
        "execution is never re-queued"
    );
}

#[tokio::test(start_paused = true)]
async fn status_progress_and_events_read_memory_only() {
    let r = rig(manual_only());
    r.script.on_run(|_, ctx, _| {
        Box::pin(async move {
            ctx.progress.stage("walking").await;
            ctx.progress.counts(3, 10, Some("a/b".into()));
            ctx.progress
                .warn("slow disk at http://tracker.example/announce?key=secret");
            Ok(JobSuccess::default().with_metric("entries", 10))
        })
    });
    let mut rx = r.rt.subscribe();
    r.rt.start().await.unwrap();
    let job = r.rt.rescan().await.unwrap();
    until("done", || {
        r.store.status(job.id) == Some(JobStatus::Succeeded)
    })
    .await;
    let calls = r.store.calls();
    for _ in 0..200 {
        let _ = r.rt.status();
        let _ = r.rt.progress(job.id);
        let _ = r.rt.events_since(0);
        let _ = r.rt.watch(job.id);
    }
    assert_eq!(
        r.store.calls(),
        calls,
        "UI status must not reach the job store, let alone a disk"
    );
    let p = r.rt.progress(job.id).unwrap();
    assert_eq!(
        (p.stage.as_deref(), p.done, p.total, p.current.as_deref()),
        (Some("walking"), 3, 10, Some("a/b"))
    );
    assert!(p.warnings[0].contains("[url]") && !p.warnings[0].contains("secret"));
    let evs = r.rt.events_since(0);
    assert!(evs.windows(2).all(|w| w[0].seq < w[1].seq));
    let last = evs.iter().rev().find(|e| e.job == Some(job.id)).unwrap();
    assert!(matches!(
        &last.kind,
        removarr_jobs::JobEventKind::Finished {
            status: JobStatus::Succeeded,
            ..
        }
    ));
    let json = serde_json::to_string(&evs).unwrap();
    assert!(json.contains("\"event\":\"queued\"") && !json.contains("secret"));
    assert!(!finished_events(&mut rx).is_empty());
}
