#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tier-2 consolidation end to end on temporary roots with the real `LiveFsMutator`, the real
//! SQLite journal and a fake qBittorrent: happy path with inode verification, dry-run and
//! simulation zero-mutation guards, probe before any content read (T-14), change during the
//! comparison (T-11/T-12), live revalidation (T-16), partially affected torrents left stopped
//! (T-23), cancellation (T-22), tier gating and the automatic stop-at-first-failure rule.

mod actions_common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;

use actions_common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{CompareOutcome, IoKind};
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::{ActionOutcome, JournalMode, JournalStep, StepOutcome};
use removarr_domain::path::RootId;
use removarr_domain::plan::ActionKind;
use removarr_domain::plan::{PlanMode, PlanOrigin, PlanStatus};
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{CancelFlag, JournalStore};
use removarr_workflows::actions::{
    ActionEvent, IMPLEMENTED_TIER, LeftStoppedReason, Mutators, Reporter, execute, supported,
};

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];
const TWO: &[(&str, &str, &[char])] = &[
    ("Rel/a.mkv", "Movies/Rel/a.mkv", &['a']),
    ("Rel/b.mkv", "Movies/Rel/b.mkv", &['b']),
];

fn user() -> PlanOrigin {
    PlanOrigin::User
}

#[tokio::test]
async fn t13_live_consolidation_links_completed_toward_media_and_verifies_by_inode() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    let before_src = lab.phys(&lab.completed_path("Rel/a.mkv"));
    let before_tgt = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
    assert_ne!(before_src, before_tgt);
    let reporter = Reporter::new();
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

    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executed);
    assert_eq!(
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        before_src,
        "media carries the completed inode"
    );
    assert_eq!(
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        before_src,
        "completed inode unchanged"
    );
    assert_eq!(
        fs::read(lab.media_path("Movies/Rel/a.mkv")).unwrap(),
        fs::read(lab.completed_path("Rel/a.mkv")).unwrap()
    );
    // Journal order: probe, stop, probe (gate), replace, recheck, start; every intent has a
    // Done result; every mutation happened after its intent (CON-15, INV-08).
    let steps = lab.steps(plan_id).await;
    assert_eq!(
        steps,
        vec![
            JournalStep::FsProbeHardlink,
            JournalStep::QbitStop,
            JournalStep::FsProbeHardlink,
            JournalStep::FsReplaceWithHardlink,
            JournalStep::QbitRecheck,
            JournalStep::QbitStart,
        ]
    );
    for (i, r) in lab.journal(plan_id).await {
        assert_eq!(i.mode, JournalMode::Live);
        assert!(
            matches!(r.unwrap().outcome, StepOutcome::Done { .. }),
            "{i:?}"
        );
    }
    assert_eq!(counting.probes.load(Ordering::SeqCst), 2);
    assert_eq!(counting.replaces.load(Ordering::SeqCst), 1);
    assert_eq!(lab.qbit.calls_of("stop"), vec![BTreeSet::from([hash('a')])]);
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(
        lab.qbit.calls_of("start"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert!(lab.qbit.calls_of("remove").is_empty());
    assert!(report.left_stopped.is_empty());
    assert_eq!(lab.content.compares.load(Ordering::SeqCst), 1);
    // The gain is recorded once with the replaced media inode.
    let totals = lab.repos.gains.totals().await.unwrap();
    assert_eq!(totals.attributable, FILE_BYTES as u64);
    // Events reach the reporter in order, ending with the plan status.
    let events = reporter.events();
    assert!(matches!(
        events.first(),
        Some(ActionEvent::PlanStarted {
            simulated: false,
            ..
        })
    ));
    assert!(matches!(
        events.last(),
        Some(ActionEvent::PlanFinished {
            status: Some(PlanStatus::Executed),
            ..
        })
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ActionEvent::TorrentsStopped { .. }))
    );
    assert_eq!(reporter.snapshot().stage, "finished");
}

