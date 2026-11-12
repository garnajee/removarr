#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(dead_code, clippy::too_many_arguments)]
//! Fixture: a real storage database populated with raw SQL (the audit triggers are respected).

use std::sync::Arc;

use removarr_domain::evidence::{Completeness, Evidence, EvidenceKind, IdentityProof, Provenance};
use removarr_domain::ids::{RawUuid, SessionId};
use removarr_domain::issue::Unit;
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::Clock;
use removarr_domain::time::Timestamp;
use removarr_reporting::Reporter;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

pub const NOW: i64 = 1_700_000_000_000;
pub const HOUR: i64 = 3_600_000;
pub const S1: &str = "00000000-0000-0000-0000-000000000001";
pub const S0: &str = "00000000-0000-0000-0000-000000000000";
pub const P1: &str = "00000000-0000-0000-0000-0000000000a1";
pub const P2: &str = "00000000-0000-0000-0000-0000000000a2";
pub const P_DRY: &str = "00000000-0000-0000-0000-0000000000d1";

#[derive(Debug)]
pub struct FixedClock(pub i64);
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0)
    }
}

#[derive(Debug)]
pub struct Fx {
    pub dir: tempfile::TempDir,
    pub w: SqlitePool,
    pub r: Reporter,
}

pub async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("removarr.db");
    let db = removarr_storage::Db::open(&path).await.unwrap();
    db.close().await;
    let w = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let r = Reporter::open(&path, Arc::new(FixedClock(NOW)))
        .await
        .unwrap();
    Fx { dir, w, r }
}

pub async fn exec(w: &SqlitePool, sql: &str) {
    sqlx::query(sql)
        .execute(w)
        .await
        .unwrap_or_else(|e| panic!("{e}: {sql}"));
}

pub fn sid(s: &str) -> SessionId {
    SessionId::new(RawUuid::parse_str(s).unwrap())
}

pub fn q(s: &str) -> String {
    s.replace('\'', "''")
}

pub fn note(session: &str, code: &str, completeness: Completeness) -> Evidence {
    Evidence {
        kind: EvidenceKind::Identity {
            subject: format!("note:{code}"),
            proof: IdentityProof::None,
        },
        provenance: Provenance::Filesystem {
            session: sid(session),
        },
        observed_at: Timestamp(NOW - 2 * HOUR),
        completeness,
    }
}

pub fn file_unit(rel: &str) -> String {
    serde_json::to_string(&Unit::File(RootedPath::new(
        RootId::completed(),
        RelPath::new(rel).unwrap(),
    )))
    .unwrap()
}

pub async fn session(w: &SqlitePool, id: &str, started: i64, finished: Option<i64>, status: &str) {
    exec(
        w,
        &format!(
            "INSERT INTO scan_sessions (id, started_at, finished_at, status, metrics) VALUES ('{id}', {started}, {}, '{status}', '{{\"entries_scanned\":10,\"scan_errors\":2,\"bytes_sampled\":0,\"bytes_compared\":0,\"duration_ms\":5}}')",
            finished.map_or("NULL".to_string(), |f| f.to_string())
        ),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn issue(
    w: &SqlitePool,
    id: &str,
    category: &str,
    severity: &str,
    subject: &str,
    unit: &str,
    evidence: &[Evidence],
    logical: i64,
    potential: i64,
    phys: &[(u64, u64)],
) {
    let phys_json = serde_json::to_string(
        &phys
            .iter()
            .map(|(d, i)| serde_json::json!({"dev": d, "ino": i}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    exec(
        w,
        &format!(
            "INSERT INTO issues (id, category, subject, severity, outcome, unit, evidence, blockers, torrent_hashes, physical_ids, logical_bytes, potential_bytes, age_basis, first_seen_at, body_hash) \
             VALUES ('{id}', '{category}', '{}', '{severity}', 'keep_manual', '{}', '{}', '[]', '[]', '{phys_json}', {logical}, {potential}, {NOW}, {}, 'h')",
            q(subject),
            q(unit),
            q(&serde_json::to_string(evidence).unwrap()),
            NOW - 24 * HOUR
        ),
    )
    .await;
}

pub async fn file_entry(
    w: &SqlitePool,
    root: &str,
    rel: &str,
    dev: i64,
    ino: i64,
    size: i64,
    nlink: i64,
    alloc: i64,
    session: &str,
) {
    exec(
        w,
        &format!(
            "INSERT INTO fs_entries (root, rel, kind, dev, ino, size, nlink, mtime_ns, mode, uid, gid, allocated, first_seen_at, changed_session) \
             VALUES ('{root}', '{}', 'file', {dev}, {ino}, {size}, {nlink}, 0, 420, 0, 0, {alloc}, 0, '{session}')",
            q(rel)
        ),
    )
    .await;
}

pub async fn plan(
    w: &SqlitePool,
    id: &str,
    hash_char: char,
    session: &str,
    mode: &str,
    origin: &str,
    body: &str,
    created: i64,
) {
    exec(
        w,
        &format!(
            "INSERT INTO plans (id, hash, status, mode, origin, session_id, created_at, expires_at, updated_at, body) \
             VALUES ('{id}', '{}', 'draft', '{mode}', '{origin}', '{session}', {created}, {}, {created}, '{}')",
            hash_char.to_string().repeat(64),
            created + 24 * HOUR,
            q(body)
        ),
    )
    .await;
}

pub async fn approve(w: &SqlitePool, id: &str, hash_char: char, by: &str, at: i64) {
    exec(
        w,
        &format!(
            "INSERT INTO approvals (plan_id, plan_hash, approved_at, approved_by, confirmed_count, confirmed_torrent_removals, acknowledged_samples, acknowledged_unrecoverable) \
             VALUES ('{id}', '{}', {at}, '{by}', 1, 0, '[]', '[]')",
            hash_char.to_string().repeat(64)
        ),
    )
    .await;
}

pub async fn set_status(w: &SqlitePool, id: &str, status: &str) {
    exec(
        w,
        &format!("UPDATE plans SET status = '{status}' WHERE id = '{id}'"),
    )
    .await;
}

pub async fn action(
    w: &SqlitePool,
    plan: &str,
    idx: i64,
    kind: &str,
    state: &str,
    outcome: Option<&str>,
    at: i64,
) {
    exec(
        w,
        &format!(
            "INSERT INTO plan_actions (plan_id, idx, kind, state, outcome, updated_at) VALUES ('{plan}', {idx}, '{kind}', '{state}', {}, {at})",
            outcome.map_or("NULL".to_string(), |o| format!("'{}'", q(o)))
        ),
    )
    .await;
}

pub async fn gain(w: &SqlitePool, id: &str, plan: &str, idx: i64, kind: &str, bytes: i64, at: i64) {
    exec(
        w,
        &format!("INSERT INTO gains (gain_id, plan_id, action_idx, kind, dev, ino, bytes, recorded_at) VALUES ('{id}', '{plan}', {idx}, '{kind}', 1, {}, {bytes}, {at})", at % 1000),
    )
    .await;
}
