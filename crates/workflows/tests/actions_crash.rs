#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Crash boundaries and recovery after restart (architecture 7.5, INV-08, INV-40, T-22).
//!
//! A crash is modelled faithfully: a fake hangs at the boundary, the test aborts the execution
//! task (the future is dropped, no compensation code runs, exactly as a killed process), then
//! `recover` runs against the same SQLite journal, the same temporary roots and the same fake
//! qBittorrent, as the next process start would.

mod actions_common;

use std::collections::BTreeSet;
use std::sync::Arc;

use actions_common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::plan::{PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::ports::{CancelFlag, JournalStore};
use removarr_domain::torrent::TorrentState;
use removarr_workflows::actions::{
    ActionEvent, LeftStoppedReason, Mutators, Reporter, execute, recover,
};

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];

/// Starts a live execution, waits for the armed crash point, kills the task.
async fn crash_at(lab: &Lab, point: CrashPoint) -> removarr_domain::ids::PlanId {
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, PlanOrigin::User), false)
        .await;
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
    assert_eq!(
        lab.status(plan_id).await,
        PlanStatus::Executing,
        "killed mid-run"
    );
    plan_id
}

async fn run_recovery(lab: &Lab) -> (removarr_workflows::actions::RecoveryReport, Reporter) {
    let (mutators, _) = lab.live();
    let reporter = Reporter::new();
    let r = recover(&lab.ports(), &mutators, &lab.settings(), &reporter)
        .await
        .unwrap();
    (r, reporter)
}

#[tokio::test]
async fn crash_before_the_replace_intent_resumes_the_untouched_torrent() {
    let lab = Lab::new().await;
    let before = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
    let plan_id = crash_at(
        &lab,
        CrashPoint::BeforeIntent(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    // The torrent was stopped (journaled, Done); no replace intent exists; the disk is intact.
    let steps = lab.steps(plan_id).await;
    assert_eq!(
        steps,
        vec![
            JournalStep::FsProbeHardlink,
            JournalStep::QbitStop,
            JournalStep::FsProbeHardlink
        ]
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::StoppedUp);
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), before);
    assert_eq!(
        lab.repos.journal.unfinished().await.unwrap().len(),
        0,
        "no intent without result"
    );
    // No unfinished intent, yet the plan is Executing with its torrent stopped: recovery finds
    // it through the executing-plans listing and resumes the untouched torrent.
    let (report, _) = run_recovery(&lab).await;
    assert_eq!(report.plans.len(), 1);
    let p = &report.plans[0];
    assert_eq!(p.resumed, BTreeSet::from([hash('a')]));
    assert!(lab.qbit.calls_of("recheck").is_empty());
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert!(matches!(p.actions[0].1, ActionOutcome::NotExecuted { .. }));
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), before);
}

#[tokio::test]
async fn crash_after_the_replace_intent_before_the_mutation_is_resolved_as_not_executed() {
    let lab = Lab::new().await;
    let before = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
    let plan_id = crash_at(
        &lab,
        CrashPoint::AfterIntent(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    assert_eq!(lab.repos.journal.unfinished().await.unwrap().len(), 1);
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        before,
        "intent durable, nothing mutated"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::StoppedUp);

    let (report, reporter) = run_recovery(&lab).await;
    assert_eq!(report.plans.len(), 1);
    let p = &report.plans[0];
    assert_eq!(p.status, Some(PlanStatus::Interrupted));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert_eq!(
        p.resumed,
        BTreeSet::from([hash('a')]),
        "data untouched: resumed"
    );
    assert!(p.left_stopped.is_empty());
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert!(
        lab.qbit.calls_of("recheck").is_empty(),
        "nothing modified, nothing rechecked"
    );
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        before,
        "recovery never redoes a mutation"
    );
    // The unfinished intent got an observed result; no intent is unfinished any more.
    let journal = lab.journal(plan_id).await;
    let replace = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsReplaceWithHardlink)
        .unwrap();
    assert!(
        matches!(
            replace.1.as_ref().unwrap().outcome,
            StepOutcome::Skipped { .. }
        ),
        "{:?}",
        replace.1
    );
    assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());
    assert!(
        matches!(p.actions[0].1, ActionOutcome::NotExecuted { .. }),
        "{:?}",
        p.actions
    );
    assert_eq!(
        lab.steps(plan_id).await.last(),
        Some(&JournalStep::QbitStart),
        "the restart is journaled"
    );
    assert!(reporter.events().iter().any(|e| matches!(
        e,
        ActionEvent::RecoveredPlan {
            status: Some(PlanStatus::Interrupted),
            ..
        }
    )));
    // Interrupted plans never resume.
    assert!(!PlanStatus::Interrupted.can_transition_to(PlanStatus::Executing));
    // A second recovery finds nothing.
    let (again, _) = run_recovery(&lab).await;
    assert!(again.plans.is_empty());
}

