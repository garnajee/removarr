#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tier-3 deletions under the completed root with the real `LiveFsMutator`, the real SQLite
//! journal and the fake qBittorrent: torrent-held file deletion with task removal (T-32),
//! folder deletion all-or-nothing with samples (T-15/T-17/T3-4/T3-6), junk only outside
//! torrents by path and inode (T3-7), empty-parent companions (INV-49), task removal refused
//! while a wanted file remains (T3-9), left-stopped partially affected torrents (T-23),
//! revalidation after the stop, media-root refusals, and the simulation of every tier-3 kind
//! with zero mutations (T-30, T3-16).

mod actions_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use actions_common::*;
use removarr_domain::arr::ArrService;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalMode, JournalStep, StepOutcome};
use removarr_domain::path::RootId;
use removarr_domain::plan::{ActionKind, HashedPlan, PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::ports::{CancelFlag, JournalStore};
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{
    ActionEvent, ExecutionReport, LeftStoppedReason, Mutators, Reporter, execute,
};

fn user() -> PlanOrigin {
    PlanOrigin::User
}

async fn run(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
) -> (ExecutionReport, Arc<CountingMutator>) {
    run_with(lab, plan, simulate, Reporter::new()).await
}

async fn run_with(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
    reporter: Reporter,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = lab.ticket(plan, simulate).await;
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
async fn t32_a_torrent_held_file_is_deleted_then_its_task_removed_without_delete_files() {
    let lab = Lab::new().await;
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
        user(),
        vec![],
    );
    let plan_id = plan.plan().id;
    let size = lab.sig(&rp(&RootId::completed(), "Rel/a.mkv")).size;
    let (report, counting) = run(&lab, plan, false).await;
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
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executed);
    assert!(!lab.completed_path("Rel/a.mkv").exists());
    assert!(
        lab.media_path("Movies/Rel/a.mkv").exists(),
        "media untouched"
    );
    assert!(
        lab.completed_path("Rel/b.mkv").exists(),
        "sibling untouched"
    );
    assert!(!lab.qbit.listed('a'), "the task is gone");
    assert_eq!(lab.qbit.calls_of("remove"), vec![[hash('a')].into()]);
    assert_eq!(lab.qbit.calls_of("stop"), vec![[hash('a')].into()]);
    assert!(
        lab.qbit.calls_of("start").is_empty(),
        "a removed task is never started"
    );
    assert!(lab.qbit.calls_of("recheck").is_empty());
    assert_eq!(counting.removes.load(Ordering::SeqCst), 1);
    // Journal: stop, remove file (Done, observed), remove task (Done); every intent durable
    // before its mutation and with a result.
    let journal = lab.journal(plan_id).await;
    assert_eq!(
        journal.iter().map(|(i, _)| i.step).collect::<Vec<_>>(),
        vec![
            JournalStep::QbitStop,
            JournalStep::FsRemoveFile,
            JournalStep::QbitRemoveKeepData
        ]
    );
    for (i, r) in &journal {
        assert_eq!(i.mode, JournalMode::Live);
        assert!(
            matches!(r.as_ref().unwrap().outcome, StepOutcome::Done { .. }),
            "{i:?}"
        );
    }
    assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());
    // One attributable deletion gain for the removed inode.
    assert_eq!(lab.repos.gains.totals().await.unwrap().attributable, size);
    assert!(report.left_stopped.is_empty());
}

