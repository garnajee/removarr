#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `CleanupStalledIncomplete` (OQ-06, T3-10), `FixMode` (T3-18) and the tier-2 service
//! commands `ArrRescan` / `JellyfinRefresh` on the lab.

mod actions_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use actions_common::*;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep, VerificationStatus};
use removarr_domain::path::RootId;
use removarr_domain::plan::{HashedPlan, PlanOrigin, PlanStatus};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{ActionEvent, ExecutionReport, Reporter, execute};

async fn run(
    lab: &Lab,
    plan: HashedPlan,
    reporter: Reporter,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = lab.ticket(plan, false).await;
    let (mutators, counting) = lab.live();
    let report = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &reporter,
    )
    .await
    .unwrap();
    (report, counting)
}

fn blockers(o: &ActionOutcome) -> &[removarr_domain::issue::Blocker] {
    match o {
        ActionOutcome::NotExecuted { blockers } => blockers,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn t3_10_stalled_cleanup_stops_removes_the_task_then_deletes_under_incomplete_only() {
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert!(!lab.incomplete_path("Dl").exists());
    assert!(
        lab.incomplete_path("lost.mkv.!qB").exists(),
        "the orphan next to it is untouched"
    );
    assert!(!lab.qbit.listed('d'));
    assert_eq!(lab.qbit.calls_of("stop"), vec![[hash('d')].into()]);
    assert_eq!(lab.qbit.calls_of("remove"), vec![[hash('d')].into()]);
    assert!(lab.qbit.calls_of("start").is_empty());
    assert_eq!(counting.trees.load(Ordering::SeqCst), 1);
    assert_eq!(
        lab.steps(plan_id).await,
        vec![
            JournalStep::QbitStop,
            JournalStep::QbitRemoveKeepData,
            JournalStep::FsRemoveTree
        ],
        "stop, task removal (deleteFiles=false), then the deletion"
    );
    assert!(lab.repos.gains.totals().await.unwrap().attributable > 0);
}

#[tokio::test]
async fn stalled_cleanup_refuses_busy_complete_or_misplaced_torrents_before_any_mutation() {
    // Busy (checking) after planning.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    lab.qbit.set_state('d', "checkingDL");
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert!(
        blockers(&report.actions[0].outcome)
            .iter()
            .any(|b| b.code == BlockerCode::TorrentProtectedState),
        "{report:?}"
    );
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    // A file of the torrent mapped outside the incomplete root (fresh file list).
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    lab.qbit.set_priority('d', 0, 1);
    lab.qbit
        .add_file('d', "Dl/other.mkv", &RootId::completed(), "Rel/b.mkv");
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    let bl = blockers(&report.actions[0].outcome);
    assert!(
        bl.iter().any(|b| b.code == BlockerCode::ProtectedZone),
        "{bl:?}"
    );
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    // A data path under the completed root in the plan itself: refused by the runner.
    let lab = Lab::new().await;
    let mut a = lab.stalled_action(0, 'd', &["Dl"]);
    if let removarr_domain::plan::ActionTarget::Torrent { data_paths, .. } = &mut a.target {
        data_paths.push(rp(&RootId::completed(), "Rel/b.mkv"));
    }
    let plan = lab.seal(vec![a], PlanOrigin::User, vec![]);
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::ProtectedZone
    );
    assert!(lab.steps(plan_id).await.is_empty());
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    assert!(lab.incomplete_path("Dl/part.mkv.!qB").exists());
}

#[tokio::test]
async fn a_failed_task_removal_restores_the_torrent_and_deletes_nothing() {
    let lab = Lab::new().await;
    *lab.qbit.fail_remove.lock().unwrap() = true;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(lab.qbit.listed('d'));
    assert_eq!(
        lab.qbit.state('d'),
        TorrentState::StalledDl,
        "started again"
    );
    assert!(lab.incomplete_path("Dl/part.mkv.!qB").exists());
    assert_eq!(counting.mutations(), 0);
    assert_eq!(
        lab.steps(plan_id).await,
        vec![
            JournalStep::QbitStop,
            JournalStep::QbitRemoveKeepData,
            JournalStep::QbitStart
        ]
    );
}

#[tokio::test]
async fn data_referenced_after_the_task_removal_is_left_as_an_orphan() {
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = plan.plan().id;
    let qbit = lab.qbit.clone();
    let reporter = Reporter::new().with_sink(Arc::new(move |e: &ActionEvent| {
        if matches!(
            e,
            ActionEvent::StepResult {
                step: JournalStep::QbitRemoveKeepData,
                ..
            }
        ) {
            qbit.add_with(
                'e',
                &RootId::incomplete(),
                "Dl",
                &[("Dl/part.mkv", "Dl/part.mkv.!qB")],
                "stalledDL",
                0.2,
            );
        }
    }));
    let (report, counting) = run(&lab, plan, reporter).await;
    let ActionOutcome::ExecutedUnverified { reason } = &report.actions[0].outcome else {
        panic!("{report:?}");
    };
    assert!(reason.contains("orphan"), "{reason}");
    assert!(!lab.qbit.listed('d'));
    assert!(
        lab.incomplete_path("Dl/part.mkv.!qB").exists(),
        "never deleted"
    );
    assert_eq!(counting.mutations(), 0);
    assert_eq!(
        lab.steps(plan_id).await,
        vec![JournalStep::QbitStop, JournalStep::QbitRemoveKeepData]
    );
}

#[tokio::test]
async fn t3_18_fix_mode_is_targeted_revalidated_and_verified() {
    let lab = Lab::new().await;
    fs::set_permissions(
        lab.completed_path("Rel/b.mkv"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    lab.store_observation_again().await;
    let plan = lab.seal(
        vec![lab.fix_mode_action(0, "Rel/b.mkv", 0o644)],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(
        report.actions[0].verification[0].status,
        VerificationStatus::Passed
    );
    assert_eq!(
        fs::metadata(lab.completed_path("Rel/b.mkv"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    assert_eq!(counting.modes.load(Ordering::SeqCst), 1);
    assert_eq!(lab.steps(plan_id).await, vec![JournalStep::FsSetMode]);
    assert_eq!(lab.qbit.mutation_count(), 0);

    // The entry changed after planning: refused, mode untouched.
    let lab = Lab::new().await;
    fs::set_permissions(
        lab.completed_path("Rel/b.mkv"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    lab.store_observation_again().await;
    let plan = lab.seal(
        vec![lab.fix_mode_action(0, "Rel/b.mkv", 0o644)],
        PlanOrigin::User,
        vec![],
    );
    fs::set_permissions(
        lab.completed_path("Rel/b.mkv"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::ChangedSincePlanning
    );
    assert_eq!(counting.mutations(), 0);
    assert_eq!(
        fs::metadata(lab.completed_path("Rel/b.mkv"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o400
    );
}

#[tokio::test]
async fn service_commands_are_journaled_and_verified_by_observing_the_service() {
    let lab = Lab::new().await;
    let plan = lab.seal(vec![lab.rescan_action(0)], PlanOrigin::User, vec![]);
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(
        lab.arr_cmd.0.load(Ordering::SeqCst),
        1,
        "one rescan command"
    );
    assert_eq!(lab.steps(plan_id).await, vec![JournalStep::ArrRescan]);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    let plan = lab.seal(vec![lab.jellyfin_action(0)], PlanOrigin::User, vec![]);
    let plan_id = plan.plan().id;
    let (report, _) = run(&lab, plan, Reporter::new()).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.jellyfin_cmd.0.load(Ordering::SeqCst), 1);
    assert_eq!(lab.steps(plan_id).await, vec![JournalStep::JellyfinRefresh]);
}
