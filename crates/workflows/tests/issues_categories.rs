#![allow(clippy::unwrap_used, clippy::expect_used)]
//! One test group per issue category (MASTER 5.1 to 5.7 plus the ARR queue).

mod common;

use common::*;
use removarr_domain::arr::ArrService;
use removarr_domain::evidence::{ContentProof, EvidenceKind};
use removarr_domain::fs::PhysicalId;
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C, Outcome, Severity, Unit};
use removarr_domain::session::ServiceKind;
use removarr_workflows::issues::{QbitObservation, evidence_level};

fn qb(snapshot: removarr_domain::torrent::TorrentSnapshot) -> QbitObservation {
    QbitObservation::Available(snapshot)
}

// ---------------------------------------------------------------- hardlink_missing

/// Bravo: a completed copy and a library copy on different inodes, one candidate pair.
fn bravo(content: ContentProof) -> removarr_workflows::issues::Observation {
    let c = Inv::new(completed()).file("Bravo.mkv", 20, 81_920).build();
    let m = Inv::new(media())
        .dir("movies", 100)
        .file("movies/Bravo.mkv", 21, 81_920)
        .build();
    let (sa, sb) = (c.entries[0].sig, m.entries[1].sig);
    let t = Tor::new(
        'b',
        &completed(),
        "stalledUP",
        1.0,
        "Bravo.mkv",
        &[("Bravo.mkv", "Bravo.mkv")],
    );
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![t])));
    o.matches = vec![candidate(
        rp(&completed(), "Bravo.mkv"),
        rp(&media(), "movies/Bravo.mkv"),
        sa,
        sb,
        content,
    )];
    o
}

#[test]
fn hardlink_missing_candidate_is_consolidate_with_candidate_level_evidence() {
    let c = run(&bravo(ContentProof::NameSize));
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(i.outcome, Outcome::Consolidate);
    assert_eq!(i.severity, Severity::Warning);
    assert!(i.is_unblocked(), "{:?}", i.blockers);
    assert_eq!(
        evidence_level(i),
        removarr_domain::evidence::EvidenceLevel::NameSize
    );
    assert_eq!(
        i.torrent_hashes.len(),
        1,
        "one content row with its torrent count"
    );
    assert_eq!(i.logical_bytes, 81_920);
    assert_eq!(i.potential_bytes, 81_920, "the library copy would be freed");
    assert!(i.physical_ids.contains(&PhysicalId { dev: 1, ino: 21 }));
    assert_eq!(i.age_basis.age_days(NOW), 3);
    assert!(matches!(i.unit, Unit::File(_)));
}

#[test]
fn only_a_full_comparison_makes_the_content_proven() {
    let c = run(&bravo(ContentProof::Identical { compared_at: NOW }));
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(
        evidence_level(i),
        removarr_domain::evidence::EvidenceLevel::Full
    );
    assert!(i.evidence.iter().any(|e| matches!(
        &e.kind,
        EvidenceKind::Content {
            proof: ContentProof::Identical { .. },
            ..
        }
    )));
}

#[test]
fn a_full_comparison_that_found_a_difference_blocks_consolidation() {
    let c = run(&bravo(ContentProof::Differs { offset: 10_000 }));
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(i.outcome, Outcome::KeepManual);
    assert!(has(i, &B::ContentDiffers));
    assert!(
        of(&c, C::Duplicate).is_empty(),
        "different bytes are not a duplicate"
    );
}

#[test]
fn a_stale_proof_is_ignored_when_a_signature_changed() {
    let mut o = bravo(ContentProof::Identical { compared_at: NOW });
    // The library copy was rewritten after the proof (V6): same size, new mtime.
    o.inventories[1].entries[1].sig.mtime_ns.0 += 1;
    let c = run(&o);
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(
        i.outcome,
        Outcome::KeepManual,
        "an invalid proof proves nothing"
    );
    assert_eq!(
        evidence_level(i),
        removarr_domain::evidence::EvidenceLevel::None
    );
    assert!(of(&c, C::Duplicate).is_empty());
}

#[test]
fn nlink_alone_is_never_a_hardlink_proof() {
    // nlink 2 with the other link outside the scanned trees (T-02): still hardlink_missing.
    let c = Inv::new(completed())
        .file_nlink("Golf.mkv", 60, 16_384, 2)
        .build();
    let m = Inv::new(media()).build();
    let c = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    let i = one(&c, C::HardlinkMissing, "p:completed:Golf.mkv");
    assert_eq!(
        i.outcome,
        Outcome::KeepManual,
        "nothing in the library to consolidate with"
    );
    assert_eq!(
        i.potential_bytes, 0,
        "no replacement is known: nothing is recoverable by it"
    );
    assert!(
        !has(i, &B::ArrMissing),
        "no replacement candidate, so no ARR question"
    );
}