#[tokio::test]
async fn t23_a_torrent_that_lost_a_file_without_task_removal_is_left_stopped_and_reported() {
    let lab = Lab::new().await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let plan = lab.seal(
        vec![lab.delete_file_action(
            0,
            ActionKind::DeleteCompletedFile,
            &RootId::completed(),
            "Rel/a.mkv",
            &['a'],
        )],
        user(),
        vec![],
    );
    let (report, _) = run(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(!lab.completed_path("Rel/a.mkv").exists());
    assert_eq!(
        report.left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert!(lab.qbit.listed('a'), "never removed without the companion");
    assert_eq!(lab.qbit.state('a'), TorrentState::MissingFiles);
    assert!(lab.qbit.calls_of("remove").is_empty());
    assert!(lab.qbit.calls_of("start").is_empty());
    assert!(report.events.iter().any(|e| matches!(
        e,
        ActionEvent::TorrentsLeftStopped { hashes, .. } if hashes.contains_key(&hash('a'))
    )));
}

#[tokio::test]
async fn t15_folder_deletion_is_all_or_nothing_and_samples_need_their_acknowledgement() {
    // The release folder with its annexes and sample, every sample acknowledged.
    let lab = Lab::new().await;
    let sample = rp(&RootId::completed(), "Orphan/Sample/s.mkv");
    let plan = lab.seal(
        vec![lab.delete_folder_action(0, "Orphan")],
        user(),
        vec![sample.clone()],
    );
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(!lab.completed_path("Orphan").exists());
    assert!(lab.completed_path("Rel/a.mkv").exists());
    assert_eq!(counting.trees.load(Ordering::SeqCst), 1);
    assert_eq!(
        lab.steps(plan_id).await,
        vec![JournalStep::FsRemoveTree],
        "torrent-less: no stop, no start"
    );
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert!(lab.repos.gains.totals().await.unwrap().attributable > 0);

    // T3-6: a plan that does not declare its sample gets no acknowledgement; the runner
    // re-checks the rule at execution and refuses before any intent.
    let lab = Lab::new().await;
    let plan = lab.seal(vec![lab.delete_folder_action(0, "Orphan")], user(), vec![]);
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::Sample
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert!(lab.steps(plan_id).await.is_empty());
    assert_eq!(counting.mutations(), 0);
    assert!(lab.completed_path("Orphan/Sample/s.mkv").exists());

    // T3-4: an entry added inside the folder after planning refuses the whole folder.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.delete_folder_action(0, "Orphan")],
        user(),
        vec![sample.clone()],
    );
    fs::write(lab.completed_path("Orphan/Subs/late.srt"), b"late").unwrap();
    let (report, counting) = run(&lab, plan, false).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::ChangedSincePlanning
    );
    assert_eq!(counting.mutations(), 0);
    assert!(lab.completed_path("Orphan/Orphan.mkv").exists());
}

