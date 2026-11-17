#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Crash boundaries of the tier-3 steps (T3-17): a fake hangs at the boundary, the execution
//! task is aborted (a process crash), then `recover` runs on the same SQLite journal and temp
//! roots. Each test asserts the disk, the torrent states and the reported outcome. Recovery
//! decides from observations and never redoes a mutation; a task removal is never followed
//! by a deletion at recovery.

mod actions_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;

use actions_common::*;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::path::RootId;
use removarr_domain::plan::{ActionKind, HashedPlan, PlanOrigin, PlanStatus};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{LeftStoppedReason, RecoveryReport, Reporter, execute, recover};

fn deletion_plan(lab: &Lab) -> HashedPlan {
    lab.seal(
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
    )
}

async fn crash_at(lab: &Lab, plan: HashedPlan, point: CrashPoint) -> removarr_domain::ids::PlanId {
    let ticket = lab.ticket(plan, false).await;
    let plan_id = ticket.plan.plan().id;
    lab.hang.arm(point);
    let (mutators, _) = lab.live();
    let (ports, settings) = (lab.ports(), lab.settings());
    let task = tokio::spawn(async move {
        let reporter = Reporter::new();
        execute(
            &ports,
            &mutators,
            &settings,
            ticket,
            &CancelFlag::new(),
            &reporter,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), lab.hang.hit.notified())
        .await
        .expect("the crash point is reached");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lab.hang.disarm();
    assert_eq!(*lab.hang.reached.lock().unwrap(), Some(point));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executing);
    plan_id
}

async fn recovered(lab: &Lab) -> (RecoveryReport, std::sync::Arc<CountingMutator>) {
    let (mutators, counting) = lab.live();
    let r = recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap();
    (r, counting)
}

fn outcome(r: &RecoveryReport, index: u32) -> &ActionOutcome {
    &r.plans[0]
        .actions
        .iter()
        .find(|(i, _)| *i == index)
        .unwrap()
        .1
}

#[tokio::test]
async fn crash_after_the_remove_file_intent_before_the_unlink_resumes_the_torrent() {
    let lab = Lab::new().await;
    let plan_id = crash_at(
        &lab,
        deletion_plan(&lab),
        CrashPoint::AfterIntent(JournalStep::FsRemoveFile),
    )
    .await;
    assert!(lab.completed_path("Rel/a.mkv").exists());
    assert_eq!(
        lab.qbit.state('a'),
        TorrentState::StoppedUp,
        "stopped by the plan"
    );
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.mutations(), 0, "never redone");
    assert!(lab.completed_path("Rel/a.mkv").exists());
    assert_eq!(blockers(outcome(&r, 0))[0].code, BlockerCode::Interrupted);
    assert_eq!(blockers(outcome(&r, 1))[0].code, BlockerCode::Interrupted);
    assert_eq!(
        r.plans[0].resumed,
        [hash('a')].into(),
        "untouched: started again"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert!(lab.qbit.calls_of("recheck").is_empty());
    assert!(lab.qbit.calls_of("remove").is_empty());
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    let journal = lab.journal(plan_id).await;
    let (_, res) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsRemoveFile)
        .unwrap();
    assert!(matches!(
        res.as_ref().unwrap().outcome,
        StepOutcome::Skipped { .. }
    ));
}

