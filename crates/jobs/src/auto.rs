//! Automatic mode (tier 5): persisted pause latch, failed sets from the journal, and the
//! two-phase cycle of architecture 7.6.
//!
//! Rules enforced here:
//! - `auto_mode` is read once at startup and never toggled at runtime (ADR-0003);
//! - `auto_paused` is persisted in the state store and only [`AutoControl::resume`] (the
//!   user's `POST /auto/resume`) clears it;
//! - the cycle stops at the first uncertainty and sets `auto_paused` with the reason;
//! - a failed consolidation never leads to a deletion of its completed file (T3-12, T5-10);
//! - scheduling never approves anything: plans are approved by policy inside the planner
//!   (`approve_auto`) and this module only asks for them and runs them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::ids::{PlanId, SessionId, SubjectKey};
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{ActionKind, PlannedAction};
use removarr_domain::ports::{CancelFlag, StateStore};
use removarr_domain::time::Timestamp;
use removarr_workflows::actions::{ActionEvent, EventSink, ExecutionReport};
use removarr_workflows::plans::{AutoContext, AutoPhase, action_paths, action_subjects};

use crate::events::{Hub, JobEventKind};
use crate::handler::{JobCtx, JobError, JobSuccess, RecoverySummary};
use crate::limits::Resource;
use crate::locks::Scope;

/// State key of the pause latch (empty value = not paused).
pub const KEY_PAUSED: &str = "auto_paused";
/// State key set while a cycle runs. A crash leaves it behind and the next start pauses.
pub const KEY_CYCLE: &str = "auto_cycle_running";
/// Failures older than this no longer exclude a subject (A4).
pub const FAILURE_WINDOW_MS: i64 = 7 * 86_400_000;
const REASON_MAX: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoStatus {
    pub enabled: bool,
    pub paused: Option<String>,
}

pub struct AutoControl {
    enabled: bool,
    state: Arc<dyn StateStore>,
    hub: Arc<Hub>,
    paused: Mutex<Option<String>>,
}

