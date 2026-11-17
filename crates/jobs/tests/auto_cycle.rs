#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The automatic cycle (tier 5, architecture 7.6): phases, rescan scope, pause conditions,
//! failed sets from the journal (T3-12, T5-10), ignore list, cancellation and crash marker.
//! The runner is a fake: its real behaviour is covered by `actions_tier5.rs` in workflows.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{Completeness, EvidenceLevel};
use removarr_domain::gain::GainEstimate;
use removarr_domain::ids::{ActionRef, IntentId, JobId, PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{ActionKind, ActionTarget, PlannedAction};
use removarr_domain::ports::{CancelFlag, StateStore};
use removarr_domain::time::Timestamp;
use removarr_jobs::auto::{
    ActionFacts, ActionHistory, AutoControl, CycleDeps, CycleEngine, ExecutedAction, KEY_CYCLE,
    KEY_PAUSED, PhaseError, PhasePlan, PhasePlans, Refusal, RescanSummary, Verdict, assess,
    classify_blockers, failed_sets, run_cycle,
};
use removarr_jobs::events::{Hub, ProgressHandle};
use removarr_jobs::handler::{JobCtx, JobError, RecoverySummary};
use removarr_jobs::limits::Limits;
use removarr_jobs::locks::{Locks, Scope};
use removarr_workflows::actions::{ActionEvent, ActionResult, EventSink, ExecutionReport};
use removarr_workflows::plans::{AutoContext, AutoPhase};
use tokio_util::sync::CancellationToken;

fn pid(n: u128) -> PlanId {
    PlanId::new(RawUuid::from_u128(n))
}
fn sid(n: u128) -> SessionId {
    SessionId::new(RawUuid::from_u128(n))
}
fn rp(root: &str, rel: &str) -> RootedPath {
    RootedPath::parse(root, rel).unwrap()
}
const MEDIA: &str = "media-00000001";

fn consolidation_action(rel: &str) -> PlannedAction {
    PlannedAction {
        index: 0,
        kind: ActionKind::ConsolidateHardlink,
        target: ActionTarget::Consolidate {
            source: rp("completed", rel),
            target: rp(MEDIA, rel),
        },
        preconditions: vec![],
        verification: vec![],
        expected_gain: GainEstimate::default(),
        depends_on: vec![],
        torrent_hashes: BTreeSet::new(),
        evidence_level: EvidenceLevel::Full,
        completeness: Completeness::Complete,
    }
}

fn phase_plan(n: u128, kind: ActionKind, rel: &str) -> PhasePlan {
    let paths: BTreeSet<RootedPath> = [rp("completed", rel), rp(MEDIA, rel)].into();
    PhasePlan {
        plan: pid(n),
        live_scope: Scope {
            exclusive: ["root:completed".to_string(), format!("root:{MEDIA}")].into(),
            ..Scope::default()
        },
        read_scope: Scope {
            shared: ["root:completed".to_string(), format!("root:{MEDIA}")].into(),
            ..Scope::default()
        },
        actions: vec![ActionFacts {
            index: 0,
            kind,
            paths,
            subjects: [SubjectKey(format!("p:completed:{rel}"))].into(),
        }],
    }
}

fn report(
    plan: u128,
    kind: ActionKind,
    outcome: ActionOutcome,
    simulated: bool,
    done_step: bool,
) -> ExecutionReport {
    let events = if done_step {
        vec![ActionEvent::StepResult {
            action: ActionRef {
                plan: pid(plan),
                index: 0,
            },
            step: JournalStep::FsReplaceWithHardlink,
            intent: IntentId::new(RawUuid::nil()),
            outcome: StepOutcome::Done {
                final_state: serde_json::Value::Null,
            },
        }]
    } else {
        vec![]
    };
    ExecutionReport {
        plan: pid(plan),
        simulated,
        status: None,
        actions: vec![ActionResult {
            index: 0,
            kind,
            outcome,
            verification: vec![],
        }],
        left_stopped: BTreeMap::new(),
        interrupted: false,
        events,
    }
}

struct Engine {
    simulate: bool,
    hang: bool,
    log: Mutex<Vec<String>>,
    phases: Mutex<HashMap<String, Result<PhasePlans, PhaseError>>>,
    ctxs: Mutex<Vec<(String, AutoContext, SessionId)>>,
    reports: Mutex<HashMap<PlanId, Result<ExecutionReport, PortError>>>,
    recover: Mutex<VecDeque<Result<RecoverySummary, PortError>>>,
    ignored: Mutex<Result<BTreeSet<SubjectKey>, PortError>>,
    rescan: Mutex<Result<RescanSummary, PortError>>,
    rescan_roots: Mutex<Vec<BTreeSet<RootId>>>,
    discarded: Mutex<Vec<PlanId>>,
    marker_seen: Mutex<Vec<Option<String>>>,
    state: Arc<MemState>,
    simulate_flags: Mutex<Vec<bool>>,
    auto_flags: Mutex<Vec<bool>>,
    pending_discards: Mutex<u32>,
}

impl Engine {
    fn new(state: Arc<MemState>) -> Self {
        Engine {
            simulate: false,
            hang: false,
            log: Mutex::default(),
            phases: Mutex::default(),
            ctxs: Mutex::default(),
            reports: Mutex::default(),
            recover: Mutex::default(),
            ignored: Mutex::new(Ok(BTreeSet::new())),
            rescan: Mutex::new(Ok(RescanSummary {
                session: sid(2),
                complete: true,
                uncertainties: vec![],
            })),
            rescan_roots: Mutex::default(),
            discarded: Mutex::default(),
            marker_seen: Mutex::default(),
            state,
            simulate_flags: Mutex::default(),
            auto_flags: Mutex::default(),
            pending_discards: Mutex::new(0),
        }
    }
    fn phase(&self, p: AutoPhase, plans: Vec<PhasePlan>) {
        self.phases.lock().unwrap().insert(
            format!("{p:?}"),
            Ok(PhasePlans {
                plans,
                refused: vec![],
            }),
        );
    }
    fn report(&self, r: ExecutionReport) {
        self.reports.lock().unwrap().insert(r.plan, Ok(r));
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl CycleEngine for Engine {
    fn simulate(&self) -> bool {
        self.simulate
    }
    async fn plan_phase(
        &self,
        phase: AutoPhase,
        ctx: &AutoContext,
        session: SessionId,
    ) -> Result<PhasePlans, PhaseError> {
        self.log.lock().unwrap().push(format!("plan:{phase:?}"));
        self.ctxs
            .lock()
            .unwrap()
            .push((format!("{phase:?}"), ctx.clone(), session));
        self.phases
            .lock()
            .unwrap()
            .remove(&format!("{phase:?}"))
            .unwrap_or(Ok(PhasePlans::default()))
    }
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        self.ignored.lock().unwrap().clone()
    }
    async fn discard(&self, plan: PlanId) -> Result<(), PortError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("discard:{}", plan.0.as_u128()));
        self.discarded.lock().unwrap().push(plan);
        Ok(())
    }
    async fn discard_pending(&self) -> Result<u32, PortError> {
        *self.pending_discards.lock().unwrap() += 1;
        Ok(0)
    }
    async fn recover(&self, _sink: EventSink) -> Result<RecoverySummary, PortError> {
        self.log.lock().unwrap().push("recover".into());
        self.recover
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(RecoverySummary::default()))
    }
    async fn execute(
        &self,
        plan: PlanId,
        simulate: bool,
        auto_active: bool,
        _c: &CancelFlag,
        _s: EventSink,
    ) -> Result<ExecutionReport, PortError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("execute:{}", plan.0.as_u128()));
        self.simulate_flags.lock().unwrap().push(simulate);
        self.auto_flags.lock().unwrap().push(auto_active);
        let marker = self.state.get(KEY_CYCLE).await.unwrap();
        self.marker_seen.lock().unwrap().push(marker);
        if self.hang {
            std::future::pending::<()>().await;
        }
        self.reports
            .lock()
            .unwrap()
            .remove(&plan)
            .unwrap_or_else(|| Err(PortError::new(ErrorClass::Internal, "no scripted report")))
    }
    async fn rescan(
        &self,
        roots: &BTreeSet<RootId>,
        _c: &CancelFlag,
    ) -> Result<RescanSummary, PortError> {
        self.log.lock().unwrap().push("rescan".into());
        self.rescan_roots.lock().unwrap().push(roots.clone());
        self.rescan.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Hist(Mutex<Vec<ExecutedAction>>);

#[async_trait]
impl ActionHistory for Hist {
    async fn executed_since(&self, since: Timestamp) -> Result<Vec<ExecutedAction>, PortError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.at >= since)
            .cloned()
            .collect())
    }
}

