//! Connection pools, pragmas and migrations.
//!
//! One writer connection (WAL, `synchronous = FULL`, foreign keys, busy timeout) and a small
//! read-only reader pool. `wal_autocheckpoint` keeps SQLite's default; there are no checkpoint
//! loops. One passive checkpoint and `PRAGMA optimize` run at [`Db::close`].

use std::path::Path;
use std::time::Duration;

use removarr_domain::error::{ErrorClass, PortError};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{ConnectOptions, Row, Sqlite, Transaction};

use crate::error::map_sqlx;

/// Forward-only migrations embedded at build time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Caps the size the WAL file keeps on disk after a checkpoint (64 MiB).
const JOURNAL_SIZE_LIMIT: &str = "67108864";
const READER_CONNECTIONS: u32 = 4;

#[derive(Clone, Debug)]
pub struct Db {
    writer: SqlitePool,
    reader: SqlitePool,
}

impl Db {
    /// Opens (creating if needed) the database at `path` and applies pending migrations.
    pub async fn open(path: &Path) -> Result<Self, PortError> {
        Self::open_with(path, &MIGRATOR).await
    }

    pub(crate) async fn open_with(path: &Path, migrator: &Migrator) -> Result<Self, PortError> {
        let writer_opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
            .pragma("temp_store", "MEMORY")
            .pragma("journal_size_limit", JOURNAL_SIZE_LIMIT)
            .disable_statement_logging();
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(writer_opts)
            .await
            .map_err(map_sqlx)?;
        migrator
            .run(&writer)
            .await
            .map_err(|e| PortError::new(ErrorClass::Storage, format!("migration failed: {e}")))?;

        let reader_opts = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
            .disable_statement_logging();
        let reader = SqlitePoolOptions::new()
            .max_connections(READER_CONNECTIONS)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(reader_opts)
            .await
            .map_err(map_sqlx)?;
        Ok(Self { writer, reader })
    }

    pub(crate) fn reader(&self) -> &SqlitePool {
        &self.reader
    }

    /// Starts a write transaction with `BEGIN IMMEDIATE`, so a lock conflict surfaces at the
    /// start (and waits for the busy timeout) rather than at the first write.
    pub(crate) async fn write_tx(&self) -> Result<Transaction<'static, Sqlite>, PortError> {
        self.writer
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx)
    }

    pub(crate) async fn read_tx(&self) -> Result<Transaction<'static, Sqlite>, PortError> {
        self.reader.begin().await.map_err(map_sqlx)
    }

    /// `PRAGMA optimize`. Call at the end of each scan and at shutdown.
    pub async fn optimize(&self) -> Result<(), PortError> {
        sqlx::query("PRAGMA optimize")
            .execute(&self.writer)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    /// Reads a pragma from the writer connection (diagnostics and tests).
    pub async fn pragma(&self, name: &str) -> Result<String, PortError> {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(PortError::new(
                ErrorClass::Validation,
                "invalid pragma name",
            ));
        }
        let row = sqlx::query(&format!("PRAGMA {name}"))
            .fetch_one(&self.writer)
            .await
            .map_err(map_sqlx)?;
        if let Ok(v) = row.try_get::<i64, _>(0) {
            return Ok(v.to_string());
        }
        row.try_get::<String, _>(0).map_err(map_sqlx)
    }

    /// Orderly shutdown: one passive checkpoint (never blocks readers), `optimize`, close.
    pub async fn close(self) {
        let _ = sqlx::query("PRAGMA optimize").execute(&self.writer).await;
        let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&self.writer)
            .await;
        self.reader.close().await;
        self.writer.close().await;
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use removarr_domain::ports::ScanStore;
    use removarr_testkit::clock::FakeClock;
    use sqlx::Row;

    use super::*;
    use crate::Repos;

    fn only_up_to(version: i64) -> Migrator {
        Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .migrations
                    .iter()
                    .filter(|m| m.version <= version)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        }
    }

    #[tokio::test]
    async fn upgrade_from_the_first_migration_keeps_existing_rows() {
        use removarr_domain::ids::{RawUuid, SessionId};
        use removarr_domain::session::{AuditSession, SessionMetrics, SessionStatus};
        use removarr_domain::time::Timestamp;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removarr.db");
        let clock = Arc::new(FakeClock::default());

        // A database as released with migration 1 only: no plan or journal tables.
        let old = Db::open_with(&path, &only_up_to(1)).await.unwrap();
        let session = AuditSession {
            id: SessionId::new(RawUuid::from_u128(7)),
            started_at: Timestamp(5),
            finished_at: None,
            status: SessionStatus::Running,
            roots: vec![],
            services: vec![],
            metrics: SessionMetrics::default(),
        };
        Repos::new(&old, clock.clone())
            .scans
            .begin_session(&session)
            .await
            .unwrap();
        let has_plans = sqlx::query("SELECT 1 FROM sqlite_master WHERE name = 'plans'")
            .fetch_optional(&old.writer)
            .await
            .unwrap();
        assert!(has_plans.is_none());
        old.close().await;

        // Upgrade: pending migrations apply on top, existing data survives.
        let new = Db::open(&path).await.unwrap();
        let repos = Repos::new(&new, clock);
        assert_eq!(
            repos.scans.latest_session().await.unwrap().unwrap().id,
            session.id
        );
        let applied: Vec<i64> =
            sqlx::query("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&new.writer)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
        let expected: Vec<i64> = MIGRATOR.migrations.iter().map(|m| m.version).collect();
        assert_eq!(applied, expected);
        let fk = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&new.writer)
            .await
            .unwrap();
        assert!(fk.is_empty());
        new.close().await;
    }

    #[tokio::test]
    async fn a_modified_released_migration_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removarr.db");
        Db::open(&path).await.unwrap().close().await;
        let mut tampered: Vec<_> = MIGRATOR.migrations.iter().cloned().collect();
        tampered[0].checksum = Cow::Owned(vec![1, 2, 3]);
        let m = Migrator {
            migrations: Cow::Owned(tampered),
            ..Migrator::DEFAULT
        };
        assert!(Db::open_with(&path, &m).await.is_err());
    }
}
