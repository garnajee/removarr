#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Every fail-closed condition (MASTER 2, OQ-05, OQ-07, CP1-F10, CP1-F19).

mod common;

use common::*;
use removarr_domain::evidence::ContentProof;
use removarr_domain::fs::ProtectedReason;
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C};
use removarr_domain::session::{ServiceKind, SessionStatus};
use removarr_domain::torrent::MappedPath;
use removarr_workflows::issues::{ClassifyError, QbitObservation, QbitPerimeter, classify};

fn qb(snapshot: removarr_domain::torrent::TorrentSnapshot) -> QbitObservation {
    QbitObservation::Available(snapshot)
}

#[test]
fn a_session_that_did_not_finish_produces_no_issues() {
    for st in [
        SessionStatus::Running,
        SessionStatus::Cancelled,
        SessionStatus::Interrupted,
        SessionStatus::Failed,
    ] {
        let mut o = obs(
            vec![Inv::new(completed()).file("a.mkv", 1, 1).build()],
            qb(snapshot(5, vec![])),
        );
        o.session_status = st;
        assert_eq!(
            classify(&o, &settings()).unwrap_err(),
            ClassifyError::SessionNotClassifiable(st)
        );
    }
    // A finished session with errors still classifies, qualified.
    let mut o = obs(vec![Inv::new(completed()).build()], qb(snapshot(5, vec![])));
    o.session_status = SessionStatus::CompleteWithErrors;
    assert!(classify(&o, &settings()).is_ok());
}

#[test]
fn an_unfinished_root_blocks_everything_in_it_and_every_absence_claim() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 1, 4096)
        .file("stray/x.txt", 2, 10)
        .unfinished()
        .build();
    let m = Inv::new(media()).file("Lib.mkv", 3, 4096).build();
    let r = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    for i in r
        .issues
        .iter()
        .filter(|i| ["hardlink_missing", "orphan_completed", "junk"].contains(&i.category.as_str()))
    {
        assert!(
            has(i, &B::ScanIncomplete),
            "{} {}",
            i.category.as_str(),
            i.subject.as_str()
        );
        assert!(!i.is_unblocked());
    }
    // "No source in completed" cannot be claimed when completed is incomplete.
    assert!(has(
        one(&r, C::MediaWithoutSource, &format!("p:{}:Lib.mkv", media())),
        &B::ScanIncomplete
    ));
}

#[test]
fn a_configured_root_without_an_inventory_is_incomplete() {
    let c = Inv::new(completed()).file("Echo.mkv", 1, 4096).build();
    let mut o = obs(vec![c], qb(snapshot(5, vec![])));
    o.configured_roots = vec![completed(), media()];
    let r = run(&o);
    assert!(
        has(
            one(&r, C::HardlinkMissing, "p:completed:Echo.mkv"),
            &B::ScanIncomplete
        ),
        "the media tree was never scanned"
    );
    let p = &r.perimeter;
    assert!(!p.root(&media()).unwrap().scanned);
    assert!(!p.auto_blockers(&[media()]).is_empty());
}

#[test]
fn a_scan_error_blocks_the_targets_around_it_only() {
    let c = Inv::new(completed())
        .dir("bad", 1)
        .file("bad/Echo.mkv", 2, 4096)
        .file("good/Foxtrot.mkv", 3, 4096)
        .error("bad/locked")
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let bad = one(&r, C::OrphanCompleted, "p:completed:bad");
    assert!(
        has(bad, &B::ScanErrorUnder),
        "an error below the unit blocks it"
    );
    assert!(
        !has(
            one(&r, C::HardlinkMissing, "p:completed:bad/Echo.mkv"),
            &B::ScanErrorUnder
        ),
        "a sibling subtree error does not touch a single file"
    );
    let good = one(&r, C::OrphanCompleted, "p:completed:good");
    assert!(
        !has(good, &B::ScanErrorUnder),
        "an error elsewhere does not block this target in manual mode"
    );
    // The whole-root rule for automatic mode does apply.
    let auto = r.perimeter.auto_blockers(&[completed()]);
    assert!(auto.iter().any(|b| b.code == B::ScanErrorUnder));
}