struct H {
    engine: Engine,
    hist: Hist,
    state: Arc<MemState>,
    auto: Arc<AutoControl>,
    ctx: JobCtx,
    clock: Arc<TokioClock>,
}

fn harness(auto_on: bool) -> H {
    let clock = TokioClock::at(1_700_000_000_000);
    let store = MemStore::new(clock.clone());
    let state = Arc::new(MemState::default());
    let hub = Hub::new(clock.clone());
    let auto = Arc::new(AutoControl::new(auto_on, state.clone(), hub.clone()));
    let ctx = JobCtx {
        id: JobId::new(RawUuid::from_u128(1)),
        cancel: CancelFlag::new(),
        token: CancellationToken::new(),
        progress: ProgressHandle::new(JobId::new(RawUuid::from_u128(1)), hub, store),
        limits: Arc::new(Limits::new(&Default::default())),
        locks: Arc::new(Locks::new()),
        clock: clock.clone(),
        auto: auto.clone(),
    };
    H {
        engine: Engine::new(state.clone()),
        hist: Hist::default(),
        state,
        auto,
        ctx,
        clock,
    }
}

async fn cycle(h: &H) -> Result<removarr_jobs::JobSuccess, JobError> {
    run_cycle(
        &CycleDeps {
            engine: &h.engine,
            history: &h.hist,
            ctx: &h.ctx,
        },
        sid(1),
    )
    .await
}

