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
//! Stage A (risk tiers 1-2) implements the hardlink probe and the atomic replacement. The
//! tier-3 removals and the mode change are present only as explicit refusals so that nothing
//! can delete or chmod until that tier is built and reviewed (MASTER section 10, CP2).

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use removarr_config::{LiveMode, Roots};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{DirSnapshot, EntryKind, EntrySignature, IoKind, PhysicalId};
use removarr_domain::path::{RootId, RootKind, RootedPath};
use removarr_domain::ports::FsMutator;
use rustix::fs::{AtFlags, fsync, linkat, renameat, unlinkat};
use rustix::io::Errno;

use crate::handle;

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

const NOT_IN_BUILD: &str = "not in this build: deletions and mode changes belong to risk tier \
    3, which is not implemented in stage A";

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

    fn remove_file(&self, _target: &RootedPath, _expect: &EntrySignature) -> Result<(), PortError> {
        Err(PortError::new(ErrorClass::CapabilityDisabled, NOT_IN_BUILD))
    }

    fn remove_tree(&self, _dir: &RootedPath, _expect: &DirSnapshot) -> Result<(), PortError> {
        Err(PortError::new(ErrorClass::CapabilityDisabled, NOT_IN_BUILD))
    }

    fn remove_empty_dir(
        &self,
        _dir: &RootedPath,
        _expect: &EntrySignature,
    ) -> Result<(), PortError> {
        Err(PortError::new(ErrorClass::CapabilityDisabled, NOT_IN_BUILD))
    }

    fn set_mode(
        &self,
        _target: &RootedPath,
        _expect: &EntrySignature,
        _mode: u32,
    ) -> Result<(), PortError> {
        Err(PortError::new(ErrorClass::CapabilityDisabled, NOT_IN_BUILD))
    }
}
