#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs
)]
//! Evaluate and VerifyContent (architecture 7.2) with the real SQLite repositories and read-only
//! fakes: proofs and ARR evidence are stored, nothing is mutated, and an ARR score change seen
//! by a fresh read invalidates a plan built on the old snapshot.

mod common;
mod plans_common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use common::*;
use plans_common::*;
use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrHistoryRecord, ArrLibraryEntry, ArrMediaManagement,
    ArrMediaRef, ArrQualitySnapshot, ArrQueueItem, ArrService, CommandState, ReleaseRef,
    ReleaseSearchResult,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{ContentProof, EvidenceKind, QualityEvidence};
use removarr_domain::fs::{
    CompareOutcome, DirSnapshot, EntryKind, EntrySignature, PermissionFinding, RootInventory,
    SampleSpec,
};
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{PlanStatus, Precondition};
use removarr_domain::ports::{
    ArrRead, ContentReader, FsInventory, IssueStore, MatchStore, ScanControl,
};
use removarr_domain::torrent::TorrentHash;
use removarr_matching::{ContentVerifier, ReadLimiter};
use removarr_workflows::evaluate::{VerifyPorts, evaluate_issues, evaluate_media, verify_content};
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{IssueAction as A, StaleReason};

// ---- read-only fakes ------------------------------------------------------------------------

struct StatFs(BTreeMap<RootedPath, EntrySignature>);

impl StatFs {
    fn of(o: &Observation) -> Arc<Self> {
        Arc::new(Self(
            o.inventories
                .iter()
                .flat_map(|i| i.entries.iter().map(|e| (e.path.clone(), e.sig)))
                .collect(),
        ))
    }
}

impl FsInventory for StatFs {
    fn scan_root(&self, _: &RootId, _: &ScanControl) -> Result<RootInventory, PortError> {
        Err(PortError::new(ErrorClass::Internal, "no scan in evaluate"))
    }
    fn stat(&self, p: &RootedPath) -> Result<Option<EntrySignature>, PortError> {
        Ok(self.0.get(p).copied())
    }
    fn snapshot_dir(&self, _: &RootedPath) -> Result<DirSnapshot, PortError> {
        Err(PortError::new(ErrorClass::Internal, "unexpected"))
    }
    fn free_space(&self, _: &RootId) -> Result<u64, PortError> {
        Err(PortError::new(ErrorClass::Internal, "unexpected"))
    }
    fn permission_findings(&self, _: &RootInventory, _: (u32, u32)) -> Vec<PermissionFinding> {
        vec![]
    }
}

struct Compare(CompareOutcome, AtomicUsize);

impl ContentReader for Compare {
    fn samples_match(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        _: [&EntrySignature; 2],
        _: SampleSpec,
        _: &ScanControl,
    ) -> Result<bool, PortError> {
        Ok(true)
    }
    fn full_compare(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        _: [&EntrySignature; 2],
        _: u32,
        _: &ScanControl,
    ) -> Result<CompareOutcome, PortError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(self.0)
    }
}

/// ARR read fake for evaluation (`ArrRead` has no mutating method at all).
struct ArrFake {
    service: ArrService,
    snapshot: Mutex<ArrQualitySnapshot>,
    previews: BTreeMap<RootedPath, Vec<ArrCandidateDecision>>,
    search: Vec<ReleaseSearchResult>,
    reads: AtomicUsize,
}

#[async_trait]
impl ArrRead for ArrFake {
    fn service(&self) -> ArrService {
        self.service
    }
    async fn status(&self) -> Result<String, PortError> {
        Ok("4".into())
    }
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError> {
        Ok(vec![])
    }
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError> {
        Ok(vec![])
    }
    async fn history_for_download(
        &self,
        _: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError> {
        Ok(vec![])
    }
    async fn quality_snapshot(&self, _: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.snapshot.lock().unwrap().clone())
    }
    async fn manual_import_preview(
        &self,
        folder: &RootedPath,
        _: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError> {
        Ok(self.previews.get(folder).cloned().unwrap_or_default())
    }
    async fn episodes(&self, _: i64, _: Option<i32>) -> Result<Vec<ArrEpisode>, PortError> {
        Ok(vec![])
    }
    async fn search_releases(
        &self,
        _: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError> {
        Ok(self.search.clone())
    }
    async fn command_status(&self, _: i64) -> Result<CommandState, PortError> {
        Err(PortError::new(ErrorClass::Internal, "unexpected"))
    }
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError> {
        Err(PortError::new(ErrorClass::Internal, "unexpected"))
    }
}

// ---- worlds -----------------------------------------------------------------------------------

fn copy_world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .file("Movie.A.2020.mkv", 10, 1000)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .file("movies/Movie.A.2020.mkv", 11, 1000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            "Movie.A.2020.mkv",
            &[("Movie.A.2020.mkv", "Movie.A.2020.mkv")],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, "Movie.A.2020.mkv"),
        rp(&m, "movies/Movie.A.2020.mkv"),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 1000, 1, 0o644),
        ContentProof::NameSize,
    )];
    o
}

