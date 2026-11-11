//! Confined filesystem adapter. This crate owns the read side (scan, stat, directory snapshots,
//! free space, permission analysis, bounded content reads). The mutation adapter lives in
//! `mutate` and is written by the action runner.
//!
//! Rules enforced here (ADR-0004):
//! - only configured roots are opened; paths are `RootedPath`, never host strings;
//! - every component is reached with `openat(O_NOFOLLOW | O_DIRECTORY)`, so symlinks are never
//!   followed and a symlinked component is refused;
//! - the inventory reads metadata only (no content, no hashing);
//! - blocking work is bounded: one walk at a time per process, and `content_read_concurrency`
//!   content readers.

mod content;
mod gate;
pub mod handle;
pub mod inventory;
pub mod mutate;
pub mod perm;
mod scan;

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

use removarr_config::Roots;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{
    DirSnapshot, EntryKind, EntrySignature, PermissionFinding, RootInventory,
};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::{FsInventory, ScanControl};

pub use content::MIN_CHUNK;
pub use mutate::LiveFsMutator;
pub use scan::{DirHook, QBIT_INTERNAL_DIR};

use gate::Gate;

/// Blocking-work limits (architecture 8.4). The walk is always sequential.
#[derive(Debug, Clone, Copy)]
pub struct FsLimits {
    /// Concurrent content readers (`limits.content_read_concurrency`, 1..=4).
    pub content_read_concurrency: u32,
}

impl Default for FsLimits {
    fn default() -> Self {
        Self {
            content_read_concurrency: 1,
        }
    }
}

/// Confined, read-only filesystem access to the configured roots.
pub struct ConfinedFs {
    roots: BTreeMap<RootId, PathBuf>,
    scan_gate: Gate,
    pub(crate) content_gate: Gate,
    hook: Option<DirHook>,
}

