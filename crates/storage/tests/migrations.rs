#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
mod common;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::ports::ScanStore;
use removarr_storage::Db;
use sqlx::Row;

#[tokio::test]
async fn fresh_database_has_pragmas_tables_and_clean_integrity() {
    let f = fx().await;
    assert_eq!(f.db.pragma("journal_mode").await.unwrap(), "wal");
    assert_eq!(f.db.pragma("synchronous").await.unwrap(), "2", "FULL");
    assert_eq!(f.db.pragma("foreign_keys").await.unwrap(), "1");
    assert_eq!(f.db.pragma("busy_timeout").await.unwrap(), "5000");
    assert_eq!(
        f.db.pragma("wal_autocheckpoint").await.unwrap(),
        "1000",
        "default kept"
    );

    let mut raw = f.raw().await;
    let tables: Vec<String> = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table'")
        .fetch_all(&mut raw)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>(0))
        .collect();
    for t in [
        "meta",
        "app_state",
        "scan_sessions",
        "session_roots",
        "session_services",
        "fs_entries",
        "scan_errors",
        "protected_zones",
        "torrents",
        "torrent_snapshot",
        "match_candidates",
        "arr_snapshots",
        "issues",
        "ignores",
        "plans",
        "plan_actions",
        "approvals",
        "simulation_runs",
        "journal_intents",
        "journal_results",
        "gains",
        "observed_deltas",
        "jobs",
    ] {
        assert!(tables.iter().any(|x| x == t), "missing table {t}");
    }
    let fk = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut raw)
        .await
        .unwrap();
    assert!(fk.is_empty());
    let integrity: String = sqlx::query("PRAGMA integrity_check")
        .fetch_one(&mut raw)
        .await
        .unwrap()
        .get(0);
    assert_eq!(integrity, "ok");
}

#[tokio::test]
async fn reopening_is_idempotent_and_keeps_data() {
    let f = fx().await;
    let s = begin(&f, 1).await;
    let path = f.path();
    f.db.clone().close().await;

    let db = Db::open(&path).await.unwrap();
    let repos = removarr_storage::Repos::new(&db, f.clock.clone());
    assert_eq!(
        repos.scans.latest_session().await.unwrap().unwrap().id,
        s.id
    );
    db.close().await;
}

#[tokio::test]
async fn a_database_from_a_newer_version_is_refused() {
    let f = fx().await;
    let path = f.path();
    let mut raw = f.raw().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (99999, 'from the future', 1, x'00', 0)",
    )
    .execute(&mut raw)
    .await
    .unwrap();
    drop(raw);
    let err = Db::open(&path).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Storage);
}
