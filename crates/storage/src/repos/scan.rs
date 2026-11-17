//! `ScanStore`: sessions and the current filesystem inventory.
//!
//! `save_inventory` is one transaction per call. It loads the current rows of the root, writes
//! only added, changed or revived entries, and marks entries gone only when the scan finished
//! and nothing (error or protected zone) hides them. Unchanged entries cause no write.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::fs::{
    DiffStats, EntryKind, EntrySignature, FsEntry, ProtectedZone, RootInventory, ScanErrorRecord,
};
use removarr_domain::ids::SessionId;
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::{Clock, ScanStore};
use removarr_domain::session::{
    AuditSession, RootStatus, ServiceKind, ServiceState, ServiceStatus, SessionMetrics,
    SessionStatus,
};
use removarr_domain::time::{MtimeNs, Timestamp};
use sqlx::sqlite::SqliteRow;
use sqlx::{Sqlite, Transaction};
use std::sync::Arc;

use crate::codec::{
    col, dec, enc, enum_text, from_enum_text, from_i64, parse_uuid, scrub, to_i64, to_u32,
    uuid_text,
};
use crate::db::Db;
use crate::error::{conflict, domain, invalid, map_sqlx, not_found};

#[derive(Clone)]
pub struct ScanRepo {
    db: Db,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ScanRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanRepo").finish_non_exhaustive()
    }
}

/// Signature columns of a stored entry, for the unchanged check.
#[derive(PartialEq, Eq)]
struct Stored {
    sig: EntrySignature,
    allocated: u64,
    symlink_target: Option<String>,
    gone: bool,
}

impl ScanRepo {
    pub(crate) fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }

    /// Loads one session with its roots and services.
    pub async fn session(&self, id: SessionId) -> Result<Option<AuditSession>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let row = sqlx::query("SELECT * FROM scan_sessions WHERE id = ?")
            .bind(uuid_text(id.0))
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(load_session(&mut tx, &r).await?)),
            None => Ok(None),
        }
    }
}

async fn load_session(
    tx: &mut Transaction<'static, Sqlite>,
    r: &SqliteRow,
) -> Result<AuditSession, PortError> {
    let id_text: String = col(r, "id")?;
    let roots = sqlx::query(
        "SELECT root, scanned, error_count, protected_count FROM session_roots \
         WHERE session_id = ? ORDER BY root",
    )
    .bind(&id_text)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let mut root_status = Vec::with_capacity(roots.len());
    for row in &roots {
        root_status.push(RootStatus {
            root: RootId::new(col::<String>(row, "root")?).map_err(domain)?,
            scanned: col::<i64>(row, "scanned")? != 0,
            error_count: to_u32(col(row, "error_count")?)?,
            protected_count: to_u32(col(row, "protected_count")?)?,
        });
    }
    let svc = sqlx::query(
        "SELECT service, state, observed_at FROM session_services \
         WHERE session_id = ? ORDER BY service",
    )
    .bind(&id_text)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    let mut services = Vec::with_capacity(svc.len());
    for row in &svc {
        services.push(ServiceStatus {
            service: from_enum_text::<ServiceKind>(&col::<String>(row, "service")?)?,
            state: dec::<ServiceState>(&col::<String>(row, "state")?)?,
            observed_at: Timestamp(col(row, "observed_at")?),
        });
    }
    Ok(AuditSession {
        id: SessionId::new(parse_uuid(&id_text)?),
        started_at: Timestamp(col(r, "started_at")?),
        finished_at: col::<Option<i64>>(r, "finished_at")?.map(Timestamp),
        status: from_enum_text::<SessionStatus>(&col::<String>(r, "status")?)?,
        roots: root_status,
        services,
        metrics: dec::<SessionMetrics>(&col::<String>(r, "metrics")?)?,
    })
}

fn scrub_state(s: &ServiceState) -> ServiceState {
    match s {
        ServiceState::Unavailable { reason } => ServiceState::Unavailable {
            reason: scrub(reason),
        },
        other => other.clone(),
    }
}

