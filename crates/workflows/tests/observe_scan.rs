#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The observe workflow with in-memory ports: scan session, fail-closed outcomes, and refresh
//! with zero filesystem calls (architecture 7.1, CP1-F07).

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::fakes::{FakeArr, FakeContent, FakeFs, FakeQbit, Mem};
use common::*;
use removarr_domain::arr::ArrService;
use removarr_domain::fs::ProtectedReason;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C};
use removarr_domain::ports::ScanControl;
use removarr_domain::session::{ServiceKind, ServiceState, SessionStatus};
use removarr_testkit::clock::FakeClock;
use removarr_testkit::ids::SeqIds;
use removarr_workflows::issues::{ClassifyError, IssueSettings, QbitObservation};
use removarr_workflows::observe::{
    ObserveConfig, ObserveError, ObservePorts, RefreshTarget, ServiceCache, parse_major, refresh,
    run_scan,
};

struct H {
    ports: ObservePorts,
    cfg: ObserveConfig,
    mem: Arc<Mem>,
    fs: Arc<FakeFs>,
    content: Arc<FakeContent>,
    qbit: Option<Arc<FakeQbit>>,
}

fn world() -> (
    Vec<removarr_domain::fs::RootInventory>,
    removarr_domain::torrent::TorrentSnapshot,
) {
    let c = Inv::new(completed())
        .file("Bravo.mkv", 20, 81_920)
        .file("Echo.mkv", 40, 32_768)
        .file("stray/readme.txt", 41, 10)
        .build();
    let m = Inv::new(media())
        .dir("movies", 100)
        .file("movies/Bravo.mkv", 21, 81_920)
        .build();
    let i = Inv::new(incomplete()).file("abandoned.mkv", 50, 10).build();
    let t = Tor::new(
        'b',
        &completed(),
        "stalledUP",
        1.0,
        "Bravo.mkv",
        &[("Bravo.mkv", "Bravo.mkv")],
    );
    (vec![c, m, i], snapshot(5, vec![t]))
}

fn harness(
    invs: Vec<removarr_domain::fs::RootInventory>,
    snap: Option<removarr_domain::torrent::TorrentSnapshot>,
    configured_qbit: bool,
    arr: Vec<Arc<FakeArr>>,
) -> H {
    let mem = Arc::new(Mem::default());
    let fs = FakeFs::with(invs);
    let content = Arc::new(FakeContent::default());
    let qbit = configured_qbit.then(|| FakeQbit::new(snap));
    let mut settings = IssueSettings::new(OWNER);
    settings.video_extensions = [".mkv".to_string()].into_iter().collect();
    let cfg = ObserveConfig {
        roots: vec![completed(), media(), incomplete()],
        settings,
        match_settings: removarr_matching::MatchSettings {
            video_extensions: [".mkv".to_string()].into_iter().collect(),
            ..Default::default()
        },
        compare_chunk_bytes: 65_536,
        content_read_concurrency: 1,
        http_concurrency: 2,
    };
    let ports = ObservePorts {
        fs: fs.clone(),
        content: content.clone(),
        qbit: qbit.clone().map(|q| q as _),
        arr: arr.into_iter().map(|a| a as _).collect(),
        jellyfin: None,
        scans: mem.clone(),
        snapshots: mem.clone(),
        matches: mem.clone(),
        issues: mem.clone(),
        ignores: mem.clone(),
        clock: Arc::new(FakeClock::new(NOW)),
        ids: Arc::new(SeqIds::new()),
    };
    H {
        ports,
        cfg,
        mem,
        fs,
        content,
        qbit,
    }
}

fn std_harness() -> H {
    let (invs, snap) = world();
    harness(invs, Some(snap), true, vec![])
}

