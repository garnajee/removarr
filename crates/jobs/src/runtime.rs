//! The persistent job runtime: two workers (Observe and Execute lanes, ADR-0007), the scheduler,
//! restart recovery, cancellation, retries and the command surface used by `backend-api`.
//!
//! Safety properties, each tested in `tests/`:
//! - **One execution lane.** One Execute worker; mutations are globally serialized, and the
//!   exclusive lock scope of a plan (roots, ARR media, torrent hashes, action types) keeps
//!   observe jobs and a second importer away from the same media (INV-16).
//! - **Persistent queue.** Every job is a `jobs` row. Queued jobs survive a restart; running
//!   jobs become `Interrupted`; observe jobs are re-queued once; execute jobs never are
//!   (recovery resolves their plans).
//! - **Recover first.** `recover` runs before every execute-lane job and after any execute
//!   error. While it is unresolved nothing executes; it is retried every 5 minutes, at most 12
//!   times, then waits for `retry_recovery` (architecture 8.4).
//! - **No duplicates.** Manual and scheduled requests share one dedupe key per kind.
//! - **Scheduling never approves.** The scheduler can enqueue only scans and the qBittorrent
//!   refresh. `ExecutePlan` is a manual command that the planner re-checks against the stored
//!   approval; `AutoCycle` is enqueued by the runtime after a scheduled complete scan and
//!   approves nothing itself.
//! - **No disk wake-ups for UI.** Status and events are in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::JobId;
use removarr_domain::jobs::{JobKind, JobOutcome, JobRecord, JobStatus, Lane};
use removarr_domain::ports::{CancelFlag, Clock, IdGen, JobStore, StateStore};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OwnedSemaphorePermit, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auto::AutoControl;
use crate::backoff::{self, Jitter};
use crate::config::{JobsConfig, RetryConfig};
use crate::events::{Hub, JobEvent, JobEventKind, JobProgress, LogLevel, ProgressHandle, redact};
use crate::handler::{JobCtx, JobError, JobHandler, JobSuccess, ScanSummary};
use crate::limits::{Limits, Resource};
use crate::locks::Locks;
use crate::schedule;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Result of a request to run a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub id: JobId,
    /// An active job with the same dedupe key already covered the request.
    pub existing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelResult {
    /// A queued job was cancelled at once.
    CancelledQueued,
    /// A running job was told to stop; it ends at its next safe checkpoint.
    Requested,
    /// The job is finished (or unknown to the queue).
    NotActive,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RecoveryStatus {
    /// The last recovery pass succeeded.
    Clean,
    /// Not run yet.
    Unknown,
    /// Failed; the next attempt is due at `next_at`.
    Pending { attempt: u32, next_at: Timestamp },
    /// 12 attempts failed. Nothing executes until `retry_recovery`.
    NeedsManual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub jobs: Vec<JobProgress>,
    pub auto_enabled: bool,
    pub auto_paused: Option<String>,
    pub recovery: RecoveryStatus,
    pub next_scan: Option<Timestamp>,
    pub next_refresh: Option<Timestamp>,
}

/// What startup found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartReport {
    pub interrupted: Vec<JobId>,
    pub requeued: Vec<JobId>,
    pub auto_paused: Option<String>,
}