impl std::fmt::Debug for AutoControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoControl")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl AutoControl {
    #[must_use]
    pub fn new(enabled: bool, state: Arc<dyn StateStore>, hub: Arc<Hub>) -> Self {
        Self {
            enabled,
            state,
            hub,
            paused: Mutex::new(None),
        }
    }

    /// Reads the persisted latch. Call once at startup.
    pub async fn load(&self) -> Result<(), PortError> {
        let v = self.state.get(KEY_PAUSED).await?.filter(|s| !s.is_empty());
        *self.paused.lock().unwrap_or_else(PoisonError::into_inner) = v;
        Ok(())
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn paused(&self) -> Option<String> {
        self.paused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn status(&self) -> AutoStatus {
        AutoStatus {
            enabled: self.enabled,
            paused: self.paused(),
        }
    }

    /// Sets the latch. The first reason is kept (stop at the first uncertainty). The latch is
    /// set in memory even when persisting fails (fail closed). Returns true when newly paused.
    pub async fn pause(&self, reason: &str) -> bool {
        let reason: String = crate::events::redact(reason)
            .chars()
            .take(REASON_MAX)
            .collect();
        {
            let mut p = self.paused.lock().unwrap_or_else(PoisonError::into_inner);
            if p.is_some() {
                return false;
            }
            *p = Some(reason.clone());
        }
        if let Err(e) = self.state.set(KEY_PAUSED, &reason).await {
            tracing::error!(class = ?e.class, "could not persist auto_paused; paused in memory only");
        }
        self.hub.publish(None, JobEventKind::AutoPaused { reason });
        true
    }

    /// The user's explicit resume. Returns whether it was paused.
    pub async fn resume(&self) -> Result<bool, PortError> {
        self.state.set(KEY_PAUSED, "").await?;
        let was = self
            .paused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .is_some();
        if was {
            self.hub.publish(None, JobEventKind::AutoResumed);
        }
        Ok(was)
    }

    /// Records that a cycle started (crash marker).
    pub async fn mark_cycle(&self, session: SessionId) -> Result<(), PortError> {
        self.state.set(KEY_CYCLE, &session.to_string()).await
    }

    pub async fn clear_cycle(&self) -> Result<(), PortError> {
        self.state.set(KEY_CYCLE, "").await
    }

    /// The marker of a cycle that never finished, if any.
    pub async fn unfinished_cycle(&self) -> Result<Option<String>, PortError> {
        Ok(self.state.get(KEY_CYCLE).await?.filter(|s| !s.is_empty()))
    }

    /// The planner's `AutoContext`: `auto_mode`, the latch, and the failed sets rebuilt from
    /// the journal (T5-10). `extra_paths` are failures of the running cycle.
    pub async fn context(
        &self,
        history: &dyn ActionHistory,
        extra_paths: &BTreeSet<RootedPath>,
        now: Timestamp,
    ) -> Result<AutoContext, PortError> {
        let mut sets = failed_sets(history, now).await?;
        sets.paths.extend(extra_paths.iter().cloned());
        Ok(AutoContext {
            auto_mode: self.enabled,
            auto_paused: self.paused(),
            failed_subjects: sets.subjects,
            failed_paths: sets.paths,
        })
    }
}

/// One planned action of a plan that started executing, with what the journal recorded.
#[derive(Debug, Clone)]
pub struct ExecutedAction {
    pub plan: PlanId,
    /// When the plan last changed (any instant inside the failure window is enough).
    pub at: Timestamp,
    pub action: PlannedAction,
    /// `None` = no final outcome was recorded (an unresolved action).
    pub outcome: Option<ActionOutcome>,
}

/// The journal's view of past executions. Implemented by the application over the plan and
/// journal tables (`PlanRepo::ids_with_status` + `actions` + `PlanStore::get`); the ports have
/// no listing, so this trait carries it.
#[async_trait]
pub trait ActionHistory: Send + Sync {
    /// Every action of every plan that started executing at or after `since` (statuses
    /// Executing, Executed, PartiallyExecuted, Failed, Interrupted, Invalidated).
    async fn executed_since(&self, since: Timestamp) -> Result<Vec<ExecutedAction>, PortError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailedSets {
    /// Paths of every consolidation that is not `Verified` (T3-12).
    pub paths: BTreeSet<RootedPath>,
    /// Subjects of every non-`Verified` action in the failure window (A4).
    pub subjects: BTreeSet<SubjectKey>,
}

/// Failed sets from the journal. A simulation is not a failure. A missing outcome counts as
/// not verified.
pub async fn failed_sets(
    history: &dyn ActionHistory,
    now: Timestamp,
) -> Result<FailedSets, PortError> {
    let since = now.plus_millis(-FAILURE_WINDOW_MS);
    let mut sets = FailedSets::default();
    for h in history.executed_since(since).await? {
        if matches!(
            h.outcome,
            Some(ActionOutcome::Verified | ActionOutcome::Simulated)
        ) {
            continue;
        }
        sets.subjects.extend(action_subjects(&h.action));
        if h.action.kind == ActionKind::ConsolidateHardlink {
            sets.paths.extend(action_paths(&h.action));
        }
    }
    Ok(sets)
}

// ---- the cycle ------------------------------------------------------------------------------

/// Facts the cycle needs about one planned action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionFacts {
    pub index: u32,
    pub kind: ActionKind,
    pub paths: BTreeSet<RootedPath>,
    pub subjects: BTreeSet<SubjectKey>,
}

/// An approved automatic plan, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhasePlan {
    pub plan: PlanId,
    /// Exclusive scope of a live run.
    pub live_scope: Scope,
    /// Shared scope of a simulation.
    pub read_scope: Scope,
    pub actions: Vec<ActionFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    /// A normal policy exclusion (some units were not eligible) rather than an error.
    pub benign: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhasePlans {
    pub plans: Vec<PhasePlan>,
    pub refused: Vec<Refusal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseError {
    /// Automatic mode is off or paused (A1): nothing to do, not an uncertainty.
    Refused(String),
    /// Planning failed: an uncertainty.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescanSummary {
    pub session: SessionId,
    pub complete: bool,
    pub uncertainties: Vec<String>,
}

/// The workflow steps the cycle orchestrates. The production implementation is
/// [`crate::workflow::WorkflowHandler`]; tests use fakes.
#[async_trait]
pub trait CycleEngine: Send + Sync {
    /// A dry-run process: every plan is simulated and no rescan happens.
    fn simulate(&self) -> bool;
    /// Builds and policy-approves the plans of one phase for `session`.
    async fn plan_phase(
        &self,
        phase: AutoPhase,
        ctx: &AutoContext,
        session: SessionId,
    ) -> Result<PhasePlans, PhaseError>;
    /// The current ignore list (T5-09).
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError>;
    /// Discards a plan that will not run, so it can never be executed later.
    async fn discard(&self, plan: PlanId) -> Result<(), PortError>;
    async fn recover(&self, sink: EventSink) -> Result<RecoverySummary, PortError>;
    async fn execute(
        &self,
        plan: PlanId,
        simulate: bool,
        cancel: &CancelFlag,
        sink: EventSink,
    ) -> Result<ExecutionReport, PortError>;
    /// Observes again after live mutations of `roots`, with fresh service reads.
    async fn rescan(
        &self,
        roots: &BTreeSet<RootId>,
        cancel: &CancelFlag,
    ) -> Result<RescanSummary, PortError>;
}

pub struct CycleDeps<'a> {
    pub engine: &'a dyn CycleEngine,
    pub history: &'a dyn ActionHistory,
    pub ctx: &'a JobCtx,
}

impl std::fmt::Debug for CycleDeps<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleDeps").finish_non_exhaustive()
    }
}

