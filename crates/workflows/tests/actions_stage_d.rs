#![allow(clippy::unwrap_used, clippy::expect_used)]
//! CP2-tier5 fixes F01-F05 and F08 (stage D step 1).

mod actions_common;

use std::fs;
use std::sync::Arc;

use actions_common::arrfake::ImportEffect;
use actions_common::tier4::{NEW_SOURCE, OLD_MEDIA, search_result};
use actions_common::*;
use removarr_domain::arr::OldFileRecovery;
use removarr_domain::error::ErrorClass;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::plan::{HashedPlan, PlanOrigin, PlanStatus, Precondition};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{ActionEvent, ExecutionReport, Reporter, execute, recover};

fn user() -> PlanOrigin {
    PlanOrigin::User
}

async fn fixture() -> Lab {
    let lab = Lab::new().await;
    lab.t4_setup().await;
    lab
}

async fn run(lab: &Lab, plan: HashedPlan) -> (ExecutionReport, Arc<CountingMutator>) {
    run_with(lab, plan, Reporter::new()).await
}

async fn run_with(
    lab: &Lab,
    plan: HashedPlan,
    reporter: Reporter,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = lab.ticket_ack(plan, false, vec![]).await;
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

fn codes(o: &ActionOutcome) -> Vec<BlockerCode> {
    match o {
        ActionOutcome::NotExecuted { blockers } => {
            blockers.iter().map(|b| b.code.clone()).collect()
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn f01_a_queue_removal_naming_a_download_id_needs_no_pin_but_a_torrent_action_does() {
    // The planner gives an `ArrRemoveQueueItem` the issue's download id and no wanted-file pin.
    let lab = fixture().await;
    lab.t4_proven_queue().await;
    let mut a = lab.queue_action(0, 7);
    a.torrent_hashes = [hash('e')].into();
    assert!(
        !a.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::TorrentWantedFiles { .. }))
    );
    let (report, _) = run(&lab, lab.seal(vec![a], user(), vec![])).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("remove_queue"), 1);

    // Any action that does touch a torrent still needs its pin.
    let lab = fixture().await;
    let mut a = lab.replace_action(0, OldFileRecovery::RecycleBin);
    a.preconditions
        .retain(|p| !matches!(p, Precondition::TorrentWantedFiles { .. }));
    let (report, counting) = run(&lab, lab.seal(vec![a], user(), vec![])).await;
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::ChangedSincePlanning]
    );
    assert_eq!(
        counting.mutations() + lab.qbit.mutation_count() + lab.service_mutations(),
        0
    );
}

#[tokio::test]
async fn f02_after_a_moved_source_no_old_media_file_is_deleted_not_even_for_another_media() {
    let lab = Lab::new().await;
    lab.t4_setup_two().await;
    lab.arr.with(|s| {
        s.upgrade_deletes_old = false;
        s.effect_by_name
            .insert("new.mkv".into(), ImportEffect::Move);
    });
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    // Media 12 imports (hardlink) and verifies first; media 11 is moved; the deletion that
    // follows the verified import of media 12 must not run.
    let plan = lab.seal(
        vec![
            lab.replace_multi(0, &[("New/new2.mkv", 12, "Movies/Rel/b.mkv")]),
            lab.replace_multi(1, &[(NEW_SOURCE, 11, OLD_MEDIA)]),
            lab.delete_media_of(2, 0, "Movies/Rel/b.mkv"),
            lab.delete_media_of(3, 1, OLD_MEDIA),
        ],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(
        report.actions[1].outcome,
        ActionOutcome::FailedNeedsReview,
        "{report:?}"
    );
    assert!(codes(&report.actions[2].outcome).contains(&BlockerCode::ArrImportMoved));
    assert!(codes(&report.actions[3].outcome).contains(&BlockerCode::ArrImportMoved));
    assert_eq!(counting.mutations(), 0);
    assert!(lab.media_path("Movies/Rel/b.mkv").exists());
    assert!(lab.media_path(OLD_MEDIA).exists());
}

#[tokio::test]
async fn f03_an_import_or_grab_that_errors_is_possibly_executed_never_untouched_failed() {
    // A transport error on the import with an unchanged fingerprint: outcome unknown.
    let lab = fixture().await;
    lab.arr
        .with(|s| s.import_error = Some(ErrorClass::Unavailable));
    let plan = lab.seal(
        vec![lab.replace_action(0, OldFileRecovery::RecycleBin)],
        user(),
        vec![],
    );
    let id = plan.plan().id;
    let (report, _) = run(&lab, plan).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
    let journal = lab.journal(id).await;
    let (_, result) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::ArrManualImport)
        .unwrap();
    let StepOutcome::Done { final_state } = &result.as_ref().unwrap().outcome else {
        panic!("{result:?}");
    };
    assert_eq!(final_state["state"], "unknown");
    // The data may still be touched by a late command: rechecked before it runs again.
    assert!(!lab.qbit.calls_of("recheck").is_empty());
    assert_eq!(lab.arr.calls("manual_import"), 1, "never sent twice");

    // A definitive refusal (nothing was sent) stays a plain failure.
    let lab = fixture().await;
    lab.arr.with(|s| s.import_error = Some(ErrorClass::Config));
    let plan = lab.seal(
        vec![lab.replace_action(0, OldFileRecovery::RecycleBin)],
        user(),
        vec![],
    );
    let (report, _) = run(&lab, plan).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
        "{report:?}"
    );

    // Grab: a transport error before anything is visible in the queue.
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.search = vec![search_result("g1")];
        s.grab_error = Some(ErrorClass::Unavailable);
    });
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    let (report, _) = run(&lab, plan).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("grab"), 1);

    // Recovery: a result recorded as a transport failure is still "possibly executed".
    let lab = fixture().await;
    *lab.rewrite_done_as_failed.lock().unwrap() = Some(JournalStep::ArrManualImport);
    let plan = lab.seal(
        vec![lab.replace_action(0, OldFileRecovery::RecycleBin)],
        user(),
        vec![],
    );
    let ticket = lab.ticket_ack(plan, false, vec![]).await;
    let (mutators, _) = lab.live();
    lab.hang
        .arm(CrashPoint::AfterIntent(JournalStep::QbitStart));
    let (ports, settings) = (lab.ports(), lab.settings());
    let task = tokio::spawn(async move {
        execute(
            &ports,
            &mutators,
            &settings,
            ticket,
            &CancelFlag::new(),
            &Reporter::new(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), lab.hang.hit.notified())
        .await
        .expect("crash point");
    task.abort();
    let _ = task.await;
    lab.hang.disarm();
    *lab.rewrite_done_as_failed.lock().unwrap() = None;
    let (mutators, _) = lab.live();
    let r = recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap();
    assert!(
        matches!(
            r.plans[0].actions[0].1,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{:?}",
        r.plans[0].actions
    );
    assert_eq!(lab.arr.calls("manual_import"), 1);
}

#[tokio::test]
async fn f05_old_media_deletions_are_revalidated_right_before_they_run_and_samples_are_refused() {
    // A torrent starts seeding the old library file after pass 1: refused, nothing deleted.
    let lab = fixture().await;
    lab.arr.with(|s| s.upgrade_deletes_old = false);
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.delete_media_action(1, 0),
        ],
        user(),
        vec![],
    );
    let qbit = lab.qbit.clone();
    let media = lab.media.clone();
    let reporter = Reporter::new().with_sink(Arc::new(move |e: &ActionEvent| {
        if matches!(
            e,
            ActionEvent::StepResult {
                step: JournalStep::ArrManualImport,
                ..
            }
        ) {
            qbit.add('c', &media, OLD_MEDIA, &[(OLD_MEDIA, OLD_MEDIA)]);
        }
    }));
    let (report, counting) = run_with(&lab, plan, reporter).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(codes(&report.actions[1].outcome).contains(&BlockerCode::TorrentReferencesTarget));
    assert_eq!(counting.mutations(), 0);
    assert!(lab.media_path(OLD_MEDIA).exists());

    // A sample is refused by the runner itself, before anything runs.
    let lab = fixture().await;
    lab.arr.with(|s| s.upgrade_deletes_old = false);
    fs::create_dir_all(lab.media_path("Movies/Sample")).unwrap();
    fs::write(lab.media_path("Movies/Sample/s.mkv"), b"sample").unwrap();
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.delete_media_of(1, 0, "Movies/Sample/s.mkv"),
        ],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan).await;
    assert!(
        codes(&report.actions[1].outcome).contains(&BlockerCode::Sample),
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
}