/// Everything the runtime needs from the outside.
pub struct Deps {
    pub store: Arc<dyn JobStore>,
    pub state: Arc<dyn StateStore>,
    pub handler: Arc<dyn JobHandler>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub jitter: Arc<dyn Jitter>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

struct Running {
    token: CancellationToken,
    flag: CancelFlag,
    user_cancelled: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct ScheduleStatus {
    next_scan: Option<Timestamp>,
    next_refresh: Option<Timestamp>,
}

struct Inner {
    cfg: JobsConfig,
    store: Arc<dyn JobStore>,
    state: Arc<dyn StateStore>,
    handler: Arc<dyn JobHandler>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    jitter: Arc<dyn Jitter>,
    hub: Arc<Hub>,
    locks: Arc<Locks>,
    limits: Arc<Limits>,
    auto: Arc<AutoControl>,
    root: CancellationToken,
    notify: [Notify; 2],
    recovery_notify: Notify,
    /// Held while a job is claimed and registered, and by `cancel`, so a cancel can never land
    /// between a claim and its registration.
    lifecycle: tokio::sync::Mutex<()>,
    running: Mutex<BTreeMap<JobId, Running>>,
    scheduled: Mutex<BTreeSet<JobId>>,
    recovery: Mutex<RecoveryStatus>,
    schedule: Mutex<ScheduleStatus>,
}

fn lane_index(lane: Lane) -> usize {
    match lane {
        Lane::Observe => 0,
        Lane::Execute => 1,
    }
}

fn resources(kind: &JobKind) -> Vec<Resource> {
    match kind {
        JobKind::Scan => vec![Resource::Scan],
        JobKind::RefreshService(s) => vec![Resource::Api(*s)],
        JobKind::EvaluateIssues { .. } => vec![Resource::Probe],
        JobKind::EvaluateMedia { media, .. } => vec![Resource::Api(match media.service {
            removarr_domain::arr::ArrService::Sonarr => ServiceKind::Sonarr,
            removarr_domain::arr::ArrService::Radarr => ServiceKind::Radarr,
        })],
        JobKind::VerifyContent { .. } => vec![Resource::Compare],
        JobKind::ExecutePlan {
            simulate: false, ..
        }
        | JobKind::AutoCycle { .. } => {
            vec![Resource::Mutation]
        }
        JobKind::ExecutePlan { .. } | JobKind::Recover => Vec::new(),
    }
}

/// Jobs that only read over HTTP and can be dropped at once on cancellation. Everything else
/// stops cooperatively at a checkpoint (a mutation always runs to completion).
fn droppable(kind: &JobKind) -> bool {
    matches!(
        kind,
        JobKind::RefreshService(_) | JobKind::EvaluateMedia { .. }
    )
}

fn validation(msg: &str) -> JobError {
    JobError::failed(ErrorClass::Validation, msg)
}

impl Inner {
    fn track(&self, job: &JobRecord) {
        if self.hub.get(job.id).is_none() {
            self.hub.track(JobProgress {
                id: job.id,
                label: job.dedupe_key.clone(),
                lane: job.lane,
                status: job.status,
                stage: job.stage.clone(),
                done: 0,
                total: 0,
                current: None,
                warnings: Vec::new(),
                attempt: job.attempts,
                enqueued_at: job.enqueued_at,
                started_at: job.started_at,
                finished_at: None,
                cancel_requested: false,
                outcome: None,
            });
        }
    }

    async fn enqueue(&self, kind: JobKind, scheduled: bool) -> Result<Enqueued, JobError> {
        let id = JobId::new(self.ids.uuid());
        let rec = JobRecord::queued(id, kind, self.clock.now())
            .map_err(|e| JobError::failed(e.class(), e.to_string()))?;
        let got = self.store.enqueue(&rec).await?;
        let existing = got != id;
        if !existing {
            self.track(&rec);
        }
        if scheduled && matches!(rec.kind, JobKind::Scan) {
            // A scheduled tick that lands on a manual scan adopts it: that scan covers the
            // same roots, and the automatic cycle follows it.
            lock(&self.scheduled).insert(got);
        }
        self.hub.publish(
            Some(got),
            JobEventKind::Queued {
                label: rec.dedupe_key,
                lane: rec.lane,
                existing,
            },
        );
        self.notify[lane_index(rec.lane)].notify_one();
        Ok(Enqueued { id: got, existing })
    }

