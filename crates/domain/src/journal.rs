//! Action journal, verification outcomes and the per-action state machine.
//!
//! An action is successful only when verification completes and every step passes. Anything
//! else is "executed, not verified" (INV-17).

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorClass};
use crate::gain::GainEstimate;
use crate::ids::{ActionRef, IntentId};
use crate::issue::Blocker;
use crate::plan::VerificationStep;
use crate::time::Timestamp;
use crate::torrent::TorrentHash;

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStep {
    FsProbeHardlink,
    FsReplaceWithHardlink,
    FsRemoveFile,
    FsRemoveTree,
    FsRemoveEmptyDir,
    FsSetMode,
    QbitStop,
    QbitStart,
    QbitRecheck,
    QbitRemoveKeepData,
    ArrManualImport,
    ArrRemoveQueueItem,
    ArrSetMonitored,
    ArrGrab,
    ArrRescan,
    JellyfinRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalMode {
    Live,
    Simulated,
}

/// Written (and durable) before the mutation it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    pub id: IntentId,
    pub action: ActionRef,
    pub step: JournalStep,
    pub mode: JournalMode,
    pub target: serde_json::Value,
    pub observed: serde_json::Value,
    pub expected_gain: GainEstimate,
    pub hashes: BTreeSet<TorrentHash>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepOutcome {
    Done { final_state: serde_json::Value },
    Failed { class: ErrorClass, message: String },
    Simulated,
    Skipped { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRecord {
    pub intent_id: IntentId,
    pub outcome: StepOutcome,
    pub at: Timestamp,
}

/// An intent with no result: the process stopped between intent and result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnfinishedIntent {
    pub intent: IntentRecord,
}

/// Final outcome of one action. Only `Verified` counts as success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionOutcome {
    Verified,
    ExecutedUnverified { reason: String },
    Failed { class: ErrorClass },
    Compensated,
    FailedNeedsReview,
    Simulated,
    NotExecuted { blockers: Vec<Blocker> },
}

impl ActionOutcome {
    /// The single success predicate.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, ActionOutcome::Verified)
    }

    /// True when a mutation may have happened and the result is not proven.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            ActionOutcome::ExecutedUnverified { .. }
                | ActionOutcome::FailedNeedsReview
                | ActionOutcome::Failed { .. }
        )
    }

    #[must_use]
    pub fn state(&self) -> ActionState {
        match self {
            ActionOutcome::Verified => ActionState::Verified,
            ActionOutcome::ExecutedUnverified { .. } => ActionState::ExecutedUnverified,
            ActionOutcome::Failed { .. } => ActionState::Failed,
            ActionOutcome::Compensated => ActionState::Compensated,
            ActionOutcome::FailedNeedsReview => ActionState::FailedNeedsReview,
            ActionOutcome::Simulated => ActionState::Simulated,
            ActionOutcome::NotExecuted { .. } => ActionState::NotExecuted,
        }
    }
}

/// Result of one verification step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationStatus {
    Passed,
    Failed {
        reason: String,
    },
    /// The recording (dry-run) path: nothing real was observed.
    Simulated,
    /// The step could not be evaluated (service down, scan error).
    NotRun {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub step: VerificationStep,
    pub status: VerificationStatus,
    pub at: Timestamp,
}

/// What a re-read of a service showed, for the service-command verification steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceObservation {
    QueueItemPresent(bool),
    Monitored(bool),
    /// The command or task was observed completed.
    CommandCompleted,
    /// The command or task was observed failed.
    CommandFailed {
        reason: String,
    },
    /// Nothing could be observed (service down, command still running, no status).
    Unobserved {
        reason: String,
    },
}

/// Judges a service-command step against a re-read. Only a positive observation passes: an
/// unobserved completion is `NotRun`, so the action stays `ExecutedUnverified`. A step that is
/// not a service step, or an observation of another kind, is `NotRun` too.
#[must_use]
pub fn evaluate_service_step(
    step: &VerificationStep,
    observed: &ServiceObservation,
) -> VerificationStatus {
    use ServiceObservation as O;
    use VerificationStep as S;
    let failed = |reason: &str| VerificationStatus::Failed {
        reason: reason.to_string(),
    };
    match (step, observed) {
        (_, O::Unobserved { reason }) => VerificationStatus::NotRun {
            reason: reason.clone(),
        },
        (S::ArrQueueItemAbsent { .. }, O::QueueItemPresent(false)) => VerificationStatus::Passed,
        (S::ArrQueueItemAbsent { .. }, O::QueueItemPresent(true)) => {
            failed("the queue item is still present")
        }
        (S::ArrMonitored { monitored, .. }, O::Monitored(now)) if now == monitored => {
            VerificationStatus::Passed
        }
        (S::ArrMonitored { .. }, O::Monitored(_)) => {
            failed("the monitored flag differs from the requested one")
        }
        (S::ArrCommandCompleted { .. } | S::JellyfinRefreshed { .. }, O::CommandCompleted) => {
            VerificationStatus::Passed
        }
        (
            S::ArrCommandCompleted { .. } | S::JellyfinRefreshed { .. },
            O::CommandFailed { reason },
        ) => VerificationStatus::Failed {
            reason: reason.clone(),
        },
        _ => VerificationStatus::NotRun {
            reason: "the observation does not match the verification step".to_string(),
        },
    }
}

