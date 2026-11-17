//! Pass 3: symlink refusal, root and sibling confinement, and unique block accounting with
//! links that live outside every configured root. Own temporary trees, no fixed paths.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::EntryKind;
use removarr_domain::path::RootId;
use removarr_domain::ports::FsInventory;
use removarr_fs::inventory::{group_physical, shared_between, unique_allocated_bytes};

struct Tree {
    _tmp: tempfile::TempDir,
    base: PathBuf,
}

impl Tree {
    fn new(dirs: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for d in dirs {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Self { _tmp: tmp, base }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }
    fn roots(&self) -> removarr_config::Roots {
        roots_for(&self.p("completed"), None, &self.p("media"))
    }
    fn media_id(&self) -> RootId {
        RootId::media(self.p("media").to_str().unwrap())
    }
}

fn blocks(path: &std::path::Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

#[test]
fn symlink_loops_and_directory_links_are_recorded_and_never_entered() {
    let t = Tree::new(&["completed/real", "media", "elsewhere"]);
    fs::write(t.p("completed/real/a.mkv"), vec![1u8; 8192]).unwrap();
    fs::write(t.p("elsewhere/secret.mkv"), b"x").unwrap();
    symlink(".", t.p("completed/loop")).unwrap();
    symlink("..", t.p("completed/up")).unwrap();
    symlink(t.p("elsewhere"), t.p("completed/out")).unwrap();
    symlink(t.p("media"), t.p("completed/real/media-link")).unwrap();
    symlink("loop/loop/a.mkv", t.p("completed/dangling")).unwrap();

    let fs_ = fs_for(&t.roots());
    let inv = scan(&fs_, &RootId::completed());
    assert!(inv.errors.is_empty(), "{:?}", inv.errors);
    assert!(inv.is_clean());
    for link in ["loop", "up", "out", "real/media-link", "dangling"] {
        assert_eq!(entry(&inv, link).sig.kind, EntryKind::Symlink, "{link}");
        assert_eq!(entry(&inv, link).allocated, 0, "{link}");
    }
    // Nothing is reported below a link, and nothing from outside the root is listed.
    assert!(
        inv.entries
            .iter()
            .all(|e| !["loop/", "up/", "out/", "real/media-link/"]
                .iter()
                .any(|pre| e.path.rel.as_str().starts_with(pre))),
        "{:?}",
        inv.entries
            .iter()
            .map(|e| e.path.rel.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().contains("secret"))
    );
    assert_eq!(
        inv.entries.len(),
        7,
        "real, real/a.mkv, real/media-link, 4 links"
    );
}

#[test]
fn a_symlink_never_adds_blocks_or_an_identity_to_its_target() {
    let t = Tree::new(&["completed", "media"]);
    fs::write(t.p("completed/a.mkv"), vec![7u8; 16384]).unwrap();
    symlink("a.mkv", t.p("completed/a-link.mkv")).unwrap();
    symlink(t.p("completed/a.mkv"), t.p("media/lib.mkv")).unwrap();

    let fs_ = fs_for(&t.roots());
    let c = scan(&fs_, &RootId::completed());
    let m = scan(&fs_, &t.media_id());
    let a = entry(&c, "a.mkv");
    assert_eq!(a.sig.nlink, 1, "a symlink is not a link to the inode");
    let groups = group_physical([&c, &m]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[&a.sig.phys()].paths.len(), 1);
    assert!(shared_between(&c, &m).is_empty());
    assert_eq!(
        unique_allocated_bytes([&c, &m]),
        blocks(&t.p("completed/a.mkv"))
    );
}

#[test]
fn links_outside_every_root_do_not_change_unique_accounting_or_sharing() {
    let t = Tree::new(&["completed", "media", "elsewhere"]);
    fs::write(t.p("completed/solo.mkv"), vec![1u8; 20_000]).unwrap();
    fs::write(t.p("completed/both.mkv"), vec![2u8; 20_000]).unwrap();
    // solo: second link outside; both: link in media plus a third link outside.
    fs::hard_link(t.p("completed/solo.mkv"), t.p("elsewhere/solo-copy.mkv")).unwrap();
    fs::hard_link(t.p("completed/both.mkv"), t.p("media/both.mkv")).unwrap();
    fs::hard_link(t.p("completed/both.mkv"), t.p("elsewhere/both-copy.mkv")).unwrap();
    // A file in the unconfigured sibling must never be touched by a scan.
    fs::set_permissions(t.p("elsewhere"), fs::Permissions::from_mode(0o000)).unwrap();

    let fs_ = fs_for(&t.roots());
    let c = scan(&fs_, &RootId::completed());
    let m = scan(&fs_, &t.media_id());
    fs::set_permissions(t.p("elsewhere"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(c.errors.is_empty() && m.errors.is_empty());
    let solo = entry(&c, "solo.mkv");
    let both = entry(&c, "both.mkv");
    assert_eq!(solo.sig.nlink, 2, "nlink counts the outside link");
    assert_eq!(both.sig.nlink, 3);

    // Only the inode that has paths in both configured trees is shared.
    assert_eq!(shared_between(&c, &m), vec![both.sig.phys()]);
    let groups = group_physical([&c, &m]);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[&solo.sig.phys()].paths.len(), 1);
    assert_eq!(groups[&both.sig.phys()].paths.len(), 2);

    // Unique blocks: each inode once, independently measured with std.
    let expected = blocks(&t.p("completed/solo.mkv")) + blocks(&t.p("completed/both.mkv"));
    assert_eq!(unique_allocated_bytes([&c, &m]), expected);
    let naive: u64 = c
        .entries
        .iter()
        .chain(&m.entries)
        .map(|e| e.allocated)
        .sum();
    assert_eq!(naive, expected + blocks(&t.p("completed/both.mkv")));
}

#[test]
fn sibling_directory_sharing_a_root_name_prefix_is_never_opened() {
    let t = Tree::new(&["completed", "completed-old/deep", "media", "media2"]);
    fs::write(t.p("completed/a.mkv"), b"a").unwrap();
    fs::write(t.p("completed-old/deep/b.mkv"), b"b").unwrap();
    for d in ["completed-old", "media2"] {
        fs::set_permissions(t.p(d), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let fs_ = fs_for(&t.roots());
    let c = scan(&fs_, &RootId::completed());
    let m = scan(&fs_, &t.media_id());
    for d in ["completed-old", "media2"] {
        fs::set_permissions(t.p(d), fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(
        c.is_clean() && m.is_clean(),
        "{:?} {:?}",
        c.errors,
        m.errors
    );
    assert_eq!(c.entries.len(), 1);
    assert!(m.entries.is_empty());
    // The sibling is not a configured root, even though its name starts with one.
    let sibling = RootId::media(t.p("completed-old").to_str().unwrap());
    assert_eq!(
        fs_.scan_root(&sibling, &Default::default())
            .unwrap_err()
            .class,
        ErrorClass::Confinement
    );
}

#[test]
fn stat_never_follows_a_final_or_intermediate_symlink_in_any_root() {
    let t = Tree::new(&["completed", "media/lib", "elsewhere"]);
    fs::write(t.p("elsewhere/target.mkv"), b"x").unwrap();
    fs::set_permissions(
        t.p("elsewhere/target.mkv"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    symlink(t.p("elsewhere"), t.p("media/lib/dir")).unwrap();
    symlink(t.p("elsewhere/target.mkv"), t.p("media/lib/file.mkv")).unwrap();
    let fs_ = fs_for(&t.roots());
    let m = t.media_id();

    let last = fs_.stat(&rp(&m, "lib/file.mkv")).unwrap().unwrap();
    assert_eq!(last.kind, EntryKind::Symlink);
    let via = fs_.stat(&rp(&m, "lib/dir/target.mkv")).unwrap_err();
    assert_eq!(via.class, ErrorClass::Confinement);
    assert!(fs_.snapshot_dir(&rp(&m, "lib/dir")).is_err());
    // The scan sees both links as links and lists nothing from the target directory.
    let inv = scan(&fs_, &m);
    assert!(
        inv.entries
            .iter()
            .all(|e| !e.path.rel.as_str().contains("target"))
    );
    fs::set_permissions(
        t.p("elsewhere/target.mkv"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

#[test]
fn special_files_are_recorded_without_being_opened() {
    let t = Tree::new(&["completed", "media"]);
    let fifo = t.p("completed/pipe");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status();
    if !status.is_ok_and(|s| s.success()) {
        eprintln!("mkfifo unavailable, skipping");
        return;
    }
    // Opening a FIFO for reading would block forever; the scan must only lstat it.
    let inv = scan(&fs_for(&t.roots()), &RootId::completed());
    assert!(inv.is_clean());
    let e = entry(&inv, "pipe");
    assert_eq!(e.sig.kind, EntryKind::Other);
    assert_eq!(e.allocated, 0);
    assert!(group_physical([&inv]).is_empty(), "not a regular file");
}
