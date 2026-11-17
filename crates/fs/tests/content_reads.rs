//! Bounded content reads: samples (eliminate only), full comparison, signature re-checks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{CompareOutcome, SampleSpec};
use removarr_domain::path::RootId;
use removarr_domain::ports::{ContentReader, FsInventory, ScanControl};
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_testkit::fixture::ParityFixture;

const C_MAIN: &str =
    "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv";
const C_COPY: &str = "movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv";
const SPEC: SampleSpec = SampleSpec {
    size: 4096,
    count: 5,
};

fn pair(
    fx: &ParityFixture,
    fs: &ConfinedFs,
) -> (
    removarr_domain::path::RootedPath,
    removarr_domain::path::RootedPath,
    removarr_domain::fs::EntrySignature,
    removarr_domain::fs::EntrySignature,
) {
    let a = rp(&RootId::completed(), C_MAIN);
    let b = rp(&media_id(fx), C_COPY);
    let sa = fs.stat(&a).unwrap().unwrap();
    let sb = fs.stat(&b).unwrap().unwrap();
    (a, b, sa, sb)
}

#[test]
fn identical_copies_match_by_samples_and_by_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let (a, b, sa, sb) = pair(&fx, &fs);
    let ctl = ScanControl::default();
    assert!(fs.samples_match(&a, &b, [&sa, &sb], SPEC, &ctl).unwrap());
    assert_eq!(
        fs.full_compare(&a, &b, [&sa, &sb], 65536, &ctl).unwrap(),
        CompareOutcome::Identical
    );
    // Tiny chunk request is raised to the 64 KiB minimum and still correct.
    assert_eq!(
        fs.full_compare(&a, &b, [&sa, &sb], 1, &ctl).unwrap(),
        CompareOutcome::Identical
    );
}

#[test]
fn v7_byte_outside_the_sample_windows_passes_samples_but_fails_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    fx.apply_v7().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let (a, b, sa, sb) = pair(&fx, &fs);
    let ctl = ScanControl::default();
    assert!(
        fs.samples_match(&a, &b, [&sa, &sb], SPEC, &ctl).unwrap(),
        "samples only eliminate"
    );
    assert_eq!(
        fs.full_compare(&a, &b, [&sa, &sb], 65536, &ctl).unwrap(),
        CompareOutcome::Differs { offset: 10000 }
    );
}

#[test]
fn different_sizes_never_match() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let a = rp(
        &RootId::completed(),
        "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv",
    );
    let b = rp(
        &media_id(&fx),
        "movies/Delta Movie (2018)/Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv",
    );
    let (sa, sb) = (fs.stat(&a).unwrap().unwrap(), fs.stat(&b).unwrap().unwrap());
    let ctl = ScanControl::default();
    assert!(!fs.samples_match(&a, &b, [&sa, &sb], SPEC, &ctl).unwrap());
    assert!(matches!(
        fs.full_compare(&a, &b, [&sa, &sb], 65536, &ctl).unwrap(),
        CompareOutcome::Differs { .. }
    ));
}

#[test]
fn v6_file_rewritten_after_the_scan_is_reported_as_changed() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let a = rp(
        &RootId::completed(),
        "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
    );
    let b = rp(
        &media_id(&fx),
        "movies/Bravo Movie (2021)/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
    );
    let (sa, sb) = (fs.stat(&a).unwrap().unwrap(), fs.stat(&b).unwrap().unwrap());
    fx.apply_v6().unwrap();
    let ctl = ScanControl::default();
    assert_eq!(
        fs.full_compare(&a, &b, [&sa, &sb], 65536, &ctl).unwrap(),
        CompareOutcome::Changed
    );
    let err = fs
        .samples_match(&a, &b, [&sa, &sb], SPEC, &ctl)
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::StateChanged);
}

#[test]
fn symlinks_and_escaping_paths_are_never_read() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let ctl = ScanControl::default();
    let (a, _b, sa, sb) = pair(&fx, &fs);

    // Juliet's library entry is a symlink to a real file: reading it is refused.
    let link = rp(
        &media_id(&fx),
        "movies/Juliet Movie (2013)/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv",
    );
    let link_sig = fs.stat(&link).unwrap().unwrap();
    let err = fs
        .full_compare(&a, &link, [&sa, &link_sig], 65536, &ctl)
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);

    // A path through a directory symlink is refused.
    let via = rp(&RootId::completed(), "escape/notes.txt");
    let err = fs
        .samples_match(&a, &via, [&sa, &sb], SPEC, &ctl)
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
}

#[test]
fn cancellation_is_honoured_between_chunks() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let (a, b, sa, sb) = pair(&fx, &fs);
    let ctl = ScanControl::default();
    ctl.cancel.cancel();
    let err = fs
        .full_compare(&a, &b, [&sa, &sb], 65536, &ctl)
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Cancelled);
}

#[test]
fn content_reader_concurrency_is_bounded_by_the_limit() {
    let fx = ParityFixture::build().unwrap();
    let roots = fixture_roots(&fx);
    let fs = Arc::new(ConfinedFs::new(
        &roots,
        FsLimits {
            content_read_concurrency: 2,
        },
    ));
    let (a, b, sa, sb) = pair(&fx, &fs);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (fs, a, b) = (Arc::clone(&fs), a.clone(), b.clone());
            std::thread::spawn(move || {
                let ctl = ScanControl::default();
                for _ in 0..5 {
                    assert_eq!(
                        fs.full_compare(&a, &b, [&sa, &sb], 65536, &ctl).unwrap(),
                        CompareOutcome::Identical
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(fs.peak_concurrent_content_reads() <= 2);
    assert!(fs.peak_concurrent_content_reads() >= 1);
}