#[tokio::test]
async fn t30_dry_run_process_runs_the_whole_pipeline_with_zero_mutations() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::DryRun, user()), true)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, recording) = Mutators::recording(&[]);
    let before = (
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
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
    assert_eq!(report.status, None);
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Simulated,
        "{report:?}"
    );
    assert_eq!(
        recording.calls(),
        0,
        "the recording adapters are never invoked"
    );
    assert_eq!(lab.qbit.mutation_count(), 0, "no stop/start/recheck/remove");
    assert_eq!(
        (
            lab.phys(&lab.completed_path("Rel/a.mkv")),
            lab.phys(&lab.media_path("Movies/Rel/a.mkv"))
        ),
        before
    );
    assert_eq!(
        lab.status(plan_id).await,
        PlanStatus::Approved,
        "status never changes"
    );
    // The same steps are journaled, all simulated; the real comparison and the real stat
    // revalidation ran (the dry-run path is the real path).
    let journal = lab.journal(plan_id).await;
    assert_eq!(
        journal.iter().map(|(i, _)| i.step).collect::<Vec<_>>(),
        vec![
            JournalStep::FsProbeHardlink,
            JournalStep::QbitStop,
            JournalStep::FsProbeHardlink,
            JournalStep::FsReplaceWithHardlink,
            JournalStep::QbitRecheck,
            JournalStep::QbitStart,
        ]
    );
    for (i, r) in &journal {
        assert_eq!(i.mode, JournalMode::Simulated);
        assert_eq!(r.as_ref().unwrap().outcome, StepOutcome::Simulated);
    }
    assert_eq!(lab.content.compares.load(Ordering::SeqCst), 1);
    assert!(
        lab.repos.journal.unfinished().await.unwrap().is_empty(),
        "simulated intents are ignored by recovery"
    );
    assert_eq!(lab.repos.gains.totals().await.unwrap().attributable, 0);
}

#[tokio::test]
async fn simulation_in_a_live_process_never_invokes_the_live_adapters() {
    // Architecture 6.8(e): live process, counting live fakes, `simulate = true`.
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), true)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    let before = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
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
    assert_eq!(counting.mutations(), 0);
    assert_eq!(lab.qbit.mutation_count(), 0);
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), before);
    assert_eq!(lab.status(plan_id).await, PlanStatus::Approved);
    assert!(
        lab.journal(plan_id)
            .await
            .iter()
            .all(|(i, _)| i.mode == JournalMode::Simulated)
    );
    // A dry-run process can never run live, and a dry-run plan can never run live.
    let (dry, _) = Mutators::recording(&[]);
    let t = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let e = execute(
        &lab.ports(),
        &dry,
        &lab.settings(),
        t,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.class, ErrorClass::CapabilityDisabled);
    let t = lab
        .ticket(lab.plan(ONE, PlanMode::DryRun, user()), false)
        .await;
    let e = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        t,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.class, ErrorClass::CapabilityDisabled);
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
}

