#![allow(clippy::unwrap_used, clippy::expect_used)]
//! CP2-tier3 findings F01-F12 (`docs/reviews/cp2-tier3.md`), each pinned by a test:
//! recovery decides from the disk (F01), the live holder set must equal the plan (F02),
//! simulations hold the recording set (F04), guards observe ARR and Jellyfin (F05), the
//! left-stopped record is durable (F09), a store read error after the stop still restores
//! (F11), and the missing crash boundaries (F12).

mod actions_common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;

use actions_common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::path::RootId;
use removarr_domain::plan::{PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::ports::{CancelFlag, JournalStore};
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{Mutators, Reporter, execute, recover};

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];
const TWO: &[(&str, &str, &[char])] = &[
    ("Rel/a.mkv", "Movies/Rel/a.mkv", &['a']),
    ("Rel/b.mkv", "Movies/Rel/b.mkv", &['b']),
];

fn user() -> PlanOrigin {
    PlanOrigin::User
}

/// Starts a live execution of `items`, waits for the armed crash point, kills the task.
async fn crash(
    lab: &Lab,
    items: &[(&str, &str, &[char])],
    point: CrashPoint,
    skip: usize,
) -> removarr_domain::ids::PlanId {
    let ticket = lab
        .ticket(lab.plan(items, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    lab.hang.arm_nth(point, skip);
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
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executing);
    plan_id
}

async fn run_recovery(lab: &Lab) -> removarr_workflows::actions::RecoveryReport {
    let (mutators, _) = lab.live();
    recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap()
}

// ------------------------------------------------------------------ F01

#[tokio::test]
async fn f01_a_failed_but_mutated_replace_result_is_rechecked_at_recovery_in_a_two_item_plan() {
    let lab = Lab::new().await;
    // The journal records item a's replacement as `Failed` although the disk shows the link
    // (an error after the rename); the process dies before item b's pre-compare gate intent.
    *lab.rewrite_done_as_failed.lock().unwrap() = Some(JournalStep::FsReplaceWithHardlink);
    let src_a = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let src_b = lab.phys(&lab.completed_path("Rel/b.mkv"));
    let tgt_b = lab.phys(&lab.media_path("Movies/Rel/b.mkv"));
    let plan_id = crash(
        &lab,
        TWO,
        CrashPoint::BeforeIntent(JournalStep::FsProbeHardlink),
        3,
    )
    .await;
    *lab.rewrite_done_as_failed.lock().unwrap() = None;
    let journal = lab.journal(plan_id).await;
    let replace = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsReplaceWithHardlink)
        .unwrap();
    assert!(
        matches!(
            replace.1.as_ref().unwrap().outcome,
            StepOutcome::Failed { .. }
        ),
        "the journal says failed"
    );
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        src_a,
        "the disk says replaced"
    );
    assert!(
        lab.repos.journal.unfinished().await.unwrap().is_empty(),
        "no unfinished intent at all"
    );

    let report = run_recovery(&lab).await;
    let p = &report.plans[0];
    // a is rechecked (modified) and reported executed-unverified, never "not executed";
    // b was never touched: started without a recheck, reported not executed.
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(p.resumed, BTreeSet::from([hash('a'), hash('b')]));
    assert!(
        matches!(p.actions[0].1, ActionOutcome::ExecutedUnverified { .. }),
        "{:?}",
        p.actions
    );
    assert!(
        matches!(p.actions[1].1, ActionOutcome::NotExecuted { .. }),
        "{:?}",
        p.actions
    );
    assert_eq!(lab.phys(&lab.completed_path("Rel/b.mkv")), src_b);
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/b.mkv")),
        tgt_b,
        "nothing redone"
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

