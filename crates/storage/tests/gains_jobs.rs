#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::BTreeMap;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::PhysicalId;
use removarr_domain::gain::{GainKind, GainRecord, ObservedDelta, gain_id};
use removarr_domain::ids::{ActionRef, JobId, RawUuid};
use removarr_domain::jobs::{JobKind, JobOutcome, JobRecord, JobStatus, Lane};
use removarr_domain::path::RootId;
use removarr_domain::ports::{GainStore, JobStore};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::{MtimeNs, Timestamp};
use sqlx::Row;

fn gain(
    plan: removarr_domain::ids::PlanId,
    idx: u32,
    ino: u64,
    bytes: u64,
    kind: GainKind,
) -> GainRecord {
    let phys = PhysicalId { dev: 1, ino };
    GainRecord {
        gain_id: gain_id(phys, bytes, MtimeNs(7)),
        action: ActionRef { plan, index: idx },
        kind,
        phys,
        bytes,
        recorded_at: Timestamp(1),
    }
}

#[tokio::test]
async fn a_gain_id_is_counted_once_across_batches_and_plans() {
    let f = fx().await;
    let p1 = executing_plan(&f, 1, 2).await;
    let p2 = executing_plan(&f, 2, 1).await;
    let g = gain(p1.id, 0, 10, 100, GainKind::AttributableDeletion);
    f.repos.gains.record(&[g.clone()], None).await.unwrap();

    // Same file version reported again by the same plan, and by another plan: both refused.
    let err = f.repos.gains.record(&[g.clone()], None).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
    let mut other = g.clone();
    other.action = ActionRef {
        plan: p2.id,
        index: 0,
    };
    assert_eq!(
        f.repos
            .gains
            .record(&[other], None)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );
    // Same id twice inside one batch.
    let g2 = gain(p1.id, 1, 11, 50, GainKind::AttributableConsolidation);
    assert_eq!(
        f.repos
            .gains
            .record(&[g2.clone(), g2], None)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );

    let t = f.repos.gains.totals().await.unwrap();
    assert_eq!(
        (t.attributable, t.added, t.net, t.duplicates),
        (100, 0, 100, 0)
    );
}

#[tokio::test]
async fn a_failing_batch_rolls_back_every_row_and_the_delta() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let good = gain(p.id, 0, 10, 100, GainKind::AttributableDeletion);
    let bad = gain(p.id, 9, 11, 5, GainKind::AddedCost); // action 9 does not exist: FK failure
    let delta = ObservedDelta {
        root: RootId::completed(),
        free_before: 1,
        free_after_local: 2,
        free_after_services: None,
    };
    let err = f
        .repos
        .gains
        .record(&[good, bad], Some(&delta))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
    let c = f.repos.retention.audit_counts().await.unwrap();
    assert_eq!((c.gains, c.observed_deltas), (0, 0));
}