#[tokio::test]
async fn t14_permission_probe_fails_before_any_content_read_and_before_the_stop() {
    if is_root_user() {
        return;
    }
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let dir = lab.media_path("Movies/Rel");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let (mutators, _) = lab.live();
    let report = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let report = report.unwrap();
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Failed {
            class: ErrorClass::Io(IoKind::PermissionDenied)
        }
    );
    assert_eq!(
        lab.content.compares.load(Ordering::SeqCst),
        0,
        "no content read"
    );
    assert_eq!(lab.qbit.mutation_count(), 0, "no torrent stopped");
    assert_eq!(lab.steps(plan_id).await, vec![JournalStep::FsProbeHardlink]);
    let (_, r) = &lab.journal(plan_id).await[0];
    match &r.as_ref().unwrap().outcome {
        StepOutcome::Failed { message, .. } => {
            assert!(message.contains("protected_hardlinks"), "{message}");
            assert!(!message.to_ascii_lowercase().contains("sudo"), "{message}");
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(lab.status(plan_id).await, PlanStatus::Failed);
    assert!(
        lab.qbit.calls_of("start").is_empty(),
        "nothing to restore: nothing was stopped"
    );
}

#[tokio::test]
async fn t11_t12_a_difference_or_change_during_the_comparison_mutates_nothing() {
    for outcome in [
        CompareOutcome::Differs { offset: 199_999 },
        CompareOutcome::Changed,
    ] {
        let lab = Lab::new().await;
        let ticket = lab
            .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
            .await;
        let plan_id = ticket.plan.plan().id;
        lab.content.override_next.lock().unwrap().push(outcome);
        let (mutators, counting) = lab.live();
        let before = lab.phys(&lab.media_path("Movies/Rel/a.mkv"));
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
        let expected = match outcome {
            CompareOutcome::Differs { .. } => ErrorClass::Rejected,
            _ => ErrorClass::StateChanged,
        };
        assert_eq!(
            report.actions[0].outcome,
            ActionOutcome::Failed { class: expected },
            "{outcome:?}"
        );
        assert_eq!(counting.replaces.load(Ordering::SeqCst), 0);
        assert_eq!(lab.phys(&lab.media_path("Movies/Rel/a.mkv")), before);
        // The torrent was stopped for the comparison and is started again: its data is intact.
        assert_eq!(lab.qbit.calls_of("stop").len(), 1);
        assert_eq!(
            lab.qbit.calls_of("start"),
            vec![BTreeSet::from([hash('a')])]
        );
        assert!(
            lab.qbit.calls_of("recheck").is_empty(),
            "nothing modified, nothing rechecked"
        );
        assert!(
            !lab.steps(plan_id)
                .await
                .contains(&JournalStep::FsReplaceWithHardlink)
        );
        assert_eq!(lab.status(plan_id).await, PlanStatus::Failed);
    }
}

#[tokio::test]
async fn t16_live_revalidation_refuses_any_change_and_invalidates_the_plan() {
    // (a) the target changed on disk after planning.
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let p = lab.media_path("Movies/Rel/a.mkv");
    let mut bytes = fs::read(&p).unwrap();
    bytes.push(1);
    fs::write(&p, bytes).unwrap();
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
    match &report.actions[0].outcome {
        ActionOutcome::NotExecuted { blockers } => {
            assert!(
                blockers
                    .iter()
                    .any(|b| b.code == BlockerCode::ChangedSincePlanning
                        && b.detail.contains("changed")),
                "{blockers:?}"
            );
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Invalidated);
    assert_eq!(
        counting.mutations() + lab.qbit.mutation_count(),
        0,
        "nothing touched"
    );
    assert!(
        lab.steps(plan_id).await.is_empty(),
        "no intent before revalidation passes"
    );

    // (b) the wanted-file set of the affected torrent changed (not stored in the plan: compared
    // live against the stored snapshot, 12 follow-up 4).
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    lab.qbit.set_priority('a', 0, 0);
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
    match &report.actions[0].outcome {
        ActionOutcome::NotExecuted { blockers } => {
            assert!(
                blockers
                    .iter()
                    .any(|b| b.code == BlockerCode::ChangedSincePlanning
                        && b.detail.contains("wanted files")),
                "{blockers:?}"
            );
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);

    // (c) the torrent left the allowlisted class; (d) qBittorrent unreachable.
    for (set_state, down) in [(Some("downloading"), false), (None, true)] {
        let lab = Lab::new().await;
        let ticket = lab
            .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
            .await;
        if let Some(s) = set_state {
            lab.qbit.set_state('a', s);
        }
        *lab.qbit.fail_reads.lock().unwrap() = down;
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
        let want = if down {
            BlockerCode::ServiceUnavailable(removarr_domain::session::ServiceKind::Qbittorrent)
        } else {
            BlockerCode::TorrentProtectedState
        };
        assert!(blockers.iter().any(|b| b.code == want), "{blockers:?}");
        assert_eq!(report.status, Some(PlanStatus::Invalidated));
        assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    }

    // (e) a change between the stop and the second revalidation: the stopped torrent is started
    // again because nothing was modified, and the plan is invalidated.
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    let src = lab.completed_path("Rel/a.mkv");
    let fs2 = lab.fs.clone();
    let hashes: BTreeSet<_> = [hash('a')].into_iter().collect();
    // The probe passes, then the file changes before the stop (modelled by changing it from
    // inside the probe counter hook is not possible: change it between pass 1 and the stop by
    // altering the mtime right after the first listing).
    let _ = (fs2, hashes);
    let sink_lab = lab.qbit.clone();
    let reporter = Reporter::new().with_sink(std::sync::Arc::new(move |e: &ActionEvent| {
        if matches!(e, ActionEvent::TorrentsStopped { .. }) {
            let mut bytes = fs::read(&src).unwrap();
            bytes[0] ^= 0xff;
            fs::write(&src, bytes).unwrap();
            let _ = &sink_lab;
        }
    }));
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
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::NotExecuted { .. }),
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated));
    assert_eq!(counting.replaces.load(Ordering::SeqCst), 0);
    assert_eq!(lab.qbit.calls_of("stop").len(), 1);
    assert_eq!(
        lab.qbit.calls_of("start"),
        vec![BTreeSet::from([hash('a')])],
        "untouched torrent resumed"
    );
    assert_eq!(
        lab.steps(plan_id).await,
        vec![
            JournalStep::FsProbeHardlink,
            JournalStep::QbitStop,
            JournalStep::QbitStart
        ]
    );
    assert!(reporter.events().iter().any(
        |e| matches!(e, ActionEvent::Revalidated { pass: 2, failed, .. } if !failed.is_empty())
    ));
}

