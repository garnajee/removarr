//! Candidate lifecycle (CONTEXTE section 6.4, CTX-12).
//!
//! The state is **derived**, not stored: it is a view over the current issue, the plan status and
//! the action outcome. The explicit transition table documents (and tests) which evolutions are
//! legal; the enforced, stored machine is [`PlanStatus`].

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::issue::{BlockerCode, Issue, Outcome};
use crate::journal::ActionOutcome;
use crate::plan::PlanStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Observed,
    Identified,
    AmbiguousManual,
    Candidate,
    Rejected,
    Planned,
    PlanExpired,
    Approved,
    Executing,
    FailedCompensated,
    FailedNeedsReview,
    Executed,
    Verified,
    Closed,
}

impl CandidateState {
    /// Legal evolutions. A rescan may re-evaluate an item, so early states can fall back to
    /// `Observed`; `Closed` is terminal and `Verified` is reachable only from `Executed`.
    #[must_use]
    pub fn can_transition_to(self, next: CandidateState) -> bool {
        use CandidateState::*;
        matches!(
            (self, next),
            (Observed, Identified | AmbiguousManual | Rejected)
                | (
                    Identified,
                    Candidate | AmbiguousManual | Rejected | Observed
                )
                | (AmbiguousManual, Identified | Rejected | Observed)
                | (Candidate, Planned | Rejected | Observed)
                | (Rejected, Observed)
                | (Planned, Approved | PlanExpired | Candidate | Rejected)
                | (PlanExpired, Planned | Candidate | Rejected)
                | (Approved, Executing | PlanExpired)
                | (Executing, FailedCompensated | FailedNeedsReview | Executed)
                | (Executed, Verified | FailedNeedsReview)
                | (FailedCompensated, Candidate | Closed)
                | (FailedNeedsReview, Candidate | Closed)
                | (Verified, Closed)
        )
    }

    pub fn transition(self, next: CandidateState) -> Result<CandidateState, DomainError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "candidate",
                from: format!("{self:?}"),
                to: format!("{next:?}"),
            })
        }
    }
}

