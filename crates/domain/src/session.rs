//! Audit sessions and service status.

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::ids::SessionId;
use crate::path::RootId;
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Qbittorrent,
    Sonarr,
    Radarr,
    Jellyfin,
}

impl ServiceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceKind::Qbittorrent => "qbittorrent",
            ServiceKind::Sonarr => "sonarr",
            ServiceKind::Radarr => "radarr",
            ServiceKind::Jellyfin => "jellyfin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceState {
    Ok { version: String },
    Unavailable { reason: String },
    AuthRejected,
    NotConfigured,
    MappingErrors { count: u32 },
}

impl ServiceState {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, ServiceState::Ok { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service: ServiceKind,
    pub state: ServiceState,
    pub observed_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Complete,
    CompleteWithErrors,
    Cancelled,
    Interrupted,
    Failed,
}

impl SessionStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        self != SessionStatus::Running
    }

    /// Plans are never built from `Cancelled`, `Interrupted`, `Failed` or running sessions (OQ-05).
    #[must_use]
    pub fn allows_planning(self) -> bool {
        matches!(
            self,
            SessionStatus::Complete | SessionStatus::CompleteWithErrors
        )
    }

    #[must_use]
    pub fn can_transition_to(self, next: SessionStatus) -> bool {
        self == SessionStatus::Running && next != SessionStatus::Running
    }

    pub fn transition(self, next: SessionStatus) -> Result<SessionStatus, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "session",
                from: format!("{self:?}"),
                to: format!("{next:?}"),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootStatus {
    pub root: RootId,
    pub scanned: bool,
    pub error_count: u32,
    pub protected_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub entries_scanned: u64,
    pub scan_errors: u64,
    pub bytes_sampled: u64,
    pub bytes_compared: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditSession {
    pub id: SessionId,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub status: SessionStatus,
    pub roots: Vec<RootStatus>,
    pub services: Vec<ServiceStatus>,
    pub metrics: SessionMetrics,
}

impl AuditSession {
    /// Finishes a running session. Fails when it is already terminal.
    pub fn finish(&mut self, status: SessionStatus, at: Timestamp) -> Result<(), DomainError> {
        self.status = self.status.transition(status)?;
        self.finished_at = Some(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_can_finish() {
        use SessionStatus::*;
        for to in [Complete, CompleteWithErrors, Cancelled, Interrupted, Failed] {
            assert!(Running.can_transition_to(to));
            for from in [Complete, CompleteWithErrors, Cancelled, Interrupted, Failed] {
                assert!(!from.can_transition_to(to));
            }
        }
        assert!(!Running.can_transition_to(Running));
    }

    #[test]
    fn planning_requires_a_finished_scan() {
        assert!(SessionStatus::Complete.allows_planning());
        assert!(SessionStatus::CompleteWithErrors.allows_planning());
        for s in [
            SessionStatus::Running,
            SessionStatus::Cancelled,
            SessionStatus::Interrupted,
            SessionStatus::Failed,
        ] {
            assert!(!s.allows_planning());
        }
    }

    #[test]
    fn finish_sets_time_once() {
        let mut s = AuditSession {
            id: SessionId::new(uuid::Uuid::from_u128(1)),
            started_at: Timestamp(1),
            finished_at: None,
            status: SessionStatus::Running,
            roots: vec![],
            services: vec![],
            metrics: SessionMetrics::default(),
        };
        s.finish(SessionStatus::Complete, Timestamp(5)).unwrap();
        assert_eq!(s.finished_at, Some(Timestamp(5)));
        assert!(s.finish(SessionStatus::Failed, Timestamp(6)).is_err());
        assert_eq!(s.status, SessionStatus::Complete);
    }
}
