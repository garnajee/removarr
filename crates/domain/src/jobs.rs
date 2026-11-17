//! Persisted job model (architecture section 8.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::arr::ArrMediaRef;
use crate::canonical::{canonical_json, sha256_hex};
use crate::error::{DomainError, ErrorClass};
use crate::ids::{IssueId, JobId, PlanId, SessionId};
use crate::session::ServiceKind;
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Observe,
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKind {
    Recover,
    Scan,
    RefreshService(ServiceKind),
    EvaluateIssues {
        issue_ids: Vec<IssueId>,
    },
    EvaluateMedia {
        media: ArrMediaRef,
        search: bool,
    },
    VerifyContent {
        issue_ids: Vec<IssueId>,
    },
    /// `simulate = true` always uses the recording mutators and never changes the plan status.
    ExecutePlan {
        plan_id: PlanId,
        simulate: bool,
    },
    AutoCycle {
        session: SessionId,
    },
}

impl JobKind {
    #[must_use]
    pub fn lane(&self) -> Lane {
        match self {
            JobKind::Recover | JobKind::ExecutePlan { .. } | JobKind::AutoCycle { .. } => {
                Lane::Execute
            }
            _ => Lane::Observe,
        }
    }

    /// Dedupe key: unique while a job with the same key is queued or running.
    pub fn dedupe_key(&self) -> Result<String, DomainError> {
        Ok(match self {
            JobKind::Recover => "recover".to_string(),
            JobKind::Scan => "scan".to_string(),
            JobKind::RefreshService(s) => format!("refresh:{}", s.as_str()),
            JobKind::EvaluateIssues { issue_ids } => format!("evaluate:{}", ids_hash(issue_ids)?),
            JobKind::EvaluateMedia { media, search } => {
                let h = sha256_hex(&canonical_json(&(media, search))?);
                format!("evaluate:{}", &h[..16])
            }
            JobKind::VerifyContent { issue_ids } => format!("verify:{}", ids_hash(issue_ids)?),
            JobKind::ExecutePlan { plan_id, simulate } => format!("exec:{plan_id}:{simulate}"),
            JobKind::AutoCycle { session } => format!("auto:{session}"),
        })
    }
}

/// Hash of the sorted, deduplicated ids so the key does not depend on selection order.
fn ids_hash(ids: &[IssueId]) -> Result<String, DomainError> {
    let mut sorted: Vec<&IssueId> = ids.iter().collect();
    sorted.sort();
    sorted.dedup();
    Ok(sha256_hex(&canonical_json(&sorted)?)[..16].to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    #[must_use]
    pub fn can_transition_to(self, next: JobStatus) -> bool {
        use JobStatus::*;
        matches!(
            (self, next),
            (Queued, Running | Cancelled) | (Running, Succeeded | Failed | Cancelled | Interrupted)
        )
    }

    /// A job is active (holds its dedupe key) while queued or running.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, JobStatus::Queued | JobStatus::Running)
    }

    pub fn transition(self, next: JobStatus) -> Result<JobStatus, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "job",
                from: format!("{self:?}"),
                to: format!("{next:?}"),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobOutcome {
    Succeeded { metrics: BTreeMap<String, u64> },
    Failed { class: ErrorClass, message: String },
    Cancelled,
    Interrupted,
}

impl JobOutcome {
    #[must_use]
    pub fn status(&self) -> JobStatus {
        match self {
            JobOutcome::Succeeded { .. } => JobStatus::Succeeded,
            JobOutcome::Failed { .. } => JobStatus::Failed,
            JobOutcome::Cancelled => JobStatus::Cancelled,
            JobOutcome::Interrupted => JobStatus::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub kind: JobKind,
    pub dedupe_key: String,
    pub lane: Lane,
    pub status: JobStatus,
    pub enqueued_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub stage: Option<String>,
    pub attempts: u32,
    pub outcome: Option<JobOutcome>,
}

impl JobRecord {
    /// A new queued job; lane and dedupe key are derived from the kind.
    pub fn queued(id: JobId, kind: JobKind, at: Timestamp) -> Result<Self, DomainError> {
        Ok(Self {
            id,
            dedupe_key: kind.dedupe_key()?,
            lane: kind.lane(),
            kind,
            status: JobStatus::Queued,
            enqueued_at: at,
            started_at: None,
            finished_at: None,
            stage: None,
            attempts: 0,
            outcome: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn lanes() {
        let plan_id = PlanId::new(Uuid::from_u128(1));
        assert_eq!(JobKind::Scan.lane(), Lane::Observe);
        assert_eq!(
            JobKind::RefreshService(ServiceKind::Qbittorrent).lane(),
            Lane::Observe
        );
        assert_eq!(
            JobKind::ExecutePlan {
                plan_id,
                simulate: true
            }
            .lane(),
            Lane::Execute
        );
        assert_eq!(JobKind::Recover.lane(), Lane::Execute);
        assert_eq!(
            JobKind::AutoCycle {
                session: SessionId::new(Uuid::from_u128(2))
            }
            .lane(),
            Lane::Execute
        );
    }

    #[test]
    fn dedupe_keys() {
        let plan_id = PlanId::new(Uuid::from_u128(1));
        assert_eq!(JobKind::Scan.dedupe_key().unwrap(), "scan");
        assert_eq!(
            JobKind::RefreshService(ServiceKind::Sonarr)
                .dedupe_key()
                .unwrap(),
            "refresh:sonarr"
        );
        let a = JobKind::ExecutePlan {
            plan_id,
            simulate: true,
        }
        .dedupe_key()
        .unwrap();
        let b = JobKind::ExecutePlan {
            plan_id,
            simulate: false,
        }
        .dedupe_key()
        .unwrap();
        assert_ne!(a, b);
        let ids = |v: &[&str]| JobKind::VerifyContent {
            issue_ids: v.iter().map(|s| IssueId((*s).into())).collect(),
        };
        assert_eq!(
            ids(&["b", "a", "a"]).dedupe_key().unwrap(),
            ids(&["a", "b"]).dedupe_key().unwrap()
        );
        assert_ne!(
            ids(&["a"]).dedupe_key().unwrap(),
            ids(&["b"]).dedupe_key().unwrap()
        );
        let ev = JobKind::EvaluateIssues {
            issue_ids: vec![IssueId("a".into())],
        };
        assert_ne!(ev.dedupe_key().unwrap(), ids(&["a"]).dedupe_key().unwrap());
    }

    #[test]
    fn status_machine() {
        use JobStatus::*;
        let all = [Queued, Running, Succeeded, Failed, Cancelled, Interrupted];
        let allowed = [
            (Queued, Running),
            (Queued, Cancelled),
            (Running, Succeeded),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Interrupted),
        ];
        for a in all {
            for b in all {
                assert_eq!(
                    a.can_transition_to(b),
                    allowed.contains(&(a, b)),
                    "{a:?} -> {b:?}"
                );
            }
        }
        assert!(Queued.is_active() && Running.is_active() && !Failed.is_active());
        assert_eq!(JobOutcome::Cancelled.status(), Cancelled);
    }

    #[test]
    fn queued_record_derives_lane_and_key() {
        let r =
            JobRecord::queued(JobId::new(Uuid::from_u128(9)), JobKind::Scan, Timestamp(5)).unwrap();
        assert_eq!(
            (r.lane, r.dedupe_key.as_str(), r.status),
            (Lane::Observe, "scan", JobStatus::Queued)
        );
    }
}
