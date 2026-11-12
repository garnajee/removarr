#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tier 4 (CP2-tier4 T4-01..T4-20): verified release replacement, `DeleteMediaFile`, grab,
//! queue removal and monitoring, on temporary directories with the real `LiveFsMutator`, the
//! real SQLite journal, a fake qBittorrent and a scriptable ARR that really hardlinks, copies
//! or moves files.

mod actions_common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use actions_common::arrfake::{ImportEffect, media_ref, queue_item};
use actions_common::tier4::{NEW_SOURCE, OLD_MEDIA, search_result};
use actions_common::*;
use removarr_domain::arr::{ArrService, CommandState, OldFileRecovery};
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome, VerificationStatus};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{HashedPlan, PlanOrigin, PlanStatus, Precondition};
use removarr_domain::ports::{CancelFlag, JournalStore};
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{
    ActionEvent, ExecutionReport, LeftStoppedReason, Mutators, Reporter, execute, recover,
};

fn user() -> PlanOrigin {
    PlanOrigin::User
}

fn old_path(lab: &Lab) -> RootedPath {
    rp(&lab.media, OLD_MEDIA)
}

async fn run(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
    ack: Vec<RootedPath>,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = lab.ticket_ack(plan, simulate, ack).await;
    let (mutators, counting) = lab.live();
    let report = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    (report, counting)
}

fn ino(p: std::path::PathBuf) -> (u64, u64) {
    let m = fs::symlink_metadata(p).unwrap();
    (m.dev(), m.ino())
}

fn blocker_codes(o: &ActionOutcome) -> Vec<BlockerCode> {
    match o {
        ActionOutcome::NotExecuted { blockers } => {
            blockers.iter().map(|b| b.code.clone()).collect()
        }
        other => panic!("{other:?}"),
    }
}

async fn fixture() -> Lab {
    let lab = Lab::new().await;
    lab.t4_setup().await;
    lab
}

#[tokio::test]
async fn t4_06_a_hardlinked_import_is_verified_per_inode_and_the_torrent_restarts() {
    let lab = fixture().await;
    let src_inode = ino(lab.completed_path(NEW_SOURCE));
    let plan = lab.seal(
        vec![lab.replace_action(0, OldFileRecovery::RecycleBin)],
        user(),
        vec![],
    );
    let id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    // The library file is the completed inode; the source kept its inode (not moved).
    assert_eq!(ino(lab.media_path("Movies/New/new.mkv")), src_inode);
    assert_eq!(ino(lab.completed_path(NEW_SOURCE)), src_inode);
    assert!(
        !lab.media_path(OLD_MEDIA).exists(),
        "ARR's upgrade removed the old file"
    );
    assert_eq!(lab.arr.calls("manual_import"), 1);
    assert_eq!(counting.mutations(), 0, "the runner itself deletes nothing");
    assert_eq!(
        lab.steps(id).await,
        vec![
            JournalStep::QbitStop,
            JournalStep::ArrManualImport,
            JournalStep::QbitStart
        ]
    );
    assert_eq!(
        lab.qbit.state('e'),
        TorrentState::Uploading,
        "restarted untouched"
    );
    assert!(lab.qbit.calls_of("recheck").is_empty());
    // Every re-read passed: ARR's file, the source inode, the library inode, the command, the
    // old file's accounting.
    let v = &report.actions[0].verification;
    assert!(
        v.iter().all(|r| r.status == VerificationStatus::Passed),
        "{v:?}"
    );
    assert!(v.len() >= 5);
    // Hardlinked: nothing was added.
    assert_eq!(lab.repos.gains.totals().await.unwrap().added, 0);
}