#[tokio::test]
async fn a_scan_walks_roots_in_order_persists_everything_and_completes() {
    let h = std_harness();
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(r.session.status, SessionStatus::Complete);
    assert_eq!(
        *h.fs.scans.lock().unwrap(),
        [completed(), media(), incomplete()],
        "sequential, in configured order"
    );
    assert_eq!(h.mem.inventories.lock().unwrap().len(), 3);
    assert!(h.mem.torrents.lock().unwrap().is_some());
    assert!(
        !h.mem.matches.lock().unwrap().is_empty(),
        "funnel output is persisted"
    );
    let classified = r.classified.unwrap();
    assert_eq!(
        *h.mem.issues.lock().unwrap(),
        classified.issues,
        "the issue set is persisted"
    );
    assert_eq!(h.mem.replace_calls.load(Ordering::SeqCst), 1);
    one(&classified, C::HardlinkMissing, "p:completed:Bravo.mkv");
    one(&classified, C::OrphanCompleted, "p:completed:Echo.mkv");
    one(&classified, C::Junk, "p:completed:stray/readme.txt");
    one(
        &classified,
        C::IncompleteOrphan,
        "p:incomplete:abandoned.mkv",
    );
    // The persisted session is finished with a finish time.
    let s = h.mem.sessions.lock().unwrap();
    assert_eq!(s.len(), 1);
    assert!(s[0].finished_at.is_some() && s[0].status == SessionStatus::Complete);
    assert_eq!(
        h.content.0.load(Ordering::SeqCst),
        0,
        "name+size candidates need no content read"
    );
}

