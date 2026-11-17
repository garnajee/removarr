//! Execution context and the journaled step: intent (durable) -> adapter call -> result.

use std::collections::BTreeSet;
use std::future::Future;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::{ActionRef, IntentId};
use removarr_domain::journal::{IntentRecord, JournalMode, JournalStep, ResultRecord, StepOutcome};
use removarr_domain::plan::{ActionPlan, Approval};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentHash;
use serde::Serialize;

use super::events::{ActionEvent, Reporter};
use super::{ActionPorts, ActionSettings, Mutators};

/// Everything one run needs. `simulated` is true for `simulate = true` tickets and for every
/// run of a dry-run process: then no mutator is ever invoked.
pub(crate) struct Ctx<'a> {
    pub ports: &'a ActionPorts,
    pub mutators: &'a Mutators,
    pub settings: &'a ActionSettings,
    pub plan: &'a ActionPlan,
    /// The approval of the ticket (acknowledged samples, confirmed removals); `None` for a
    /// simulation of a draft plan and at recovery.
    pub approval: Option<&'a Approval>,
    pub simulated: bool,
    pub cancel: &'a CancelFlag,
    pub reporter: &'a Reporter,
}

/// What a journaled step did.
#[derive(Debug)]
pub(crate) enum StepRun<T> {
    Done(T),
    Simulated,
    Failed(PortError),
}

impl Ctx<'_> {
    pub fn action_ref(&self, index: u32) -> ActionRef {
        ActionRef {
            plan: self.plan.id,
            index,
        }
    }

    pub fn journal_mode(&self) -> JournalMode {
        if self.simulated {
            JournalMode::Simulated
        } else {
            JournalMode::Live
        }
    }

    /// Writes the intent; returns only once it is durable. A failure here aborts the run before
    /// any mutation (INV-08).
    pub async fn intent(
        &self,
        action: u32,
        step: JournalStep,
        target: serde_json::Value,
        observed: serde_json::Value,
        hashes: BTreeSet<TorrentHash>,
    ) -> Result<IntentId, PortError> {
        let id = IntentId::new(self.ports.ids.uuid());
        let gain = self
            .plan
            .actions
            .get(action as usize)
            .map(|a| a.expected_gain)
            .unwrap_or_default();
        let record = IntentRecord {
            id,
            action: self.action_ref(action),
            step,
            mode: self.journal_mode(),
            target,
            observed,
            expected_gain: gain,
            hashes,
            created_at: self.ports.clock.now(),
        };
        self.ports.journal.record_intent(&record).await?;
        self.reporter.emit(ActionEvent::StepIntent {
            action: self.action_ref(action),
            step,
            intent: id,
        });
        Ok(id)
    }

    pub async fn result(
        &self,
        intent: IntentId,
        action: u32,
        step: JournalStep,
        outcome: StepOutcome,
    ) -> Result<(), PortError> {
        self.ports
            .journal
            .record_result(&ResultRecord {
                intent_id: intent,
                outcome: outcome.clone(),
                at: self.ports.clock.now(),
            })
            .await?;
        self.reporter.emit(ActionEvent::StepResult {
            action: self.action_ref(action),
            step,
            intent,
            outcome,
        });
        Ok(())
    }

    /// One journaled mutation. `run` is built only when the step is live; a simulation records
    /// `Simulated` and never calls it. `Err` means the journal itself failed (the run stops);
    /// an adapter failure is `StepRun::Failed` and is journaled.
    pub async fn step<T, F, Fut>(
        &self,
        action: u32,
        step: JournalStep,
        target: serde_json::Value,
        observed: serde_json::Value,
        hashes: BTreeSet<TorrentHash>,
        run: F,
    ) -> Result<StepRun<T>, PortError>
    where
        T: Serialize,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, PortError>>,
    {
        let id = self.intent(action, step, target, observed, hashes).await?;
        if self.simulated {
            self.result(id, action, step, StepOutcome::Simulated)
                .await?;
            return Ok(StepRun::Simulated);
        }
        let outcome = run().await;
        let record = match &outcome {
            Ok(v) => StepOutcome::Done {
                final_state: serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            },
            Err(e) => StepOutcome::Failed {
                class: e.class.clone(),
                message: e.detail.clone(),
            },
        };
        self.result(id, action, step, record).await?;
        Ok(match outcome {
            Ok(v) => StepRun::Done(v),
            Err(e) => StepRun::Failed(e),
        })
    }

    /// Runs blocking filesystem work off the async runtime.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, PortError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, PortError> + Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| PortError::new(ErrorClass::Internal, format!("blocking task: {e}")))?
    }
}

/// JSON helper for intent payloads.
pub(crate) fn json<T: Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// CP2T5-F03: whether a mutating command that errored may still have been accepted. Only a
/// definitive refusal before anything was sent (configuration, validation, capability,
/// authorisation, a stale token) rules it out; a transport or internal error does not: ARR
/// commands are asynchronous and may run a moment later.
pub(crate) fn command_may_have_run(class: &ErrorClass) -> bool {
    !matches!(
        class,
        ErrorClass::Config
            | ErrorClass::Validation
            | ErrorClass::NotFound
            | ErrorClass::Unauthorized
            | ErrorClass::Forbidden
            | ErrorClass::RateLimited
            | ErrorClass::StateChanged
            | ErrorClass::Rejected
            | ErrorClass::CapabilityDisabled
            | ErrorClass::Confinement
            | ErrorClass::Cancelled
    )
}
