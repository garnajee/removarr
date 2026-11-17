//! Tier-3 `LiveFsMutator` operations on temporary directories: `remove_file`, `remove_tree`,
//! `remove_empty_dir`, `set_mode`. Every refusal leaves the tree untouched; nothing here
//! touches anything outside `tempfile` directories.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use common::{fs_for, is_root_user, roots_for, rp};
use removarr_config::{LiveMode, Roots};
use removarr_domain::error::ErrorClass;
use removarr_domain::fs::{DirSnapshot, EntrySignature};
use removarr_domain::path::RootId;
use removarr_domain::ports::{FsInventory, FsMutator};
use removarr_fs::{ConfinedFs, LiveFsMutator};
use tempfile::TempDir;

struct Lab {
    _tmp: TempDir,
    base: PathBuf,
    roots: Roots,
    media: RootId,
}

/// completed: `Rel/{a.mkv, a.nfo, Subs/a.srt, link -> ../outside.txt}`, `Empty/`, `Full/x`;
/// incomplete: `Dl/part.mkv.!qB`; media: `Movies/m.mkv`; outside: `outside.txt`.
fn lab() -> Lab {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let completed = base.join("completed");
    let incomplete = base.join("incomplete");
    let media = base.join("media");
    fs::create_dir_all(completed.join("Rel/Subs")).unwrap();
    fs::create_dir_all(completed.join("Empty")).unwrap();
    fs::create_dir_all(completed.join("Full")).unwrap();
    fs::create_dir_all(incomplete.join("Dl")).unwrap();
    fs::create_dir_all(media.join("Movies")).unwrap();
    fs::write(completed.join("Rel/a.mkv"), b"video").unwrap();
    fs::write(completed.join("Rel/a.nfo"), b"nfo").unwrap();
    fs::write(completed.join("Rel/Subs/a.srt"), b"srt").unwrap();
    fs::write(completed.join("Full/x"), b"x").unwrap();
    fs::write(incomplete.join("Dl/part.mkv.!qB"), b"part").unwrap();
    fs::write(media.join("Movies/m.mkv"), b"media").unwrap();
    fs::write(base.join("outside.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink("../../outside.txt", completed.join("Rel/link")).unwrap();
    let roots = roots_for(&completed, Some(&incomplete), &media);
    let media_id = RootId::media(media.to_str().unwrap());
    Lab {
        _tmp: tmp,
        base,
        roots,
        media: media_id,
    }
}

fn cfs(lab: &Lab) -> ConfinedFs {
    fs_for(&lab.roots)
}

fn sig(lab: &Lab, root: &RootId, rel: &str) -> EntrySignature {
    cfs(lab).stat(&rp(root, rel)).unwrap().expect("exists")
}

fn snap(lab: &Lab, rel: &str) -> DirSnapshot {
    cfs(lab)
        .snapshot_dir(&rp(&RootId::completed(), rel))
        .unwrap()
}

fn mutator(lab: &Lab) -> LiveFsMutator {
    LiveFsMutator::new(&lab.roots, &LiveMode::for_tests())
}

fn completed(lab: &Lab, rel: &str) -> PathBuf {
    lab.base.join("completed").join(rel)
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn remove_file_unlinks_only_an_unchanged_regular_file() {
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let p = rp(&c, "Rel/a.nfo");
    let s = sig(&lab, &c, "Rel/a.nfo");
    // A changed signature (nlink counts, CON-14) refuses before anything happens.
    fs::hard_link(completed(&lab, "Rel/a.nfo"), completed(&lab, "Rel/a.nfo.2")).unwrap();
    assert_eq!(
        m.remove_file(&p, &s).unwrap_err().class,
        ErrorClass::StateChanged
    );
    assert!(completed(&lab, "Rel/a.nfo").exists());
    fs::remove_file(completed(&lab, "Rel/a.nfo.2")).unwrap();
    let s = sig(&lab, &c, "Rel/a.nfo");
    m.remove_file(&p, &s).expect("removed");
    assert!(!completed(&lab, "Rel/a.nfo").exists());
    // Already gone is a state change, never a success (CONTEXTE 3.15).
    assert_eq!(
        m.remove_file(&p, &s).unwrap_err().class,
        ErrorClass::StateChanged
    );
    // A symlink target is a confinement refusal and its target survives.
    let link = rp(&c, "Rel/link");
    let ls = sig(&lab, &c, "Rel/link");
    assert_eq!(
        m.remove_file(&link, &ls).unwrap_err().class,
        ErrorClass::Confinement
    );
    assert!(lab.base.join("outside.txt").exists());
    assert!(completed(&lab, "Rel/link").symlink_metadata().is_ok());
    // A directory is not a regular file; the root itself is never a target.
    let d = sig(&lab, &c, "Rel");
    assert_eq!(
        m.remove_file(&rp(&c, "Rel"), &d).unwrap_err().class,
        ErrorClass::StateChanged
    );
    assert_eq!(
        m.remove_file(&rp(&c, ""), &d).unwrap_err().class,
        ErrorClass::Confinement
    );
}

#[test]
fn removals_never_touch_a_media_root() {
    let lab = lab();
    let m = mutator(&lab);
    let p = rp(&lab.media, "Movies/m.mkv");
    let s = sig(&lab, &lab.media, "Movies/m.mkv");
    assert_eq!(
        m.remove_file(&p, &s).unwrap_err().class,
        ErrorClass::Confinement
    );
    let d = rp(&lab.media, "Movies");
    let ds = sig(&lab, &lab.media, "Movies");
    assert_eq!(
        m.remove_empty_dir(&d, &ds).unwrap_err().class,
        ErrorClass::Confinement
    );
    let snapshot = cfs(&lab).snapshot_dir(&d).unwrap();
    assert_eq!(
        m.remove_tree(&d, &snapshot).unwrap_err().class,
        ErrorClass::Confinement
    );
    assert!(lab.base.join("media/Movies/m.mkv").exists());
}

#[test]
fn t17_remove_tree_removes_the_planned_subtree_and_never_follows_symlinks() {
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let dir = rp(&c, "Rel");
    let snapshot = snap(&lab, "Rel");
    assert_eq!(snapshot.entries.len(), 6, "dir, 2 files, Subs, srt, link");
    m.remove_tree(&dir, &snapshot).expect("tree removed");
    assert!(!completed(&lab, "Rel").exists());
    assert_eq!(
        fs::read_to_string(lab.base.join("outside.txt")).unwrap(),
        "outside",
        "the symlink was unlinked, its target untouched"
    );
    assert_eq!(names(&lab.base.join("completed")), ["Empty", "Full"]);
}

#[test]
fn remove_tree_is_all_or_nothing_at_the_decision_point() {
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let dir = rp(&c, "Rel");
    let snapshot = snap(&lab, "Rel");
    // An entry added after planning: nothing is removed.
    fs::write(completed(&lab, "Rel/Subs/new.srt"), b"new").unwrap();
    let e = m.remove_tree(&dir, &snapshot).unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    assert!(e.detail.contains("nothing removed"));
    assert_eq!(
        names(&completed(&lab, "Rel")),
        ["Subs", "a.mkv", "a.nfo", "link"]
    );
    fs::remove_file(completed(&lab, "Rel/Subs/new.srt")).unwrap();
    // A changed mtime inside the tree refuses too (full signature equality).
    let f = fs::OpenOptions::new()
        .append(true)
        .open(completed(&lab, "Rel/a.mkv"))
        .unwrap();
    drop(f);
    fs::write(completed(&lab, "Rel/a.mkv"), b"video2").unwrap();
    assert_eq!(
        m.remove_tree(&dir, &snapshot).unwrap_err().class,
        ErrorClass::StateChanged
    );
    assert!(completed(&lab, "Rel/Subs/a.srt").exists());
    // The snapshot must describe the directory it is applied to.
    let other = snap(&lab, "Full");
    assert_eq!(
        m.remove_tree(&dir, &other).unwrap_err().class,
        ErrorClass::Validation
    );
    // A symlinked directory is never entered.
    std::os::unix::fs::symlink(completed(&lab, "Full"), completed(&lab, "Linked")).unwrap();
    let fsnap = snap(&lab, "Full");
    let linked = DirSnapshot {
        dir: rp(&c, "Linked"),
        entries: fsnap.entries,
    };
    assert_eq!(
        m.remove_tree(&rp(&c, "Linked"), &linked).unwrap_err().class,
        ErrorClass::Confinement
    );
    assert!(completed(&lab, "Full/x").exists());
}

#[test]
fn remove_tree_stops_at_the_first_mismatch_and_reports_what_was_removed() {
    if is_root_user() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let dir = rp(&c, "Rel");
    // `Rel` is not writable: `Subs/a.srt` (inside the writable `Subs`) is unlinked first,
    // deepest first, then `rmdir Subs` fails; the removal stops there.
    fs::set_permissions(completed(&lab, "Rel"), fs::Permissions::from_mode(0o500)).unwrap();
    let snapshot = snap(&lab, "Rel");
    let e = m.remove_tree(&dir, &snapshot).unwrap_err();
    fs::set_permissions(completed(&lab, "Rel"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        e.detail.contains("removed 1 of 6 entries before stopping"),
        "{e:?}"
    );
    assert!(!completed(&lab, "Rel/Subs/a.srt").exists());
    assert!(completed(&lab, "Rel/Subs").exists());
    assert!(completed(&lab, "Rel/a.mkv").exists());
    assert!(completed(&lab, "Rel/a.nfo").exists());
    assert!(completed(&lab, "Rel/link").symlink_metadata().is_ok());
}

#[test]
fn remove_empty_dir_needs_an_empty_unchanged_directory_under_completed() {
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let full = rp(&c, "Full");
    let fs_ = sig(&lab, &c, "Full");
    let e = m.remove_empty_dir(&full, &fs_).unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    assert!(e.detail.contains("not empty"));
    assert!(completed(&lab, "Full/x").exists());
    // `mtime` and `nlink` may differ (the transaction just emptied it); identity, owner and
    // mode may not.
    let empty = rp(&c, "Empty");
    let es = sig(&lab, &c, "Empty");
    fs::write(completed(&lab, "Empty/tmp"), b"t").unwrap();
    fs::remove_file(completed(&lab, "Empty/tmp")).unwrap();
    let mut changed_mode = es;
    changed_mode.mode ^= 0o001;
    assert_eq!(
        m.remove_empty_dir(&empty, &changed_mode).unwrap_err().class,
        ErrorClass::StateChanged
    );
    assert!(completed(&lab, "Empty").exists());
    m.remove_empty_dir(&empty, &es).expect("removed");
    assert!(!completed(&lab, "Empty").exists());
    assert_eq!(
        m.remove_empty_dir(&empty, &es).unwrap_err().class,
        ErrorClass::StateChanged
    );
    // A file is not a directory; the incomplete root is not the completed root (INV-49).
    let a = sig(&lab, &c, "Rel/a.mkv");
    assert_eq!(
        m.remove_empty_dir(&rp(&c, "Rel/a.mkv"), &a)
            .unwrap_err()
            .class,
        ErrorClass::StateChanged
    );
    let i = RootId::incomplete();
    let dl = sig(&lab, &i, "Dl");
    assert_eq!(
        m.remove_empty_dir(&rp(&i, "Dl"), &dl).unwrap_err().class,
        ErrorClass::Confinement
    );
    assert!(lab.base.join("incomplete/Dl").exists());
    assert_eq!(
        m.remove_empty_dir(&rp(&c, ""), &es).unwrap_err().class,
        ErrorClass::Confinement
    );
}

#[test]
fn set_mode_is_targeted_verified_and_refuses_symlinks_or_changed_entries() {
    let lab = lab();
    let c = RootId::completed();
    let m = mutator(&lab);
    let p = rp(&c, "Rel/a.mkv");
    fs::set_permissions(
        completed(&lab, "Rel/a.mkv"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let s = sig(&lab, &c, "Rel/a.mkv");
    assert_eq!(s.mode, 0o444);
    m.set_mode(&p, &s, 0o644).expect("mode set");
    let md = fs::metadata(completed(&lab, "Rel/a.mkv")).unwrap();
    assert_eq!(md.mode() & 0o7777, 0o644);
    assert_eq!(md.ino(), s.ino, "same inode");
    // The signature changed (mode): the same request is refused now.
    assert_eq!(
        m.set_mode(&p, &s, 0o644).unwrap_err().class,
        ErrorClass::StateChanged
    );
    // A symlink is never followed; its target keeps its mode.
    let link = rp(&c, "Rel/link");
    let ls = sig(&lab, &c, "Rel/link");
    assert_eq!(
        m.set_mode(&link, &ls, 0o777).unwrap_err().class,
        ErrorClass::Confinement
    );
    let outside = fs::metadata(lab.base.join("outside.txt")).unwrap().mode() & 0o7777;
    assert_ne!(outside, 0o777);
    // Directories are allowed, never recursively: the child keeps its mode.
    let subs = rp(&c, "Rel/Subs");
    let ss = sig(&lab, &c, "Rel/Subs");
    let child_before = fs::metadata(completed(&lab, "Rel/Subs/a.srt"))
        .unwrap()
        .mode();
    // CP2T4-F04: bits may not be removed, nor group/other/special bits added.
    let current = ss.mode;
    for bad in [current & !0o444, current | 0o002, current | 0o4000, 0o000] {
        assert_eq!(
            m.set_mode(&subs, &ss, bad).unwrap_err().class,
            ErrorClass::Validation,
            "{bad:o}"
        );
    }
    fs::set_permissions(
        completed(&lab, "Rel/Subs"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    let ss = sig(&lab, &c, "Rel/Subs");
    m.set_mode(&subs, &ss, 0o755).expect("dir mode set");
    assert_eq!(
        fs::metadata(completed(&lab, "Rel/Subs")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        fs::metadata(completed(&lab, "Rel/Subs/a.srt"))
            .unwrap()
            .mode(),
        child_before
    );
}

#[test]
fn t4_13_remove_media_file_is_the_only_media_removal_and_is_strictly_confined() {
    let lab = lab();
    let m = mutator(&lab);
    let c = RootId::completed();
    let p = rp(&lab.media, "Movies/m.mkv");
    let s = sig(&lab, &lab.media, "Movies/m.mkv");
    // Never outside a media root, never through `remove_file`.
    let a = sig(&lab, &c, "Rel/a.nfo");
    assert_eq!(
        m.remove_media_file(&rp(&c, "Rel/a.nfo"), &a)
            .unwrap_err()
            .class,
        ErrorClass::Confinement
    );
    assert!(completed(&lab, "Rel/a.nfo").exists());
    assert_eq!(
        m.remove_file(&p, &s).unwrap_err().class,
        ErrorClass::Confinement
    );
    // A changed link count (a new hardlink) refuses.
    let mp = lab.base.join("media/Movies/m.mkv");
    fs::hard_link(&mp, lab.base.join("media/Movies/m2.mkv")).unwrap();
    assert_eq!(
        m.remove_media_file(&p, &s).unwrap_err().class,
        ErrorClass::StateChanged
    );
    assert!(mp.exists());
    fs::remove_file(lab.base.join("media/Movies/m2.mkv")).unwrap();
    // A symlink is never followed.
    std::os::unix::fs::symlink("../../outside.txt", lab.base.join("media/Movies/l")).unwrap();
    let ls = sig(&lab, &lab.media, "Movies/l");
    assert_eq!(
        m.remove_media_file(&rp(&lab.media, "Movies/l"), &ls)
            .unwrap_err()
            .class,
        ErrorClass::Confinement
    );
    assert!(lab.base.join("outside.txt").exists());
    // The unchanged regular file goes; the root and a directory never do.
    m.remove_media_file(&p, &s).expect("removed");
    assert!(!mp.exists());
    let d = sig(&lab, &lab.media, "Movies");
    assert_eq!(
        m.remove_media_file(&rp(&lab.media, "Movies"), &d)
            .unwrap_err()
            .class,
        ErrorClass::StateChanged
    );
    assert_eq!(
        m.remove_media_file(&rp(&lab.media, ""), &d)
            .unwrap_err()
            .class,
        ErrorClass::Confinement
    );
}