#[test]
fn a_shared_inode_is_healthy_and_a_symlink_is_not_a_hardlink() {
    let c = Inv::new(completed())
        .file_nlink("Alpha/Alpha.mkv", 10, 65_536, 2)
        .file("Juliet.mkv", 70, 16_384)
        .build();
    let m = Inv::new(media())
        .file_nlink("Alpha.mkv", 10, 65_536, 2)
        .symlink("Juliet.mkv", 71)
        .build();
    let c = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    assert!(!subjects(&c, C::HardlinkMissing).contains(&"p:completed:Alpha/Alpha.mkv".to_string()));
    one(&c, C::HardlinkMissing, "p:completed:Juliet.mkv");
    assert_eq!(
        subjects(&c, C::Symlink),
        [format!("p:{}:Juliet.mkv", media())]
    );
}

#[test]
fn sample_videos_are_listed_as_keep_sample_and_protected() {
    let c = Inv::new(completed())
        .file("Rel/Sample/rel.sample.mkv", 80, 8192)
        .file("Kilo/kilo-sample.mkv", 81, 8192)
        .build();
    let m = Inv::new(media()).build();
    let t = Tor::new(
        'k',
        &completed(),
        "forcedUP",
        1.0,
        "Kilo",
        &[("Kilo/kilo-sample.mkv", "Kilo/kilo-sample.mkv")],
    );
    let c = run(&obs(vec![c, m], qb(snapshot(5, vec![t]))));
    for s in [
        "p:completed:Rel/Sample/rel.sample.mkv",
        "p:completed:Kilo/kilo-sample.mkv",
    ] {
        let i = one(&c, C::HardlinkMissing, s);
        assert_eq!(i.outcome, Outcome::KeepSample, "{s}");
        assert_eq!(i.severity, Severity::Info);
        assert!(has(i, &B::Sample));
        assert!(!i.is_unblocked());
    }
}

#[test]
fn candidates_blockers_cross_device_unwritable_target_duplicate_target() {
    let c = Inv::new(completed())
        .file("A.mkv", 1, 4096)
        .file("B.mkv", 2, 4096)
        .file_dev("C.mkv", 2, 3, 4096)
        .build();
    let m = Inv::new(media())
        .dir_mode("ro", 9, 0o555)
        .file("ro/A.mkv", 11, 4096)
        .file("shared.mkv", 12, 4096)
        .file_dev("C.mkv", 1, 13, 4096)
        .build();
    let sig = |inv: &removarr_domain::fs::RootInventory, rel: &str| {
        inv.entries
            .iter()
            .find(|e| e.path.rel.as_str() == rel)
            .unwrap()
            .sig
    };
    let mut o = obs(vec![c.clone(), m.clone()], qb(snapshot(5, vec![])));
    let mk = |a: &str, b: &str| {
        candidate(
            rp(&completed(), a),
            rp(&media(), b),
            sig(&c, a),
            sig(&m, b),
            ContentProof::NameSize,
        )
    };
    o.matches = vec![
        mk("A.mkv", "ro/A.mkv"),
        mk("B.mkv", "shared.mkv"),
        mk("C.mkv", "C.mkv"),
    ];
    // A second source onto the same target `shared.mkv`.
    o.matches.push(candidate(
        rp(&completed(), "A.mkv"),
        rp(&media(), "shared.mkv"),
        sig(&c, "A.mkv"),
        sig(&m, "shared.mkv"),
        ContentProof::NameSize,
    ));
    let c = run(&o);
    let a = one(&c, C::HardlinkMissing, "p:completed:A.mkv");
    assert!(
        has(a, &B::PermissionDenied),
        "target dir mode 0555 for the expected owner"
    );
    let cc = one(&c, C::HardlinkMissing, "p:completed:C.mkv");
    assert!(has(cc, &B::CrossDevice));
    let b = one(&c, C::HardlinkMissing, "p:completed:B.mkv");
    assert!(has(b, &B::DuplicateTarget), "two sources, one target");
}

#[test]
fn a_torrent_outside_the_allowlist_prohibits_destructive_consideration() {
    let mut o = bravo(ContentProof::NameSize);
    let t = Tor::new(
        'b',
        &completed(),
        "checkingUP",
        1.0,
        "Bravo.mkv",
        &[("Bravo.mkv", "Bravo.mkv")],
    );
    o.qbit = qb(snapshot(5, vec![t]));
    let c = run(&o);
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert!(has(i, &B::TorrentProtectedState));
    assert!(!i.is_unblocked());
}

