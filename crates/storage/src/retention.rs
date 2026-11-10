//! Retention primitives. Only derived or per-session data can be pruned, and only when a caller
//! asks (`prune`). Audit evidence (plans, actions, approvals, live journal rows, gains, observed
//! deltas) has no prune path at all, and database triggers refuse deletes even from raw SQL.
//!
//! The current inventory of each root keeps its errors and protected zones, whatever the cutoff.

use removarr_domain::error::PortError;
use removarr_domain::time::Timestamp;

use crate::codec::col;
use crate::db::Db;
use crate::error::map_sqlx;

#[derive(Clone, Debug)]
pub struct Retention {
    db: Db,
}

/// What a prune removed (or would remove, for [`Retention::preview`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub scan_errors: u64,
    pub protected_zones: u64,
    pub gone_entries: u64,
    pub simulated_results: u64,
    pub simulated_intents: u64,
    pub simulation_runs: u64,
    pub resolved_issues: u64,
}

/// Row counts of the tables that are never pruned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditCounts {
    pub plans: u64,
    pub plan_actions: u64,
    pub approvals: u64,
    pub journal_intents: u64,
    pub journal_results: u64,
    pub gains: u64,
    pub observed_deltas: u64,
}

fn keep_current_inventory(table: &str) -> String {
    format!(
        "{table}.session_id IN (SELECT id FROM scan_sessions WHERE started_at < ?1) AND NOT EXISTS ( \
           SELECT 1 FROM session_roots sr JOIN scan_sessions s ON s.id = sr.session_id \
           WHERE sr.session_id = {table}.session_id AND sr.root = {table}.root \
             AND sr.inventory_saved = 1 AND s.started_at = ( \
               SELECT MAX(s2.started_at) FROM session_roots sr2 \
               JOIN scan_sessions s2 ON s2.id = sr2.session_id \
               WHERE sr2.root = sr.root AND sr2.inventory_saved = 1))"
    )
}

/// `(report field, table, where clause)`. Order matters: results before their intents.
fn targets() -> Vec<(&'static str, String)> {
    vec![
        (
            "journal_results",
            "intent_id IN (SELECT id FROM journal_intents WHERE mode = 'simulated' AND created_at < ?1)"
                .to_string(),
        ),
        ("journal_intents", "mode = 'simulated' AND created_at < ?1".to_string()),
        ("simulation_runs", "finished_at < ?1".to_string()),
        ("scan_errors", keep_current_inventory("scan_errors")),
        ("protected_zones", keep_current_inventory("protected_zones")),
        ("fs_entries", "gone_session IS NOT NULL AND gone_at < ?1".to_string()),
        ("issues", "resolved_at IS NOT NULL AND resolved_at < ?1".to_string()),
    ]
}

impl Retention {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Counts what `prune(older_than)` would delete. Writes nothing.
    pub async fn preview(&self, older_than: Timestamp) -> Result<PruneReport, PortError> {
        let mut tx = self.db.read_tx().await?;
        let mut counts = Vec::new();
        for (table, clause) in targets() {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table} WHERE {clause}"))
                .bind(older_than.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            counts.push(u64::try_from(col::<i64>(&row, "n")?).unwrap_or(0));
        }
        Ok(report(&counts))
    }

    /// Deletes per-session and derived rows older than `older_than`, in one transaction.
    /// Never touches audit evidence. Must be called explicitly (for example at the end of a scan).
    pub async fn prune(&self, older_than: Timestamp) -> Result<PruneReport, PortError> {
        let mut tx = self.db.write_tx().await?;
        let mut counts = Vec::new();
        for (table, clause) in targets() {
            let done = sqlx::query(&format!("DELETE FROM {table} WHERE {clause}"))
                .bind(older_than.0)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            counts.push(done.rows_affected());
        }
        tx.commit().await.map_err(map_sqlx)?;
        Ok(report(&counts))
    }

    /// Sizes of the protected audit tables.
    pub async fn audit_counts(&self) -> Result<AuditCounts, PortError> {
        let mut tx = self.db.read_tx().await?;
        let mut n = Vec::new();
        for t in [
            "plans",
            "plan_actions",
            "approvals",
            "journal_intents",
            "journal_results",
            "gains",
            "observed_deltas",
        ] {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {t}"))
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            n.push(u64::try_from(col::<i64>(&row, "n")?).unwrap_or(0));
        }
        Ok(AuditCounts {
            plans: n[0],
            plan_actions: n[1],
            approvals: n[2],
            journal_intents: n[3],
            journal_results: n[4],
            gains: n[5],
            observed_deltas: n[6],
        })
    }
}

fn report(c: &[u64]) -> PruneReport {
    PruneReport {
        simulated_results: c[0],
        simulated_intents: c[1],
        simulation_runs: c[2],
        scan_errors: c[3],
        protected_zones: c[4],
        gone_entries: c[5],
        resolved_issues: c[6],
    }
}