#[tokio::test]
async fn a_cancelled_scan_produces_no_issues() {
    let h = std_harness();
    let ctl = ScanControl::default();
    ctl.cancel.cancel();
    let r = run_scan(&h.ports, &h.cfg, &ctl).await.unwrap();
    assert_eq!(r.session.status, SessionStatus::Cancelled);
    assert!(r.classified.is_none());
    assert_eq!(h.mem.replace_calls.load(Ordering::SeqCst), 0);
    assert!(h.mem.issues.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_root_that_stops_early_cancels_the_session_and_classifies_nothing() {
    let (mut invs, snap) = world();
    invs[1].finished = false;
    let h = harness(invs, Some(snap), true, vec![]);
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(r.session.status, SessionStatus::Cancelled);
    assert!(r.classified.is_none());
    assert_eq!(
        h.fs.scans.lock().unwrap().len(),
        2,
        "the walk stops at the unfinished root"
    );
    assert_eq!(h.mem.replace_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_root_that_cannot_be_opened_fails_the_session() {
    let h = std_harness();
    h.fs.fail_roots.lock().unwrap().insert(media());
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(r.session.status, SessionStatus::Failed);
    assert!(r.classified.is_none());
    assert_eq!(h.mem.replace_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_qbittorrent_outage_does_not_abort_the_scan_but_blocks_what_depends_on_it() {
    let (invs, _) = world();
    let h = harness(invs, None, true, vec![]); // configured, unreachable
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(r.session.status, SessionStatus::CompleteWithErrors);
    let c = r.classified.unwrap();
    for (cat, s) in [
        (C::OrphanCompleted, "p:completed:Echo.mkv"),
        (C::Junk, "p:completed:stray/readme.txt"),
        (C::IncompleteOrphan, "p:incomplete:abandoned.mkv"),
    ] {
        assert!(has(
            one(&c, cat, s),
            &B::ServiceUnavailable(ServiceKind::Qbittorrent)
        ));
    }
    assert!(matches!(
        r.services.qbit,
        Some(QbitObservation::Unavailable { .. })
    ));
    assert!(
        r.session
            .services
            .iter()
            .any(|s| s.service == ServiceKind::Qbittorrent
                && matches!(s.state, ServiceState::Unavailable { .. }))
    );
}

#[tokio::test]
async fn qbittorrent_not_configured_is_not_a_degraded_session_but_still_blocks() {
    let (invs, _) = world();
    let h = harness(invs, None, false, vec![]);
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(r.session.status, SessionStatus::Complete);
    let c = r.classified.unwrap();
    assert!(has(
        one(&c, C::OrphanCompleted, "p:completed:Echo.mkv"),
        &B::ServiceNotConfigured(ServiceKind::Qbittorrent)
    ));
}

#[tokio::test]
async fn scan_errors_and_unreadable_zones_degrade_the_session_but_qbit_internals_do_not() {
    let (mut invs, snap) = world();
    invs[0] = Inv {
        inv: invs[0].clone(),
    }
    .zone("BT_backup", ProtectedReason::QbitInternal)
    .build();
    let h = harness(invs.clone(), Some(snap.clone()), true, vec![]);
    assert_eq!(
        run_scan(&h.ports, &h.cfg, &ScanControl::default())
            .await
            .unwrap()
            .session
            .status,
        SessionStatus::Complete
    );
    for degrade in [
        Inv {
            inv: invs[0].clone(),
        }
        .error("x")
        .build(),
        Inv {
            inv: invs[0].clone(),
        }
        .zone("m", ProtectedReason::MountBoundary)
        .build(),
    ] {
        let mut invs2 = invs.clone();
        invs2[0] = degrade;
        let h = harness(invs2, Some(snap.clone()), true, vec![]);
        let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
            .await
            .unwrap();
        assert_eq!(r.session.status, SessionStatus::CompleteWithErrors);
        assert!(
            r.classified.is_some(),
            "a degraded session still lists, qualified"
        );
    }
}

#[tokio::test]
async fn arr_state_is_read_without_quality_calls_and_an_outage_is_recorded() {
    let (invs, snap) = world();
    let up = Arc::new(FakeArr {
        service: ArrService::Radarr,
        up: true,
        library: vec![library_entry(
            ArrService::Radarr,
            9,
            rp(&media(), "movies/Bravo.mkv"),
        )],
        queue: vec![queue_item(
            5,
            'b',
            9,
            ArrService::Radarr,
            "warning",
            "importBlocked",
        )],
        calls: Default::default(),
    });
    let down = Arc::new(FakeArr {
        service: ArrService::Sonarr,
        up: false,
        library: vec![],
        queue: vec![],
        calls: Default::default(),
    });
    let h = harness(invs, Some(snap), true, vec![up, down]);
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    assert_eq!(
        r.session.status,
        SessionStatus::CompleteWithErrors,
        "Sonarr is configured and down"
    );
    let c = r.classified.unwrap();
    one(&c, C::ArrQueue, "q:radarr:5");
    let obs = r.services.arr.unwrap();
    assert!(
        obs.iter()
            .any(|a| a.service == ArrService::Sonarr && !a.is_ok())
    );
}

#[tokio::test]
async fn ignored_subjects_are_marked_by_a_scan_and_restored_by_a_refresh() {
    let h = std_harness();
    h.ports
        .ignores
        .ignore(
            &[SubjectKey("p:completed:Echo.mkv".into())],
            Some("keep"),
            NOW,
        )
        .await
        .unwrap();
    let mut r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    let c = r.classified.take().unwrap();
    assert!(
        c.ignored()
            .any(|i| i.subject.as_str() == "p:completed:Echo.mkv")
    );
    assert!(
        c.active()
            .all(|i| i.subject.as_str() != "p:completed:Echo.mkv")
    );
    // Restore, then refresh: the issues come back, from stored data only.
    h.ports
        .ignores
        .restore(&[SubjectKey("p:completed:Echo.mkv".into())])
        .await
        .unwrap();
    let mut cache = r.services;
    let c2 = refresh(
        &h.ports,
        &h.cfg,
        RefreshTarget::ClassificationOnly,
        &mut cache,
    )
    .await
    .unwrap();
    assert!(
        c2.active()
            .any(|i| i.subject.as_str() == "p:completed:Echo.mkv")
    );
    assert_eq!(c2.ignored().count(), 0);
}

// ------------------------------------------------------------------ refresh

#[tokio::test]
async fn refresh_makes_zero_filesystem_calls_and_zero_content_reads() {
    let h = std_harness();
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    let scans_before = h.fs.scans.lock().unwrap().len();
    let content_before = h.content.0.load(Ordering::SeqCst);
    let mut cache = r.services;
    for target in [
        RefreshTarget::ClassificationOnly,
        RefreshTarget::Qbittorrent,
        RefreshTarget::Arr,
        RefreshTarget::Jellyfin,
        RefreshTarget::All,
    ] {
        let c = refresh(&h.ports, &h.cfg, target, &mut cache).await.unwrap();
        one(&c, C::OrphanCompleted, "p:completed:Echo.mkv");
    }
    assert_eq!(
        h.fs.scans.lock().unwrap().len(),
        scans_before,
        "no root is walked again"
    );
    assert_eq!(
        h.fs.other_calls.load(Ordering::SeqCst),
        0,
        "no stat, snapshot, free-space or permission call"
    );
    assert_eq!(
        h.content.0.load(Ordering::SeqCst),
        content_before,
        "no content read"
    );
    assert!(
        h.qbit.as_ref().unwrap().calls.load(Ordering::SeqCst) > 1,
        "qBittorrent was re-read through its API"
    );
}

#[tokio::test]
async fn refresh_after_a_restart_reads_the_missing_services_through_their_apis_only() {
    let h = std_harness();
    run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    let before = h.fs.total_calls();
    let mut empty = ServiceCache::default();
    let c = refresh(
        &h.ports,
        &h.cfg,
        RefreshTarget::ClassificationOnly,
        &mut empty,
    )
    .await
    .unwrap();
    assert!(empty.qbit.is_some() && empty.arr.is_some() && empty.jellyfin.is_some());
    assert_eq!(h.fs.total_calls(), before);
    assert_eq!(*h.mem.issues.lock().unwrap(), c.issues);
}

#[tokio::test]
async fn refresh_picks_up_a_changed_qbittorrent_state_without_rescanning() {
    let h = std_harness();
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    let c = r.classified.unwrap();
    assert!(one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv").is_unblocked());
    // The torrent starts checking: protected on the next refresh.
    let t = Tor::new(
        'b',
        &completed(),
        "checkingUP",
        1.0,
        "Bravo.mkv",
        &[("Bravo.mkv", "Bravo.mkv")],
    );
    *h.qbit.as_ref().unwrap().snapshot.lock().unwrap() = Some(snapshot(5, vec![t]));
    let mut cache = r.services;
    let c2 = refresh(&h.ports, &h.cfg, RefreshTarget::Qbittorrent, &mut cache)
        .await
        .unwrap();
    assert!(has(
        one(&c2, C::HardlinkMissing, "p:completed:Bravo.mkv"),
        &B::TorrentProtectedState
    ));
    assert_eq!(
        h.fs.scans.lock().unwrap().len(),
        3,
        "still the three scans of the first run"
    );
}

#[tokio::test]
async fn refresh_needs_a_finished_session() {
    let h = std_harness();
    let mut cache = ServiceCache::default();
    assert!(matches!(
        refresh(&h.ports, &h.cfg, RefreshTarget::All, &mut cache).await,
        Err(ObserveError::NoSession)
    ));
    let ctl = ScanControl::default();
    ctl.cancel.cancel();
    run_scan(&h.ports, &h.cfg, &ctl).await.unwrap();
    let e = refresh(&h.ports, &h.cfg, RefreshTarget::All, &mut cache)
        .await
        .unwrap_err();
    assert!(matches!(
        e,
        ObserveError::Classify(ClassifyError::SessionNotClassifiable(
            SessionStatus::Cancelled
        ))
    ));
}

#[tokio::test]
async fn refresh_treats_a_root_without_a_stored_inventory_as_incomplete() {
    let h = std_harness();
    let r = run_scan(&h.ports, &h.cfg, &ScanControl::default())
        .await
        .unwrap();
    h.mem.inventories.lock().unwrap().remove(&media());
    let mut cache = r.services;
    let c = refresh(
        &h.ports,
        &h.cfg,
        RefreshTarget::ClassificationOnly,
        &mut cache,
    )
    .await
    .unwrap();
    assert!(has(
        one(&c, C::HardlinkMissing, "p:completed:Echo.mkv"),
        &B::ScanIncomplete
    ));
}

#[test]
fn qbittorrent_major_versions_parse_and_unknown_is_zero() {
    assert_eq!(parse_major("v5.0.4"), 5);
    assert_eq!(parse_major("4.6.7"), 4);
    assert_eq!(parse_major("V4.4.0"), 4);
    assert_eq!(parse_major(""), 0);
    assert_eq!(parse_major("garbage"), 0);
}
