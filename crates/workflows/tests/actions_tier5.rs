#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Runner-side tier 5 (CP2-tier5 T5-01, T5-03..T5-09, T5-13, T5-15, T5-18, T5-21), behind the
//! production `execute` (`IMPLEMENTED_TIER = T5`); `execute_with_gate` is used only to prove a
//! lower gate still refuses automatic plans.

mod actions_common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use actions_common::arrfake::{decision, episode, media_ref};
use actions_common::*;
use removarr_domain::evidence::EvidenceLevel;
use removarr_domain::fs::CompareOutcome;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::BlockerCode;
use removarr_domain::journal::ActionOutcome;
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{
    ActionKind, ApprovalRequest, HashedPlan, PlanMode, PlanOrigin, PlanStatus, Precondition,
};
use removarr_domain::policy::{Capability, RiskTier};
use removarr_domain::ports::{CancelFlag, IgnoreStore, PlanStore};
use removarr_workflows::actions::{
    ExecutionReport, Mutators, Reporter, automatic_defects_for_test, execute,
};
use removarr_workflows::plans::ExecutionTicket;

fn set_age(path: std::path::PathBuf, hours_old: u64) {
    let f = fs::File::options().write(true).open(&path).unwrap();
    let at = std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(NOW.0).unwrap())
        - Duration::from_secs(hours_old * 3600);
    f.set_modified(at).unwrap();
}

/// Marks the lab's files old enough for the automatic minimum age (48 h).
fn age_files(lab: &Lab, hours: u64) {
    for p in ["Rel/a.mkv", "Rel/b.mkv"] {
        set_age(lab.completed_path(p), hours);
    }
    for p in ["Movies/Rel/a.mkv", "Movies/Rel/b.mkv"] {
        set_age(lab.media_path(p), hours);
    }
}

fn reseal(plan: HashedPlan, f: impl Fn(&mut removarr_domain::plan::PlannedAction)) -> HashedPlan {
    let (mut p, _) = plan.into_parts();
    for a in &mut p.actions {
        f(a);
    }
    HashedPlan::seal(p).unwrap()
}

async fn ticket_auto(lab: &Lab, plan: HashedPlan, simulate: bool) -> ExecutionTicket {
    let (p, h) = plan.clone().into_parts();
    lab.repos.plans.insert(&p, &h).await.unwrap();
    let approval = plan.approve_auto(NOW).unwrap();
    lab.repos.plans.approve(&approval).await.unwrap();
    ExecutionTicket {
        plan,
        status: PlanStatus::Approved,
        approval: Some(approval),
        simulate,
    }
}

async fn run_auto(
    lab: &Lab,
    plan: HashedPlan,
    simulate: bool,
) -> (ExecutionReport, Arc<CountingMutator>) {
    let ticket = ticket_auto(lab, plan, simulate).await;
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

fn consolidation(lab: &Lab, items: &[(&str, &str, &[char])]) -> HashedPlan {
    reseal(
        lab.plan(items, PlanMode::Live, PlanOrigin::AutoPolicy),
        |a| {
            a.evidence_level = EvidenceLevel::NameSize;
        },
    )
}

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];
const TWO: &[(&str, &str, &[char])] = &[
    ("Rel/a.mkv", "Movies/Rel/a.mkv", &['a']),
    ("Rel/b.mkv", "Movies/Rel/b.mkv", &['b']),
];