async fn paused(h: &H) -> Option<String> {
    let v = h
        .state
        .get(KEY_PAUSED)
        .await
        .unwrap()
        .filter(|s| !s.is_empty());
    assert_eq!(v, h.auto.paused(), "memory and persisted latch agree");
    v
}

fn ok_consolidation(n: u128) -> ExecutionReport {
    report(
        n,
        ActionKind::ConsolidateHardlink,
        ActionOutcome::Verified,
        false,
        true,
    )
}

#[tokio::test(start_paused = true)]
async fn off_or_paused_does_nothing_at_all() {
    let h = harness(false);
    let r = cycle(&h).await.unwrap();
    assert_eq!(r.metrics["skipped"], 1);
    assert!(h.engine.log().is_empty());
    assert_eq!(
        h.state.get(KEY_CYCLE).await.unwrap(),
        None,
        "no marker for a cycle that never started"
    );

    let h = harness(true);
    h.auto.pause("earlier uncertainty").await;
    cycle(&h).await.unwrap();
    assert!(h.engine.log().is_empty(), "A1: nothing runs while paused");
    assert_eq!(paused(&h).await.as_deref(), Some("earlier uncertainty"));
}

#[tokio::test(start_paused = true)]
async fn two_phases_with_a_rescan_of_only_the_mutated_roots_between_them() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    h.engine.report(ok_consolidation(10));
    h.engine.phase(
        AutoPhase::Deletions,
        vec![phase_plan(20, ActionKind::DeleteCompletedFile, "m/b.mkv")],
    );
    h.engine.report(report(
        20,
        ActionKind::DeleteCompletedFile,
        ActionOutcome::Verified,
        false,
        true,
    ));
    let r = cycle(&h).await.unwrap();
    assert_eq!(
        h.engine.log(),
        [
            "plan:Consolidations",
            "recover",
            "execute:10",
            "rescan",
            "plan:Deletions",
            "recover",
            "execute:20"
        ]
    );
    // The rescan covers the roots of the live mutation, phase 2 plans from the new session.
    assert_eq!(
        h.engine.rescan_roots.lock().unwrap()[0],
        BTreeSet::from([RootId::completed(), RootId::new(MEDIA).unwrap()])
    );
    {
        let ctxs = h.engine.ctxs.lock().unwrap();
        assert_eq!((ctxs[0].2, ctxs[1].2), (sid(1), sid(2)));
    }
    assert_eq!(
        (
            r.metrics["rescans"],
            r.metrics["plans_executed"],
            r.metrics["actions_verified"]
        ),
        (1, 2, 2)
    );
    // The marker was set while executing, and cleared at the end; nothing is paused.
    assert_eq!(
        *h.engine.marker_seen.lock().unwrap(),
        [Some(sid(1).to_string()), Some(sid(1).to_string())]
    );
    assert_eq!(h.state.get(KEY_CYCLE).await.unwrap().as_deref(), Some(""));
    assert_eq!(paused(&h).await, None);
    assert!(h.engine.discarded.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn no_live_mutation_means_no_rescan_and_phase_two_reuses_the_session() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    // Revalidation refused it: not executed, so nothing changed on disk.
    h.engine.report(report(
        10,
        ActionKind::ConsolidateHardlink,
        ActionOutcome::NotExecuted { blockers: vec![] },
        false,
        false,
    ));
    cycle(&h).await.unwrap();
    assert!(!h.engine.log().contains(&"rescan".to_string()));
    assert_eq!(h.engine.ctxs.lock().unwrap()[1].2, sid(1));
    assert_eq!(
        paused(&h).await,
        None,
        "NotExecuted by revalidation does not stop the cycle"
    );
}

#[tokio::test(start_paused = true)]
async fn a_dry_run_cycle_simulates_every_plan_and_never_rescans() {
    let mut h = harness(true);
    h.engine.simulate = true;
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    h.engine.report(report(
        10,
        ActionKind::ConsolidateHardlink,
        ActionOutcome::Simulated,
        true,
        false,
    ));
    h.engine.phase(
        AutoPhase::Deletions,
        vec![phase_plan(20, ActionKind::DeleteCompletedFile, "m/b.mkv")],
    );
    h.engine.report(report(
        20,
        ActionKind::DeleteCompletedFile,
        ActionOutcome::Simulated,
        true,
        false,
    ));
    cycle(&h).await.unwrap();
    assert_eq!(*h.engine.simulate_flags.lock().unwrap(), [true, true]);
    assert!(!h.engine.log().contains(&"rescan".to_string()));
    assert_eq!(h.engine.ctxs.lock().unwrap()[1].2, sid(1));
    assert_eq!(paused(&h).await, None);
}

