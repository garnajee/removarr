//! Symlinks are never followed, symlinked components are rejected, and handles stay confined.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::symlink;
use std::sync::Arc;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{EntryKind, IoKind};
use removarr_domain::path::{RelPath, RootId};
use removarr_domain::ports::FsInventory;
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_testkit::fixture::ParityFixture;

#[test]
fn stat_reports_symlinks_but_refuses_to_cross_them() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let c = RootId::completed();

    // The link itself is visible as a symlink.
    let link = fs.stat(&rp(&c, "escape")).unwrap().unwrap();
    assert_eq!(link.kind, EntryKind::Symlink);
    let file_link = fs.stat(&rp(
        &c,
        "India.Movie.2014.1080p.WEB.x264-GRP/link-to-outside.txt",
    ));
    assert_eq!(file_link.unwrap().unwrap().kind, EntryKind::Symlink);

    // A path that goes through the directory symlink is a confinement error, not a hit.
    let err = fs.stat(&rp(&c, "escape/notes.txt")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    let err = fs.stat(&rp(&c, "escape/missing.txt")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);

    // Missing entries are None (ENOENT), also below a missing directory.
    assert_eq!(fs.stat(&rp(&c, "nope")).unwrap(), None);
    assert_eq!(fs.stat(&rp(&c, "nope/deeper/file.mkv")).unwrap(), None);
    // The root itself.
    assert_eq!(fs.stat(&rp(&c, "")).unwrap().unwrap().kind, EntryKind::Dir);
    // A file used as a directory is an error, not "absent".
    let err = fs.stat(&rp(&c, "stray/readme.txt/x")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Io(IoKind::NotADirectory));
}

#[test]
fn stat_signature_equals_scan_signature() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let inv = scan(&fs, &RootId::completed());
    for e in &inv.entries {
        let s = fs.stat(&e.path).unwrap().unwrap();
        assert_eq!(s, e.sig, "{}", e.path.rel);
    }
}

#[test]
fn snapshot_covers_the_subtree_and_refuses_symlinked_components() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let c = RootId::completed();
    let snap = fs
        .snapshot_dir(&rp(&c, "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP"))
        .unwrap();
    let keys: Vec<_> = snap.entries.keys().map(RelPath::as_str).collect();
    assert_eq!(keys.len(), 5);
    assert!(keys.contains(&""));
    assert!(keys.contains(&"Sample"));
    assert!(
        keys.iter()
            .any(|k| k.starts_with("Sample/") && k.ends_with("sample.mkv"))
    );
    assert_eq!(snap.entries[&RelPath::root()].kind, EntryKind::Dir);

    // The same inode data the scan records.
    let inv = scan(&fs, &c);
    let dir_entry = entry(&inv, "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP");
    assert_eq!(snap.entries[&RelPath::root()], dir_entry.sig);

    // A directory symlink is not a directory to snapshot.
    let err = fs.snapshot_dir(&rp(&c, "escape")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    let err = fs.snapshot_dir(&rp(&c, "escape/anything")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    // Missing directory is an error (a snapshot must prove existence).
    let err = fs.snapshot_dir(&rp(&c, "nope")).unwrap_err();
    assert_eq!(err.class, ErrorClass::Io(IoKind::NotFound));
}

#[test]
fn symlink_inside_a_snapshot_is_recorded_not_followed() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let c = RootId::completed();
    let snap = fs
        .snapshot_dir(&rp(&c, "India.Movie.2014.1080p.WEB.x264-GRP"))
        .unwrap();
    assert_eq!(snap.entries.len(), 3);
    assert_eq!(
        snap.entries[&RelPath::new("link-to-outside.txt").unwrap()].kind,
        EntryKind::Symlink
    );
}

#[test]
fn root_replaced_by_a_symlink_after_startup_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    for d in ["completed", "medias", "elsewhere"] {
        std::fs::create_dir(base.join(d)).unwrap();
    }
    std::fs::write(base.join("elsewhere/secret.txt"), b"x").unwrap();
    let roots = roots_for(&base.join("completed"), None, &base.join("medias"));
    let fs = ConfinedFs::new(&roots, FsLimits::default());
    assert!(
        fs.scan_root(&RootId::completed(), &Default::default())
            .is_ok()
    );

    std::fs::remove_dir(base.join("completed")).unwrap();
    symlink(base.join("elsewhere"), base.join("completed")).unwrap();
    let err = fs
        .scan_root(&RootId::completed(), &Default::default())
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
    let err = fs
        .stat(&rp(&RootId::completed(), "secret.txt"))
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
}

#[test]
fn symlinked_ancestor_of_a_root_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    std::fs::create_dir_all(base.join("real/library/completed")).unwrap();
    std::fs::create_dir_all(base.join("real/library/medias")).unwrap();
    std::fs::create_dir_all(base.join("attacker/completed")).unwrap();
    let roots = roots_for(
        &base.join("real/library/completed"),
        None,
        &base.join("real/library/medias"),
    );
    let fs = ConfinedFs::new(&roots, FsLimits::default());
    assert!(
        fs.scan_root(&RootId::completed(), &Default::default())
            .is_ok()
    );

    std::fs::rename(base.join("real/library"), base.join("moved")).unwrap();
    symlink(base.join("attacker"), base.join("real/library")).unwrap();
    let err = fs
        .scan_root(&RootId::completed(), &Default::default())
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Confinement);
}

#[test]
fn directory_swapped_for_a_symlink_during_the_scan_is_not_followed() {
    let fx = ParityFixture::build().unwrap();
    let outside = fx.outside();
    let victim = fx.completed().join("stray");
    let hook_victim = victim.clone();
    let hook_outside = outside.clone();
    let swapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&swapped);
    let fs = fs_for(&fixture_roots(&fx)).with_dir_hook(Arc::new(move |rel: &RelPath| {
        // Runs after the root listing, before its entries are examined.
        if rel.is_root() && !flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::rename(&hook_victim, hook_victim.with_file_name("stray-moved")).unwrap();
            symlink(&hook_outside, &hook_victim).unwrap();
        }
    }));
    let inv = scan(&fs, &RootId::completed());
    assert!(swapped.load(std::sync::atomic::Ordering::SeqCst));
    let stray = entry(&inv, "stray");
    assert_eq!(stray.sig.kind, EntryKind::Symlink);
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().starts_with("stray/"))
    );
    assert!(
        inv.entries
            .iter()
            .all(|e| e.path.rel.as_str() != "stray/notes.txt")
    );
}
