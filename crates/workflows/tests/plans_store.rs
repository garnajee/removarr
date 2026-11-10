#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The planner end to end with the real SQLite repositories: immutable persistence, previews
//! that persist nothing, approval records, expiry, invalidation after filesystem changes
//! (legacy T-16, plan side) and the execution ticket handed to the action runner.

mod common;
mod plans_common;

use common::*;
use plans_common::*;
use removarr_domain::error::DomainError;
use removarr_domain::evidence::ContentProof;
use removarr_domain::fs::EntryKind;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::{ApprovalRequest, PlanMode, PlanStatus};
use removarr_domain::ports::PlanStore;
use removarr_domain::time::MtimeNs;
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{
    IssueAction as A, PlanError, PlanRequest, PlanSettings, Planner, StaleReason, preview,
};

fn observation() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .file("Movie.A.2020.mkv", 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .file("Orphan.Rel/Orphan.Rel.nfo", 21, 100)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .file("movies/Movie.A.2020.mkv", 11, 1000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            "Movie.A.2020.mkv",
            &[("Movie.A.2020.mkv", "Movie.A.2020.mkv")],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, "Movie.A.2020.mkv"),
        rp(&m, "movies/Movie.A.2020.mkv"),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 1000, 1, 0o644),
        ContentProof::NameSize,
    )];
    o
}

async fn world() -> StoreWorld {
    let w = store_world().await;
    store_obs(&w, observation(), 7).await;
    w
}

async fn store(w: &StoreWorld, o: Observation, n: u128) {
    store_obs(w, o, n).await;
}

fn planner(w: &StoreWorld, settings: PlanSettings) -> Planner {
    store_planner(w, settings, vec![])
}

fn folder_request() -> PlanRequest {
    let subject = SubjectKey::rooted(&rp(&completed(), "Orphan.Rel"));
    let id = removarr_domain::ids::IssueId::derive(C::OrphanCompleted, &subject);
    req(C::OrphanCompleted, vec![id], A::Delete)
}

fn approval(hash: &removarr_domain::plan::PlanHash, count: u32) -> ApprovalRequest {
    ApprovalRequest {
        plan_hash: hash.clone(),
        confirmed_count: count,
        confirmed_torrent_removals: 0,
        acknowledged_samples: vec![],
        acknowledged_unrecoverable: vec![],
    }
}

#[tokio::test]
async fn plans_are_persisted_immutably_and_previews_persist_nothing() {
    let w = world().await;
    let p = planner(&w, live());
    let pv = p.preview(&folder_request(), vec![]).await.unwrap();
    assert!(
        w.repos
            .plans
            .get(pv.plan.plan().id)
            .await
            .unwrap()
            .is_none(),
        "a preview is not stored"
    );
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    let stored = p.get(id).await.unwrap();
    assert_eq!(stored.status, PlanStatus::Draft);
    assert_eq!(stored.plan.hash(), built.plan.hash());
    assert_eq!(
        preview(&stored.plan, Some(stored.status)).actions,
        preview(&built.plan, None).actions
    );
    // The same plan cannot be stored twice, and a stored plan cannot be rewritten.
    let again = w
        .repos
        .plans
        .insert(built.plan.plan(), built.plan.hash())
        .await;
    assert!(again.is_err());
    // The preview and the stored plan were built by the same path.
    let (mut a, b) = (pv.plan.plan().clone(), built.plan.plan().clone());
    a.id = b.id;
    assert_eq!(a, b);
}

