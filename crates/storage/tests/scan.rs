#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{IoKind, ProtectedReason, ProtectedZone, ScanErrorRecord};
use removarr_domain::path::RootId;
use removarr_domain::ports::ScanStore;
use removarr_domain::session::SessionStatus;
use removarr_domain::time::Timestamp;
use sqlx::Row;

async fn changed_session(f: &Fx, rel: &str) -> String {
    let mut raw = f.raw().await;
    sqlx::query("SELECT changed_session FROM fs_entries WHERE rel = ?")
        .bind(rel)
        .fetch_one(&mut raw)
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn inventory_roundtrip() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let inv = inventory(
        vec![entry("a/x.mkv", 10, 100), entry("a/y.mkv", 11, 200)],
        true,
    );
    let stats = f.repos.scans.save_inventory(s.id, &inv).await.unwrap();
    assert_eq!(
        (stats.added, stats.changed, stats.removed, stats.unchanged),
        (2, 0, 0, 0)
    );
    let back = f
        .repos
        .scans
        .load_inventory(&RootId::completed())
        .await
        .unwrap();
    assert_eq!(back.entries, inv.entries);
    assert!(back.finished);
}

#[tokio::test]
async fn unchanged_entries_are_not_rewritten() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    let s2 = begin(&f, 2).await;
    let inv = inventory(vec![entry("a.mkv", 10, 100), entry("b.mkv", 11, 200)], true);
    f.repos.scans.save_inventory(s1.id, &inv).await.unwrap();

    let stats = f.repos.scans.save_inventory(s2.id, &inv).await.unwrap();
    assert_eq!(
        (stats.added, stats.changed, stats.removed, stats.unchanged),
        (0, 0, 0, 2)
    );
    // The row still says it last changed in the first session: it was not touched.
    assert_eq!(changed_session(&f, "a.mkv").await, s1.id.0.to_string());

    let inv2 = inventory(vec![entry("a.mkv", 10, 100), entry("b.mkv", 11, 999)], true);
    let s3 = begin(&f, 3).await;
    let stats = f.repos.scans.save_inventory(s3.id, &inv2).await.unwrap();
    assert_eq!((stats.changed, stats.unchanged), (1, 1));
    assert_eq!(changed_session(&f, "a.mkv").await, s1.id.0.to_string());
    assert_eq!(changed_session(&f, "b.mkv").await, s3.id.0.to_string());
}

#[tokio::test]
async fn a_large_inventory_is_one_commit() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let entries = (0..3000)
        .map(|i| entry(&format!("d/{i}.bin"), 1000 + i, 5))
        .collect();
    let mut raw = f.raw().await;
    let before: i64 = sqlx::query("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap()
        .get(0);
    f.repos
        .scans
        .save_inventory(s.id, &inventory(entries, true))
        .await
        .unwrap();
    let after: i64 = sqlx::query("PRAGMA data_version")
        .fetch_one(&mut raw)
        .await
        .unwrap()
        .get(0);
    // data_version changes once per commit by another connection.
    assert_eq!(after - before, 1);
}

