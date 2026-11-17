//! `JobStore`: persisted jobs. A partial unique index on `dedupe_key` (queued or running)
//! prevents duplicate active jobs; `enqueue` returns the existing job's id in that case.

use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::JobId;
use removarr_domain::jobs::{JobOutcome, JobRecord, JobStatus, Lane};
use removarr_domain::ports::{Clock, JobStore};
use removarr_domain::time::Timestamp;
use sqlx::sqlite::SqliteRow;
use sqlx::{Sqlite, Transaction};

use crate::codec::{
    col, dec, enc, enum_text, from_enum_text, parse_uuid, scrub, to_u32, uuid_text,
};
use crate::db::Db;
use crate::error::{conflict, domain, invalid, map_sqlx, not_found};

/// Result of [`JobRepo::enqueue_outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// A new queued row was written.
    New(JobId),
    /// An active job with the same dedupe key already existed; nothing was written.
    Existing(JobId),
}

#[derive(Clone)]
pub struct JobRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for JobRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRepo").finish_non_exhaustive()
    }
}

fn job_from_row(r: &SqliteRow) -> Result<JobRecord, PortError> {
    Ok(JobRecord {
        id: JobId::new(parse_uuid(&col::<String>(r, "id")?)?),
        kind: dec(&col::<String>(r, "kind")?)?,
        dedupe_key: col(r, "dedupe_key")?,
        lane: from_enum_text(&col::<String>(r, "lane")?)?,
        status: from_enum_text(&col::<String>(r, "status")?)?,
        enqueued_at: Timestamp(col(r, "enqueued_at")?),
        started_at: col::<Option<i64>>(r, "started_at")?.map(Timestamp),
        finished_at: col::<Option<i64>>(r, "finished_at")?.map(Timestamp),
        stage: col(r, "stage")?,
        attempts: to_u32(col(r, "attempts")?)?,
        outcome: col::<Option<String>>(r, "outcome")?
            .map(|s| dec(&s))
            .transpose()?,
    })
}

fn scrub_job_outcome(o: &JobOutcome) -> JobOutcome {
    match o {
        JobOutcome::Failed { class, message } => JobOutcome::Failed {
            class: class.clone(),
            message: scrub(message),
        },
        other => other.clone(),
    }
}