/// ARR serves `Movies/Rel/a.mkv` for episode 11 and rejects the completed copy as not an
/// upgrade: the evidence of an automatic `DeleteReplaced`.
fn arr_validated_deletion(lab: &Lab, with_task: bool) -> HashedPlan {
    let target = rp(&RootId::completed(), "Rel/a.mkv");
    let repl = rp(&lab.media, "Movies/Rel/a.mkv");
    let m = media_ref(5, &[11]);
    let size = lab.sig(&repl).size;
    let file = lab.arr.file_entry(1, "Movies/Rel/a.mkv");
    lab.arr.with(|s| {
        s.episodes = vec![episode(11, Some(1), true)];
        s.files = vec![file];
        let mut d = decision(&target, &[11]);
        d.approved = false;
        d.rejections = vec!["Not an upgrade".into()];
        s.previews.insert(rp(&RootId::completed(), "Rel"), vec![d]);
    });
    let mut del = lab.delete_file_action(
        0,
        ActionKind::DeleteCompletedFile,
        &RootId::completed(),
        "Rel/a.mkv",
        &['a'],
    );
    del.preconditions.extend([
        Precondition::ArrReplacementPresent {
            media: m.clone(),
            path: repl.clone(),
            size,
        },
        Precondition::EntryUnchanged {
            path: repl.clone(),
            sig: lab.sig(&repl),
            compare_nlink: false,
        },
        Precondition::ArrFingerprint {
            media: m.clone(),
            fingerprint: lab.arr.fingerprint(5),
        },
    ]);
    del.evidence_level = EvidenceLevel::ArrValidatedReplacement;
    let mut actions = vec![del];
    if with_task {
        let mut t = lab.remove_task_action(1, 'a', &[0]);
        t.evidence_level = EvidenceLevel::Full;
        actions.push(t);
    }
    lab.seal(actions, PlanOrigin::AutoPolicy, vec![])
}

fn codes(o: &ActionOutcome) -> Vec<BlockerCode> {
    match o {
        ActionOutcome::NotExecuted { blockers } => {
            blockers.iter().map(|b| b.code.clone()).collect()
        }
        other => panic!("{other:?}"),
    }
}

fn untouched(lab: &Lab, counting: &CountingMutator) {
    assert_eq!(
        counting.mutations() + lab.qbit.mutation_count() + lab.service_mutations(),
        0
    );
}

#[tokio::test]
async fn t5_01_the_production_execute_accepts_a_valid_automatic_plan_and_refuses_invalid_ones() {
    // Valid: runs through every automatic check on the production entry point.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    let id = plan.plan().id;
    let (report, _) = run_auto(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert_eq!(lab.status(id).await, PlanStatus::Executed);

    // Invalid: too young, ignored subject, deletion without ARR-validated evidence.
    let lab = Lab::new().await;
    age_files(&lab, 1);
    let (report, counting) = run_auto(&lab, consolidation(&lab, ONE), false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::Recent));
    untouched(&lab, &counting);

    let lab = Lab::new().await;
    age_files(&lab, 72);
    let target = rp(&lab.media, "Movies/Rel/a.mkv");
    lab.repos
        .ignores
        .ignore(&[SubjectKey::rooted(&target)], None, NOW)
        .await
        .unwrap();
    let (report, counting) = run_auto(&lab, consolidation(&lab, ONE), false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::Ignored));
    untouched(&lab, &counting);

    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = reseal(arr_validated_deletion(&lab, false), |a| {
        a.evidence_level = EvidenceLevel::Full
    });
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrNotUpgradeEvidence));
    untouched(&lab, &counting);

    // A lower gate (test entry) still refuses every automatic plan before any intent.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let ticket = ticket_auto(&lab, consolidation(&lab, ONE), false).await;
    let (mutators, counting) = lab.live();
    let report = removarr_workflows::actions::execute_with_gate(
        &lab.ports(),
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
        RiskTier::T4,
    )
    .await
    .unwrap();
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::NotInBuild]
    );
    untouched(&lab, &counting);
}

