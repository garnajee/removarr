//! `MatchStore`: persisted match funnel output (current set only, diffed on replace).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::evidence::{ContentProof, MatchCandidate};
use removarr_domain::ids::SessionId;
use removarr_domain::path::{RelPath, RootedPath};
use removarr_domain::ports::MatchStore;

use crate::codec::{col, dec, enc, parse_uuid, uuid_text};
use crate::db::Db;
use crate::error::{domain, invalid, map_sqlx, not_found};

const SESSION_KEY: &str = "match_session";

#[derive(Clone, Debug)]
pub struct MatchRepo {
    db: Db,
}

impl MatchRepo {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }
}

type Key = (String, String, String, String);

fn key(a: &RootedPath, b: &RootedPath) -> Key {
    (
        a.root.as_str().to_string(),
        a.rel.as_str().to_string(),
        b.root.as_str().to_string(),
        b.rel.as_str().to_string(),
    )
}

/// Columns that decide whether a stored row is unchanged.
type Body = [String; 5];

fn body(c: &MatchCandidate) -> Result<Body, PortError> {
    Ok([
        enc(&c.sig_a)?,
        enc(&c.sig_b)?,
        enc(&c.identity)?,
        enc(&c.content)?,
        enc(&c.reasons)?,
    ])
}

#[async_trait]
impl MatchStore for MatchRepo {
    async fn replace_for_session(
        &self,
        session: SessionId,
        c: &[MatchCandidate],
    ) -> Result<(), PortError> {
        let sid = uuid_text(session.0);
        let mut incoming: HashMap<Key, Body> = HashMap::with_capacity(c.len());
        for m in c {
            if incoming.insert(key(&m.a, &m.b), body(m)?).is_some() {
                return Err(invalid("the same candidate pair is listed twice"));
            }
        }
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
        let rows = sqlx::query("SELECT * FROM match_candidates")
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let mut existing: HashMap<Key, Body> = HashMap::with_capacity(rows.len());
        for r in &rows {
            existing.insert(
                (
                    col(r, "a_root")?,
                    col(r, "a_rel")?,
                    col(r, "b_root")?,
                    col(r, "b_rel")?,
                ),
                [
                    col(r, "sig_a")?,
                    col(r, "sig_b")?,
                    col(r, "identity")?,
                    col(r, "content")?,
                    col(r, "reasons")?,
                ],
            );
        }
        for (k, b) in &incoming {
            if existing.get(k) == Some(b) {
                continue;
            }
            sqlx::query(
                "INSERT INTO match_candidates (a_root, a_rel, b_root, b_rel, sig_a, sig_b, \
                   identity, content, reasons) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (a_root, a_rel, b_root, b_rel) DO UPDATE SET sig_a = excluded.sig_a, \
                   sig_b = excluded.sig_b, identity = excluded.identity, \
                   content = excluded.content, reasons = excluded.reasons",
            )
            .bind(&k.0)
            .bind(&k.1)
            .bind(&k.2)
            .bind(&k.3)
            .bind(&b[0])
            .bind(&b[1])
            .bind(&b[2])
            .bind(&b[3])
            .bind(&b[4])
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        let stale: HashSet<&Key> = existing
            .keys()
            .filter(|k| !incoming.contains_key(*k))
            .collect();
        for k in stale {
            sqlx::query(
                "DELETE FROM match_candidates WHERE a_root = ? AND a_rel = ? AND b_root = ? AND b_rel = ?",
            )
            .bind(&k.0)
            .bind(&k.1)
            .bind(&k.2)
            .bind(&k.3)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
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

    async fn update_proof(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        proof: &ContentProof,
    ) -> Result<(), PortError> {
        let proof = enc(proof)?;
        let (ka, kb) = (key(a, b), key(b, a));
        let mut tx = self.db.write_tx().await?;
        let done = sqlx::query(
            "UPDATE match_candidates SET content = ? WHERE content <> ? AND \
             ((a_root = ? AND a_rel = ? AND b_root = ? AND b_rel = ?) OR \
              (a_root = ? AND a_rel = ? AND b_root = ? AND b_rel = ?))",
        )
        .bind(&proof)
        .bind(&proof)
        .bind(&ka.0)
        .bind(&ka.1)
        .bind(&ka.2)
        .bind(&ka.3)
        .bind(&kb.0)
        .bind(&kb.1)
        .bind(&kb.2)
        .bind(&kb.3)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if done.rows_affected() == 0 {
            let exists = sqlx::query(
                "SELECT 1 FROM match_candidates WHERE \
                 (a_root = ? AND a_rel = ? AND b_root = ? AND b_rel = ?) OR \
                 (a_root = ? AND a_rel = ? AND b_root = ? AND b_rel = ?)",
            )
            .bind(&ka.0)
            .bind(&ka.1)
            .bind(&ka.2)
            .bind(&ka.3)
            .bind(&kb.0)
            .bind(&kb.1)
            .bind(&kb.2)
            .bind(&kb.3)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if exists.is_none() {
                return Err(not_found("match candidate not found"));
            }
        }
        tx.commit().await.map_err(map_sqlx)
    }

    async fn current(&self) -> Result<Vec<MatchCandidate>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let Some(meta) = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(SESSION_KEY)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
        else {
            return Ok(vec![]);
        };
        let session = SessionId::new(parse_uuid(&col::<String>(&meta, "value")?)?);
        let rows =
            sqlx::query("SELECT * FROM match_candidates ORDER BY a_root, a_rel, b_root, b_rel")
                .fetch_all(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        let path = |root: String, rel: String| -> Result<RootedPath, PortError> {
            Ok(RootedPath::new(
                removarr_domain::path::RootId::new(root).map_err(domain)?,
                RelPath::new(rel).map_err(domain)?,
            ))
        };
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(MatchCandidate {
                a: path(col(r, "a_root")?, col(r, "a_rel")?)?,
                b: path(col(r, "b_root")?, col(r, "b_rel")?)?,
                sig_a: dec(&col::<String>(r, "sig_a")?)?,
                sig_b: dec(&col::<String>(r, "sig_b")?)?,
                identity: dec(&col::<String>(r, "identity")?)?,
                content: dec(&col::<String>(r, "content")?)?,
                reasons: dec(&col::<String>(r, "reasons")?)?,
                session,
            });
        }
        Ok(out)
    }
}
