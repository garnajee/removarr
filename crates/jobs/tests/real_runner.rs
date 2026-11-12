#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The automatic cycle on top of the real action runner, real SQLite and the real confined
//! filesystem (the lab of `workflows/tests/actions_common`, included by path).
//!
//! `IMPLEMENTED_TIER` is T4, so the production `execute` refuses every automatic plan. The
//! cycle is therefore driven through the `tier5-test-gate` entry (`execute_with_gate`), which
//! only the test builds enable, to show it works once the gate is raised; and through the
//! production entry to show that nothing runs today.

#[path = "../../workflows/tests/actions_common/mod.rs"]
mod actions_common;
mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actions_common::*;
use async_trait::async_trait;
use removarr_domain::error::PortError;
use removarr_domain::evidence::EvidenceLevel;
use removarr_domain::ids::{JobId, PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::path::RootId;
use removarr_domain::plan::{Approval, Approver, HashedPlan, PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{CancelFlag, PlanStore};
use removarr_domain::time::Timestamp;
use removarr_jobs::auto::{
    ActionFacts, ActionHistory, AutoControl, CycleDeps, CycleEngine, ExecutedAction, KEY_CYCLE,
    PhaseError, PhasePlan, PhasePlans, RescanSummary, failed_sets, run_cycle,
};
use removarr_jobs::events::{Hub, ProgressHandle};
use removarr_jobs::handler::{JobCtx, RecoverySummary};
use removarr_jobs::limits::Limits;
use removarr_jobs::locks::{Locks, Scope};
use removarr_workflows::actions::{
    EventSink, ExecutionReport, Reporter, execute, execute_with_gate, recover,
};
use removarr_workflows::plans::{
    AutoContext, AutoPhase, ExecutionTicket, action_paths, action_subjects,
};
use tokio_util::sync::CancellationToken;

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];

fn age_files(lab: &Lab, hours: u64) {
    let at = std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(NOW.0).unwrap())
        - Duration::from_secs(hours * 3600);
    for p in [
        lab.completed_path("Rel/a.mkv"),
        lab.completed_path("Rel/b.mkv"),
        lab.media_path("Movies/Rel/a.mkv"),
        lab.media_path("Movies/Rel/b.mkv"),
    ] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }
}

/// Reference implementation of `ActionHistory` over the storage repositories (for `app`):
/// plans that started executing, their stored actions and recorded outcomes.
struct StoreHistory {
    plans: removarr_storage::PlanRepo,
}

