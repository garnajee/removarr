//! Persistent job runtime, lanes, scheduler and locks (architecture 8, ADR-0007).
//!
//! - [`runtime::JobRuntime`]: the persistent queue, the Observe and Execute workers, restart
//!   restoration, cancellation, retries, recovery gating and the command surface for
//!   `backend-api` (`submit`, `rescan`, `cancel`, `retry_recovery`, `resume_auto`, `status`,
//!   `subscribe`).
//! - [`schedule`], [`backoff`], [`limits`], [`locks`], [`events`]: the building blocks.
//! - [`auto`]: the persisted `auto_paused` latch, failed sets from the journal and the
//!   two-phase automatic cycle.
//! - [`handler`] and [`workflow`]: the contract with the code that does the work, and its
//!   production implementation over `removarr-workflows`.
//!
//! This crate approves nothing and mutates nothing by itself: every mutation goes through the
//! action runner behind its own journal.

pub mod auto;
pub mod backoff;
pub mod config;
pub mod events;
pub mod handler;
pub mod limits;
pub mod locks;
pub mod runtime;
pub mod schedule;
pub mod workflow;

pub use auto::{
    ActionHistory, AutoControl, CycleEngine, ExecutedAction, FailedSets, PendingAutoPlans,
};
pub use config::{JobsConfig, LimitsConfig, RetryConfig, ScheduleConfig};
pub use events::{Hub, JobEvent, JobEventKind, JobProgress, LogLevel};
pub use handler::{JobCtx, JobError, JobHandler, JobSuccess, RecoverySummary, ScanSummary};
pub use limits::{Limits, Resource};
pub use locks::{Locks, Scope};
pub use runtime::{
    CancelResult, Deps, Enqueued, JobRuntime, RecoveryStatus, RuntimeStatus, StartReport,
};
