//! `GainStore`: the gain ledger. `gain_id` is the primary key, so one physical file version can
//! be counted once, whatever plan or action reports it. A duplicate rolls the whole batch back.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::gain::{GainKind, GainRecord, GainTotals, ObservedDelta};
use removarr_domain::ports::{Clock, GainStore};

use crate::codec::{col, enum_text, from_i64, to_i64, uuid_text};
use crate::db::Db;
use crate::error::{conflict, map_sqlx};

#[derive(Clone)]
pub struct GainRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for GainRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GainRepo").finish_non_exhaustive()
    }
}

impl GainRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    /// Ledger totals. Each `gain_id` exists once, so `duplicates` is always 0.
    pub async fn totals(&self) -> Result<GainTotals, PortError> {
        let rows = sqlx::query("SELECT kind, SUM(bytes) AS total FROM gains GROUP BY kind")
            .fetch_all(self.db.reader())
            .await
            .map_err(map_sqlx)?;
        let (mut attributable, mut added) = (0u64, 0u64);
        for r in &rows {
            let kind: GainKind = crate::codec::from_enum_text(&col::<String>(r, "kind")?)?;
            let total = from_i64(col(r, "total")?);
            match kind {
                GainKind::AttributableDeletion | GainKind::AttributableConsolidation => {
                    attributable = attributable.saturating_add(total);
                }
                GainKind::AddedCost => added = added.saturating_add(total),
            }
        }
        Ok(GainTotals {
            attributable,
            added,
            net: i64::try_from(attributable)
                .unwrap_or(i64::MAX)
                .saturating_sub(i64::try_from(added).unwrap_or(i64::MAX)),
            duplicates: 0,
        })
    }
}

#[async_trait]
impl GainStore for GainRepo {
    async fn record(
        &self,
        gains: &[GainRecord],
        delta: Option<&ObservedDelta>,
    ) -> Result<(), PortError> {
        let mut ids = HashSet::new();
        if !gains.iter().all(|g| ids.insert(g.gain_id.0.as_str())) {
            return Err(conflict("duplicate gain id in one batch"));
        }
        let mut tx = self.db.write_tx().await?;
        for g in gains {
            sqlx::query(
                "INSERT INTO gains (gain_id, plan_id, action_idx, kind, dev, ino, bytes, recorded_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&g.gain_id.0)
            .bind(uuid_text(g.action.plan.0))
            .bind(i64::from(g.action.index))
            .bind(enum_text(&g.kind)?)
            .bind(to_i64(g.phys.dev))
            .bind(to_i64(g.phys.ino))
            .bind(to_i64(g.bytes))
            .bind(g.recorded_at.0)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        if let Some(d) = delta {
            sqlx::query(
                "INSERT INTO observed_deltas (plan_id, root, free_before, free_after_local, \
                   free_after_services, recorded_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(gains.first().map(|g| uuid_text(g.action.plan.0)))
            .bind(d.root.as_str())
            .bind(to_i64(d.free_before))
            .bind(to_i64(d.free_after_local))
            .bind(d.free_after_services.map(to_i64))
            .bind(self.clock.now().0)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        tx.commit().await.map_err(map_sqlx)
    }
}