#[tokio::test]
async fn t4_02_t4_05_an_import_that_moves_the_source_needs_review_and_cancels_the_rest() {
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.effect = ImportEffect::Move;
        s.upgrade_deletes_old = false;
    });
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.delete_media_action(1, 0),
        ],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::FailedNeedsReview,
        "{report:?}"
    );
    assert_ne!(report.actions[0].outcome, ActionOutcome::Verified);
    assert!(
        report.actions[0]
            .verification
            .iter()
            .any(|r| matches!(&r.status, VerificationStatus::Failed { reason } if reason.contains("ArrImportMoved"))),
        "{:?}",
        report.actions[0].verification
    );
    // The old media file is never deleted behind a moved source.
    assert!(blocker_codes(&report.actions[1].outcome).contains(&BlockerCode::ArrImportMoved));
    assert!(lab.media_path(OLD_MEDIA).exists());
    assert_eq!(counting.mutations(), 0);
    // The affected torrent stays stopped and visible.
    assert_eq!(
        report.left_stopped.get(&hash('e')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert!(lab.qbit.calls_of("start").is_empty());
    assert_eq!(report.status, Some(PlanStatus::Failed));
}

#[tokio::test]
async fn t4_06_t4_08_a_copy_import_is_executed_unverified_with_an_added_cost() {
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.effect = ImportEffect::Copy;
        s.hardlinks_enabled = false;
        s.upgrade_deletes_old = false;
    });
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.delete_media_action(1, 0),
        ],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
    assert_ne!(report.actions[0].outcome, ActionOutcome::Verified);
    // DeleteMediaFile is blocked and the old file stays.
    assert!(matches!(
        report.actions[1].outcome,
        ActionOutcome::NotExecuted { .. }
    ));
    assert!(lab.media_path(OLD_MEDIA).exists());
    assert_eq!(counting.mutations(), 0);
    // The torrent data is intact: restarted, no recheck.
    assert_eq!(lab.qbit.state('e'), TorrentState::Uploading);
    // The new bytes are an added cost, recorded once.
    let totals = lab.repos.gains.totals().await.unwrap();
    assert_eq!(totals.added, 8192);
    assert_eq!(totals.attributable, 0);
}

#[tokio::test]
async fn t4_06_a_command_that_does_not_complete_in_time_is_never_a_success() {
    let lab = fixture().await;
    lab.arr.with(|s| s.command_state = CommandState::Started);
    let plan = lab.seal(
        vec![lab.replace_action(0, OldFileRecovery::RecycleBin)],
        user(),
        vec![],
    );
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
}

#[tokio::test]
async fn t4_13_the_old_media_file_goes_only_after_a_verified_import_through_the_dedicated_entry() {
    let lab = fixture().await;
    // ARR keeps the old file (a different release name): the deletion companion removes it.
    lab.arr.with(|s| s.upgrade_deletes_old = false);
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.delete_media_action(1, 0),
        ],
        user(),
        vec![],
    );
    let id = plan.plan().id;
    let old_size = lab.sig_len_of_old();
    let (report, counting) = run(&lab, plan, false, vec![]).await;
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
    assert!(!lab.media_path(OLD_MEDIA).exists());
    assert!(
        lab.media_path("Movies/New/new.mkv").exists(),
        "the new file is never touched"
    );
    assert_eq!(
        counting
            .media_removes
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        counting.removes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "never remove_file"
    );
    assert_eq!(
        lab.steps(id).await,
        vec![
            JournalStep::QbitStop,
            JournalStep::ArrManualImport,
            JournalStep::QbitStart,
            JournalStep::FsRemoveFile
        ]
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    // The old file's inode was freed (nlink 1): an attributable deletion gain.
    assert_eq!(
        lab.repos.gains.totals().await.unwrap().attributable,
        old_size
    );
}

#[tokio::test]
async fn t4_13_a_deletion_that_does_not_follow_a_verified_import_is_refused() {
    // The import is refused (the preview rejects): the deletion never runs.
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
    lab.arr.with(|s| {
        for v in s.previews.values_mut() {
            for d in v {
                d.approved = false;
                d.rejections = vec!["not an upgrade".into()];
            }
        }
    });
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert!(blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::ArrRejected));
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert!(lab.media_path(OLD_MEDIA).exists());
}

