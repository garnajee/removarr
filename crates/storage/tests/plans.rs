#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::plan::{HashedPlan, PlanHash, PlanStatus};
use removarr_domain::ports::PlanStore;
use removarr_domain::time::Timestamp;
use sqlx::Row;

#[tokio::test]
async fn insert_then_get_returns_the_verified_plan() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 3).await;
    let (back, hash, status, approval) = f.repos.plans.get(p.id).await.unwrap().unwrap();
    assert_eq!(back, p);
    assert_eq!(hash, PlanHash::compute(&p).unwrap());
    assert_eq!((status, approval), (PlanStatus::Draft, None));
    assert_eq!(f.repos.plans.actions(p.id).await.unwrap().len(), 3);
    assert!(f.repos.plans.get(pid(99)).await.unwrap().is_none());
}

#[tokio::test]
async fn insert_rejects_a_wrong_hash_and_stores_nothing() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let p = plan(&f, 1, s.id, 2);
    let err = f
        .repos
        .plans
        .insert(&p, &PlanHash("0".repeat(64)))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
    assert!(f.repos.plans.get(p.id).await.unwrap().is_none());
}

#[tokio::test]
async fn insert_with_an_unknown_session_stores_neither_plan_nor_actions() {
    // The session FK fails the plan insert; no action row may remain.
    let f = fx().await;
    let p = plan(&f, 1, sid(404), 2);
    let sealed = HashedPlan::seal(p.clone()).unwrap();
    let err = f.repos.plans.insert(&p, sealed.hash()).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
    let mut raw = f.raw().await;
    let n: i64 = sqlx::query("SELECT COUNT(*) FROM plan_actions")
        .fetch_one(&mut raw)
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn the_same_plan_cannot_be_inserted_twice() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    let sealed = HashedPlan::seal(p.clone()).unwrap();
    let err = f.repos.plans.insert(&p, sealed.hash()).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
}

#[tokio::test]
async fn tampered_stored_plan_is_detected_on_read() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    // The immutability trigger blocks UPDATE, so tamper by rewriting the table.
    let mut raw = f.raw().await;
    sqlx::query("DROP TRIGGER plans_immutable")
        .execute(&mut raw)
        .await
        .unwrap();
    sqlx::query("UPDATE plans SET body = replace(body, 'cfg', 'evil')")
        .execute(&mut raw)
        .await
        .unwrap();
    let err = f.repos.plans.get(p.id).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Integrity);
}

#[tokio::test]
async fn approval_requires_draft_hash_and_unexpired() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 2).await;

    let mut wrong = approval_for(&f, &p);
    wrong.plan_hash = PlanHash("f".repeat(64));
    assert_eq!(
        f.repos.plans.approve(&wrong).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    assert_eq!(
        f.repos.plans.get(p.id).await.unwrap().unwrap().2,
        PlanStatus::Draft
    );

    let a = approval_for(&f, &p);
    f.repos.plans.approve(&a).await.unwrap();
    let (_, _, status, stored) = f.repos.plans.get(p.id).await.unwrap().unwrap();
    assert_eq!(status, PlanStatus::Approved);
    assert_eq!(stored, Some(a.clone()));

    // A second approval is a conflict (no longer a draft) and leaves the first untouched.
    assert_eq!(
        f.repos.plans.approve(&a).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    // Another plan is unaffected.
    let other = insert_plan(&f, 2, 1).await;
    f.repos
        .plans
        .approve(&approval_for(&f, &other))
        .await
        .unwrap();
}

#[tokio::test]
async fn expiry_boundary_is_inclusive_and_blocks_approval() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    let a = approval_for(&f, &p);
    f.clock.set(p.expires_at);
    assert_eq!(
        f.repos.plans.approve(&a).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    f.clock.set(Timestamp(p.expires_at.0 - 1));
    f.repos.plans.approve(&a).await.unwrap();
}

#[tokio::test]
async fn unknown_plan_approval_is_not_found() {
    let f = fx().await;
    let p = plan(&f, 1, sid(1), 1);
    let err = f
        .repos
        .plans
        .approve(&approval_for(&f, &p))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::NotFound);
}

#[tokio::test]
async fn transitions_follow_the_domain_table_and_are_compare_and_swap() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    let t = |from, to| f.repos.plans.transition(p.id, from, to);

    // Not allowed by the table, even though the stored status matches.
    assert_eq!(
        t(PlanStatus::Draft, PlanStatus::Executing)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );
    // Approval has its own method.
    assert_eq!(
        t(PlanStatus::Draft, PlanStatus::Approved)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Validation
    );
    // Stale `from`.
    assert_eq!(
        t(PlanStatus::Approved, PlanStatus::Executing)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Conflict
    );
    // Unknown plan.
    let err = f
        .repos
        .plans
        .transition(pid(77), PlanStatus::Draft, PlanStatus::Discarded)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::NotFound);

    f.repos.plans.approve(&approval_for(&f, &p)).await.unwrap();
    t(PlanStatus::Approved, PlanStatus::Executing)
        .await
        .unwrap();
    t(PlanStatus::Executing, PlanStatus::Interrupted)
        .await
        .unwrap();
    // Interrupted is terminal: it never resumes.
    assert!(
        t(PlanStatus::Interrupted, PlanStatus::Executing)
            .await
            .is_err()
    );
    assert_eq!(
        f.repos.plans.get(p.id).await.unwrap().unwrap().2,
        PlanStatus::Interrupted
    );
}

#[tokio::test]
async fn the_database_refuses_an_approved_status_without_an_approval_row() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    let mut raw = f.raw().await;
    let r = sqlx::query("UPDATE plans SET status = 'executing' WHERE id = ?")
        .bind(p.id.0.to_string())
        .execute(&mut raw)
        .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn concurrent_approvals_let_exactly_one_win() {
    let f = fx().await;
    let p = insert_plan(&f, 1, 1).await;
    let a = approval_for(&f, &p);
    let mut handles = vec![];
    for _ in 0..8 {
        let repo = f.repos.plans.clone();
        let a = a.clone();
        handles.push(tokio::spawn(async move { repo.approve(&a).await }));
    }
    let mut ok = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(()) => ok += 1,
            Err(e) => assert_eq!(e.class, ErrorClass::Conflict),
        }
    }
    assert_eq!(ok, 1);
}
