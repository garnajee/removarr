//! Maps sqlx errors to the domain's `PortError`. No sqlx type leaves this crate.
//!
//! Error details name constraints and tables only. Bound values are never included, so a
//! failure cannot copy a path, a URL or a secret into a log line.

use removarr_domain::error::{DomainError, ErrorClass, PortError};
use sqlx::error::ErrorKind;

pub(crate) fn map_sqlx(e: sqlx::Error) -> PortError {
    match &e {
        sqlx::Error::Database(db) => {
            let class = match db.kind() {
                ErrorKind::UniqueViolation => ErrorClass::Conflict,
                ErrorKind::ForeignKeyViolation
                | ErrorKind::NotNullViolation
                | ErrorKind::CheckViolation => ErrorClass::Integrity,
                _ => classify_code(db.code().as_deref()),
            };
            PortError::new(class, db.message().to_string())
        }
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => {
            PortError::new(ErrorClass::Unavailable, "database pool unavailable")
        }
        sqlx::Error::RowNotFound => PortError::new(ErrorClass::NotFound, "row not found"),
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => {
            PortError::new(ErrorClass::Integrity, "stored value could not be decoded")
        }
        other => PortError::new(ErrorClass::Storage, other.to_string()),
    }
}

/// SQLite primary result codes: 5 = BUSY, 6 = LOCKED. Extended 1811 = trigger abort.
fn classify_code(code: Option<&str>) -> ErrorClass {
    match code {
        Some(c) if c == "5" || c == "6" || c.starts_with("517") || c.starts_with("261") => {
            ErrorClass::Unavailable
        }
        Some("1811") => ErrorClass::Integrity,
        _ => ErrorClass::Storage,
    }
}

pub(crate) fn domain(e: DomainError) -> PortError {
    PortError::new(e.class(), e.to_string())
}

pub(crate) fn integrity(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::Integrity, detail)
}

pub(crate) fn not_found(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::NotFound, detail)
}

pub(crate) fn conflict(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::Conflict, detail)
}

pub(crate) fn invalid(detail: impl Into<String>) -> PortError {
    PortError::new(ErrorClass::Validation, detail)
}
