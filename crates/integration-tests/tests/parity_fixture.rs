//! Task 0: the synthetic parity fixture of `docs/legacy-parity.md` section 7, scanned through
//! the production wiring (real SQLite, real qBittorrent connector over the loopback fake), and
//! compared with the expected legacy inventory (7.5) and the Rust expectations (7.7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use removarr_domain::fs::{EntryKind, FsEntry, RootInventory};
use removarr_domain::issue::{IssueCategory as C, Outcome};
use removarr_integration_tests::*;

struct Stats {
    files: usize,
    videos: usize,
    symlinks: usize,
    dirs: usize,
    bytes: u64,
    inodes: usize,
}

fn stats(inv: &RootInventory) -> Stats {
    let files: Vec<&FsEntry> = inv
        .entries
        .iter()
        .filter(|e| e.sig.kind == EntryKind::File)
        .collect();
    Stats {
        files: files.len(),
        videos: files
            .iter()
            .filter(|e| e.path.rel.as_str().ends_with(".mkv"))
            .count(),
        symlinks: inv
            .entries
            .iter()
            .filter(|e| e.sig.kind == EntryKind::Symlink)
            .count(),
        dirs: inv
            .entries
            .iter()
            .filter(|e| e.sig.kind == EntryKind::Dir)
            .count(),
        bytes: files.iter().map(|e| e.sig.size).sum(),
        inodes: files
            .iter()
            .map(|e| e.sig.phys())
            .collect::<BTreeSet<_>>()
            .len(),
    }
}

fn paths(rows: &[removarr_reporting::IssueRowVm]) -> BTreeSet<String> {
    rows.iter().flat_map(|r| r.paths.iter().cloned()).collect()
}

