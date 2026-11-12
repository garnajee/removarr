//! The contract between the runtime and the code that does the work. The runtime owns lanes,
//! locks, limits, cancellation, retries, persistence and events; a [`JobHandler`] only runs one
//! job. [`crate::workflow::WorkflowHandler`] is the production implementation.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::{JobId, SessionId};
use removarr_domain::jobs::JobKind;
use removarr_domain::ports::{CancelFlag, Clock};
use removarr_workflows::actions::EventSink;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::auto::AutoControl;
use crate::events::ProgressHandle;
use crate::limits::Limits;
use crate::locks::{Locks, Scope};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobError {
    /// The job stopped at a safe checkpoint because cancellation was requested.
    #[error("cancelled")]
    Cancelled,
    #[error("{class:?}: {message}")]
    Failed { class: ErrorClass, message: String },
}

impl JobError {
    #[must_use]
    pub fn failed(class: ErrorClass, message: impl Into<String>) -> Self {
        JobError::Failed {
            class,
            message: message.into(),
        }
    }

    /// True for a failure worth another attempt (non-mutating jobs only).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            JobError::Failed {
                class: ErrorClass::Unavailable,
                ..
            }
        )
    }
}

impl From<PortError> for JobError {
    fn from(e: PortError) -> Self {
        if e.class == ErrorClass::Cancelled {
            JobError::Cancelled
        } else {
            JobError::Failed {
                class: e.class,
                message: e.detail,
            }
        }
    }
}

/// What a finished scan tells the scheduler (automatic mode, architecture 7.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    pub session: SessionId,
    /// The session is `Complete`: no scan error, no unreadable zone, every service `Ok`.
    pub complete: bool,
    /// Whole-root perimeter blockers (scan, mappings, permissions, APIs). Any entry stops
    /// automatic mode.
    pub uncertainties: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobSuccess {
    pub metrics: BTreeMap<String, u64>,
    pub scan: Option<ScanSummary>,
}

impl JobSuccess {
    #[must_use]
    pub fn with_metric(mut self, key: &str, value: u64) -> Self {
        self.metrics.insert(key.to_string(), value);
        self
    }
}

/// What a recovery pass resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// Interrupted plans resolved (any value above 0 pauses automatic mode).
    pub plans: u32,
    pub left_stopped: u32,
    pub notes: Vec<String>,
}

/// Everything a running job may use.
#[derive(Clone)]
pub struct JobCtx {
    pub id: JobId,
    /// For blocking filesystem work and the action runner.
    pub cancel: CancelFlag,
    /// For `select!` on awaitable work.
    pub token: CancellationToken,
    pub progress: ProgressHandle,
    pub limits: Arc<Limits>,
    pub locks: Arc<Locks>,
    pub clock: Arc<dyn Clock>,
    pub auto: Arc<AutoControl>,
}

impl std::fmt::Debug for JobCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCtx")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl JobCtx {
    /// A safe cancellation checkpoint: call between units of work, never inside a mutation.
    pub fn checkpoint(&self) -> Result<(), JobError> {
        if self.cancel.is_cancelled() || self.token.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
pub trait JobHandler: Send + Sync {
    /// Locks the runtime holds for the whole job. An `ExecutePlan` returns the exclusive scope
    /// of its plan; an `AutoCycle` returns an empty scope and locks per plan itself.
    async fn scope(&self, kind: &JobKind) -> Result<Scope, JobError>;

    /// Runs one job. It must poll [`JobCtx::checkpoint`] between units of work. It must not
    /// retry mutating calls.
    async fn run(&self, kind: &JobKind, ctx: &JobCtx) -> Result<JobSuccess, JobError>;

    /// Resolves interrupted plans (`actions::recover`). `Err` means unresolved: the runtime
    /// retries with backoff and starts no execute job meanwhile.
    async fn recover(&self, sink: EventSink) -> Result<RecoverySummary, PortError>;
}