#[tokio::test]
async fn t23_partially_affected_torrent_is_left_stopped_and_reported() {
    for (result, reason) in [
        (RecheckResult::Stuck, LeftStoppedReason::RecheckTimedOut),
        (RecheckResult::Failed, LeftStoppedReason::RecheckFailed),
    ] {
        let lab = Lab::new().await;
        *lab.qbit.recheck_result.lock().unwrap() = result;
        let ticket = lab
            .ticket(lab.plan(TWO, PlanMode::Live, user()), false)
            .await;
        let plan_id = ticket.plan.plan().id;
        // Item b fails its comparison: torrent b is stopped but never modified.
        lab.content.override_next.lock().unwrap().extend([
            CompareOutcome::Identical,
            CompareOutcome::Differs { offset: 0 },
        ]);
        let (mutators, _) = lab.live();
        let reporter = Reporter::new();
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
        assert!(
            matches!(
                report.actions[0].outcome,
                ActionOutcome::ExecutedUnverified { .. }
            ),
            "{:?}",
            report.actions[0]
        );
        assert!(
            !report.actions[0].outcome.is_success(),
            "a failed recheck is never success"
        );
        assert_eq!(
            report.left_stopped.get(&hash('a')),
            Some(&reason),
            "{result:?}"
        );
        assert_eq!(
            lab.qbit.calls_of("start"),
            vec![BTreeSet::from([hash('b')])],
            "only the untouched torrent is started"
        );
        assert_eq!(
            lab.qbit.state('a'),
            if result == RecheckResult::Failed {
                removarr_domain::torrent::TorrentState::MissingFiles
            } else {
                removarr_domain::torrent::TorrentState::CheckingUp
            }
        );
        assert!(lab.qbit.calls_of("remove").is_empty());
        assert_eq!(lab.status(plan_id).await, PlanStatus::PartiallyExecuted);
        assert!(reporter.events().iter().any(|e| matches!(e, ActionEvent::TorrentsLeftStopped { hashes, .. } if hashes.contains_key(&hash('a')))));
        // The inode proof holds, the recheck does not: ExecutedUnverified, no gain.
        assert_eq!(lab.repos.gains.totals().await.unwrap().attributable, 0);
        let v = &report.actions[0].verification;
        assert_eq!(v.len(), 3);
        assert!(
            v.iter()
                .filter(|r| r.status == removarr_domain::journal::VerificationStatus::Passed)
                .count()
                == 2
        );
    }
}

