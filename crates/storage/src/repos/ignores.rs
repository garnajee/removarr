//! `IgnoreStore`: the ignore list. Restoring keeps the row (`restored_at`) for history.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ids::SubjectKey;
use removarr_domain::ports::{Clock, IgnoreStore};
use removarr_domain::time::Timestamp;

use crate::codec::{col, scrub};
use crate::db::Db;
use crate::error::map_sqlx;

#[derive(Clone)]
pub struct IgnoreRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for IgnoreRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreRepo").finish_non_exhaustive()
    }
}

impl IgnoreRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

#[async_trait]
impl IgnoreStore for IgnoreRepo {
    async fn ignore(
        &self,
        subjects: &[SubjectKey],
        reason: Option<&str>,
        at: Timestamp,
    ) -> Result<(), PortError> {
        let reason = reason.map(scrub);
        let mut tx = self.db.write_tx().await?;
        for s in subjects {
            sqlx::query(
                "INSERT INTO ignores (subject_key, reason, created_at, restored_at) \
                 VALUES (?, ?, ?, NULL) \
                 ON CONFLICT (subject_key) DO UPDATE SET reason = excluded.reason, \
                   created_at = CASE WHEN ignores.restored_at IS NULL \
                     THEN ignores.created_at ELSE excluded.created_at END, \
                   restored_at = NULL \
                 WHERE ignores.restored_at IS NOT NULL OR ignores.reason IS NOT excluded.reason",
            )
            .bind(s.as_str())
            .bind(reason.as_deref())
            .bind(at.0)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn restore(&self, subjects: &[SubjectKey]) -> Result<(), PortError> {
        let now = self.clock.now().0;
        let mut tx = self.db.write_tx().await?;
        for s in subjects {
            sqlx::query(
                "UPDATE ignores SET restored_at = ? WHERE subject_key = ? AND restored_at IS NULL",
            )
            .bind(now)
            .bind(s.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        let rows = sqlx::query("SELECT subject_key FROM ignores WHERE restored_at IS NULL")
            .fetch_all(self.db.reader())
            .await
            .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| Ok(SubjectKey(col(r, "subject_key")?)))
            .collect()
    }
}
