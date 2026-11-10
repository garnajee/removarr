//! Recovery queries after a restart. Read-only; resolving what they find is the job of the
//! action runner (architecture 7.5). Nothing here mutates the journal or any plan.

use std::collections::BTreeMap;

use removarr_domain::error::PortError;
use removarr_domain::ids::PlanId;
use removarr_domain::jobs::JobRecord;
use removarr_domain::journal::{IntentRecord, UnfinishedIntent};
use removarr_domain::plan::PlanStatus;
use removarr_domain::ports::JournalStore;

use crate::repos::{JobRepo, JournalRepo, PlanRepo};

/// Everything that was in flight when the process stopped.
#[derive(Debug, Clone, Default)]
pub struct RecoverySnapshot {
    /// Live intents without a result, oldest first.
    pub unfinished: Vec<UnfinishedIntent>,
    /// Plans still `Executing`. Each must become `Interrupted`; none can resume.
    pub executing_plans: Vec<PlanId>,
    /// Jobs still `Running`. `JobStore::interrupt_running` marks them.
    pub running_jobs: Vec<JobRecord>,
}

impl RecoverySnapshot {
    /// True when an execute job must not start before recovery has run.
    #[must_use]
    pub fn needs_recovery(&self) -> bool {
        !self.unfinished.is_empty() || !self.executing_plans.is_empty()
    }

    /// Unfinished intents grouped by plan, in journal order.
    #[must_use]
    pub fn unfinished_by_plan(&self) -> BTreeMap<PlanId, Vec<&IntentRecord>> {
        let mut m: BTreeMap<PlanId, Vec<&IntentRecord>> = BTreeMap::new();
        for u in &self.unfinished {
            m.entry(u.intent.action.plan).or_default().push(&u.intent);
        }
        m
    }
}

#[derive(Clone, Debug)]
pub struct Recovery {
    journal: JournalRepo,
    plans: PlanRepo,
    jobs: JobRepo,
}

impl Recovery {
    pub(crate) fn new(journal: JournalRepo, plans: PlanRepo, jobs: JobRepo) -> Self {
        Self {
            journal,
            plans,
            jobs,
        }
    }

    pub async fn snapshot(&self) -> Result<RecoverySnapshot, PortError> {
        Ok(RecoverySnapshot {
            unfinished: self.journal.unfinished().await?,
            executing_plans: self.plans.ids_with_status(PlanStatus::Executing).await?,
            running_jobs: self.jobs.running().await?,
        })
    }
}