/// Decides the outcome of an executed action from its verification results.
///
/// `Verified` requires at least one step and every step `Passed`. An empty list, a failed step
/// or a step that did not run yields `ExecutedUnverified` with the reason. Simulated results
/// yield `Simulated` and can never yield `Verified`.
#[must_use]
pub fn outcome_from_verification(results: &[VerificationResult]) -> ActionOutcome {
    if results.is_empty() {
        return ActionOutcome::ExecutedUnverified {
            reason: "no verification step ran".to_string(),
        };
    }
    if results
        .iter()
        .any(|r| r.status == VerificationStatus::Simulated)
    {
        return ActionOutcome::Simulated;
    }
    for r in results {
        match &r.status {
            VerificationStatus::Failed { reason } | VerificationStatus::NotRun { reason } => {
                return ActionOutcome::ExecutedUnverified {
                    reason: reason.clone(),
                };
            }
            VerificationStatus::Passed | VerificationStatus::Simulated => {}
        }
    }
    ActionOutcome::Verified
}

/// Per-action state machine (explicit table).
///
/// ```text
/// Pending -> Running -> Verifying -> Verified
///    |          |           |-> ExecutedUnverified -> Verified | FailedNeedsReview
///    |          |           |-> Compensating -> Compensated | FailedNeedsReview
///    |          |-> Compensating | Failed | FailedNeedsReview
///    |-> NotExecuted | Simulated
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Pending,
    Running,
    Verifying,
    Verified,
    ExecutedUnverified,
    Compensating,
    Compensated,
    Failed,
    FailedNeedsReview,
    NotExecuted,
    Simulated,
}

impl ActionState {
    #[must_use]
    pub fn can_transition_to(self, next: ActionState) -> bool {
        use ActionState::*;
        matches!(
            (self, next),
            (Pending, Running | NotExecuted | Simulated)
                | (
                    Running,
                    Verifying | Compensating | Failed | FailedNeedsReview
                )
                | (
                    Verifying,
                    Verified | ExecutedUnverified | Compensating | FailedNeedsReview
                )
                | (ExecutedUnverified, Verified | FailedNeedsReview)
                | (Compensating, Compensated | FailedNeedsReview)
        )
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        use ActionState::*;
        matches!(
            self,
            Verified | Compensated | Failed | FailedNeedsReview | NotExecuted | Simulated
        )
    }

