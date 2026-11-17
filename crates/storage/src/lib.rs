//! SQLite persistence, migrations and repositories (architecture section 9, ADR-0002).
//!
//! No sqlx type appears in the public API: repositories implement the `removarr-domain`
//! persistence ports and return `PortError`. Open the database with [`Db::open`], then build
//! the repositories with [`Repos::new`].

mod codec;
mod db;
mod error;
mod recovery;
mod repos;
mod retention;

use std::sync::Arc;

use removarr_domain::ports::Clock;

pub use db::Db;
pub use recovery::{Recovery, RecoverySnapshot};
pub use repos::{
    ActionRow, Enqueued, GainRepo, IgnoreRepo, IssueRepo, JobRepo, JournalRepo, MatchRepo,
    PlanRepo, ScanRepo, SnapshotRepo, StateRepo, arr_media_key,
};
pub use retention::{AuditCounts, PruneReport, Retention};

/// Every repository over one [`Db`]. Fields are cheap to clone and hand out as `Arc<dyn Port>`.
#[derive(Clone, Debug)]
pub struct Repos {
    pub scans: ScanRepo,
    pub snapshots: SnapshotRepo,
    pub matches: MatchRepo,
    pub issues: IssueRepo,
    pub ignores: IgnoreRepo,
    pub plans: PlanRepo,
    pub journal: JournalRepo,
    pub gains: GainRepo,
    pub jobs: JobRepo,
    pub state: StateRepo,
    pub retention: Retention,
    pub recovery: Recovery,
}

impl Repos {
    /// `clock` stamps rows the ports give no time for (resolved issues, status changes).
    #[must_use]
    pub fn new(db: &Db, clock: Arc<dyn Clock>) -> Self {
        let plans = PlanRepo::new(db.clone(), clock.clone());
        let journal = JournalRepo::new(db.clone(), clock.clone());
        let jobs = JobRepo::new(db.clone(), clock.clone());
        Self {
            scans: ScanRepo::new(db.clone(), clock.clone()),
            snapshots: SnapshotRepo::new(db.clone()),
            matches: MatchRepo::new(db.clone()),
            issues: IssueRepo::new(db.clone(), clock.clone()),
            ignores: IgnoreRepo::new(db.clone(), clock.clone()),
            gains: GainRepo::new(db.clone(), clock.clone()),
            state: StateRepo::new(db.clone(), clock),
            retention: Retention::new(db.clone()),
            recovery: Recovery::new(journal.clone(), plans.clone(), jobs.clone()),
            plans,
            journal,
            jobs,
        }
    }
}
