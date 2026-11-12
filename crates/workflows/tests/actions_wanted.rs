#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The pinned wanted-file set (CP2T3-F03, domain patch 3): the live `files()` set of every
//! affected hash must equal the plan's `TorrentWantedFiles` pin. Any difference, a removed
//! torrent or a read fault refuses with `ChangedSincePlanning` before any mutation; and a
//! consolidation built by the real planner (which emits the pins) executes end to end.

mod actions_common;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;

use actions_common::*;
use removarr_domain::evidence::{ContentProof, IdentityProof, MatchCandidate};
use removarr_domain::ids::PlanId;
use removarr_domain::issue::{BlockerCode, IssueCategory};
use removarr_domain::journal::ActionOutcome;
use removarr_domain::path::RootId;
use removarr_domain::plan::{PlanMode, PlanOrigin, PlanStatus, Precondition};
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{CancelFlag, FsInventory, IdGen, ScanControl, ScanStore};
use removarr_workflows::actions::{Reporter, execute};
use removarr_workflows::issues::{IssueSettings, Observation, QbitObservation, classify};
use removarr_workflows::plans::{
    ALL_CAPABILITIES, BuildContext, IssueAction, PlanInput, PlanRequest, PlanSettings, build_plan,
};

const ONE: &[(&str, &str, &[char])] = &[("Rel/a.mkv", "Movies/Rel/a.mkv", &['a'])];

/// Plans first, applies `change` to the fake, then executes.
async fn run(lab: &Lab, change: impl FnOnce(&Lab)) -> removarr_workflows::actions::ExecutionReport {
    let ticket = lab
        .ticket(lab.plan(ONE, PlanMode::Live, PlanOrigin::User), false)
        .await;
    change(lab);
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
    if report.status != Some(PlanStatus::Executed) {
        assert_eq!(counting.mutations(), 0, "a refused plan mutates nothing");
        assert_eq!(lab.qbit.mutation_count(), 0, "not even a stop");
    }
    report
}

fn refused_changed(report: &removarr_workflows::actions::ExecutionReport, what: &str) {
    let ActionOutcome::NotExecuted { blockers } = &report.actions[0].outcome else {
        panic!("{what}: {:?}", report.actions[0].outcome);
    };
    assert!(
        blockers
            .iter()
            .any(|b| b.code == BlockerCode::ChangedSincePlanning),
        "{what}: {blockers:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Invalidated), "{what}");
}

#[tokio::test]
async fn the_pinned_wanted_set_passes_unchanged_and_every_change_refuses_before_any_mutation() {
    // The lab plan pins the wanted set of `a` exactly as the planner does.
    let lab = Lab::new().await;
    let plan = lab.plan(ONE, PlanMode::Live, PlanOrigin::User);
    assert!(
        plan.plan().actions[0]
            .preconditions
            .iter()
            .any(|p| matches!(
                p,
                Precondition::TorrentWantedFiles { hash, wanted }
                    if *hash == hash_of('a') && wanted.len() == 1
            ))
    );
    assert_eq!(run(&lab, |_| {}).await.status, Some(PlanStatus::Executed));

    // A wanted file added after planning.
    let lab = Lab::new().await;
    let r = run(&lab, |l| {
        l.qbit
            .add_file('a', "Rel/extra.mkv", &RootId::completed(), "Rel/extra.mkv");
    })
    .await;
    refused_changed(&r, "added wanted file");

    // A file deselected after planning (priority 0).
    let lab = Lab::new().await;
    let r = run(&lab, |l| l.qbit.set_priority('a', 0, 0)).await;
    refused_changed(&r, "deselected file");

    // The torrent was removed after planning.
    let lab = Lab::new().await;
    let r = run(&lab, |l| l.qbit.remove_torrent('a')).await;
    refused_changed(&r, "removed torrent");

    // The file list cannot be read (the listing can): fail closed.
    let lab = Lab::new().await;
    let r = run(&lab, |l| *l.qbit.fail_files.lock().unwrap() = true).await;
    refused_changed(&r, "read fault");
}

fn hash_of(c: char) -> removarr_domain::torrent::TorrentHash {
    hash(c)
}

/// The lab's real roots and fake listing as the planner's input.
async fn observation(lab: &Lab) -> (PlanInput, IssueSettings) {
    let mut inventories = Vec::new();
    for r in [RootId::completed(), RootId::incomplete(), lab.media.clone()] {
        inventories.push(lab.fs.scan_root(&r, &ScanControl::default()).unwrap());
    }
    let session = lab.repos.scans.latest_session().await.unwrap().unwrap();
    let src = rp(&RootId::completed(), "Rel/a.mkv");
    let tgt = rp(&lab.media, "Movies/Rel/a.mkv");
    let (sa, sb) = (lab.sig(&src), lab.sig(&tgt));
    let o = Observation {
        session: session.id,
        session_status: session.status,
        observed_at: NOW,
        now: NOW,
        configured_roots: vec![RootId::completed(), RootId::incomplete(), lab.media.clone()],
        inventories,
        qbit: QbitObservation::Available(lab.qbit.snapshot()),
        arr: vec![],
        matches: vec![MatchCandidate {
            a: src,
            b: tgt,
            sig_a: sa,
            sig_b: sb,
            identity: IdentityProof::NameParse { score_milli: 1000 },
            content: ContentProof::NameSize,
            reasons: vec!["same_name_and_size".into()],
            session: session.id,
        }],
        ignored: BTreeSet::new(),
    };
    let md = std::fs::metadata(lab.completed_path("Rel/a.mkv")).unwrap();
    let settings = IssueSettings::new((md.uid(), md.gid()));
    let classified = classify(&o, &settings).expect("classifies");
    (
        PlanInput {
            session,
            observation: o,
            issues: classified.issues,
            arr: BTreeMap::new(),
            search: None,
        },
        settings,
    )
}

#[tokio::test]
async fn a_consolidation_built_through_the_planner_executes_end_to_end() {
    let lab = Lab::new().await;
    let (input, files) = observation(&lab).await;
    let id = input
        .issues
        .iter()
        .find(|i| {
            i.category == IssueCategory::HardlinkMissing && i.subject.as_str().contains("Rel/a.mkv")
        })
        .map(|i| i.id.clone())
        .expect("the completed copy without a library inode is a hardlink_missing issue");
    let mut settings = PlanSettings::new(CFG, files);
    settings.mode = PlanMode::Live;
    settings.implemented_tier = RiskTier::T3;
    settings.capabilities = ALL_CAPABILITIES.into_iter().collect();
    let built = build_plan(
        &PlanRequest::issues(
            IssueCategory::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ),
        &input,
        &settings,
        &BuildContext {
            id: PlanId::new(lab.ids.uuid()),
            now: NOW,
            origin: PlanOrigin::User,
            auto: None,
        },
    )
    .expect("planned");
    let plan = built.plan.plan();
    assert_eq!(plan.actions.len(), 1, "{:?}", plan.excluded);
    assert!(
        plan.actions[0]
            .preconditions
            .iter()
            .any(|p| matches!(p, Precondition::TorrentWantedFiles { .. })),
        "the planner pins the wanted set"
    );
    let ticket = lab.ticket(built.plan.clone(), false).await;
    let plan_id = plan.id;
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
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert_eq!(lab.status(plan_id).await, PlanStatus::Executed);
    assert_eq!(
        counting.replaces.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        lab.phys(&lab.completed_path("Rel/a.mkv")),
        lab.phys(&lab.media_path("Movies/Rel/a.mkv")),
        "the media copy now carries the completed inode"
    );
}
