//! Action events and in-memory progress (architecture 8.1: progress lives in memory, the
//! database is written only through the journal and the plan/action outcomes).
//!
//! `jobs` (14) bridges a [`Reporter`] to its per-job `watch` channel, `web` (16/19) polls the
//! [`ActionProgress`] snapshot, and `reporting` (15) reads the event list of an
//! `ExecutionReport` or subscribes through a sink. Events never carry secrets: paths are
//! root-relative, hashes are torrent hashes, messages are adapter details already redacted.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use removarr_domain::ids::{ActionRef, IntentId, PlanId};
use removarr_domain::issue::Blocker;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::plan::PlanStatus;
use removarr_domain::torrent::TorrentHash;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::qbit::LeftStoppedReason;

/// One observable step of an execution or a recovery, in order of occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum ActionEvent {
    PlanStarted {
        plan: PlanId,
        simulated: bool,
        actions: u32,
    },
    /// Live revalidation finished; `failed` lists the actions that no longer match the world.
    Revalidated {
        plan: PlanId,
        pass: u8,
        failed: BTreeMap<u32, Vec<Blocker>>,
    },
    /// An intent is durable in the journal (written before the mutation it describes).
    StepIntent {
        action: ActionRef,
        step: JournalStep,
        intent: IntentId,
    },
    StepResult {
        action: ActionRef,
        step: JournalStep,
        intent: IntentId,
        outcome: StepOutcome,
    },
    TorrentsStopped {
        plan: PlanId,
        hashes: BTreeSet<TorrentHash>,
    },
    TorrentsStarted {
        plan: PlanId,
        hashes: BTreeSet<TorrentHash>,
    },
    /// Hashes that stay stopped and visible, with the reason (INV-14, INV-20).
    TorrentsLeftStopped {
        plan: PlanId,
        hashes: BTreeMap<TorrentHash, LeftStoppedReason>,
    },
    ActionFinished {
        action: ActionRef,
        outcome: ActionOutcome,
    },
    /// Cancellation or an aborting failure: no further mutation is issued, compensation runs.
    BatchInterrupted {
        plan: PlanId,
        reason: String,
    },
    PlanFinished {
        plan: PlanId,
        /// `None` for a simulation (the status never changes).
        status: Option<PlanStatus>,
    },
    RecoveryStarted {
        plans: u32,
    },
    RecoveredPlan {
        plan: PlanId,
        status: Option<PlanStatus>,
    },
}

/// In-memory progress snapshot (polled by the UI, never persisted per file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionProgress {
    pub plan: Option<PlanId>,
    pub simulated: bool,
    pub stage: String,
    pub done: u32,
    pub total: u32,
    pub current: Option<String>,
    pub last_event: Option<ActionEvent>,
}

/// Optional push subscriber (reporting, logs).
pub type EventSink = Arc<dyn Fn(&ActionEvent) + Send + Sync>;

/// Collects events and publishes progress through a `watch` channel.
pub struct Reporter {
    tx: watch::Sender<ActionProgress>,
    events: Mutex<Vec<ActionEvent>>,
    sink: Option<EventSink>,
}

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter")
            .field("events", &self.events.lock().map(|e| e.len()).unwrap_or(0))
            .finish_non_exhaustive()
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = watch::channel(ActionProgress::default());
        Self {
            tx,
            events: Mutex::new(Vec::new()),
            sink: None,
        }
    }

    #[must_use]
    pub fn with_sink(mut self, sink: EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// A receiver for the progress snapshot (`hx-trigger="every 2s"` reads this, not the DB).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ActionProgress> {
        self.tx.subscribe()
    }

    #[must_use]
    pub fn snapshot(&self) -> ActionProgress {
        self.tx.borrow().clone()
    }

    /// Every event emitted so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<ActionEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }

    pub(crate) fn begin(&self, plan: Option<PlanId>, simulated: bool, total: u32) {
        self.tx.send_modify(|p| {
            *p = ActionProgress {
                plan,
                simulated,
                stage: "starting".into(),
                done: 0,
                total,
                current: None,
                last_event: None,
            };
        });
    }

    pub(crate) fn stage(&self, stage: &str, done: u32, current: Option<String>) {
        self.tx.send_modify(|p| {
            p.stage = stage.to_string();
            p.done = done;
            p.current = current;
        });
    }

    pub(crate) fn emit(&self, event: ActionEvent) {
        if let Some(sink) = &self.sink {
            sink(&event);
        }
        if let Ok(mut e) = self.events.lock() {
            e.push(event.clone());
        }
        self.tx.send_modify(|p| p.last_event = Some(event));
    }
}
