#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The legacy parity fixture (docs/legacy-parity.md section 7) end to end: the real confined
//! filesystem adapter, the real matching funnel, a hand-built qBittorrent snapshot, and the
//! classifier. Expectations are section 7.7 (T-01, T-02, T-08, T-15 adapted and the category
//! tables). Linux/macOS temp directories only; nothing outside them is read.

mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_config::{RootPaths, Roots};
use removarr_domain::evidence::{ContentProof, EvidenceLevel};
use removarr_domain::fs::{RootInventory, SampleSpec};
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C, Outcome, Severity, Unit};
use removarr_domain::path::RootId;
use removarr_domain::ports::{FsInventory, ScanControl};
use removarr_domain::torrent::MappedPath;
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_matching::{ContentVerifier, FunnelInput, MatchSettings, ReadLimiter, run_funnel};
use removarr_testkit::fixture::ParityFixture;
use removarr_workflows::issues::{
    Classified, IssueSettings, Observation, QbitObservation, classify, evidence_level,
};

const ALPHA: &str = "Alpha.Movie.2020.1080p.WEB.x264-GRP";
const BRAVO: &str = "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv";
const CHARLIE: &str = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
const DELTA: &str = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
const FOXTROT: &str = "Foxtrot.Show.S01.1080p.WEB.x264-GRP";
const JULIET: &str = "Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv";
const KILO: &str = "Kilo.Movie.2012.1080p.BluRay.x264-GRP";
const NOVEMBER: &str = "November.Movie.2010.1080p.WEB.x264-GRP.mkv";
const TANGO: &str = "Tango.Movie.2007.1080p.WEB.x264-GRP.mkv";
const OSCAR: &str = "Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv";

struct World {
    fs: ConfinedFs,
    media: RootId,
}

fn world(fx: &ParityFixture) -> World {
    let roots = Roots::validate(&RootPaths {
        completed: Some(fx.completed().to_str().unwrap().into()),
        incomplete: Some(fx.incomplete().to_str().unwrap().into()),
        media: vec![fx.medias().to_str().unwrap().into()],
    })
    .unwrap();
    let fs = ConfinedFs::new(&roots, FsLimits::default());
    let media = RootId::media(fx.medias().to_str().unwrap());
    World { fs, media }
}

fn scan(w: &World, root: &RootId) -> RootInventory {
    w.fs.scan_root(root, &ScanControl::default()).unwrap()
}

/// The 13 torrents of section 7.3 (T_Q is outside every mapping rule).
fn torrents() -> Vec<Tor> {
    let c = completed();
    let one =
        |h, st: &str, content: &str, name: &str| Tor::new(h, &c, st, 1.0, content, &[(name, name)]);
    vec![
        Tor::new(
            '1',
            &c,
            "uploading",
            1.0,
            ALPHA,
            &[
                (
                    &format!("{ALPHA}/{ALPHA}.mkv"),
                    &format!("{ALPHA}/{ALPHA}.mkv"),
                ),
                (
                    &format!("{ALPHA}/{ALPHA}.nfo"),
                    &format!("{ALPHA}/{ALPHA}.nfo"),
                ),
            ],
        ),
        one('2', "stalledUP", BRAVO, BRAVO),
        one('3', "pausedUP", BRAVO, BRAVO),
        Tor::new(
            '4',
            &c,
            "uploading",
            1.0,
            CHARLIE,
            &[
                (
                    &format!("{CHARLIE}/{CHARLIE}.mkv"),
                    &format!("{CHARLIE}/{CHARLIE}.mkv"),
                ),
                (
                    &format!(
                        "{CHARLIE}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"
                    ),
                    &format!(
                        "{CHARLIE}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"
                    ),
                ),
                (
                    &format!("{CHARLIE}/{CHARLIE}.nfo"),
                    &format!("{CHARLIE}/{CHARLIE}.nfo"),
                ),
            ],
        ),
        one('5', "stoppedUP", DELTA, DELTA),
        Tor::new(
            '6',
            &c,
            "queuedUP",
            1.0,
            FOXTROT,
            &[
                (
                    &format!("{FOXTROT}/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv"),
                    &format!("{FOXTROT}/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv"),
                ),
                (
                    &format!("{FOXTROT}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv"),
                    &format!("{FOXTROT}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv"),
                ),
            ],
        ),
        one('7', "checkingUP", JULIET, JULIET),
        Tor::new(
            '8',
            &c,
            "forcedUP",
            1.0,
            KILO,
            &[
                (&format!("{KILO}/{KILO}.mkv"), &format!("{KILO}/{KILO}.mkv")),
                (
                    &format!("{KILO}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"),
                    &format!("{KILO}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"),
                ),
            ],
        ),
        one('9', "uploading", NOVEMBER, NOVEMBER),
        one('a', "uploading", TANGO, TANGO),
        one(
            'b',
            "missingFiles",
            "Papa.Movie.2008.1080p.WEB.x264-GRP.mkv",
            "Papa.Movie.2008.1080p.WEB.x264-GRP.mkv",
        ),
        one(
            'c',
            "uploading",
            "Quebec.Movie.2006.mkv",
            "Quebec.Movie.2006.mkv",
        )
        .raw_content(MappedPath::NoRule {
            raw: "/srv/other/Quebec.Movie.2006.mkv".into(),
        })
        .save_path(MappedPath::NoRule {
            raw: "/srv/other".into(),
        })
        .unmapped_files(),
        Tor::new(
            'd',
            &incomplete(),
            "stalledDL",
            0.4,
            OSCAR,
            &[(OSCAR, OSCAR)],
        )
        .added_days_ago(30),
    ]
}