/// What one executed plan means for the cycle.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Assessment {
    /// Why the cycle must stop (first uncertainty), if it must.
    pub stop: Option<String>,
    /// Consolidation paths that did not verify: phase 2 never deletes them.
    pub failed_paths: BTreeSet<RootedPath>,
    /// Roots with at least one live mutation result.
    pub touched: BTreeSet<RootId>,
    pub verified: u64,
}

fn is_service_step(step: JournalStep) -> bool {
    matches!(
        step,
        JournalStep::QbitStop
            | JournalStep::QbitStart
            | JournalStep::QbitRecheck
            | JournalStep::QbitRemoveKeepData
    )
}

/// Applies T5-08: only `Verified` (or `Simulated`) and `NotExecuted` by revalidation let the
/// cycle go on. Anything else, a left-stopped torrent or an interruption stops it.
#[must_use]
pub fn assess(plan: &PhasePlan, report: &ExecutionReport) -> Assessment {
    let mut a = Assessment::default();
    let facts = |i: u32| plan.actions.iter().find(|f| f.index == i);
    for r in &report.actions {
        let not_verified = match &r.outcome {
            ActionOutcome::Verified => {
                a.verified += 1;
                false
            }
            ActionOutcome::Simulated => false,
            ActionOutcome::NotExecuted { .. } => true,
            other => {
                a.stop.get_or_insert_with(|| {
                    format!("action {} ({:?}) ended as {other:?}", r.index, r.kind)
                });
                true
            }
        };
        let paths = facts(r.index).map(|f| &f.paths);
        if not_verified && r.kind == ActionKind::ConsolidateHardlink {
            a.failed_paths.extend(paths.into_iter().flatten().cloned());
        }
    }
    if report.interrupted {
        a.stop
            .get_or_insert_with(|| "execution was interrupted".to_string());
    }
    if !report.left_stopped.is_empty() {
        a.stop.get_or_insert_with(|| {
            format!("{} torrent(s) were left stopped", report.left_stopped.len())
        });
    }
    if !report.simulated {
        for ev in &report.events {
            let ActionEvent::StepResult {
                action,
                step,
                outcome: StepOutcome::Done { .. },
                ..
            } = ev
            else {
                continue;
            };
            if !is_service_step(*step) {
                let roots = facts(action.index)
                    .into_iter()
                    .flat_map(|f| f.paths.iter().map(|p| p.root.clone()));
                a.touched.extend(roots);
            }
        }
    }
    a
}

enum Flow {
    Go,
    Stop,
}

struct Cycle<'a, 'b> {
    d: &'b CycleDeps<'a>,
    session: SessionId,
    failed_paths: BTreeSet<RootedPath>,
    touched: BTreeSet<RootId>,
    m: BTreeMap<String, u64>,
}

