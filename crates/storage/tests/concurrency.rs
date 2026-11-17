#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use removarr_domain::path::RootId;
use removarr_domain::plan::{HashedPlan, PlanStatus};
use removarr_domain::ports::{PlanStore, ScanStore};

#[tokio::test]
async fn readers_never_block_and_never_see_half_a_transaction() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let stop = Arc::new(AtomicBool::new(false));

    let mut readers = vec![];
    for _ in 0..6 {
        let repos = f.repos.clone();
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let mut seen = 0usize;
            while !stop.load(Ordering::Relaxed) {
                for id in repos
                    .plans
                    .ids_with_status(PlanStatus::Draft)
                    .await
                    .unwrap()
                {
                    // Plan + all of its actions commit together.
                    assert_eq!(repos.plans.actions(id).await.unwrap().len(), 5);
                    seen += 1;
                }
                let inv = repos
                    .scans
                    .load_inventory(&RootId::completed())
                    .await
                    .unwrap();
                assert!(inv.entries.len() % 100 == 0, "inventory batches are atomic");
                tokio::task::yield_now().await;
            }
            seen
        }));
    }

    for n in 1..=15u128 {
        let p = plan(&f, n, s.id, 5);
        let sealed = HashedPlan::seal(p.clone()).unwrap();
        f.repos.plans.insert(&p, sealed.hash()).await.unwrap();
        let entries = (0..n * 100)
            .map(|i| entry(&format!("f/{i}"), u64::try_from(i).unwrap() + 1, 1))
            .collect();
        f.repos
            .scans
            .save_inventory(s.id, &inventory(entries, false))
            .await
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.await.unwrap();
    }
    assert_eq!(
        f.repos
            .plans
            .ids_with_status(PlanStatus::Draft)
            .await
            .unwrap()
            .len(),
        15
    );
}

#[tokio::test]
async fn concurrent_writers_are_serialized_without_busy_errors() {
    let f = fx().await;
    let mut handles = vec![];
    for n in 1..=24u128 {
        let repos = f.repos.clone();
        handles.push(tokio::spawn(async move {
            let mut s = session(n, i64::try_from(n).unwrap());
            s.roots.clear();
            repos.scans.begin_session(&s).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert!(f.repos.scans.latest_session().await.unwrap().is_some());
}