#[tokio::test]
async fn a_partially_removed_tree_needs_review_and_its_hashes_stay_stopped() {
    if is_root_user() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let lab = Lab::new().await;
    // Torrent `c` holds the folder; `Orphan` is made non-writable so that the deepest
    // entries go and `rmdir` of a subdirectory fails (deepest first, stop at once).
    lab.add_holder('c', &RootId::completed(), "Orphan");
    fs::set_permissions(
        lab.completed_path("Orphan"),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    lab.store_observation_again().await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Failed;
    let sample = rp(&RootId::completed(), "Orphan/Sample/s.mkv");
    let mut a = lab.delete_folder_action(0, "Orphan");
    a.preconditions.retain(|p| {
        !matches!(
            p,
            removarr_domain::plan::Precondition::NoTorrentReferences { .. }
        )
    });
    let (pins, _) = lab.torrent_pins(&['c']);
    a.preconditions.extend(pins);
    a.torrent_hashes = [hash('c')].into();
    a.verification
        .push(removarr_domain::plan::VerificationStep::RecheckVerified {
            hashes: [hash('c')].into(),
        });
    let plan = lab.seal(vec![a], user(), vec![sample]);
    let plan_id = plan.plan().id;
    let (report, _) = run(&lab, plan, false).await;
    fs::set_permissions(
        lab.completed_path("Orphan"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::FailedNeedsReview,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Failed));
    assert!(lab.completed_path("Orphan/Orphan.mkv").exists());
    assert!(
        !lab.completed_path("Orphan/Subs/o.srt").exists(),
        "deepest entries went first"
    );
    assert_eq!(
        report.left_stopped.get(&hash('c')),
        Some(&LeftStoppedReason::RecheckFailed)
    );
    assert!(lab.qbit.calls_of("start").is_empty());
    // The journal says what the disk shows: a Done result that did not remove everything.
    let journal = lab.journal(plan_id).await;
    let (_, tree) = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsRemoveTree)
        .unwrap();
    let StepOutcome::Done { final_state } = &tree.as_ref().unwrap().outcome else {
        panic!("{tree:?}");
    };
    assert_eq!(final_state["removed"], false);
    assert!(
        !final_state["tree"]["removed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !final_state["tree"]["remaining"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn t3_7_junk_is_deleted_only_when_no_torrent_references_it_by_path_or_inode() {
    // Torrent-less junk: deleted and verified.
    let lab = Lab::new().await;
    let (report, counting) = run(&lab, lab.junk_plan(), false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(!lab.completed_path("Rel/a.nfo").exists());
    assert_eq!(counting.removes.load(Ordering::SeqCst), 1);
    assert_eq!(lab.qbit.mutation_count(), 0, "no torrent touched");

    // The same inode as a torrent-held file (a hardlink of `Rel/b.mkv`): refused by inode.
    let lab = Lab::new().await;
    fs::hard_link(
        lab.completed_path("Rel/b.mkv"),
        lab.completed_path("Rel/b.nfo"),
    )
    .unwrap();
    lab.store_observation_again().await;
    let plan = lab.seal(
        vec![lab.delete_file_action(
            0,
            ActionKind::DeleteJunk,
            &RootId::completed(),
            "Rel/b.nfo",
            &[],
        )],
        user(),
        vec![],
    );
    let (report, counting) = run(&lab, plan, false).await;
    assert!(
        blockers(&report.actions[0].outcome)
            .iter()
            .any(|b| b.code == BlockerCode::TorrentReferencesTarget),
        "{report:?}"
    );
    assert_eq!(counting.mutations(), 0);
    assert!(lab.completed_path("Rel/b.nfo").exists());

    // A torrent that started holding the path after planning: refused by path.
    let lab = Lab::new().await;
    let plan = lab.junk_plan();
    lab.add_holder('c', &RootId::completed(), "Rel/a.nfo");
    let (report, counting) = run(&lab, plan, false).await;
    assert!(
        blockers(&report.actions[0].outcome)
            .iter()
            .any(|b| b.code == BlockerCode::TorrentReferencesTarget),
        "{report:?}"
    );
    assert_eq!(counting.mutations(), 0);

    // qBittorrent unreachable: blocked, nothing deleted.
    let lab = Lab::new().await;
    let plan = lab.junk_plan();
    *lab.qbit.fail_reads.lock().unwrap() = true;
    let (report, counting) = run(&lab, plan, false).await;
    assert!(
        blockers(&report.actions[0].outcome)
            .iter()
            .any(|b| matches!(b.code, BlockerCode::ServiceUnavailable(_))),
        "{report:?}"
    );
    assert_eq!(counting.mutations(), 0);
    assert!(lab.completed_path("Rel/a.nfo").exists());
}

#[tokio::test]
async fn inv49_empty_parents_are_removed_after_their_contents_and_only_when_empty() {
    // `Orphan/Subs/o.srt` then its parent `Orphan/Subs`; `Orphan` itself stays.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![
            lab.delete_file_action(
                0,
                ActionKind::DeleteJunk,
                &RootId::completed(),
                "Orphan/Subs/o.srt",
                &[],
            ),
            lab.empty_parent_action(1, "Orphan/Subs", &[0]),
        ],
        user(),
        vec![],
    );
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
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
    assert!(!lab.completed_path("Orphan/Subs").exists());
    assert!(lab.completed_path("Orphan/Orphan.mkv").exists());
    assert_eq!(counting.empty_dirs.load(Ordering::SeqCst), 1);
    assert_eq!(
        lab.steps(plan_id).await,
        vec![JournalStep::FsRemoveFile, JournalStep::FsRemoveEmptyDir]
    );

    // A parent that is not empty at the call is refused (rmdir semantics), nothing removed.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.empty_parent_action(0, "Orphan/Subs", &[])],
        user(),
        vec![],
    );
    let (report, _) = run(&lab, plan, false).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(lab.completed_path("Orphan/Subs/o.srt").exists());

    // A companion whose dependency did not execute is not executed either (manual plans
    // continue past the failure; the dependency chain does not).
    if !is_root_user() {
        let lab = Lab::new().await;
        // Read-only before planning: the pin carries that mode, `unlinkat` is refused.
        fs::set_permissions(
            lab.completed_path("Orphan/Subs"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let plan = lab.seal(
            vec![
                lab.delete_file_action(
                    0,
                    ActionKind::DeleteJunk,
                    &RootId::completed(),
                    "Orphan/Subs/o.srt",
                    &[],
                ),
                lab.empty_parent_action(1, "Orphan/Subs", &[0]),
                lab.delete_file_action(
                    2,
                    ActionKind::DeleteJunk,
                    &RootId::completed(),
                    "Rel/a.nfo",
                    &[],
                ),
            ],
            user(),
            vec![],
        );
        let (report, _) = run(&lab, plan, false).await;
        fs::set_permissions(
            lab.completed_path("Orphan/Subs"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(
            matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
            "{report:?}"
        );
        assert_eq!(
            blockers(&report.actions[1].outcome)[0].code,
            BlockerCode::Interrupted
        );
        assert_eq!(
            report.actions[2].outcome,
            ActionOutcome::Verified,
            "manual plans continue"
        );
        assert_eq!(report.status, Some(PlanStatus::PartiallyExecuted));
    }
}

#[tokio::test]
async fn t3_9_a_task_is_kept_while_any_wanted_file_remains() {
    let lab = Lab::new().await;
    // The companion names `b`, whose wanted file `Rel/b.mkv` is not part of the deletions.
    let plan = lab.seal(
        vec![
            lab.delete_file_action(
                0,
                ActionKind::DeleteCompletedFile,
                &RootId::completed(),
                "Rel/a.mkv",
                &['a'],
            ),
            lab.remove_task_action(1, 'b', &[0]),
        ],
        user(),
        vec![],
    );
    let (report, _) = run(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    let bl = blockers(&report.actions[1].outcome);
    assert_eq!(bl[0].code, BlockerCode::ChangedSincePlanning, "{bl:?}");
    assert!(bl[0].detail.contains("still present"));
    assert!(lab.qbit.calls_of("remove").is_empty(), "never removed");
    assert!(lab.qbit.listed('b'));
    assert_eq!(
        lab.qbit.state('b'),
        TorrentState::Uploading,
        "untouched: started again"
    );
    assert!(lab.completed_path("Rel/b.mkv").exists());
}

#[tokio::test]
async fn revalidation_after_the_stop_restores_the_torrent_and_media_targets_are_refused() {
    // A change injected right after the stop (pass 2 catches it): invalidated, resumed.
    let lab = Lab::new().await;
    let plan = lab.seal(
        vec![lab.delete_file_action(
            0,
            ActionKind::DeleteCompletedFile,
            &RootId::completed(),
            "Rel/a.mkv",
            &['a'],
        )],
        user(),
        vec![],
    );
    let path = lab.completed_path("Rel/a.mkv");
    let reporter = Reporter::new().with_sink(Arc::new(move |e: &ActionEvent| {
        if matches!(e, ActionEvent::TorrentsStopped { .. }) {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            std::io::Write::write_all(&mut f, b"x").unwrap();
        }
    }));
    let (report, counting) = run_with(&lab, plan, false, reporter).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::ChangedSincePlanning
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading, "resumed");
    assert!(lab.completed_path("Rel/a.mkv").exists());

    // A media-root target, whatever the plan says: refused before any intent (T3-2).
    let lab = Lab::new().await;
    let mut a = lab.delete_file_action(
        0,
        ActionKind::DeleteCompletedFile,
        &lab.media.clone(),
        "Movies/Rel/a.mkv",
        &[],
    );
    a.preconditions.retain(|p| {
        !matches!(
            p,
            removarr_domain::plan::Precondition::NoTorrentReferences { .. }
        )
    });
    let plan = lab.seal(vec![a], user(), vec![]);
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
    assert_eq!(
        blockers(&report.actions[0].outcome)[0].code,
        BlockerCode::MediaRootDeletion
    );
    assert!(lab.steps(plan_id).await.is_empty());
    assert_eq!(counting.mutations(), 0);
    assert!(lab.media_path("Movies/Rel/a.mkv").exists());
}

/// Every tier-3 kind in one plan per family, simulated.
fn tier3_plans(lab: &Lab, mode: PlanMode) -> Vec<(HashedPlan, Vec<JournalStep>)> {
    let sample = rp(&RootId::completed(), "Orphan/Sample/s.mkv");
    // A junk file alone in its directory: the companion parent removal has something to do.
    fs::create_dir_all(lab.completed_path("Extra")).unwrap();
    fs::write(lab.completed_path("Extra/x.nfo"), b"x").unwrap();
    vec![
        (
            lab.seal_mode(
                vec![
                    lab.delete_file_action(
                        0,
                        ActionKind::DeleteCompletedFile,
                        &RootId::completed(),
                        "Rel/a.mkv",
                        &['a'],
                    ),
                    lab.delete_folder_action(1, "Orphan"),
                    lab.delete_file_action(
                        2,
                        ActionKind::DeleteJunk,
                        &RootId::completed(),
                        "Extra/x.nfo",
                        &[],
                    ),
                    lab.delete_file_action(
                        3,
                        ActionKind::DeleteIncompleteOrphan,
                        &RootId::incomplete(),
                        "lost.mkv.!qB",
                        &[],
                    ),
                    lab.empty_parent_action(4, "Extra", &[2]),
                    lab.remove_task_action(5, 'a', &[0]),
                ],
                mode,
                user(),
                vec![sample],
            ),
            vec![
                JournalStep::QbitStop,
                JournalStep::FsRemoveFile,
                JournalStep::FsRemoveTree,
                JournalStep::FsRemoveFile,
                JournalStep::FsRemoveFile,
                JournalStep::FsRemoveEmptyDir,
                JournalStep::QbitRemoveKeepData,
            ],
        ),
        (
            lab.seal_mode(
                vec![lab.stalled_action(0, 'd', &["Dl"])],
                mode,
                user(),
                vec![],
            ),
            vec![
                JournalStep::QbitStop,
                JournalStep::QbitRemoveKeepData,
                JournalStep::FsRemoveTree,
            ],
        ),
        (
            lab.seal_mode(
                vec![lab.fix_mode_action(0, "Rel/b.mkv", 0o644)],
                mode,
                user(),
                vec![],
            ),
            vec![JournalStep::FsSetMode],
        ),
        (
            lab.seal_mode(vec![lab.rescan_action(0)], mode, user(), vec![]),
            vec![JournalStep::ArrRescan],
        ),
        (
            lab.seal_mode(vec![lab.jellyfin_action(0)], mode, user(), vec![]),
            vec![JournalStep::JellyfinRefresh],
        ),
    ]
}

fn disk_untouched(lab: &Lab) {
    for p in [
        "Rel/a.mkv",
        "Rel/b.mkv",
        "Rel/a.nfo",
        "Orphan/Orphan.mkv",
        "Orphan/Subs/o.srt",
        "Orphan/Sample/s.mkv",
        "Extra/x.nfo",
    ] {
        assert!(lab.completed_path(p).exists(), "{p}");
    }
    assert!(lab.incomplete_path("Dl/part.mkv.!qB").exists());
    assert!(lab.incomplete_path("lost.mkv.!qB").exists());
    assert_eq!(
        fs::metadata(lab.completed_path("Rel/b.mkv"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    for h in ['a', 'b', 'd'] {
        assert!(lab.qbit.listed(h), "{h}");
    }
}

#[tokio::test]
async fn t30_simulating_every_tier3_kind_walks_the_full_path_and_mutates_nothing() {
    // Dry-run process: the recording set (with ARR adapters) sees zero calls.
    let lab = Lab::new().await;
    fs::set_permissions(
        lab.completed_path("Rel/b.mkv"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    lab.store_observation_again().await;
    let (mutators, recording) = Mutators::recording(&[ArrService::Sonarr, ArrService::Radarr]);
    for (plan, steps) in tier3_plans(&lab, PlanMode::DryRun) {
        let plan_id = plan.plan().id;
        let n = plan.plan().actions.len();
        let ticket = lab.ticket(plan, true).await;
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
        assert!(report.simulated);
        assert_eq!(report.status, None);
        for a in &report.actions {
            assert_eq!(a.outcome, ActionOutcome::Simulated, "{report:?}");
        }
        assert_eq!(report.actions.len(), n);
        let journal = lab.journal(plan_id).await;
        assert_eq!(
            journal.iter().map(|(i, _)| i.step).collect::<Vec<_>>(),
            steps,
            "{plan_id:?}"
        );
        for (i, r) in &journal {
            assert_eq!(i.mode, JournalMode::Simulated);
            assert_eq!(r.as_ref().unwrap().outcome, StepOutcome::Simulated);
        }
        assert_eq!(lab.status(plan_id).await, PlanStatus::Approved);
    }
    assert_eq!(
        recording.calls(),
        0,
        "the recording adapters are never invoked"
    );
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.service_mutations(), 0);
    disk_untouched(&lab);
    assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());

    // Live process, `simulate = true`: the live counting adapters see zero calls too (6.8(e)).
    let lab = Lab::new().await;
    fs::set_permissions(
        lab.completed_path("Rel/b.mkv"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    lab.store_observation_again().await;
    let (mutators, counting) = lab.live();
    for (plan, _) in tier3_plans(&lab, PlanMode::Live) {
        let ticket = lab.ticket(plan, true).await;
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
            report
                .actions
                .iter()
                .all(|a| a.outcome == ActionOutcome::Simulated),
            "{report:?}"
        );
    }
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.service_mutations(), 0);
    disk_untouched(&lab);
}

#[tokio::test]
async fn t3_8_a_torrent_that_started_holding_a_torrent_held_target_refuses_before_the_stop() {
    let lab = Lab::new().await;
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
        user(),
        vec![],
    );
    // A cross-seed of the same file appears after planning: the holder set differs.
    lab.add_holder('c', &RootId::completed(), "Rel/a.mkv");
    let plan_id = plan.plan().id;
    let (report, counting) = run(&lab, plan, false).await;
    assert!(
        blockers(&report.actions[0].outcome)
            .iter()
            .any(|b| b.code == BlockerCode::TorrentReferencesTarget),
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(counting.mutations(), 0);
    assert_eq!(
        lab.qbit.mutation_count(),
        0,
        "nothing stopped, nothing removed"
    );
    assert!(lab.steps(plan_id).await.is_empty());
    assert!(lab.completed_path("Rel/a.mkv").exists());
}
