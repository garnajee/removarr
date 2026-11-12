#![allow(clippy::unwrap_used, clippy::expect_used)]
//! How the runtime starts (and refuses to start) an automatic cycle: only after a scheduled,
//! complete and clean scan, only when automatic mode is on and not paused.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use removarr_domain::ids::{RawUuid, SessionId};
use removarr_domain::jobs::{JobKind, JobStatus};
use removarr_jobs::backoff::NoJitter;
use removarr_jobs::handler::{JobSuccess, ScanSummary};

fn scan_ok(
    complete: bool,
    uncertainties: Vec<&str>,
) -> impl Fn(JobKind, removarr_jobs::JobCtx, u32) -> Fut + Send + Sync + 'static {
    let uncertainties: Vec<String> = uncertainties.into_iter().map(String::from).collect();
    move |_, _, _| {
        let u = uncertainties.clone();
        Box::pin(async move {
            Ok(JobSuccess {
                scan: Some(ScanSummary {
                    session: SessionId::new(RawUuid::from_u128(77)),
                    complete,
                    uncertainties: u,
                }),
                ..JobSuccess::default()
            })
        })
    }
}

fn scheduled(auto: bool) -> Rig {
    let mut cfg = manual_only();
    cfg.schedule.scan_interval_minutes = 1440;
    cfg.schedule.anchor_minute_of_day = 180;
    cfg.auto_mode = auto;
    rig_with(cfg, Arc::new(NoJitter), 20_000 * DAY + 3_600_000)
}

async fn through_the_schedule(r: &Rig) {
    r.rt.start().await.unwrap();
    tokio::time::sleep(Duration::from_secs(2 * 3600 + 120)).await;
    until("scheduled scan ran", || r.script.calls_of("scan") == 1).await;
    settle().await;
}

fn cycle_key() -> String {
    JobKind::AutoCycle {
        session: SessionId::new(RawUuid::from_u128(77)),
    }
    .dedupe_key()
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_complete_scheduled_scan_queues_the_cycle_for_its_session() {
    let r = scheduled(true);
    r.script.on_run(scan_ok(true, vec![]));
    through_the_schedule(&r).await;
    until("cycle ran", || r.script.calls_of(&cycle_key()) == 1).await;
    let cycle = &r.store.with_key(&cycle_key())[0];
    assert_eq!(cycle.lane, removarr_domain::jobs::Lane::Execute);
    until("cycle done", || {
        r.store.with_key(&cycle_key())[0].status == JobStatus::Succeeded
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_or_uncertain_scheduled_scan_pauses_instead_of_cycling() {
    for (complete, why) in [
        (false, "root completed was not observed"),
        (true, "ServiceUnavailable(Sonarr): down"),
        (true, "MappingError: no rule"),
    ] {
        let r = scheduled(true);
        r.script.on_run(scan_ok(complete, vec![why]));
        through_the_schedule(&r).await;
        until("paused", || r.rt.status().auto_paused.is_some()).await;
        assert!(r.rt.status().auto_paused.unwrap().contains(why));
        assert!(
            r.store.with_key(&cycle_key()).is_empty(),
            "no cycle after an uncertain scan: {why}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manual_scans_off_and_paused_never_start_a_cycle() {
    // A manual scan, however complete.
    let r = scheduled(true);
    r.script.on_run(scan_ok(true, vec![]));
    r.rt.start().await.unwrap();
    let job = r.rt.rescan().await.unwrap();
    until("manual scan done", || {
        r.store.status(job.id) == Some(JobStatus::Succeeded)
    })
    .await;
    settle().await;
    assert!(r.store.with_key(&cycle_key()).is_empty());
    assert_eq!(r.rt.status().auto_paused, None);

    // auto_mode off: no cycle and no pause.
    let r = scheduled(false);
    r.script.on_run(scan_ok(false, vec!["x"]));
    through_the_schedule(&r).await;
    assert!(r.store.with_key(&cycle_key()).is_empty());
    assert_eq!(r.rt.status().auto_paused, None);

    // Already paused: still no cycle, and the first reason is kept.
    let r = scheduled(true);
    r.script.on_run(scan_ok(true, vec![]));
    r.rt.start().await.unwrap();
    r.rt.auto().pause("first reason").await;
    r.rt.auto().pause("second reason").await;
    tokio::time::sleep(Duration::from_secs(2 * 3600 + 120)).await;
    until("scheduled scan ran", || r.script.calls_of("scan") == 1).await;
    settle().await;
    assert!(r.store.with_key(&cycle_key()).is_empty());
    assert_eq!(r.rt.status().auto_paused.as_deref(), Some("first reason"));
    assert!(r.rt.resume_auto().await.unwrap());
    assert!(
        !r.rt.resume_auto().await.unwrap(),
        "resuming twice is a no-op"
    );
}

#[tokio::test(start_paused = true)]
async fn f06_pending_automatic_plans_are_discarded_at_startup_and_whenever_automatic_mode_pauses() {
    use std::sync::atomic::Ordering;
    let r = scheduled(true);
    r.script
        .on_run(scan_ok(false, vec!["root completed was not observed"]));
    r.rt.start().await.unwrap();
    settle().await;
    assert_eq!(r.script.discards.load(Ordering::SeqCst), 1, "at startup");
    tokio::time::sleep(Duration::from_secs(2 * 3600 + 120)).await;
    until("paused by the scheduled scan", || {
        r.rt.status().auto_paused.is_some()
    })
    .await;
    until("discarded after the pause", || {
        r.script.discards.load(Ordering::SeqCst) == 2
    })
    .await;
}
