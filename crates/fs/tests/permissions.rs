//! Ownership and permission anomalies. Detection only: nothing is ever changed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use removarr_domain::fs::{
    EntryKind, EntrySignature, PermissionFinding, PermissionKind, RootInventory,
};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::ports::FsInventory;
use removarr_domain::time::MtimeNs;
use removarr_fs::perm::{can_modify_dir, findings};
use removarr_testkit::fixture::ParityFixture;

fn dir_sig(uid: u32, gid: u32, mode: u32) -> EntrySignature {
    EntrySignature {
        kind: EntryKind::Dir,
        dev: 1,
        ino: 2,
        size: 0,
        nlink: 2,
        mtime_ns: MtimeNs(0),
        mode,
        uid,
        gid,
    }
}

#[test]
fn writability_uses_the_first_matching_permission_class() {
    // Expected identity 1000:1000.
    assert!(can_modify_dir(&dir_sig(1000, 1000, 0o755), 1000, 1000));
    // Root-owned 0755 (the classic wrong-owner case): cannot rename or unlink.
    assert!(!can_modify_dir(&dir_sig(0, 0, 0o755), 1000, 1000));
    // Group writable with the expected gid.
    assert!(can_modify_dir(&dir_sig(0, 1000, 0o775), 1000, 1000));
    // Owner class wins over a more permissive group class.
    assert!(!can_modify_dir(&dir_sig(1000, 1000, 0o577), 1000, 1000));
    // Write without search permission is not enough.
    assert!(!can_modify_dir(&dir_sig(1000, 1000, 0o600), 1000, 1000));
    // Other class.
    assert!(can_modify_dir(&dir_sig(5, 5, 0o777), 1000, 1000));
    assert!(!can_modify_dir(&dir_sig(5, 5, 0o775), 1000, 1000));
    // Read-only directory owned by the expected user (FX-33 parent).
    assert!(!can_modify_dir(&dir_sig(1000, 1000, 0o555), 1000, 1000));
    // The superuser is not blocked by modes.
    assert!(can_modify_dir(&dir_sig(5, 5, 0o000), 0, 0));
}

#[test]
fn base_fixture_flags_only_the_read_only_parent_of_fx33() {
    let fx = ParityFixture::build().unwrap();
    let owner = fx.creator_ids().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let media = scan(&fs, &media_id(&fx));
    let found = fs.permission_findings(&media, owner);
    let id = media_id(&fx);
    assert_eq!(
        found,
        vec![PermissionFinding {
            path: rp(&id, "movies/November Movie (2010)"),
            kind: PermissionKind::ParentNotWritable,
        }]
    );
    // Completed has no anomaly.
    let completed = scan(&fs, &RootId::completed());
    assert!(fs.permission_findings(&completed, owner).is_empty());
    // Nothing was modified by the analysis.
    let mode = std::fs::metadata(fx.medias().join("movies/November Movie (2010)")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o555
    );
}

#[test]
fn v5_wrong_expected_owner_flags_every_entry() {
    let fx = ParityFixture::build().unwrap();
    let wrong = fx.apply_v5_expected_owner().unwrap();
    let real = fx.creator_ids().unwrap();
    let fs = fs_for(&fixture_roots(&fx));
    let inv = scan(&fs, &RootId::completed());
    let found = fs.permission_findings(&inv, wrong);

    let mismatches: Vec<_> = found
        .iter()
        .filter(|f| matches!(f.kind, PermissionKind::OwnerMismatch { .. }))
        .collect();
    // Every regular file and directory plus the root; symlinks are not owner-checked.
    let expected = count(&inv, EntryKind::File) + count(&inv, EntryKind::Dir) + 1;
    assert_eq!(mismatches.len(), expected);
    assert!(mismatches.iter().all(|f| f.kind
        == PermissionKind::OwnerMismatch {
            uid: real.0,
            gid: real.1
        }));
    // Directories owned by a different user (mode 0755, other class: r-x) cannot be modified.
    if wrong.0 != 0 {
        assert!(
            found
                .iter()
                .any(|f| f.kind == PermissionKind::ParentNotWritable)
        );
    }
}

#[test]
fn root_owned_tree_is_reported_as_owner_mismatch_and_unwritable_parent() {
    // Synthetic inventory: what a root-owned directory under a 1000:1000 service looks like.
    let root = RootId::completed();
    let mk = |rel: &str, sig: EntrySignature| removarr_domain::fs::FsEntry {
        path: RootedPath::parse(root.as_str(), rel).unwrap(),
        sig,
        allocated: 0,
        symlink_target: None,
    };
    let mut file = dir_sig(0, 0, 0o644);
    file.kind = EntryKind::File;
    let inv = RootInventory {
        root: root.clone(),
        root_sig: Some(dir_sig(1000, 1000, 0o755)),
        entries: vec![
            mk("rootowned", dir_sig(0, 0, 0o755)),
            mk("rootowned/movie.mkv", file),
            mk("fine", dir_sig(1000, 1000, 0o755)),
        ],
        errors: vec![],
        protected: vec![],
        finished: true,
    };
    let found = findings(&inv, (1000, 1000));
    let at = |rel: &str| RootedPath::parse(root.as_str(), rel).unwrap();
    assert!(found.contains(&PermissionFinding {
        path: at("rootowned"),
        kind: PermissionKind::OwnerMismatch { uid: 0, gid: 0 },
    }));
    assert!(found.contains(&PermissionFinding {
        path: at("rootowned"),
        kind: PermissionKind::ParentNotWritable,
    }));
    assert!(found.contains(&PermissionFinding {
        path: at("rootowned/movie.mkv"),
        kind: PermissionKind::OwnerMismatch { uid: 0, gid: 0 },
    }));
    assert!(
        found
            .iter()
            .all(|f| f.path != at("fine") && f.path.rel.as_str() != "")
    );
}

#[test]
fn owner_file_without_owner_write_bit_is_flagged() {
    let root = RootId::completed();
    let mut file = dir_sig(1000, 1000, 0o444);
    file.kind = EntryKind::File;
    let inv = RootInventory {
        root: root.clone(),
        root_sig: Some(dir_sig(1000, 1000, 0o755)),
        entries: vec![removarr_domain::fs::FsEntry {
            path: RootedPath::parse(root.as_str(), "a.mkv").unwrap(),
            sig: file,
            allocated: 0,
            symlink_target: None,
        }],
        errors: vec![],
        protected: vec![],
        finished: true,
    };
    let found = findings(&inv, (1000, 1000));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, PermissionKind::NotWritableByOwner);
}