#[tokio::test(start_paused = true)]
async fn every_uncertainty_stops_the_cycle_and_pauses_with_a_reason() {
    type Setup = Box<dyn Fn(&H)>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "executed unverified",
            Box::new(|h| {
                h.engine.report(report(
                    10,
                    ActionKind::ConsolidateHardlink,
                    ActionOutcome::ExecutedUnverified { reason: "x".into() },
                    false,
                    true,
                ))
            }),
        ),
        (
            "failed needs review",
            Box::new(|h| {
                h.engine.report(report(
                    10,
                    ActionKind::ConsolidateHardlink,
                    ActionOutcome::FailedNeedsReview,
                    false,
                    true,
                ))
            }),
        ),
        (
            "failed",
            Box::new(|h| {
                h.engine.report(report(
                    10,
                    ActionKind::ConsolidateHardlink,
                    ActionOutcome::Failed {
                        class: ErrorClass::Unavailable,
                    },
                    false,
                    false,
                ))
            }),
        ),
        (
            "interrupted",
            Box::new(|h| {
                let mut r = ok_consolidation(10);
                r.interrupted = true;
                h.engine.report(r);
            }),
        ),
        (
            "left stopped",
            Box::new(|h| {
                let mut r = ok_consolidation(10);
                r.left_stopped.insert(
                    removarr_domain::torrent::TorrentHash::new("a".repeat(40)).unwrap(),
                    removarr_workflows::actions::LeftStoppedReason::RecheckTimedOut,
                );
                h.engine.report(r);
            }),
        ),
        (
            "recovery needed",
            Box::new(|h| {
                h.engine.report(ok_consolidation(10));
                h.engine
                    .recover
                    .lock()
                    .unwrap()
                    .push_back(Ok(RecoverySummary {
                        plans: 1,
                        left_stopped: 0,
                        notes: vec![],
                    }));
            }),
        ),
        (
            "recovery unresolved",
            Box::new(|h| {
                h.engine.report(ok_consolidation(10));
                h.engine
                    .recover
                    .lock()
                    .unwrap()
                    .push_back(Err(PortError::new(ErrorClass::Unavailable, "qbit down")));
            }),
        ),
        (
            "execute error",
            Box::new(|h| {
                h.engine
                    .reports
                    .lock()
                    .unwrap()
                    .insert(pid(10), Err(PortError::new(ErrorClass::Storage, "journal")));
            }),
        ),
        (
            "rescan with errors",
            Box::new(|h| {
                h.engine.report(ok_consolidation(10));
                *h.engine.rescan.lock().unwrap() = Ok(RescanSummary {
                    session: sid(2),
                    complete: false,
                    uncertainties: vec!["root completed has 2 scan errors".into()],
                });
            }),
        ),
        (
            "rescan service down",
            Box::new(|h| {
                h.engine.report(ok_consolidation(10));
                *h.engine.rescan.lock().unwrap() = Ok(RescanSummary {
                    session: sid(2),
                    complete: true,
                    uncertainties: vec!["ServiceUnavailable(Sonarr)".into()],
                });
            }),
        ),
        (
            "rescan error",
            Box::new(|h| {
                h.engine.report(ok_consolidation(10));
                *h.engine.rescan.lock().unwrap() =
                    Err(PortError::new(ErrorClass::Unavailable, "db"));
            }),
        ),
    ];
    for (name, setup) in cases {
        let h = harness(true);
        h.engine.phase(
            AutoPhase::Consolidations,
            vec![
                phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv"),
                phase_plan(11, ActionKind::ConsolidateHardlink, "m/b.mkv"),
            ],
        );
        h.engine.phase(
            AutoPhase::Deletions,
            vec![phase_plan(20, ActionKind::DeleteCompletedFile, "m/c.mkv")],
        );
        h.engine.report(ok_consolidation(11)); // runs only when the stop comes after phase 1
        setup(&h);
        let r = cycle(&h).await.unwrap();
        assert_eq!(r.metrics["paused"], 1, "{name}");
        assert!(
            paused(&h).await.is_some(),
            "{name}: auto_paused must be persisted with a reason"
        );
        let log = h.engine.log();
        assert!(
            !log.contains(&"plan:Deletions".to_string()),
            "{name}: phase 2 must not start: {log:?}"
        );
        let after_rescan = name.starts_with("rescan");
        assert!(
            after_rescan || !log.contains(&"execute:11".to_string()),
            "{name}: the next plan must not run: {log:?}"
        );
        assert!(
            h.engine.discarded.lock().unwrap().contains(&pid(11))
                || log.contains(&"rescan".to_string()),
            "{name}: unused plans are discarded"
        );
        assert_eq!(
            h.state.get(KEY_CYCLE).await.unwrap().as_deref(),
            Some(""),
            "{name}: a stop by design clears the marker"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_execute_error_triggers_recovery_before_the_pause() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    h.engine
        .reports
        .lock()
        .unwrap()
        .insert(pid(10), Err(PortError::new(ErrorClass::Storage, "journal")));
    cycle(&h).await.unwrap();
    assert_eq!(
        h.engine.log(),
        ["plan:Consolidations", "recover", "execute:10", "recover"]
    );
}

#[tokio::test(start_paused = true)]
async fn planning_failures_and_policy_errors_stop_the_cycle_but_a_refused_a1_does_not_pause() {
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Err(PhaseError::Failed("db busy".into())),
    );
    cycle(&h).await.unwrap();
    assert!(paused(&h).await.unwrap().contains("planning failed"));

    // A non-benign refusal (a policy defect) stops it and discards what was approved.
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Ok(PhasePlans {
            plans: vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
            refused: vec![Refusal {
                reason: "plan lacks the ARR pin".into(),
                benign: false,
                blockers: vec![],
            }],
        }),
    );
    cycle(&h).await.unwrap();
    assert!(paused(&h).await.unwrap().contains("policy refusal"));
    assert_eq!(*h.engine.discarded.lock().unwrap(), [pid(10)]);
    assert!(!h.engine.log().iter().any(|l| l.starts_with("execute")));

    // Benign exclusions ("n item(s) excluded") are normal.
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Ok(PhasePlans {
            plans: vec![],
            refused: vec![Refusal {
                reason: "3 item(s) excluded".into(),
                benign: true,
                blockers: vec![],
            }],
        }),
    );
    cycle(&h).await.unwrap();
    assert_eq!(paused(&h).await, None);

    // The planner itself refusing (auto off/paused between the check and the call) is a skip.
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Err(PhaseError::Refused("paused".into())),
    );
    let r = cycle(&h).await.unwrap();
    assert_eq!(r.metrics["skipped"], 1);
    assert_eq!(paused(&h).await, None);
}