#[test]
fn different_release_in_the_library_needs_arr_and_is_never_auto_resolved() {
    let c = Inv::new(completed())
        .file("Delta.FRENCH.720p.mkv", 5, 40_960)
        .build();
    let m = Inv::new(media())
        .file("Delta.MULTi.1080p.mkv", 6, 81_920)
        .build();
    let t = Tor::new(
        'd',
        &completed(),
        "stoppedUP",
        1.0,
        "Delta.FRENCH.720p.mkv",
        &[("Delta.FRENCH.720p.mkv", "Delta.FRENCH.720p.mkv")],
    );
    let (sa, sb) = (c.entries[0].sig, m.entries[0].sig);
    let mk = |o: &mut removarr_workflows::issues::Observation| {
        o.matches = vec![candidate(
            rp(&completed(), "Delta.FRENCH.720p.mkv"),
            rp(&media(), "Delta.MULTi.1080p.mkv"),
            sa,
            sb,
            ContentProof::None,
        )];
    };
    // No ARR configured: identity unknown.
    let mut o = obs(vec![c.clone(), m.clone()], qb(snapshot(5, vec![t])));
    mk(&mut o);
    let r = run(&o);
    let i = one(&r, C::HardlinkMissing, "p:completed:Delta.FRENCH.720p.mkv");
    assert_eq!(i.outcome, Outcome::KeepManual);
    assert!(has(i, &B::ArrMissing));
    // ARR knows the library file: identity found, but no quality evidence yet.
    let mut o2 = o.clone();
    o2.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(
            ArrService::Radarr,
            5,
            rp(&media(), "Delta.MULTi.1080p.mkv"),
        )],
        vec![],
    )];
    let r2 = run(&o2);
    let i2 = one(&r2, C::HardlinkMissing, "p:completed:Delta.FRENCH.720p.mkv");
    assert!(has(i2, &B::ArrNotUpgradeEvidence));
    assert!(!has(i2, &B::ArrMissing));
    // ARR unavailable: a service blocker as well.
    let mut o3 = o.clone();
    let mut a = arr_ok(ArrService::Sonarr, vec![], vec![]);
    a.state = removarr_domain::session::ServiceState::Unavailable {
        reason: "down".into(),
    };
    o3.arr = vec![a];
    let r3 = run(&o3);
    let i3 = one(&r3, C::HardlinkMissing, "p:completed:Delta.FRENCH.720p.mkv");
    assert!(has(i3, &B::ServiceUnavailable(ServiceKind::Sonarr)));
    assert!(has(i3, &B::ArrMissing));
}

#[test]
fn arr_import_in_progress_blocks_the_same_media() {
    let mut o = bravo(ContentProof::NameSize);
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![],
        vec![queue_item(1, 'b', 9, ArrService::Radarr, "ok", "importing")],
    )];
    let c = run(&o);
    let i = one(&c, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert!(has(i, &B::ImportInProgress));
}

// ---------------------------------------------------------------- duplicate

#[test]
fn duplicate_pair_has_a_sorted_physical_pair_subject() {
    let c = run(&bravo(ContentProof::Sampled));
    let d = one(&c, C::Duplicate, "d:1:20-1:21");
    assert_eq!(d.outcome, Outcome::Consolidate);
    assert!(
        matches!(&d.unit, Unit::Pair { a, b } if a.rel.as_str() == "Bravo.mkv" && b.rel.as_str() == "movies/Bravo.mkv")
    );
    assert_eq!(
        evidence_level(d),
        removarr_domain::evidence::EvidenceLevel::Sampled
    );
    assert_eq!(d.potential_bytes, 81_920);
}

#[test]
fn identity_only_pairs_and_size_differences_are_not_duplicates() {
    let c = Inv::new(completed()).file("D.mkv", 5, 40_960).build();
    let m = Inv::new(media()).file("D2.mkv", 6, 81_920).build();
    let (sa, sb) = (c.entries[0].sig, m.entries[0].sig);
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![])));
    o.matches = vec![candidate(
        rp(&completed(), "D.mkv"),
        rp(&media(), "D2.mkv"),
        sa,
        sb,
        ContentProof::Semantic { score_milli: 950 },
    )];
    assert!(
        of(&run(&o), C::Duplicate).is_empty(),
        "different sizes cannot be the same bytes"
    );
}

