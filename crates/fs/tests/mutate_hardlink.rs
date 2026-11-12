//! `LiveFsMutator` on temporary directories: probe, atomic replacement, refusals. Nothing here
//! touches anything outside `tempfile` directories.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::{fs_for, roots_for, rp};
use removarr_config::{LiveMode, Roots};
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{EntrySignature, IoKind};
use removarr_domain::path::RootId;
use removarr_domain::ports::{FsInventory, FsMutator};
use removarr_fs::LiveFsMutator;
use removarr_fs::mutate::{CROSS_DEVICE_HINT, HARDLINK_REFUSED_HINT};
use tempfile::TempDir;

struct Lab {
    _tmp: TempDir,
    roots: Roots,
    media: RootId,
}

fn lab() -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let completed = base.join("completed");
    let media = base.join("media");
    fs::create_dir_all(completed.join("Rel")).unwrap();
    fs::create_dir_all(media.join("Movies/Rel")).unwrap();
    fs::write(completed.join("Rel/a.mkv"), b"same bytes here").unwrap();
    fs::write(media.join("Movies/Rel/a.mkv"), b"same bytes here").unwrap();
    let roots = roots_for(&completed, None, &media);
    let media_id = RootId::media(media.to_str().unwrap());
    Lab {
        _tmp: tmp,
        roots,
        media: media_id,
    }
}

fn sig(lab: &Lab, root: &RootId, rel: &str) -> EntrySignature {
    fs_for(&lab.roots)
        .stat(&rp(root, rel))
        .unwrap()
        .expect("exists")
}

fn mutator(lab: &Lab) -> LiveFsMutator {
    LiveFsMutator::new(&lab.roots, &LiveMode::for_tests())
}

fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn t13_replacement_links_completed_toward_media_and_verifies_the_inode() {
    let lab = lab();
    let completed = RootId::completed();
    let src = rp(&completed, "Rel/a.mkv");
    let tgt = rp(&lab.media, "Movies/Rel/a.mkv");
    let (s, t) = (
        sig(&lab, &completed, "Rel/a.mkv"),
        sig(&lab, &lab.media, "Movies/Rel/a.mkv"),
    );
    assert_ne!(s.phys(), t.phys(), "two distinct inodes before");
    let m = mutator(&lab);
    m.probe_hardlink(&src, &rp(&lab.media, "Movies/Rel"))
        .expect("probe");
    let phys = m
        .replace_with_hardlink(&src, &tgt, &s, &t)
        .expect("replace");
    assert_eq!(phys, s.phys(), "the result is the source inode");
    let after_t = sig(&lab, &lab.media, "Movies/Rel/a.mkv");
    let after_s = sig(&lab, &completed, "Rel/a.mkv");
    assert_eq!(
        after_t.phys(),
        s.phys(),
        "media path now carries the completed inode"
    );
    assert_eq!(after_s.phys(), s.phys(), "completed inode unchanged");
    assert_eq!(after_s.nlink, 2);
    let media_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    assert_eq!(
        entries(&media_dir),
        vec!["a.mkv"],
        "no temporary link left behind"
    );
    let probe_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    assert_eq!(entries(&probe_dir), vec!["a.mkv"]);
}

#[test]
fn replacement_refuses_a_changed_or_missing_target_and_leaves_nothing_behind() {
    let lab = lab();
    let completed = RootId::completed();
    let src = rp(&completed, "Rel/a.mkv");
    let tgt = rp(&lab.media, "Movies/Rel/a.mkv");
    let s = sig(&lab, &completed, "Rel/a.mkv");
    let mut stale = sig(&lab, &lab.media, "Movies/Rel/a.mkv");
    stale.size += 1;
    let m = mutator(&lab);
    let e = m.replace_with_hardlink(&src, &tgt, &s, &stale).unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    let media_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    assert_eq!(entries(&media_dir), vec!["a.mkv"]);
    let t = sig(&lab, &lab.media, "Movies/Rel/a.mkv");
    assert_ne!(t.phys(), s.phys(), "target untouched");
    // Source changed: refused before any link.
    let mut stale_s = s;
    stale_s.mtime_ns.0 += 1;
    let e = m
        .replace_with_hardlink(&src, &tgt, &stale_s, &t)
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    // Missing target.
    fs::remove_file(media_dir.join("a.mkv")).unwrap();
    let e = m.replace_with_hardlink(&src, &tgt, &s, &t).unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    assert!(entries(&media_dir).is_empty());
}