#[tokio::test]
async fn t5_03_t5_04_t5_13_approval_evidence_and_samples_are_checked_by_the_runner() {
    // A user approval of an automatic plan is refused.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    let (p, h) = plan.clone().into_parts();
    lab.repos.plans.insert(&p, &h).await.unwrap();
    let approval = plan
        .approve(
            &ApprovalRequest {
                plan_hash: h,
                confirmed_count: p.required_count(),
                confirmed_torrent_removals: p.required_torrent_removals(),
                acknowledged_samples: vec![],
                acknowledged_unrecoverable: vec![],
            },
            NOW,
        )
        .unwrap();
    lab.repos.plans.approve(&approval).await.unwrap();
    let ticket = ExecutionTicket {
        plan,
        status: PlanStatus::Approved,
        approval: Some(approval),
        simulate: false,
    };
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
        codes(&report.actions[0].outcome),
        vec![BlockerCode::CapabilityDisabled]
    );
    untouched(&lab, &counting);

    // A deletion without ARR-validated evidence: refused (approve_auto itself rejects the
    // missing evidence of a non-validated level only for `None`; the runner is stricter).
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = reseal(arr_validated_deletion(&lab, false), |a| {
        a.evidence_level = EvidenceLevel::Full
    });
    let (p, h) = plan.clone().into_parts();
    lab.repos.plans.insert(&p, &h).await.unwrap();
    let approval = plan.approve_auto(NOW).unwrap();
    lab.repos.plans.approve(&approval).await.unwrap();
    let ticket = ExecutionTicket {
        plan,
        status: PlanStatus::Approved,
        approval: Some(approval),
        simulate: false,
    };
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
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrNotUpgradeEvidence));
    untouched(&lab, &counting);
    assert!(lab.completed_path("Rel/a.mkv").exists());

    // A consolidation without a content level cannot be approved automatically at all.
    let lab = Lab::new().await;
    let none = reseal(lab.plan(ONE, PlanMode::Live, PlanOrigin::AutoPolicy), |a| {
        a.evidence_level = EvidenceLevel::None
    });
    assert!(none.approve_auto(NOW).is_err());
}

#[tokio::test]
async fn t5_18_no_manual_only_kind_runs_in_an_automatic_plan() {
    // `HashedPlan::seal` refuses these plans; the runner's own allowlist is checked directly on
    // an unsealed copy, for every kind outside `is_auto_eligible`.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let valid = consolidation(&lab, ONE);
    let (base, _) = valid.clone().into_parts();
    for kind in [
        ActionKind::CleanupStalledIncomplete,
        ActionKind::DeleteIncompleteOrphan,
        ActionKind::DeleteJunk,
        ActionKind::FixMode,
        ActionKind::ReplaceRelease,
        ActionKind::DeleteMediaFile,
        ActionKind::ArrGrabRelease,
        ActionKind::ArrRemoveQueueItem,
        ActionKind::ArrSetMonitored,
        ActionKind::ArrRescan,
        ActionKind::JellyfinRefresh,
    ] {
        let mut p = base.clone();
        p.actions[0].kind = kind;
        let d = automatic_defects_for_test(&p, &valid, None, &lab.settings(), NOW, true);
        assert!(
            d.get(&0)
                .is_some_and(|b| b.iter().any(|x| x.code == BlockerCode::CapabilityDisabled)),
            "{kind:?}: {d:?}"
        );
    }
    // And the eligible kinds pass the pure checks.
    let d = automatic_defects_for_test(
        &base,
        &valid,
        valid_approval(&lab, &valid).as_ref(),
        &lab.settings(),
        NOW,
        false,
    );
    assert!(d.is_empty(), "{d:?}");
}

fn valid_approval(_: &Lab, plan: &HashedPlan) -> Option<removarr_domain::plan::Approval> {
    plan.approve_auto(NOW).ok()
}

