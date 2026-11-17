//! Parity inventory (legacy section 7.5), hardlink proofs, block accounting (T-01, T-02) and
//! scope (INV-09): nothing outside the configured roots is visited.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;

use common::*;
use removarr_domain::fs::{EntryKind, ProtectedReason};
use removarr_domain::path::RootId;
use removarr_domain::ports::FsInventory;
use removarr_fs::inventory::{
    group_physical, logical_bytes, shared_between, unique_allocated_bytes,
};
use removarr_testkit::fixture::ParityFixture;

#[test]
fn expected_scan_inventory_of_the_parity_tree() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let completed = scan(&fs, &RootId::completed());
    let media = scan(&fs, &media_id(&fx));

    assert!(completed.finished && completed.errors.is_empty() && completed.protected.is_empty());
    assert!(media.finished && media.errors.is_empty() && media.protected.is_empty());
    assert!(completed.is_clean() && media.is_clean());

    assert_eq!(count(&completed, EntryKind::File), 21);
    assert_eq!(count(&completed, EntryKind::Symlink), 2);
    assert_eq!(count(&completed, EntryKind::Dir), 8);
    assert_eq!(logical_bytes(&completed), 636_288);
    assert_eq!(group_physical([&completed]).len(), 20);

    assert_eq!(count(&media, EntryKind::File), 8);
    assert_eq!(count(&media, EntryKind::Symlink), 1);
    assert_eq!(count(&media, EntryKind::Dir), 12);
    assert_eq!(logical_bytes(&media), 491_520);
    assert_eq!(group_physical([&media]).len(), 8);

    // Nothing from the undeclared sibling is ever listed, and symlinks are not traversed.
    for inv in [&completed, &media] {
        assert!(
            inv.entries
                .iter()
                .all(|e| !e.path.rel.as_str().starts_with("outside"))
        );
        assert!(
            inv.entries
                .iter()
                .all(|e| !e.path.rel.as_str().starts_with("escape/"))
        );
    }
    let escape = entry(&completed, "escape");
    assert_eq!(escape.sig.kind, EntryKind::Symlink);
    assert_eq!(escape.symlink_target.as_deref(), Some("../outside"));
    assert_eq!(escape.allocated, 0);
    let root_sig = completed.root_sig.unwrap();
    assert_eq!(root_sig.kind, EntryKind::Dir);
}

#[test]
fn t01_real_hardlinks_are_one_physical_identity_and_blocks_count_once() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let completed = scan(&fs, &RootId::completed());
    let media = scan(&fs, &media_id(&fx));

    // Alpha, Foxtrot S01E01 and Kilo main file are real hardlinks between the trees.
    let shared: BTreeSet<_> = shared_between(&completed, &media).into_iter().collect();
    assert_eq!(shared.len(), 3);
    let alpha_c = entry(
        &completed,
        "Alpha.Movie.2020.1080p.WEB.x264-GRP/Alpha.Movie.2020.1080p.WEB.x264-GRP.mkv",
    );
    let alpha_m = entry(&media, "movies/Alpha Movie (2020)/Alpha Movie (2020).mkv");
    assert_eq!(alpha_c.sig.phys(), alpha_m.sig.phys());
    assert!(shared.contains(&alpha_c.sig.phys()));
    assert_eq!(alpha_c.sig.nlink, 2);

    // Unique allocated bytes of both trees: 25 distinct inodes, each counted once.
    let both = group_physical([&completed, &media]);
    assert_eq!(both.len(), 25);
    let mut expected = 0u64;
    // Reference computed independently with std::fs on one path per inode.
    let mut seen = BTreeSet::new();
    for e in completed.entries.iter().chain(&media.entries) {
        if e.sig.kind != EntryKind::File || !seen.insert(e.sig.phys()) {
            continue;
        }
        let root = if e.path.root == RootId::completed() {
            fx.completed()
        } else {
            fx.medias()
        };
        let md = std::fs::symlink_metadata(root.join(e.path.rel.as_str())).unwrap();
        expected += md.blocks() * 512;
    }
    assert_eq!(unique_allocated_bytes([&completed, &media]), expected);

    // Naive per-path accounting would overcount the three shared inodes.
    let naive: u64 = completed
        .entries
        .iter()
        .chain(&media.entries)
        .map(|e| e.allocated)
        .sum();
    // Overcount = allocated bytes of every extra path of a multi-path inode (3 cross-tree
    // hardlinks plus Tango's second path inside completed).
    let extra: u64 = both
        .values()
        .map(|g| g.allocated * (g.paths.len() as u64 - 1))
        .sum();
    assert_eq!(naive, expected + extra);
    assert!(extra > 0);
    assert_eq!(both.values().filter(|g| g.paths.len() > 1).count(), 4);

    // Tango (FX-34/35): one inode, two paths inside the same tree.
    let tango = group_physical([&completed])
        .into_values()
        .find(|g| g.paths.len() == 2 && g.paths[0].rel.as_str().contains("Tango"))
        .expect("tango group");
    assert_eq!(tango.paths.len(), 2);
}

