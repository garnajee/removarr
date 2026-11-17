//! Job events and in-memory progress (architecture 8.1: progress lives in memory, the database
//! is written only on stage changes and completion). Backend and UI read this module; a status
//! poll never reaches a disk or the database.
//!
//! Events are serializable and carry no secrets: messages go through [`redact`].

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use removarr_domain::ids::JobId;
use removarr_domain::jobs::{JobOutcome, JobRecord, JobStatus, Lane};
use removarr_domain::ports::{Clock, JobStore};
use removarr_domain::time::Timestamp;
use removarr_workflows::actions::{ActionEvent, EventSink};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

/// Events kept for late subscribers (architecture 8.5 item 7).
pub const RING_CAPACITY: usize = 2000;
/// Terminal jobs kept in the memory view.
const FINISHED_KEPT: usize = 100;
const WARNINGS_KEPT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum JobEventKind {
    /// `existing` is true when an active job with the same dedupe key absorbed the request.
    Queued {
        label: String,
        lane: Lane,
        existing: bool,
    },
    Started {
        label: String,
        lane: Lane,
        attempt: u32,
    },
    Stage {
        stage: String,
    },
    Progress {
        done: u64,
        total: u64,
        current: Option<String>,
    },
    Warning {
        message: String,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// A transient failure; the job runs again after `delay_ms`.
    Retrying {
        retry: u32,
        delay_ms: u64,
        reason: String,
    },
    CancelRequested,
    /// Terminal outcome of a job. Always the last event of that job.
    Finished {
        status: JobStatus,
        outcome: JobOutcome,
    },
    /// An event of the action runner (execution or recovery).
    Action {
        action: ActionEvent,
    },
    AutoPaused {
        reason: String,
    },
    AutoResumed,
    /// One recovery attempt (T3-19).
    Recovery {
        attempt: u32,
        ok: bool,
        plans: u32,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    /// Strictly increasing per process.
    pub seq: u64,
    pub at: Timestamp,
    pub job: Option<JobId>,
    #[serde(flatten)]
    pub kind: JobEventKind,
}

/// Memory-only view of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobProgress {
    pub id: JobId,
    pub label: String,
    pub lane: Lane,
    pub status: JobStatus,
    pub stage: Option<String>,
    pub done: u64,
    pub total: u64,
    pub current: Option<String>,
    pub warnings: Vec<String>,
    pub attempt: u32,
    pub enqueued_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub cancel_requested: bool,
    pub outcome: Option<JobOutcome>,
}

/// Removes URLs and credentials from a message before it is published (no tracker URL, no
/// credential in the log stream), with the one shared rule.
#[must_use]
pub fn redact(message: &str) -> String {
    removarr_domain::redact::sanitize(message)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Event bus and progress table shared by the runtime and every job.
pub struct Hub {
    clock: Arc<dyn Clock>,
    seq: AtomicU64,
    tx: broadcast::Sender<JobEvent>,
    ring: Mutex<VecDeque<JobEvent>>,
    jobs: Mutex<BTreeMap<JobId, watch::Sender<JobProgress>>>,
    finished: Mutex<VecDeque<JobId>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

impl Hub {
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            clock,
            seq: AtomicU64::new(0),
            tx,
            ring: Mutex::new(VecDeque::new()),
            jobs: Mutex::new(BTreeMap::new()),
            finished: Mutex::new(VecDeque::new()),
        })
    }

    pub fn publish(&self, job: Option<JobId>, kind: JobEventKind) {
        let ev = JobEvent {
            seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
            at: self.clock.now(),
            job,
            kind,
        };
        {
            let mut ring = lock(&self.ring);
            if ring.len() >= RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        // No subscriber is not an error.
        let _ = self.tx.send(ev);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }

    /// Buffered events with `seq > after` (a subscriber that connects late replays from 0).
    #[must_use]
    pub fn events_since(&self, after: u64) -> Vec<JobEvent> {
        lock(&self.ring)
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    pub(crate) fn track(&self, p: JobProgress) {
        let (tx, _) = watch::channel(p.clone());
        lock(&self.jobs).insert(p.id, tx);
    }

    /// Makes a job that the previous process left running readable as `Interrupted`. It counts
    /// as a finished job (same eviction); nothing about it can resume.
    pub(crate) fn track_interrupted(&self, old: &JobRecord) {
        let at = old.finished_at.unwrap_or_else(|| self.clock.now());
        let (tx, _) = watch::channel(JobProgress {
            id: old.id,
            label: old.dedupe_key.clone(),
            lane: old.lane,
            status: JobStatus::Interrupted,
            stage: old.stage.clone(),
            done: 0,
            total: 0,
            current: None,
            warnings: vec!["interrupted by a restart".to_string()],
            attempt: old.attempts,
            enqueued_at: old.enqueued_at,
            started_at: old.started_at,
            finished_at: Some(at),
            cancel_requested: false,
            outcome: Some(JobOutcome::Interrupted),
        });
        lock(&self.jobs).insert(old.id, tx);
        let mut fin = lock(&self.finished);
        fin.push_back(old.id);
        while fin.len() > FINISHED_KEPT {
            if let Some(o) = fin.pop_front() {
                lock(&self.jobs).remove(&o);
            }
        }
    }

    pub(crate) fn update(&self, id: JobId, f: impl FnOnce(&mut JobProgress)) {
        if let Some(tx) = lock(&self.jobs).get(&id) {
            tx.send_modify(f);
        }
    }

    /// Marks a job terminal and forgets the oldest finished jobs.
    pub(crate) fn finish(&self, id: JobId, status: JobStatus, outcome: &JobOutcome) {
        let now = self.clock.now();
        self.update(id, |p| {
            p.status = status;
            p.finished_at = Some(now);
            p.outcome = Some(outcome.clone());
        });
        let mut fin = lock(&self.finished);
        fin.push_back(id);
        while fin.len() > FINISHED_KEPT {
            if let Some(old) = fin.pop_front() {
                lock(&self.jobs).remove(&old);
            }
        }
    }

    /// Every tracked job, memory only.
    #[must_use]
    pub fn snapshot(&self) -> Vec<JobProgress> {
        lock(&self.jobs)
            .values()
            .map(|tx| tx.borrow().clone())
            .collect()
    }

    #[must_use]
    pub fn get(&self, id: JobId) -> Option<JobProgress> {
        lock(&self.jobs).get(&id).map(|tx| tx.borrow().clone())
    }

    /// A receiver that wakes on every change of one job (`hx-trigger="every 2s"` can read it).
    #[must_use]
    pub fn watch(&self, id: JobId) -> Option<watch::Receiver<JobProgress>> {
        lock(&self.jobs).get(&id).map(watch::Sender::subscribe)
    }

    /// A sink for the action runner's events. With a job it also maps them to counters.
    #[must_use]
    pub fn action_sink(self: &Arc<Self>, job: Option<JobId>) -> EventSink {
        let hub = Arc::clone(self);
        let done = Arc::new(AtomicU64::new(0));
        Arc::new(move |ev: &ActionEvent| {
            if let Some(job) = job {
                match ev {
                    ActionEvent::PlanStarted { actions, .. } => {
                        done.store(0, Ordering::SeqCst);
                        hub.update(job, |p| {
                            p.total = u64::from(*actions);
                            p.done = 0;
                        });
                    }
                    ActionEvent::ActionFinished { .. } => {
                        let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                        hub.update(job, |p| p.done = d);
                    }
                    ActionEvent::StepIntent { step, .. } => {
                        hub.update(job, |p| {
                            p.current = Some(format!("{step:?}").to_lowercase())
                        });
                    }
                    _ => {}
                }
            }
            hub.publish(job, JobEventKind::Action { action: ev.clone() });
        })
    }
}

/// What a running job uses to publish its stage, counters, warnings and logs.
#[derive(Clone)]
pub struct ProgressHandle {
    job: JobId,
    hub: Arc<Hub>,
    store: Arc<dyn JobStore>,
}

impl std::fmt::Debug for ProgressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressHandle")
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

impl ProgressHandle {
    #[must_use]
    pub fn new(job: JobId, hub: Arc<Hub>, store: Arc<dyn JobStore>) -> Self {
        Self { job, hub, store }
    }

    /// Stage change: memory, event, and one database write (never per file).
    pub async fn stage(&self, stage: &str) {
        let stage = redact(stage);
        self.hub.update(self.job, |p| p.stage = Some(stage.clone()));
        self.hub.publish(
            Some(self.job),
            JobEventKind::Stage {
                stage: stage.clone(),
            },
        );
        if let Err(e) = self.store.set_stage(self.job, &stage).await {
            tracing::warn!(job = %self.job, class = ?e.class, "could not persist the job stage");
        }
    }

    /// Counters and current item: memory only.
    pub fn counts(&self, done: u64, total: u64, current: Option<String>) {
        let current = current.map(|c| redact(&c));
        self.hub.update(self.job, |p| {
            p.done = done;
            p.total = total;
            p.current.clone_from(&current);
        });
    }

    /// A warning is kept on the job and published.
    pub fn warn(&self, message: &str) {
        let message = redact(message);
        self.hub.update(self.job, |p| {
            if p.warnings.len() < WARNINGS_KEPT {
                p.warnings.push(message.clone());
            }
        });
        self.hub
            .publish(Some(self.job), JobEventKind::Warning { message });
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        self.hub.publish(
            Some(self.job),
            JobEventKind::Log {
                level,
                message: redact(message),
            },
        );
    }

    /// A sink for the action runner: forwards its events and maps them to counters.
    #[must_use]
    pub fn action_sink(&self) -> EventSink {
        self.hub.action_sink(Some(self.job))
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redact_removes_credentials_as_well_as_urls() {
        let m = redact("retry http://t.example/a?x=1 apikey=abc123 Authorization: Bearer tok9");
        for leaked in ["t.example", "abc123", "tok9"] {
            assert!(!m.contains(leaked), "{leaked} in {m}");
        }
        assert!(m.contains("retry"));
    }
}