// ---------------------------------------------------------------- incomplete

#[test]
fn stalled_incomplete_download_is_listed_with_its_torrent_and_age() {
    let i = Inv::new(incomplete())
        .file("Oscar.mkv", 90, 8192)
        .file("abandoned.partial.mkv", 91, 4096)
        .build();
    let c0 = Inv::new(completed()).build();
    let t = Tor::new(
        'd',
        &incomplete(),
        "stalledDL",
        0.4,
        "Oscar.mkv",
        &[("Oscar.mkv", "Oscar.mkv")],
    )
    .added_days_ago(30);
    let young = Tor::new(
        'e',
        &incomplete(),
        "downloading",
        0.1,
        "Young.mkv",
        &[("Young.mkv", "Young.mkv")],
    )
    .added_days_ago(2);
    let done = Tor::new(
        'f',
        &incomplete(),
        "uploading",
        1.0,
        "Done.mkv",
        &[("Done.mkv", "Done.mkv")],
    )
    .added_days_ago(60);
    let r = run(&obs(vec![i, c0], qb(snapshot(5, vec![t, young, done]))));
    let s = of(&r, C::IncompleteStalled);
    assert_eq!(s.len(), 1, "only the old unfinished torrent");
    let s = s[0];
    assert_eq!(s.subject.as_str(), tid('d'));
    assert_eq!(s.age_basis.age_days(NOW), 30);
    assert!(s.is_unblocked(), "{:?}", s.blockers);
    assert_eq!(s.logical_bytes, 8192);
    assert_eq!(s.potential_bytes, 8192);
    assert!(matches!(s.unit, Unit::Torrent(_)));
}

#[test]
fn a_busy_stalled_torrent_is_listed_but_blocked() {
    let i = Inv::new(incomplete()).file("Oscar.mkv", 90, 8192).build();
    let t = Tor::new(
        'd',
        &incomplete(),
        "checkingDL",
        0.4,
        "Oscar.mkv",
        &[("Oscar.mkv", "Oscar.mkv")],
    )
    .added_days_ago(30);
    let r = run(&obs(
        vec![i, Inv::new(completed()).build()],
        qb(snapshot(5, vec![t])),
    ));
    let s = of(&r, C::IncompleteStalled);
    assert_eq!(s.len(), 1);
    assert!(has(s[0], &B::TorrentProtectedState));
}

#[test]
fn incomplete_file_without_a_torrent_is_an_orphan() {
    let i = Inv::new(incomplete())
        .file("Oscar.mkv", 90, 8192)
        .file("abandoned.partial.mkv", 91, 4096)
        .file("Oscar.mkv.!qB", 92, 100)
        .build();
    let t = Tor::new(
        'd',
        &incomplete(),
        "stalledDL",
        0.4,
        "Oscar.mkv",
        &[("Oscar.mkv", "Oscar.mkv")],
    );
    let r = run(&obs(
        vec![i, Inv::new(completed()).build()],
        qb(snapshot(5, vec![t])),
    ));
    let o = of(&r, C::IncompleteOrphan);
    let names: Vec<_> = o.iter().map(|i| i.subject.as_str()).collect();
    assert!(names.contains(&"p:incomplete:abandoned.partial.mkv"));
    assert!(!names.contains(&"p:incomplete:Oscar.mkv"));
    // A `.!qB` working file next to an unfinished torrent's file belongs to that torrent.
    assert!(!names.contains(&"p:incomplete:Oscar.mkv.!qB"));
}

// ---------------------------------------------------------------- orphan_completed

#[test]
fn orphan_file_without_a_torrent_with_potential_only_for_a_sole_inode() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 40, 32_768)
        .file_nlink("Golf.mkv", 41, 16_384, 2) // second link outside the scanned trees
        .file("Alpha.mkv", 42, 4096) // also in media
        .build();
    let m = Inv::new(media()).file("Alpha.mkv", 42, 4096).build();
    let r = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    let echo = one(&r, C::OrphanCompleted, "p:completed:Echo.mkv");
    assert_eq!(echo.potential_bytes, 32_768);
    assert_eq!(echo.severity, Severity::Warning);
    assert!(echo.is_unblocked(), "{:?}", echo.blockers);
    assert_eq!(
        one(&r, C::OrphanCompleted, "p:completed:Golf.mkv").potential_bytes,
        0
    );
    assert_eq!(
        one(&r, C::OrphanCompleted, "p:completed:Alpha.mkv").potential_bytes,
        0
    );
}

