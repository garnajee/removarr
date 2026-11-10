//! ENOENT races, permission failures, incomplete subtree propagation, cancellation, progress.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::*;
use removarr_domain::fs::{EntryKind, IoKind};
use removarr_domain::path::{RelPath, RootId};
use removarr_domain::ports::{FsInventory, ScanControl};
use removarr_testkit::fixture::ParityFixture;

fn rel(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

#[test]
fn enoent_during_the_scan_is_recorded_and_the_subtree_is_incomplete() {
    let fx = ParityFixture::build().unwrap();
    let stray = fx.completed().join("stray");
    let gone_file = fx
        .completed()
        .join("Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv");
    let gone_dir = fx.completed().join("tango-dupe");
    let hook_paths = (gone_file.clone(), gone_dir.clone(), stray.clone());
    let fs = fs_for(&fixture_roots(&fx)).with_dir_hook(Arc::new(move |r: &RelPath| {
        // Deletions happen between the directory listing and the stat of its entries.
        if r.is_root() {
            let _ = std::fs::remove_file(&hook_paths.0);
            let _ = std::fs::remove_dir_all(&hook_paths.1);
        }
        if r.as_str() == "stray" {
            let _ = std::fs::remove_file(hook_paths.2.join("archive.rar"));
        }
    }));
    let inv = scan(&fs, &RootId::completed());

    assert!(inv.finished, "a race does not abort the walk");
    assert!(!inv.is_clean());
    let missing: Vec<_> = inv
        .errors
        .iter()
        .filter(|e| e.io == IoKind::NotFound)
        .map(|e| e.path.rel.as_str().to_string())
        .collect();
    assert!(missing.contains(&"Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv".to_string()));
    assert!(missing.contains(&"tango-dupe".to_string()));
    assert!(missing.contains(&"stray/archive.rar".to_string()));

    // The affected subtrees are incomplete; unrelated ones are not.
    assert!(inv.has_issue_under(&rel("tango-dupe")));
    assert!(inv.has_issue_under(&rel("stray")));
    assert!(!inv.has_issue_under(&rel("Alpha.Movie.2020.1080p.WEB.x264-GRP")));
    assert!(!inv.has_issue_under(&rel("Kilo.Movie.2012.1080p.BluRay.x264-GRP")));
    // The surviving entries of the same directory are still inventoried.
    entry(&inv, "stray/readme.txt");
}

#[test]
fn unreadable_directory_is_one_error_and_only_its_subtree_is_incomplete() {
    if is_root_user() {
        return; // root ignores mode bits, the failure cannot be produced
    }
    let fx = ParityFixture::build().unwrap();
    let lima = fx.apply_v2().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let inv = scan(&fs, &RootId::completed());

    assert!(inv.finished);
    assert!(!inv.is_clean());
    assert_eq!(inv.errors.len(), 1, "{:?}", inv.errors);
    let err = &inv.errors[0];
    assert_eq!(err.io, IoKind::PermissionDenied);
    assert_eq!(
        err.path.rel.as_str(),
        lima.file_name().unwrap().to_str().unwrap()
    );
    assert!(inv.has_issue_under(&err.path.rel));
    assert!(inv.has_issue_under(&rel(&format!("{}/x.mkv", err.path.rel))));
    assert!(!inv.has_issue_under(&rel("stray")));
    // The directory entry itself was inventoried (we could stat it), its content was not.
    assert_eq!(entry(&inv, err.path.rel.as_str()).sig.kind, EntryKind::Dir);
    // Everything else is intact: 21 files + 8 + 1 new directory.
    assert_eq!(count(&inv, EntryKind::File), 21);
    assert_eq!(count(&inv, EntryKind::Dir), 9);
    // The error is attached to the path, never silently dropped, and nothing was changed.
    let mode = std::fs::metadata(&lima).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o000);
}

#[test]
fn cancellation_stops_at_a_directory_checkpoint_and_marks_the_inventory_unfinished() {
    let fx = ParityFixture::build().unwrap();
    let ctl = ScanControl::default();
    let cancel = ctl.cancel.clone();
    let fs = fs_for(&fixture_roots(&fx)).with_dir_hook(Arc::new(move |r: &RelPath| {
        if r.is_root() {
            cancel.cancel();
        }
    }));
    let inv = fs.scan_root(&RootId::completed(), &ctl).unwrap();
    assert!(!inv.finished);
    assert!(
        !inv.is_clean(),
        "an unfinished inventory must never feed a mutation"
    );
    // The root listing was completed, deeper directories were not visited.
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().contains('/'))
    );

    // Cancelled before the first directory: nothing is read at all.
    let pre = ScanControl::default();
    pre.cancel.cancel();
    let fs2 = fs_for(&fixture_roots(&fx));
    let inv = fs2.scan_root(&RootId::completed(), &pre).unwrap();
    assert!(!inv.finished);
    assert!(inv.entries.is_empty());
    assert_eq!(pre.progress.load(Ordering::Relaxed), 0);
}

#[test]
fn progress_counts_examined_entries_in_memory() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let ctl = ScanControl::default();
    let inv = fs.scan_root(&RootId::completed(), &ctl).unwrap();
    assert_eq!(
        ctl.progress.load(Ordering::Relaxed),
        inv.entries.len() as u64
    );
}
