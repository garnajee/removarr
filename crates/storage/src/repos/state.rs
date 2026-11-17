//! `StateStore`: small key/value settings (`app_state`). Unchanged values cause no write.

use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ports::{Clock, StateStore};

use crate::codec::{col, scrub};
use crate::db::Db;
use crate::error::{invalid, map_sqlx};

#[derive(Clone)]
pub struct StateRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for StateRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateRepo").finish_non_exhaustive()
    }
}

impl StateRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

#[async_trait]
impl StateStore for StateRepo {
    async fn get(&self, key: &str) -> Result<Option<String>, PortError> {
        let row = sqlx::query("SELECT value FROM app_state WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db.reader())
            .await
            .map_err(map_sqlx)?;
        row.map(|r| col(&r, "value")).transpose()
    }

    /// The value is scrubbed of URLs and credentials: settings are not a place for secrets.
    async fn set(&self, key: &str, value: &str) -> Result<(), PortError> {
        if key.is_empty() || key.len() > 128 {
            return Err(invalid("invalid state key"));
        }
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO app_state (key, value, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at \
             WHERE app_state.value <> excluded.value",
        )
        .bind(key)
        .bind(scrub(value))
        .bind(self.clock.now().0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }
}