#[tokio::test]
async fn removal_needs_a_finished_scan_and_a_visible_directory() {
    let f = fx().await;
    let s1 = begin(&f, 1).await;
    f.repos
        .scans
        .save_inventory(
            s1.id,
            &inventory(
                vec![
                    entry("seen.mkv", 1, 1),
                    entry("gone.mkv", 2, 1),
                    entry("hidden/x.mkv", 3, 1),
                ],
                true,
            ),
        )
        .await
        .unwrap();

    // Unfinished scan: nothing is marked gone.
    let s2 = begin(&f, 2).await;
    let stats = f
        .repos
        .scans
        .save_inventory(s2.id, &inventory(vec![entry("seen.mkv", 1, 1)], false))
        .await
        .unwrap();
    assert_eq!(stats.removed, 0);

    // Finished scan with an unreadable directory: entries under it stay.
    let s3 = begin(&f, 3).await;
    let mut inv = inventory(vec![entry("seen.mkv", 1, 1)], true);
    inv.errors.push(ScanErrorRecord {
        path: rp("hidden"),
        io: IoKind::PermissionDenied,
        message: "denied".into(),
    });
    let stats = f.repos.scans.save_inventory(s3.id, &inv).await.unwrap();
    assert_eq!(stats.removed, 1, "only gone.mkv");
    let names: Vec<String> = f
        .repos
        .scans
        .load_inventory(&RootId::completed())
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| e.path.rel.as_str().to_string())
        .collect();
    assert_eq!(names, vec!["hidden/x.mkv", "seen.mkv"]);

    // A reappearing path is revived, not duplicated.
    let s4 = begin(&f, 4).await;
    let stats = f
        .repos
        .scans
        .save_inventory(
            s4.id,
            &inventory(
                vec![
                    entry("seen.mkv", 1, 1),
                    entry("gone.mkv", 2, 1),
                    entry("hidden/x.mkv", 3, 1),
                ],
                true,
            ),
        )
        .await
        .unwrap();
    assert_eq!((stats.added, stats.removed), (1, 0));
    assert_eq!(
        f.repos
            .scans
            .load_inventory(&RootId::completed())
            .await
            .unwrap()
            .entries
            .len(),
        3
    );
}

#[tokio::test]
async fn errors_and_protected_zones_are_returned_with_the_inventory() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let mut inv = inventory(vec![entry("a", 1, 1)], true);
    inv.errors.push(ScanErrorRecord {
        path: rp("bad"),
        io: IoKind::PermissionDenied,
        message: "open failed for https://tracker.example/announce?passkey=abc".into(),
    });
    inv.protected.push(ProtectedZone {
        path: rp(".qbit"),
        reason: ProtectedReason::QbitInternal,
    });
    f.repos.scans.save_inventory(s.id, &inv).await.unwrap();
    let back = f
        .repos
        .scans
        .load_inventory(&RootId::completed())
        .await
        .unwrap();
    assert_eq!(back.protected, inv.protected);
    assert_eq!(back.errors.len(), 1);
    assert!(!back.errors[0].message.contains("tracker") && !back.errors[0].message.contains("abc"));

    // Saving the same root again for the same session replaces, not appends.
    f.repos.scans.save_inventory(s.id, &inv).await.unwrap();
    assert_eq!(
        f.repos
            .scans
            .load_inventory(&RootId::completed())
            .await
            .unwrap()
            .errors
            .len(),
        1
    );
}

#[tokio::test]
async fn session_lifecycle_is_a_compare_and_swap() {
    let f = fx().await;
    let mut s = begin(&f, 1).await;
    assert_eq!(
        f.repos
            .scans
            .latest_session()
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Running
    );

    s.finish(SessionStatus::Complete, Timestamp(9_000_000_000_000))
        .unwrap();
    s.metrics.entries_scanned = 42;
    f.repos.scans.finish_session(&s).await.unwrap();
    let back = f.repos.scans.latest_session().await.unwrap().unwrap();
    assert_eq!(
        (back.status, back.metrics.entries_scanned),
        (SessionStatus::Complete, 42)
    );

    assert_eq!(
        f.repos.scans.finish_session(&s).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    let err = f
        .repos
        .scans
        .save_inventory(s.id, &inventory(vec![], true))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Conflict);
    let err = f
        .repos
        .scans
        .save_inventory(sid(99), &inventory(vec![], true))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::NotFound);
    assert_eq!(
        f.repos.scans.begin_session(&s).await.unwrap_err().class,
        ErrorClass::Validation
    );
}

#[tokio::test]
async fn duplicate_session_is_a_conflict_and_foreign_roots_are_rejected() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    assert_eq!(
        f.repos.scans.begin_session(&s).await.unwrap_err().class,
        ErrorClass::Conflict
    );
    let mut inv = inventory(vec![entry("a", 1, 1)], true);
    inv.root = RootId::incomplete();
    assert_eq!(
        f.repos
            .scans
            .save_inventory(s.id, &inv)
            .await
            .unwrap_err()
            .class,
        ErrorClass::Validation
    );
}