#[tokio::test]
async fn t5_05_the_arr_replacement_and_a_re_read_preview_are_checked_at_execution() {
    // Happy path with the companion task removal (A8 capability on).
    let lab = Lab::new().await;
    age_files(&lab, 72);
    lab.capabilities
        .lock()
        .unwrap()
        .insert(Capability::AutoRemoveEmptyTorrents);
    let plan = arr_validated_deletion(&lab, true);
    let (report, _) = run_auto(&lab, plan, false).await;
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
    assert!(!lab.completed_path("Rel/a.mkv").exists());
    assert!(lab.media_path("Movies/Rel/a.mkv").exists());
    assert!(!lab.qbit.listed('a'));

    // A8: the same plan without `auto_remove_empty_torrents`: nothing runs.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = arr_validated_deletion(&lab, true);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[1].outcome).contains(&BlockerCode::CapabilityDisabled));
    untouched(&lab, &counting);
    assert!(lab.completed_path("Rel/a.mkv").exists());

    // The preview now approves the completed file: it is no longer "not an upgrade".
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = arr_validated_deletion(&lab, false);
    lab.arr.with(|s| {
        for v in s.previews.values_mut() {
            v[0].approved = true;
            v[0].rejections.clear();
        }
    });
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrNotUpgradeEvidence));
    untouched(&lab, &counting);

    // ARR still rejects, but now for a reason other than "not an upgrade": as strict as plan time.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = arr_validated_deletion(&lab, false);
    lab.arr.with(|s| {
        for v in s.previews.values_mut() {
            v[0].rejections = vec!["Unknown series".into()];
        }
    });
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrRejected));
    untouched(&lab, &counting);

    // ARR's current file is no longer the pinned replacement.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = arr_validated_deletion(&lab, false);
    let other = lab.arr.file_entry(2, "Movies/Rel/b.mkv");
    lab.arr.with(|s| {
        s.files = vec![other];
        s.episodes[0].file_id = Some(2);
    });
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrAmbiguous));
    untouched(&lab, &counting);

    // ARR is down: fail closed.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = arr_validated_deletion(&lab, false);
    lab.arr.with(|s| s.fail_reads = true);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::ArrAmbiguous));
    untouched(&lab, &counting);
}

#[tokio::test]
async fn t5_06_a_recent_entry_is_never_executed() {
    let lab = Lab::new().await;
    age_files(&lab, 1);
    let plan = consolidation(&lab, ONE);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(codes(&report.actions[0].outcome).contains(&BlockerCode::Recent));
    untouched(&lab, &counting);
}

#[tokio::test]
async fn t5_07_the_whole_root_perimeter_is_checked_not_only_around_the_path() {
    if is_root_user() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    // An unreadable directory far from the target: `NoScanErrorUnder` of the target holds,
    // the root is not clean.
    fs::set_permissions(
        lab.completed_path("Orphan/Subs"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    lab.store_observation_again().await;
    let (report, counting) = run_auto(&lab, plan, false).await;
    fs::set_permissions(
        lab.completed_path("Orphan/Subs"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(
        codes(&report.actions[0].outcome).contains(&BlockerCode::ScanIncomplete),
        "{report:?}"
    );
    untouched(&lab, &counting);

    // qBittorrent unreachable.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    *lab.qbit.fail_reads.lock().unwrap() = true;
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(matches!(
        report.actions[0].outcome,
        ActionOutcome::NotExecuted { .. }
    ));
    untouched(&lab, &counting);
}

#[tokio::test]
async fn t5_09_the_ignore_list_is_re_read_at_execution() {
    // Ignored after planning and approval.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    let target = rp(&lab.media, "Movies/Rel/a.mkv");
    lab.repos
        .ignores
        .ignore(&[SubjectKey::rooted(&target)], None, NOW)
        .await
        .unwrap();
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::Ignored]
    );
    untouched(&lab, &counting);

    // No ignore list available: fail closed.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    let ticket = ticket_auto(&lab, plan, false).await;
    let mut ports = lab.ports();
    ports.ignores = None;
    let (mutators, counting) = lab.live();
    let report = execute(
        &ports,
        &mutators,
        &lab.settings(),
        ticket,
        &CancelFlag::new(),
        &Reporter::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::Ignored]
    );
    untouched(&lab, &counting);
}

#[tokio::test]
async fn t5_08_an_automatic_batch_stops_at_the_first_uncertainty() {
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, TWO);
    lab.content
        .override_next
        .lock()
        .unwrap()
        .push(CompareOutcome::Differs { offset: 5 });
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::Failed { .. }),
        "{report:?}"
    );
    assert!(
        matches!(report.actions[1].outcome, ActionOutcome::NotExecuted { .. }),
        "{report:?}"
    );
    assert_eq!(
        counting.replaces.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(report.status, Some(PlanStatus::Failed));
}