#[async_trait]
impl ActionHistory for StoreHistory {
    async fn executed_since(&self, since: Timestamp) -> Result<Vec<ExecutedAction>, PortError> {
        let mut out = Vec::new();
        for status in [
            PlanStatus::Executing,
            PlanStatus::Executed,
            PlanStatus::PartiallyExecuted,
            PlanStatus::Failed,
            PlanStatus::Interrupted,
            PlanStatus::Invalidated,
        ] {
            for id in self.plans.ids_with_status(status).await? {
                let Some((plan, ..)) = PlanStore::get(&self.plans, id).await? else {
                    continue;
                };
                if plan.created_at < since {
                    continue;
                }
                let rows = self.plans.actions(id).await?;
                for a in plan.actions {
                    let outcome = rows
                        .iter()
                        .find(|r| r.index == a.index)
                        .and_then(|r| r.outcome.clone());
                    out.push(ExecutedAction {
                        plan: id,
                        at: plan.created_at,
                        action: a,
                        outcome,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// The cycle's engine over the lab: hand-built automatic plans (the planner needs a classified
/// session, covered by workflows' own tests), the real runner and the real recovery.
struct LabEngine {
    lab: Arc<Lab>,
    gate: Option<RiskTier>,
    plans: Mutex<HashMap<PlanId, (HashedPlan, Approval)>>,
    rescans: Mutex<Vec<BTreeSet<RootId>>>,
    log: Mutex<Vec<String>>,
}

impl LabEngine {
    fn new(lab: Arc<Lab>, gate: Option<RiskTier>) -> Arc<Self> {
        Arc::new(Self {
            lab,
            gate,
            plans: Mutex::default(),
            rescans: Mutex::default(),
            log: Mutex::default(),
        })
    }
}

#[async_trait]
impl CycleEngine for LabEngine {
    fn simulate(&self) -> bool {
        false
    }

    async fn plan_phase(
        &self,
        phase: AutoPhase,
        _ctx: &AutoContext,
        _s: SessionId,
    ) -> Result<PhasePlans, PhaseError> {
        if phase == AutoPhase::Deletions {
            return Ok(PhasePlans::default());
        }
        let plan = self.lab.plan(ONE, PlanMode::Live, PlanOrigin::AutoPolicy);
        let (mut p, _) = plan.into_parts();
        p.actions
            .iter_mut()
            .for_each(|a| a.evidence_level = EvidenceLevel::NameSize);
        let plan = HashedPlan::seal(p).unwrap();
        let (p, h) = plan.clone().into_parts();
        self.lab.repos.plans.insert(&p, &h).await.unwrap();
        let approval = plan
            .approve_auto(NOW)
            .map_err(|e| PhaseError::Failed(e.to_string()))?;
        self.lab.repos.plans.approve(&approval).await.unwrap();
        let ap = plan.plan();
        let phase_plan = PhasePlan {
            plan: ap.id,
            live_scope: Scope::for_plan(ap, false),
            read_scope: Scope::for_plan(ap, true),
            actions: ap
                .actions
                .iter()
                .map(|a| ActionFacts {
                    index: a.index,
                    kind: a.kind,
                    paths: action_paths(a),
                    subjects: action_subjects(a),
                })
                .collect(),
        };
        self.plans.lock().unwrap().insert(ap.id, (plan, approval));
        Ok(PhasePlans {
            plans: vec![phase_plan],
            refused: vec![],
        })
    }

    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        Ok(BTreeSet::new())
    }

    async fn discard(&self, plan: PlanId) -> Result<(), PortError> {
        self.log.lock().unwrap().push(format!("discard {plan}"));
        Ok(())
    }

    async fn recover(&self, sink: EventSink) -> Result<RecoverySummary, PortError> {
        let (mutators, _) = self.lab.live();
        let report = recover(
            &self.lab.ports(),
            &mutators,
            &self.lab.settings(),
            &Reporter::new().with_sink(sink),
        )
        .await?;
        self.log
            .lock()
            .unwrap()
            .push(format!("recover:{}", report.plans.len()));
        Ok(RecoverySummary {
            plans: report.plans.len() as u32,
            left_stopped: report
                .plans
                .iter()
                .map(|p| p.left_stopped.len() as u32)
                .sum(),
            notes: vec![],
        })
    }

    async fn execute(
        &self,
        plan: PlanId,
        simulate: bool,
        cancel: &CancelFlag,
        sink: EventSink,
    ) -> Result<ExecutionReport, PortError> {
        self.log.lock().unwrap().push("execute".into());
        let (plan, approval) = self.plans.lock().unwrap().get(&plan).cloned().unwrap();
        let ticket = ExecutionTicket {
            plan,
            status: PlanStatus::Approved,
            approval: Some(approval),
            simulate,
        };
        let (mutators, _) = self.lab.live();
        let reporter = Reporter::new().with_sink(sink);
        match self.gate {
            Some(g) => {
                execute_with_gate(
                    &self.lab.ports(),
                    &mutators,
                    &self.lab.settings(),
                    ticket,
                    cancel,
                    &reporter,
                    g,
                )
                .await
            }
            None => {
                execute(
                    &self.lab.ports(),
                    &mutators,
                    &self.lab.settings(),
                    ticket,
                    cancel,
                    &reporter,
                )
                .await
            }
        }
    }

    async fn rescan(
        &self,
        roots: &BTreeSet<RootId>,
        _cancel: &CancelFlag,
    ) -> Result<RescanSummary, PortError> {
        self.rescans.lock().unwrap().push(roots.clone());
        Ok(RescanSummary {
            session: SessionId::new(RawUuid::from_u128(2)),
            complete: true,
            uncertainties: vec![],
        })
    }
}

struct Rig {
    state: Arc<common::MemState>,
    auto: Arc<AutoControl>,
    ctx: JobCtx,
}

fn rig() -> Rig {
    let clock = common::TokioClock::at(NOW.0);
    let store = common::MemStore::new(clock.clone());
    let state = Arc::new(common::MemState::default());
    let hub = Hub::new(clock.clone());
    let auto = Arc::new(AutoControl::new(true, state.clone(), hub.clone()));
    let id = JobId::new(RawUuid::from_u128(1));
    let ctx = JobCtx {
        id,
        cancel: CancelFlag::new(),
        token: CancellationToken::new(),
        progress: ProgressHandle::new(id, hub, store),
        limits: Arc::new(Limits::new(&Default::default())),
        locks: Arc::new(Locks::new()),
        clock,
        auto: auto.clone(),
    };
    Rig { state, auto, ctx }
}

fn session() -> SessionId {
    SessionId::new(RawUuid::from_u128(1))
}

#[tokio::test]
async fn t5_the_cycle_runs_an_automatic_consolidation_end_to_end_once_the_gate_is_raised() {
    let lab = Arc::new(Lab::new().await);
    age_files(&lab, 72);
    let engine = LabEngine::new(lab.clone(), Some(RiskTier::T5));
    let r = rig();
    let hist = StoreHistory {
        plans: lab.repos.plans.clone(),
    };
    let out = run_cycle(
        &CycleDeps {
            engine: &*engine,
            history: &hist,
            ctx: &r.ctx,
        },
        session(),
    )
    .await
    .unwrap();

    assert_eq!(
        out.metrics["actions_verified"],
        1,
        "{out:?} {:?}",
        r.auto.paused()
    );
    assert_eq!(r.auto.paused(), None);
    // The completed file and the library copy now share one inode (verified by the runner).
    assert_eq!(
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/a.mkv"))
    );
    // Recovery ran first, then the plan, then a rescan of exactly the roots it mutated.
    let log = engine.log.lock().unwrap().clone();
    assert_eq!(log, ["recover:0", "execute"]);
    let rescans = engine.rescans.lock().unwrap().clone();
    assert_eq!(rescans.len(), 1);
    assert!(rescans[0].contains(&RootId::completed()) && rescans[0].contains(&lab.media));
    // The approval is the policy's, distinguishable in the history (T5-19).
    let (plan_id, (_, approval)) = engine
        .plans
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (*k, v.clone()))
        .next()
        .unwrap();
    assert_eq!(approval.by, Approver::AutoPolicy);
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executed);
    assert_eq!(lab.service_mutations(), 0);
    assert_eq!(r.state.get_raw(KEY_CYCLE), Some(String::new()));
}

#[tokio::test]
async fn t5_with_the_production_gate_nothing_runs_and_the_refusal_feeds_the_failed_sets() {
    let lab = Arc::new(Lab::new().await);
    age_files(&lab, 72);
    let engine = LabEngine::new(lab.clone(), None); // production `execute`: IMPLEMENTED_TIER = T4
    let r = rig();
    let hist = StoreHistory {
        plans: lab.repos.plans.clone(),
    };
    let (_, counting) = lab.live();
    let before = (
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
    );
    run_cycle(
        &CycleDeps {
            engine: &*engine,
            history: &hist,
            ctx: &r.ctx,
        },
        session(),
    )
    .await
    .unwrap();

    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.service_mutations(), 0);
    assert_eq!(
        before,
        (
            lab.phys(&lab.completed_path("Rel/a.mkv")),
            lab.phys(&lab.media_path("Movies/Rel/a.mkv"))
        )
    );
    assert!(
        engine.rescans.lock().unwrap().is_empty(),
        "no live mutation, no rescan"
    );
    assert_eq!(
        r.auto.paused(),
        None,
        "NotExecuted by the gate is a refusal, not an uncertainty"
    );
    let plan_id = *engine.plans.lock().unwrap().keys().next().unwrap();
    assert_eq!(lab.status(plan_id).await, PlanStatus::Invalidated);
    assert!(
        lab.journal(plan_id).await.is_empty(),
        "refused before any intent"
    );

    // T3-12 / T5-10 from the real journal tables: the refused consolidation's paths are failed.
    let sets = failed_sets(&hist, NOW).await.unwrap();
    assert!(sets.paths.contains(&rp(&RootId::completed(), "Rel/a.mkv")));
    assert!(sets.paths.contains(&rp(&lab.media, "Movies/Rel/a.mkv")));
    assert!(!sets.subjects.is_empty());
}

#[tokio::test]
async fn t5_16_a_crash_inside_the_cycle_is_recovered_first_and_stops_the_next_cycle() {
    let lab = Arc::new(Lab::new().await);
    age_files(&lab, 72);
    let engine = LabEngine::new(lab.clone(), Some(RiskTier::T5));
    let r = rig();
    let hist = Arc::new(StoreHistory {
        plans: lab.repos.plans.clone(),
    });
    lab.hang.arm(CrashPoint::AfterIntent(
        removarr_domain::journal::JournalStep::FsReplaceWithHardlink,
    ));
    let (e2, h2, ctx2) = (engine.clone(), hist.clone(), r.ctx.clone());
    let task = tokio::spawn(async move {
        run_cycle(
            &CycleDeps {
                engine: &*e2,
                history: &*h2,
                ctx: &ctx2,
            },
            session(),
        )
        .await
        .map(|_| ())
    });
    tokio::time::timeout(Duration::from_secs(10), lab.hang.hit.notified())
        .await
        .expect("crash point reached");
    task.abort();
    let _ = task.await;
    lab.hang.disarm();
    // The crashed process left its marker and an unfinished intent.
    assert!(r.state.get_raw(KEY_CYCLE).is_some_and(|m| !m.is_empty()));
    assert_eq!(
        removarr_domain::ports::JournalStore::unfinished(&lab.repos.journal)
            .await
            .unwrap()
            .len(),
        1,
        "an intent without a result"
    );

    // Next process: a fresh cycle first runs `recover` (T3-19). It resolves the interrupted
    // plan, which stops automatic execution (T5-08) before anything new is planned or run.
    let engine2 = LabEngine::new(lab.clone(), Some(RiskTier::T5));
    let out = run_cycle(
        &CycleDeps {
            engine: &*engine2,
            history: &*hist,
            ctx: &r.ctx,
        },
        session(),
    )
    .await
    .unwrap();
    assert_eq!(out.metrics["paused"], 1);
    assert!(r.auto.paused().unwrap().contains("recovery resolved 1"));
    assert!(!engine2.log.lock().unwrap().contains(&"execute".to_string()));
    let crashed = *engine.plans.lock().unwrap().keys().next().unwrap();
    assert_eq!(lab.status(crashed).await, PlanStatus::Interrupted);
}