#[tokio::test]
async fn deltas_and_added_costs_are_recorded_and_totals_net_out() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 2).await;
    let delta = ObservedDelta {
        root: RootId::completed(),
        free_before: u64::MAX,
        free_after_local: 5,
        free_after_services: Some(6),
    };
    f.repos
        .gains
        .record(
            &[
                gain(p.id, 0, 10, 100, GainKind::AttributableDeletion),
                gain(p.id, 1, 11, 30, GainKind::AddedCost),
            ],
            Some(&delta),
        )
        .await
        .unwrap();
    let t = f.repos.gains.totals().await.unwrap();
    assert_eq!((t.attributable, t.added, t.net), (100, 30, 70));
    let mut raw = f.raw().await;
    let row = sqlx::query("SELECT free_before, plan_id FROM observed_deltas")
        .fetch_one(&mut raw)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>(0), -1, "u64::MAX keeps its 64 bits");
    assert_eq!(row.get::<String, _>(1), p.id.0.to_string());
    assert!(
        sqlx::query("DELETE FROM gains")
            .execute(&mut raw)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn gains_require_an_existing_action() {
    let f = fx().await;
    let err = f
        .repos
        .gains
        .record(
            &[gain(pid(5), 0, 1, 1, GainKind::AttributableDeletion)],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
}

fn job(n: u128, kind: JobKind, at: i64) -> JobRecord {
    JobRecord::queued(
        JobId::new(RawUuid::from_u128(0x3000 + n)),
        kind,
        Timestamp(at),
    )
    .unwrap()
}

#[tokio::test]
async fn dedupe_applies_while_active_and_releases_when_finished() {
    let f = fx().await;
    let a = job(1, JobKind::Scan, 10);
    let b = job(2, JobKind::Scan, 11);
    assert_eq!(f.repos.jobs.enqueue(&a).await.unwrap(), a.id);
    // Duplicate while queued: the existing id comes back and nothing is written.
    assert_eq!(f.repos.jobs.enqueue(&b).await.unwrap(), a.id);
    assert!(f.repos.jobs.get(b.id).await.unwrap().is_none());

    let claimed = f
        .repos
        .jobs
        .claim_next(Lane::Observe)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (claimed.id, claimed.status, claimed.attempts),
        (a.id, JobStatus::Running, 1)
    );
    assert_eq!(
        f.repos.jobs.enqueue(&b).await.unwrap(),
        a.id,
        "running still dedupes"
    );

    f.repos
        .jobs
        .finish(
            a.id,
            &JobOutcome::Succeeded {
                metrics: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.repos.jobs.enqueue(&b).await.unwrap(), b.id);
}

#[tokio::test]
async fn the_unique_index_is_enforced_by_the_database() {
    let f = fx().await;
    f.repos
        .jobs
        .enqueue(&job(1, JobKind::Scan, 10))
        .await
        .unwrap();
    let mut raw = f.raw().await;
    let r = sqlx::query(
        "INSERT INTO jobs (id, kind, dedupe_key, lane, status, enqueued_at, attempts) \
         VALUES ('x', '{}', 'scan', 'observe', 'queued', 1, 0)",
    )
    .execute(&mut raw)
    .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn claims_are_fifo_per_lane_and_validated() {
    let f = fx().await;
    let r1 = job(1, JobKind::RefreshService(ServiceKind::Sonarr), 10);
    let r2 = job(2, JobKind::RefreshService(ServiceKind::Radarr), 10);
    let e = job(3, JobKind::Recover, 5);
    for j in [&r1, &r2, &e] {
        f.repos.jobs.enqueue(j).await.unwrap();
    }
    assert_eq!(
        f.repos
            .jobs
            .claim_next(Lane::Observe)
            .await
            .unwrap()
            .unwrap()
            .id,
        r1.id
    );
    assert_eq!(
        f.repos
            .jobs
            .claim_next(Lane::Observe)
            .await
            .unwrap()
            .unwrap()
            .id,
        r2.id
    );
    assert!(
        f.repos
            .jobs
            .claim_next(Lane::Observe)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.repos
            .jobs
            .claim_next(Lane::Execute)
            .await
            .unwrap()
            .unwrap()
            .id,
        e.id
    );

    let mut bad = job(4, JobKind::Scan, 1);
    bad.dedupe_key = "other".into();
    assert_eq!(
        f.repos.jobs.enqueue(&bad).await.unwrap_err().class,
        ErrorClass::Validation
    );
    let mut running = job(5, JobKind::Scan, 1);
    running.status = JobStatus::Running;
    assert_eq!(
        f.repos.jobs.enqueue(&running).await.unwrap_err().class,
        ErrorClass::Validation
    );
}

#[tokio::test]
async fn stage_updates_skip_unchanged_values_and_need_a_running_job() {
    let f = fx().await;
    let a = job(1, JobKind::Scan, 10);
    f.repos.jobs.enqueue(&a).await.unwrap();
    assert_eq!(
        f.repos
            .jobs
            .set_stage(a.id, "walk")
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );
    f.repos.jobs.claim_next(Lane::Observe).await.unwrap();
    f.repos.jobs.set_stage(a.id, "walk").await.unwrap();
    f.repos.jobs.set_stage(a.id, "walk").await.unwrap();
    assert_eq!(
        f.repos
            .jobs
            .get(a.id)
            .await
            .unwrap()
            .unwrap()
            .stage
            .as_deref(),
        Some("walk")
    );
    assert_eq!(
        f.repos
            .jobs
            .set_stage(job(9, JobKind::Scan, 1).id, "x")
            .await
            .unwrap_err()
            .class,
        ErrorClass::NotFound
    );
}

#[tokio::test]
async fn finish_follows_the_job_machine_and_scrubs_messages() {
    let f = fx().await;
    let a = job(1, JobKind::Scan, 10);
    f.repos.jobs.enqueue(&a).await.unwrap();
    let failed = JobOutcome::Failed {
        class: ErrorClass::Unavailable,
        message: "GET http://qbit.local:8080/api failed apikey=hunter2".into(),
    };
    // Queued -> Failed is not allowed.
    assert_eq!(
        f.repos.jobs.finish(a.id, &failed).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    f.repos.jobs.claim_next(Lane::Observe).await.unwrap();
    f.repos.jobs.finish(a.id, &failed).await.unwrap();
    let stored = f.repos.jobs.get(a.id).await.unwrap().unwrap();
    let JobOutcome::Failed { message, .. } = stored.outcome.unwrap() else {
        panic!("outcome")
    };
    assert!(!message.contains("qbit.local") && !message.contains("hunter2"));
    assert_eq!(
        f.repos.jobs.finish(a.id, &failed).await.unwrap_err().class,
        ErrorClass::Conflict
    );
}

#[tokio::test]
async fn running_jobs_are_interrupted_at_startup() {
    let f = fx().await;
    let a = job(1, JobKind::Scan, 10);
    let b = job(2, JobKind::Recover, 11);
    let c = job(3, JobKind::RefreshService(ServiceKind::Jellyfin), 12);
    for j in [&a, &b, &c] {
        f.repos.jobs.enqueue(j).await.unwrap();
    }
    f.repos.jobs.claim_next(Lane::Observe).await.unwrap(); // a runs; c stays queued
    assert_eq!(
        f.repos
            .recovery
            .snapshot()
            .await
            .unwrap()
            .running_jobs
            .len(),
        1
    );
    let interrupted = f.repos.jobs.interrupt_running().await.unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(
        (interrupted[0].id, interrupted[0].status),
        (a.id, JobStatus::Interrupted)
    );
    assert_eq!(
        f.repos.jobs.get(b.id).await.unwrap().unwrap().status,
        JobStatus::Queued
    );
    assert!(f.repos.jobs.interrupt_running().await.unwrap().is_empty());
    // The dedupe slot of the interrupted job is free again.
    assert_eq!(
        f.repos
            .jobs
            .enqueue(&job(4, JobKind::Scan, 20))
            .await
            .unwrap()
            .0,
        RawUuid::from_u128(0x3004)
    );
}
