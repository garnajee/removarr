//! Execute and verify (architecture 7.4, 7.5; agent 13). The safety-critical executor.
//!
//! Rules enforced here, in this order for every plan:
//! 1. an [`ExecutionTicket`] from the planner is the only input: tamper-checked plan, approval
//!    of exactly its hash, and the `simulate` flag;
//! 2. every action kind above [`IMPLEMENTED_TIER`] or outside [`supported`] is refused before
//!    anything happens (`NotInBuild`);
//! 3. every precondition is revalidated against **live** reads (`stat`, `snapshot_dir`, a fresh
//!    torrent listing and file lists, ARR queue and fingerprints, the config fingerprint) before
//!    the first service mutation, and again after the torrents are stopped; any difference
//!    refuses the action and invalidates the plan;
//! 4. every mutation is `record_intent` (durable, `synchronous = FULL`) then the adapter call
//!    then `record_result`; a journal failure stops the run before the mutation;
//! 5. simulations (`simulate = true`, or a dry-run process) walk the same steps and write the
//!    same journal records with `JournalMode::Simulated`; no mutator is ever invoked and the
//!    plan status never changes;
//! 6. qBittorrent: affected hashes are stopped before the mutation, rechecked after it with a
//!    timeout, and only hashes that were active initially and are either untouched or
//!    recheck-verified are started again; everything else stays stopped and is reported;
//! 7. an action is `Verified` only when every verification step passes on a real re-read;
//!    otherwise it is `ExecutedUnverified`;
//! 8. unfinished live intents found at startup are resolved by [`recovery::recover`] without
//!    ever redoing a mutation; the plan becomes `Interrupted`.
//!
//! Tiers 1-3 (stage B step 2): the live exact-copy consolidation, the tier-2 service commands
//! (`ArrRescan`, `JellyfinRefresh`), the deletions under the completed root with their
//! companions, the manual stalled-incomplete cleanup and the targeted mode fix, each with its
//! simulation. Tier-4 kinds are explicit `NotInBuild` refusals, live and simulated alike,
//! until their CP2 review passes (MASTER section 10). A plan mixing families is refused too.
//! A simulation (`simulate = true`, or any run of a dry-run process) always executes on the
//! recording adapters: `execute` swaps them in before the context exists (CP2T3-F04).

mod batch;
mod consolidate;
mod delete;
pub mod dry_run;
pub mod events;
mod journal;
mod qbit;
pub mod recovery;
mod revalidate;
mod run;
mod simple;
mod stalled;
mod verify;

use std::sync::Arc;
use std::time::Duration;

use removarr_domain::error::PortError;
use removarr_domain::plan::ActionKind;
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{
    ArrRead, Clock, ContentReader, FsInventory, GainStore, IdGen, JellyfinRead, JournalStore,
    PlanStore, QbitRead, ScanStore, SnapshotStore,
};
use removarr_matching::proof::ContentVerifier;

pub use dry_run::{
    CallCounter, Mutators, RecordingArrCommands, RecordingFsMutator, RecordingJellyfinCommands,
    RecordingQbitControl, RecordingSet,
};
pub use events::{ActionEvent, ActionProgress, EventSink, Reporter};
pub use qbit::{LeftStoppedReason, RestorePlan, plan_restore};
pub use recovery::{RecoveredPlan, RecoveryReport, recover};
pub use run::{ActionResult, ExecutionReport, execute};

/// Highest risk tier this build executes (architecture 6.7, 13.1). Raised only after the
/// tier's crash-point and dry-run tests pass and CP2 passes. Stage B step 2: tier 3.
pub const IMPLEMENTED_TIER: RiskTier = RiskTier::T3;

/// Kinds this build can execute live: every kind at or below [`IMPLEMENTED_TIER`].
#[must_use]
pub fn supported(kind: ActionKind) -> bool {
    kind.tier() <= IMPLEMENTED_TIER
}

/// Families of kinds a plan may hold (one family per plan; the planner builds them so).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Consolidate,
    Delete,
    StalledCleanup,
    FixMode,
    Service,
}

/// The family of a kind, `None` for kinds outside this build.
#[must_use]
pub fn family(kind: ActionKind) -> Option<Family> {
    use ActionKind::*;
    match kind {
        ConsolidateHardlink => Some(Family::Consolidate),
        DeleteCompletedFile
        | DeleteCompletedFolder
        | DeleteJunk
        | DeleteIncompleteOrphan
        | RemoveEmptyParent
        | RemoveTorrentTask => Some(Family::Delete),
        CleanupStalledIncomplete => Some(Family::StalledCleanup),
        FixMode => Some(Family::FixMode),
        ArrRescan | JellyfinRefresh => Some(Family::Service),
        ReplaceRelease | DeleteMediaFile | ArrGrabRelease | ArrRemoveQueueItem
        | ArrSetMonitored => None,
    }
}

/// Plans in `Executing` status (storage `PlanRepo::ids_with_status(Executing)`). Recovery
/// resolves them too: a process killed between a step result and the next intent leaves no
/// unfinished intent, yet its torrents may be stopped.
#[async_trait::async_trait]
pub trait ExecutingPlans: Send + Sync {
    async fn executing(&self) -> Result<Vec<removarr_domain::ids::PlanId>, PortError>;
}

/// Read ports and stores of the runner. Mutating adapters live in [`Mutators`].
#[derive(Clone)]
pub struct ActionPorts {
    pub plans: Arc<dyn PlanStore>,
    pub executing: Arc<dyn ExecutingPlans>,
    pub journal: Arc<dyn JournalStore>,
    pub gains: Arc<dyn GainStore>,
    pub scans: Arc<dyn ScanStore>,
    pub snapshots: Arc<dyn SnapshotStore>,
    pub fs: Arc<dyn FsInventory>,
    pub content: Arc<dyn ContentReader>,
    pub qbit: Option<Arc<dyn QbitRead>>,
    pub arr: Vec<Arc<dyn ArrRead>>,
    pub jellyfin: Option<Arc<dyn JellyfinRead>>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

impl std::fmt::Debug for ActionPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionPorts")
            .field("qbit", &self.qbit.is_some())
            .field("arr", &self.arr.len())
            .field("jellyfin", &self.jellyfin.is_some())
            .finish_non_exhaustive()
    }
}

/// Execution settings from the configuration.
#[derive(Clone)]
pub struct ActionSettings {
    /// `Config::fingerprint()`: must equal every action's `ConfigFingerprint` precondition.
    pub config_fingerprint: String,
    /// `thresholds.recheck_timeout_seconds`.
    pub recheck_timeout: Duration,
    /// Poll interval of the recheck (architecture: 2 s).
    pub recheck_interval: Duration,
    /// Bounded full-compare runner (shared with `evaluate`).
    pub verifier: ContentVerifier,
}

impl std::fmt::Debug for ActionSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionSettings")
            .field("recheck_timeout", &self.recheck_timeout)
            .field("recheck_interval", &self.recheck_interval)
            .finish_non_exhaustive()
    }
}