    pub fn transition(self, next: ActionState) -> Result<ActionState, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "action",
                from: format!("{self:?}"),
                to: format!("{next:?}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::p;

    fn vr(status: VerificationStatus) -> VerificationResult {
        VerificationResult {
            step: VerificationStep::Absent {
                path: p("completed", "a"),
            },
            status,
            at: Timestamp(0),
        }
    }

    #[test]
    fn mutation_is_not_success_until_verification_passes() {
        assert!(matches!(
            outcome_from_verification(&[]),
            ActionOutcome::ExecutedUnverified { .. }
        ));
        assert_eq!(
            outcome_from_verification(&[vr(VerificationStatus::Passed)]),
            ActionOutcome::Verified
        );
        let mixed = [
            vr(VerificationStatus::Passed),
            vr(VerificationStatus::Failed {
                reason: "inode differs".into(),
            }),
        ];
        assert_eq!(
            outcome_from_verification(&mixed),
            ActionOutcome::ExecutedUnverified {
                reason: "inode differs".into()
            }
        );
        let not_run = [
            vr(VerificationStatus::Passed),
            vr(VerificationStatus::NotRun {
                reason: "qbit down".into(),
            }),
        ];
        assert!(!outcome_from_verification(&not_run).is_success());
        let sim = [
            vr(VerificationStatus::Passed),
            vr(VerificationStatus::Simulated),
        ];
        assert_eq!(outcome_from_verification(&sim), ActionOutcome::Simulated);
        assert!(!ActionOutcome::Simulated.is_success());
    }

    #[test]
    fn only_verified_is_success() {
        let all = [
            ActionOutcome::ExecutedUnverified {
                reason: String::new(),
            },
            ActionOutcome::Failed {
                class: ErrorClass::Internal,
            },
            ActionOutcome::Compensated,
            ActionOutcome::FailedNeedsReview,
            ActionOutcome::Simulated,
            ActionOutcome::NotExecuted { blockers: vec![] },
        ];
        assert!(all.iter().all(|o| !o.is_success()));
        assert!(ActionOutcome::Verified.is_success());
        assert_eq!(ActionOutcome::Verified.state(), ActionState::Verified);
    }

    #[test]
    fn action_state_table_is_exact() {
        use ActionState::*;
        let all = [
            Pending,
            Running,
            Verifying,
            Verified,
            ExecutedUnverified,
            Compensating,
            Compensated,
            Failed,
            FailedNeedsReview,
            NotExecuted,
            Simulated,
        ];
        let allowed = [
            (Pending, Running),
            (Pending, NotExecuted),
            (Pending, Simulated),
            (Running, Verifying),
            (Running, Compensating),
            (Running, Failed),
            (Running, FailedNeedsReview),
            (Verifying, Verified),
            (Verifying, ExecutedUnverified),
            (Verifying, Compensating),
            (Verifying, FailedNeedsReview),
            (ExecutedUnverified, Verified),
            (ExecutedUnverified, FailedNeedsReview),
            (Compensating, Compensated),
            (Compensating, FailedNeedsReview),
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
    }

    #[test]
    fn verified_is_unreachable_from_running_or_pending() {
        assert!(
            ActionState::Pending
                .transition(ActionState::Verified)
                .is_err()
        );
        assert!(
            ActionState::Running
                .transition(ActionState::Verified)
                .is_err()
        );
        assert!(
            ActionState::Running
                .transition(ActionState::Verifying)
                .is_ok()
        );
        for s in [
            ActionState::Verified,
            ActionState::Compensated,
            ActionState::Failed,
            ActionState::Simulated,
        ] {
            assert!(s.is_terminal());
            assert!(!s.can_transition_to(ActionState::Running));
        }
    }

    fn service_steps() -> Vec<(VerificationStep, ServiceObservation, VerificationStatus)> {
        use crate::arr::{ArrMediaRef, ArrService};
        let media = ArrMediaRef {
            service: ArrService::Radarr,
            media_id: 1,
            episode_ids: vec![],
        };
        let q = VerificationStep::ArrQueueItemAbsent {
            service: ArrService::Radarr,
            id: 7,
        };
        let m = VerificationStep::ArrMonitored {
            media: media.clone(),
            monitored: false,
        };
        let r = VerificationStep::ArrCommandCompleted {
            service: ArrService::Radarr,
            media: Some(media),
        };
        let j = VerificationStep::JellyfinRefreshed { item_id: None };
        let failed = |s: &str| VerificationStatus::Failed { reason: s.into() };
        vec![
            (
                q.clone(),
                ServiceObservation::QueueItemPresent(false),
                VerificationStatus::Passed,
            ),
            (
                q,
                ServiceObservation::QueueItemPresent(true),
                failed("the queue item is still present"),
            ),
            (
                m.clone(),
                ServiceObservation::Monitored(false),
                VerificationStatus::Passed,
            ),
            (
                m,
                ServiceObservation::Monitored(true),
                failed("the monitored flag differs from the requested one"),
            ),
            (
                r.clone(),
                ServiceObservation::CommandCompleted,
                VerificationStatus::Passed,
            ),
            (
                r,
                ServiceObservation::CommandFailed {
                    reason: "boom".into(),
                },
                failed("boom"),
            ),
            (
                j,
                ServiceObservation::CommandCompleted,
                VerificationStatus::Passed,
            ),
        ]
    }

    #[test]
    fn service_steps_pass_only_on_a_positive_reread() {
        for (step, obs, expected) in service_steps() {
            let status = evaluate_service_step(&step, &obs);
            assert_eq!(status, expected, "{step:?} {obs:?}");
            let outcome = outcome_from_verification(&[VerificationResult {
                step,
                status,
                at: Timestamp(0),
            }]);
            assert_eq!(
                outcome == ActionOutcome::Verified,
                expected == VerificationStatus::Passed
            );
        }
    }

    #[test]
    fn unobserved_or_mismatched_service_observation_stays_unverified() {
        for (step, _, _) in service_steps() {
            for obs in [
                ServiceObservation::Unobserved {
                    reason: "running".into(),
                },
                ServiceObservation::Monitored(true),
                ServiceObservation::QueueItemPresent(false),
            ] {
                let status = evaluate_service_step(&step, &obs);
                let passes = status == VerificationStatus::Passed;
                let matches_kind = matches!(
                    (&step, &obs),
                    (
                        VerificationStep::ArrQueueItemAbsent { .. },
                        ServiceObservation::QueueItemPresent(false)
                    ) | (
                        VerificationStep::ArrMonitored {
                            monitored: true,
                            ..
                        },
                        ServiceObservation::Monitored(true)
                    )
                );
                assert_eq!(passes, matches_kind, "{step:?} {obs:?}");
                if !passes {
                    assert!(matches!(
                        outcome_from_verification(&[VerificationResult {
                            step: step.clone(),
                            status,
                            at: Timestamp(0)
                        }]),
                        ActionOutcome::ExecutedUnverified { .. }
                    ));
                }
            }
        }
    }
}