fn funnel(w: &World, invs: &[RootInventory]) -> Vec<removarr_domain::evidence::MatchCandidate> {
    let src: Vec<_> = invs
        .iter()
        .filter(|i| i.root == completed())
        .cloned()
        .collect();
    let tgt: Vec<_> = invs.iter().filter(|i| i.root == w.media).cloned().collect();
    let settings = MatchSettings {
        video_extensions: [".mkv".to_string()].into_iter().collect(),
        sample: SampleSpec {
            size: 4096,
            count: 5,
        },
        ..MatchSettings::default()
    };
    run_funnel(
        FunnelInput {
            sources: &src,
            targets: &tgt,
            previous: &[],
        },
        &settings,
        &ContentVerifier::new(ReadLimiter::new(1), 65536),
        &w.fs,
        &ScanControl::default(),
    )
    .unwrap()
    .match_candidates(session())
}

struct Run {
    c: Classified,
    media: RootId,
}

fn classify_fixture(
    fx: &ParityFixture,
    major: u8,
    owner: (u32, u32),
    tweak: impl FnOnce(&mut Observation),
) -> Run {
    let w = world(fx);
    let invs: Vec<RootInventory> = [completed(), incomplete(), w.media.clone()]
        .iter()
        .map(|r| scan(&w, r))
        .collect();
    let matches = funnel(&w, &invs);
    let mut o = obs(
        invs,
        QbitObservation::Available(snapshot(major, torrents())),
    );
    o.configured_roots = vec![completed(), incomplete(), w.media.clone()];
    o.matches = matches;
    tweak(&mut o);
    let mut s = IssueSettings::new(owner);
    s.video_extensions = [".mkv".to_string()].into_iter().collect();
    Run {
        c: classify(&o, &s).unwrap(),
        media: w.media,
    }
}

fn paths(c: &Classified, cat: C) -> BTreeSet<String> {
    c.issues
        .iter()
        .filter(|i| i.category == cat)
        .map(|i| i.subject.as_str().to_string())
        .collect()
}

fn pc(rel: &str) -> String {
    format!("p:completed:{rel}")
}