#[tokio::test]
async fn t5_15_automatic_simulations_mutate_nothing_in_either_process() {
    for dry_run_process in [true, false] {
        let lab = Lab::new().await;
        age_files(&lab, 72);
        let plans = [
            consolidation(&lab, TWO),
            arr_validated_deletion(&lab, false),
        ];
        let (rec, recording) = Mutators::recording(&[
            removarr_domain::arr::ArrService::Sonarr,
            removarr_domain::arr::ArrService::Radarr,
        ]);
        let (live, counting) = lab.live();
        for plan in plans {
            let ticket = ticket_auto(&lab, plan, true).await;
            let m = if dry_run_process { &rec } else { &live };
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
        }
        assert_eq!(recording.calls(), 0);
        untouched(&lab, &counting);
        assert!(lab.completed_path("Rel/a.mkv").exists());
        assert!(lab.qbit.listed('a'));
    }
}

#[tokio::test]
async fn t5_13_samples_are_never_automatic() {
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = reseal(consolidation(&lab, ONE), |_| {});
    let (mut p, _) = plan.into_parts();
    p.samples = vec![RootedPath::new(
        RootId::completed(),
        removarr_domain::path::RelPath::new("Orphan/Sample/s.mkv").unwrap(),
    )];
    let sealed = HashedPlan::seal(p).unwrap();
    // `approve_auto` refuses it, so no automatic ticket exists; a manual one is refused by the
    // runner's approver check as well.
    assert!(sealed.approve_auto(NOW).is_err());
    let d = automatic_defects_for_test(sealed.plan(), &sealed, None, &lab.settings(), NOW, true);
    assert!(
        d.values().flatten().any(|b| b.code == BlockerCode::Sample),
        "{d:?}"
    );
}

fn set_dir_age(path: std::path::PathBuf, hours_old: u64) {
    let f = fs::File::open(&path).unwrap();
    let at = std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(NOW.0).unwrap())
        - Duration::from_secs(hours_old * 3600);
    f.set_modified(at).unwrap();
}

/// An automatic `DeleteCompletedFolder` of `Season/` (a video, its sidecars, optionally one more
/// video) replaced by ARR's `Movies/Rel/a.mkv`; ARR's preview lists only `ep.mkv`.
fn season_deletion(lab: &Lab, extra_video: bool) -> HashedPlan {
    let c = RootId::completed();
    fs::create_dir_all(lab.completed_path("Season")).unwrap();
    for (name, bytes) in [
        ("ep.mkv", 4096usize),
        ("ep.nfo", 10),
        ("ep.en.srt", 20),
        ("poster.jpg", 30),
    ] {
        fs::write(
            lab.completed_path(&format!("Season/{name}")),
            vec![5u8; bytes],
        )
        .unwrap();
    }
    if extra_video {
        fs::write(lab.completed_path("Season/ep2.mkv"), vec![6u8; 4096]).unwrap();
    }
    for name in ["ep.mkv", "ep.nfo", "ep.en.srt", "poster.jpg", "ep2.mkv"] {
        let p = lab.completed_path(&format!("Season/{name}"));
        if p.exists() {
            set_age(p, 72);
        }
    }
    set_dir_age(lab.completed_path("Season"), 72);
    set_age(lab.media_path("Movies/Rel/a.mkv"), 72);
    let repl = rp(&lab.media, "Movies/Rel/a.mkv");
    let m = media_ref(5, &[11]);
    let file = lab.arr.file_entry(1, "Movies/Rel/a.mkv");
    let ep = rp(&c, "Season/ep.mkv");
    lab.arr.with(|s| {
        s.episodes = vec![episode(11, Some(1), true)];
        s.files = vec![file];
        let mut d = decision(&ep, &[11]);
        d.approved = false;
        d.rejections = vec!["Not an upgrade".into()];
        s.previews.insert(rp(&c, "Season"), vec![d]);
    });
    let mut del = lab.delete_folder_action(0, "Season");
    del.preconditions.extend([
        Precondition::ArrReplacementPresent {
            media: m.clone(),
            path: repl.clone(),
            size: lab.sig(&repl).size,
        },
        Precondition::EntryUnchanged {
            path: repl.clone(),
            sig: lab.sig(&repl),
            compare_nlink: false,
        },
        Precondition::ArrFingerprint {
            media: m,
            fingerprint: lab.arr.fingerprint(5),
        },
    ]);
    del.evidence_level = EvidenceLevel::ArrValidatedReplacement;
    lab.seal(vec![del], PlanOrigin::AutoPolicy, vec![])
}