#[test]
fn a_fully_torrentless_release_folder_is_one_unit_with_its_annexes() {
    let c = Inv::new(completed())
        .dir("India", 1)
        .file("India/India.mkv", 50, 16_384)
        .symlink("India/link.txt", 51)
        .dir("India/Sample", 2)
        .file("India/Sample/india-sample.mkv", 52, 4096)
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let o = of(&r, C::OrphanCompleted);
    assert_eq!(
        o.len(),
        1,
        "one unit, not a main file plus a Sample/ folder"
    );
    let u = o[0];
    assert_eq!(u.subject.as_str(), "p:completed:India");
    let Unit::Folder {
        dir,
        members,
        annexes,
    } = &u.unit
    else {
        panic!("folder unit")
    };
    assert_eq!(dir.rel.as_str(), "India");
    assert_eq!(
        members.iter().map(|m| m.rel.as_str()).collect::<Vec<_>>(),
        ["India/India.mkv", "India/Sample/india-sample.mkv"]
    );
    let ann: Vec<_> = annexes.iter().map(|a| a.rel.as_str()).collect();
    assert!(ann.contains(&"India/link.txt") && ann.contains(&"India/Sample"));
    assert!(
        has(u, &B::Sample),
        "a sample inside the unit needs per-sample acknowledgement"
    );
    assert_eq!(u.logical_bytes, 16_384 + 4096);
}

#[test]
fn a_release_folder_with_a_held_file_is_not_a_unit() {
    let c = Inv::new(completed())
        .dir("R", 1)
        .file("R/R.mkv", 50, 4096)
        .file("R/extra.mkv", 51, 4096)
        .build();
    let t = Tor::new(
        'a',
        &completed(),
        "uploading",
        1.0,
        "R",
        &[("R/R.mkv", "R/R.mkv")],
    );
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![t])),
    ));
    let o = subjects(&r, C::OrphanCompleted);
    assert_eq!(
        o,
        ["p:completed:R/extra.mkv"],
        "only the unlisted file, never the folder"
    );
}

#[test]
fn an_inode_held_by_a_torrent_elsewhere_is_listed_with_zero_potential() {
    // FX-34 / FX-35: the second path of a seeding inode, with no torrent of its own.
    let c = Inv::new(completed())
        .dir("tango-dupe", 1)
        .file_nlink("Tango.mkv", 30, 24_576, 2)
        .file_nlink("tango-dupe/Tango.mkv", 30, 24_576, 2)
        .build();
    let t = Tor::new(
        'a',
        &completed(),
        "uploading",
        1.0,
        "Tango.mkv",
        &[("Tango.mkv", "Tango.mkv")],
    );
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![t])),
    ));
    let i = one(&r, C::OrphanCompleted, "p:completed:tango-dupe");
    assert_eq!(i.potential_bytes, 0);
    assert!(i.evidence.iter().any(|e| matches!(&e.kind, EvidenceKind::Identity { subject, .. } if subject.contains("inode_shared_with_torrent_file"))));
}

// ---------------------------------------------------------------- torrent_missing_files

#[test]
fn missing_files_by_state_and_by_a_clean_scan() {
    let c = Inv::new(completed()).file("Present.mkv", 1, 1000).build();
    let m = Inv::new(media()).file("Lib/Gone.mkv", 2, 1000).build();
    let by_state = Tor::new(
        'p',
        &completed(),
        "missingFiles",
        1.0,
        "Papa.mkv",
        &[("Papa.mkv", "Papa.mkv")],
    );
    let by_scan = Tor::new(
        'q',
        &completed(),
        "uploading",
        1.0,
        "Gone.mkv",
        &[("Gone.mkv", "Gone.mkv")],
    );
    let fine = Tor::new(
        'r',
        &completed(),
        "uploading",
        1.0,
        "Present.mkv",
        &[("Present.mkv", "Present.mkv")],
    );
    let r = run(&obs(
        vec![c, m],
        qb(snapshot(5, vec![by_state, by_scan, fine])),
    ));
    let m = of(&r, C::TorrentMissingFiles);
    assert_eq!(m.len(), 2);
    assert!(
        m.iter()
            .all(|i| i.severity == Severity::Critical && i.outcome == Outcome::KeepManual)
    );
    let q = one(&r, C::TorrentMissingFiles, &tid('q'));
    assert_eq!(q.logical_bytes, 1000);
}

#[test]
fn an_absence_is_not_claimed_when_the_scan_around_it_failed() {
    let c = Inv::new(completed()).error("dir").build();
    let t = Tor::new(
        'q',
        &completed(),
        "uploading",
        1.0,
        "dir/Gone.mkv",
        &[("dir/Gone.mkv", "dir/Gone.mkv")],
    );
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![t])),
    ));
    assert!(of(&r, C::TorrentMissingFiles).is_empty());
}

