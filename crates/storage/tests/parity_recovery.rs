//! Pass 2 gap tests: recovery of unfinished work across restarts, journal ordering across
//! plans, double-count prevention after a restart, and plan hash re-verification after reopen.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::PhysicalId;
use removarr_domain::gain::{GainKind, GainRecord, gain_id};
use removarr_domain::ids::{ActionRef, IntentId, PlanId, RawUuid};
use removarr_domain::journal::{IntentRecord, JournalMode, JournalStep, ResultRecord, StepOutcome};
use removarr_domain::plan::PlanStatus;
use removarr_domain::ports::{GainStore, JournalStore, PlanStore};
use removarr_domain::time::{MtimeNs, Timestamp};
use removarr_storage::{Db, Repos};

fn intent(n: u128, plan: PlanId, idx: u32, mode: JournalMode) -> IntentRecord {
    IntentRecord {
        id: IntentId::new(RawUuid::from_u128(0x7000 + n)),
        action: ActionRef { plan, index: idx },
        step: JournalStep::FsRemoveFile,
        mode,
        target: serde_json::json!({"path": "junk/a.nfo"}),
        observed: serde_json::json!({"size": 1}),
        expected_gain: removarr_domain::gain::GainEstimate::default(),
        hashes: BTreeSet::new(),
        created_at: Timestamp(5),
    }
}

fn done(i: &IntentRecord) -> ResultRecord {
    ResultRecord {
        intent_id: i.id,
        outcome: StepOutcome::Done {
            final_state: serde_json::json!({"gone": true}),
        },
        at: Timestamp(6),
    }
}

async fn reopen(f: &Fx) -> (Db, Repos) {
    let path = f.path();
    f.db.clone().close().await;
    let db = Db::open(&path).await.unwrap();
    let repos = Repos::new(&db, f.clock.clone());
    (db, repos)
}

#[tokio::test]
async fn unfinished_intents_across_plans_keep_journal_order_after_a_restart() {
    let f = fx().await;
    let p1 = executing_plan(&f, 1, 2).await;
    let p2 = executing_plan(&f, 2, 1).await;
    // Interleaved writes: p1#0, p2#0, p1#1. p1#0 completes, the others are in flight.
    let a = intent(1, p1.id, 0, JournalMode::Live);
    let b = intent(2, p2.id, 0, JournalMode::Live);
    let c = intent(3, p1.id, 1, JournalMode::Live);
    let sim = intent(4, p2.id, 0, JournalMode::Simulated);
    for i in [&a, &b, &c, &sim] {
        f.repos.journal.record_intent(i).await.unwrap();
    }
    f.repos.journal.record_result(&done(&a)).await.unwrap();

    let (db, repos) = reopen(&f).await;
    let snap = repos.recovery.snapshot().await.unwrap();
    let ids: Vec<_> = snap.unfinished.iter().map(|u| u.intent.id).collect();
    assert_eq!(ids, vec![b.id, c.id], "oldest first, simulated excluded");
    let by_plan = snap.unfinished_by_plan();
    assert_eq!(by_plan[&p1.id].len(), 1);
    assert_eq!(by_plan[&p2.id].len(), 1);
    let mut executing = snap.executing_plans.clone();
    executing.sort();
    let mut expect = vec![p1.id, p2.id];
    expect.sort();
    assert_eq!(executing, expect);
    db.close().await;
}

#[tokio::test]
async fn an_executing_plan_without_open_intents_still_needs_recovery() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let i = intent(1, p.id, 0, JournalMode::Live);
    f.repos.journal.record_intent(&i).await.unwrap();
    f.repos.journal.record_result(&done(&i)).await.unwrap();
    // Crash before the plan was closed: no unfinished intent, but the plan never resumes.
    let (db, repos) = reopen(&f).await;
    let snap = repos.recovery.snapshot().await.unwrap();
    assert!(snap.unfinished.is_empty());
    assert_eq!(snap.executing_plans, vec![p.id]);
    assert!(snap.needs_recovery());
    db.close().await;
}

#[tokio::test]
async fn a_fresh_database_needs_no_recovery() {
    let f = fx().await;
    let snap = f.repos.recovery.snapshot().await.unwrap();
    assert!(!snap.needs_recovery());
    assert!(snap.running_jobs.is_empty());
}

#[tokio::test]
async fn a_recorded_gain_cannot_be_counted_again_after_a_restart() {
    let f = fx().await;
    let p = executing_plan(&f, 1, 1).await;
    let phys = PhysicalId { dev: 1, ino: 42 };
    let g = GainRecord {
        gain_id: gain_id(phys, 500, MtimeNs(3)),
        action: ActionRef {
            plan: p.id,
            index: 0,
        },
        kind: GainKind::AttributableDeletion,
        phys,
        bytes: 500,
        recorded_at: Timestamp(1),
    };
    f.repos.gains.record(&[g.clone()], None).await.unwrap();
    let (db, repos) = reopen(&f).await;
    let err = repos.gains.record(&[g], None).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
    let t = repos.gains.totals().await.unwrap();
    assert_eq!((t.attributable, t.duplicates), (500, 0));
    db.close().await;
}

#[tokio::test]
async fn approved_plan_hash_is_reverified_after_a_restart() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 2).await;
    let approval = approval_for(&f, &p);
    f.repos.plans.approve(&approval).await.unwrap();
    let (db, repos) = reopen(&f).await;
    let (stored, hash, status, stored_approval) = repos.plans.get(p.id).await.unwrap().unwrap();
    assert_eq!(stored, p);
    assert_eq!(hash, approval.plan_hash);
    assert_eq!(status, PlanStatus::Approved);
    assert_eq!(stored_approval.unwrap().plan_hash, approval.plan_hash);
    // Approving again is refused: the plan is no longer a draft.
    assert!(repos.plans.approve(&approval).await.is_err());
    // A status change that would skip the approval gate is refused.
    assert!(
        repos
            .plans
            .transition(p.id, PlanStatus::Approved, PlanStatus::Draft)
            .await
            .is_err()
    );
    db.close().await;
}
