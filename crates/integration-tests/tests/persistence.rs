//! Task 1: migrations and repositories against a temporary SQLite database, through the
//! production composition root: a fresh database migrates, a restart is idempotent and keeps
//! scan, issues and plans byte-for-byte, and a database from a newer build is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::issue::IssueCategory as C;
use removarr_integration_tests::*;
use removarr_workflows::plans::{IssueAction, PlanRequest};
use sqlx::{Connection, Row, SqliteConnection};

async fn conn(s: &Stack) -> SqliteConnection {
    let path = s.fx.appdata().join(removarr::wiring::DB_FILE);
    SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap()
}

async fn applied(c: &mut SqliteConnection) -> Vec<(i64, bool)> {
    sqlx::query("SELECT version, success FROM _sqlx_migrations ORDER BY version")
        .fetch_all(c)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get::<i64, _>(0), r.get::<bool, _>(1)))
        .collect()
}

#[tokio::test]
async fn a_fresh_database_is_migrated_and_a_restart_changes_nothing() {
    let s = Stack::parity(Options::default().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other"))
        .await;
    let mut c = conn(&s).await;
    let first = applied(&mut c).await;
    assert!(
        !first.is_empty() && first.iter().all(|(_, ok)| *ok),
        "{first:?}"
    );
    let tables: Vec<String> = sqlx::query("SELECT name FROM sqlite_master WHERE type='table'")
        .fetch_all(&mut c)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for t in ["plans", "issues", "jobs", "journal_intents"] {
        assert!(
            tables.iter().any(|n| n == t || n.starts_with(t)),
            "table {t} missing in {tables:?}"
        );
    }
    drop(c);

    // Persisted state: scan, issues, a plan with its hash.
    s.scan().await;
    let before_issues = s.issues(C::Junk).await;
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let p = s
        .create_plan(&PlanRequest::issues(C::Junk, vec![id], IssueAction::Delete))
        .await;
    s.approve(&p).await;
    let stored = s.stored_plan(p.id).await;

    let s = s.restart().await;
    let mut c = conn(&s).await;
    assert_eq!(applied(&mut c).await, first, "no migration re-run or added");
    drop(c);
    assert_eq!(s.issues(C::Junk).await, before_issues);
    assert_eq!(s.stored_plan(p.id).await, stored);
    assert_eq!(
        s.plan_status(p.id).await,
        removarr_domain::plan::PlanStatus::Approved
    );
    // The approval still names exactly the same hash after a restart.
    let (_, hash, _, approval) = removarr_domain::ports::PlanStore::get(&s.repos.plans, p.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hash, p.hash);
    assert_eq!(approval.unwrap().plan_hash, p.hash);
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused_at_startup() {
    let s = Stack::parity(Options::default()).await;
    let mut c = conn(&s).await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (99999999999999, 'from the future', 1, x'00', 0)",
    )
    .execute(&mut c)
    .await
    .unwrap();
    drop(c);
    let surviving = s.into_surviving();
    let cfg = removarr_config::Config::from_sources(None, &surviving.env).unwrap();
    let r = removarr::wiring::build(
        &cfg,
        removarr_reporting::LogBuffer::new(10, removarr_reporting::LogLevel::Info),
        removarr::wiring::default_guard(),
    )
    .await;
    assert!(
        matches!(r, Err(removarr::wiring::StartupError::Storage(_))),
        "an unknown applied migration must stop startup, not be ignored"
    );
}
