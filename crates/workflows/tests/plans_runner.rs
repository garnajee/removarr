#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A queue removal built by the real classifier and planner, executed by the real runner
//! (CP2T5-F01, T5-20): the planner's shape (download id in `torrent_hashes`, no wanted-file
//! pin) is accepted end to end, and a removal whose evidence is gone is refused.

mod actions_common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;

use actions_common::arrfake::media_ref;
use actions_common::tier4::NEW_SOURCE;
use actions_common::*;
use removarr_domain::arr::{ArrLibraryEntry, ArrService, MediaIdentity, MediaKind};
use removarr_domain::ids::PlanId;
use removarr_domain::issue::IssueCategory;
use removarr_domain::journal::ActionOutcome;
use removarr_domain::path::RootId;
use removarr_domain::plan::{ActionKind, PlanMode, PlanOrigin, PlanStatus, Precondition};
use removarr_domain::policy::RiskTier;
use removarr_domain::ports::{ArrRead, CancelFlag, FsInventory, IdGen, ScanControl, ScanStore};
use removarr_domain::session::{ServiceKind, ServiceState, ServiceStatus};
use removarr_workflows::actions::{Reporter, execute};
use removarr_workflows::issues::{
    ArrObservation, IssueSettings, Observation, QbitObservation, classify,
};
use removarr_workflows::plans::{
    ALL_CAPABILITIES, BuildContext, Built, IssueAction, PlanInput, PlanRequest, PlanSettings,
    action_subjects, build_plan,
};

/// The lab's stored session, real inventories, fake listing and ARR state as planner input.
async fn planner_input(lab: &Lab) -> (PlanInput, IssueSettings) {
    let mut inventories = Vec::new();
    for r in [RootId::completed(), RootId::incomplete(), lab.media.clone()] {
        inventories.push(lab.fs.scan_root(&r, &ScanControl::default()).unwrap());
    }
    let mut session = lab.repos.scans.latest_session().await.unwrap().unwrap();
    session.services.push(ServiceStatus {
        service: ServiceKind::Sonarr,
        state: ServiceState::Ok {
            version: "4".into(),
        },
        observed_at: NOW,
    });
    let media = media_ref(5, &[11]);
    let library = vec![ArrLibraryEntry {
        media: media.clone(),
        identity: MediaIdentity {
            kind: MediaKind::Episode,
            arr: Some(media),
            external_ids: BTreeMap::new(),
            title: "Rel".into(),
            year: None,
            season: Some(1),
            episode: Some(11),
            absolute: None,
        },
        file: lab.arr.file_entry(1, "Movies/New/new.mkv"),
        monitored: true,
    }];
    let o = Observation {
        session: session.id,
        session_status: session.status,
        observed_at: NOW,
        now: NOW,
        configured_roots: vec![RootId::completed(), RootId::incomplete(), lab.media.clone()],
        inventories,
        qbit: QbitObservation::Available(lab.qbit.snapshot()),
        arr: vec![ArrObservation {
            service: ArrService::Sonarr,
            state: ServiceState::Ok {
                version: "4".into(),
            },
            captured_at: NOW,
            library,
            queue: lab.arr.queue().await.unwrap(),
            history: vec![],
        }],
        matches: vec![],
        ignored: BTreeSet::new(),
    };
    let md = fs::metadata(lab.completed_path(NEW_SOURCE)).unwrap();
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

/// Plans the removal of queue item 7 from its full `arr_queue` issue.
async fn planned_queue_removal(lab: &Lab) -> Built {
    let (input, files) = planner_input(lab).await;
    let issue = input
        .issues
        .iter()
        .find(|i| i.category == IssueCategory::ArrQueue && i.subject.as_str() == "q:sonarr:7")
        .expect("the stale queue item is an arr_queue issue")
        .clone();
    assert!(
        issue.is_unblocked(),
        "INV-39 proven at classification: {:?}",
        issue.blockers
    );
    let mut settings = PlanSettings::new(CFG, files);
    settings.mode = PlanMode::Live;
    settings.implemented_tier = RiskTier::T4;
    settings.capabilities = ALL_CAPABILITIES.into_iter().collect();
    build_plan(
        &PlanRequest::issues(
            IssueCategory::ArrQueue,
            vec![issue.id.clone()],
            IssueAction::RemoveQueueItem,
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
    .expect("planned")
}

async fn execute_plan(lab: &Lab, built: &Built) -> removarr_workflows::actions::ExecutionReport {
    let ticket = lab.ticket(built.plan.clone(), false).await;
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
    assert_eq!(counting.mutations(), 0, "a queue removal touches no file");
    assert_eq!(lab.qbit.mutation_count(), 0, "nor any torrent");
    report
}

#[tokio::test]
async fn a_planner_built_queue_removal_executes_end_to_end() {
    let lab = Lab::new().await;
    lab.t4_setup().await;
    lab.t4_proven_queue().await;
    let built = planned_queue_removal(&lab).await;
    let plan = built.plan.plan();
    assert_eq!(plan.actions.len(), 1, "{:?}", plan.excluded);
    let a = &plan.actions[0];
    assert_eq!(a.kind, ActionKind::ArrRemoveQueueItem);
    assert_eq!(
        a.torrent_hashes,
        [hash('e')].into(),
        "the download id is named"
    );
    assert!(
        !a.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::TorrentWantedFiles { .. })),
        "no wanted-file pin: the torrent is not touched"
    );
    assert!(
        action_subjects(a).contains(&removarr_domain::ids::SubjectKey::queue(
            ArrService::Sonarr,
            7
        ))
    );
    let id = plan.id;
    let report = execute_plan(&lab, &built).await;
    assert_eq!(
        report.actions[0].outcome,
        ActionOutcome::Verified,
        "{report:?}"
    );
    assert_eq!(report.status, Some(PlanStatus::Executed));
    assert_eq!(lab.status(id).await, PlanStatus::Executed);
    assert_eq!(lab.arr.calls("remove_queue"), 1);
}

#[tokio::test]
async fn a_planner_built_queue_removal_is_refused_when_the_import_proof_is_gone() {
    let lab = Lab::new().await;
    lab.t4_setup().await;
    lab.t4_proven_queue().await;
    let built = planned_queue_removal(&lab).await;
    // After planning, ARR's library file becomes a copy: INV-39 no longer holds.
    fs::remove_file(lab.media_path("Movies/New/new.mkv")).unwrap();
    fs::copy(
        lab.completed_path(NEW_SOURCE),
        lab.media_path("Movies/New/new.mkv"),
    )
    .unwrap();
    let report = execute_plan(&lab, &built).await;
    assert!(
        matches!(report.actions[0].outcome, ActionOutcome::NotExecuted { .. }),
        "{report:?}"
    );
    assert_eq!(lab.arr.calls("remove_queue"), 0);
}