fn one_replace(lab: &Lab, rec: OldFileRecovery) -> HashedPlan {
    lab.seal(vec![lab.replace_action(0, rec)], user(), vec![])
}

#[tokio::test]
async fn t4_01_gates_capability_families_and_missing_preconditions_refuse_before_any_intent() {
    use removarr_domain::policy::Capability;
    // The capability is off: refused by the runner itself, whatever the adapter would say.
    let lab = fixture().await;
    lab.capabilities
        .lock()
        .unwrap()
        .remove(&Capability::ArrManualImport);
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    let id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        blocker_codes(&report.actions[0].outcome),
        vec![BlockerCode::CapabilityDisabled]
    );
    assert!(lab.steps(id).await.is_empty());
    assert_eq!(
        counting.mutations() + lab.qbit.mutation_count() + lab.service_mutations(),
        0
    );

    // A tier-4 plan never mixes families.
    let lab = fixture().await;
    let plan = lab.seal(
        vec![
            lab.replace_action(0, OldFileRecovery::RecycleBin),
            lab.monitor_action(1, false),
        ],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert!(blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::NotInBuild));
    assert_eq!(counting.mutations() + lab.service_mutations(), 0);

    // T4-03: every mandatory precondition must be in the plan, whatever the planner did.
    for drop in 0..4 {
        let lab = fixture().await;
        let mut a = lab.replace_action(0, OldFileRecovery::RecycleBin);
        a.preconditions.retain(|p| match drop {
            0 => !matches!(p, Precondition::ArrFingerprint { .. }),
            1 => !matches!(p, Precondition::NoArrImport { .. }),
            2 => !matches!(p, Precondition::OldCoverageRecoverable { .. }),
            _ => !matches!(p, Precondition::SameDevice { .. }),
        });
        let plan = lab.seal(vec![a], user(), vec![]);
        let id = plan.plan().id;
        let (report, counting) = run(&lab, plan, false, vec![]).await;
        assert!(
            matches!(report.actions[0].outcome, ActionOutcome::NotExecuted { .. }),
            "drop {drop}: {report:?}"
        );
        assert!(lab.steps(id).await.is_empty(), "drop {drop}");
        assert_eq!(
            counting.mutations() + lab.qbit.mutation_count() + lab.service_mutations(),
            0
        );
    }
}

#[tokio::test]
async fn t4_03_a_changed_score_profile_or_hardlink_setting_invalidates_the_plan() {
    for change in 0..3 {
        let lab = fixture().await;
        let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
        let id = plan.plan().id;
        lab.arr.with(|s| match change {
            0 => s.hardlinks_enabled = false,
            1 => s.recycle_bin = true,
            _ => s.files[0].cf_score = 99,
        });
        let (report, counting) = run(&lab, plan, false, vec![]).await;
        assert_eq!(
            report.status,
            Some(PlanStatus::Invalidated),
            "change {change}"
        );
        assert!(matches!(
            report.actions[0].outcome,
            ActionOutcome::NotExecuted { .. }
        ));
        assert_eq!(lab.arr.calls("manual_import"), 0);
        assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
        assert!(lab.steps(id).await.is_empty());
    }
}

#[tokio::test]
async fn t4_10_a_numbering_difference_in_the_execution_time_preview_refuses() {
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    lab.arr.with(|s| {
        for v in s.previews.values_mut() {
            v[0].episode_ids = vec![12];
        }
    });
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert!(blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::NumberingMismatch));
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
}

#[tokio::test]
async fn t4_04_one_importer_per_media_checked_again_right_before_the_command() {
    // Already importing at revalidation.
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.queue = vec![queue_item(7, None, "importing", Some(media_ref(5, &[11])))];
    });
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert!(blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::ImportInProgress));
    assert_eq!(lab.arr.calls("manual_import"), 0);

    // An importer that appears after the second revalidation: the last read refuses, the
    // stopped torrent is started again, nothing is sent.
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    let arr = lab.arr.clone();
    let reporter = Reporter::new().with_sink(Arc::new(move |e: &ActionEvent| {
        if matches!(e, ActionEvent::Revalidated { pass: 2, .. }) {
            arr.with(|s| {
                s.queue = vec![queue_item(8, None, "importing", Some(media_ref(5, &[11])))];
            });
        }
    }));
    let ticket = lab.ticket_ack(plan, false, vec![]).await;
    let (mutators, _) = lab.live();
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
    assert!(blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::ImportInProgress));
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert_eq!(lab.qbit.state('e'), TorrentState::Uploading, "restored");
}