#[test]
fn missing_file_carries_a_name_and_size_hint_never_a_proof() {
    let m = Inv::new(media()).file("Lib/Gone.mkv", 2, 1000).build();
    let t = Tor::new(
        'q',
        &completed(),
        "missingFiles",
        1.0,
        "Gone.mkv",
        &[("Gone.mkv", "Gone.mkv")],
    );
    // State-based only: the hint needs a certainly-absent file.
    let t2 = Tor::new(
        's',
        &completed(),
        "uploading",
        1.0,
        "Gone.mkv",
        &[("Gone.mkv", "Gone.mkv")],
    );
    let r = run(&obs(
        vec![Inv::new(completed()).build(), m],
        qb(snapshot(5, vec![t, t2])),
    ));
    let i = one(&r, C::TorrentMissingFiles, &tid('s'));
    let hint = i.evidence.iter().find_map(|e| match &e.kind {
        EvidenceKind::Content { proof, .. } => Some(proof.clone()),
        _ => None,
    });
    assert_eq!(hint, Some(ContentProof::NameSize));
}

// ---------------------------------------------------------------- media_without_source

#[test]
fn media_without_source_is_information_only_and_never_a_deletion_candidate() {
    let c = Inv::new(completed()).file("A.mkv", 1, 4096).build();
    let m = Inv::new(media())
        .file("A.mkv", 1, 4096) // linked: has a source
        .file_nlink("Hotel.mkv", 2, 4096, 2) // second link outside the trees
        .file("Managed.mkv", 3, 4096)
        .build();
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![])));
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(
            ArrService::Radarr,
            3,
            rp(&media(), "Managed.mkv"),
        )],
        vec![],
    )];
    let r = run(&o);
    let w = of(&r, C::MediaWithoutSource);
    assert_eq!(w.len(), 2);
    for i in &w {
        assert_eq!(i.severity, Severity::Info);
        assert!(has(i, &B::MediaRootDeletion));
        assert_eq!(i.potential_bytes, 0);
        assert!(i.physical_ids.is_empty());
    }
    let hotel = one(
        &r,
        C::MediaWithoutSource,
        &format!("p:{}:Hotel.mkv", media()),
    );
    assert!(hotel.evidence.iter().any(|e| matches!(&e.kind, EvidenceKind::Identity { subject, .. } if subject.contains("links_outside_scanned_trees_possible"))));
    let managed = one(
        &r,
        C::MediaWithoutSource,
        &format!("p:{}:Managed.mkv", media()),
    );
    assert!(managed.evidence.iter().any(|e| matches!(&e.kind, EvidenceKind::Identity { subject, .. } if subject.starts_with("arr:radarr:3"))));
}

// ---------------------------------------------------------------- permission

#[test]
fn owner_mismatch_and_unwritable_parent_directories_are_reported() {
    let m = Inv::new(media())
        .dir_mode("movies", 1, 0o555)
        .file("movies/a.mkv", 2, 4096)
        .file("b.mkv", 3, 4096)
        .with("b.mkv", |s| s.uid = 0)
        .file("c.mkv", 4, 4096)
        .with("c.mkv", |s| s.mode = 0o444)
        .build();
    let r = run(&obs(
        vec![Inv::new(completed()).build(), m],
        qb(snapshot(5, vec![])),
    ));
    let mr = media();
    let dir = one(&r, C::Permission, &format!("p:{mr}:movies"));
    assert_eq!(dir.severity, Severity::Critical, "parent not writable");
    let own = one(&r, C::Permission, &format!("p:{mr}:b.mkv"));
    assert_eq!(own.severity, Severity::Warning);
    assert!(has(own, &B::PermissionDenied), "no chown is ever offered");
    let ro = one(&r, C::Permission, &format!("p:{mr}:c.mkv"));
    assert_eq!(ro.severity, Severity::Warning);
    assert!(
        !has(ro, &B::PermissionDenied),
        "own entry: a targeted mode fix is possible"
    );
    assert_eq!(ro.outcome, Outcome::KeepManual);
}

#[test]
fn a_healthy_tree_has_no_permission_issue() {
    let m = Inv::new(media()).dir("d", 1).file("d/a.mkv", 2, 1).build();
    let r = run(&obs(
        vec![Inv::new(completed()).build(), m],
        qb(snapshot(5, vec![])),
    ));
    assert!(of(&r, C::Permission).is_empty());
}