#[test]
fn hardlink_defects_are_the_thirteen_completed_paths_with_the_expected_outcomes() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let r = classify_fixture(&fx, 4, owner, |_| {});
    let c = &r.c;
    let expected: BTreeSet<String> = [
        BRAVO.to_string(),
        format!("{CHARLIE}/{CHARLIE}.mkv"),
        format!("{CHARLIE}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"),
        DELTA.to_string(),
        "Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv".to_string(),
        format!("{FOXTROT}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv"),
        "Golf.Movie.2016.1080p.WEB.x264-GRP.mkv".to_string(),
        "India.Movie.2014.1080p.WEB.x264-GRP/India.Movie.2014.1080p.WEB.x264-GRP.mkv".to_string(),
        JULIET.to_string(),
        format!("{KILO}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"),
        NOVEMBER.to_string(),
        format!("tango-dupe/{TANGO}"),
        TANGO.to_string(),
    ]
    .iter()
    .map(|p| pc(p))
    .collect();
    assert_eq!(
        paths(c, C::HardlinkMissing),
        expected,
        "T-08: the completed side, shared inodes excluded"
    );

    // FX-04: consolidate candidate (name + size), two torrents, no blocker on 4.x (H2 stalledUP, H3 pausedUP).
    let fx04 = one(c, C::HardlinkMissing, &pc(BRAVO));
    assert_eq!(fx04.outcome, Outcome::Consolidate);
    assert_eq!(
        fx04.torrent_hashes.len(),
        2,
        "INV-29: one row, two torrents"
    );
    assert!(fx04.is_unblocked(), "{:?}", fx04.blockers);
    assert_eq!(evidence_level(fx04), EvidenceLevel::NameSize);
    // FX-06: consolidate through parsing + samples.
    let fx06 = one(
        c,
        C::HardlinkMissing,
        &pc(&format!("{CHARLIE}/{CHARLIE}.mkv")),
    );
    assert_eq!(fx06.outcome, Outcome::Consolidate);
    assert_eq!(evidence_level(fx06), EvidenceLevel::Sampled);
    // FX-32: consolidate, blocked by the unwritable target directory.
    let fx32 = one(c, C::HardlinkMissing, &pc(NOVEMBER));
    assert_eq!(fx32.outcome, Outcome::Consolidate);
    assert!(fx32.blockers.iter().any(|b| b.code == B::PermissionDenied));
    // FX-10: another release in the library: manual, ARR required (no ARR configured here).
    let fx10 = one(c, C::HardlinkMissing, &pc(DELTA));
    assert_eq!(fx10.outcome, Outcome::KeepManual);
    assert!(fx10.blockers.iter().any(|b| b.code == B::ArrMissing));
    // Samples: protected.
    for s in [
        format!("{CHARLIE}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"),
        format!("{KILO}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"),
    ] {
        let i = one(c, C::HardlinkMissing, &pc(&s));
        assert_eq!(
            (i.outcome, i.severity),
            (Outcome::KeepSample, Severity::Info)
        );
    }
    // FX-25 (Juliet, symlinked in media) is blocked by its checkingUP torrent H7.
    let juliet = one(c, C::HardlinkMissing, &pc(JULIET));
    assert!(
        juliet
            .blockers
            .iter()
            .any(|b| b.code == B::TorrentProtectedState)
    );
    // Manual / keep: no replacement exists.
    for s in [
        "Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv",
        "Golf.Movie.2016.1080p.WEB.x264-GRP.mkv",
    ] {
        assert_eq!(
            one(c, C::HardlinkMissing, &pc(s)).outcome,
            Outcome::KeepManual
        );
    }
}

#[test]
fn the_allowlist_is_version_aware_in_the_mixed_name_fixture() {
    // The base fixture mixes a 4.x name (pausedUP, H3) and a 5.x name (stoppedUP, H5). A strict,
    // version-aware allowlist protects whichever does not exist on the observed major.
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let v5 = classify_fixture(&fx, 5, owner, |_| {});
    assert!(
        one(&v5.c, C::HardlinkMissing, &pc(BRAVO))
            .blockers
            .iter()
            .any(|b| b.code == B::TorrentProtectedState),
        "pausedUP on 5.x"
    );
    let v4 = classify_fixture(&fx, 4, owner, |_| {});
    assert!(
        one(&v4.c, C::HardlinkMissing, &pc(DELTA))
            .blockers
            .iter()
            .any(|b| b.code == B::TorrentProtectedState),
        "stoppedUP on 4.x"
    );
}

#[test]
fn duplicates_are_the_three_pairs_and_become_proven_after_a_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let r = classify_fixture(&fx, 4, owner, |_| {});
    let d: Vec<_> =
        r.c.issues
            .iter()
            .filter(|i| i.category == C::Duplicate)
            .collect();
    assert_eq!(d.len(), 3, "FX-04/05, FX-06/09, FX-32/33");
    assert!(
        d.iter().all(|i| evidence_level(i) < EvidenceLevel::Full),
        "scans stop at sample level"
    );

    // After VerifyContent the same pairs carry a full-comparison proof.
    let proven = classify_fixture(&fx, 4, owner, |o| {
        for m in &mut o.matches {
            if m.content != ContentProof::None
                && !matches!(m.content, ContentProof::Semantic { .. })
                && m.sig_a.size == m.sig_b.size
            {
                m.content = ContentProof::Identical { compared_at: NOW };
            }
        }
    });
    let d: Vec<_> = proven
        .c
        .issues
        .iter()
        .filter(|i| i.category == C::Duplicate)
        .collect();
    assert_eq!(d.len(), 3);
    assert!(d.iter().all(|i| evidence_level(i) == EvidenceLevel::Full));
}

