//! Error classes, the port error and domain validation errors.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fs::IoKind;
use crate::ids::GainId;
use crate::path::RootedPath;
use crate::time::Timestamp;

/// Stable error classes (architecture section 10.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Config,
    Validation,
    NotFound,
    Conflict,
    Unauthorized,
    Forbidden,
    RateLimited,
    StateChanged,
    Unavailable,
    Rejected,
    CapabilityDisabled,
    Io(IoKind),
    Confinement,
    Integrity,
    Storage,
    Cancelled,
    Internal,
}

impl ErrorClass {
    /// Only transient service errors are retried, and only for reads (section 8.4).
    #[must_use]
    pub fn is_retryable_read(&self) -> bool {
        matches!(self, ErrorClass::Unavailable)
    }
}

/// Error crossing a port boundary. `detail` is already redacted by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{class:?}: {detail}")]
pub struct PortError {
    pub class: ErrorClass,
    pub detail: String,
}

impl PortError {
    #[must_use]
    pub fn new(class: ErrorClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
        }
    }
}

/// Validation and transition errors raised by pure domain code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    #[error("invalid relative path: {0}")]
    InvalidRelPath(String),
    #[error("invalid root id: {0}")]
    InvalidRootId(String),
    #[error("invalid torrent hash: {0}")]
    InvalidTorrentHash(String),
    #[error("invalid issue category: {0}")]
    InvalidIssueCategory(String),
    #[error("invalid {entity} transition {from} -> {to}")]
    InvalidTransition {
        entity: &'static str,
        from: String,
        to: String,
    },
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    #[error("plan hash mismatch: expected {expected}, got {actual}")]
    PlanHashMismatch { expected: String, actual: String },
    #[error("plan expired at {expires_at:?} (now {now:?})")]
    PlanExpired {
        expires_at: Timestamp,
        now: Timestamp,
    },
    #[error("approval {field} mismatch: expected {expected}, got {actual}")]
    ApprovalMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("{} sample(s) not acknowledged", .0.len())]
    SamplesNotAcknowledged(Vec<RootedPath>),
    #[error("{} unrecoverable old file(s) not acknowledged", .0.len())]
    UnrecoverableNotAcknowledged(Vec<RootedPath>),
    #[error("action {action_index} has unknown or incomplete evidence")]
    IncompleteEvidence { action_index: u32 },
    #[error("automatic approval forbidden: {0}")]
    AutoApprovalForbidden(String),
    #[error("duplicate gain id {0:?}")]
    DuplicateGain(GainId),
    #[error("serialization failed: {0}")]
    Serialization(String),
}

impl DomainError {
    /// Maps the error to its architecture error class.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            DomainError::PlanHashMismatch { .. }
            | DomainError::PlanExpired { .. }
            | DomainError::DuplicateGain(_)
            | DomainError::InvalidTransition { .. } => ErrorClass::Conflict,
            DomainError::Serialization(_) => ErrorClass::Internal,
            _ => ErrorClass::Validation,
        }
    }
}