    /// Re-queues an interrupted observe job once: the replacement's id is remembered, and a
    /// job that is itself that replacement is not queued again.
    async fn requeue_once(&self, old: &JobRecord) -> Option<JobId> {
        let key = format!("requeued:{}", old.dedupe_key);
        let prev = self.state.get(&key).await.ok().flatten();
        if prev.as_deref() == Some(old.id.to_string().as_str()) {
            self.hub.publish(
                Some(old.id),
                JobEventKind::Warning {
                    message: format!(
                        "{} was interrupted again; it is not re-queued twice",
                        old.dedupe_key
                    ),
                },
            );
            return None;
        }
        let queued = match self.enqueue(old.kind.clone(), false).await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "could not re-queue an interrupted job");
                return None;
            }
        };
        if queued.existing {
            return None;
        }
        if let Err(e) = self.state.set(&key, &queued.id.to_string()).await {
            tracing::warn!(class = ?e.class, "could not remember the re-queued job");
        }
        Some(queued.id)
    }

    // ---- workers ---------------------------------------------------------------------------

    async fn worker(self: Arc<Self>, lane: Lane) {
        let mut failures = 0u32;
        loop {
            if self.root.is_cancelled() {
                return;
            }
            if lane == Lane::Execute && !self.ensure_recovered().await {
                return;
            }
            let claimed = {
                let _g = self.lifecycle.lock().await;
                match self.store.claim_next(lane).await {
                    Ok(Some(job)) => {
                        let token = self.root.child_token();
                        let flag = CancelFlag::new();
                        lock(&self.running).insert(
                            job.id,
                            Running {
                                token: token.clone(),
                                flag: flag.clone(),
                                user_cancelled: false,
                            },
                        );
                        Ok(Some((job, token, flag)))
                    }
                    other => other.map(|_| None),
                }
            };
            match claimed {
                Ok(Some((job, token, flag))) => {
                    failures = 0;
                    self.execute_job(job, token, flag).await;
                }
                Ok(None) => {
                    failures = 0;
                    tokio::select! {
                        () = self.notify[lane_index(lane)].notified() => {}
                        () = self.root.cancelled() => return,
                    }
                }
                Err(e) => {
                    failures += 1;
                    tracing::error!(?lane, class = ?e.class, "cannot claim the next job");
                    let wait = backoff::delay(&self.cfg.retry, failures, &*self.jitter);
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        () = self.root.cancelled() => return,
                    }
                }
            }
        }
    }

    /// T3-19: `recover` before the execute lane claims anything. Returns false on shutdown.
    async fn ensure_recovered(&self) -> bool {
        loop {
            let gate = *lock(&self.recovery);
            let attempt = match gate {
                RecoveryStatus::NeedsManual => {
                    tokio::select! {
                        () = self.recovery_notify.notified() => {}
                        () = self.root.cancelled() => return false,
                    }
                    continue;
                }
                RecoveryStatus::Pending { attempt, next_at } => {
                    let wait = next_at.0.saturating_sub(self.clock.now().0).max(0);
                    if wait > 0 {
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_millis(wait.unsigned_abs())) => {}
                            () = self.recovery_notify.notified() => continue,
                            () = self.root.cancelled() => return false,
                        }
                    }
                    attempt + 1
                }
                RecoveryStatus::Clean | RecoveryStatus::Unknown => 1,
            };
            let sink = self.hub.action_sink(None);
            match self.handler.recover(sink).await {
                Ok(sum) => {
                    *lock(&self.recovery) = RecoveryStatus::Clean;
                    self.hub.publish(
                        None,
                        JobEventKind::Recovery {
                            attempt,
                            ok: true,
                            plans: sum.plans,
                            detail: redact(&sum.notes.join("; ")),
                        },
                    );
                    if sum.plans > 0 && self.auto.enabled() {
                        // T5-16: automatic mode never resumes by itself after an interruption.
                        self.pause_auto(&format!(
                            "recovery resolved {} interrupted plan(s)",
                            sum.plans
                        ))
                        .await;
                    }
                    return true;
                }
                Err(e) => {
                    let next = if attempt >= self.cfg.retry.recovery_attempts {
                        RecoveryStatus::NeedsManual
                    } else {
                        let wait = self.cfg.retry.recovery_interval;
                        RecoveryStatus::Pending {
                            attempt,
                            next_at: self
                                .clock
                                .now()
                                .plus_millis(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
                        }
                    };
                    *lock(&self.recovery) = next;
                    self.hub.publish(
                        None,
                        JobEventKind::Recovery {
                            attempt,
                            ok: false,
                            plans: 0,
                            detail: redact(&format!("{:?}: {}", e.class, e.detail)),
                        },
                    );
                    if self.root.is_cancelled() {
                        return false;
                    }
                }
            }
        }
    }

    // ---- one job ---------------------------------------------------------------------------

    async fn execute_job(
        self: &Arc<Self>,
        job: JobRecord,
        token: CancellationToken,
        flag: CancelFlag,
    ) {
        self.track(&job);
        self.hub.update(job.id, |p| {
            p.status = JobStatus::Running;
            p.attempt = job.attempts;
            p.started_at = job.started_at;
        });
        self.hub.publish(
            Some(job.id),
            JobEventKind::Started {
                label: job.dedupe_key.clone(),
                lane: job.lane,
                attempt: job.attempts,
            },
        );
        // Bridges the token to the flag used by blocking filesystem work and the runner.
        let bridge = {
            let (t, f) = (token.clone(), flag.clone());
            tokio::spawn(async move {
                t.cancelled().await;
                f.cancel();
            })
        };
        let ctx = JobCtx {
            id: job.id,
            cancel: flag,
            token,
            progress: ProgressHandle::new(job.id, Arc::clone(&self.hub), Arc::clone(&self.store)),
            limits: Arc::clone(&self.limits),
            locks: Arc::clone(&self.locks),
            clock: Arc::clone(&self.clock),
            auto: Arc::clone(&self.auto),
        };
        let result = self.run_with_retries(&job, &ctx).await;
        bridge.abort();
        self.complete(job, result).await;
    }

    async fn run_with_retries(
        &self,
        job: &JobRecord,
        ctx: &JobCtx,
    ) -> Result<JobSuccess, JobError> {
        if job.kind == JobKind::Recover {
            return self.recover_job().await;
        }
        // Mutating lane: never retried automatically (REJ-21). Observe lane: bounded backoff.
        let cfg = if job.lane == Lane::Observe {
            self.cfg.retry
        } else {
            RetryConfig {
                read_retries: 0,
                ..self.cfg.retry
            }
        };
        let hub = &self.hub;
        backoff::retry(
            &cfg,
            &*self.jitter,
            &ctx.token,
            JobError::is_transient,
            |retry, wait, e| {
                let reason = redact(&e.to_string());
                ctx.progress.warn(&format!(
                    "transient failure, retry {retry} in {} ms",
                    wait.as_millis()
                ));
                hub.publish(
                    Some(job.id),
                    JobEventKind::Retrying {
                        retry,
                        delay_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                        reason,
                    },
                );
            },
            || self.attempt(job, ctx),
        )
        .await
        .map_err(|f| {
            if f.cancelled {
                JobError::Cancelled
            } else {
                f.error
            }
        })
    }

    async fn recover_job(&self) -> Result<JobSuccess, JobError> {
        if self.ensure_recovered().await {
            Ok(JobSuccess::default())
        } else {
            Err(JobError::Cancelled)
        }
    }

    async fn permits(
        &self,
        kind: &JobKind,
        ctx: &JobCtx,
    ) -> Result<Vec<OwnedSemaphorePermit>, JobError> {
        let mut out = Vec::new();
        for r in resources(kind) {
            let p = tokio::select! {
                p = self.limits.acquire(r) => p?,
                () = ctx.token.cancelled() => return Err(JobError::Cancelled),
            };
            out.push(p);
        }
        Ok(out)
    }

    async fn attempt(&self, job: &JobRecord, ctx: &JobCtx) -> Result<JobSuccess, JobError> {
        ctx.checkpoint()?;
        let scope = self.handler.scope(&job.kind).await?;
        let _locks = self
            .locks
            .acquire(&scope, &ctx.token)
            .await
            .map_err(|_| JobError::Cancelled)?;
        let _permits = self.permits(&job.kind, ctx).await?;
        ctx.checkpoint()?;
        if droppable(&job.kind) {
            tokio::select! {
                r = self.handler.run(&job.kind, ctx) => r,
                () = ctx.token.cancelled() => Err(JobError::Cancelled),
            }
        } else {
            self.handler.run(&job.kind, ctx).await
        }
    }

    async fn complete(&self, job: JobRecord, result: Result<JobSuccess, JobError>) {
        let user_cancelled = lock(&self.running)
            .get(&job.id)
            .is_some_and(|r| r.user_cancelled);
        let (outcome, scan) = match result {
            Ok(s) => (JobOutcome::Succeeded { metrics: s.metrics }, s.scan),
            // Cancelled by the user: Cancelled. Cancelled by shutdown: Interrupted, so the
            // work is picked up again at the next start.
            Err(JobError::Cancelled) if user_cancelled => (JobOutcome::Cancelled, None),
            Err(JobError::Cancelled) => (JobOutcome::Interrupted, None),
            Err(JobError::Failed { class, message }) => (
                JobOutcome::Failed {
                    class,
                    message: redact(&message),
                },
                None,
            ),
        };
        if let Err(e) = self.store.finish(job.id, &outcome).await {
            // The row stays Running and becomes Interrupted at the next start.
            tracing::error!(job = %job.id, class = ?e.class, "could not persist the job outcome");
        }
        let status = outcome.status();
        self.hub.finish(job.id, status, &outcome);
        self.hub.publish(
            Some(job.id),
            JobEventKind::Finished {
                status,
                outcome: outcome.clone(),
            },
        );
        lock(&self.running).remove(&job.id);
        let was_scheduled = lock(&self.scheduled).remove(&job.id);
        if outcome == JobOutcome::Interrupted && job.lane == Lane::Observe {
            self.requeue_once(&job).await;
        }
        if let (Some(summary), true) = (scan, was_scheduled) {
            self.after_scheduled_scan(&summary).await;
        }
    }

    /// Pauses automatic mode and discards approved automatic plans left waiting (F06).
    async fn pause_auto(&self, reason: &str) {
        self.auto.pause(reason).await;
        self.discard_pending().await;
    }

    async fn discard_pending(&self) {
        match self.handler.discard_pending_auto().await {
            Ok(n) if n > 0 => self.hub.publish(
                None,
                JobEventKind::Log {
                    level: LogLevel::Warn,
                    message: format!("{n} approved automatic plan(s) discarded"),
                },
            ),
            Ok(_) => {}
            Err(e) => {
                tracing::error!(class = ?e.class, "could not discard pending automatic plans");
                self.auto
                    .pause("approved automatic plans could not be discarded")
                    .await;
            }
        }
    }

    /// Automatic mode follows a scheduled scan only when it is complete and clean; otherwise it
    /// stops (scan, mappings, permissions, APIs).
    async fn after_scheduled_scan(&self, s: &ScanSummary) {
        if !self.auto.enabled() || self.auto.paused().is_some() {
            return;
        }
        if s.complete && s.uncertainties.is_empty() {
            if let Err(e) = self
                .enqueue(JobKind::AutoCycle { session: s.session }, false)
                .await
            {
                tracing::error!(error = %e, "could not queue the automatic cycle");
            }
        } else {
            let why = s
                .uncertainties
                .first()
                .map_or("the scheduled scan is incomplete", String::as_str);
            self.pause_auto(&format!("scheduled scan is not clean: {why}"))
                .await;
        }
    }

    // ---- scheduler -------------------------------------------------------------------------

    async fn scheduler(self: Arc<Self>) {
        let sc = self.cfg.schedule;
        let now = self.clock.now();
        let mut next_scan =
            schedule::next_anchored(now, sc.anchor_minute_of_day, sc.scan_interval_minutes);
        let mut next_refresh = schedule::next_every(now, sc.qbit_refresh_minutes);
        loop {
            *lock(&self.schedule) = ScheduleStatus {
                next_scan,
                next_refresh,
            };
            let Some(due) = [next_scan, next_refresh].into_iter().flatten().min() else {
                return;
            };
            // One sleep until the next due instant: no polling. The floor only guards a clock
            // that does not advance.
            let wait = due.0.saturating_sub(self.clock.now().0).max(1000);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(wait.unsigned_abs())) => {}
                () = self.root.cancelled() => return,
            }
            let now = self.clock.now();
            if next_scan.is_some_and(|t| t <= now) {
                if let Err(e) = self.enqueue(JobKind::Scan, true).await {
                    tracing::error!(error = %e, "could not queue the scheduled scan");
                }
                next_scan =
                    schedule::next_anchored(now, sc.anchor_minute_of_day, sc.scan_interval_minutes);
            }
            if next_refresh.is_some_and(|t| t <= now) {
                if let Err(e) = self
                    .enqueue(JobKind::RefreshService(ServiceKind::Qbittorrent), true)
                    .await
                {
                    tracing::error!(error = %e, "could not queue the scheduled refresh");
                }
                next_refresh = schedule::next_every(now, sc.qbit_refresh_minutes);
            }
        }
    }
}