#[test]
fn orphans_junk_media_only_symlinks_incomplete_and_torrent_categories() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let r = classify_fixture(&fx, 4, owner, |_| {});
    let c = &r.c;

    // Completed files without a torrent: FX-12, FX-16, the India folder unit, tango-dupe, FX-29..31.
    let expected_orphans: BTreeSet<String> = [
        "Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv",
        "Golf.Movie.2016.1080p.WEB.x264-GRP.mkv",
        "India.Movie.2014.1080p.WEB.x264-GRP",
        "tango-dupe",
        "stray/readme.txt",
        "stray/archive.rar",
        "stray/Unrelated.Film.2010.srt",
    ]
    .iter()
    .map(|p| pc(p))
    .collect();
    assert_eq!(paths(c, C::OrphanCompleted), expected_orphans);
    let india = one(
        c,
        C::OrphanCompleted,
        &pc("India.Movie.2014.1080p.WEB.x264-GRP"),
    );
    let Unit::Folder {
        members, annexes, ..
    } = &india.unit
    else {
        panic!("unit")
    };
    assert_eq!(members.len(), 1);
    assert!(
        annexes
            .iter()
            .any(|a| a.rel.as_str().ends_with("link-to-outside.txt")),
        "the symlink annex FX-22 is in the preview"
    );
    // OQ-08: FX-35 shares an inode with the seeding FX-34: listed, nothing recoverable.
    assert_eq!(
        one(c, C::OrphanCompleted, &pc("tango-dupe")).potential_bytes,
        0
    );
    assert_eq!(
        one(
            c,
            C::OrphanCompleted,
            &pc("Golf.Movie.2016.1080p.WEB.x264-GRP.mkv")
        )
        .potential_bytes,
        0,
        "nlink 2, one link outside the trees"
    );

    // Junk outside torrents only; FX-02/FX-08 (held) and the samples are not junk.
    assert_eq!(
        paths(c, C::Junk),
        [
            "stray/readme.txt",
            "stray/archive.rar",
            "stray/Unrelated.Film.2010.srt"
        ]
        .iter()
        .map(|p| pc(p))
        .collect()
    );

    // Media-only: informational.
    let mr = &r.media;
    let expected_media: BTreeSet<String> = [
        "movies/Bravo Movie (2021)/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
        "movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv",
        "movies/Delta Movie (2018)/Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv",
        "movies/Hotel Movie (2015)/Hotel Movie (2015).mkv",
        "movies/November Movie (2010)/November.Movie.2010.1080p.WEB.x264-GRP.mkv",
    ]
    .iter()
    .map(|p| format!("p:{mr}:{p}"))
    .collect();
    assert_eq!(paths(c, C::MediaWithoutSource), expected_media);
    assert!(
        c.issues
            .iter()
            .filter(|i| i.category == C::MediaWithoutSource)
            .all(|i| i.severity == Severity::Info
                && i.blockers.iter().any(|b| b.code == B::MediaRootDeletion))
    );

    // Symlinks FX-22, FX-23, FX-24, never followed; no undeclared path anywhere.
    assert_eq!(paths(c, C::Symlink).len(), 3);
    let json = serde_json::to_string(&c.issues).unwrap();
    // (`link-to-outside.txt` is a legitimate completed-root file name; the undeclared sibling's
    // own paths and the link targets must never appear.)
    for leaked in [
        "outside/",
        "../",
        "golf-external-link",
        "notes.txt",
        "Hotel.Movie.2015.1080p",
    ] {
        assert!(!json.contains(leaked), "INV-09: {leaked} reached an issue");
    }

    // Permissions: only FX-33's directory, no owner mismatch.
    let perms: Vec<_> = c
        .issues
        .iter()
        .filter(|i| i.category == C::Permission)
        .collect();
    assert_eq!(
        perms.len(),
        1,
        "{:?}",
        perms.iter().map(|p| p.subject.as_str()).collect::<Vec<_>>()
    );
    assert!(
        perms[0]
            .subject
            .as_str()
            .ends_with("movies/November Movie (2010)")
    );
    assert_eq!(perms[0].severity, Severity::Critical);

    // Torrents: missing files, protected, unmapped, stalled and incomplete orphan.
    assert_eq!(
        paths(c, C::TorrentMissingFiles),
        [tid('b')].into_iter().collect()
    );
    let protected = paths(c, C::TorrentProtected);
    assert!(
        protected.contains(&tid('7')) && protected.contains(&tid('b')),
        "checkingUP and missingFiles are protected"
    );
    assert!(!protected.contains(&tid('2')) && !protected.contains(&tid('d')));
    assert_eq!(
        paths(c, C::UnmappedTorrent),
        [tid('c')].into_iter().collect()
    );
    assert_eq!(
        one(c, C::UnmappedTorrent, &tid('c')).severity,
        Severity::Critical
    );
    assert_eq!(
        paths(c, C::IncompleteStalled),
        [tid('d')].into_iter().collect()
    );
    assert_eq!(
        paths(c, C::IncompleteOrphan),
        ["p:incomplete:abandoned.partial.mkv".to_string()]
            .into_iter()
            .collect()
    );

    // The unmapped T_Q blocks torrent-less deletions (OQ-05/OQ-07) everywhere.
    for i in c.issues.iter().filter(|i| {
        matches!(
            i.category,
            C::OrphanCompleted | C::Junk | C::IncompleteOrphan
        )
    }) {
        assert!(
            i.blockers.iter().any(|b| b.code == B::UnmappedTorrents),
            "{}",
            i.subject.as_str()
        );
    }
}

