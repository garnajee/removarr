#![allow(clippy::unwrap_used, clippy::expect_used)]
//! CP2-tier4 fixes F01-F05 (stage C step 1).

mod actions_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use actions_common::*;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep};
use removarr_domain::path::RootId;
use removarr_domain::plan::{
    ActionKind, HashedPlan, PlanMode, PlanOrigin, PlanStatus, Precondition,
};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{ExecutionReport, Mutators, Reporter, execute};

async fn run_with(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
    settings: removarr_workflows::actions::ActionSettings,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = lab.ticket(plan, simulate).await;
    let (mutators, counting) = lab.live();
    let report = execute(
        &lab.ports(),
        &mutators,
        &settings,
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    (report, counting)
}

async fn run(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
) -> (ExecutionReport, Arc<CountingMutator>) {
    run_with(lab, plan, simulate, lab.settings()).await
}

fn first_blocker(o: &ActionOutcome) -> BlockerCode {
    match o {
        ActionOutcome::NotExecuted { blockers } => blockers[0].code.clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn f01_automatic_plans_are_refused_before_any_intent_live_and_simulated() {
    for simulate in [false, true] {
        let lab = Lab::new().await;
        let deletion = lab.seal(
            vec![lab.delete_file_action(
                0,
                ActionKind::DeleteCompletedFile,
                &RootId::completed(),
                "Rel/a.nfo",
                &[],
            )],
            PlanOrigin::AutoPolicy,
            vec![],
        );
        let consolidation = lab.plan(
            &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])],
            PlanMode::Live,
            PlanOrigin::AutoPolicy,
        );
        for plan in [deletion, consolidation] {
            let id = plan.plan().id;
            let (report, counting) = run(&lab, plan, simulate).await;
            assert_eq!(
                first_blocker(&report.actions[0].outcome),
                BlockerCode::NotInBuild,
                "{report:?}"
            );
            assert!(lab.steps(id).await.is_empty(), "no intent");
            assert_eq!(counting.mutations(), 0);
            assert_eq!(lab.qbit.mutation_count(), 0);
            assert_eq!(lab.service_mutations(), 0);
            assert!(lab.completed_path("Rel/a.nfo").exists());
            assert!(lab.completed_path("Rel/a.mkv").exists());
        }
    }
}

#[tokio::test]
async fn f02_a_task_removal_that_happened_despite_a_transport_error_is_observed() {
    let lab = Lab::new().await;
    *lab.qbit.remove_then_fail.lock().unwrap() = true;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let (report, _) = run(&lab, plan, false).await;
    assert!(
        !matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(!lab.qbit.listed('d'));
    assert!(
        lab.qbit.calls_of("start").is_empty(),
        "a removed task is never started"
    );
    assert!(!lab.incomplete_path("Dl/part.mkv.!qB").exists());
    assert!(report.left_stopped.is_empty(), "{:?}", report.left_stopped);

    // The torrent-held deletion path: same observation.
    let lab = Lab::new().await;
    *lab.qbit.remove_then_fail.lock().unwrap() = true;
    let plan = lab.seal(
        vec![
            lab.delete_file_action(
                0,
                ActionKind::DeleteCompletedFile,
                &RootId::completed(),
                "Rel/a.mkv",
                &['a'],
            ),
            lab.remove_task_action(1, 'a', &[0]),
        ],
        PlanOrigin::User,
        vec![],
    );
    let (report, _) = run(&lab, plan, false).await;
    assert_eq!(
        report.actions[1].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(report.left_stopped.is_empty());
}

#[tokio::test]
async fn f03_a_listing_fault_before_the_stop_never_leaves_a_torrent_stopped_and_unreported() {
    // Fail every listing call position in turn: whatever the fault position, a torrent that
    // is stopped at the end is reported as left stopped, or was never stopped.
    for n in 0..14 {
        let lab = Lab::new().await;
        *lab.qbit.fail_listing_at.lock().unwrap() = Some(n);
        let plan = lab.seal(
            vec![lab.stalled_action(0, 'd', &["Dl"])],
            PlanOrigin::User,
            vec![],
        );
        let (report, _) = run(&lab, plan, false).await;
        if lab.qbit.listed('d') {
            let stopped = matches!(
                lab.qbit.state('d'),
                TorrentState::StoppedDl | TorrentState::StoppedUp
            );
            assert!(
                !stopped || report.left_stopped.contains_key(&hash('d')),
                "call {n}: stopped and unreported: {report:?}"
            );
        }
    }
}

#[tokio::test]
async fn f04_a_mode_that_removes_bits_or_adds_foreign_ones_is_refused_before_any_intent() {
    for bad in [0o000, 0o4644, 0o666, 0o464, 0o444] {
        let lab = Lab::new().await;
        // Rel/b.mkv is 0o644 on disk (owner rw, group/other r).
        let plan = lab.seal(
            vec![lab.fix_mode_action(0, "Rel/b.mkv", bad)],
            PlanOrigin::User,
            vec![],
        );
        let id = plan.plan().id;
        let (report, counting) = run(&lab, plan, false).await;
        assert_eq!(
            first_blocker(&report.actions[0].outcome),
            BlockerCode::ProtectedZone,
            "{bad:o}: {report:?}"
        );
        assert!(lab.steps(id).await.is_empty());
        assert_eq!(counting.mutations(), 0);
        assert_eq!(
            fs::metadata(lab.completed_path("Rel/b.mkv"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
    }
}

#[tokio::test]
async fn f05_missing_pins_a_changed_config_and_a_disabled_capability_refuse_the_plan() {
    use removarr_domain::policy::Capability;
    // No wanted-file pin for the affected hash.
    let lab = Lab::new().await;
    let mut a = lab.delete_file_action(
        0,
        ActionKind::DeleteCompletedFile,
        &RootId::completed(),
        "Rel/a.mkv",
        &['a'],
    );
    a.preconditions
        .retain(|p| !matches!(p, Precondition::TorrentWantedFiles { .. }));
    let plan = lab.seal(vec![a], PlanOrigin::User, vec![]);
    let id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
    assert_eq!(
        first_blocker(&report.actions[0].outcome),
        BlockerCode::ChangedSincePlanning
    );
    assert!(lab.steps(id).await.is_empty());
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.status(id).await, PlanStatus::Invalidated);

    // A configuration that differs from the plan's fingerprint.
    let lab = Lab::new().await;
    let plan = lab.junk_plan();
    let mut settings = lab.settings();
    settings.config_fingerprint = "another".into();
    let (report, counting) = run_with(&lab, plan, false, settings).await;
    assert_eq!(
        first_blocker(&report.actions[0].outcome),
        BlockerCode::ChangedSincePlanning
    );
    assert_eq!(counting.mutations(), 0);

    // The capability of the kind is off.
    let lab = Lab::new().await;
    lab.capabilities
        .lock()
        .unwrap()
        .remove(&Capability::DeleteCompleted);
    let plan = lab.junk_plan();
    let (report, counting) = run(&lab, plan, true).await;
    assert_eq!(
        first_blocker(&report.actions[0].outcome),
        BlockerCode::CapabilityDisabled
    );
    assert_eq!(counting.mutations(), 0);
    let _ = Mutators::recording(&[]);
    let _ = JournalStep::QbitStop;
}
