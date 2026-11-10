//! Shared fixtures. Every test uses its own temporary database.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use removarr_domain::evidence::{Completeness, EvidenceLevel};
use removarr_domain::fs::{EntryKind, EntrySignature, FsEntry, RootInventory};
use removarr_domain::gain::{GainEstimate, PlanGains};
use removarr_domain::ids::{PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionPlan, ActionTarget, Approval, ApprovalRequest, HashedPlan, PlanMode,
    PlanOrigin, PlannedAction,
};
use removarr_domain::ports::{PlanStore, ScanStore};
use removarr_domain::session::{AuditSession, RootStatus, SessionMetrics, SessionStatus};
use removarr_domain::time::{MtimeNs, Timestamp};
use removarr_storage::{Db, Repos};
use removarr_testkit::clock::FakeClock;
use sqlx::Connection;
use sqlx::sqlite::SqliteConnection;
use tempfile::TempDir;

pub struct Fx {
    pub dir: TempDir,
    pub db: Db,
    pub repos: Repos,
    pub clock: Arc<FakeClock>,
}

impl Fx {
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("removarr.db")
    }

    /// A raw connection for inspection and for attempts to bypass the repositories.
    pub async fn raw(&self) -> SqliteConnection {
        SqliteConnection::connect(&format!("sqlite://{}", self.path().display()))
            .await
            .expect("raw connection")
    }
}

pub async fn fx() -> Fx {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(&dir.path().join("removarr.db"))
        .await
        .expect("open");
    let clock = Arc::new(FakeClock::default());
    let repos = Repos::new(&db, clock.clone());
    Fx {
        dir,
        db,
        repos,
        clock,
    }
}

pub fn sid(n: u128) -> SessionId {
    SessionId::new(RawUuid::from_u128(n))
}

pub fn pid(n: u128) -> PlanId {
    PlanId::new(RawUuid::from_u128(0x1000 + n))
}

pub fn session(n: u128, at: i64) -> AuditSession {
    AuditSession {
        id: sid(n),
        started_at: Timestamp(at),
        finished_at: None,
        status: SessionStatus::Running,
        roots: vec![RootStatus {
            root: RootId::completed(),
            scanned: false,
            error_count: 0,
            protected_count: 0,
        }],
        services: vec![],
        metrics: SessionMetrics::default(),
    }
}

pub async fn begin(f: &Fx, n: u128) -> AuditSession {
    let s = session(n, f.clock_ms() + i64::try_from(n).unwrap());
    f.repos.scans.begin_session(&s).await.expect("begin");
    s
}

impl Fx {
    pub fn clock_ms(&self) -> i64 {
        use removarr_domain::ports::Clock;
        self.clock.now().0
    }
}

pub fn rp(rel: &str) -> RootedPath {
    RootedPath::parse("completed", rel).expect("path")
}

pub fn sig(ino: u64, size: u64) -> EntrySignature {
    EntrySignature {
        kind: EntryKind::File,
        dev: 1,
        ino,
        size,
        nlink: 1,
        mtime_ns: MtimeNs(1_000),
        mode: 0o644,
        uid: 1000,
        gid: 1000,
    }
}

pub fn entry(rel: &str, ino: u64, size: u64) -> FsEntry {
    FsEntry {
        path: rp(rel),
        sig: sig(ino, size),
        allocated: size,
        symlink_target: None,
    }
}

pub fn inventory(entries: Vec<FsEntry>, finished: bool) -> RootInventory {
    RootInventory {
        root: RootId::completed(),
        root_sig: None,
        entries,
        errors: vec![],
        protected: vec![],
        finished,
    }
}

/// A valid draft plan of `n` junk deletions in the `completed` root, expiring in 24 hours.
pub fn plan(f: &Fx, n: u128, session: SessionId, actions: u32) -> ActionPlan {
    let created = Timestamp(f.clock_ms());
    ActionPlan {
        id: pid(n),
        created_at: created,
        expires_at: created.plus_hours(24),
        mode: PlanMode::Live,
        origin: PlanOrigin::User,
        session,
        config_fingerprint: "cfg".into(),
        actions: (0..actions)
            .map(|i| PlannedAction {
                index: i,
                kind: ActionKind::DeleteJunk,
                target: ActionTarget::Path {
                    path: rp(&format!("junk/{n}-{i}.nfo")),
                },
                preconditions: vec![],
                verification: vec![],
                expected_gain: GainEstimate::default(),
                depends_on: vec![],
                torrent_hashes: BTreeSet::new(),
                evidence_level: EvidenceLevel::Sampled,
                completeness: Completeness::Complete,
            })
            .collect(),
        excluded: vec![(SubjectKey("p:completed:x".into()), vec![])],
        lock_roots: BTreeSet::from([RootId::completed()]),
        torrent_states: BTreeMap::new(),
        arr_snapshots: vec![],
        samples: vec![],
        gains: PlanGains::default(),
    }
}

pub fn approval_for(f: &Fx, plan: &ActionPlan) -> Approval {
    let sealed = HashedPlan::seal(plan.clone()).expect("seal");
    sealed
        .approve(
            &ApprovalRequest {
                plan_hash: sealed.hash().clone(),
                confirmed_count: plan.required_count(),
                confirmed_torrent_removals: 0,
                acknowledged_samples: vec![],
                acknowledged_unrecoverable: vec![],
            },
            Timestamp(f.clock_ms()),
        )
        .expect("approve")
}

/// Session + sealed plan inserted as a draft. Returns the plan.
pub async fn insert_plan(f: &Fx, n: u128, actions: u32) -> ActionPlan {
    let s = begin(f, n).await;
    let p = plan(f, n, s.id, actions);
    let sealed = HashedPlan::seal(p.clone()).expect("seal");
    f.repos
        .plans
        .insert(&p, sealed.hash())
        .await
        .expect("insert plan");
    p
}

/// Plan inserted, approved and moved to `Executing`.
pub async fn executing_plan(f: &Fx, n: u128, actions: u32) -> ActionPlan {
    use removarr_domain::plan::PlanStatus;
    let p = insert_plan(f, n, actions).await;
    f.repos
        .plans
        .approve(&approval_for(f, &p))
        .await
        .expect("approve");
    f.repos
        .plans
        .transition(p.id, PlanStatus::Approved, PlanStatus::Executing)
        .await
        .expect("executing");
    p
}
