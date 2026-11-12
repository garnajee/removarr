//! Shared fakes: a manual-clock-free `TokioClock`, an in-memory job and state store with the
//! storage crate's semantics, and a scripted job handler that tracks concurrency.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ids::JobId;
use removarr_domain::jobs::{JobKind, JobOutcome, JobRecord, JobStatus, Lane};
use removarr_domain::ports::{Clock, JobStore, StateStore};
use removarr_domain::time::Timestamp;
use removarr_jobs::backoff::{Jitter, NoJitter};
use removarr_jobs::config::JobsConfig;
use removarr_jobs::events::{JobEvent, JobEventKind};
use removarr_jobs::handler::{JobCtx, JobError, JobHandler, JobSuccess, RecoverySummary};
use removarr_jobs::locks::Scope;
use removarr_jobs::runtime::{Deps, JobRuntime};
use removarr_testkit::ids::SeqIds;
use removarr_workflows::actions::EventSink;
use tokio::sync::broadcast;

pub const DAY: i64 = 86_400_000;

/// Follows tokio's (possibly paused) time, so a virtual 24 hours is 24 hours for the clock too.
#[derive(Debug)]
pub struct TokioClock {
    base: i64,
    start: tokio::time::Instant,
}

impl TokioClock {
    pub fn at(base: i64) -> Arc<Self> {
        Arc::new(Self {
            base,
            start: tokio::time::Instant::now(),
        })
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.base + i64::try_from(self.start.elapsed().as_millis()).unwrap_or(0))
    }
}

/// In-memory `JobStore` with the semantics of the SQLite one (dedupe on active keys,
/// state-machine checked `finish`, claim in enqueue order). Counts every call.
#[derive(Default)]
pub struct MemStore {
    pub jobs: Mutex<Vec<JobRecord>>,
    pub calls: AtomicUsize,
    pub clock: Mutex<Option<Arc<dyn Clock>>>,
}

impl MemStore {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            clock: Mutex::new(Some(clock)),
            ..Self::default()
        })
    }

    fn now(&self) -> Timestamp {
        self.clock
            .lock()
            .unwrap()
            .as_ref()
            .map_or(Timestamp(0), |c| c.now())
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn all(&self) -> Vec<JobRecord> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn with_key(&self, key: &str) -> Vec<JobRecord> {
        self.all()
            .into_iter()
            .filter(|j| j.dedupe_key == key)
            .collect()
    }

    pub fn status(&self, id: JobId) -> Option<JobStatus> {
        self.all()
            .into_iter()
            .find(|j| j.id == id)
            .map(|j| j.status)
    }

    pub fn insert_raw(&self, kind: JobKind, status: JobStatus, id: u128) -> JobId {
        let id = JobId::new(removarr_domain::ids::RawUuid::from_u128(id));
        let mut r = JobRecord::queued(id, kind, self.now()).unwrap();
        r.status = status;
        if status == JobStatus::Running {
            r.started_at = Some(self.now());
            r.attempts = 1;
        }
        self.jobs.lock().unwrap().push(r);
        id
    }
}

