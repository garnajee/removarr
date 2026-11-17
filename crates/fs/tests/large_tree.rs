//! Synthetic large tree: bounded concurrency, metadata-only reads, and measurements.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Instant;

use common::*;
use removarr_domain::fs::EntryKind;
use removarr_domain::path::RootId;
use removarr_domain::ports::{FsInventory, ScanControl};
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_testkit::fixture::{TreeShape, synthetic_tree};

/// 1 + 20 + 400 directories, 47 files each: about 20,000 files (the B-budget reference size).
const SHAPE: TreeShape = TreeShape {
    depth: 2,
    dirs_per_level: 20,
    files_per_dir: 47,
    file_size: 64,
};

fn build() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    (u64, u64),
) {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let completed = base.join("completed");
    let media = base.join("medias");
    std::fs::create_dir(&completed).unwrap();
    std::fs::create_dir(&media).unwrap();
    let counts = synthetic_tree(&completed, SHAPE).unwrap();
    (tmp, completed, media, counts)
}

#[test]
fn large_tree_is_walked_completely_with_one_walker_at_a_time() {
    let (_tmp, completed, media, (dirs, files)) = build();
    let roots = roots_for(&completed, None, &media);
    let fs = Arc::new(ConfinedFs::new(&roots, FsLimits::default()));

    let t = Instant::now();
    let inv = scan(&fs, &RootId::completed());
    let first = t.elapsed();
    assert!(inv.is_clean());
    assert_eq!(count(&inv, EntryKind::File) as u64, files);
    assert_eq!(count(&inv, EntryKind::Dir) as u64, dirs);
    eprintln!("walk of {files} files and {dirs} dirs: {first:?}");

    // Eight callers at once still produce one walk at a time, with identical results.
    let started = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let fs = Arc::clone(&fs);
            std::thread::spawn(move || {
                fs.scan_root(&RootId::completed(), &ScanControl::default())
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    eprintln!("8 callers, serialized: {:?}", started.elapsed());
    assert_eq!(fs.peak_concurrent_scans(), 1);
    assert!(
        results.iter().all(|r| r == &inv),
        "unchanged tree gives an identical inventory"
    );
}

#[test]
fn inventory_never_opens_file_contents() {
    // Unreadable files still inventory cleanly: a content read would fail with EACCES.
    if is_root_user() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let completed = base.join("completed");
    let media = base.join("medias");
    std::fs::create_dir(&completed).unwrap();
    std::fs::create_dir(&media).unwrap();
    let shape = TreeShape {
        depth: 1,
        dirs_per_level: 5,
        files_per_dir: 40,
        file_size: 16,
    };
    let (_, files) = synthetic_tree(&completed, shape).unwrap();
    let mut all = vec![completed.clone()];
    while let Some(d) = all.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                all.push(p);
            } else {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
            }
        }
    }
    let fs = fs_for(&roots_for(&completed, None, &media));
    let inv = scan(&fs, &RootId::completed());
    assert!(inv.errors.is_empty(), "{:?}", inv.errors.first());
    assert_eq!(count(&inv, EntryKind::File) as u64, files);
}

#[test]
fn unchanged_rescan_gives_identical_entries() {
    let (_tmp, completed, media, _) = build();
    let fs = fs_for(&roots_for(&completed, None, &media));
    let a = scan(&fs, &RootId::completed());
    let t = Instant::now();
    let b = scan(&fs, &RootId::completed());
    eprintln!("rescan: {:?}", t.elapsed());
    assert_eq!(a.entries, b.entries);
}