fn set<const N: usize>(v: [&str; N]) -> BTreeSet<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[tokio::test]
async fn t0_legacy_inventory_matches_on_the_parity_tree() {
    let s = Stack::parity(Options::default()).await;
    let before = world(&s.fx);
    s.scan().await;

    // 7.5 scan inventory, from the stored (SQLite) inventories.
    let c = s.inventory(&s.cfg.roots().completed().id).await;
    let m = s.inventory(&s.media_id()).await;
    let (sc, sm) = (stats(&c), stats(&m));
    assert_eq!(
        (sc.files, sc.videos, sc.symlinks, sc.dirs),
        (21, 16, 2, 8),
        "completed"
    );
    assert_eq!(
        (sc.bytes, sc.inodes),
        (636_288, 20),
        "completed bytes and inodes"
    );
    assert_eq!(
        (sm.files, sm.videos, sm.symlinks, sm.dirs),
        (8, 8, 1, 12),
        "media"
    );
    assert_eq!(
        (sm.bytes, sm.inodes),
        (491_520, 8),
        "media bytes and inodes"
    );
    assert!(c.errors.is_empty() && m.errors.is_empty() && c.finished && m.finished);
    let symlink_video = m
        .entries
        .iter()
        .find(|e| e.sig.kind == EntryKind::Symlink)
        .unwrap();
    assert!(symlink_video.path.rel.as_str().contains("Juliet"));

    // Nothing under `outside/` is inventoried, and no entry or text of the DB names it.
    for e in c.entries.iter().chain(m.entries.iter()) {
        assert!(!e.path.rel.as_str().starts_with("outside"), "{:?}", e.path);
    }
    let raw: Vec<u8> = fs::read_dir(s.fx.appdata())
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .flat_map(|e| fs::read(e.path()).unwrap())
        .collect();
    let has = |needle: &str| raw.windows(needle.len()).any(|w| w == needle.as_bytes());
    // (The literal text of the FX-22 symlink, `../../outside/notes.txt`, is stored as a display
    // string; the files of `outside/` themselves are never visited, so their names are absent.)
    for needle in ["golf-external-link", "Hotel.Movie.2015"] {
        assert!(
            !has(needle),
            "the database names an undeclared-sibling path: {needle}"
        );
    }

    // shared inodes = {I_A, I_F1, I_K}: inodes with a File path in completed and in media.
    let c_ids: BTreeMap<_, _> = c
        .entries
        .iter()
        .filter(|e| e.sig.kind == EntryKind::File)
        .map(|e| (e.sig.phys(), e))
        .collect();
    let shared: BTreeSet<String> = m
        .entries
        .iter()
        .filter(|e| e.sig.kind == EntryKind::File)
        .filter_map(|e| c_ids.get(&e.sig.phys()))
        .map(|e| e.path.rel.as_str().to_string())
        .collect();
    assert_eq!(shared.len(), 3, "{shared:?}");
    for n in [fx::ALPHA, "Foxtrot.Show.S01E01", fx::KILO] {
        assert!(
            shared.iter().any(|p| p.contains(n)),
            "{n} not shared: {shared:?}"
        );
    }

    // 7.7 hardlink defects: the 13 completed paths and their outcomes.
    let hl = s.issues(C::HardlinkMissing).await;
    let hl_paths = paths(&hl);
    assert_eq!(hl_paths.len(), 13);
    let outcome = |needle: &str| -> Outcome {
        hl.iter()
            .find(|r| r.paths.iter().any(|p| p.ends_with(needle)))
            .unwrap_or_else(|| panic!("no hardlink_missing row for {needle}"))
            .outcome
    };
    for n in [
        format!("{}.mkv", fx::BRAVO),
        format!("{}/{}.mkv", fx::CHARLIE, fx::CHARLIE),
        format!("{}.mkv", fx::NOVEMBER),
    ] {
        assert_eq!(outcome(&n), Outcome::Consolidate, "{n}");
    }
    for n in [
        "charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv",
        "kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv",
    ] {
        assert_eq!(outcome(n), Outcome::KeepSample, "{n}");
    }
    for n in [
        "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv",
        "Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv",
        "Golf.Movie.2016.1080p.WEB.x264-GRP.mkv",
        "India.Movie.2014.1080p.WEB.x264-GRP/India.Movie.2014.1080p.WEB.x264-GRP.mkv",
        "Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv",
        "Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv",
        "tango-dupe/Tango.Movie.2007.1080p.WEB.x264-GRP.mkv",
        "completed:Tango.Movie.2007.1080p.WEB.x264-GRP.mkv",
    ] {
        assert_eq!(outcome(n), Outcome::KeepManual, "{n}");
    }
    // No healthy hardlink is reported as a defect (FX-01, FX-13, FX-26).
    for healthy in [
        "Alpha.Movie.2020",
        "Foxtrot.Show.S01E01",
        "Kilo.Movie.2012.1080p.BluRay.x264-GRP/Kilo",
    ] {
        assert!(!hl_paths.iter().any(|p| p.contains(healthy)), "{healthy}");
    }
    // FX-33 (November): the target directory is not writable by the expected owner.
    let nov = hl
        .iter()
        .find(|r| r.paths[0].ends_with(&format!("{}.mkv", fx::NOVEMBER)))
        .unwrap();
    assert!(
        nov.blockers.iter().any(|b| b == "PermissionDenied"),
        "{:?}",
        nov.blockers
    );

    // 7.7 real duplicates: exactly the three proven pairs.
    let dup = s.issues(C::Duplicate).await;
    assert_eq!(dup.len(), 3);
    for (a, b) in [
        (format!("{}.mkv", fx::BRAVO), "Bravo Movie (2021)"),
        (
            format!("{}/{}.mkv", fx::CHARLIE, fx::CHARLIE),
            "Charlie Movie (2019)",
        ),
        (format!("{}.mkv", fx::NOVEMBER), "November Movie (2010)"),
    ] {
        assert!(
            dup.iter().any(|r| r.paths.iter().any(|p| p.ends_with(&a))
                && r.paths.iter().any(|p| p.contains(b))),
            "duplicate pair {a} / {b}"
        );
    }
    // The Delta near-duplicate (other release, different size) is never a proven duplicate.
    assert!(!paths(&dup).iter().any(|p| p.contains("Delta")));

    // 7.7 content rows: FX-04 is one row with two torrents.
    let bravo = hl
        .iter()
        .find(|r| r.paths[0].ends_with(&format!("{}.mkv", fx::BRAVO)))
        .unwrap();
    assert_eq!(bravo.torrent_count, 2);

    // 7.7 orphans, junk, media-only, symlinks, permissions.
    let orphans = paths(&s.issues(C::OrphanCompleted).await);
    for p in [
        "completed:Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv",
        "completed:Golf.Movie.2016.1080p.WEB.x264-GRP.mkv",
        "completed:India.Movie.2014.1080p.WEB.x264-GRP",
        "completed:India.Movie.2014.1080p.WEB.x264-GRP/link-to-outside.txt",
        "completed:tango-dupe",
        "completed:tango-dupe/Tango.Movie.2007.1080p.WEB.x264-GRP.mkv",
    ] {
        assert!(orphans.contains(p), "orphan {p}: {orphans:?}");
    }
    assert!(
        !orphans
            .iter()
            .any(|p| p.contains("Alpha") || p.contains("Foxtrot") || p.contains(".nfo"))
    );
    let junk = paths(&s.issues(C::Junk).await);
    assert_eq!(
        junk,
        set([
            "completed:stray/readme.txt",
            "completed:stray/archive.rar",
            "completed:stray/Unrelated.Film.2010.srt"
        ]),
        "FX-02 and FX-08 are inside torrents, FX-07 and FX-27 are samples: not junk"
    );
    assert_eq!(
        paths(&s.issues(C::MediaWithoutSource).await),
        set([
            &s.m("movies/Bravo Movie (2021)/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv") as &str,
            &s.m("movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv"),
            &s.m("movies/Delta Movie (2018)/Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv"),
            &s.m("movies/Hotel Movie (2015)/Hotel Movie (2015).mkv"),
            &s.m("movies/November Movie (2010)/November.Movie.2010.1080p.WEB.x264-GRP.mkv"),
        ])
    );
    for r in s.issues(C::MediaWithoutSource).await {
        assert_eq!(
            r.outcome,
            Outcome::KeepManual,
            "media-only items are informational"
        );
        assert!(r.blockers.iter().any(|b| b == "MediaRootDeletion"));
    }
    assert_eq!(
        paths(&s.issues(C::Symlink).await),
        set([
            "completed:escape",
            "completed:India.Movie.2014.1080p.WEB.x264-GRP/link-to-outside.txt",
            &s.m("movies/Juliet Movie (2013)/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv"),
        ])
    );
    assert_eq!(
        paths(&s.issues(C::Permission).await),
        set([&s.m("movies/November Movie (2010)") as &str])
    );

    // 7.7 qBittorrent protection, out-of-scope torrent, missing files, incomplete.
    let hashes = |rows: Vec<removarr_reporting::IssueRowVm>| -> BTreeSet<String> {
        rows.into_iter()
            .flat_map(|r| r.torrents.into_iter().map(|t| t.hash))
            .collect()
    };
    let detailed = |cat: C| {
        let s = &s;
        async move {
            s.app
                .state
                .read
                .issues(&removarr_reporting::IssueQuery {
                    category: Some(cat),
                    limit: 1000,
                    include_detail: true,
                    ..Default::default()
                })
                .await
                .unwrap()
                .rows
        }
    };
    assert_eq!(
        hashes(detailed(C::TorrentProtected).await),
        [hash('3'), hash('7'), hash('b')].into_iter().collect(),
        "checkingUP and missingFiles are protected; pausedUP (a 4.x state name) is not in the \
         5.x allowlist, so under the v5.0.4 fake T_B2 fails closed (documented deviation from 7.7, \
         which assumes the 4.x allowlist; see the V8 test). stalledDL T_O is incomplete, not here."
    );
    assert!(
        bravo.blockers.iter().any(|b| b == "TorrentProtectedState"),
        "the Bravo consolidation is blocked by T_B2's state on a 5.x server: {:?}",
        bravo.blockers
    );
    assert_eq!(
        hashes(detailed(C::UnmappedTorrent).await),
        [hash('c')].into_iter().collect()
    );
    assert_eq!(
        hashes(detailed(C::TorrentMissingFiles).await),
        [hash('b')].into_iter().collect()
    );
    let stalled = detailed(C::IncompleteStalled).await;
    assert_eq!(stalled.len(), 1);
    assert_eq!(hashes(stalled), [hash('d')].into_iter().collect());
    assert_eq!(
        paths(&s.issues(C::IncompleteOrphan).await),
        set(["incomplete:abandoned.partial.mkv"])
    );

    // Scanning and reading mutate nothing: tree identical, no mutating call on any fake.
    assert_eq!(world(&s.fx), before);
    assert_eq!(s.qbit.mutation_count(), 0);
}

#[tokio::test]
async fn permission_anomaly_is_restored_and_the_fixture_is_stamped_old() {
    let s = Stack::parity(Options::default()).await;
    let parent = s.p("medias/movies/November Movie (2010)");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&parent).unwrap().permissions())
            & 0o777;
    assert_eq!(mode, 0o555);
    // Real hardlinks and copies.
    assert_eq!(
        ino(&s.p(
            "completed/Alpha.Movie.2020.1080p.WEB.x264-GRP/Alpha.Movie.2020.1080p.WEB.x264-GRP.mkv"
        )),
        ino(&s.p("medias/movies/Alpha Movie (2020)/Alpha Movie (2020).mkv"))
    );
    assert_ne!(
        ino(&s.p(&fx::bravo_completed())),
        ino(&s.p(&fx::bravo_media()))
    );
    assert_eq!(
        fs::read(s.p(&fx::bravo_completed())).unwrap(),
        fs::read(s.p(&fx::bravo_media())).unwrap()
    );
    let _ = Path::new("");
}