// ---------------------------------------------------------------- junk

#[test]
fn junk_is_only_what_no_torrent_holds_and_never_a_sample() {
    let c = Inv::new(completed())
        .dir("stray", 1)
        .file("stray/readme.txt", 10, 128)
        .file("stray/archive.rar", 11, 4096)
        .file("stray/archive.r05", 12, 4096)
        .file("stray/Unrelated.Film.2010.srt", 13, 256)
        .file("stray/Movie.en.srt", 14, 256)
        .file("stray/Movie.mkv", 15, 4096)
        .dir("Alpha", 2)
        .file("Alpha/Alpha.nfo", 16, 512)
        .file("Alpha/Alpha.mkv", 17, 4096)
        .file("Alpha/sample.nfo", 18, 10)
        .file("Alpha/Extra.txt", 19, 10)
        .build();
    // Alpha.nfo is held by the torrent; Extra.txt shares its inode with a held file via a hardlink.
    let t = Tor::new(
        'a',
        &completed(),
        "uploading",
        1.0,
        "Alpha",
        &[
            ("Alpha/Alpha.nfo", "Alpha/Alpha.nfo"),
            ("Alpha/Alpha.mkv", "Alpha/Alpha.mkv"),
        ],
    );
    let c = Inv { inv: c }
        .with("Alpha/Extra.txt", |s| s.ino = 16)
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![t])),
    ));
    let j = subjects(&r, C::Junk);
    assert_eq!(
        j,
        [
            "p:completed:stray/Unrelated.Film.2010.srt",
            "p:completed:stray/archive.r05",
            "p:completed:stray/archive.rar",
            "p:completed:stray/readme.txt",
        ],
        "held nfo, inode-held txt, sample.nfo and the subtitle of a present video are not junk"
    );
    assert!(of(&r, C::Junk).iter().all(|i| i.severity == Severity::Info));
}

#[test]
fn junk_in_a_media_root_does_not_exist() {
    let m = Inv::new(media())
        .file("poster.jpg", 1, 10)
        .file("movie.nfo", 2, 10)
        .build();
    let r = run(&obs(
        vec![Inv::new(completed()).build(), m],
        qb(snapshot(5, vec![])),
    ));
    assert!(of(&r, C::Junk).is_empty(), "Jellyfin metadata is not junk");
    assert!(of(&r, C::OrphanCompleted).is_empty());
}

// ---------------------------------------------------------------- torrent_protected / unmapped

#[test]
fn torrents_outside_the_allowlist_are_listed_with_their_severity() {
    let c = Inv::new(completed()).build();
    let mk = |h: char, st: &str, p: f64| {
        Tor::new(
            h,
            &completed(),
            st,
            p,
            &format!("{h}.mkv"),
            &[(&format!("{h}.mkv"), &format!("{h}.mkv"))],
        )
    };
    let snap = snapshot(
        5,
        vec![
            mk('1', "uploading", 1.0),
            mk('2', "checkingUP", 1.0),
            mk('3', "error", 1.0),
            mk('4', "somethingNew", 1.0),
            mk('5', "uploading", 0.5),
            mk('6', "pausedUP", 1.0),
        ],
    );
    let r = run(&obs(vec![c, Inv::new(media()).build()], qb(snap)));
    let p = |h: char| subjects(&r, C::TorrentProtected).contains(&tid(h));
    assert!(!p('1'), "eligible torrent is not protected");
    assert!(p('2') && p('3') && p('4') && p('5'));
    assert!(p('6'), "pausedUP is a 4.x name: protected on 5.x");
    assert_eq!(
        one(&r, C::TorrentProtected, &tid('2')).severity,
        Severity::Info
    );
    assert_eq!(
        one(&r, C::TorrentProtected, &tid('3')).severity,
        Severity::Critical
    );
    assert_eq!(
        one(&r, C::TorrentProtected, &tid('4')).severity,
        Severity::Critical
    );
}

