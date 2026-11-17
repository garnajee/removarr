//! `IssueStore`: current issues. `first_seen_at` survives replaces; issues that a later
//! evaluation no longer reports get `resolved_at` (the row is kept, never deleted here).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::canonical::sha256_hex;
use removarr_domain::error::PortError;
use removarr_domain::evidence::Evidence;
use removarr_domain::ids::{IssueId, SessionId};
use removarr_domain::issue::Issue;
use removarr_domain::ports::{Clock, IssueStore};
use removarr_domain::time::Timestamp;
use sqlx::sqlite::SqliteRow;
use sqlx::{Sqlite, Transaction};

use crate::codec::{
    col, dec, enc, enum_text, from_enum_text, from_i64, parse_uuid, to_i64, uuid_text,
};
use crate::db::Db;
use crate::error::{invalid, map_sqlx, not_found};

const SESSION_KEY: &str = "issues_session";

#[derive(Clone)]
pub struct IssueRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for IssueRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssueRepo").finish_non_exhaustive()
    }
}

impl IssueRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

/// Hash of everything except `session` and `first_seen_at`, which do not make a row "changed".
fn body_hash(i: &Issue) -> Result<String, PortError> {
    let mut c = i.clone();
    c.session = SessionId::new(removarr_domain::ids::RawUuid::nil());
    c.first_seen_at = Timestamp(0);
    Ok(sha256_hex(enc(&c)?.as_bytes()))
}

async fn current_session(tx: &mut Transaction<'static, Sqlite>) -> Result<SessionId, PortError> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(SESSION_KEY)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    match row {
        Some(r) => Ok(SessionId::new(parse_uuid(&col::<String>(&r, "value")?)?)),
        None => Ok(SessionId::new(removarr_domain::ids::RawUuid::nil())),
    }
}

fn issue_from_row(r: &SqliteRow, session: SessionId) -> Result<Issue, PortError> {
    Ok(Issue {
        id: IssueId(col(r, "id")?),
        category: from_enum_text(&col::<String>(r, "category")?)?,
        subject: removarr_domain::ids::SubjectKey(col(r, "subject")?),
        unit: dec(&col::<String>(r, "unit")?)?,
        severity: from_enum_text(&col::<String>(r, "severity")?)?,
        outcome: from_enum_text(&col::<String>(r, "outcome")?)?,
        evidence: dec(&col::<String>(r, "evidence")?)?,
        blockers: dec(&col::<String>(r, "blockers")?)?,
        torrent_hashes: dec(&col::<String>(r, "torrent_hashes")?)?,
        logical_bytes: from_i64(col(r, "logical_bytes")?),
        potential_bytes: from_i64(col(r, "potential_bytes")?),
        age_basis: Timestamp(col(r, "age_basis")?),
        first_seen_at: Timestamp(col(r, "first_seen_at")?),
        session,
        physical_ids: dec(&col::<String>(r, "physical_ids")?)?,
    })
}

#[async_trait]
impl IssueStore for IssueRepo {
    async fn replace_current(&self, session: SessionId, issues: &[Issue]) -> Result<(), PortError> {
        let sid = uuid_text(session.0);
        let mut hashes: HashMap<&str, String> = HashMap::with_capacity(issues.len());
        for i in issues {
            if i.id != i.expected_id() {
                return Err(invalid("issue id does not match category and subject"));
            }
            if hashes.insert(i.id.as_str(), body_hash(i)?).is_some() {
                return Err(invalid("the same issue is listed twice"));
            }
        }
        let now = self.clock.now().0;
        let mut tx = self.db.write_tx().await?;
        if sqlx::query("SELECT 1 FROM scan_sessions WHERE id = ?")
            .bind(&sid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .is_none()
        {
            return Err(not_found("scan session not found"));
        }
        let rows = sqlx::query("SELECT id, body_hash, resolved_at FROM issues")
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let mut existing: HashMap<String, (String, bool)> = HashMap::with_capacity(rows.len());
        for r in &rows {
            existing.insert(
                col(r, "id")?,
                (
                    col(r, "body_hash")?,
                    col::<Option<i64>>(r, "resolved_at")?.is_some(),
                ),
            );
        }
        for i in issues {
            let hash = &hashes[i.id.as_str()];
            if existing
                .get(i.id.as_str())
                .is_some_and(|(h, resolved)| h == hash && !resolved)
            {
                continue;
            }
            sqlx::query(
                "INSERT INTO issues (id, category, subject, severity, outcome, unit, evidence, \
                   blockers, torrent_hashes, physical_ids, logical_bytes, potential_bytes, \
                   age_basis, first_seen_at, resolved_at, body_hash) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?) \
                 ON CONFLICT (id) DO UPDATE SET severity = excluded.severity, \
                   outcome = excluded.outcome, unit = excluded.unit, evidence = excluded.evidence, \
                   blockers = excluded.blockers, torrent_hashes = excluded.torrent_hashes, \
                   physical_ids = excluded.physical_ids, logical_bytes = excluded.logical_bytes, \
                   potential_bytes = excluded.potential_bytes, age_basis = excluded.age_basis, \
                   resolved_at = NULL, body_hash = excluded.body_hash",
            )
            .bind(i.id.as_str())
            .bind(i.category.as_str())
            .bind(i.subject.as_str())
            .bind(enum_text(&i.severity)?)
            .bind(enum_text(&i.outcome)?)
            .bind(enc(&i.unit)?)
            .bind(enc(&i.evidence)?)
            .bind(enc(&i.blockers)?)
            .bind(enc(&i.torrent_hashes)?)
            .bind(enc(&i.physical_ids)?)
            .bind(to_i64(i.logical_bytes))
            .bind(to_i64(i.potential_bytes))
            .bind(i.age_basis.0)
            .bind(i.first_seen_at.0)
            .bind(hash)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        let keep: HashSet<&str> = hashes.keys().copied().collect();
        for (id, (_, resolved)) in &existing {
            if !resolved && !keep.contains(id.as_str()) {
                sqlx::query("UPDATE issues SET resolved_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_sqlx)?;
            }
        }
        sqlx::query(
            "INSERT INTO meta (key, value) VALUES (?, ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value WHERE value <> excluded.value",
        )
        .bind(SESSION_KEY)
        .bind(&sid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn get_many(&self, ids: &[IssueId]) -> Result<Vec<Issue>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let session = current_session(&mut tx).await?;
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids.iter().filter(|i| seen.insert(i.as_str())) {
            if let Some(r) =
                sqlx::query("SELECT * FROM issues WHERE id = ? AND resolved_at IS NULL")
                    .bind(id.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(map_sqlx)?
            {
                out.push(issue_from_row(&r, session)?);
            }
        }
        Ok(out)
    }

    async fn attach_evidence(&self, id: &IssueId, ev: &[Evidence]) -> Result<(), PortError> {
        let mut tx = self.db.write_tx().await?;
        let session = current_session(&mut tx).await?;
        let Some(row) = sqlx::query("SELECT * FROM issues WHERE id = ? AND resolved_at IS NULL")
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
        else {
            return Err(not_found("issue not found"));
        };
        let mut issue = issue_from_row(&row, session)?;
        let mut added = false;
        for e in ev {
            if !issue.evidence.contains(e) {
                issue.evidence.push(e.clone());
                added = true;
            }
        }
        if !added {
            return Ok(());
        }
        sqlx::query("UPDATE issues SET evidence = ?, body_hash = ? WHERE id = ?")
            .bind(enc(&issue.evidence)?)
            .bind(body_hash(&issue)?)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }
}