#[test]
fn an_error_on_the_directory_of_a_target_blocks_it() {
    let c = Inv::new(completed())
        .file("bad/Echo.mkv", 2, 4096)
        .error("bad")
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    assert!(has(
        one(&r, C::OrphanCompleted, "p:completed:bad"),
        &B::ScanErrorUnder
    ));
    assert!(has(
        one(&r, C::HardlinkMissing, "p:completed:bad/Echo.mkv"),
        &B::ScanErrorUnder
    ));
}

#[test]
fn an_error_in_the_media_tree_blocks_the_hardlink_absence_claim() {
    let c = Inv::new(completed()).file("Echo.mkv", 2, 4096).build();
    let m = Inv::new(media()).error("somewhere").build();
    let r = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    assert!(has(
        one(&r, C::HardlinkMissing, "p:completed:Echo.mkv"),
        &B::ScanErrorUnder
    ));
}

#[test]
fn protected_zones_block_their_unit_and_qbit_internals_do_not_degrade_the_root() {
    let c = Inv::new(completed())
        .dir("Rel", 1)
        .file("Rel/Rel.mkv", 2, 4096)
        .file("Other/Other.mkv", 3, 4096)
        .zone("Rel/BT_backup", ProtectedReason::QbitInternal)
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    assert!(has(
        one(&r, C::OrphanCompleted, "p:completed:Rel"),
        &B::ProtectedZone
    ));
    assert!(!has(
        one(&r, C::OrphanCompleted, "p:completed:Other"),
        &B::ProtectedZone
    ));
    let p = r.perimeter.root(&completed()).unwrap();
    assert_eq!((p.qbit_internal_zones, p.degrading_zones), (1, 0));
    assert!(
        p.auto_clean(),
        "A2: QbitInternal zones do not count for the root"
    );
}

#[test]
fn mount_boundaries_and_non_utf8_zones_degrade_the_root_for_automation() {
    for reason in [ProtectedReason::MountBoundary, ProtectedReason::NonUtf8Name] {
        let c = Inv::new(completed())
            .file("a/Echo.mkv", 2, 4096)
            .zone("z", reason)
            .build();
        let r = run(&obs(
            vec![c, Inv::new(media()).build()],
            qb(snapshot(5, vec![])),
        ));
        let p = r.perimeter.root(&completed()).unwrap();
        assert!(!p.auto_clean());
        assert!(
            r.perimeter
                .auto_blockers(&[completed()])
                .iter()
                .any(|b| b.code == B::ProtectedZone)
        );
    }
}

#[test]
fn unmapped_torrents_block_every_torrentless_deletion_and_automation() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 1, 4096)
        .file("stray/readme.txt", 2, 10)
        .build();
    let i = Inv::new(incomplete()).file("abandoned.mkv", 3, 10).build();
    let no_rule = Tor::new(
        '1',
        &completed(),
        "uploading",
        1.0,
        "X.mkv",
        &[("X.mkv", "X.mkv")],
    )
    .raw_content(MappedPath::NoRule {
        raw: "/srv/other/X.mkv".into(),
    });
    let r = run(&obs(
        vec![c.clone(), i.clone(), Inv::new(media()).build()],
        qb(snapshot(5, vec![no_rule])),
    ));
    for (cat, s) in [
        (C::OrphanCompleted, "p:completed:Echo.mkv"),
        (C::Junk, "p:completed:stray/readme.txt"),
        (C::IncompleteOrphan, "p:incomplete:abandoned.mkv"),
    ] {
        assert!(has(one(&r, cat, s), &B::UnmappedTorrents), "{s}");
    }
    assert_eq!(
        r.perimeter.qbit,
        QbitPerimeter::Available {
            no_rule: 1,
            invalid: 0
        }
    );
    assert!(
        r.perimeter
            .auto_blockers(&[completed()])
            .iter()
            .any(|b| b.code == B::UnmappedTorrents)
    );

    // An escaping prefix is a mapping error (CON-07, V4).
    let invalid = Tor::new(
        '2',
        &completed(),
        "uploading",
        1.0,
        "Y.mkv",
        &[("Y.mkv", "Y.mkv")],
    )
    .raw_content(MappedPath::Invalid {
        raw: "/data/../../etc".into(),
        reason: "escape".into(),
    });
    let r2 = run(&obs(
        vec![c, i, Inv::new(media()).build()],
        qb(snapshot(5, vec![invalid])),
    ));
    assert!(has(
        one(&r2, C::OrphanCompleted, "p:completed:Echo.mkv"),
        &B::MappingError
    ));
}