impl std::fmt::Debug for ConfinedFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedFs")
            .field("roots", &self.roots.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ConfinedFs {
    /// Builds the adapter from validated roots. Nothing is opened until a call needs it.
    #[must_use]
    pub fn new(roots: &Roots, limits: FsLimits) -> Self {
        Self {
            roots: roots
                .all()
                .map(|r| (r.id.clone(), PathBuf::from(&r.path)))
                .collect(),
            scan_gate: Gate::new(1),
            content_gate: Gate::new(limits.content_read_concurrency.clamp(1, 4) as usize),
            hook: None,
        }
    }

    /// Test seam: called after each directory listing, before its entries are examined.
    #[doc(hidden)]
    #[must_use]
    pub fn with_dir_hook(mut self, hook: DirHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Highest number of walks that ran at the same time (always 1 by construction).
    #[must_use]
    pub fn peak_concurrent_scans(&self) -> usize {
        self.scan_gate.peak()
    }

    /// Highest number of concurrent content readers.
    #[must_use]
    pub fn peak_concurrent_content_reads(&self) -> usize {
        self.content_gate.peak()
    }

    /// Opens a configured root through a no-follow walk from `/`. Unknown roots are refused.
    pub fn open_root(&self, root: &RootId) -> Result<OwnedFd, PortError> {
        let path = self.roots.get(root).ok_or_else(|| {
            PortError::new(
                ErrorClass::Confinement,
                format!("root {root} is not configured"),
            )
        })?;
        handle::open_abs_dir(path)
    }

    fn open_dir(&self, dir: &RootedPath) -> Result<OwnedFd, PortError> {
        let root = self.open_root(&dir.root)?;
        handle::open_rel_dir(&root, &dir.rel)
    }
}

impl FsInventory for ConfinedFs {
    fn scan_root(&self, root: &RootId, ctl: &ScanControl) -> Result<RootInventory, PortError> {
        let _permit = self.scan_gate.acquire();
        let fd = self.open_root(root)?;
        scan::walk(root, fd, ctl, self.hook.as_ref())
    }

    fn stat(&self, path: &RootedPath) -> Result<Option<EntrySignature>, PortError> {
        let root_fd = self.open_root(&path.root)?;
        let (Some(parent), Some(name)) = (path.rel.parent(), path.rel.file_name()) else {
            return handle::handle_signature(&root_fd).map(|(s, _)| Some(s));
        };
        let dir = match handle::open_rel_dir(&root_fd, &parent) {
            Ok(d) => d,
            Err(e) if e.class == ErrorClass::Io(removarr_domain::fs::IoKind::NotFound) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let cname = std::ffi::CString::new(name)
            .map_err(|_| PortError::new(ErrorClass::Validation, "NUL in file name"))?;
        match handle::lstat_at(&dir, &cname) {
            Ok(st) => Ok(Some(handle::signature(&st))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(e) => Err(handle::port_error(e, "stat")),
        }
    }

    /// Signatures of `dir` and of everything below it (keys relative to `dir`, `""` is the
    /// directory itself). This is the subtree form that folder removal revalidates (DR-07),
    /// a superset of "direct entries". Any error, non-UTF-8 name or mount boundary fails the
    /// whole snapshot: a partial snapshot would not protect a deletion.
    fn snapshot_dir(&self, dir: &RootedPath) -> Result<DirSnapshot, PortError> {
        let fd = self.open_dir(dir)?;
        let (top, _) = handle::handle_signature(&fd)?;
        let mut entries = BTreeMap::new();
        entries.insert(RelPath::root(), top);
        let mut stack = vec![(fd, RelPath::root())];
        while let Some((dfd, rel)) = stack.pop() {
            let names = match scan::list_dir(&dfd) {
                Ok(scan::Listing::Names(n)) => n,
                Ok(scan::Listing::Partial(_, e)) | Err(e) => {
                    return Err(handle::port_error(e, "listing directory"));
                }
            };
            for n in names {
                let text = n.name.to_str().map_err(|_| {
                    PortError::new(ErrorClass::Rejected, "non-UTF-8 name inside snapshot")
                })?;
                let child = rel
                    .join(text)
                    .map_err(|e| PortError::new(ErrorClass::Rejected, e.to_string()))?;
                let st = handle::lstat_at(&dfd, &n.name)
                    .map_err(|e| handle::port_error(e, "stat inside snapshot"))?;
                let sig = handle::signature(&st);
                if sig.kind == EntryKind::Dir {
                    if sig.dev != top.dev {
                        return Err(handle::confinement_error("mount boundary inside snapshot"));
                    }
                    let sub = handle::open_dir_at(&dfd, &n.name)?;
                    let (opened, _) = handle::handle_signature(&sub)?;
                    if opened.ino != sig.ino || opened.dev != sig.dev {
                        return Err(PortError::new(
                            ErrorClass::StateChanged,
                            "directory replaced during snapshot",
                        ));
                    }
                    stack.push((sub, child.clone()));
                }
                entries.insert(child, sig);
            }
        }
        Ok(DirSnapshot {
            dir: dir.clone(),
            entries,
        })
    }

    /// Bytes available to an unprivileged user on the filesystem holding the root.
    fn free_space(&self, root: &RootId) -> Result<u64, PortError> {
        let fd = self.open_root(root)?;
        let v = rustix::fs::fstatvfs(&fd).map_err(|e| handle::port_error(e, "statvfs"))?;
        Ok(v.f_bavail.saturating_mul(v.f_frsize))
    }

    fn permission_findings(
        &self,
        inv: &RootInventory,
        owner: (u32, u32),
    ) -> Vec<PermissionFinding> {
        perm::findings(inv, owner)
    }
}

/// Convenience: shared handle for wiring (`Arc<dyn FsInventory>` and `Arc<dyn ContentReader>`).
#[must_use]
pub fn shared(roots: &Roots, limits: FsLimits) -> Arc<ConfinedFs> {
    Arc::new(ConfinedFs::new(roots, limits))
}