#[tokio::test]
async fn approval_records_the_exact_hash_counts_and_hands_a_ticket_to_the_runner() {
    let w = world().await;
    let p = planner(&w, live());
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    // A live execution needs an approval first; a simulation does not.
    assert!(p.ticket(id, false).await.is_err());
    assert!(p.ticket(id, true).await.unwrap().approval.is_none());
    let wrong = p.approve(id, &approval(built.plan.hash(), 2), vec![]).await;
    assert!(matches!(
        wrong,
        Err(PlanError::Domain(DomainError::ApprovalMismatch {
            field: "confirmed_count",
            ..
        }))
    ));
    let mut other = approval(built.plan.hash(), 1);
    other.plan_hash = removarr_domain::plan::PlanHash("f".repeat(64));
    assert!(matches!(
        p.approve(id, &other, vec![]).await,
        Err(PlanError::Domain(DomainError::PlanHashMismatch { .. }))
    ));
    let ok = p
        .approve(id, &approval(built.plan.hash(), 1), vec![])
        .await
        .unwrap();
    assert_eq!(&ok.plan_hash, built.plan.hash());
    let t = p.ticket(id, false).await.unwrap();
    assert_eq!(t.status, PlanStatus::Approved);
    assert_eq!(t.approval.unwrap().plan_hash, *built.plan.hash());
    // Approving twice is refused.
    assert!(
        p.approve(id, &approval(built.plan.hash(), 1), vec![])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_filesystem_change_after_planning_invalidates_the_plan() {
    let w = world().await;
    let p = planner(&w, live());
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    // T-16: a new file appears in the folder after the scan the plan was built on.
    let mut o = observation();
    o.inventories[0] = Inv::new(completed())
        .file("Movie.A.2020.mkv", 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .file("Orphan.Rel/Orphan.Rel.nfo", 21, 100)
        .file("Orphan.Rel/new.srt", 23, 10)
        .build();
    store(&w, o, 8).await;
    let check = p.refresh_status(id, vec![]).await.unwrap();
    assert_eq!(check.status, PlanStatus::Invalidated);
    assert!(check.reasons.iter().any(
        |r| matches!(r, StaleReason::DirChanged { dir } if *dir == rp(&completed(), "Orphan.Rel"))
    ));
    assert!(
        p.approve(id, &approval(built.plan.hash(), 1), vec![])
            .await
            .is_err()
    );
    assert!(
        p.ticket(id, true).await.is_err(),
        "an invalidated plan is not even simulated"
    );
}

#[tokio::test]
async fn a_changed_file_signature_invalidates_and_an_expired_plan_cannot_run() {
    let w = world().await;
    let p = planner(&w, live());
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    // mtime of a member changes.
    let mut o = observation();
    let e = o.inventories[0]
        .entries
        .iter_mut()
        .find(|e| e.path == rp(&completed(), "Orphan.Rel/Orphan.Rel.nfo"))
        .unwrap();
    e.sig.mtime_ns = MtimeNs(e.sig.mtime_ns.0 + 1);
    store(&w, o, 8).await;
    let check = p.refresh_status(id, vec![]).await.unwrap();
    assert_eq!(check.status, PlanStatus::Invalidated);

    // Expiry: only time passed -> Expired.
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    w.clock.advance_hours(25);
    let r = p.approve(id, &approval(built.plan.hash(), 1), vec![]).await;
    assert!(
        matches!(
            r,
            Err(PlanError::Stale {
                status: PlanStatus::Expired,
                ..
            })
        ),
        "{r:?}"
    );
    assert_eq!(p.get(id).await.unwrap().status, PlanStatus::Expired);
}

#[tokio::test]
async fn approved_plans_expire_before_execution_and_dry_run_plans_only_simulate() {
    let w = world().await;
    let p = planner(&w, live());
    let built = p.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    p.approve(id, &approval(built.plan.hash(), 1), vec![])
        .await
        .unwrap();
    w.clock.advance_hours(25);
    assert!(matches!(
        p.ticket(id, false).await,
        Err(PlanError::Domain(DomainError::PlanExpired { .. }))
    ));
    assert_eq!(p.get(id).await.unwrap().status, PlanStatus::Expired);

    let mut dry = live();
    dry.mode = PlanMode::DryRun;
    let d = planner(&w, dry);
    w.clock.set(NOW);
    let built = d.create(&folder_request(), vec![]).await.unwrap();
    let id = built.plan.plan().id;
    d.approve(id, &approval(built.plan.hash(), 1), vec![])
        .await
        .unwrap();
    assert!(
        d.ticket(id, false).await.is_err(),
        "a dry-run plan never runs live"
    );
    assert!(d.ticket(id, true).await.unwrap().simulate);
    d.discard(id).await.unwrap();
    assert_eq!(d.get(id).await.unwrap().status, PlanStatus::Discarded);
    assert!(d.ticket(id, true).await.is_err());
}

#[tokio::test]
async fn stale_or_foreign_ids_are_rejected_through_the_planner() {
    let w = world().await;
    let p = planner(&w, live());
    let bogus = removarr_domain::ids::IssueId("0".repeat(32));
    let r = p
        .create(&req(C::OrphanCompleted, vec![bogus], A::Delete), vec![])
        .await;
    assert!(matches!(r, Err(PlanError::RejectedSelection { .. })));
}
