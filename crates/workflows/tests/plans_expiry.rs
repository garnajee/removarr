#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Expiry and invalidation triggers: time, configuration (mappings included), files, folders,
//! scan errors, torrent states, mapping faults and services. Plans never adapt.

mod common;
mod plans_common;

use std::collections::BTreeMap;

use common::*;
use plans_common::*;
use removarr_domain::fs::{IoKind, ScanErrorRecord};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::{ActionPlan, PlanStatus};
use removarr_domain::session::{ServiceKind, ServiceState};
use removarr_domain::time::MtimeNs;
use removarr_domain::torrent::{MappedPath, TorrentState};
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{IssueAction as A, PlanInput, StaleReason, staleness, verdict};

const MOVIE: &str = "Movie.A.2020.mkv";

fn world() -> Observation {
    let c = completed();
    let comp = Inv::new(c.clone())
        .file(MOVIE, 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            MOVIE,
            &[(MOVIE, MOVIE)],
        )],
    );
    obs(vec![comp], QbitObservation::Available(snap))
}

/// A plan deleting the torrent-held movie (captures its torrent state) and the orphan folder
/// (captures a folder snapshot), built from `world()`.
fn plans() -> (ActionPlan, ActionPlan) {
    let i = input(&world());
    let c = completed();
    let movie = id_of(
        &i,
        C::HardlinkMissing,
        &SubjectKey::rooted(&rp(&c, MOVIE)).0,
    );
    let folder = id_of(
        &i,
        C::OrphanCompleted,
        &SubjectKey::rooted(&rp(&c, "Orphan.Rel")).0,
    );
    let a = build(
        &i,
        &live(),
        &req(C::HardlinkMissing, vec![movie], A::Delete),
    )
    .unwrap();
    let b = build(
        &i,
        &live(),
        &req(C::OrphanCompleted, vec![folder], A::Delete),
    )
    .unwrap();
    (a.plan.plan().clone(), b.plan.plan().clone())
}

fn stale(plan: &ActionPlan, current: &PlanInput) -> Vec<StaleReason> {
    staleness(plan, NOW, current, &live(), &BTreeMap::new())
}

#[test]
fn an_unchanged_world_keeps_the_plan_and_time_alone_expires_it() {
    let (movie, folder) = plans();
    let i = input(&world());
    assert!(stale(&movie, &i).is_empty());
    assert!(stale(&folder, &i).is_empty());
    let late = staleness(&movie, movie.expires_at, &i, &live(), &BTreeMap::new());
    assert_eq!(
        late,
        [StaleReason::Expired {
            expires_at: movie.expires_at
        }]
    );
    assert_eq!(verdict(&late), Some(PlanStatus::Expired));
    assert_eq!(verdict(&[]), None);
}

#[test]
fn configuration_and_mapping_changes_invalidate() {
    let (movie, _) = plans();
    let i = input(&world());
    let mut s = live();
    s.config_fingerprint = "cfg-2".into();
    let r = staleness(&movie, NOW, &i, &s, &BTreeMap::new());
    assert!(r.contains(&StaleReason::ConfigChanged));
    assert_eq!(verdict(&r), Some(PlanStatus::Invalidated));
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        snap.torrents[0].content_path = MappedPath::NoRule {
            raw: "/elsewhere".into(),
        };
    }
    assert!(stale(&movie, &input(&o)).contains(&StaleReason::MappingChanged { hash: hash('a') }));
}