#[tokio::test]
async fn f01_an_unobservable_target_stays_possibly_modified_across_a_recovery_retry() {
    if is_root_user() {
        return;
    }
    let lab = Lab::new().await;
    let plan_id = crash(
        &lab,
        ONE,
        CrashPoint::BeforeResult(JournalStep::FsReplaceWithHardlink),
        0,
    )
    .await;
    let dir = lab.media_path("Movies/Rel");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    // First attempt: the target cannot be observed (written as Failed), then qBittorrent is
    // down: Err, plan still Executing.
    *lab.qbit.fail_reads.lock().unwrap() = true;
    let (mutators, _) = lab.live();
    let e = recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Unavailable);
    let journal = lab.journal(plan_id).await;
    let replace = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsReplaceWithHardlink)
        .unwrap();
    assert!(matches!(
        replace.1.as_ref().unwrap().outcome,
        StepOutcome::Failed { .. }
    ));
    // Retry with the target still unobservable: the Failed result is not trusted as "not
    // mutated"; the hash is rechecked and the action needs review.
    *lab.qbit.fail_reads.lock().unwrap() = false;
    let report = run_recovery(&lab).await;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let p = &report.plans[0];
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])],
        "rechecked, not blindly started"
    );
    assert_eq!(
        p.actions[0].1,
        ActionOutcome::FailedNeedsReview,
        "{:?}",
        p.actions
    );
    assert_eq!(
        p.resumed,
        BTreeSet::from([hash('a')]),
        "started only because the recheck verified"
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

// ------------------------------------------------------------------ F02

#[tokio::test]
async fn f02_a_torrent_that_started_holding_a_path_after_planning_invalidates_the_plan() {
    for (root_kind, rel) in [("completed", "Rel/a.mkv"), ("media", "Movies/Rel/a.mkv")] {
        let lab = Lab::new().await;
        let ticket = lab
            .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
            .await;
        let plan_id = ticket.plan.plan().id;
        let root = if root_kind == "completed" {
            RootId::completed()
        } else {
            lab.media.clone()
        };
        lab.add_holder('c', &root, rel); // cross-seed or library seeder, after planning
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
        let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
            panic!("{root_kind}: {report:?}");
        };
        assert!(
            blockers
                .iter()
                .any(|b| b.code == BlockerCode::TorrentReferencesTarget
                    && b.detail.contains(hash('c').as_str())),
            "{root_kind}: {blockers:?}"
        );
        assert_eq!(report.status, Some(PlanStatus::Invalidated));
        assert_eq!(lab.status(plan_id).await, PlanStatus::Invalidated);
        assert_eq!(
            counting.mutations() + lab.qbit.mutation_count(),
            0,
            "{root_kind}: nothing stopped or touched"
        );
    }
}

#[tokio::test]
async fn f02_a_holder_by_inode_only_and_a_mapping_fault_near_the_paths_both_refuse() {
    // A torrent holding another link of the target's inode (no path of the plan).
    let lab = Lab::new().await;
    fs::hard_link(
        lab.completed_path("Rel/a.mkv"),
        lab.completed_path("Rel/a-link.mkv"),
    )
    .unwrap();
    lab.store_observation_again().await; // the inventory knows the second link
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    lab.add_holder('d', &RootId::completed(), "Rel/a-link.mkv");
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
    let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
        panic!("{report:?}");
    };
    assert!(
        blockers
            .iter()
            .any(|b| b.code == BlockerCode::TorrentReferencesTarget),
        "{blockers:?}"
    );
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    // A mapping fault on a torrent whose paths cannot be placed: fail closed.
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    lab.qbit.add_unmapped('e');
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
    let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
        panic!("{report:?}");
    };
    assert!(
        blockers.iter().any(|b| b.code == BlockerCode::MappingError),
        "{blockers:?}"
    );
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
}

// ------------------------------------------------------------------ F04 / F05

#[tokio::test]
async fn f04_f05_a_simulation_in_a_live_process_runs_on_the_recording_set_and_touches_no_service() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(TWO, PlanMode::Live, user()), true)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    let before = (
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/b.mkv")),
    );
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
    assert!(
        report
            .actions
            .iter()
            .all(|a| a.outcome == ActionOutcome::Simulated),
        "{report:?}"
    );
    // Every live adapter the caller passed saw zero calls: fs, qBittorrent, ARR, Jellyfin.
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.service_mutations(), 0);
    assert_eq!(
        (
            lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
            lab.phys(&lab.media_path("Movies/Rel/b.mkv"))
        ),
        before
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Approved);
    // The same in a dry-run process with the full recording set (ARR services included).
    let (dry, recording) = Mutators::recording(&[
        removarr_domain::arr::ArrService::Sonarr,
        removarr_domain::arr::ArrService::Radarr,
    ]);
    let ticket = lab
        .ticket(lab.plan(TWO, PlanMode::DryRun, user()), true)
        .await;
    let report = execute(
        &lab.ports(),
        &dry,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    assert!(report.simulated);
    assert_eq!(recording.calls(), 0);
    assert_eq!(
        counting.mutations() + lab.qbit.mutation_count() + lab.service_mutations(),
        0
    );
}

// ------------------------------------------------------------------ F09

#[tokio::test]
async fn f09_hashes_left_stopped_and_their_reason_are_journaled_even_when_nothing_starts() {
    let lab = Lab::new().await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Stuck;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, _) = lab.live();
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
    assert_eq!(
        report.left_stopped.get(&hash('a')),
        Some(&removarr_workflows::actions::LeftStoppedReason::RecheckTimedOut)
    );
    assert!(lab.qbit.calls_of("start").is_empty(), "nothing started");
    // The QbitStart intent exists with an empty start set and carries the left-stopped map.
    let observed = lab
        .last_start_observed(plan_id)
        .await
        .expect("a QbitStart intent is journaled");
    assert_eq!(
        observed["left_stopped"][hash('a').as_str()],
        "recheck_timed_out",
        "{observed}"
    );
    assert_eq!(observed["recheck"][hash('a').as_str()], "timed_out");
    let journal = lab.journal(plan_id).await;
    let (start, result) = journal
        .iter()
        .rev()
        .find(|(i, _)| i.step == JournalStep::QbitStart)
        .unwrap();
    assert_eq!(start.target, serde_json::json!([]));
    assert!(matches!(
        result.as_ref().unwrap().outcome,
        StepOutcome::Done { .. }
    ));
}