#[tokio::test(start_paused = true)]
async fn a_failed_consolidation_is_never_followed_by_a_deletion_of_its_completed_file() {
    // T3-12 / T5-10: the consolidation was refused at revalidation, so its paths enter the
    // failed set that phase 2 is planned with.
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![
            phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv"),
            phase_plan(11, ActionKind::ConsolidateHardlink, "m/b.mkv"),
        ],
    );
    h.engine.report(report(
        10,
        ActionKind::ConsolidateHardlink,
        ActionOutcome::NotExecuted { blockers: vec![] },
        false,
        false,
    ));
    h.engine.report(ok_consolidation(11));
    cycle(&h).await.unwrap();
    let ctxs = h.engine.ctxs.lock().unwrap();
    let phase2 = &ctxs[1].1;
    assert!(phase2.failed_paths.contains(&rp("completed", "m/a.mkv")));
    assert!(phase2.failed_paths.contains(&rp(MEDIA, "m/a.mkv")));
    assert!(
        !phase2.failed_paths.contains(&rp("completed", "m/b.mkv")),
        "a verified consolidation is not excluded"
    );
    assert!(phase2.auto_mode && phase2.auto_paused.is_none());
}

#[tokio::test(start_paused = true)]
async fn failed_sets_are_rebuilt_from_the_journal_within_seven_days() {
    let h = harness(true);
    let now = h.clock.now_ts();
    let day = 86_400_000;
    let mk = |rel: &str, at: Timestamp, outcome: Option<ActionOutcome>, kind: ActionKind| {
        let mut action = consolidation_action(rel);
        action.kind = kind;
        if kind != ActionKind::ConsolidateHardlink {
            action.target = ActionTarget::Path {
                path: rp("completed", rel),
            };
        }
        ExecutedAction {
            plan: pid(1),
            at,
            action,
            outcome,
        }
    };
    *h.hist.0.lock().unwrap() = vec![
        mk(
            "fail/unverified.mkv",
            Timestamp(now.0 - day),
            Some(ActionOutcome::ExecutedUnverified { reason: "r".into() }),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "fail/review.mkv",
            Timestamp(now.0 - 2 * day),
            Some(ActionOutcome::FailedNeedsReview),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "fail/open.mkv",
            Timestamp(now.0 - 3 * day),
            None,
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "fail/notexec.mkv",
            Timestamp(now.0 - 3 * day),
            Some(ActionOutcome::NotExecuted { blockers: vec![] }),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "ok/verified.mkv",
            Timestamp(now.0 - day),
            Some(ActionOutcome::Verified),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "ok/simulated.mkv",
            Timestamp(now.0 - day),
            Some(ActionOutcome::Simulated),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "old/ancient.mkv",
            Timestamp(now.0 - 8 * day),
            Some(ActionOutcome::FailedNeedsReview),
            ActionKind::ConsolidateHardlink,
        ),
        mk(
            "fail/deletion.mkv",
            Timestamp(now.0 - day),
            Some(ActionOutcome::Failed {
                class: ErrorClass::Io(removarr_domain::fs::IoKind::Other),
            }),
            ActionKind::DeleteCompletedFile,
        ),
    ];
    let sets = failed_sets(&h.hist, now).await.unwrap();
    for rel in [
        "fail/unverified.mkv",
        "fail/review.mkv",
        "fail/open.mkv",
        "fail/notexec.mkv",
    ] {
        assert!(sets.paths.contains(&rp("completed", rel)), "{rel}");
        assert!(sets.paths.contains(&rp(MEDIA, rel)), "{rel}");
        assert!(
            sets.subjects
                .contains(&SubjectKey(format!("p:completed:{rel}"))),
            "{rel}"
        );
    }
    for rel in ["ok/verified.mkv", "ok/simulated.mkv", "old/ancient.mkv"] {
        assert!(!sets.paths.contains(&rp("completed", rel)), "{rel}");
        assert!(
            !sets
                .subjects
                .contains(&SubjectKey(format!("p:completed:{rel}"))),
            "{rel}"
        );
    }
    // A failed deletion excludes its subject but is not a consolidation path.
    assert!(
        sets.subjects
            .contains(&SubjectKey("p:completed:fail/deletion.mkv".into()))
    );
    assert!(!sets.paths.contains(&rp("completed", "fail/deletion.mkv")));
    // And the cycle hands exactly these to the planner.
    h.engine.phase(AutoPhase::Consolidations, vec![]);
    cycle(&h).await.unwrap();
    assert_eq!(h.engine.ctxs.lock().unwrap()[0].1.failed_paths, sets.paths);
    assert_eq!(
        h.engine.ctxs.lock().unwrap()[0].1.failed_subjects,
        sets.subjects
    );
}

