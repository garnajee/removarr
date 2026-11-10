//! `PlanStore`: plan + actions insert, approval and status compare-and-swap.
//!
//! Boundaries (each is one `BEGIN IMMEDIATE` transaction):
//! - `insert`: plan row and every action row, or nothing.
//! - `approve`: status/hash/expiry check, approval row and `draft -> approved`, or nothing.
//! - `transition`: `UPDATE .. WHERE status = from`, after the domain table allows the edge.

use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ids::PlanId;
use removarr_domain::journal::{ActionOutcome, ActionState};
use removarr_domain::plan::{ActionPlan, Approval, HashedPlan, PlanHash, PlanStatus};
use removarr_domain::ports::{Clock, PlanStore};
use removarr_domain::time::Timestamp;
use sqlx::sqlite::SqliteRow;
use sqlx::{Sqlite, Transaction};

use crate::codec::{col, dec, enc, enum_text, from_enum_text, parse_uuid, to_u32, uuid_text};
use crate::db::Db;
use crate::error::{conflict, domain, integrity, invalid, map_sqlx, not_found};

#[derive(Clone)]
pub struct PlanRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PlanRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanRepo").finish_non_exhaustive()
    }
}

/// Stored state of one planned action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub index: u32,
    pub state: ActionState,
    pub outcome: Option<ActionOutcome>,
}

impl PlanRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    /// Plans currently in `status` (oldest first). Recovery uses `Executing`.
    pub async fn ids_with_status(&self, status: PlanStatus) -> Result<Vec<PlanId>, PortError> {
        let rows = sqlx::query("SELECT id FROM plans WHERE status = ? ORDER BY created_at, rowid")
            .bind(enum_text(&status)?)
            .fetch_all(self.db.reader())
            .await
            .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| Ok(PlanId::new(parse_uuid(&col::<String>(r, "id")?)?)))
            .collect()
    }

    /// Notes that a dry run of the plan happened. The simulated journal rows are the detail;
    /// both are per-session data that retention may prune.
    pub async fn record_simulation_run(
        &self,
        id: PlanId,
        started_at: Timestamp,
        finished_at: Timestamp,
    ) -> Result<(), PortError> {
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO simulation_runs (plan_id, started_at, finished_at) VALUES (?, ?, ?)",
        )
        .bind(uuid_text(id.0))
        .bind(started_at.0)
        .bind(finished_at.0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    /// Per-action state and outcome of a plan, by index.
    pub async fn actions(&self, id: PlanId) -> Result<Vec<ActionRow>, PortError> {
        let rows = sqlx::query(
            "SELECT idx, state, outcome FROM plan_actions WHERE plan_id = ? ORDER BY idx",
        )
        .bind(uuid_text(id.0))
        .fetch_all(self.db.reader())
        .await
        .map_err(map_sqlx)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(ActionRow {
                index: to_u32(col(r, "idx")?)?,
                state: from_enum_text(&col::<String>(r, "state")?)?,
                outcome: col::<Option<String>>(r, "outcome")?
                    .map(|s| dec(&s))
                    .transpose()?,
            });
        }
        Ok(out)
    }
}

fn approval_from_row(r: &SqliteRow) -> Result<Approval, PortError> {
    Ok(Approval {
        plan_id: PlanId::new(parse_uuid(&col::<String>(r, "plan_id")?)?),
        plan_hash: PlanHash(col(r, "plan_hash")?),
        approved_at: Timestamp(col(r, "approved_at")?),
        by: from_enum_text(&col::<String>(r, "approved_by")?)?,
        confirmed_count: to_u32(col(r, "confirmed_count")?)?,
        confirmed_torrent_removals: to_u32(col(r, "confirmed_torrent_removals")?)?,
        acknowledged_samples: dec(&col::<String>(r, "acknowledged_samples")?)?,
        acknowledged_unrecoverable: dec(&col::<String>(r, "acknowledged_unrecoverable")?)?,
    })
}

async fn status_of(
    tx: &mut Transaction<'static, Sqlite>,
    id: &str,
) -> Result<Option<(PlanStatus, String, Timestamp)>, PortError> {
    let row = sqlx::query("SELECT status, hash, expires_at FROM plans WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    row.map(|r| {
        Ok((
            from_enum_text(&col::<String>(&r, "status")?)?,
            col::<String>(&r, "hash")?,
            Timestamp(col(&r, "expires_at")?),
        ))
    })
    .transpose()
}