#[tokio::test]
async fn t4_07_unrecoverable_old_files_need_each_acknowledgement_and_other_links_keep_them() {
    // Unrecoverable and acknowledged: executes; the old file is accounted as acknowledged.
    let lab = fixture().await;
    let old = old_path(&lab);
    let plan = one_replace(&lab, OldFileRecovery::Unrecoverable);
    let (report, _) = run(&lab, plan, false, vec![old.clone()]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );

    // The same plan whose approval does not name the file: the runner refuses it itself.
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::Unrecoverable);
    let mut ticket = lab.ticket_ack(plan, false, vec![old_path(&lab)]).await;
    ticket
        .approval
        .as_mut()
        .unwrap()
        .acknowledged_unrecoverable
        .clear();
    let (mutators, counting) = lab.live();
    let report = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    assert!(
        blocker_codes(&report.actions[0].outcome).contains(&BlockerCode::OldCoverageUnrecoverable)
    );
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    assert!(lab.media_path(OLD_MEDIA).exists());

    // Another retained link keeps the old inode alive: accounted, verified.
    let lab = fixture().await;
    fs::create_dir_all(lab.completed_path("Keep")).unwrap();
    fs::hard_link(
        lab.media_path(OLD_MEDIA),
        lab.completed_path("Keep/old.mkv"),
    )
    .unwrap();
    let other = rp(&RootId::completed(), "Keep/old.mkv");
    let plan = one_replace(&lab, OldFileRecovery::OtherLink(other.clone()));
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(lab.completed_path("Keep/old.mkv").exists());

    // The other link vanishes before ARR runs: nothing keeps the old content, not verified.
    let lab = fixture().await;
    fs::create_dir_all(lab.completed_path("Keep")).unwrap();
    fs::hard_link(
        lab.media_path(OLD_MEDIA),
        lab.completed_path("Keep/old.mkv"),
    )
    .unwrap();
    let other = rp(&RootId::completed(), "Keep/old.mkv");
    let plan = one_replace(&lab, OldFileRecovery::OtherLink(other));
    fs::remove_file(lab.completed_path("Keep/old.mkv")).unwrap();
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{report:?}"
    );
}

#[tokio::test]
async fn t4_12_grab_is_manual_through_a_fresh_search_and_verified_by_the_queue() {
    let lab = fixture().await;
    lab.arr.with(|s| s.search = vec![search_result("g1")]);
    // A search never grabs.
    use removarr_domain::ports::ArrRead;
    lab.arr.search_releases(&media_ref(5, &[11])).await.unwrap();
    assert_eq!(lab.arr.calls("grab"), 0);
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    let id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("grab"), 1);
    assert_eq!(lab.steps(id).await, vec![JournalStep::ArrGrab]);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    // A token that is not in the fresh search: refused, never sent.
    let lab = fixture().await;
    lab.arr.with(|s| s.search = vec![search_result("other")]);
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    let id = plan.plan().id;
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        blocker_codes(&report.actions[0].outcome),
        vec![BlockerCode::ReleaseNotInSearch]
    );
    assert_eq!(lab.arr.calls("grab"), 0);
    assert!(lab.steps(id).await.is_empty());

    // The call errors after ARR queued the release: judged by the queue, never sent twice.
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.search = vec![search_result("g1")];
        s.grab_then_fail = true;
    });
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("grab"), 1);
}