#[test]
fn declared_external_torrents_do_not_block_anything() {
    let c = Inv::new(completed()).file("Echo.mkv", 1, 4096).build();
    let ext = Tor::new(
        '1',
        &completed(),
        "uploading",
        1.0,
        "X.mkv",
        &[("X.mkv", "X.mkv")],
    )
    .raw_content(MappedPath::DeclaredExternal {
        raw: "/ext/X.mkv".into(),
    });
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![ext])),
    ));
    assert!(one(&r, C::OrphanCompleted, "p:completed:Echo.mkv").is_unblocked());
}

#[test]
fn qbittorrent_not_configured_or_unavailable_blocks_everything_needing_ownership() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 1, 4096)
        .file("stray/readme.txt", 2, 10)
        .build();
    let i = Inv::new(incomplete()).file("abandoned.mkv", 3, 10).build();
    for (q, expected) in [
        (
            QbitObservation::NotConfigured,
            B::ServiceNotConfigured(ServiceKind::Qbittorrent),
        ),
        (
            QbitObservation::Unavailable {
                reason: "down".into(),
            },
            B::ServiceUnavailable(ServiceKind::Qbittorrent),
        ),
    ] {
        let r = run(&obs(
            vec![c.clone(), i.clone(), Inv::new(media()).build()],
            q,
        ));
        for (cat, s) in [
            (C::OrphanCompleted, "p:completed:Echo.mkv"),
            (C::HardlinkMissing, "p:completed:Echo.mkv"),
            (C::Junk, "p:completed:stray/readme.txt"),
            (C::IncompleteOrphan, "p:incomplete:abandoned.mkv"),
        ] {
            let issue = one(&r, cat, s);
            assert!(has(issue, &expected), "{cat:?} {s}");
            assert!(!issue.is_unblocked());
        }
        assert!(
            of(&r, C::IncompleteStalled).is_empty() && of(&r, C::TorrentProtected).is_empty(),
            "nothing is invented without torrents"
        );
        assert!(
            r.perimeter
                .auto_blockers(&[completed()])
                .iter()
                .any(|b| b.code == expected)
        );
    }
}

#[test]
fn unknown_state_unknown_major_and_wrong_version_names_are_protected() {
    let mk = |h: char, st: &str| {
        Tor::new(
            h,
            &completed(),
            st,
            1.0,
            &format!("{h}.mkv"),
            &[(&format!("{h}.mkv"), &format!("{h}.mkv"))],
        )
    };
    let build = |major: u8, st: &str| {
        let c = Inv::new(completed()).file("a.mkv", 1, 4096).build();
        let mut o = obs(
            vec![c, Inv::new(media()).build()],
            qb(snapshot(major, vec![mk('a', st)])),
        );
        let m = Inv::new(media()).file("m/a.mkv", 2, 4096).build();
        let ca = Inv::new(completed()).file("a.mkv", 1, 4096).build();
        o.matches = vec![candidate(
            rp(&completed(), "a.mkv"),
            rp(&media(), "m/a.mkv"),
            ca.entries[0].sig,
            m.entries[0].sig,
            ContentProof::NameSize,
        )];
        o.inventories = vec![ca, m];
        run(&o)
    };
    let blocked = |major, st: &str| {
        has(
            one(&build(major, st), C::HardlinkMissing, "p:completed:a.mkv"),
            &B::TorrentProtectedState,
        )
    };
    assert!(!blocked(5, "uploading") && !blocked(4, "uploading"));
    assert!(!blocked(4, "pausedUP") && blocked(5, "pausedUP"));
    assert!(!blocked(5, "stoppedUP") && blocked(4, "stoppedUP"));
    assert!(blocked(0, "uploading"), "unknown major version");
    assert!(blocked(5, "mysteryState"), "unknown state");
    for st in [
        "downloading",
        "metaDL",
        "forcedDL",
        "queuedDL",
        "stalledDL",
        "checkingDL",
        "checkingUP",
        "checkingResumeData",
        "allocating",
        "moving",
        "pausedDL",
        "stoppedDL",
        "error",
        "missingFiles",
    ] {
        assert!(blocked(5, st), "{st}");
    }
    for st in ["stalledUP", "queuedUP", "forcedUP"] {
        assert!(!blocked(5, st), "{st}");
    }
}