#[tokio::test(start_paused = true)]
async fn an_item_ignored_after_planning_is_never_executed() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![
            phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv"),
            phase_plan(11, ActionKind::ConsolidateHardlink, "m/b.mkv"),
        ],
    );
    h.engine.report(ok_consolidation(11));
    *h.engine.ignored.lock().unwrap() = Ok([SubjectKey("p:completed:m/a.mkv".into())].into());
    let r = cycle(&h).await.unwrap();
    assert_eq!(r.metrics["plans_skipped_ignored"], 1);
    assert_eq!(
        *h.engine.discarded.lock().unwrap(),
        [pid(10)],
        "an unused automatic approval must not stay executable"
    );
    assert!(!h.engine.log().contains(&"execute:10".to_string()));
    assert!(h.engine.log().contains(&"execute:11".to_string()));
    assert_eq!(paused(&h).await, None);

    // An unreadable ignore list fails closed.
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    *h.engine.ignored.lock().unwrap() = Err(PortError::new(ErrorClass::Storage, "db"));
    cycle(&h).await.unwrap();
    assert!(paused(&h).await.unwrap().contains("ignore list unreadable"));
    assert!(!h.engine.log().iter().any(|l| l.starts_with("execute")));
}

#[tokio::test(start_paused = true)]
async fn cancelling_between_plans_discards_the_rest_without_pausing() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![
            phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv"),
            phase_plan(11, ActionKind::ConsolidateHardlink, "m/b.mkv"),
        ],
    );
    h.engine.report(ok_consolidation(10));
    h.ctx.cancel.cancel();
    let r = cycle(&h).await;
    assert_eq!(r.unwrap_err(), JobError::Cancelled);
    assert!(
        h.engine.log().iter().all(|l| !l.starts_with("execute")),
        "checked before the first plan"
    );
    assert_eq!(h.state.get(KEY_CYCLE).await.unwrap().as_deref(), Some(""));
    assert_eq!(
        paused(&h).await,
        None,
        "a user cancel is not an uncertainty"
    );
}

#[tokio::test(start_paused = true)]
async fn a_crash_inside_the_cycle_leaves_the_marker_and_the_next_start_pauses() {
    // T5-16: the process dies while a plan executes (the task is killed, nothing finishes).
    let mut h = harness(true);
    h.engine.hang = true;
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    let h = Arc::new(h);
    let h2 = Arc::clone(&h);
    let task = tokio::spawn(async move { cycle(&h2).await.map(|_| ()) });
    until("executing", || {
        h.engine.log().iter().any(|l| l.starts_with("execute"))
    })
    .await;
    task.abort();
    let _ = task.await;
    assert_eq!(
        h.state.get(KEY_CYCLE).await.unwrap().as_deref(),
        Some(sid(1).to_string().as_str())
    );
    assert_eq!(paused(&h).await, None, "nothing records the crash yet");
    // The next process: the marker pauses automatic mode, and nothing restarts by itself.
    let mut cfg = manual_only();
    cfg.auto_mode = true;
    let script = Script::new();
    let store = MemStore::new(h.clock.clone());
    let rt = removarr_jobs::JobRuntime::new(
        cfg,
        deps(
            &store,
            &h.state,
            &script,
            &h.clock,
            Arc::new(removarr_jobs::backoff::NoJitter),
        ),
    );
    let report = rt.start().await.unwrap();
    assert!(report.auto_paused.is_some());
    settle().await;
    assert!(script.log().iter().all(|l| l == "recover"));
    assert!(store.all().is_empty());
}