fn blockers(o: &ActionOutcome) -> &[removarr_domain::issue::Blocker] {
    match o {
        ActionOutcome::NotExecuted { blockers } => blockers,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn crash_after_the_unlink_before_the_result_rechecks_and_leaves_the_torrent_stopped() {
    let lab = Lab::new().await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan_id = crash_at(
        &lab,
        deletion_plan(&lab),
        CrashPoint::BeforeResult(JournalStep::FsRemoveFile),
    )
    .await;
    assert!(
        !lab.completed_path("Rel/a.mkv").exists(),
        "the unlink happened"
    );
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.mutations(), 0);
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert_eq!(blockers(outcome(&r, 1))[0].code, BlockerCode::Interrupted);
    assert_eq!(lab.qbit.calls_of("recheck"), vec![[hash('a')].into()]);
    assert_eq!(
        r.plans[0].left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert!(lab.qbit.listed('a'), "never removed at recovery");
    assert!(lab.qbit.calls_of("remove").is_empty());
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    let journal = lab.journal(plan_id).await;
    let (_, res) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsRemoveFile)
        .unwrap();
    assert!(
        matches!(res.as_ref().unwrap().outcome, StepOutcome::Done { .. }),
        "observed absent"
    );
}

#[tokio::test]
async fn crash_around_the_task_removal_is_resolved_from_the_listing() {
    // After the intent, before the call: the task is still listed, the hash (modified by the
    // deletion) is rechecked and left stopped.
    let lab = Lab::new().await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan_id = crash_at(
        &lab,
        deletion_plan(&lab),
        CrashPoint::AfterIntent(JournalStep::QbitRemoveKeepData),
    )
    .await;
    assert!(lab.qbit.listed('a'));
    let (r, _) = recovered(&lab).await;
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert_eq!(blockers(outcome(&r, 1))[0].code, BlockerCode::Interrupted);
    assert!(lab.qbit.listed('a'));
    assert!(lab.qbit.calls_of("remove").is_empty());
    assert_eq!(
        r.plans[0].left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);

    // After the call, before the result: the task is gone; nothing is started or rechecked.
    let lab = Lab::new().await;
    let plan_id = crash_at(
        &lab,
        deletion_plan(&lab),
        CrashPoint::BeforeResult(JournalStep::QbitRemoveKeepData),
    )
    .await;
    assert!(!lab.qbit.listed('a'));
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.mutations(), 0);
    assert!(
        matches!(outcome(&r, 1), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert!(lab.qbit.calls_of("recheck").is_empty());
    assert!(lab.qbit.calls_of("start").is_empty());
    assert!(r.plans[0].left_stopped.is_empty());
    assert!(
        r.plans[0]
            .notes
            .iter()
            .any(|n| n.contains("task was removed"))
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    let journal = lab.journal(plan_id).await;
    let (_, res) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::QbitRemoveKeepData)
        .unwrap();
    assert!(matches!(
        res.as_ref().unwrap().outcome,
        StepOutcome::Done { .. }
    ));
}

#[tokio::test]
async fn stalled_crash_after_the_task_removal_never_deletes_the_data_at_recovery() {
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = crash_at(
        &lab,
        plan,
        CrashPoint::BeforeResult(JournalStep::QbitRemoveKeepData),
    )
    .await;
    assert!(!lab.qbit.listed('d'));
    assert!(lab.incomplete_path("Dl/part.mkv.!qB").exists());
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.mutations(), 0, "recovery never deletes");
    assert!(
        lab.incomplete_path("Dl/part.mkv.!qB").exists(),
        "an orphan for a later plan"
    );
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert!(r.plans[0].notes.iter().any(|n| n.contains("orphan")));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);

    // Before the intent of the tree removal (task gone, data intact): same resolution.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.stalled_action(0, 'd', &["Dl"])],
        PlanOrigin::User,
        vec![],
    );
    let plan_id = crash_at(
        &lab,
        plan,
        CrashPoint::BeforeIntent(JournalStep::FsRemoveTree),
    )
    .await;
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.mutations(), 0);
    assert!(lab.incomplete_path("Dl/part.mkv.!qB").exists());
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

#[tokio::test]
async fn a_partial_tree_removal_followed_by_a_crash_is_reviewed_at_recovery() {
    if is_root_user() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let lab = Lab::new().await;
    fs::set_permissions(
        lab.completed_path("Orphan"),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    lab.store_observation_again().await;
    let sample = rp(&RootId::completed(), "Orphan/Sample/s.mkv");
    let plan = lab.seal(
        vec![lab.delete_folder_action(0, "Orphan")],
        PlanOrigin::User,
        vec![sample],
    );
    // The mutator stops half way (a subdirectory cannot be removed); the crash hits before
    // the result is written, so the journal holds only the intent.
    let plan_id = crash_at(
        &lab,
        plan,
        CrashPoint::BeforeResult(JournalStep::FsRemoveTree),
    )
    .await;
    assert!(lab.completed_path("Orphan/Orphan.mkv").exists());
    assert!(!lab.completed_path("Orphan/Subs/o.srt").exists());
    let (r, counting) = recovered(&lab).await;
    fs::set_permissions(
        lab.completed_path("Orphan"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(counting.mutations(), 0, "never redone");
    assert_eq!(outcome(&r, 0), &ActionOutcome::FailedNeedsReview, "{r:?}");
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    let journal = lab.journal(plan_id).await;
    let (_, res) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsRemoveTree)
        .unwrap();
    assert!(
        matches!(res.as_ref().unwrap().outcome, StepOutcome::Failed { .. }),
        "partially removed"
    );
}

#[tokio::test]
async fn a_failed_result_followed_by_a_crash_is_re_observed_at_recovery() {
    // The journal says `Failed` (test seam) while the disk says the file is gone; the crash
    // happens before the task removal intent. Recovery trusts the disk (F01).
    let lab = Lab::new().await;
    *lab.rewrite_done_as_failed.lock().unwrap() = Some(JournalStep::FsRemoveFile);
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan_id = crash_at(
        &lab,
        deletion_plan(&lab),
        CrashPoint::BeforeIntent(JournalStep::QbitRemoveKeepData),
    )
    .await;
    *lab.rewrite_done_as_failed.lock().unwrap() = None;
    assert!(!lab.completed_path("Rel/a.mkv").exists());
    let (r, _) = recovered(&lab).await;
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert_eq!(
        lab.qbit.calls_of("recheck").len(),
        1,
        "rechecked, not started blindly"
    );
    assert_eq!(
        r.plans[0].left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

#[tokio::test]
async fn fix_mode_crash_after_the_chmod_is_observed_from_the_mode() {
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
    let plan_id = crash_at(&lab, plan, CrashPoint::BeforeResult(JournalStep::FsSetMode)).await;
    let (r, counting) = recovered(&lab).await;
    assert_eq!(counting.modes.load(Ordering::SeqCst), 0);
    assert!(
        matches!(outcome(&r, 0), ActionOutcome::ExecutedUnverified { .. }),
        "{r:?}"
    );
    assert_eq!(
        fs::metadata(lab.completed_path("Rel/b.mkv"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}