#[test]
fn file_folder_and_scan_error_changes_invalidate() {
    let (movie, folder) = plans();
    let c = completed();
    let mut o = world();
    o.inventories[0]
        .entries
        .iter_mut()
        .find(|e| e.path == rp(&c, MOVIE))
        .unwrap()
        .sig
        .mtime_ns = MtimeNs(1);
    assert!(
        stale(&movie, &input(&o)).contains(&StaleReason::EntryChanged {
            path: rp(&c, MOVIE)
        })
    );
    let mut o = world();
    o.inventories[0].entries.retain(|e| e.path != rp(&c, MOVIE));
    assert!(
        stale(&movie, &input(&o)).contains(&StaleReason::EntryMissing {
            path: rp(&c, MOVIE)
        })
    );
    let mut o = world();
    o.inventories[0] = Inv::new(c.clone())
        .file(MOVIE, 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .file("Orphan.Rel/extra.nfo", 21, 1)
        .build();
    assert!(
        stale(&folder, &input(&o)).contains(&StaleReason::DirChanged {
            dir: rp(&c, "Orphan.Rel")
        })
    );
    let mut o = world();
    o.inventories[0].errors.push(ScanErrorRecord {
        path: rp(&c, "Orphan.Rel/Orphan.Rel.mkv"),
        io: IoKind::PermissionDenied,
        message: "denied".into(),
    });
    assert!(
        stale(&folder, &input(&o)).contains(&StaleReason::ScanErrorUnder {
            path: rp(&c, "Orphan.Rel")
        })
    );
}

fn with_state(state: &str, progress: f64) -> PlanInput {
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        snap.torrents[0].state = TorrentState::parse(state);
        snap.torrents[0].progress = progress;
    }
    input(&o)
}

#[test]
fn torrents_keep_a_plan_valid_while_their_eligibility_class_and_progress_hold() {
    let (movie, _) = plans();
    // Seeding torrents flip between allowlisted states: no change (version 4 snapshot).
    for state in ["stalledUP", "queuedUP", "forcedUP", "pausedUP"] {
        assert!(stale(&movie, &with_state(state, 1.0)).is_empty(), "{state}");
    }
    // Any move to a protected or unknown state, or a progress change, invalidates.
    for state in [
        "pausedDL",
        "error",
        "missingFiles",
        "checkingUP",
        "stoppedUP",
        "somethingNew",
    ] {
        let r = stale(&movie, &with_state(state, 1.0));
        assert!(
            r.contains(&StaleReason::TorrentChanged { hash: hash('a') }),
            "{state}: {r:?}"
        );
        assert_eq!(verdict(&r), Some(PlanStatus::Invalidated));
    }
    assert!(
        stale(&movie, &with_state("uploading", 0.99))
            .contains(&StaleReason::TorrentChanged { hash: hash('a') })
    );
}

#[test]
fn torrent_and_service_changes_invalidate() {
    let (movie, folder) = plans();
    let c = completed();
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        snap.torrents[0].state = TorrentState::parse("checkingUP");
    }
    assert!(stale(&movie, &input(&o)).contains(&StaleReason::TorrentChanged { hash: hash('a') }));
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        snap.torrents.clear();
        snap.files.clear();
    }
    assert!(stale(&movie, &input(&o)).contains(&StaleReason::TorrentMissing { hash: hash('a') }));
    // A torrent now references the orphan folder.
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        let t = Tor::new(
            'b',
            &c,
            "uploading",
            1.0,
            "Orphan.Rel",
            &[("Orphan.Rel/Orphan.Rel.mkv", "Orphan.Rel/Orphan.Rel.mkv")],
        );
        snap.files.insert(t.info.hash.clone(), t.files);
        snap.torrents.push(t.info);
    }
    assert!(
        stale(&folder, &input(&o)).contains(&StaleReason::TorrentReferences {
            path: rp(&c, "Orphan.Rel")
        })
    );
    // qBittorrent down in the latest session.
    let mut i = input(&world());
    i.session.services[0].state = ServiceState::Unavailable {
        reason: "down".into(),
    };
    assert!(stale(&movie, &i).contains(&StaleReason::ServiceNotOk {
        service: ServiceKind::Qbittorrent
    }));
    // A session that cannot plan invalidates everything.
    let mut i = input(&world());
    i.session.status = removarr_domain::session::SessionStatus::Failed;
    assert!(stale(&movie, &i).contains(&StaleReason::SessionNotPlannable));
}