#[async_trait]
impl JobStore for MemStore {
    async fn enqueue(&self, job: &JobRecord) -> Result<JobId, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs
            .iter()
            .find(|j| j.dedupe_key == job.dedupe_key && j.status.is_active())
        {
            return Ok(j.id);
        }
        jobs.push(job.clone());
        Ok(job.id)
    }

    async fn claim_next(&self, lane: Lane) -> Result<Option<JobRecord>, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.now();
        let mut jobs = self.jobs.lock().unwrap();
        let Some(j) = jobs
            .iter_mut()
            .find(|j| j.lane == lane && j.status == JobStatus::Queued)
        else {
            return Ok(None);
        };
        j.status = JobStatus::Running;
        j.started_at = Some(now);
        j.attempts += 1;
        Ok(Some(j.clone()))
    }

    async fn set_stage(&self, id: JobId, stage: &str) -> Result<(), PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut jobs = self.jobs.lock().unwrap();
        let j = jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| PortError::new(ErrorClass::NotFound, "job"))?;
        j.stage = Some(stage.to_string());
        Ok(())
    }

    async fn finish(&self, id: JobId, outcome: &JobOutcome) -> Result<(), PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.now();
        let mut jobs = self.jobs.lock().unwrap();
        let j = jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or_else(|| PortError::new(ErrorClass::NotFound, "job"))?;
        if !j.status.can_transition_to(outcome.status()) {
            return Err(PortError::new(
                ErrorClass::Conflict,
                "invalid job transition",
            ));
        }
        j.status = outcome.status();
        j.finished_at = Some(now);
        j.outcome = Some(outcome.clone());
        Ok(())
    }

    async fn interrupt_running(&self) -> Result<Vec<JobRecord>, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.now();
        let mut out = Vec::new();
        for j in self
            .jobs
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|j| j.status == JobStatus::Running)
        {
            j.status = JobStatus::Interrupted;
            j.finished_at = Some(now);
            j.outcome = Some(JobOutcome::Interrupted);
            out.push(j.clone());
        }
        Ok(out)
    }
}