#[test]
fn a_torrent_at_progress_below_one_is_protected_even_in_an_allowlisted_state() {
    let c = Inv::new(completed()).file("a.mkv", 1, 4096).build();
    let t = Tor::new(
        'a',
        &completed(),
        "uploading",
        0.99,
        "a.mkv",
        &[("a.mkv", "a.mkv")],
    );
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![t])),
    ));
    assert!(has(
        one(&r, C::HardlinkMissing, "p:completed:a.mkv"),
        &B::TorrentProtectedState
    ));
}

#[test]
fn a_torrent_without_a_file_list_owns_its_whole_content_directory() {
    let c = Inv::new(completed())
        .file("R/a.mkv", 1, 4096)
        .file("R/readme.txt", 2, 10)
        .build();
    let mut t = Tor::new('a', &completed(), "uploading", 1.0, "R", &[]);
    t.files.clear();
    let snap = snapshot(5, vec![t]);
    assert!(snap.files.get(&hash('a')).unwrap().is_empty());
    let r = run(&obs(vec![c, Inv::new(media()).build()], qb(snap)));
    assert!(
        of(&r, C::OrphanCompleted).is_empty(),
        "fail-closed: it may hold anything below"
    );
    assert!(of(&r, C::Junk).is_empty());
}

#[test]
fn arr_state_blocks_identity_dependent_decisions() {
    use removarr_domain::session::ServiceState;
    let c = Inv::new(completed()).file("D.mkv", 5, 40_960).build();
    let m = Inv::new(media()).file("D2.mkv", 6, 81_920).build();
    let (sa, sb) = (c.entries[0].sig, m.entries[0].sig);
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![])));
    o.matches = vec![candidate(
        rp(&completed(), "D.mkv"),
        rp(&media(), "D2.mkv"),
        sa,
        sb,
        ContentProof::None,
    )];
    for (state, code) in [
        (
            ServiceState::AuthRejected,
            B::ServiceUnavailable(ServiceKind::Radarr),
        ),
        (
            ServiceState::NotConfigured,
            B::ServiceNotConfigured(ServiceKind::Radarr),
        ),
        (
            ServiceState::Unavailable { reason: "x".into() },
            B::ServiceUnavailable(ServiceKind::Radarr),
        ),
    ] {
        let mut a = arr_ok(removarr_domain::arr::ArrService::Radarr, vec![], vec![]);
        a.state = state;
        o.arr = vec![a];
        let r = run(&o);
        let i = one(&r, C::HardlinkMissing, "p:completed:D.mkv");
        assert!(has(i, &code) && has(i, &B::ArrMissing));
    }
}

#[test]
fn files_under_media_roots_are_never_deletion_candidates() {
    // Everything that can be deleted lives under completed or incomplete.
    let m = Inv::new(media())
        .dir("d", 1)
        .file("d/readme.txt", 2, 10)
        .file("d/a.rar", 3, 10)
        .file("d/orphan.srt", 4, 10)
        .file("d/x.mkv", 5, 10)
        .file("d/sample.mkv", 6, 10)
        .build();
    let r = run(&obs(
        vec![
            Inv::new(completed()).build(),
            Inv::new(incomplete()).build(),
            m,
        ],
        qb(snapshot(5, vec![])),
    ));
    for i in &r.issues {
        let under_media = match &i.unit {
            removarr_domain::issue::Unit::File(p) => {
                p.root.kind() == removarr_domain::path::RootKind::Media
            }
            _ => false,
        };
        if under_media {
            assert!(
                matches!(
                    i.category,
                    C::MediaWithoutSource | C::Permission | C::Symlink
                ),
                "{:?} must not exist for a media file",
                i.category
            );
            if i.category == C::MediaWithoutSource {
                assert!(has(i, &B::MediaRootDeletion));
            }
        }
    }
    assert!(of(&r, C::Junk).is_empty() && of(&r, C::OrphanCompleted).is_empty());
}