/// Derives the display state. `ActionOutcome::Verified` is the only path to `Verified`.
///
/// `outcome` is the outcome of the latest action of the plan for this candidate; with
/// `PlanStatus::Executed`/`PartiallyExecuted` and no outcome the state is `Executed` (never
/// `Verified`). A verified candidate whose issue has disappeared is `Closed`.
#[must_use]
pub fn derive_candidate_state(
    issue: Option<&Issue>,
    plan: Option<PlanStatus>,
    outcome: Option<&ActionOutcome>,
) -> CandidateState {
    match plan {
        Some(PlanStatus::Draft) => return CandidateState::Planned,
        Some(PlanStatus::Approved) => return CandidateState::Approved,
        Some(PlanStatus::Executing) => return CandidateState::Executing,
        Some(PlanStatus::Expired | PlanStatus::Invalidated) => return CandidateState::PlanExpired,
        Some(PlanStatus::Interrupted | PlanStatus::Failed) => {
            return match outcome {
                Some(ActionOutcome::Compensated) => CandidateState::FailedCompensated,
                _ => CandidateState::FailedNeedsReview,
            };
        }
        Some(PlanStatus::Executed | PlanStatus::PartiallyExecuted) => {
            return match outcome {
                Some(ActionOutcome::Verified) if issue.is_none() => CandidateState::Closed,
                Some(ActionOutcome::Verified) => CandidateState::Verified,
                Some(ActionOutcome::Compensated) => CandidateState::FailedCompensated,
                Some(ActionOutcome::Failed { .. } | ActionOutcome::FailedNeedsReview) => {
                    CandidateState::FailedNeedsReview
                }
                Some(ActionOutcome::NotExecuted { .. }) => CandidateState::PlanExpired,
                Some(ActionOutcome::ExecutedUnverified { .. } | ActionOutcome::Simulated)
                | None => CandidateState::Executed,
            };
        }
        Some(PlanStatus::Discarded) | None => {}
    }
    let Some(issue) = issue else {
        return CandidateState::Observed;
    };
    let ambiguous = issue.outcome == Outcome::KeepManual
        || issue
            .blockers
            .iter()
            .any(|b| b.code == BlockerCode::ArrAmbiguous);
    if ambiguous {
        CandidateState::AmbiguousManual
    } else if issue.outcome == Outcome::KeepSample || !issue.is_unblocked() {
        CandidateState::Rejected
    } else if issue.completeness().is_complete() {
        CandidateState::Candidate
    } else {
        CandidateState::Identified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorClass;
    use crate::evidence::Completeness;
    use crate::issue::Blocker;
    use crate::testutil::issue;

    #[test]
    fn transition_table_is_exact() {
        use CandidateState::*;
        let all = [
            Observed,
            Identified,
            AmbiguousManual,
            Candidate,
            Rejected,
            Planned,
            PlanExpired,
            Approved,
            Executing,
            FailedCompensated,
            FailedNeedsReview,
            Executed,
            Verified,
            Closed,
        ];
        let allowed: &[(CandidateState, CandidateState)] = &[
            (Observed, Identified),
            (Observed, AmbiguousManual),
            (Observed, Rejected),
            (Identified, Candidate),
            (Identified, AmbiguousManual),
            (Identified, Rejected),
            (Identified, Observed),
            (AmbiguousManual, Identified),
            (AmbiguousManual, Rejected),
            (AmbiguousManual, Observed),
            (Candidate, Planned),
            (Candidate, Rejected),
            (Candidate, Observed),
            (Rejected, Observed),
            (Planned, Approved),
            (Planned, PlanExpired),
            (Planned, Candidate),
            (Planned, Rejected),
            (PlanExpired, Planned),
            (PlanExpired, Candidate),
            (PlanExpired, Rejected),
            (Approved, Executing),
            (Approved, PlanExpired),
            (Executing, FailedCompensated),
            (Executing, FailedNeedsReview),
            (Executing, Executed),
            (Executed, Verified),
            (Executed, FailedNeedsReview),
            (FailedCompensated, Candidate),
            (FailedCompensated, Closed),
            (FailedNeedsReview, Candidate),
            (FailedNeedsReview, Closed),
            (Verified, Closed),
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
    fn execution_cannot_skip_approval_or_verification() {
        use CandidateState::*;
        assert!(Candidate.transition(Executing).is_err());
        assert!(Planned.transition(Executing).is_err());
        assert!(Executing.transition(Verified).is_err());
        assert!(Executed.transition(Closed).is_err());
        assert!(Closed.transition(Observed).is_err());
    }

    #[test]
    fn derive_from_plan_and_outcome() {
        let i = issue();
        let d = |p, o: Option<ActionOutcome>| derive_candidate_state(Some(&i), p, o.as_ref());
        assert_eq!(d(Some(PlanStatus::Draft), None), CandidateState::Planned);
        assert_eq!(
            d(Some(PlanStatus::Approved), None),
            CandidateState::Approved
        );
        assert_eq!(
            d(Some(PlanStatus::Executing), None),
            CandidateState::Executing
        );
        assert_eq!(
            d(Some(PlanStatus::Expired), None),
            CandidateState::PlanExpired
        );
        assert_eq!(
            d(Some(PlanStatus::Invalidated), None),
            CandidateState::PlanExpired
        );
        assert_eq!(
            d(Some(PlanStatus::Interrupted), None),
            CandidateState::FailedNeedsReview
        );
        assert_eq!(
            d(Some(PlanStatus::Failed), Some(ActionOutcome::Compensated)),
            CandidateState::FailedCompensated
        );
        assert_eq!(
            d(
                Some(PlanStatus::Failed),
                Some(ActionOutcome::Failed {
                    class: ErrorClass::Internal
                })
            ),
            CandidateState::FailedNeedsReview
        );
    }

    #[test]
    fn verified_only_from_a_verified_outcome() {
        let i = issue();
        let e = |o: Option<ActionOutcome>| {
            derive_candidate_state(Some(&i), Some(PlanStatus::Executed), o.as_ref())
        };
        assert_eq!(e(Some(ActionOutcome::Verified)), CandidateState::Verified);
        assert_eq!(e(None), CandidateState::Executed);
        assert_eq!(
            e(Some(ActionOutcome::ExecutedUnverified {
                reason: "x".into()
            })),
            CandidateState::Executed
        );
        assert_eq!(e(Some(ActionOutcome::Simulated)), CandidateState::Executed);
        assert_eq!(
            derive_candidate_state(
                None,
                Some(PlanStatus::Executed),
                Some(&ActionOutcome::Verified)
            ),
            CandidateState::Closed
        );
    }

    #[test]
    fn derive_from_issue_without_plan() {
        let mut i = issue();
        assert_eq!(
            derive_candidate_state(None, None, None),
            CandidateState::Observed
        );
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::Candidate
        );
        i.blockers
            .push(Blocker::new(BlockerCode::Recent, "too new"));
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::Rejected
        );
        i.blockers = vec![Blocker::new(BlockerCode::ArrAmbiguous, "two matches")];
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::AmbiguousManual
        );
        i.blockers.clear();
        i.outcome = Outcome::KeepSample;
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::Rejected
        );
        i.outcome = Outcome::Consolidate;
        i.evidence[0].completeness = Completeness::Unknown;
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::Identified
        );
        i.evidence.clear();
        assert_eq!(
            derive_candidate_state(Some(&i), None, None),
            CandidateState::Identified
        );
        assert_eq!(
            derive_candidate_state(Some(&i), Some(PlanStatus::Discarded), None),
            CandidateState::Identified
        );
    }
}