async fn upsert_root_status(
    tx: &mut Transaction<'static, Sqlite>,
    session: &str,
    r: &RootStatus,
) -> Result<(), PortError> {
    sqlx::query(
        "INSERT INTO session_roots (session_id, root, scanned, error_count, protected_count) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (session_id, root) DO UPDATE SET scanned = excluded.scanned, \
           error_count = excluded.error_count, protected_count = excluded.protected_count",
    )
    .bind(session)
    .bind(r.root.as_str())
    .bind(i64::from(r.scanned))
    .bind(i64::from(r.error_count))
    .bind(i64::from(r.protected_count))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

async fn upsert_service(
    tx: &mut Transaction<'static, Sqlite>,
    session: &str,
    s: &ServiceStatus,
) -> Result<(), PortError> {
    sqlx::query(
        "INSERT INTO session_services (session_id, service, state, observed_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (session_id, service) DO UPDATE SET state = excluded.state, \
           observed_at = excluded.observed_at",
    )
    .bind(session)
    .bind(enum_text(&s.service)?)
    .bind(enc(&scrub_state(&s.state))?)
    .bind(s.observed_at.0)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Used by the snapshot repository.
pub(crate) async fn save_services(
    tx: &mut Transaction<'static, Sqlite>,
    session: &str,
    statuses: &[ServiceStatus],
) -> Result<(), PortError> {
    for s in statuses {
        upsert_service(tx, session, s).await?;
    }
    Ok(())
}

fn entry_from_row(root: &RootId, r: &SqliteRow) -> Result<(FsEntry, bool), PortError> {
    let rel = RelPath::new(col::<String>(r, "rel")?).map_err(domain)?;
    let sig = EntrySignature {
        kind: from_enum_text::<EntryKind>(&col::<String>(r, "kind")?)?,
        dev: from_i64(col(r, "dev")?),
        ino: from_i64(col(r, "ino")?),
        size: from_i64(col(r, "size")?),
        nlink: from_i64(col(r, "nlink")?),
        mtime_ns: MtimeNs(col(r, "mtime_ns")?),
        mode: to_u32(col(r, "mode")?)?,
        uid: to_u32(col(r, "uid")?)?,
        gid: to_u32(col(r, "gid")?)?,
    };
    let gone = col::<Option<String>>(r, "gone_session")?.is_some();
    Ok((
        FsEntry {
            path: RootedPath::new(root.clone(), rel),
            sig,
            allocated: from_i64(col(r, "allocated")?),
            symlink_target: col(r, "symlink_target")?,
        },
        gone,
    ))
}

#[async_trait]
impl ScanStore for ScanRepo {
    async fn begin_session(&self, s: &AuditSession) -> Result<(), PortError> {
        if s.status != SessionStatus::Running || s.finished_at.is_some() {
            return Err(invalid("a session must begin in the running state"));
        }
        let id = uuid_text(s.id.0);
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO scan_sessions (id, started_at, finished_at, status, metrics) \
             VALUES (?, ?, NULL, 'running', ?)",
        )
        .bind(&id)
        .bind(s.started_at.0)
        .bind(enc(&s.metrics)?)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        for r in &s.roots {
            upsert_root_status(&mut tx, &id, r).await?;
        }
        save_services(&mut tx, &id, &s.services).await?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn save_inventory(
        &self,
        session: SessionId,
        inv: &RootInventory,
    ) -> Result<DiffStats, PortError> {
        let sid = uuid_text(session.0);
        let root = inv.root.as_str();
        if inv.entries.iter().any(|e| e.path.root != inv.root)
            || inv.errors.iter().any(|e| e.path.root != inv.root)
            || inv.protected.iter().any(|z| z.path.root != inv.root)
        {
            return Err(invalid("inventory contains paths of another root"));
        }
        let mut tx = self.db.write_tx().await?;
        let started: Option<(i64, String)> =
            sqlx::query_as("SELECT started_at, status FROM scan_sessions WHERE id = ?")
                .bind(&sid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        let Some((started_at, status)) = started else {
            return Err(not_found("scan session not found"));
        };
        if status != "running" {
            return Err(conflict("scan session is already finished"));
        }

        let rows = sqlx::query("SELECT * FROM fs_entries WHERE root = ?")
            .bind(root)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let mut existing: HashMap<String, Stored> = HashMap::with_capacity(rows.len());
        for r in &rows {
            let (e, gone) = entry_from_row(&inv.root, r)?;
            existing.insert(
                e.path.rel.as_str().to_string(),
                Stored {
                    sig: e.sig,
                    allocated: e.allocated,
                    symlink_target: e.symlink_target,
                    gone,
                },
            );
        }
        drop(rows);

        let mut stats = DiffStats::default();
        let mut seen: HashSet<&str> = HashSet::with_capacity(inv.entries.len());
        for e in &inv.entries {
            let rel = e.path.rel.as_str();
            if !seen.insert(rel) {
                return Err(invalid("inventory lists the same path twice"));
            }
            let current = Stored {
                sig: e.sig,
                allocated: e.allocated,
                symlink_target: e.symlink_target.clone(),
                gone: false,
            };
            match existing.get(rel) {
                Some(old) if *old == current => {
                    stats.unchanged += 1;
                    continue;
                }
                Some(old) if old.gone => stats.added += 1,
                Some(_) => stats.changed += 1,
                None => stats.added += 1,
            }
            sqlx::query(
                "INSERT INTO fs_entries (root, rel, kind, dev, ino, size, nlink, mtime_ns, mode, \
                   uid, gid, allocated, symlink_target, first_seen_at, changed_session) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (root, rel) DO UPDATE SET kind = excluded.kind, dev = excluded.dev, \
                   ino = excluded.ino, size = excluded.size, nlink = excluded.nlink, \
                   mtime_ns = excluded.mtime_ns, mode = excluded.mode, uid = excluded.uid, \
                   gid = excluded.gid, allocated = excluded.allocated, \
                   symlink_target = excluded.symlink_target, \
                   changed_session = excluded.changed_session, \
                   gone_session = NULL, gone_at = NULL",
            )
            .bind(root)
            .bind(rel)
            .bind(enum_text(&e.sig.kind)?)
            .bind(to_i64(e.sig.dev))
            .bind(to_i64(e.sig.ino))
            .bind(to_i64(e.sig.size))
            .bind(to_i64(e.sig.nlink))
            .bind(e.sig.mtime_ns.0)
            .bind(i64::from(e.sig.mode))
            .bind(i64::from(e.sig.uid))
            .bind(i64::from(e.sig.gid))
            .bind(to_i64(e.allocated))
            .bind(e.symlink_target.as_deref())
            .bind(started_at)
            .bind(&sid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }

        // Removal: only for a finished scan, and never under a directory that had an error or
        // a protected zone (the scan could not see it, so absence proves nothing).
        if inv.finished {
            let hidden: Vec<&RelPath> = inv
                .errors
                .iter()
                .map(|e| &e.path.rel)
                .chain(inv.protected.iter().map(|z| &z.path.rel))
                .collect();
            let now = self.clock.now().0;
            for (rel, old) in &existing {
                if old.gone || seen.contains(rel.as_str()) {
                    continue;
                }
                let rel_path = RelPath::new(rel.clone()).map_err(domain)?;
                if hidden.iter().any(|h| rel_path.starts_with(h)) {
                    continue;
                }
                sqlx::query(
                    "UPDATE fs_entries SET gone_session = ?, gone_at = ? WHERE root = ? AND rel = ?",
                )
                .bind(&sid)
                .bind(now)
                .bind(root)
                .bind(rel)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                stats.removed += 1;
            }
        }

        replace_issues_rows(&mut tx, &sid, root, &inv.errors, &inv.protected).await?;
        sqlx::query(
            "INSERT INTO session_roots (session_id, root, scanned, error_count, protected_count, \
               root_sig, inventory_saved, inventory_finished) VALUES (?, ?, 1, ?, ?, ?, 1, ?) \
             ON CONFLICT (session_id, root) DO UPDATE SET root_sig = excluded.root_sig, \
               inventory_saved = 1, inventory_finished = excluded.inventory_finished",
        )
        .bind(&sid)
        .bind(root)
        .bind(i64::try_from(inv.errors.len()).unwrap_or(i64::MAX))
        .bind(i64::try_from(inv.protected.len()).unwrap_or(i64::MAX))
        .bind(match &inv.root_sig {
            Some(s) => Some(enc(s)?),
            None => None,
        })
        .bind(i64::from(inv.finished))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)?;
        Ok(stats)
    }

    async fn finish_session(&self, s: &AuditSession) -> Result<(), PortError> {
        let Some(finished_at) = s.finished_at else {
            return Err(invalid("finish_session needs finished_at"));
        };
        if !s.status.is_terminal() {
            return Err(invalid("finish_session needs a terminal status"));
        }
        let id = uuid_text(s.id.0);
        let mut tx = self.db.write_tx().await?;
        let done = sqlx::query(
            "UPDATE scan_sessions SET status = ?, finished_at = ?, metrics = ? \
             WHERE id = ? AND status = 'running'",
        )
        .bind(enum_text(&s.status)?)
        .bind(finished_at.0)
        .bind(enc(&s.metrics)?)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if done.rows_affected() == 0 {
            let exists = sqlx::query("SELECT 1 FROM scan_sessions WHERE id = ?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
            return Err(if exists.is_some() {
                conflict("scan session is not running")
            } else {
                not_found("scan session not found")
            });
        }
        for r in &s.roots {
            upsert_root_status(&mut tx, &id, r).await?;
        }
        save_services(&mut tx, &id, &s.services).await?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn latest_session(&self) -> Result<Option<AuditSession>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let row =
            sqlx::query("SELECT * FROM scan_sessions ORDER BY started_at DESC, rowid DESC LIMIT 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(load_session(&mut tx, &r).await?)),
            None => Ok(None),
        }
    }

    async fn load_inventory(&self, root: &RootId) -> Result<RootInventory, PortError> {
        let mut tx = self.db.read_tx().await?;
        let rows = sqlx::query(
            "SELECT * FROM fs_entries WHERE root = ? AND gone_session IS NULL ORDER BY rel",
        )
        .bind(root.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        let mut entries = Vec::with_capacity(rows.len());
        for r in &rows {
            entries.push(entry_from_row(root, r)?.0);
        }
        let latest = sqlx::query(
            "SELECT sr.session_id, sr.root_sig, sr.inventory_finished FROM session_roots sr \
             JOIN scan_sessions s ON s.id = sr.session_id \
             WHERE sr.root = ? AND sr.inventory_saved = 1 \
             ORDER BY s.started_at DESC, s.rowid DESC LIMIT 1",
        )
        .bind(root.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        let Some(latest) = latest else {
            return Ok(RootInventory {
                root: root.clone(),
                root_sig: None,
                entries,
                errors: vec![],
                protected: vec![],
                finished: false,
            });
        };
        let sid: String = col(&latest, "session_id")?;
        let root_sig = match col::<Option<String>>(&latest, "root_sig")? {
            Some(s) => Some(dec::<EntrySignature>(&s)?),
            None => None,
        };
        let mut errors = Vec::new();
        for r in sqlx::query(
            "SELECT rel, io, message FROM scan_errors WHERE session_id = ? AND root = ? ORDER BY rowid",
        )
        .bind(&sid)
        .bind(root.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx)?
        {
            errors.push(ScanErrorRecord {
                path: RootedPath::new(
                    root.clone(),
                    RelPath::new(col::<String>(&r, "rel")?).map_err(domain)?,
                ),
                io: from_enum_text(&col::<String>(&r, "io")?)?,
                message: col(&r, "message")?,
            });
        }
        let mut protected = Vec::new();
        for r in sqlx::query(
            "SELECT rel, reason FROM protected_zones WHERE session_id = ? AND root = ? ORDER BY rowid",
        )
        .bind(&sid)
        .bind(root.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx)?
        {
            protected.push(ProtectedZone {
                path: RootedPath::new(
                    root.clone(),
                    RelPath::new(col::<String>(&r, "rel")?).map_err(domain)?,
                ),
                reason: from_enum_text(&col::<String>(&r, "reason")?)?,
            });
        }
        Ok(RootInventory {
            root: root.clone(),
            root_sig,
            entries,
            errors,
            protected,
            finished: col::<i64>(&latest, "inventory_finished")? != 0,
        })
    }
}

async fn replace_issues_rows(
    tx: &mut Transaction<'static, Sqlite>,
    session: &str,
    root: &str,
    errors: &[ScanErrorRecord],
    protected: &[ProtectedZone],
) -> Result<(), PortError> {
    sqlx::query("DELETE FROM scan_errors WHERE session_id = ? AND root = ?")
        .bind(session)
        .bind(root)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    sqlx::query("DELETE FROM protected_zones WHERE session_id = ? AND root = ?")
        .bind(session)
        .bind(root)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    for e in errors {
        sqlx::query(
            "INSERT INTO scan_errors (session_id, root, rel, io, message) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session)
        .bind(root)
        .bind(e.path.rel.as_str())
        .bind(enum_text(&e.io)?)
        .bind(scrub(&e.message))
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }
    for z in protected {
        sqlx::query(
            "INSERT INTO protected_zones (session_id, root, rel, reason) VALUES (?, ?, ?, ?)",
        )
        .bind(session)
        .bind(root)
        .bind(z.path.rel.as_str())
        .bind(enum_text(&z.reason)?)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