#[tokio::test]
async fn crash_after_the_mutation_before_the_result_is_rechecked_and_reported_unverified() {
    let lab = Lab::new().await;
    let src = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let plan_id = crash_at(
        &lab,
        CrashPoint::BeforeResult(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        src,
        "the link is on disk, the result is not"
    );
    assert_eq!(lab.repos.journal.unfinished().await.unwrap().len(), 1);

    let (report, _) = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert_eq!(p.status, Some(PlanStatus::Interrupted));
    // Modified data: rechecked, verified, then started.
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(p.resumed, BTreeSet::from([hash('a')]));
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    assert!(
        matches!(p.actions[0].1, ActionOutcome::ExecutedUnverified { .. }),
        "{:?}",
        p.actions
    );
    let journal = lab.journal(plan_id).await;
    let replace = journal
        .iter()
        .find(|(i, _)| i.step == JournalStep::FsReplaceWithHardlink)
        .unwrap();
    assert!(
        matches!(
            replace.1.as_ref().unwrap().outcome,
            StepOutcome::Done { .. }
        ),
        "observed state recorded"
    );
    assert_eq!(
        lab.steps(plan_id)
            .await
            .iter()
            .rev()
            .take(2)
            .collect::<Vec<_>>(),
        vec![&JournalStep::QbitStart, &JournalStep::QbitRecheck]
    );
    assert_eq!(
        lab.repos.gains.totals().await.unwrap().attributable,
        0,
        "nothing verified, nothing gained"
    );
}

#[tokio::test]
async fn crash_after_the_mutation_with_a_failing_recheck_leaves_the_torrent_stopped() {
    let lab = Lab::new().await;
    let _ = crash_at(
        &lab,
        CrashPoint::BeforeResult(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    *lab.qbit.recheck_result.lock().unwrap() = RecheckResult::Stuck;
    let (report, reporter) = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert!(p.resumed.is_empty());
    assert_eq!(
        p.left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::RecheckTimedOut)
    );
    assert!(lab.qbit.calls_of("start").is_empty());
    assert_eq!(p.status, Some(PlanStatus::Interrupted));
    assert!(
        reporter
            .events()
            .iter()
            .any(|e| matches!(e, ActionEvent::TorrentsLeftStopped { .. }))
    );
}

#[tokio::test]
async fn crash_during_the_recheck_rechecks_again_at_recovery() {
    let lab = Lab::new().await;
    let src = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let plan_id = crash_at(&lab, CrashPoint::DuringRecheck).await;
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), src);
    let unfinished = lab.repos.journal.unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].intent.step, JournalStep::QbitRecheck);
    assert_eq!(lab.qbit.calls_of("recheck").len(), 1);

    let (report, _) = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert_eq!(
        lab.qbit.calls_of("recheck").len(),
        2,
        "rechecked again, never trusted"
    );
    assert_eq!(p.resumed, BTreeSet::from([hash('a')]));
    assert_eq!(p.status, Some(PlanStatus::Interrupted));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert!(matches!(
        p.actions[0].1,
        ActionOutcome::ExecutedUnverified { .. }
    ));
    assert!(lab.repos.journal.unfinished().await.unwrap().is_empty());
}

#[tokio::test]
async fn recovery_without_a_live_controller_leaves_torrents_stopped_and_visible() {
    let lab = Lab::new().await;
    let plan_id = crash_at(
        &lab,
        CrashPoint::AfterIntent(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    let (dry, recording) = Mutators::recording(&[]);
    let reporter = Reporter::new();
    let report = recover(&lab.ports(), &dry, &lab.settings(), &reporter)
        .await
        .unwrap();
    let p = &report.plans[0];
    assert_eq!(
        p.left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::NoLiveController)
    );
    assert!(p.resumed.is_empty());
    assert_eq!(
        lab.qbit.state('a'),
        TorrentState::StoppedUp,
        "stopped and visible"
    );
    assert_eq!(recording.calls(), 0);
    assert_eq!(lab.qbit.mutation_count(), 1, "only the original stop");
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert!(
        lab.repos.journal.unfinished().await.unwrap().is_empty(),
        "fs intents resolved by observation"
    );
}

#[tokio::test]
async fn recovery_retries_when_qbittorrent_is_unreachable_and_executes_nothing_meanwhile() {
    let lab = Lab::new().await;
    let plan_id = crash_at(
        &lab,
        CrashPoint::BeforeResult(JournalStep::FsReplaceWithHardlink),
    )
    .await;
    *lab.qbit.fail_reads.lock().unwrap() = true;
    let (mutators, _) = lab.live();
    let e = recover(&lab.ports(), &mutators, &lab.settings(), &Reporter::new())
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Unavailable);
    assert_eq!(
        lab.status(plan_id).await,
        PlanStatus::Executing,
        "unresolved: stays for the retry"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::StoppedUp);
    // The filesystem observation was already written; the retry resolves the torrents.
    assert!(
        lab.repos
            .journal
            .unfinished()
            .await
            .unwrap()
            .iter()
            .all(|u| u.intent.step != JournalStep::FsReplaceWithHardlink)
    );
    *lab.qbit.fail_reads.lock().unwrap() = false;
    let (report, _) = run_recovery(&lab).await;
    assert_eq!(
        report.plans.len(),
        1,
        "found again through the executing-plans listing"
    );
    assert_eq!(report.plans[0].resumed, BTreeSet::from([hash('a')]));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    let _ = Arc::new(());
}

#[tokio::test]
async fn crash_before_the_stop_intent_touches_no_torrent_and_interrupts_the_plan() {
    let lab = Lab::new().await;
    let before = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
    let plan_id = crash_at(&lab, CrashPoint::BeforeIntent(JournalStep::QbitStop)).await;
    assert_eq!(lab.steps(plan_id).await, vec![JournalStep::FsProbeHardlink]);
    assert_eq!(
        lab.qbit.mutation_count(),
        0,
        "no stop without its durable intent"
    );
    assert_eq!(lab.qbit.state('a'), TorrentState::Uploading);
    let (report, _) = run_recovery(&lab).await;
    let p = &report.plans[0];
    assert!(p.resumed.is_empty() && p.left_stopped.is_empty());
    assert_eq!(
        lab.qbit.mutation_count(),
        0,
        "recovery issues no command for an untouched torrent"
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert!(matches!(p.actions[0].1, ActionOutcome::NotExecuted { .. }));
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), before);
}
