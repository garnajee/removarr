//! Safe, consistent error responses (architecture 10.1).
//!
//! A response carries a status from the error class, a translated generic message, optional
//! details the web layer produced itself (field names, excluded subject keys, blocker codes)
//! and a correlation id. Raw error text from services, storage or the filesystem is logged
//! (sanitized) under that id and never rendered: no path, no URL, no secret.

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use removarr_domain::error::{DomainError, ErrorClass, PortError};
use removarr_jobs::JobError;
use removarr_workflows::plans::{PlanError, SelectionFault};

use crate::i18n::{Locale, Tr};
use crate::state::{ExcludedItem, UseCaseError};

/// Rendered error. Build it with the constructors; the fields are public for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebError {
    pub status: StatusCode,
    pub key: &'static str,
    pub details: Vec<String>,
    pub correlation: String,
    pub retry_after: Option<u64>,
}

fn correlation_id() -> String {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(1);
        b = N.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Status and default message key of an error class.
#[must_use]
pub fn class_status(class: &ErrorClass) -> (StatusCode, &'static str) {
    match class {
        ErrorClass::Validation => (StatusCode::UNPROCESSABLE_ENTITY, "error.validation"),
        ErrorClass::NotFound => (StatusCode::NOT_FOUND, "error.not_found"),
        ErrorClass::Conflict => (StatusCode::CONFLICT, "error.conflict"),
        ErrorClass::StateChanged => (StatusCode::CONFLICT, "error.state_changed"),
        ErrorClass::Cancelled => (StatusCode::CONFLICT, "error.cancelled"),
        ErrorClass::Unauthorized => (StatusCode::UNAUTHORIZED, "error.unauthorized"),
        ErrorClass::Forbidden => (StatusCode::FORBIDDEN, "error.forbidden"),
        ErrorClass::CapabilityDisabled => (StatusCode::FORBIDDEN, "error.capability_disabled"),
        ErrorClass::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "error.rate_limited"),
        ErrorClass::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "error.unavailable"),
        ErrorClass::Storage => (StatusCode::SERVICE_UNAVAILABLE, "error.storage"),
        ErrorClass::Rejected => (StatusCode::BAD_GATEWAY, "error.rejected"),
        ErrorClass::Config
        | ErrorClass::Io(_)
        | ErrorClass::Confinement
        | ErrorClass::Integrity
        | ErrorClass::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "error.internal"),
    }
}

impl WebError {
    /// An error of `class`; `key` overrides the default message. `log_detail` is logged only.
    #[must_use]
    pub fn from_class(class: &ErrorClass, key: Option<&'static str>, log_detail: &str) -> Self {
        let (status, default_key) = class_status(class);
        let e = Self {
            status,
            key: key.unwrap_or(default_key),
            details: Vec::new(),
            correlation: correlation_id(),
            retry_after: None,
        };
        let detail = removarr_reporting::redact::sanitize(log_detail);
        if status.is_server_error() || *class == ErrorClass::Confinement {
            tracing::warn!(correlation = %e.correlation, class = ?class, detail = %detail, "request failed");
        } else {
            tracing::info!(correlation = %e.correlation, class = ?class, detail = %detail, "request refused");
        }
        e
    }

    /// Bad input. `field` and `problem` are static strings written by the web layer.
    #[must_use]
    pub fn invalid(field: &'static str, problem: &'static str) -> Self {
        let mut e = Self::from_class(&ErrorClass::Validation, None, field);
        e.details.push(format!("{field}: {problem}"));
        e
    }

    #[must_use]
    pub fn not_found() -> Self {
        Self::from_class(&ErrorClass::NotFound, None, "not found")
    }

    #[must_use]
    pub fn status(status: StatusCode, key: &'static str) -> Self {
        Self {
            status,
            key,
            details: Vec::new(),
            correlation: correlation_id(),
            retry_after: None,
        }
    }

    #[must_use]
    pub fn internal(log_detail: &str) -> Self {
        Self::from_class(&ErrorClass::Internal, None, log_detail)
    }
}

impl From<UseCaseError> for WebError {
    fn from(u: UseCaseError) -> Self {
        let mut e = Self::from_class(&u.class, Some(u.key), &u.log_detail);
        e.details = u
            .excluded
            .iter()
            .take(100)
            .map(|x| format!("{}: {}", x.subject, x.blockers.join(", ")))
            .collect();
        e
    }
}

impl From<PortError> for WebError {
    fn from(p: PortError) -> Self {
        Self::from_class(&p.class, None, &p.detail)
    }
}

fn selection_key(f: SelectionFault) -> &'static str {
    match f {
        SelectionFault::UnknownOrResolved => "error.plan.selection_unknown",
        SelectionFault::Stale => "error.plan.selection_stale",
        SelectionFault::Ignored => "error.plan.selection_ignored",
        SelectionFault::WrongCategory => "error.plan.selection_wrong_category",
    }
}

fn domain_key(e: &DomainError) -> Option<&'static str> {
    Some(match e {
        DomainError::PlanHashMismatch { .. } => "error.plan.hash_mismatch",
        DomainError::PlanExpired { .. } => "error.plan.expired",
        DomainError::ApprovalMismatch { .. } => "error.plan.confirmation_mismatch",
        DomainError::SamplesNotAcknowledged(_) => "error.plan.samples_not_acknowledged",
        DomainError::UnrecoverableNotAcknowledged(_) => "error.plan.unrecoverable_not_acknowledged",
        DomainError::InvalidTransition { .. } => "error.plan.wrong_status",
        _ => return None,
    })
}

