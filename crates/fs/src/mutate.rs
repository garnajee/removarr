//! Live filesystem mutation adapter (`FsMutator`), owned by the action runner (architecture
//! 15.2). Constructible only with the `LiveMode` token (ADR-0003).
//!
//! Every path is reached through the confined handle helpers of [`crate::handle`]: the root is
//! opened from `/` with no-follow at every component, the parent directory of the target is
//! opened the same way, and the final component is `lstat`ed and compared with the signature
//! the plan captured. A symlink anywhere is a refusal, never followed. No host path string is
//! ever assembled and no shell is ever involved: every operation is an `*at` system call on a
//! directory handle.
//!
//! Tiers 1-2: the hardlink probe and the atomic replacement. Tier 3: `remove_file`,
//! `remove_tree`, `remove_empty_dir` (never under a media root, `remove_empty_dir` under the
//! completed root only, INV-49) and the targeted `set_mode`. Every removal re-checks the entry
//! through its parent handle immediately before `unlinkat`; a tree is compared whole with the
//! plan's snapshot before the first unlink and each entry's `(dev, ino)` again before its own
//! unlink (T3-4). Symlink entries are unlinked, never followed (T-17).

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use removarr_config::{LiveMode, Roots};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{DirSnapshot, EntryKind, EntrySignature, IoKind, PhysicalId};
use removarr_domain::path::{RelPath, RootId, RootKind, RootedPath};
use removarr_domain::ports::FsMutator;
use rustix::fs::{AtFlags, Mode, OFlags, fchmod, fstat, fsync, linkat, openat, renameat, unlinkat};
use rustix::io::Errno;

use crate::handle;
use crate::scan;

/// Explanation attached to a refused `link()` (CON-30, T-14): ownership and the kernel's
/// `fs.protected_hardlinks` are named; privilege elevation is never suggested.
pub const HARDLINK_REFUSED_HINT: &str = "the kernel or the filesystem refused to create a \
    hardlink: with fs.protected_hardlinks=1 only the owner of the source file (or a process \
    with write and read access to it) may link it; align the file owner with expected_owner and \
    make the target directory writable by the service identity, without running the service as \
    root";

/// Explanation attached to `EXDEV` (MASTER section 8, volume topology).
pub const CROSS_DEVICE_HINT: &str = "source and target are not on the same device or bind \
    mount: hardlinks need the common parent of completed and media mounted as one volume";

/// Longest prefix of the target name kept in a temporary name.
pub const TEMP_BASE_MAX: usize = 64;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Confined mutation adapter. One instance per process, built in `app::wiring` with the
/// `LiveMode` token.
pub struct LiveFsMutator {
    roots: BTreeMap<RootId, PathBuf>,
}