#[test]
fn assess_applies_the_t5_08_rule_per_outcome() {
    let plan = phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv");
    let a = assess(&plan, &ok_consolidation(10));
    assert_eq!((a.stop.clone(), a.verified), (None, 1));
    assert_eq!(
        a.touched,
        BTreeSet::from([RootId::completed(), RootId::new(MEDIA).unwrap()])
    );
    // A qBittorrent-only step touches no root.
    let mut r = ok_consolidation(10);
    if let ActionEvent::StepResult { step, .. } = &mut r.events[0] {
        *step = JournalStep::QbitStop;
    }
    assert!(assess(&plan, &r).touched.is_empty());
    // A simulation never reports touched roots even if events were present.
    let mut r = ok_consolidation(10);
    r.simulated = true;
    assert!(assess(&plan, &r).touched.is_empty());
    for bad in [
        ActionOutcome::ExecutedUnverified { reason: "x".into() },
        ActionOutcome::FailedNeedsReview,
        ActionOutcome::Failed {
            class: ErrorClass::Internal,
        },
        ActionOutcome::Compensated,
    ] {
        let a = assess(
            &plan,
            &report(10, ActionKind::ConsolidateHardlink, bad, false, false),
        );
        assert!(a.stop.is_some());
        assert!(a.failed_paths.contains(&rp("completed", "m/a.mkv")));
    }
}

fn blocker(code: removarr_domain::issue::BlockerCode, d: &str) -> removarr_domain::issue::Blocker {
    removarr_domain::issue::Blocker::new(code, d)
}

fn pause_class_blockers() -> Vec<(&'static str, removarr_domain::issue::Blocker)> {
    use removarr_domain::issue::BlockerCode as B;
    use removarr_domain::session::ServiceKind as S;
    vec![
        (
            "qbittorrent down",
            blocker(
                B::ServiceUnavailable(S::Qbittorrent),
                "qbittorrent did not answer",
            ),
        ),
        (
            "arr down",
            blocker(B::ServiceUnavailable(S::Sonarr), "sonarr did not answer"),
        ),
        (
            "service not configured",
            blocker(B::ServiceNotConfigured(S::Qbittorrent), ""),
        ),
        ("mapping", blocker(B::MappingError, "no rule")),
        (
            "scan incomplete",
            blocker(B::ScanIncomplete, "root not finished"),
        ),
        ("scan errors", blocker(B::ScanErrorUnder, "2 scan errors")),
        (
            "ignore list unreadable",
            blocker(B::Ignored, "ignore list unreadable: db"),
        ),
        ("arr ambiguous", blocker(B::ArrAmbiguous, "ARR unreadable")),
        (
            "capability",
            blocker(
                B::CapabilityDisabled,
                "automatic mode is disabled or paused",
            ),
        ),
        (
            "config drift",
            blocker(
                B::ChangedSincePlanning,
                "configuration changed since the plan was built; re-plan",
            ),
        ),
        (
            "missing pin",
            blocker(
                B::ChangedSincePlanning,
                "the plan lacks the wanted-file pin of torrent x",
            ),
        ),
        ("not in build", blocker(B::NotInBuild, "tier")),
        (
            "unknown code fails closed",
            blocker(B::Symlink, "anything else"),
        ),
        ("interrupted", blocker(B::Interrupted, "batch stopped")),
    ]
}

#[test]
fn f01_every_uncertainty_or_defect_code_pauses_and_ordinary_changes_continue() {
    use removarr_domain::issue::BlockerCode as B;
    for (name, b) in pause_class_blockers() {
        assert!(
            matches!(classify_blockers(&[b]), Verdict::Pause(_)),
            "{name}"
        );
    }
    for (name, b) in [
        (
            "entry changed",
            blocker(
                B::ChangedSincePlanning,
                "entry changed since the plan was built: completed:a",
            ),
        ),
        (
            "torrent protected",
            blocker(B::TorrentProtectedState, "now checking"),
        ),
        (
            "torrent references",
            blocker(B::TorrentReferencesTarget, ""),
        ),
        (
            "too young",
            blocker(B::Recent, "younger than the minimum age"),
        ),
        (
            "ignored by the user",
            blocker(B::Ignored, "the subject is on the ignore list"),
        ),
        ("import in progress", blocker(B::ImportInProgress, "")),
        (
            "arr now upgrades",
            blocker(
                B::ArrNotUpgradeEvidence,
                "ARR no longer rejects x as not an upgrade",
            ),
        ),
    ] {
        assert_eq!(classify_blockers(&[b]), Verdict::Ordinary, "{name}");
    }
    // One bad blocker among ordinary ones still pauses.
    let mixed = [blocker(B::Recent, ""), blocker(B::MappingError, "x")];
    assert!(matches!(classify_blockers(&mixed), Verdict::Pause(_)));
}