/// Safe mapping of a plan-engine error. Blocked items keep their subject key and blocker codes
/// only (blocker detail text is not rendered).
#[must_use]
pub fn plan_failure(e: &PlanError) -> UseCaseError {
    let class = e.class();
    let log = e.to_string();
    match e {
        PlanError::InvalidRequest(_) => UseCaseError::new(class, "error.plan.invalid_request", log),
        PlanError::RejectedSelection { reason, .. } => {
            UseCaseError::new(class, selection_key(*reason), log)
        }
        PlanError::SessionNotPlannable(_) | PlanError::NoSession => {
            UseCaseError::new(class, "error.plan.no_session", log)
        }
        PlanError::NothingPlannable { excluded } => UseCaseError {
            class,
            key: "error.plan.nothing_plannable",
            excluded: excluded
                .iter()
                .map(|(s, bs)| ExcludedItem {
                    subject: s.as_str().to_string(),
                    blockers: bs.iter().map(|b| format!("{:?}", b.code)).collect(),
                })
                .collect(),
            log_detail: log,
        },
        PlanError::AutoRefused(_) => UseCaseError::new(class, "error.plan.auto_refused", log),
        PlanError::Stale { .. } => UseCaseError::new(class, "error.plan.stale", log),
        PlanError::Domain(d) => match domain_key(d) {
            Some(k) => UseCaseError::new(class, k, log),
            None => UseCaseError::new(class.clone(), class_status(&class).1, log),
        },
        PlanError::Port(_) | PlanError::RuleViolation(_) => {
            UseCaseError::new(class.clone(), class_status(&class).1, log)
        }
    }
}

/// Safe mapping of a job-runner error.
#[must_use]
pub fn job_failure(e: &JobError) -> UseCaseError {
    match e {
        JobError::Failed { class, message } => {
            UseCaseError::new(class.clone(), class_status(class).1, message.clone())
        }
        JobError::Cancelled => UseCaseError::new(ErrorClass::Cancelled, "error.cancelled", ""),
    }
}

/// Safe mapping of a port error.
#[must_use]
pub fn port_failure(e: &PortError) -> UseCaseError {
    UseCaseError::new(e.class.clone(), class_status(&e.class).1, e.detail.clone())
}

#[derive(Debug, Template)]
#[template(path = "pages/error.html")]
struct ErrorPage<'a> {
    code: u16,
    message: &'a str,
    details: &'a [String],
    correlation: &'a str,
    reference: &'a str,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let tr = Tr::new(Locale::En);
        let page = ErrorPage {
            code: self.status.as_u16(),
            message: tr.t(self.key),
            details: &self.details,
            correlation: &self.correlation,
            reference: tr.t("error.reference"),
        };
        let body = page
            .render()
            .unwrap_or_else(|_| "<!doctype html><title>Error</title><p>Error</p>".to_string());
        let mut resp = (self.status, axum::response::Html(body)).into_response();
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use removarr_domain::ids::{IssueId, SubjectKey};
    use removarr_domain::issue::{Blocker, BlockerCode};

    #[test]
    fn every_class_maps_to_a_status_and_a_translated_key() {
        let tr = Tr::new(Locale::En);
        for class in [
            ErrorClass::Config,
            ErrorClass::Validation,
            ErrorClass::NotFound,
            ErrorClass::Conflict,
            ErrorClass::Unauthorized,
            ErrorClass::Forbidden,
            ErrorClass::RateLimited,
            ErrorClass::StateChanged,
            ErrorClass::Unavailable,
            ErrorClass::Rejected,
            ErrorClass::CapabilityDisabled,
            ErrorClass::Confinement,
            ErrorClass::Integrity,
            ErrorClass::Storage,
            ErrorClass::Cancelled,
            ErrorClass::Internal,
        ] {
            let (status, key) = class_status(&class);
            assert!(status.is_client_error() || status.is_server_error());
            assert_ne!(tr.t(key), key, "{key} is not translated");
        }
        assert_eq!(
            class_status(&ErrorClass::CapabilityDisabled).0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            class_status(&ErrorClass::Rejected).0,
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn plan_errors_keep_codes_and_drop_free_text() {
        let e = PlanError::NothingPlannable {
            excluded: vec![(
                SubjectKey("p:completed:Rel/a.mkv".into()),
                vec![Blocker::new(
                    BlockerCode::Sample,
                    "/srv/secret/host/path should never show",
                )],
            )],
        };
        let u = plan_failure(&e);
        assert_eq!(u.class, ErrorClass::Validation);
        assert_eq!(u.key, "error.plan.nothing_plannable");
        assert_eq!(u.excluded[0].blockers, vec!["Sample".to_string()]);
        let w = WebError::from(u);
        assert!(w.details.iter().all(|d| !d.contains("/srv")));
        let tr = Tr::new(Locale::En);
        for key in [
            plan_failure(&PlanError::RejectedSelection {
                reason: SelectionFault::Stale,
                ids: vec![IssueId("x".into())],
            })
            .key,
            plan_failure(&PlanError::NoSession).key,
            plan_failure(&PlanError::Domain(DomainError::PlanHashMismatch {
                expected: "a".into(),
                actual: "b".into(),
            }))
            .key,
        ] {
            assert_ne!(tr.t(key), key);
        }
    }
}