async fn fetch(
    tx: &mut Transaction<'static, Sqlite>,
    id: &str,
) -> Result<Option<JobRecord>, PortError> {
    sqlx::query("SELECT * FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?
        .map(|r| job_from_row(&r))
        .transpose()
}

impl JobRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    pub async fn get(&self, id: JobId) -> Result<Option<JobRecord>, PortError> {
        let mut tx = self.db.read_tx().await?;
        fetch(&mut tx, &uuid_text(id.0)).await
    }

    /// Jobs in `Running` state (recovery view).
    pub async fn running(&self) -> Result<Vec<JobRecord>, PortError> {
        let rows =
            sqlx::query("SELECT * FROM jobs WHERE status = 'running' ORDER BY started_at, rowid")
                .fetch_all(self.db.reader())
                .await
                .map_err(map_sqlx)?;
        rows.iter().map(job_from_row).collect()
    }

    /// Like `JobStore::enqueue`, but tells the caller whether the job is new.
    pub async fn enqueue_outcome(&self, job: &JobRecord) -> Result<Enqueued, PortError> {
        if job.status != JobStatus::Queued {
            return Err(invalid("only a queued job can be enqueued"));
        }
        if job.kind.dedupe_key().map_err(domain)? != job.dedupe_key || job.kind.lane() != job.lane {
            return Err(invalid("job dedupe key or lane does not match its kind"));
        }
        let mut tx = self.db.write_tx().await?;
        let dup = sqlx::query(
            "SELECT id FROM jobs WHERE dedupe_key = ? AND status IN ('queued', 'running')",
        )
        .bind(&job.dedupe_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if let Some(r) = dup {
            return Ok(Enqueued::Existing(JobId::new(parse_uuid(&col::<String>(
                &r, "id",
            )?)?)));
        }
        sqlx::query(
            "INSERT INTO jobs (id, kind, dedupe_key, lane, status, enqueued_at, attempts) \
             VALUES (?, ?, ?, ?, 'queued', ?, 0)",
        )
        .bind(uuid_text(job.id.0))
        .bind(enc(&job.kind)?)
        .bind(&job.dedupe_key)
        .bind(enum_text(&job.lane)?)
        .bind(job.enqueued_at.0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)?;
        Ok(Enqueued::New(job.id))
    }
}

#[async_trait]
impl JobStore for JobRepo {
    async fn enqueue(&self, job: &JobRecord) -> Result<JobId, PortError> {
        Ok(match self.enqueue_outcome(job).await? {
            Enqueued::New(id) | Enqueued::Existing(id) => id,
        })
    }

    async fn claim_next(&self, lane: Lane) -> Result<Option<JobRecord>, PortError> {
        let now = self.clock.now().0;
        let mut tx = self.db.write_tx().await?;
        let Some(row) = sqlx::query(
            "SELECT id FROM jobs WHERE lane = ? AND status = 'queued' \
             ORDER BY enqueued_at, rowid LIMIT 1",
        )
        .bind(enum_text(&lane)?)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?
        else {
            return Ok(None);
        };
        let id: String = col(&row, "id")?;
        sqlx::query(
            "UPDATE jobs SET status = 'running', started_at = ?, attempts = attempts + 1 \
             WHERE id = ? AND status = 'queued'",
        )
        .bind(now)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        let job = fetch(&mut tx, &id).await?;
        tx.commit().await.map_err(map_sqlx)?;
        Ok(job)
    }

    async fn set_stage(&self, id: JobId, stage: &str) -> Result<(), PortError> {
        let idt = uuid_text(id.0);
        let mut tx = self.db.write_tx().await?;
        let done = sqlx::query(
            "UPDATE jobs SET stage = ? WHERE id = ? AND status = 'running' AND stage IS NOT ?",
        )
        .bind(scrub(stage))
        .bind(&idt)
        .bind(scrub(stage))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if done.rows_affected() == 0 {
            match fetch(&mut tx, &idt).await? {
                None => return Err(not_found("job not found")),
                Some(j) if j.status != JobStatus::Running => {
                    return Err(conflict("job is not running"));
                }
                Some(_) => return Ok(()),
            }
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn finish(&self, id: JobId, outcome: &JobOutcome) -> Result<(), PortError> {
        let idt = uuid_text(id.0);
        let to = outcome.status();
        let mut tx = self.db.write_tx().await?;
        let Some(job) = fetch(&mut tx, &idt).await? else {
            return Err(not_found("job not found"));
        };
        job.status.transition(to).map_err(domain)?;
        let done = sqlx::query(
            "UPDATE jobs SET status = ?, finished_at = ?, outcome = ? WHERE id = ? AND status = ?",
        )
        .bind(enum_text(&to)?)
        .bind(self.clock.now().0)
        .bind(enc(&scrub_job_outcome(outcome))?)
        .bind(&idt)
        .bind(enum_text(&job.status)?)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if done.rows_affected() != 1 {
            return Err(PortError::new(
                ErrorClass::Conflict,
                "job status changed concurrently",
            ));
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn interrupt_running(&self) -> Result<Vec<JobRecord>, PortError> {
        let now = self.clock.now().0;
        let mut tx = self.db.write_tx().await?;
        let rows =
            sqlx::query("SELECT id FROM jobs WHERE status = 'running' ORDER BY started_at, rowid")
                .fetch_all(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        let outcome = enc(&JobOutcome::Interrupted)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = col(r, "id")?;
            sqlx::query(
                "UPDATE jobs SET status = 'interrupted', finished_at = ?, outcome = ? WHERE id = ?",
            )
            .bind(now)
            .bind(&outcome)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if let Some(j) = fetch(&mut tx, &id).await? {
                out.push(j);
            }
        }
        tx.commit().await.map_err(map_sqlx)?;
        Ok(out)
    }
}