#[tokio::test(start_paused = true)]
async fn f01_each_pause_class_stops_the_cycle_end_to_end_and_an_ordinary_refusal_continues() {
    use removarr_domain::issue::BlockerCode as B;
    for (name, b) in pause_class_blockers() {
        let h = harness(true);
        h.engine.phase(
            AutoPhase::Consolidations,
            vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
        );
        h.engine.phase(
            AutoPhase::Deletions,
            vec![phase_plan(20, ActionKind::DeleteCompletedFile, "m/c.mkv")],
        );
        h.engine.report(report(
            10,
            ActionKind::ConsolidateHardlink,
            ActionOutcome::NotExecuted { blockers: vec![b] },
            false,
            false,
        ));
        let r = cycle(&h).await.unwrap();
        assert_eq!(r.metrics["paused"], 1, "{name}");
        let why = paused(&h).await.unwrap_or_default();
        assert!(
            why.contains("was refused"),
            "{name}: the reason is recorded: {why}"
        );
        assert!(
            !h.engine.log().contains(&"plan:Deletions".to_string()),
            "{name}"
        );
        assert_eq!(
            *h.engine.pending_discards.lock().unwrap(),
            1,
            "{name}: pending automatic plans are discarded"
        );
    }
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    let b = blocker(
        B::ChangedSincePlanning,
        "entry changed since the plan was built: completed:m/a.mkv",
    );
    h.engine.report(report(
        10,
        ActionKind::ConsolidateHardlink,
        ActionOutcome::NotExecuted { blockers: vec![b] },
        false,
        false,
    ));
    cycle(&h).await.unwrap();
    assert_eq!(paused(&h).await, None);
    assert!(h.engine.log().contains(&"plan:Deletions".to_string()));
}

#[tokio::test(start_paused = true)]
async fn f05_nothing_plannable_is_harmless_only_without_uncertainty_exclusions() {
    use removarr_domain::issue::BlockerCode as B;
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Ok(PhasePlans {
            plans: vec![],
            refused: vec![Refusal {
                reason: "3 item(s) excluded".into(),
                benign: true,
                blockers: vec![blocker(
                    B::ServiceUnavailable(removarr_domain::session::ServiceKind::Sonarr),
                    "ARR down",
                )],
            }],
        }),
    );
    cycle(&h).await.unwrap();
    assert!(paused(&h).await.unwrap().contains("policy refusal"));
    // The same text with only ordinary exclusions stays harmless.
    let h = harness(true);
    h.engine.phases.lock().unwrap().insert(
        "Consolidations".into(),
        Ok(PhasePlans {
            plans: vec![],
            refused: vec![Refusal {
                reason: "3 item(s) excluded".into(),
                benign: true,
                blockers: vec![blocker(B::Recent, "")],
            }],
        }),
    );
    cycle(&h).await.unwrap();
    assert_eq!(paused(&h).await, None);
}

#[tokio::test(start_paused = true)]
async fn f06_only_a_cycle_run_while_enabled_and_not_paused_is_active() {
    let h = harness(true);
    h.engine.phase(
        AutoPhase::Consolidations,
        vec![phase_plan(10, ActionKind::ConsolidateHardlink, "m/a.mkv")],
    );
    h.engine.report(ok_consolidation(10));
    cycle(&h).await.unwrap();
    assert_eq!(*h.engine.auto_flags.lock().unwrap(), [true]);
    // Off or paused: nothing executes at all, so the flag can never be true.
    let h = harness(false);
    cycle(&h).await.unwrap();
    assert!(h.engine.auto_flags.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn f07_t4_era_refusals_do_not_block_subjects() {
    let h = harness(true);
    let now = h.clock.now_ts();
    let mk = |rel: &str, outcome: ActionOutcome| ExecutedAction {
        plan: pid(1),
        at: now,
        action: consolidation_action(rel),
        outcome: Some(outcome),
    };
    let not_in_build = ActionOutcome::NotExecuted {
        blockers: vec![blocker(
            removarr_domain::issue::BlockerCode::NotInBuild,
            "tier",
        )],
    };
    let other = ActionOutcome::NotExecuted {
        blockers: vec![blocker(
            removarr_domain::issue::BlockerCode::Recent,
            "young",
        )],
    };
    *h.hist.0.lock().unwrap() = vec![
        mk("t4/era.mkv", not_in_build),
        mk("real/failure.mkv", other),
    ];
    let sets = failed_sets(&h.hist, now).await.unwrap();
    assert!(!sets.paths.contains(&rp("completed", "t4/era.mkv")));
    assert!(
        !sets
            .subjects
            .contains(&SubjectKey("p:completed:t4/era.mkv".into()))
    );
    assert!(sets.paths.contains(&rp("completed", "real/failure.mkv")));
    // The window is keyed on the outcome time: an old plan whose outcome is recent counts.
    let old_plan_recent_outcome = ExecutedAction {
        plan: pid(2),
        at: Timestamp(now.0 - 6 * 86_400_000),
        action: consolidation_action("late/recovered.mkv"),
        outcome: Some(ActionOutcome::FailedNeedsReview),
    };
    h.hist.0.lock().unwrap().push(old_plan_recent_outcome);
    assert!(
        failed_sets(&h.hist, now)
            .await
            .unwrap()
            .paths
            .contains(&rp("completed", "late/recovered.mkv"))
    );
}