#[tokio::test]
async fn t22_cancellation_between_items_resumes_untouched_and_rechecks_modified_torrents() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(TWO, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    let cancel = CancelFlag::new();
    let c2 = cancel.clone();
    *counting.after_replace.lock().unwrap() = Some(Box::new(move || c2.cancel()));
    let reporter = Reporter::new();
    let report = execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &cancel,
        &reporter,
    )
    .await
    .unwrap();
    assert!(report.interrupted);
    assert_eq!(report.status, Some(PlanStatus::Interrupted));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Interrupted);
    assert_eq!(
        counting.replaces.load(Ordering::SeqCst),
        1,
        "no mutation after the cancel"
    );
    assert!(
        matches!(report.actions[1].outcome, ActionOutcome::NotExecuted { .. }),
        "{:?}",
        report.actions[1]
    );
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{:?}",
        report.actions[0]
    );
    // Both hashes were stopped once; a was rechecked (modified), b was not; both started once.
    assert_eq!(
        lab.qbit.calls_of("stop"),
        vec![BTreeSet::from([hash('a'), hash('b')])]
    );
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(
        lab.qbit.calls_of("start"),
        vec![BTreeSet::from([hash('a'), hash('b')])]
    );
    assert!(
        reporter
            .events()
            .iter()
            .any(|e| matches!(e, ActionEvent::BatchInterrupted { .. }))
    );
    let events = reporter.events();
    let pos = |f: &dyn Fn(&ActionEvent) -> bool| events.iter().position(f).unwrap();
    assert!(
        pos(&|e| matches!(e, ActionEvent::BatchInterrupted { .. }))
            < pos(&|e| matches!(e, ActionEvent::PlanFinished { .. }))
    );
}

#[tokio::test]
async fn a_failed_replace_step_is_judged_by_the_inode_and_never_reported_verified() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    let (mutators, counting) = lab.live();
    *counting.fail_after_replace.lock().unwrap() = true;
    let src = lab.phys(&lab.completed_path("Rel/a.mkv"));
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
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        src,
        "the link is in place"
    );
    assert!(
        matches!(
            report.actions[0].outcome,
            ActionOutcome::ExecutedUnverified { .. }
        ),
        "{:?}",
        report.actions[0]
    );
    // The hash was treated as modified: rechecked, and started because the recheck verified.
    assert_eq!(
        lab.qbit.calls_of("recheck"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(
        lab.qbit.calls_of("start"),
        vec![BTreeSet::from([hash('a')])]
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::PartiallyExecuted);
    assert_eq!(
        lab.repos.gains.totals().await.unwrap().attributable,
        0,
        "no gain without verification"
    );
}

#[tokio::test]
async fn a_stop_failure_leaves_nothing_modified_and_restores_what_it_can() {
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let plan_id = ticket.plan.plan().id;
    *lab.qbit.fail_mutations.lock().unwrap() = Some("outcome unknown, not retried".into());
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
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Failed {
            class: ErrorClass::Unavailable
        }
    );
    assert_eq!(counting.replaces.load(Ordering::SeqCst), 0);
    assert_eq!(
        lab.content.compares.load(Ordering::SeqCst),
        0,
        "no compare after a failed stop"
    );
    let steps = lab.steps(plan_id).await;
    assert_eq!(
        steps,
        vec![
            JournalStep::FsProbeHardlink,
            JournalStep::QbitStop,
            JournalStep::QbitStart
        ]
    );
    // The compensating start failed too: the hash is reported left stopped, never hidden.
    assert_eq!(
        report.left_stopped.get(&hash('a')),
        Some(&LeftStoppedReason::StartFailed)
    );
    assert_eq!(lab.status(plan_id).await, PlanStatus::Failed);
}

