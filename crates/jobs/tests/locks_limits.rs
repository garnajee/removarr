#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Lock contention, deadlock freedom and concurrency limits.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use removarr_domain::session::ServiceKind;
use removarr_jobs::config::LimitsConfig;
use removarr_jobs::limits::{Limits, Resource};
use removarr_jobs::locks::{LockCancelled, Locks, Scope};
use tokio_util::sync::CancellationToken;

fn shared(keys: &[&str]) -> Scope {
    Scope {
        shared: keys.iter().map(|k| (*k).to_string()).collect(),
        ..Scope::default()
    }
}

fn exclusive(keys: &[&str]) -> Scope {
    Scope {
        exclusive: keys.iter().map(|k| (*k).to_string()).collect(),
        ..Scope::default()
    }
}

#[tokio::test]
async fn shared_locks_coexist_and_an_exclusive_lock_excludes_everything_on_its_key() {
    let locks = Locks::new();
    let a = locks.try_acquire(&shared(&["root:completed"])).unwrap();
    let b = locks
        .try_acquire(&shared(&["root:completed", "root:incomplete"]))
        .unwrap();
    assert!(
        locks.try_acquire(&exclusive(&["root:completed"])).is_none(),
        "readers block the writer"
    );
    drop((a, b));
    let w = locks.try_acquire(&exclusive(&["root:completed"])).unwrap();
    assert!(locks.try_acquire(&shared(&["root:completed"])).is_none());
    assert!(locks.try_acquire(&exclusive(&["root:completed"])).is_none());
    assert!(
        locks
            .try_acquire(&exclusive(&["root:incomplete"]))
            .is_some(),
        "disjoint scopes do not contend"
    );
    drop(w);
    assert!(locks.try_acquire(&exclusive(&["root:completed"])).is_some());
}

#[tokio::test]
async fn one_importer_per_media_two_plans_on_the_same_media_serialize() {
    let locks = Arc::new(Locks::new());
    let key = "media:sonarr:7";
    let first = locks
        .acquire(
            &exclusive(&[key, "root:completed"]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let l2 = Arc::clone(&locks);
    let second = tokio::spawn(async move {
        l2.acquire(
            &exclusive(&[key, "root:media-00000001"]),
            &CancellationToken::new(),
        )
        .await
        .map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "the second importer must wait for the media lock"
    );
    drop(first);
    assert!(second.await.unwrap().is_ok());
}

#[tokio::test]
async fn overlapping_scopes_in_any_order_never_deadlock() {
    let locks = Arc::new(Locks::new());
    let done = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for i in 0..16 {
        let (locks, done) = (Arc::clone(&locks), Arc::clone(&done));
        tasks.push(tokio::spawn(async move {
            // BTreeSets sort the keys, so a scope listed "backwards" is still taken in order.
            let scope = if i % 2 == 0 {
                exclusive(&["a", "b", "c"])
            } else {
                exclusive(&["c", "b", "a"])
            };
            let g = locks
                .acquire(&scope, &CancellationToken::new())
                .await
                .unwrap();
            tokio::task::yield_now().await;
            drop(g);
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for t in tasks {
            t.await.unwrap();
        }
    })
    .await
    .expect("deadlock");
    assert_eq!(done.load(Ordering::SeqCst), 16);
}

#[tokio::test]
async fn cancellation_while_waiting_releases_what_was_taken() {
    let locks = Locks::new();
    let held = locks
        .acquire(&exclusive(&["b"]), &CancellationToken::new())
        .await
        .unwrap();
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        t.cancel();
    });
    // Takes "a", then waits for "b" and is cancelled.
    let r = locks.acquire(&exclusive(&["a", "b"]), &token).await;
    assert_eq!(r.unwrap_err(), LockCancelled);
    assert!(
        locks.try_acquire(&exclusive(&["a"])).is_some(),
        "a was released by the cancelled waiter"
    );
    drop(held);
}

#[test]
fn conflicts_follow_shared_and_exclusive_rules() {
    assert!(!shared(&["x"]).conflicts_with(&shared(&["x"])));
    assert!(shared(&["x"]).conflicts_with(&exclusive(&["x"])));
    assert!(exclusive(&["x"]).conflicts_with(&exclusive(&["x"])));
    assert!(!exclusive(&["x"]).conflicts_with(&exclusive(&["y"])));
    assert!(
        exclusive(&["action:consolidatehardlink"]).conflicts_with(&exclusive(&[
            "action:consolidatehardlink",
            "root:media-00000001"
        ]))
    );
}

#[tokio::test]
async fn limits_are_clamped_and_enforced() {
    let l = Limits::new(&LimitsConfig {
        content_read_concurrency: 9,
        ffprobe_concurrency: 0,
        http_concurrency_per_service: 99,
    });
    assert_eq!(l.capacity(Resource::Compare), 4);
    assert_eq!(l.capacity(Resource::Probe), 1);
    assert_eq!(l.capacity(Resource::Api(ServiceKind::Sonarr)), 16);
    assert_eq!(
        (l.capacity(Resource::Scan), l.capacity(Resource::Mutation)),
        (1, 1)
    );

    let l = Arc::new(Limits::new(&LimitsConfig {
        content_read_concurrency: 2,
        ..LimitsConfig::default()
    }));
    let (now, max) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (l, now, max) = (Arc::clone(&l), Arc::clone(&now), Arc::clone(&max));
        tasks.push(tokio::spawn(async move {
            let _p = l.acquire(Resource::Compare).await.unwrap();
            let n = now.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            now.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        max.load(Ordering::SeqCst),
        2,
        "content reads never exceed content_read_concurrency"
    );
    // Scans and mutations: one at a time.
    let p = l.acquire(Resource::Scan).await.unwrap();
    assert_eq!(l.available(Resource::Scan), 0);
    drop(p);
    assert_eq!(l.available(Resource::Scan), 1);
}