impl Cycle<'_, '_> {
    fn count(&mut self, key: &str, n: u64) {
        *self.m.entry(key.to_string()).or_default() += n;
    }

    async fn stop(&mut self, reason: String) -> Flow {
        let ctx = self.d.ctx;
        ctx.progress
            .warn(&format!("automatic cycle stopped: {reason}"));
        ctx.auto.pause(&reason).await;
        self.m.insert("paused".into(), 1);
        Flow::Stop
    }

    async fn discard(&self, plans: &[PhasePlan]) {
        for p in plans {
            if let Err(e) = self.d.engine.discard(p.plan).await {
                tracing::warn!(plan = %p.plan, class = ?e.class, "could not discard an unused automatic plan");
            }
        }
    }

    async fn phase(&mut self, phase: AutoPhase) -> Result<Flow, JobError> {
        let (d, ctx) = (self.d, self.d.ctx);
        ctx.checkpoint()?;
        let stage = match phase {
            AutoPhase::Consolidations => "phase 1: consolidations",
            AutoPhase::Deletions => "phase 2: deletions",
        };
        ctx.progress.stage(stage).await;
        let auto_ctx = match ctx
            .auto
            .context(d.history, &self.failed_paths, ctx.clock.now())
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(self.stop(format!("journal unreadable: {}", e.detail)).await),
        };
        let planned = match d.engine.plan_phase(phase, &auto_ctx, self.session).await {
            Ok(p) => p,
            Err(PhaseError::Refused(why)) => {
                ctx.progress
                    .warn(&format!("automatic planning refused: {why}"));
                self.m.insert("skipped".into(), 1);
                return Ok(Flow::Stop);
            }
            Err(PhaseError::Failed(why)) => {
                return Ok(self.stop(format!("planning failed: {why}")).await);
            }
        };
        self.count("plans_planned", planned.plans.len() as u64);
        if let Some(r) = planned.refused.iter().find(|r| !r.benign) {
            self.discard(&planned.plans).await;
            return Ok(self.stop(format!("policy refusal: {}", r.reason)).await);
        }
        let ignored = match d.engine.ignored().await {
            Ok(i) => i,
            Err(e) => {
                self.discard(&planned.plans).await;
                return Ok(self
                    .stop(format!("ignore list unreadable: {}", e.detail))
                    .await);
            }
        };
        let total = planned.plans.len() as u64;
        for (i, plan) in planned.plans.iter().enumerate() {
            let rest = &planned.plans[i..];
            if ctx.checkpoint().is_err() {
                self.discard(rest).await;
                return Err(JobError::Cancelled);
            }
            ctx.progress
                .counts(i as u64, total, Some(plan.plan.to_string()));
            // T5-09: an item ignored after planning is never executed.
            if plan
                .actions
                .iter()
                .any(|a| a.subjects.iter().any(|s| ignored.contains(s)))
            {
                self.discard(std::slice::from_ref(plan)).await;
                self.count("plans_skipped_ignored", 1);
                continue;
            }
            // T3-19: recovery before every execute; unresolved or needed recovery stops the cycle.
            match d.engine.recover(ctx.progress.action_sink()).await {
                Ok(s) if s.plans == 0 && s.left_stopped == 0 => {}
                Ok(s) => {
                    self.discard(rest).await;
                    return Ok(self
                        .stop(format!("recovery resolved {} interrupted plan(s)", s.plans))
                        .await);
                }
                Err(e) => {
                    self.discard(rest).await;
                    return Ok(self
                        .stop(format!("recovery unresolved: {}", e.detail))
                        .await);
                }
            }
            let simulate = d.engine.simulate();
            let scope = if simulate {
                &plan.read_scope
            } else {
                &plan.live_scope
            };
            let Ok(guard) = ctx.locks.acquire(scope, &ctx.token).await else {
                self.discard(rest).await;
                return Err(JobError::Cancelled);
            };
            let result = d
                .engine
                .execute(plan.plan, simulate, &ctx.cancel, ctx.progress.action_sink())
                .await;
            drop(guard);
            self.count("plans_executed", 1);
            let report = match result {
                Ok(r) => r,
                Err(e) => {
                    // T3-19: recover after any execute error, then stop.
                    let _ = d.engine.recover(ctx.progress.action_sink()).await;
                    self.discard(&rest[1..]).await;
                    return Ok(self.stop(format!("execution failed: {:?}", e.class)).await);
                }
            };
            let a = assess(plan, &report);
            self.count("actions_verified", a.verified);
            self.failed_paths.extend(a.failed_paths);
            self.touched.extend(a.touched);
            if let Some(why) = a.stop {
                self.discard(&rest[1..]).await;
                return Ok(self.stop(why).await);
            }
        }
        ctx.progress.counts(total, total, None);
        Ok(Flow::Go)
    }

    async fn rescan(&mut self) -> Result<Flow, JobError> {
        let (d, ctx) = (self.d, self.d.ctx);
        if self.touched.is_empty() || d.engine.simulate() {
            return Ok(Flow::Go);
        }
        ctx.checkpoint()?;
        ctx.progress.stage("rescan of mutated roots").await;
        let scope = Scope::shared_roots(self.touched.iter());
        let Ok(_guard) = ctx.locks.acquire(&scope, &ctx.token).await else {
            return Err(JobError::Cancelled);
        };
        let permit = tokio::select! {
            p = ctx.limits.acquire(Resource::Scan) => p,
            () = ctx.token.cancelled() => return Err(JobError::Cancelled),
        };
        let Ok(_permit) = permit else {
            return Ok(self.stop("scan limit unavailable".into()).await);
        };
        let summary = match d.engine.rescan(&self.touched, &ctx.cancel).await {
            Ok(s) => s,
            Err(e) if e.class == removarr_domain::error::ErrorClass::Cancelled => {
                return Err(JobError::Cancelled);
            }
            Err(e) => return Ok(self.stop(format!("rescan failed: {}", e.detail)).await),
        };
        self.count("rescans", 1);
        if !summary.complete || !summary.uncertainties.is_empty() {
            let why = summary
                .uncertainties
                .first()
                .cloned()
                .unwrap_or_else(|| "scan not complete".into());
            return Ok(self.stop(format!("rescan is not clean: {why}")).await);
        }
        self.session = summary.session;
        Ok(Flow::Go)
    }
}