#[tokio::test]
async fn cp2t5f_f04_an_automatic_folder_with_sidecars_runs_and_an_unlisted_extra_video_refuses() {
    // Sidecars (.nfo, .srt, image) are pinned by the folder snapshot, never judged by ARR.
    let lab = Lab::new().await;
    let plan = season_deletion(&lab, false);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert!(!lab.completed_path("Season").exists());
    assert!(lab.media_path("Movies/Rel/a.mkv").exists());
    assert_eq!(counting.trees.load(std::sync::atomic::Ordering::SeqCst), 1);
    // The preview is read once for the folder (one video).
    // A video ARR never evaluated is not covered by the evidence: refused, nothing removed.
    let lab = Lab::new().await;
    let plan = season_deletion(&lab, true);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(
        codes(&report.actions[0].outcome).contains(&BlockerCode::ArrNotUpgradeEvidence),
        "{report:?}"
    );
    untouched(&lab, &counting);
    assert!(lab.completed_path("Season/ep2.mkv").exists());
    // A file added after planning is caught by the folder pin (revalidation), not skipped.
    let lab = Lab::new().await;
    let plan = season_deletion(&lab, false);
    fs::write(lab.completed_path("Season/new.nfo"), b"x").unwrap();
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::NotExecuted { .. }),
        "{report:?}"
    );
    untouched(&lab, &counting);
    // An unknown non-video file in the folder is refused.
    let lab = Lab::new().await;
    fs::create_dir_all(lab.completed_path("Season")).unwrap();
    fs::write(lab.completed_path("Season/blob.bin"), b"x").unwrap();
    let plan = season_deletion(&lab, false);
    let (report, counting) = run_auto(&lab, plan, false).await;
    assert!(
        codes(&report.actions[0].outcome).contains(&BlockerCode::ArrAmbiguous),
        "{report:?}"
    );
    untouched(&lab, &counting);
}

#[tokio::test]
async fn cp2t5f_f02_f03_runner_uses_the_planners_subjects_and_age_set() {
    // A torrent that holds the unit is a subject too: ignoring it refuses the action.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    lab.repos
        .ignores
        .ignore(&[SubjectKey::torrent(&hash('a'))], None, NOW)
        .await
        .unwrap();
    let (report, counting) = run_auto(&lab, consolidation(&lab, ONE), false).await;
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::Ignored]
    );
    untouched(&lab, &counting);

    // The replacement media file is evidence, not a target: its age does not count.
    let lab = Lab::new().await;
    age_files(&lab, 72);
    set_age(lab.media_path("Movies/Rel/a.mkv"), 1);
    let plan = arr_validated_deletion(&lab, false);
    let (report, _) = run_auto(&lab, plan, false).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
}

#[tokio::test]
async fn cp2t5f_f06_an_approved_automatic_plan_never_runs_while_automatic_mode_is_inactive() {
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let plan = consolidation(&lab, ONE);
    let id = plan.plan().id;
    let ticket = ticket_auto(&lab, plan.clone(), false).await;
    let mut settings = lab.settings();
    settings.auto_mode_active = false;
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
    assert_eq!(
        codes(&report.actions[0].outcome),
        vec![BlockerCode::CapabilityDisabled]
    );
    assert!(lab.steps(id).await.is_empty());
    untouched(&lab, &counting);
    assert!(lab.completed_path("Rel/a.mkv").exists());
    // A dry-run simulation stays possible (it mutates nothing).
    let lab = Lab::new().await;
    age_files(&lab, 72);
    let ticket = ticket_auto(&lab, consolidation(&lab, ONE), true).await;
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
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Simulated,
        "{report:?}"
    );
    untouched(&lab, &counting);
}
