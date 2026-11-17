//! `JournalStore`: the intent journal.
//!
//! `record_intent` returns only after its transaction committed. The writer connection runs with
//! `synchronous = FULL`, so the WAL is fsynced before `COMMIT` returns: the intent is durable
//! before the caller mutates anything (INV-08). Live intents are accepted only for an executing
//! plan (trigger); results need an existing intent (foreign key) and are unique per intent.

use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ids::{ActionRef, IntentId, PlanId};
use removarr_domain::journal::{
    ActionOutcome, ActionState, IntentRecord, ResultRecord, StepOutcome, UnfinishedIntent,
};
use removarr_domain::ports::{Clock, JournalStore};
use removarr_domain::time::Timestamp;
use sqlx::sqlite::SqliteRow;

use crate::codec::{
    col, dec, enc, enum_text, from_enum_text, parse_uuid, scrub, scrub_value, to_u32, uuid_text,
};
use crate::db::Db;
use crate::error::{conflict, map_sqlx, not_found};

#[derive(Clone)]
pub struct JournalRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for JournalRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalRepo").finish_non_exhaustive()
    }
}

impl JournalRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

fn intent_from_row(r: &SqliteRow) -> Result<IntentRecord, PortError> {
    Ok(IntentRecord {
        id: IntentId::new(parse_uuid(&col::<String>(r, "id")?)?),
        action: ActionRef {
            plan: PlanId::new(parse_uuid(&col::<String>(r, "plan_id")?)?),
            index: to_u32(col(r, "action_idx")?)?,
        },
        step: from_enum_text(&col::<String>(r, "step")?)?,
        mode: from_enum_text(&col::<String>(r, "mode")?)?,
        target: dec(&col::<String>(r, "target")?)?,
        observed: dec(&col::<String>(r, "observed")?)?,
        expected_gain: dec(&col::<String>(r, "expected_gain")?)?,
        hashes: dec(&col::<String>(r, "hashes")?)?,
        created_at: Timestamp(col(r, "created_at")?),
    })
}

fn scrub_outcome(o: &StepOutcome) -> StepOutcome {
    match o {
        StepOutcome::Done { final_state } => StepOutcome::Done {
            final_state: scrub_value(final_state),
        },
        StepOutcome::Failed { class, message } => StepOutcome::Failed {
            class: class.clone(),
            message: scrub(message),
        },
        StepOutcome::Skipped { reason } => StepOutcome::Skipped {
            reason: scrub(reason),
        },
        StepOutcome::Simulated => StepOutcome::Simulated,
    }
}

fn scrub_action_outcome(o: &ActionOutcome) -> ActionOutcome {
    match o {
        ActionOutcome::ExecutedUnverified { reason } => ActionOutcome::ExecutedUnverified {
            reason: scrub(reason),
        },
        other => other.clone(),
    }
}

#[async_trait]
impl JournalStore for JournalRepo {
    async fn record_intent(&self, i: &IntentRecord) -> Result<(), PortError> {
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO journal_intents (id, plan_id, action_idx, step, mode, target, observed, \
               expected_gain, hashes, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid_text(i.id.0))
        .bind(uuid_text(i.action.plan.0))
        .bind(i64::from(i.action.index))
        .bind(enum_text(&i.step)?)
        .bind(enum_text(&i.mode)?)
        .bind(enc(&scrub_value(&i.target))?)
        .bind(enc(&scrub_value(&i.observed))?)
        .bind(enc(&i.expected_gain)?)
        .bind(enc(&i.hashes)?)
        .bind(i.created_at.0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn record_result(&self, r: &ResultRecord) -> Result<(), PortError> {
        let id = uuid_text(r.intent_id.0);
        let mut tx = self.db.write_tx().await?;
        if sqlx::query("SELECT 1 FROM journal_intents WHERE id = ?")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .is_none()
        {
            return Err(not_found("journal intent not found"));
        }
        sqlx::query("INSERT INTO journal_results (intent_id, outcome, at) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(enc(&scrub_outcome(&r.outcome))?)
            .bind(r.at.0)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn record_outcome(&self, a: ActionRef, o: &ActionOutcome) -> Result<(), PortError> {
        let plan = uuid_text(a.plan.0);
        let new_state = o.state();
        let mut tx = self.db.write_tx().await?;
        let Some(row) =
            sqlx::query("SELECT state, outcome FROM plan_actions WHERE plan_id = ? AND idx = ?")
                .bind(&plan)
                .bind(i64::from(a.index))
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
        else {
            return Err(not_found("plan action not found"));
        };
        let old: ActionState = from_enum_text(&col::<String>(&row, "state")?)?;
        let body = enc(&scrub_action_outcome(o))?;
        if old == new_state
            && col::<Option<String>>(&row, "outcome")?.as_deref() == Some(body.as_str())
        {
            return Ok(());
        }
        // The runner records the final outcome of an action that is still open. After that only
        // the transitions the domain allows (re-verification) may change it.
        let open = matches!(
            old,
            ActionState::Pending
                | ActionState::Running
                | ActionState::Verifying
                | ActionState::Compensating
        );
        if !open && !old.can_transition_to(new_state) {
            return Err(conflict("action outcome is already final"));
        }
        sqlx::query(
            "UPDATE plan_actions SET state = ?, outcome = ?, updated_at = ? WHERE plan_id = ? AND idx = ?",
        )
        .bind(enum_text(&new_state)?)
        .bind(body)
        .bind(self.clock.now().0)
        .bind(&plan)
        .bind(i64::from(a.index))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn unfinished(&self) -> Result<Vec<UnfinishedIntent>, PortError> {
        let rows = sqlx::query(
            "SELECT i.* FROM journal_intents i \
             LEFT JOIN journal_results r ON r.intent_id = i.id \
             WHERE i.mode = 'live' AND r.intent_id IS NULL ORDER BY i.seq",
        )
        .fetch_all(self.db.reader())
        .await
        .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| {
                Ok(UnfinishedIntent {
                    intent: intent_from_row(r)?,
                })
            })
            .collect()
    }

    async fn for_plan(
        &self,
        id: PlanId,
    ) -> Result<Vec<(IntentRecord, Option<ResultRecord>)>, PortError> {
        let rows = sqlx::query(
            "SELECT i.*, r.outcome AS r_outcome, r.at AS r_at FROM journal_intents i \
             LEFT JOIN journal_results r ON r.intent_id = i.id \
             WHERE i.plan_id = ? ORDER BY i.seq",
        )
        .bind(uuid_text(id.0))
        .fetch_all(self.db.reader())
        .await
        .map_err(map_sqlx)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let intent = intent_from_row(r)?;
            let result = match col::<Option<String>>(r, "r_outcome")? {
                Some(o) => Some(ResultRecord {
                    intent_id: intent.id,
                    outcome: dec(&o)?,
                    at: Timestamp(col(r, "r_at")?),
                }),
                None => None,
            };
            out.push((intent, result));
        }
        Ok(out)
    }
}