#[tokio::test]
async fn tiers_four_and_five_are_explicit_refusals() {
    assert_eq!(IMPLEMENTED_TIER, RiskTier::T3);
    for k in [
        ActionKind::ReplaceRelease,
        ActionKind::DeleteMediaFile,
        ActionKind::ArrGrabRelease,
        ActionKind::ArrRemoveQueueItem,
        ActionKind::ArrSetMonitored,
    ] {
        assert!(!supported(k), "{k:?}");
    }
    for k in [
        ActionKind::ConsolidateHardlink,
        ActionKind::ArrRescan,
        ActionKind::JellyfinRefresh,
        ActionKind::DeleteCompletedFile,
        ActionKind::DeleteCompletedFolder,
        ActionKind::DeleteJunk,
        ActionKind::RemoveEmptyParent,
        ActionKind::RemoveTorrentTask,
        ActionKind::CleanupStalledIncomplete,
        ActionKind::DeleteIncompleteOrphan,
        ActionKind::FixMode,
    ] {
        assert!(supported(k), "{k:?}");
    }
    let lab = Lab::new().await;
    let ticket = lab.ticket(lab.tier4_plan(), false).await;
    let plan_id = ticket.plan.plan().id;
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
    assert_eq!(blockers[0].code, BlockerCode::NotInBuild);
    assert_eq!(lab.status(plan_id).await, PlanStatus::Invalidated);
    assert!(
        lab.steps(plan_id).await.is_empty(),
        "no intent for a refused kind"
    );
    assert_eq!(counting.mutations() + lab.qbit.mutation_count(), 0);
    assert!(lab.completed_path("Rel/a.mkv").exists());
    // Simulating a refused kind touches nothing and changes nothing either.
    let ticket = lab.ticket(lab.tier4_plan(), true).await;
    let plan_id = ticket.plan.plan().id;
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
    assert!(matches!(
        report.actions[0].outcome,
        ActionOutcome::NotExecuted { .. }
    ));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Approved);
}

#[tokio::test]
async fn an_automatic_plan_stops_at_the_first_failure_and_a_manual_one_continues() {
    for origin in [PlanOrigin::AutoPolicy, PlanOrigin::User] {
        let lab = Lab::new().await;
        let ticket = lab
            .ticket(lab.plan(TWO, PlanMode::Live, origin), false)
            .await;
        lab.content
            .override_next
            .lock()
            .unwrap()
            .push(CompareOutcome::Differs { offset: 5 });
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
        assert!(matches!(
            report.actions[0].outcome,
            ActionOutcome::Failed { .. }
        ));
        match origin {
            PlanOrigin::AutoPolicy => {
                assert!(
                    matches!(report.actions[1].outcome, ActionOutcome::NotExecuted { .. }),
                    "{:?}",
                    report.actions[1]
                );
                assert_eq!(counting.replaces.load(Ordering::SeqCst), 0);
                assert_eq!(report.status, Some(PlanStatus::Failed));
            }
            PlanOrigin::User => {
                assert_eq!(
                    report.actions[1].outcome,
                    ActionOutcome::Verified,
                    "{:?}",
                    report.actions[1]
                );
                assert_eq!(counting.replaces.load(Ordering::SeqCst), 1);
                assert_eq!(report.status, Some(PlanStatus::PartiallyExecuted));
            }
        }
        // Both hashes were stopped once and started once (a untouched, b verified or untouched).
        assert_eq!(
            lab.qbit.calls_of("start"),
            vec![BTreeSet::from([hash('a'), hash('b')])]
        );
        assert!(report.left_stopped.is_empty());
    }
}

#[tokio::test]
async fn paths_never_reach_a_shell_and_the_media_root_is_the_only_write_location() {
    // Structural: the mutator works on directory handles and the only files that may change
    // are the plan's targets. A sibling with the same name in completed is untouched.
    let lab = Lab::new().await;
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, user()), false)
        .await;
    let (mutators, _) = lab.live();
    let before: Vec<_> = ["Rel/a.mkv", "Rel/b.mkv"]
        .iter()
        .map(|r| lab.phys(&lab.completed_path(r)))
        .collect();
    let before_b = lab.phys(&lab.media_path("Movies/Rel/b.mkv"));
    execute(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    let after: Vec<_> = ["Rel/a.mkv", "Rel/b.mkv"]
        .iter()
        .map(|r| lab.phys(&lab.completed_path(r)))
        .collect();
    assert_eq!(before, after);
    assert_eq!(lab.phys(&lab.media_path("Movies/Rel/b.mkv")), before_b);
    let names: BTreeSet<String> = fs::read_dir(lab.media_path("Movies/Rel"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["a.mkv".to_string(), "b.mkv".to_string()]),
        "no temporary link left"
    );
    let _ = RootId::completed();
}