#[derive(Default)]
pub struct MemState {
    pub map: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get(&self, key: &str) -> Result<Option<String>, PortError> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), PortError> {
        self.map
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

pub type Fut = Pin<Box<dyn Future<Output = Result<JobSuccess, JobError>> + Send>>;
type RunFn = dyn Fn(JobKind, JobCtx, u32) -> Fut + Send + Sync;
type ScopeFn = Arc<dyn Fn(&JobKind) -> Scope + Send + Sync>;

/// A handler whose behaviour is a closure per call. It records every start and end, the
/// calls of `recover`, and the maximum concurrency per lane.
pub struct Script {
    pub run: Mutex<Arc<RunFn>>,
    pub scope: Mutex<ScopeFn>,
    pub log: Mutex<Vec<String>>,
    pub recover_results: Mutex<VecDeque<Result<RecoverySummary, PortError>>>,
    pub recover_calls: AtomicU32,
    calls: Mutex<HashMap<String, u32>>,
    current: Mutex<BTreeMap<&'static str, usize>>,
    pub max: Mutex<BTreeMap<&'static str, usize>>,
}

impl Script {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            run: Mutex::new(Arc::new(|_, _, _| {
                Box::pin(async { Ok(JobSuccess::default()) })
            })),
            scope: Mutex::new(Arc::new(|_| Scope::default())),
            log: Mutex::new(Vec::new()),
            recover_results: Mutex::new(VecDeque::new()),
            recover_calls: AtomicU32::new(0),
            calls: Mutex::new(HashMap::new()),
            current: Mutex::new(BTreeMap::new()),
            max: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn on_run(&self, f: impl Fn(JobKind, JobCtx, u32) -> Fut + Send + Sync + 'static) {
        *self.run.lock().unwrap() = Arc::new(f);
    }

    pub fn on_scope(&self, f: impl Fn(&JobKind) -> Scope + Send + Sync + 'static) {
        *self.scope.lock().unwrap() = Arc::new(f);
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn max_of(&self, lane: &'static str) -> usize {
        self.max.lock().unwrap().get(lane).copied().unwrap_or(0)
    }

    pub fn calls_of(&self, key: &str) -> u32 {
        self.calls.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    fn enter(&self, lane: &'static str) {
        let mut cur = self.current.lock().unwrap();
        let c = cur.entry(lane).or_default();
        *c += 1;
        let mut max = self.max.lock().unwrap();
        let m = max.entry(lane).or_default();
        *m = (*m).max(*c);
    }

    fn leave(&self, lane: &'static str) {
        *self.current.lock().unwrap().entry(lane).or_default() -= 1;
    }
}

fn lane_name(l: Lane) -> &'static str {
    match l {
        Lane::Observe => "observe",
        Lane::Execute => "execute",
    }
}

#[async_trait]
impl JobHandler for Script {
    async fn scope(&self, kind: &JobKind) -> Result<Scope, JobError> {
        let f = Arc::clone(&self.scope.lock().unwrap());
        Ok(f(kind))
    }

    async fn run(&self, kind: &JobKind, ctx: &JobCtx) -> Result<JobSuccess, JobError> {
        let key = kind.dedupe_key().unwrap();
        let n = {
            let mut c = self.calls.lock().unwrap();
            let e = c.entry(key.clone()).or_default();
            *e += 1;
            *e
        };
        let lane = lane_name(kind.lane());
        self.enter(lane);
        self.log.lock().unwrap().push(format!("start:{key}"));
        let f = Arc::clone(&self.run.lock().unwrap());
        let r = f(kind.clone(), ctx.clone(), n).await;
        self.log.lock().unwrap().push(format!("end:{key}"));
        self.leave(lane);
        r
    }

    async fn recover(&self, _sink: EventSink) -> Result<RecoverySummary, PortError> {
        self.recover_calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push("recover".into());
        self.recover_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(RecoverySummary::default()))
    }
}

pub struct FixedJitter(pub f64);
impl Jitter for FixedJitter {
    fn unit(&self) -> f64 {
        self.0
    }
}

pub struct Rig {
    pub rt: JobRuntime,
    pub store: Arc<MemStore>,
    pub state: Arc<MemState>,
    pub script: Arc<Script>,
    pub clock: Arc<TokioClock>,
}

pub fn deps(
    store: &Arc<MemStore>,
    state: &Arc<MemState>,
    script: &Arc<Script>,
    clock: &Arc<TokioClock>,
    jitter: Arc<dyn Jitter>,
) -> Deps {
    Deps {
        store: store.clone(),
        state: state.clone(),
        handler: script.clone(),
        clock: clock.clone(),
        ids: Arc::new(SeqIds::new()),
        jitter,
    }
}

/// A rig with no automatic schedule. Must run inside a tokio runtime.
pub fn rig(cfg: JobsConfig) -> Rig {
    rig_with(cfg, Arc::new(NoJitter), 1_700_000_000_000)
}

pub fn rig_with(cfg: JobsConfig, jitter: Arc<dyn Jitter>, base: i64) -> Rig {
    let clock = TokioClock::at(base);
    let store = MemStore::new(clock.clone());
    let state = Arc::new(MemState::default());
    let script = Script::new();
    let rt = JobRuntime::new(cfg, deps(&store, &state, &script, &clock, jitter));
    Rig {
        rt,
        store,
        state,
        script,
        clock,
    }
}

pub fn manual_only() -> JobsConfig {
    let mut cfg = JobsConfig::default();
    cfg.schedule.scan_interval_minutes = 0;
    cfg.schedule.qbit_refresh_minutes = 0;
    cfg
}

/// Lets spawned tasks run (virtual time advances only when everything is idle).
pub async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Polls a condition on virtual time. Fails the test instead of hanging.
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

pub fn finished_events(rx: &mut broadcast::Receiver<JobEvent>) -> Vec<(JobId, JobStatus)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let (Some(id), JobEventKind::Finished { status, .. }) = (e.job, e.kind) {
            out.push((id, status));
        }
    }
    out
}

pub fn exec_kind(n: u128, simulate: bool) -> JobKind {
    JobKind::ExecutePlan {
        plan_id: removarr_domain::ids::PlanId::new(removarr_domain::ids::RawUuid::from_u128(n)),
        simulate,
    }
}

impl TokioClock {
    pub fn now_ts(&self) -> Timestamp {
        removarr_domain::ports::Clock::now(self)
    }
}

impl MemState {
    pub fn get_raw(&self, key: &str) -> Option<String> {
        self.map.lock().unwrap().get(key).cloned()
    }
}