#[test]
fn recent_age_and_deterministic_ids_survive_a_rescan() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let a = classify_fixture(&fx, 4, owner, |_| {});
    let b = classify_fixture(&fx, 4, owner, |_| {});
    assert_eq!(a.c.issues, b.c.issues);
    let echo = one(
        &a.c,
        C::OrphanCompleted,
        &pc("Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv"),
    );
    assert_eq!(echo.id, echo.expected_id());
}

#[test]
fn v2_an_unreadable_directory_is_a_scan_error_and_blocks_automation() {
    let fx = ParityFixture::build().unwrap();
    let dir = fx.apply_v2().unwrap();
    if std::fs::read_dir(&dir).is_ok() {
        return; // permission errors cannot be produced as root
    }
    let owner = fx.creator_ids().unwrap();
    let r = classify_fixture(&fx, 4, owner, |_| {});
    let p = r.c.perimeter.root(&completed()).unwrap();
    assert_eq!(p.error_count, 1);
    assert!(
        r.c.perimeter
            .auto_blockers(&[completed()])
            .iter()
            .any(|b| b.code == B::ScanErrorUnder)
    );
    // The unreadable directory itself is blocked; its siblings stay manually actionable.
    let lima = dir.file_name().unwrap().to_str().unwrap().to_string();
    for i in
        r.c.issues
            .iter()
            .filter(|i| i.subject.as_str().contains(&lima))
    {
        assert!(
            i.blockers
                .iter()
                .any(|b| matches!(b.code, B::ScanErrorUnder)),
            "{}",
            i.subject.as_str()
        );
    }
    let echo = one(
        &r.c,
        C::OrphanCompleted,
        &pc("Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv"),
    );
    assert!(!echo.blockers.iter().any(|b| b.code == B::ScanErrorUnder));
}

#[test]
fn v5_a_different_expected_owner_flags_every_entry_and_changes_nothing() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.apply_v5_expected_owner().unwrap();
    let r = classify_fixture(&fx, 4, owner, |_| {});
    let flagged: BTreeSet<_> =
        r.c.issues
            .iter()
            .filter(|i| i.category == C::Permission)
            .map(|i| i.subject.as_str().to_string())
            .collect();
    for rel in [
        BRAVO,
        "Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv",
        "stray/readme.txt",
    ] {
        assert!(flagged.contains(&pc(rel)), "{rel}");
    }
    assert!(
        r.c.issues
            .iter()
            .filter(|i| i.category == C::Permission)
            .all(|i| i.blockers.iter().any(|b| b.code == B::PermissionDenied)),
        "an owner mismatch is never fixable by the application (INV-32)"
    );
}

#[test]
fn the_local_permission_rule_equals_the_fs_adapter_on_the_fixture() {
    use removarr_workflows::issues::permission_findings;
    let fx = ParityFixture::build().unwrap();
    let w = world(&fx);
    let owner = fx.creator_ids().unwrap();
    for owner in [owner, (owner.0 + 1, owner.1), (0, 0)] {
        for root in [completed(), incomplete(), w.media.clone()] {
            let inv = scan(&w, &root);
            let mut ours = permission_findings(&inv, owner);
            let mut theirs = w.fs.permission_findings(&inv, owner);
            let key =
                |f: &removarr_domain::fs::PermissionFinding| format!("{:?}", (&f.path, &f.kind));
            ours.sort_by_key(key);
            theirs.sort_by_key(key);
            assert_eq!(ours, theirs, "{root} as {owner:?}");
        }
    }
}