#[async_trait]
impl PlanStore for PlanRepo {
    async fn insert(&self, plan: &ActionPlan, hash: &PlanHash) -> Result<(), PortError> {
        // Validates the plan and proves that `hash` is the hash of this exact content.
        HashedPlan::from_parts(plan.clone(), hash.clone()).map_err(domain)?;
        let id = uuid_text(plan.id.0);
        let now = self.clock.now().0;
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO plans (id, hash, status, mode, origin, session_id, created_at, \
               expires_at, updated_at, body) VALUES (?, ?, 'draft', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(hash.as_str())
        .bind(enum_text(&plan.mode)?)
        .bind(enum_text(&plan.origin)?)
        .bind(uuid_text(plan.session.0))
        .bind(plan.created_at.0)
        .bind(plan.expires_at.0)
        .bind(now)
        .bind(enc(plan)?)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        for a in &plan.actions {
            sqlx::query(
                "INSERT INTO plan_actions (plan_id, idx, kind, state, outcome, updated_at) \
                 VALUES (?, ?, ?, 'pending', NULL, ?)",
            )
            .bind(&id)
            .bind(i64::from(a.index))
            .bind(enum_text(&a.kind)?)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn get(
        &self,
        id: PlanId,
    ) -> Result<Option<(ActionPlan, PlanHash, PlanStatus, Option<Approval>)>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let idt = uuid_text(id.0);
        let Some(row) = sqlx::query("SELECT status, hash, body FROM plans WHERE id = ?")
            .bind(&idt)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
        else {
            return Ok(None);
        };
        let plan: ActionPlan = dec(&col::<String>(&row, "body")?)?;
        let hash = PlanHash(col(&row, "hash")?);
        // A stored plan whose content no longer matches its hash is corrupt or tampered.
        let sealed = HashedPlan::from_parts(plan, hash)
            .map_err(|e| integrity(format!("stored plan failed verification: {e}")))?;
        let (plan, hash) = sealed.into_parts();
        let status = from_enum_text(&col::<String>(&row, "status")?)?;
        let approval = sqlx::query("SELECT * FROM approvals WHERE plan_id = ?")
            .bind(&idt)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .map(|r| approval_from_row(&r))
            .transpose()?;
        Ok(Some((plan, hash, status, approval)))
    }

    async fn approve(&self, a: &Approval) -> Result<(), PortError> {
        let id = uuid_text(a.plan_id.0);
        let now = self.clock.now();
        let mut tx = self.db.write_tx().await?;
        let Some((status, hash, expires_at)) = status_of(&mut tx, &id).await? else {
            return Err(not_found("plan not found"));
        };
        if status != PlanStatus::Draft {
            return Err(conflict("plan is not a draft"));
        }
        if hash != a.plan_hash.as_str() {
            return Err(conflict("approval hash does not match the stored plan"));
        }
        if now >= expires_at {
            return Err(conflict("plan has expired"));
        }
        sqlx::query(
            "INSERT INTO approvals (plan_id, plan_hash, approved_at, approved_by, confirmed_count, \
               confirmed_torrent_removals, acknowledged_samples, acknowledged_unrecoverable) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&hash)
        .bind(a.approved_at.0)
        .bind(enum_text(&a.by)?)
        .bind(i64::from(a.confirmed_count))
        .bind(i64::from(a.confirmed_torrent_removals))
        .bind(enc(&a.acknowledged_samples)?)
        .bind(enc(&a.acknowledged_unrecoverable)?)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        let done = sqlx::query(
            "UPDATE plans SET status = 'approved', updated_at = ? WHERE id = ? AND status = 'draft'",
        )
        .bind(now.0)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if done.rows_affected() != 1 {
            return Err(conflict("plan status changed during approval"));
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn transition(
        &self,
        id: PlanId,
        from: PlanStatus,
        to: PlanStatus,
    ) -> Result<(), PortError> {
        from.transition(to).map_err(domain)?;
        if to == PlanStatus::Approved {
            return Err(invalid("a plan becomes approved only through approve()"));
        }
        let idt = uuid_text(id.0);
        let mut tx = self.db.write_tx().await?;
        let done =
            sqlx::query("UPDATE plans SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
                .bind(enum_text(&to)?)
                .bind(self.clock.now().0)
                .bind(&idt)
                .bind(enum_text(&from)?)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        if done.rows_affected() == 0 {
            return Err(match status_of(&mut tx, &idt).await? {
                None => not_found("plan not found"),
                Some(_) => conflict("plan is not in the expected status"),
            });
        }
        tx.commit().await.map_err(map_sqlx)
    }
}