#[test]
fn unmapped_torrents_record_the_field_never_the_raw_path() {
    use removarr_domain::torrent::MappedPath;
    let q = Tor::new(
        'c',
        &completed(),
        "uploading",
        1.0,
        "Quebec.mkv",
        &[("Quebec.mkv", "Quebec.mkv")],
    )
    .raw_content(MappedPath::NoRule {
        raw: "/srv/other/Quebec.Movie.2006.mkv".into(),
    })
    .save_path(MappedPath::NoRule {
        raw: "/srv/other".into(),
    });
    let ext = Tor::new(
        'd',
        &completed(),
        "uploading",
        1.0,
        "X.mkv",
        &[("X.mkv", "X.mkv")],
    )
    .raw_content(MappedPath::DeclaredExternal {
        raw: "/ext/X.mkv".into(),
    });
    let bad = Tor::new(
        'e',
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
    let r = run(&obs(
        vec![Inv::new(completed()).build(), Inv::new(media()).build()],
        qb(snapshot(5, vec![q, ext, bad])),
    ));
    let qi = one(&r, C::UnmappedTorrent, &tid('c'));
    assert_eq!(qi.severity, Severity::Critical);
    assert!(has(qi, &B::UnmappedTorrents));
    assert_eq!(
        one(&r, C::UnmappedTorrent, &tid('d')).severity,
        Severity::Info
    );
    let bi = one(&r, C::UnmappedTorrent, &tid('e'));
    assert!(has(bi, &B::MappingError));
    let json = serde_json::to_string(&r.issues).unwrap();
    assert!(
        !json.contains("/srv/other") && !json.contains("/etc") && !json.contains("/ext/"),
        "no raw service path is stored"
    );
}

// ---------------------------------------------------------------- symlink

#[test]
fn symlinks_are_reported_never_followed_and_never_store_their_target() {
    let c = Inv::new(completed()).symlink("escape", 1).build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let s = one(&r, C::Symlink, "p:completed:escape");
    assert_eq!(s.severity, Severity::Info);
    assert!(has(s, &B::Symlink));
    assert!(
        !serde_json::to_string(&s).unwrap().contains("outside"),
        "INV-09: no undeclared path"
    );
}

// ---------------------------------------------------------------- arr_queue

fn queue_world(link: bool, parent_mode: u32) -> removarr_workflows::issues::Observation {
    let c = Inv::new(completed())
        .dir("Rel", 1)
        .file("Rel/Rel.mkv", 20, 4096)
        .build();
    let m = Inv::new(media())
        .dir_mode("movies", 9, parent_mode)
        .file("movies/Rel.mkv", if link { 20 } else { 21 }, 4096)
        .build();
    let t = Tor::new(
        'b',
        &completed(),
        "uploading",
        1.0,
        "Rel",
        &[("Rel/Rel.mkv", "Rel/Rel.mkv")],
    );
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![t])));
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(
            ArrService::Radarr,
            9,
            rp(&media(), "movies/Rel.mkv"),
        )],
        vec![queue_item(
            5,
            'b',
            9,
            ArrService::Radarr,
            "warning",
            "importBlocked",
        )],
    )];
    o
}

#[test]
fn a_queue_alert_with_a_proven_hardlink_and_no_permission_finding_is_stale() {
    let r = run(&queue_world(true, 0o755));
    let q = one(&r, C::ArrQueue, "q:radarr:5");
    assert!(!has(q, &B::QueueItemHidesError), "{:?}", q.blockers);
    assert_eq!(q.severity, Severity::Warning);
    assert!(
        q.evidence
            .iter()
            .any(|e| matches!(&e.kind, EvidenceKind::SamePhysical { a, b } if a == b))
    );
    assert!(
        !serde_json::to_string(q).unwrap().contains("secret message"),
        "ARR messages are not stored"
    );
}

#[test]
fn a_queue_alert_without_an_inode_proof_hides_an_error() {
    let r = run(&queue_world(false, 0o755));
    assert!(has(
        one(&r, C::ArrQueue, "q:radarr:5"),
        &B::QueueItemHidesError
    ));
}

#[test]
fn a_permission_finding_on_the_media_parent_keeps_the_error_hidden() {
    // The inode check passes, but the parent directory is not writable (the permission case).
    let r = run(&queue_world(true, 0o555));
    assert!(has(
        one(&r, C::ArrQueue, "q:radarr:5"),
        &B::QueueItemHidesError
    ));
}

#[test]
fn healthy_queue_items_and_unknown_identity() {
    let mut o = queue_world(true, 0o755);
    o.arr[0].queue = vec![
        queue_item(1, 'b', 9, ArrService::Radarr, "ok", "downloading"),
        {
            let mut q = queue_item(2, 'b', 9, ArrService::Radarr, "error", "failed");
            q.media = None;
            q
        },
    ];
    let r = run(&o);
    assert_eq!(subjects(&r, C::ArrQueue), ["q:radarr:2"]);
    let q = one(&r, C::ArrQueue, "q:radarr:2");
    assert_eq!(q.severity, Severity::Critical);
    assert!(
        has(q, &B::ArrAmbiguous),
        "unknown identity requires manual review"
    );
}