/// The runtime. Cheap to share behind an `Arc`.
pub struct JobRuntime {
    inner: Arc<Inner>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for JobRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRuntime").finish_non_exhaustive()
    }
}

impl JobRuntime {
    #[must_use]
    pub fn new(cfg: JobsConfig, deps: Deps) -> Self {
        let hub = Hub::new(Arc::clone(&deps.clock));
        let auto = Arc::new(AutoControl::new(
            cfg.auto_mode,
            Arc::clone(&deps.state),
            Arc::clone(&hub),
        ));
        let inner = Arc::new(Inner {
            limits: Arc::new(Limits::new(&cfg.limits)),
            cfg,
            store: deps.store,
            state: deps.state,
            handler: deps.handler,
            clock: deps.clock,
            ids: deps.ids,
            jitter: deps.jitter,
            hub,
            locks: Arc::new(Locks::new()),
            auto,
            root: CancellationToken::new(),
            notify: [Notify::new(), Notify::new()],
            recovery_notify: Notify::new(),
            lifecycle: tokio::sync::Mutex::new(()),
            running: Mutex::new(BTreeMap::new()),
            scheduled: Mutex::new(BTreeSet::new()),
            recovery: Mutex::new(RecoveryStatus::Unknown),
            schedule: Mutex::new(ScheduleStatus::default()),
        });
        Self {
            inner,
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Restores state, then starts the two workers and the scheduler.
    ///
    /// Running jobs of the previous process become `Interrupted`; observe jobs are re-queued
    /// once; an interrupted automatic cycle pauses automatic mode. Queued jobs simply stay
    /// queued and are claimed in order.
    pub async fn start(&self) -> Result<StartReport, PortError> {
        let i = &self.inner;
        i.auto.load().await?;
        let mut report = StartReport::default();
        for old in i.store.interrupt_running().await? {
            report.interrupted.push(old.id);
            i.hub.publish(
                Some(old.id),
                JobEventKind::Log {
                    level: LogLevel::Warn,
                    message: format!("{} was interrupted by a restart", old.dedupe_key),
                },
            );
            match old.lane {
                Lane::Observe => {
                    if let Some(id) = i.requeue_once(&old).await {
                        report.requeued.push(id);
                    }
                }
                Lane::Execute => {
                    if matches!(old.kind, JobKind::AutoCycle { .. }) {
                        i.pause_auto("an automatic cycle was interrupted by a restart")
                            .await;
                    }
                }
            }
        }
        if let Some(marker) = i.auto.unfinished_cycle().await? {
            i.pause_auto(&format!(
                "an automatic cycle (session {marker}) never finished"
            ))
            .await;
            i.auto.clear_cycle().await?;
        }
        // F06: whatever the previous process left approved is never run by a user job.
        i.discard_pending().await;
        report.auto_paused = i.auto.paused();
        {
            let mut tasks = lock(&self.tasks);
            tasks.push(tokio::spawn(Arc::clone(i).worker(Lane::Observe)));
            tasks.push(tokio::spawn(Arc::clone(i).worker(Lane::Execute)));
            tasks.push(tokio::spawn(Arc::clone(i).scheduler()));
        }
        i.notify[0].notify_one();
        i.notify[1].notify_one();
        Ok(report)
    }

    /// Cancels every job, waits up to `grace`, then stops the workers. Jobs that did not stop
    /// stay `Running` in the database and are interrupted at the next start.
    pub async fn shutdown(&self, grace: Duration) {
        self.inner.root.cancel();
        let tasks: Vec<_> = std::mem::take(&mut *lock(&self.tasks));
        let abort: Vec<_> = tasks.iter().map(JoinHandle::abort_handle).collect();
        let joined = async {
            for t in tasks {
                let _ = t.await;
            }
        };
        if tokio::time::timeout(grace, joined).await.is_err() {
            abort.iter().for_each(tokio::task::AbortHandle::abort);
        }
    }

    // ---- commands for backend-api ------------------------------------------------------------

    /// Queues a user-requested job. `AutoCycle` and `Recover` cannot be requested: the first is
    /// queued by the runtime after a scheduled scan, the second by [`Self::retry_recovery`].
    /// A request that matches an active job returns that job (`existing`).
    pub async fn submit(&self, kind: JobKind) -> Result<Enqueued, JobError> {
        match kind {
            JobKind::AutoCycle { .. } => Err(validation("an automatic cycle cannot be requested")),
            JobKind::Recover => Err(validation("use retry_recovery")),
            kind => self.inner.enqueue(kind, false).await,
        }
    }

    /// Manual rescan of every root and service (`POST /scan`).
    pub async fn rescan(&self) -> Result<Enqueued, JobError> {
        self.submit(JobKind::Scan).await
    }

    /// Cancels a job: a queued one at once, a running one at its next safe checkpoint. A
    /// cancelled execution stops issuing mutations and the runner runs its compensation.
    pub async fn cancel(&self, id: JobId) -> Result<CancelResult, PortError> {
        let i = &self.inner;
        let _g = i.lifecycle.lock().await;
        if let Some(r) = lock(&i.running).get_mut(&id) {
            r.user_cancelled = true;
            r.token.cancel();
            r.flag.cancel();
            i.hub.update(id, |p| p.cancel_requested = true);
            i.hub.publish(Some(id), JobEventKind::CancelRequested);
            return Ok(CancelResult::Requested);
        }
        match i.store.finish(id, &JobOutcome::Cancelled).await {
            Ok(()) => {
                i.hub
                    .finish(id, JobStatus::Cancelled, &JobOutcome::Cancelled);
                i.hub.publish(
                    Some(id),
                    JobEventKind::Finished {
                        status: JobStatus::Cancelled,
                        outcome: JobOutcome::Cancelled,
                    },
                );
                lock(&i.scheduled).remove(&id);
                Ok(CancelResult::CancelledQueued)
            }
            Err(e) if e.class == ErrorClass::NotFound => Ok(CancelResult::NotFound),
            Err(e)
                if matches!(
                    e.class,
                    ErrorClass::Storage | ErrorClass::Unavailable | ErrorClass::Internal
                ) =>
            {
                Err(e)
            }
            Err(_) => Ok(CancelResult::NotActive),
        }
    }

    /// `POST /recovery/retry`: starts a fresh round of recovery attempts.
    pub fn retry_recovery(&self) {
        *lock(&self.inner.recovery) = RecoveryStatus::Unknown;
        self.inner.recovery_notify.notify_one();
        self.inner.notify[lane_index(Lane::Execute)].notify_one();
    }

    /// `POST /auto/resume`: the only way to clear `auto_paused`.
    pub async fn resume_auto(&self) -> Result<bool, PortError> {
        self.inner.auto.resume().await
    }

    #[must_use]
    pub fn auto(&self) -> Arc<AutoControl> {
        Arc::clone(&self.inner.auto)
    }

    // ---- event streams and status (memory only) ---------------------------------------------

    /// Live events. A late subscriber replays with [`Self::events_since`] first.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<JobEvent> {
        self.inner.hub.subscribe()
    }

    #[must_use]
    pub fn events_since(&self, after: u64) -> Vec<JobEvent> {
        self.inner.hub.events_since(after)
    }

    #[must_use]
    pub fn progress(&self, id: JobId) -> Option<JobProgress> {
        self.inner.hub.get(id)
    }

    #[must_use]
    pub fn watch(&self, id: JobId) -> Option<watch::Receiver<JobProgress>> {
        self.inner.hub.watch(id)
    }

    /// Everything the dashboard needs about jobs. Reads memory only: no database call, no
    /// filesystem call, nothing that can wake a disk.
    #[must_use]
    pub fn status(&self) -> RuntimeStatus {
        let i = &self.inner;
        let sched = *lock(&i.schedule);
        RuntimeStatus {
            jobs: i.hub.snapshot(),
            auto_enabled: i.auto.enabled(),
            auto_paused: i.auto.paused(),
            recovery: *lock(&i.recovery),
            next_scan: sched.next_scan,
            next_refresh: sched.next_refresh,
        }
    }

    #[must_use]
    pub fn limits(&self) -> Arc<Limits> {
        Arc::clone(&self.inner.limits)
    }

    #[must_use]
    pub fn locks(&self) -> Arc<Locks> {
        Arc::clone(&self.inner.locks)
    }
}