#[test]
fn symlinks_are_never_followed_as_source_target_or_component() {
    let lab = lab();
    let completed = RootId::completed();
    let media_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    let completed_dir = lab.roots.completed().as_path().join("Rel");
    // Target is a symlink to the completed file.
    fs::remove_file(media_dir.join("a.mkv")).unwrap();
    std::os::unix::fs::symlink(completed_dir.join("a.mkv"), media_dir.join("a.mkv")).unwrap();
    let s = sig(&lab, &completed, "Rel/a.mkv");
    let t = sig(&lab, &lab.media, "Movies/Rel/a.mkv");
    let m = mutator(&lab);
    let e = m
        .replace_with_hardlink(
            &rp(&completed, "Rel/a.mkv"),
            &rp(&lab.media, "Movies/Rel/a.mkv"),
            &s,
            &t,
        )
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement, "{e:?}");
    assert!(
        fs::symlink_metadata(media_dir.join("a.mkv"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // Source is a symlink.
    fs::write(media_dir.join("b.mkv"), b"x").unwrap();
    std::os::unix::fs::symlink(media_dir.join("b.mkv"), completed_dir.join("link.mkv")).unwrap();
    let e = m
        .probe_hardlink(
            &rp(&completed, "Rel/link.mkv"),
            &rp(&lab.media, "Movies/Rel"),
        )
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
    // A symlinked directory component.
    std::os::unix::fs::symlink(&media_dir, lab.roots.media()[0].as_path().join("Link")).unwrap();
    let e = m
        .probe_hardlink(&rp(&completed, "Rel/a.mkv"), &rp(&lab.media, "Link"))
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
    assert_eq!(
        entries(&media_dir),
        vec!["a.mkv", "b.mkv"],
        "nothing created"
    );
    // The root itself is never a target.
    let e = m
        .replace_with_hardlink(&rp(&completed, "Rel/a.mkv"), &rp(&lab.media, ""), &s, &t)
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
}

#[test]
fn t14_probe_refusal_is_explanatory_and_never_suggests_sudo() {
    if common::is_root_user() {
        return; // root ignores directory permissions
    }
    let lab = lab();
    let completed = RootId::completed();
    let media_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    fs::set_permissions(&media_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let m = mutator(&lab);
    let e = m
        .probe_hardlink(&rp(&completed, "Rel/a.mkv"), &rp(&lab.media, "Movies/Rel"))
        .unwrap_err();
    fs::set_permissions(&media_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(e.class, ErrorClass::Io(IoKind::PermissionDenied), "{e:?}");
    assert!(e.detail.contains("protected_hardlinks"), "{}", e.detail);
    assert!(e.detail.contains("expected_owner"));
    assert!(
        !e.detail.to_ascii_lowercase().contains("sudo"),
        "{}",
        e.detail
    );
    assert!(!HARDLINK_REFUSED_HINT.contains("sudo") && !CROSS_DEVICE_HINT.contains("sudo"));
    assert_eq!(
        entries(&media_dir),
        vec!["a.mkv"],
        "no probe link left behind"
    );
    // The source bytes were never opened: a probe is metadata plus link only. Make the source
    // unreadable and probe into a writable directory.
    let src = lab.roots.completed().as_path().join("Rel/a.mkv");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
    let r = m.probe_hardlink(&rp(&completed, "Rel/a.mkv"), &rp(&lab.media, "Movies/Rel"));
    fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
    // On Linux with protected_hardlinks the link may be refused (owner still allowed); either
    // way the probe never reads content and leaves nothing behind.
    if let Err(e) = r {
        assert!(!e.detail.contains("sudo"));
    }
    assert_eq!(entries(&media_dir), vec!["a.mkv"]);
}

#[test]
fn f08_the_mutator_refuses_any_direction_but_completed_to_media() {
    let lab = lab();
    let completed = RootId::completed();
    let m = mutator(&lab);
    let media_file = rp(&lab.media, "Movies/Rel/a.mkv");
    let completed_file = rp(&completed, "Rel/a.mkv");
    let (s, t) = (
        sig(&lab, &lab.media, "Movies/Rel/a.mkv"),
        sig(&lab, &completed, "Rel/a.mkv"),
    );
    // media -> completed
    let e = m
        .replace_with_hardlink(&media_file, &completed_file, &s, &t)
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement, "{e:?}");
    let e = m
        .probe_hardlink(&media_file, &rp(&completed, "Rel"))
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
    // completed -> completed and media -> media
    let e = m
        .replace_with_hardlink(&completed_file, &rp(&completed, "Rel/b.mkv"), &t, &t)
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
    let e = m
        .probe_hardlink(&media_file, &rp(&lab.media, "Movies/Rel"))
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Confinement);
    assert_eq!(
        sig(&lab, &completed, "Rel/a.mkv").phys(),
        t.phys(),
        "nothing changed"
    );
    assert_eq!(sig(&lab, &lab.media, "Movies/Rel/a.mkv").phys(), s.phys());
    let completed_dir = lab.roots.completed().as_path().join("Rel");
    assert_eq!(
        entries(&completed_dir),
        vec!["a.mkv"],
        "no temporary link anywhere"
    );
}

#[test]
fn f13_long_names_get_a_temporary_name_within_name_max() {
    let lab = lab();
    let completed = RootId::completed();
    let long = format!("{}.mkv", "x".repeat(246)); // 250 bytes
    assert_eq!(long.len(), 250);
    let completed_dir = lab.roots.completed().as_path().join("Rel");
    let media_dir = lab.roots.media()[0].as_path().join("Movies/Rel");
    fs::write(completed_dir.join(&long), b"long name bytes").unwrap();
    fs::write(media_dir.join(&long), b"long name bytes").unwrap();
    let src = rp(&completed, &format!("Rel/{long}"));
    let tgt = rp(&lab.media, &format!("Movies/Rel/{long}"));
    let (s, t) = (
        sig(&lab, &completed, &src.rel.to_string()),
        sig(&lab, &lab.media, &tgt.rel.to_string()),
    );
    let m = mutator(&lab);
    m.probe_hardlink(&src, &rp(&lab.media, "Movies/Rel"))
        .expect("probe with a long name");
    let phys = m
        .replace_with_hardlink(&src, &tgt, &s, &t)
        .expect("replace with a long name");
    assert_eq!(phys, s.phys());
    assert_eq!(sig(&lab, &lab.media, &tgt.rel.to_string()).phys(), s.phys());
    let mut names = entries(&media_dir);
    names.retain(|n| n.starts_with('.'));
    assert!(names.is_empty(), "no temporary link left: {names:?}");
}