const OLD: &str = "Old.Movie.2020.720p/Old.Movie.2020.720p.WEB.x264-GRP.mkv";
const LIB: &str = "movies/Old Movie (2020)/Old.Movie.2020.1080p.BluRay.x264-NEW.mkv";

fn movie_world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .dir("Old.Movie.2020.720p", 2)
        .file(OLD, 10, 1000)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/Old Movie (2020)", 6)
        .file(LIB, 11, 3000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            "Old.Movie.2020.720p",
            &[(OLD, OLD)],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, OLD),
        rp(&m, LIB),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 3000, 1, 0o644),
        ContentProof::None,
    )];
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(ArrService::Radarr, 7, rp(&m, LIB))],
        vec![],
    )];
    o
}

fn radarr(score: i64) -> Arc<ArrFake> {
    let (c, m) = (completed(), media());
    let movie = arr_media(ArrService::Radarr, 7);
    Arc::new(ArrFake {
        service: ArrService::Radarr,
        snapshot: Mutex::new(arr_snapshot(
            &movie,
            vec![file_ref(100, &rp(&m, LIB), 3000)],
            Some(mm(None, None, true)),
            score,
        )),
        previews: BTreeMap::from([(
            rp(&c, "Old.Movie.2020.720p"),
            vec![decision(
                &rp(&c, OLD),
                vec![],
                false,
                &["Not an upgrade for existing movie file"],
            )],
        )]),
        search: vec![],
        reads: AtomicUsize::new(0),
    })
}

fn hardlink_id(path: &str) -> IssueId {
    IssueId::derive(
        C::HardlinkMissing,
        &SubjectKey::rooted(&rp(&completed(), path)),
    )
}

// ---- tests ----------------------------------------------------------------------------------

#[tokio::test]
async fn verify_content_stores_full_compare_proofs_and_evidence() {
    for (outcome, identical) in [
        (CompareOutcome::Identical, true),
        (CompareOutcome::Differs { offset: 7 }, false),
    ] {
        let w = store_world().await;
        let o = copy_world();
        store_obs(&w, o.clone(), 7).await;
        let content = Arc::new(Compare(outcome, AtomicUsize::new(0)));
        let ports = VerifyPorts {
            issues: Arc::new(w.repos.issues.clone()),
            matches: Arc::new(w.repos.matches.clone()),
            fs: StatFs::of(&o),
            content: content.clone(),
            clock: w.clock.clone(),
        };
        let verifier = ContentVerifier::new(ReadLimiter::new(1), 65_536);
        let id = hardlink_id("Movie.A.2020.mkv");
        let report = verify_content(&ports, &verifier, &[id.clone()], &ScanControl::default())
            .await
            .unwrap();
        assert_eq!(content.1.load(Ordering::SeqCst), 1);
        assert_eq!(
            (report.identical, report.differs),
            (u32::from(identical), u32::from(!identical))
        );
        let proof = w.repos.matches.current().await.unwrap()[0].content.clone();
        assert_eq!(proof.proves_equal(), identical);
        assert_eq!(proof.proves_different(), !identical);
        let issue = w.repos.issues.get_many(&[id]).await.unwrap().remove(0);
        assert!(
            issue
                .evidence
                .iter()
                .any(|e| matches!(&e.kind, EvidenceKind::Content { proof: p, .. } if *p == proof))
        );
    }
}

#[tokio::test]
async fn evaluation_attaches_arr_evidence_and_stores_the_snapshot() {
    let w = store_world().await;
    let o = movie_world();
    store_obs(&w, o.clone(), 7).await;
    let arr = radarr(10);
    let planner = store_planner(&w, live(), vec![arr.clone()]);
    let id = hardlink_id(OLD);
    let report = evaluate_issues(&planner, &[id.clone()], o.arr.clone())
        .await
        .unwrap();
    assert_eq!(report.evaluated, [id.clone()]);
    let issue = w.repos.issues.get_many(&[id]).await.unwrap().remove(0);
    let fp = arr.snapshot.lock().unwrap().fingerprint.clone();
    assert!(issue.evidence.iter().any(|e| matches!(&e.kind,
        EvidenceKind::Quality { proof: QualityEvidence::ArrSnapshot { fingerprint }, .. } if *fingerprint == fp)));
    assert!(issue.evidence.iter().any(|e| matches!(&e.kind,
        EvidenceKind::Identity { subject, .. } if subject.starts_with("note:arr_preview:rejected:not_upgrade:"))));
}