#[tokio::test]
async fn t4_09_a_season_replacement_is_all_or_nothing_per_unit() {
    let two = |lab: &Lab| {
        lab.seal(
            vec![
                lab.replace_multi(
                    0,
                    &[
                        (NEW_SOURCE, 11, OLD_MEDIA),
                        ("New/new2.mkv", 12, "Movies/Rel/b.mkv"),
                    ],
                ),
                lab.delete_media_of(1, 0, OLD_MEDIA),
                lab.delete_media_of(2, 0, "Movies/Rel/b.mkv"),
            ],
            user(),
            vec![],
        )
    };
    // Every episode hardlinked: verified, both old files go.
    let lab = Lab::new().await;
    lab.t4_setup_two().await;
    lab.arr.with(|s| s.upgrade_deletes_old = false);
    let (report, _) = run(&lab, two(&lab)).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(
        report.actions[1].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(
        report.actions[2].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(!lab.media_path(OLD_MEDIA).exists() && !lab.media_path("Movies/Rel/b.mkv").exists());

    // One episode copied: the unit is not verified and no old file is deleted.
    let lab = Lab::new().await;
    lab.t4_setup_two().await;
    lab.arr.with(|s| {
        s.upgrade_deletes_old = false;
        s.effect_by_name
            .insert("new2.mkv".into(), ImportEffect::Copy);
    });
    let (report, counting) = run(&lab, two(&lab)).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
    assert!(matches!(
        report.actions[1].outcome,
        ActionOutcome::NotExecuted { .. }
    ));
    assert!(matches!(
        report.actions[2].outcome,
        ActionOutcome::NotExecuted { .. }
    ));
    assert_eq!(counting.mutations(), 0);
    assert!(lab.media_path(OLD_MEDIA).exists() && lab.media_path("Movies/Rel/b.mkv").exists());

    // One episode moved: needs review, nothing deleted, the torrent stays stopped.
    let lab = Lab::new().await;
    lab.t4_setup_two().await;
    lab.arr.with(|s| {
        s.upgrade_deletes_old = false;
        s.effect_by_name
            .insert("new2.mkv".into(), ImportEffect::Move);
    });
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let (report, counting) = run(&lab, two(&lab)).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::FailedNeedsReview,
        "{report:?}"
    );
    assert!(codes(&report.actions[1].outcome).contains(&BlockerCode::ArrImportMoved));
    assert_eq!(counting.mutations(), 0);
    assert!(report.left_stopped.contains_key(&hash('e')));
    assert_ne!(lab.qbit.state('e'), TorrentState::Uploading);
}
