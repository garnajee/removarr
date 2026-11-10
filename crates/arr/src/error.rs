//! Connector errors. Messages carry the service, the operation (`GET /api/v3/series`, never a
//! query string, host or header) and a reason. Upstream bodies are never copied into errors.

use removarr_domain::error::{ErrorClass, PortError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArrError {
    #[error("{op}: service unavailable ({reason})")]
    Unavailable { op: String, reason: &'static str },
    #[error("{op}: authentication rejected (HTTP {status})")]
    Auth { op: String, status: u16 },
    #[error("{op}: not found (HTTP 404)")]
    NotFound { op: String },
    #[error("{op}: rejected (HTTP {status})")]
    Http { op: String, status: u16 },
    #[error("{op}: redirect refused (HTTP {status})")]
    Redirect { op: String, status: u16 },
    #[error("{op}: response exceeds {limit} bytes")]
    TooLarge { op: String, limit: u64 },
    #[error("{op}: unexpected response shape")]
    Decode { op: String },
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    StateChanged(String),
    #[error("capability `{0}` is disabled")]
    CapabilityDisabled(&'static str),
    #[error("{0}")]
    Config(String),
}

impl ArrError {
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            ArrError::Unavailable { .. } => ErrorClass::Unavailable,
            ArrError::Auth { status: 403, .. } => ErrorClass::Forbidden,
            ArrError::Auth { .. } => ErrorClass::Unauthorized,
            ArrError::NotFound { .. } => ErrorClass::NotFound,
            ArrError::Http { .. }
            | ArrError::Redirect { .. }
            | ArrError::TooLarge { .. }
            | ArrError::Decode { .. } => ErrorClass::Rejected,
            ArrError::Validation(_) => ErrorClass::Validation,
            ArrError::StateChanged(_) => ErrorClass::StateChanged,
            ArrError::CapabilityDisabled(_) => ErrorClass::CapabilityDisabled,
            ArrError::Config(_) => ErrorClass::Config,
        }
    }
}

impl From<ArrError> for PortError {
    fn from(e: ArrError) -> Self {
        // Messages are built from static text, operation labels and ids only. The scrub is a
        // second line of defense.
        PortError::new(e.class(), removarr_config::redact_text(&e.to_string()))
    }
}