impl std::fmt::Debug for LiveFsMutator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveFsMutator")
            .field("roots", &self.roots.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl LiveFsMutator {
    fn unlink_regular(
        &self,
        target: &RootedPath,
        expect: &EntrySignature,
    ) -> Result<(), PortError> {
        let (dir, name) = self.open_parent(target)?;
        let sig = lstat_regular(&dir, &name, "target")?;
        if !sig.matches(expect, true) {
            return Err(state_changed("target changed since the plan was built"));
        }
        unlinkat(&dir, &name, AtFlags::empty()).map_err(|e| handle::port_error(e, "unlink"))?;
        fsync(&dir).map_err(|e| handle::port_error(e, "fsync of the parent directory"))?;
        Ok(())
    }

    /// The `LiveMode` token proves `dry_run = false`; without it this type cannot exist.
    #[must_use]
    pub fn new(roots: &Roots, _live: &LiveMode) -> Self {
        Self {
            roots: roots
                .all()
                .map(|r| (r.id.clone(), PathBuf::from(&r.path)))
                .collect(),
        }
    }

    fn open_root(&self, root: &RootId) -> Result<OwnedFd, PortError> {
        let path = self
            .roots
            .get(root)
            .ok_or_else(|| handle::confinement_error(format!("root {root} is not configured")))?;
        handle::open_abs_dir(path)
    }

    fn open_dir(&self, dir: &RootedPath) -> Result<OwnedFd, PortError> {
        let root = self.open_root(&dir.root)?;
        handle::open_rel_dir(&root, &dir.rel)
    }

    /// Opens the parent directory of `path` (no-follow at every component) and returns the
    /// handle with the final component name. The root itself is never a valid target.
    fn open_parent(&self, path: &RootedPath) -> Result<(OwnedFd, CString), PortError> {
        let (Some(parent), Some(name)) = (path.rel.parent(), path.rel.file_name()) else {
            return Err(handle::confinement_error(
                "a configured root is never a mutation target",
            ));
        };
        let root = self.open_root(&path.root)?;
        let dir = handle::open_rel_dir(&root, &parent)?;
        let cname = CString::new(name)
            .map_err(|_| PortError::new(ErrorClass::Validation, "NUL in file name"))?;
        Ok((dir, cname))
    }
}

/// Defense in depth (CP2T3-F08): a hardlink always goes from the completed root to a media
/// root, whatever the plan says.
fn check_direction(source: &RootedPath, target_root: &RootId) -> Result<(), PortError> {
    if source.root.kind() != RootKind::Completed || target_root.kind() != RootKind::Media {
        return Err(handle::confinement_error(
            "a hardlink repair goes from the completed root to a media root only",
        ));
    }
    Ok(())
}

/// `lstat` of a regular file that must still match what the plan captured (`nlink` ignored:
/// the plan pins consolidation pairs with `compare_nlink = false`).
fn expect_file(
    dir: impl AsFd,
    name: &CStr,
    expect: &EntrySignature,
    what: &str,
) -> Result<EntrySignature, PortError> {
    let sig = lstat_regular(dir, name, what)?;
    if !sig.matches(expect, false) {
        return Err(PortError::new(
            ErrorClass::StateChanged,
            format!("{what} changed since the plan was built"),
        ));
    }
    Ok(sig)
}

/// `lstat` that accepts only a regular file: a symlink is a confinement refusal (never
/// followed), anything else is a state change.
fn lstat_regular(dir: impl AsFd, name: &CStr, what: &str) -> Result<EntrySignature, PortError> {
    match handle::lstat_at(dir, name) {
        Ok(st) => {
            let sig = handle::signature(&st);
            match sig.kind {
                EntryKind::File => Ok(sig),
                EntryKind::Symlink => Err(handle::confinement_error(format!(
                    "{what} is a symlink; symlinks are never followed"
                ))),
                EntryKind::Dir | EntryKind::Other => Err(PortError::new(
                    ErrorClass::StateChanged,
                    format!("{what} is not a regular file"),
                )),
            }
        }
        Err(Errno::NOENT) => Err(PortError::new(
            ErrorClass::StateChanged,
            format!("{what} no longer exists"),
        )),
        Err(e) => Err(handle::port_error(e, what)),
    }
}

/// Temporary name next to the target: `.<base>.removarr-<tag>-<seq>-<ns>`, where `<base>` is
/// the target name truncated to [`TEMP_BASE_MAX`] bytes (on a char boundary) so the result
/// stays well under `NAME_MAX` (255) for long release names (CP2T3-F13).
fn temp_name(name: &CStr, tag: &str) -> Result<CString, PortError> {
    let base = name
        .to_str()
        .map_err(|_| PortError::new(ErrorClass::Validation, "file name is not valid UTF-8"))?;
    let mut cut = base.len().min(TEMP_BASE_MAX);
    while !base.is_char_boundary(cut) {
        cut -= 1;
    }
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    CString::new(format!(".{}.removarr-{tag}-{seq}-{ns}", &base[..cut]))
        .map_err(|_| PortError::new(ErrorClass::Validation, "NUL in temporary name"))
}

fn link_error(e: Errno) -> PortError {
    match e {
        Errno::PERM | Errno::ACCESS => PortError::new(
            ErrorClass::Io(IoKind::PermissionDenied),
            format!("link: {}; {HARDLINK_REFUSED_HINT}", handle::io_message(e)),
        ),
        Errno::XDEV => PortError::new(
            ErrorClass::Io(IoKind::CrossDevice),
            format!("link: {}; {CROSS_DEVICE_HINT}", handle::io_message(e)),
        ),
        other => handle::port_error(other, "link"),
    }
}

/// Best effort removal of a temporary link; the original error wins.
fn unlink_temp(dir: impl AsFd, tmp: &CStr) {
    let _ = unlinkat(dir, tmp, AtFlags::empty());
}

/// Creates the temporary link `tmp` in `tgt_dir` pointing at the source inode and proves it
/// by `lstat`. On any failure the temporary link is removed again.
fn link_temp(
    src_dir: impl AsFd,
    src_name: &CStr,
    tgt_dir: impl AsFd,
    tmp: &CStr,
    expect: PhysicalId,
) -> Result<PhysicalId, PortError> {
    linkat(
        src_dir.as_fd(),
        src_name,
        tgt_dir.as_fd(),
        tmp,
        AtFlags::empty(),
    )
    .map_err(link_error)?;
    let linked = match handle::lstat_at(tgt_dir.as_fd(), tmp) {
        Ok(st) => handle::signature(&st).phys(),
        Err(e) => {
            unlink_temp(tgt_dir.as_fd(), tmp);
            return Err(handle::port_error(e, "stat of the temporary link"));
        }
    };
    if linked != expect {
        unlink_temp(tgt_dir.as_fd(), tmp);
        return Err(PortError::new(
            ErrorClass::StateChanged,
            "the temporary link does not carry the source inode",
        ));
    }
    Ok(linked)
}

/// Defense in depth (T3-2): a removal never targets a media root, whatever the plan says.
fn refuse_media(path: &RootedPath) -> Result<(), PortError> {
    if path.root.kind() == RootKind::Media {
        return Err(handle::confinement_error(
            "nothing is ever removed under a media root",
        ));
    }
    Ok(())
}

fn state_changed(what: &str) -> PortError {
    PortError::new(ErrorClass::StateChanged, what.to_string())
}

/// `lstat` that accepts only a directory (symlink: confinement; missing or other: state change).
fn lstat_dir(dir: impl AsFd, name: &CStr, what: &str) -> Result<EntrySignature, PortError> {
    match handle::lstat_at(dir, name) {
        Ok(st) => {
            let sig = handle::signature(&st);
            match sig.kind {
                EntryKind::Dir => Ok(sig),
                EntryKind::Symlink => Err(handle::confinement_error(format!(
                    "{what} is a symlink; symlinks are never followed"
                ))),
                EntryKind::File | EntryKind::Other => {
                    Err(state_changed(&format!("{what} is not a directory")))
                }
            }
        }
        Err(Errno::NOENT) => Err(state_changed(&format!("{what} no longer exists"))),
        Err(e) => Err(handle::port_error(e, what)),
    }
}

/// One entry of a tree walk: its parent handle (index into the handle list), its name, its
/// path relative to the tree root and the signature observed during the walk.
struct Node {
    parent: usize,
    name: CString,
    rel: RelPath,
    sig: EntrySignature,
}

/// Recursive walk of an opened directory through handles only: every subdirectory is opened
/// no-follow and proven by `(dev, ino)`, names are sorted so the deletion order is
/// deterministic, and a mount boundary refuses the whole walk.
fn walk(top: OwnedFd, top_sig: EntrySignature) -> Result<(Vec<OwnedFd>, Vec<Node>), PortError> {
    let mut dirs = vec![top];
    let mut nodes = Vec::new();
    let mut stack = vec![(0usize, RelPath::root())];
    while let Some((di, rel)) = stack.pop() {
        let mut names = match scan::list_dir(&dirs[di]) {
            Ok(scan::Listing::Names(n)) => n,
            Ok(scan::Listing::Partial(_, e)) | Err(e) => {
                return Err(handle::port_error(e, "listing directory"));
            }
        };
        names.sort_by(|a, b| a.name.cmp(&b.name));
        for n in names {
            let text = n
                .name
                .to_str()
                .map_err(|_| PortError::new(ErrorClass::Rejected, "non-UTF-8 name in tree"))?;
            let child = rel
                .join(text)
                .map_err(|e| PortError::new(ErrorClass::Rejected, e.to_string()))?;
            let st = handle::lstat_at(&dirs[di], &n.name)
                .map_err(|e| handle::port_error(e, "stat inside tree"))?;
            let sig = handle::signature(&st);
            if sig.kind == EntryKind::Dir {
                if sig.dev != top_sig.dev {
                    return Err(handle::confinement_error("mount boundary inside the tree"));
                }
                let sub = handle::open_dir_at(&dirs[di], &n.name)?;
                let (opened, _) = handle::handle_signature(&sub)?;
                if opened.phys() != sig.phys() {
                    return Err(state_changed("directory replaced during the walk"));
                }
                dirs.push(sub);
                stack.push((dirs.len() - 1, child.clone()));
            }
            nodes.push(Node {
                parent: di,
                name: n.name,
                rel: child,
                sig,
            });
        }
    }
    Ok((dirs, nodes))
}

fn unlink_flags(kind: EntryKind) -> AtFlags {
    if kind == EntryKind::Dir {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    }
}

impl FsMutator for LiveFsMutator {
    /// Temporary hardlink of `source` inside `target_dir`, proven by `(dev, ino)`, then removed.
    /// A refusal (`EPERM`, `EACCES`, `EXDEV`) carries an explanatory message and happens before
    /// any content is read (T-14, CON-30).
    fn probe_hardlink(
        &self,
        source: &RootedPath,
        target_dir: &RootedPath,
    ) -> Result<(), PortError> {
        check_direction(source, &target_dir.root)?;
        let (src_dir, src_name) = self.open_parent(source)?;
        let src = lstat_regular(&src_dir, &src_name, "source")?;
        let tgt_dir = self.open_dir(target_dir)?;
        let tmp = temp_name(&src_name, "probe")?;
        link_temp(&src_dir, &src_name, &tgt_dir, &tmp, src.phys())?;
        unlinkat(&tgt_dir, &tmp, AtFlags::empty())
            .map_err(|e| handle::port_error(e, "removing the probe link"))?;
        Ok(())
    }

    /// `linkat(source -> .<name>.removarr-link-...)` in the target's directory, `renameat` over
    /// the target, `lstat` proving the target now carries the source inode, `fsync` of the
    /// directory. Both signatures are checked right before the link; the target is checked
    /// again right before the rename, while the source is proven by the inode of the temporary
    /// link itself. Any difference refuses with `StateChanged` and leaves nothing behind.
    fn replace_with_hardlink(
        &self,
        source: &RootedPath,
        target: &RootedPath,
        expect_source: &EntrySignature,
        expect_target: &EntrySignature,
    ) -> Result<PhysicalId, PortError> {
        check_direction(source, &target.root)?;
        let (src_dir, src_name) = self.open_parent(source)?;
        let src = expect_file(&src_dir, &src_name, expect_source, "source")?;
        let (tgt_dir, tgt_name) = self.open_parent(target)?;
        expect_file(&tgt_dir, &tgt_name, expect_target, "target")?;
        let tmp = temp_name(&tgt_name, "link")?;
        let phys = link_temp(&src_dir, &src_name, &tgt_dir, &tmp, src.phys())?;
        // Last look at the target before it is replaced.
        if let Err(e) = expect_file(&tgt_dir, &tgt_name, expect_target, "target") {
            unlink_temp(&tgt_dir, &tmp);
            return Err(e);
        }
        if let Err(e) = renameat(&tgt_dir, &tmp, &tgt_dir, &tgt_name) {
            unlink_temp(&tgt_dir, &tmp);
            return Err(handle::port_error(e, "rename over the target"));
        }
        let after = lstat_regular(&tgt_dir, &tgt_name, "replaced target")?;
        if after.phys() != phys {
            return Err(PortError::new(
                ErrorClass::StateChanged,
                "the target does not carry the source inode after the rename",
            ));
        }
        fsync(&tgt_dir).map_err(|e| handle::port_error(e, "fsync of the target directory"))?;
        Ok(phys)
    }

    /// `lstat` through the parent handle immediately before `unlinkat`; the entry must still
    /// match the planned signature **including `nlink`** (CON-14). A symlink is refused, a
    /// missing entry is a state change (never "already gone = success", CONTEXTE 3.15).
    fn remove_file(&self, target: &RootedPath, expect: &EntrySignature) -> Result<(), PortError> {
        refuse_media(target)?;
        self.unlink_regular(target, expect)
    }

    /// The only removal allowed under a media root (tier 4 `DeleteMediaFile`): the target must
    /// be a regular file of a media root and match the planned signature including `nlink`.
    /// Everything else about it is `remove_file`: handle-based, no symlink, no root.
    fn remove_media_file(
        &self,
        target: &RootedPath,
        expect: &EntrySignature,
    ) -> Result<(), PortError> {
        if target.root.kind() != RootKind::Media {
            return Err(handle::confinement_error(
                "remove_media_file only removes files under a media root",
            ));
        }
        self.unlink_regular(target, expect)
    }

    /// All or nothing at the decision point (T3-4): a fresh recursive walk through handles
    /// must equal the plan's snapshot exactly (every signature, `nlink`, owner, mode, mtime,
    /// one device); then entries are unlinked deepest first on the same handles, each one
    /// re-checked by `(dev, ino)` and kind right before its own unlink. Any mismatch after the
    /// first unlink stops at once; the error says how many entries were removed. Symlinks are
    /// unlinked, never followed.
    fn remove_tree(&self, dir: &RootedPath, expect: &DirSnapshot) -> Result<(), PortError> {
        refuse_media(dir)?;
        if &expect.dir != dir {
            return Err(PortError::new(
                ErrorClass::Validation,
                "the snapshot does not describe this directory",
            ));
        }
        let (parent, name) = self.open_parent(dir)?;
        let top_sig = lstat_dir(&parent, &name, "directory")?;
        let top = handle::open_dir_at(&parent, &name)?;
        let (opened, _) = handle::handle_signature(&top)?;
        if opened.phys() != top_sig.phys() {
            return Err(state_changed("directory replaced while opening it"));
        }
        let (dirs, nodes) = walk(top, top_sig)?;
        let mut observed: BTreeMap<RelPath, EntrySignature> = BTreeMap::new();
        observed.insert(RelPath::root(), top_sig);
        for n in &nodes {
            observed.insert(n.rel.clone(), n.sig);
        }
        if observed != expect.entries {
            return Err(state_changed(
                "folder contents changed since the plan was built; nothing removed",
            ));
        }
        let total = nodes.len() + 1;
        let mut removed = 0usize;
        let stop = |e: PortError, removed: usize| {
            PortError::new(
                e.class.clone(),
                format!(
                    "{}; removed {removed} of {total} entries before stopping",
                    e.detail
                ),
            )
        };
        for n in nodes.iter().rev() {
            let pfd = &dirs[n.parent];
            let now = match handle::lstat_at(pfd, &n.name) {
                Ok(st) => handle::signature(&st),
                Err(e) => return Err(stop(handle::port_error(e, "stat before unlink"), removed)),
            };
            if now.phys() != n.sig.phys() || now.kind != n.sig.kind {
                return Err(stop(
                    state_changed(&format!("{} changed during the removal", n.rel)),
                    removed,
                ));
            }
            unlinkat(pfd, &n.name, unlink_flags(n.sig.kind))
                .map_err(|e| stop(handle::port_error(e, "unlink inside tree"), removed))?;
            removed += 1;
        }
        let now = lstat_dir(&parent, &name, "directory").map_err(|e| stop(e, removed))?;
        if now.phys() != top_sig.phys() {
            return Err(stop(
                state_changed("directory replaced during the removal"),
                removed,
            ));
        }
        drop(dirs);
        unlinkat(&parent, &name, AtFlags::REMOVEDIR)
            .map_err(|e| stop(handle::port_error(e, "rmdir"), removed))?;
        fsync(&parent).map_err(|e| handle::port_error(e, "fsync of the parent directory"))?;
        Ok(())
    }

    /// `rmdir` semantics under the completed root only (INV-49): the directory must be empty
    /// at the call and keep the planned `dev`, `ino`, `uid`, `gid` and `mode`. `mtime_ns` and
    /// `nlink` are deliberately not compared: the same transaction just removed its children.
    fn remove_empty_dir(&self, dir: &RootedPath, expect: &EntrySignature) -> Result<(), PortError> {
        if dir.root.kind() != RootKind::Completed {
            return Err(handle::confinement_error(
                "empty parents are removed under the completed root only",
            ));
        }
        let (parent, name) = self.open_parent(dir)?;
        let sig = lstat_dir(&parent, &name, "directory")?;
        let same = sig.phys() == expect.phys()
            && sig.uid == expect.uid
            && sig.gid == expect.gid
            && sig.mode == expect.mode
            && expect.kind == EntryKind::Dir;
        if !same {
            return Err(state_changed(
                "directory identity, owner or mode changed since the plan was built",
            ));
        }
        match unlinkat(&parent, &name, AtFlags::REMOVEDIR) {
            Ok(()) => {}
            Err(Errno::NOTEMPTY | Errno::EXIST) => {
                return Err(state_changed("directory is not empty"));
            }
            Err(e) => return Err(handle::port_error(e, "rmdir")),
        }
        fsync(&parent).map_err(|e| handle::port_error(e, "fsync of the parent directory"))?;
        Ok(())
    }

    /// Targeted, never recursive (INV-32, T3-18): the entry must match the planned signature,
    /// be owned by the process, and be a regular file or a directory. It is opened no-follow,
    /// proven by `(dev, ino)`, `fchmod`ed, and the new mode is read back.
    fn set_mode(
        &self,
        target: &RootedPath,
        expect: &EntrySignature,
        mode: u32,
    ) -> Result<(), PortError> {
        let (dir, name) = self.open_parent(target)?;
        let sig = match handle::lstat_at(&dir, &name) {
            Ok(st) => handle::signature(&st),
            Err(Errno::NOENT) => return Err(state_changed("target no longer exists")),
            Err(e) => return Err(handle::port_error(e, "target")),
        };
        let flags = match sig.kind {
            EntryKind::File => OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            EntryKind::Dir => {
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
            }
            EntryKind::Symlink => {
                return Err(handle::confinement_error(
                    "target is a symlink; symlinks are never followed",
                ));
            }
            EntryKind::Other => return Err(state_changed("target is not a file or directory")),
        };
        if !sig.matches(expect, false) {
            return Err(state_changed("target changed since the plan was built"));
        }
        // CP2T4-F04: only the owner's write bit (and search on directories) may be added.
        let extra = if sig.kind == EntryKind::Dir {
            0o300
        } else {
            0o200
        };
        let (cur, new) = (sig.mode & 0o7777, mode & 0o7777);
        if mode != new || new & cur != cur || new & !cur & !extra != 0 {
            return Err(PortError::new(
                ErrorClass::Validation,
                "a mode fix may only add the owner write bit (and search on a directory)",
            ));
        }
        if sig.uid != rustix::process::geteuid().as_raw() {
            return Err(PortError::new(
                ErrorClass::Io(IoKind::PermissionDenied),
                "only an entry owned by the service identity can have its mode changed",
            ));
        }
        let fd = openat(&dir, &name, flags, Mode::empty())
            .map_err(|e| handle::port_error(e, "opening the target"))?;
        let opened = handle::signature(&fstat(&fd).map_err(|e| handle::port_error(e, "fstat"))?);
        if opened.phys() != sig.phys() {
            return Err(state_changed("target replaced while opening it"));
        }
        fchmod(&fd, Mode::from_raw_mode(mode as rustix::fs::RawMode))
            .map_err(|e| handle::port_error(e, "chmod"))?;
        let after = handle::signature(&fstat(&fd).map_err(|e| handle::port_error(e, "fstat"))?);
        if after.mode != mode & 0o7777 {
            return Err(state_changed("the mode was not applied"));
        }
        Ok(())
    }
}