// ------------------------------------------------------------------ F11

#[tokio::test]
async fn f11_a_store_read_error_after_the_stop_still_restores_the_untouched_torrent() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    // Pass 1 reads the session once; pass 2 (after the stop) fails.
    lab.scans.allow.store(1, Ordering::SeqCst);
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
    .expect("a store read error is a blocker, not an abort");
    let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
        panic!("{report:?}");
    };
    assert!(
        blockers
            .iter()
            .any(|b| b.detail.contains("could not read the stored state")),
        "{blockers:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(counting.replaces.load(Ordering::SeqCst), 0);
    assert_eq!(lab.qbit.calls_of("stop").len(), 1);
    assert_eq!(
        lab.qbit.calls_of("start"),
        vec![BTreeSet::from([hash('a')])],
        "compensating start ran"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert_eq!(lab.status(plan_id).await, PlanStatus::Invalidated);
}

// ------------------------------------------------------------------ F12

#[tokio::test]
async fn f12_crash_after_the_stop_intent_before_the_stop_resolves_from_the_listing() {
    let lab = Lab::new().await;
    let plan_id = crash(&lab, ONE, CrashPoint::AfterIntent(JournalStep::QbitStop), 0).await;
    assert_eq!(
        lab.qbit.mutation_count(),
        0,
        "intent durable, stop not sent"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    let report = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert!(p.resumed.is_empty(), "already running: nothing to start");
    assert!(p.left_stopped.is_empty());
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert!(matches!(p.actions[0].1, ActionOutcome::NotExecuted { .. }));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    let journal = lab.journal(plan_id).await;
    let stop = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::QbitStop)
        .unwrap();
    assert!(matches!(
        stop.1.as_ref().unwrap().outcome,
        StepOutcome::Skipped { .. }
    ));
}

#[tokio::test]
async fn f12_crash_after_the_start_intent_before_the_start_rechecks_and_starts_at_recovery() {
    let lab = Lab::new().await;
    let src = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let plan_id = crash(
        &lab,
        ONE,
        CrashPoint::AfterIntent(JournalStep::QbitStart),
        0,
    )
    .await;
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), src);
    assert_eq!(
        lab.qbit.state('a'),
        TorrentState::StoppedUp,
        "start intent durable, start not sent"
    );
    assert_eq!(lab.qbit.calls_of("recheck").len(), 1);
    let report = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert_eq!(
        lab.qbit.calls_of("recheck").len(),
        2,
        "modified data is rechecked again"
    );
    assert_eq!(p.resumed, BTreeSet::from([hash('a')]));
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert!(
        matches!(p.actions[0].1, ActionOutcome::ExecutedUnverified { .. }),
        "{:?}",
        p.actions
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

#[tokio::test]
async fn f12_crash_between_items_rechecks_the_done_item_and_resumes_the_untouched_one() {
    let lab = Lab::new().await;
    let src_a = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let tgt_b = lab.phys(&lab.media_path("Movies/Rel/b.mkv"));
    // 4th FsProbeHardlink intent = item b's pre-compare gate: a is done, b not started.
    let plan_id = crash(
        &lab,
        TWO,
        CrashPoint::BeforeIntent(JournalStep::FsProbeHardlink),
        3,
    )
    .await;
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), src_a);
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/b.mkv")), tgt_b);
    let report = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(p.resumed, BTreeSet::from([hash('a'), hash('b')]));
    assert!(matches!(
        p.actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));
    assert!(matches!(p.actions[1].1, ActionOutcome::NotExecuted { .. }));
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/b.mkv")),
        tgt_b,
        "b never redone"
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
}

// ------------------------------------------------------------------ F08

#[tokio::test]
async fn f08_the_runner_refuses_a_plan_whose_direction_is_not_completed_to_media() {
    let lab = Lab::new().await;
    let (mut plan, _) = lab.plan(ONE, PlanMode::Live, user()).into_parts();
    // Swap source and target (media -> completed); the plan still seals and gets approved.
    if let removarr_domain::plan::ActionTarget::Consolidate { source, target } =
        &mut plan.actions[0].target
    {
        std::mem::swap(source, target);
    }
    let sealed = removarr_domain::plan::HashedPlan::seal(plan).unwrap();
    let ticket = lab.ticket(sealed, false).await;
    let plan_id = ticket.plan.plan().id;
    let before = (
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
    );
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
    let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
        panic!("{report:?}");
    };
    assert_eq!(blockers[0].code, BlockerCode::ProtectedZone, "{blockers:?}");
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    assert!(
        lab.steps(plan_id).await.is_empty(),
        "no intent for a refused direction"
    );
    assert_eq!(
        (
            lab.phys(&lab.completed_path("Rel/a.mkv")),
            lab.phys(&lab.media_path("Movies/Rel/a.mkv"))
        ),
        before
    );
}