#[tokio::test]
async fn t4_16_queue_removal_only_for_a_proven_stale_item_and_monitoring_journals_the_previous_value()
 {
    // Stale: its download is not a live torrent.
    let lab = fixture().await;
    lab.arr
        .with(|s| s.queue = vec![queue_item(7, Some(hash('c')), "failed", None)]);
    let plan = lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("remove_queue"), 1);

    // The item still points to a live torrent: removing it would hide an error.
    let lab = fixture().await;
    lab.arr
        .with(|s| s.queue = vec![queue_item(7, Some(hash('a')), "failed", None)]);
    let plan = lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        blocker_codes(&report.actions[0].outcome),
        vec![BlockerCode::QueueItemHidesError]
    );
    assert_eq!(lab.arr.calls("remove_queue"), 0);

    // An import in progress is never removed.
    let lab = fixture().await;
    lab.arr
        .with(|s| s.queue = vec![queue_item(7, Some(hash('c')), "importing", None)]);
    let plan = lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        blocker_codes(&report.actions[0].outcome),
        vec![BlockerCode::ImportInProgress]
    );

    // The removal happened although the call errored: observed.
    let lab = fixture().await;
    lab.arr.with(|s| {
        s.queue = vec![queue_item(7, Some(hash('c')), "failed", None)];
        s.remove_then_fail = true;
    });
    let plan = lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("remove_queue"), 1);

    // Monitoring: targeted, previous value journaled, verified by a re-read.
    let lab = fixture().await;
    let plan = lab.seal(vec![lab.monitor_action(0, false)], user(), vec![]);
    let id = plan.plan().id;
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    let journal = lab.journal(id).await;
    assert_eq!(journal[0].0.step, JournalStep::ArrSetMonitored);
    assert_eq!(journal[0].0.observed["previous"], true);
    // The call errored after the change: judged by the flag.
    let lab = fixture().await;
    lab.arr.with(|s| s.monitor_then_fail = true);
    let plan = lab.seal(vec![lab.monitor_action(0, false)], user(), vec![]);
    let (report, _) = run(&lab, plan, false, vec![]).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
}

fn every_tier4_plan(lab: &Lab) -> Vec<HashedPlan> {
    lab.arr.with(|s| {
        s.search = vec![search_result("g1")];
        s.queue = vec![queue_item(7, Some(hash('c')), "failed", None)];
        s.upgrade_deletes_old = false;
    });
    vec![
        lab.seal(
            vec![
                lab.replace_action(0, OldFileRecovery::RecycleBin),
                lab.delete_media_action(1, 0),
            ],
            user(),
            vec![],
        ),
        lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]),
        lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]),
        lab.seal(vec![lab.monitor_action(0, false)], user(), vec![]),
    ]
}

#[tokio::test]
async fn t4_18_simulating_every_tier4_kind_mutates_nothing_in_either_process() {
    for dry_run_process in [true, false] {
        let lab = fixture().await;
        let plans = every_tier4_plan(&lab);
        let (rec_mutators, recording) =
            Mutators::recording(&[ArrService::Sonarr, ArrService::Radarr]);
        let (live_mutators, counting) = lab.live();
        let src = ino(lab.completed_path(NEW_SOURCE));
        for plan in plans {
            let id = plan.plan().id;
            let ticket = lab.ticket_ack(plan, true, vec![]).await;
            let m = if dry_run_process {
                &rec_mutators
            } else {
                &live_mutators
            };
            let report = execute(
                &lab.ports(),
                m,
                &lab.settings(),
                ticket,
                &CancelFlag::new(),
                &Reporter::new(),
            )
            .await
            .unwrap();
            assert!(report.simulated);
            assert!(
                report
                    .actions
                    .iter()
                    .all(|a| a.outcome == ActionOutcome::Simulated),
                "{report:?}"
            );
            let journal = lab.journal(id).await;
            assert!(!journal.is_empty());
            for (i, r) in &journal {
                assert_eq!(i.mode, removarr_domain::journal::JournalMode::Simulated);
                assert_eq!(r.as_ref().unwrap().outcome, StepOutcome::Simulated);
            }
            assert_eq!(lab.status(id).await, PlanStatus::Approved);
        }
        assert_eq!(recording.calls(), 0);
        assert_eq!(counting.mutations(), 0);
        assert_eq!(lab.qbit.mutation_count(), 0);
        assert_eq!(lab.service_mutations(), 0);
        assert_eq!(lab.arr.calls("manual_import") + lab.arr.calls("grab"), 0);
        assert_eq!(ino(lab.completed_path(NEW_SOURCE)), src);
        assert!(lab.media_path(OLD_MEDIA).exists());
        assert!(!lab.media_path("Movies/New/new.mkv").exists());
        assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());
    }
}