/// The two-phase automatic cycle (architecture 7.6, T5-11).
///
/// 1. consolidations; 2. rescan only the roots a live mutation touched (none in a dry-run);
/// 3. deletions from the fresh session. Stops and pauses at the first uncertainty.
pub async fn run_cycle(deps: &CycleDeps<'_>, session: SessionId) -> Result<JobSuccess, JobError> {
    let auto = &deps.ctx.auto;
    let mut cycle = Cycle {
        d: deps,
        session,
        failed_paths: BTreeSet::new(),
        touched: BTreeSet::new(),
        m: BTreeMap::new(),
    };
    // A1 before every phase, and before anything is recorded.
    if !auto.enabled() {
        return Ok(JobSuccess::default().with_metric("skipped", 1));
    }
    if let Some(why) = auto.paused() {
        deps.ctx
            .progress
            .warn(&format!("automatic mode is paused: {why}"));
        return Ok(JobSuccess::default().with_metric("skipped", 1));
    }
    if let Err(e) = auto.mark_cycle(session).await {
        auto.pause(&format!("cycle marker unwritable: {}", e.detail))
            .await;
        return Ok(JobSuccess::default().with_metric("paused", 1));
    }
    let outcome = async {
        if matches!(cycle.phase(AutoPhase::Consolidations).await?, Flow::Stop) {
            return Ok(());
        }
        if matches!(cycle.rescan().await?, Flow::Stop) {
            return Ok(());
        }
        cycle.phase(AutoPhase::Deletions).await.map(|_| ())
    }
    .await;
    // A normal end (including a stop by design) clears the marker; a crash never gets here.
    if let Err(e) = auto.clear_cycle().await {
        tracing::warn!(class = ?e.class, "could not clear the cycle marker");
    }
    outcome?;
    Ok(JobSuccess {
        metrics: cycle.m,
        scan: None,
    })
}