#[tokio::test]
async fn an_arr_score_change_invalidates_a_stored_plan() {
    let w = store_world().await;
    let o = movie_world();
    store_obs(&w, o.clone(), 7).await;
    let arr = radarr(10);
    let planner = store_planner(&w, live(), vec![arr.clone()]);
    let request = req(
        C::HardlinkMissing,
        vec![hardlink_id(OLD)],
        A::DeleteReplaced,
    );
    let built = planner.create(&request, o.arr.clone()).await.unwrap();
    let id = built.plan.plan().id;
    assert!(
        built.plan.plan().actions[0]
            .preconditions
            .iter()
            .any(|p| matches!(p, Precondition::ArrFingerprint { .. }))
    );
    assert_eq!(
        planner
            .refresh_status(id, o.arr.clone())
            .await
            .unwrap()
            .status,
        PlanStatus::Draft
    );
    // ARR's custom-format score changes: the old plan never adapts.
    let movie = arr_media(ArrService::Radarr, 7);
    *arr.snapshot.lock().unwrap() = arr_snapshot(
        &movie,
        vec![file_ref(100, &rp(&media(), LIB), 3000)],
        Some(mm(None, None, true)),
        99,
    );
    let check = planner.refresh_status(id, o.arr.clone()).await.unwrap();
    assert_eq!(check.status, PlanStatus::Invalidated);
    assert!(
        check
            .reasons
            .iter()
            .any(|r| matches!(r, StaleReason::ArrChanged { .. }))
    );
}

#[tokio::test]
async fn media_evaluation_searches_without_grabbing_and_sorts_by_arr_verdict() {
    let arr = radarr(10);
    let rel = |guid: &str, approved: bool, cf: i64| ReleaseSearchResult {
        release: ReleaseRef {
            guid: guid.into(),
            indexer_id: 1,
        },
        indexer: "i".into(),
        title: guid.into(),
        size: 10,
        seeders: None,
        leechers: None,
        age_days: 1,
        protocol: "torrent".into(),
        quality: "Q".into(),
        custom_formats: vec![],
        cf_score: cf,
        approved,
        rejections: if approved { vec![] } else { vec!["no".into()] },
        download_allowed: true,
    };
    let fake = ArrFake {
        search: vec![rel("rel-b", false, 500), rel("rel-a", true, 1)],
        service: ArrService::Radarr,
        snapshot: Mutex::new(arr.snapshot.lock().unwrap().clone()),
        previews: BTreeMap::new(),
        reads: AtomicUsize::new(0),
    };
    let e = evaluate_media(&fake, &arr_media(ArrService::Radarr, 7), true)
        .await
        .unwrap();
    let order: Vec<_> = e
        .search
        .unwrap()
        .iter()
        .map(|r| r.release.guid.clone())
        .collect();
    assert_eq!(
        order,
        ["rel-a", "rel-b"],
        "ARR approval first, whatever the score"
    );
    assert!(
        evaluate_media(&fake, &arr_media(ArrService::Radarr, 7), false)
            .await
            .unwrap()
            .search
            .is_none()
    );
}

#[tokio::test]
async fn the_automatic_deletion_phase_plans_and_approves_only_by_policy() {
    use removarr_domain::plan::Approver;
    use removarr_domain::ports::{IgnoreStore, PlanStore};
    use removarr_workflows::plans::{AutoPhase, PlanError};
    let w = store_world().await;
    let o = movie_world();
    store_obs(&w, o.clone(), 7).await;
    let planner = store_planner(&w, live(), vec![radarr(10)]);
    let id = hardlink_id(OLD);
    // A1: paused or off refuses the whole phase.
    let mut paused = auto_on();
    paused.auto_paused = Some("previous failure".into());
    assert!(matches!(
        planner
            .create_auto(AutoPhase::Deletions, &paused, &[id.clone()], o.arr.clone())
            .await,
        Err(PlanError::AutoRefused(_))
    ));
    // Every rule holds: one plan, approved by policy and stored as approved.
    let out = planner
        .create_auto(
            AutoPhase::Deletions,
            &auto_on(),
            &[id.clone()],
            o.arr.clone(),
        )
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 1, "{:?}", out.refused);
    let p = &out.plans[0];
    assert_eq!(p.approval.by, Approver::AutoPolicy);
    let (_, hash, status, approval) = w
        .repos
        .plans
        .get(p.built.plan.plan().id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((status, &hash), (PlanStatus::Approved, p.built.plan.hash()));
    assert_eq!(approval.unwrap().by, Approver::AutoPolicy);
    // The phase-1 consolidation phase has nothing to do here.
    let none = planner
        .create_auto(
            AutoPhase::Consolidations,
            &auto_on(),
            &[id.clone()],
            o.arr.clone(),
        )
        .await
        .unwrap();
    assert!(none.plans.is_empty());
    // Ignored after classification (read from the store): never planned automatically.
    w.repos
        .ignores
        .ignore(&[SubjectKey::rooted(&rp(&completed(), OLD))], None, NOW)
        .await
        .unwrap();
    let out = planner
        .create_auto(AutoPhase::Deletions, &auto_on(), &[id], o.arr.clone())
        .await
        .unwrap();
    assert!(out.plans.is_empty(), "{:?}", out.refused);
}