// ------------------------------------------------------------------ crash boundaries

async fn crash_at(lab: &Lab, plan: HashedPlan, point: CrashPoint) -> removarr_domain::ids::PlanId {
    let ticket = lab.ticket_ack(plan, false, vec![]).await;
    let plan_id = ticket.plan.plan().id;
    lab.hang.arm(point);
    let (mutators, _) = lab.live();
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
        .expect("the crash point is reached");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lab.hang.disarm();
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executing);
    plan_id
}

async fn recovered(lab: &Lab) -> removarr_workflows::actions::RecoveryReport {
    let (mutators, _) = lab.live();
    recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn crash_boundaries_around_the_import_never_resend_and_decide_from_observation() {
    // Before the intent: nothing sent, the stopped torrent resumes.
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    let id = crash_at(
        &lab,
        plan,
        CrashPoint::BeforeIntent(JournalStep::ArrManualImport),
    )
    .await;
    let r = recovered(&lab).await;
    assert_eq!(lab.arr.calls("manual_import"), 0);
    assert!(matches!(
        r.plans[0].actions[0].1,
        ActionOutcome::NotExecuted { .. }
    ));
    assert_eq!(lab.qbit.state('e'), TorrentState::Uploading);
    assert_eq!(lab.status(id).await, PlanStatus::Interrupted);

    // After the intent, before the command: possibly sent, never sent again; data intact so
    // the torrent resumes without a recheck.
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    crash_at(
        &lab,
        plan,
        CrashPoint::AfterIntent(JournalStep::ArrManualImport),
    )
    .await;
    let r = recovered(&lab).await;
    assert_eq!(
        lab.arr.calls("manual_import"),
        0,
        "recovery never sends a command"
    );
    assert!(matches!(
        r.plans[0].actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));
    assert_eq!(lab.qbit.state('e'), TorrentState::Uploading);
    assert!(lab.qbit.calls_of("recheck").is_empty());
    assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());

    // After the command (hardlink), before the result: sources intact.
    let lab = fixture().await;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    crash_at(
        &lab,
        plan,
        CrashPoint::BeforeResult(JournalStep::ArrManualImport),
    )
    .await;
    assert_eq!(lab.arr.calls("manual_import"), 1);
    let r = recovered(&lab).await;
    assert_eq!(lab.arr.calls("manual_import"), 1, "never re-issued");
    assert!(matches!(
        r.plans[0].actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));
    assert_eq!(lab.qbit.state('e'), TorrentState::Uploading);

    // After a move, before the result: needs review, the torrent stays stopped.
    let lab = fixture().await;
    lab.arr.with(|s| s.effect = ImportEffect::Move);
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan = one_replace(&lab, OldFileRecovery::RecycleBin);
    crash_at(
        &lab,
        plan,
        CrashPoint::BeforeResult(JournalStep::ArrManualImport),
    )
    .await;
    let r = recovered(&lab).await;
    assert_eq!(r.plans[0].actions[0].1, ActionOutcome::FailedNeedsReview);
    assert_eq!(
        r.plans[0].left_stopped.get(&hash('e')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert!(lab.qbit.calls_of("start").is_empty());
    assert!(
        r.plans[0]
            .notes
            .iter()
            .any(|n| n.contains("ArrImportMoved")),
        "{:?}",
        r.plans[0].notes
    );
}

#[tokio::test]
async fn crash_boundaries_around_the_old_media_deletion_are_decided_from_the_disk() {
    for (point, removed) in [
        (CrashPoint::AfterIntent(JournalStep::FsRemoveFile), false),
        (CrashPoint::BeforeResult(JournalStep::FsRemoveFile), true),
    ] {
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
        crash_at(&lab, plan, point).await;
        assert_eq!(!lab.media_path(OLD_MEDIA).exists(), removed);
        let r = recovered(&lab).await;
        let outcome = &r.plans[0].actions.iter().find(|(i, _)| *i == 1).unwrap().1;
        if removed {
            assert!(
                matches!(outcome, ActionOutcome::ExecutedUnverified { .. }),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(outcome, ActionOutcome::NotExecuted { .. }),
                "{outcome:?}"
            );
            assert!(lab.media_path(OLD_MEDIA).exists(), "recovery never deletes");
        }
        assert_eq!(lab.arr.calls("manual_import"), 1, "never re-issued");
        assert!(lab.media_path("Movies/New/new.mkv").exists());
    }
}

#[tokio::test]
async fn crash_boundaries_around_grab_queue_removal_and_monitoring_never_resend() {
    // Grab: sent, then crash before the result: possibly executed, never grabbed again.
    let lab = fixture().await;
    lab.arr.with(|s| s.search = vec![search_result("g1")]);
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    crash_at(&lab, plan, CrashPoint::BeforeResult(JournalStep::ArrGrab)).await;
    let r = recovered(&lab).await;
    assert_eq!(lab.arr.calls("grab"), 1);
    assert!(matches!(
        r.plans[0].actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));
    // Grab: crash before the POST: still never re-sent by recovery.
    let lab = fixture().await;
    lab.arr.with(|s| s.search = vec![search_result("g1")]);
    let plan = lab.seal(vec![lab.grab_action(0, "g1")], user(), vec![]);
    crash_at(&lab, plan, CrashPoint::AfterIntent(JournalStep::ArrGrab)).await;
    let r = recovered(&lab).await;
    assert_eq!(lab.arr.calls("grab"), 0);
    assert!(matches!(
        r.plans[0].actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));

    // Queue removal: decided from the queue.
    for (point, gone) in [
        (
            CrashPoint::AfterIntent(JournalStep::ArrRemoveQueueItem),
            false,
        ),
        (
            CrashPoint::BeforeResult(JournalStep::ArrRemoveQueueItem),
            true,
        ),
    ] {
        let lab = fixture().await;
        lab.arr
            .with(|s| s.queue = vec![queue_item(7, Some(hash('c')), "failed", None)]);
        let plan = lab.seal(vec![lab.queue_action(0, 7)], user(), vec![]);
        crash_at(&lab, plan, point).await;
        let r = recovered(&lab).await;
        assert_eq!(lab.arr.calls("remove_queue"), usize::from(gone));
        if gone {
            assert!(matches!(
                r.plans[0].actions[0].1,
                ActionOutcome::ExecutedUnverified { .. }
            ));
        } else {
            assert!(matches!(
                r.plans[0].actions[0].1,
                ActionOutcome::NotExecuted { .. }
            ));
        }
    }

    // Monitoring: decided from the flag, against the journaled previous value.
    for (point, changed) in [
        (CrashPoint::AfterIntent(JournalStep::ArrSetMonitored), false),
        (CrashPoint::BeforeResult(JournalStep::ArrSetMonitored), true),
    ] {
        let lab = fixture().await;
        let plan = lab.seal(vec![lab.monitor_action(0, false)], user(), vec![]);
        crash_at(&lab, plan, point).await;
        let r = recovered(&lab).await;
        assert_eq!(lab.arr.calls("set_monitored"), usize::from(changed));
        if changed {
            assert!(matches!(
                r.plans[0].actions[0].1,
                ActionOutcome::ExecutedUnverified { .. }
            ));
        } else {
            assert!(matches!(
                r.plans[0].actions[0].1,
                ActionOutcome::NotExecuted { .. }
            ));
        }
    }
}