#[test]
fn t02_nlink_two_with_a_link_outside_the_trees_is_not_a_shared_inode() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let completed = scan(&fs, &RootId::completed());
    let media = scan(&fs, &media_id(&fx));

    let golf = entry(&completed, "Golf.Movie.2016.1080p.WEB.x264-GRP.mkv");
    assert_eq!(
        golf.sig.nlink, 2,
        "nlink is observed, but it is not a proof"
    );
    let shared: BTreeSet<_> = shared_between(&completed, &media).into_iter().collect();
    assert!(!shared.contains(&golf.sig.phys()));

    // Hotel: media-only file whose second link lives in the undeclared sibling.
    let hotel = entry(&media, "movies/Hotel Movie (2015)/Hotel Movie (2015).mkv");
    assert_eq!(hotel.sig.nlink, 2);
    assert!(!shared.contains(&hotel.sig.phys()));
    assert_eq!(shared.len(), 3);

    // A symlink is not a hardlink: Juliet's library entry has its own identity.
    let juliet_link = entry(
        &media,
        "movies/Juliet Movie (2013)/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv",
    );
    assert_eq!(juliet_link.sig.kind, EntryKind::Symlink);
    let juliet = entry(&completed, "Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv");
    assert_ne!(juliet.sig.phys(), juliet_link.sig.phys());
}

#[test]
fn unconfigured_sibling_is_never_opened() {
    let fx = ParityFixture::build().unwrap();
    if !is_root_user() {
        // If the scanner ever opened or listed it, this would show up as a permission error.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(fx.outside(), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let fs = fs_for(&fixture_roots(&fx));
    for root in [RootId::completed(), RootId::incomplete(), media_id(&fx)] {
        let inv = scan(&fs, &root);
        assert!(inv.errors.is_empty(), "{root}: {:?}", inv.errors);
        assert!(inv.is_clean());
    }
    // A root that was not configured is refused outright.
    let other = RootId::media("/definitely/not/configured");
    let err = fs.open_root(&other).unwrap_err();
    assert_eq!(err.class, removarr_domain::error::ErrorClass::Confinement);
    let err = fs.scan_root(&other, &Default::default()).unwrap_err();
    assert_eq!(err.class, removarr_domain::error::ErrorClass::Confinement);
}

#[test]
fn qbit_internals_are_protected_and_never_listed() {
    let fx = ParityFixture::build().unwrap();
    fx.add_qbit_internals(&fx.completed()).unwrap();
    // A directory that holds resume files without the conventional name.
    let odd = fx.completed().join("resume-store");
    std::fs::create_dir_all(&odd).unwrap();
    std::fs::write(odd.join("abc.fastresume"), b"x").unwrap();
    std::fs::write(odd.join("other.bin"), b"x").unwrap();

    let fs = fs_for(&fixture_roots(&fx));
    let inv = scan(&fs, &RootId::completed());
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().starts_with("BT_backup/"))
    );
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().starts_with("resume-store/"))
    );
    let zones: Vec<_> = inv
        .protected
        .iter()
        .map(|z| (z.path.rel.as_str().to_string(), z.reason))
        .collect();
    assert!(zones.contains(&("BT_backup".into(), ProtectedReason::QbitInternal)));
    assert!(zones.contains(&("resume-store".into(), ProtectedReason::QbitInternal)));
    // Expected zones never degrade the scan itself (architecture 7.1 step 8).
    assert!(inv.errors.is_empty());
    assert!(inv.is_clean());
    // But a unit that contains one is still reported as having an issue under it.
    assert!(inv.has_issue_under(&"BT_backup".parse_rel()));
}

#[test]
fn non_utf8_names_become_protected_zones() {
    use std::os::unix::ffi::OsStrExt;
    let fx = ParityFixture::build().unwrap();
    let name = std::ffi::OsStr::from_bytes(b"bad-\xff-name.mkv");
    if std::fs::write(fx.completed().join("stray").join(name), b"x").is_err() {
        return; // The filesystem refuses invalid UTF-8 names (for example APFS).
    }
    let fs = fs_for(&fixture_roots(&fx));
    let inv = scan(&fs, &RootId::completed());
    assert!(
        inv.protected
            .iter()
            .any(|z| z.path.rel.as_str() == "stray" && z.reason == ProtectedReason::NonUtf8Name)
    );
    assert!(!inv.is_clean());
    assert!(
        inv.entries
            .iter()
            .all(|e| e.path.rel.as_str() != "stray/bad-")
    );
    assert!(
        inv.entries
            .iter()
            .any(|e| e.path.rel.as_str() == "stray/readme.txt")
    );
}

#[test]
fn free_space_is_reported_for_a_root() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    assert!(fs.free_space(&RootId::completed()).unwrap() > 0);
}

trait ParseRel {
    fn parse_rel(&self) -> removarr_domain::path::RelPath;
}
impl ParseRel for str {
    fn parse_rel(&self) -> removarr_domain::path::RelPath {
        removarr_domain::path::RelPath::new(self).unwrap()
    }
}
