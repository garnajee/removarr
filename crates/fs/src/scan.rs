//! The read-only walk of one root: iterative DFS, one directory handle per level, no-follow,
//! metadata only (`fstatat`), never a content read.

use std::ffi::CString;
use std::os::fd::{AsFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use removarr_domain::error::PortError;
use removarr_domain::fs::{
    EntryKind, FsEntry, IoKind, ProtectedReason, ProtectedZone, RootInventory, ScanErrorRecord,
};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::ScanControl;
use rustix::fs::{Dir, readlinkat};
use rustix::io::Errno;

use crate::handle::{
    allocated_bytes, handle_signature, io_kind, io_message, lstat_at, open_dir_at, signature,
};

/// qBittorrent's internal directory name. Never opened, never listed.
pub const QBIT_INTERNAL_DIR: &str = "BT_backup";
const FASTRESUME_SUFFIX: &[u8] = b".fastresume";

pub(crate) struct RawName {
    pub name: CString,
    pub ino: u64,
}

pub(crate) enum Listing {
    Names(Vec<RawName>),
    /// Some names were read before the error.
    Partial(Vec<RawName>, Errno),
}

/// Lists a directory (names and inode numbers only), `.` and `..` skipped.
pub(crate) fn list_dir(fd: &OwnedFd) -> Result<Listing, Errno> {
    let dir = Dir::read_from(fd)?;
    let mut out = Vec::new();
    for entry in dir {
        match entry {
            Ok(e) => {
                let n = e.file_name();
                if n.to_bytes() == b"." || n.to_bytes() == b".." {
                    continue;
                }
                out.push(RawName {
                    name: n.to_owned(),
                    ino: e.ino(),
                });
            }
            Err(err) => return Ok(Listing::Partial(out, err)),
        }
    }
    Ok(Listing::Names(out))
}

pub(crate) fn has_fastresume(names: &[RawName]) -> bool {
    names
        .iter()
        .any(|n| n.name.to_bytes().ends_with(FASTRESUME_SUFFIX))
}

struct Pending {
    parent: Rc<OwnedFd>,
    name: CString,
    rel: RelPath,
    dev: u64,
    ino: u64,
}

/// Optional observer called after a directory is listed and before its entries are examined.
/// It exists so tests can create races deterministically; production code leaves it unset.
pub type DirHook = std::sync::Arc<dyn Fn(&RelPath) + Send + Sync>;

pub(crate) fn walk(
    root: &RootId,
    root_fd: OwnedFd,
    ctl: &ScanControl,
    hook: Option<&DirHook>,
) -> Result<RootInventory, PortError> {
    let (root_sig, _) = handle_signature(&root_fd)?;
    let mut inv = RootInventory {
        root: root.clone(),
        root_sig: Some(root_sig),
        entries: Vec::new(),
        errors: Vec::new(),
        protected: Vec::new(),
        finished: true,
    };
    let rooted = |rel: &RelPath| RootedPath::new(root.clone(), rel.clone());
    let root_fd = Rc::new(root_fd);

    // The root itself is processed as a pseudo-pending entry with an empty name.
    let mut stack: Vec<Option<Pending>> = vec![None];
    while let Some(pending) = stack.pop() {
        if ctl.cancel.is_cancelled() {
            inv.finished = false;
            break;
        }
        let (dir_fd, rel) = match pending {
            None => (Rc::clone(&root_fd), RelPath::root()),
            Some(p) => match open_dir_at(p.parent.as_fd(), &p.name) {
                Ok(fd) => {
                    match handle_signature(&fd) {
                        Ok((s, _)) if s.dev == p.dev && s.ino == p.ino => {}
                        Ok(_) => {
                            inv.errors.push(ScanErrorRecord {
                                path: rooted(&p.rel),
                                io: IoKind::Other,
                                message: "directory was replaced during the scan".into(),
                            });
                            continue;
                        }
                        Err(e) => {
                            inv.errors.push(ScanErrorRecord {
                                path: rooted(&p.rel),
                                io: IoKind::Other,
                                message: e.detail,
                            });
                            continue;
                        }
                    }
                    (Rc::new(fd), p.rel)
                }
                Err(e) => {
                    let io = match e.class {
                        removarr_domain::error::ErrorClass::Io(k) => k,
                        _ => IoKind::Other,
                    };
                    inv.errors.push(ScanErrorRecord {
                        path: rooted(&p.rel),
                        io,
                        message: e.detail,
                    });
                    continue;
                }
            },
        };

        let mut names = match list_dir(&dir_fd) {
            Ok(Listing::Names(n)) => n,
            Ok(Listing::Partial(n, err)) => {
                inv.errors.push(ScanErrorRecord {
                    path: rooted(&rel),
                    io: io_kind(err),
                    message: io_message(err),
                });
                n
            }
            Err(err) => {
                inv.errors.push(ScanErrorRecord {
                    path: rooted(&rel),
                    io: io_kind(err),
                    message: io_message(err),
                });
                continue;
            }
        };

        if has_fastresume(&names) {
            inv.protected.push(ProtectedZone {
                path: rooted(&rel),
                reason: ProtectedReason::QbitInternal,
            });
            continue;
        }

        if let Some(h) = hook {
            h(&rel);
        }

        // Stat in inode order: closer to on-disk order on rotating disks.
        names.sort_by_key(|n| n.ino);
        let mut non_utf8_zone = false;
        let mut examined: u64 = 0;
        for n in &names {
            examined += 1;
            let Ok(text) = n.name.to_str() else {
                if !non_utf8_zone {
                    non_utf8_zone = true;
                    inv.protected.push(ProtectedZone {
                        path: rooted(&rel),
                        reason: ProtectedReason::NonUtf8Name,
                    });
                }
                continue;
            };
            let Ok(child_rel) = rel.join(text) else {
                if !non_utf8_zone {
                    non_utf8_zone = true;
                    inv.protected.push(ProtectedZone {
                        path: rooted(&rel),
                        reason: ProtectedReason::NonUtf8Name,
                    });
                }
                continue;
            };
            let st = match lstat_at(dir_fd.as_fd(), &n.name) {
                Ok(st) => st,
                Err(e) => {
                    inv.errors.push(ScanErrorRecord {
                        path: rooted(&child_rel),
                        io: io_kind(e),
                        message: io_message(e),
                    });
                    continue;
                }
            };
            let sig = signature(&st);
            let symlink_target = if sig.kind == EntryKind::Symlink {
                readlinkat(dir_fd.as_fd(), &n.name, Vec::new())
                    .ok()
                    .map(|c| c.to_string_lossy().into_owned())
            } else {
                None
            };
            inv.entries.push(FsEntry {
                path: rooted(&child_rel),
                sig,
                allocated: allocated_bytes(&st),
                symlink_target,
            });
            if sig.kind != EntryKind::Dir {
                continue;
            }
            if text == QBIT_INTERNAL_DIR {
                inv.protected.push(ProtectedZone {
                    path: rooted(&child_rel),
                    reason: ProtectedReason::QbitInternal,
                });
                continue;
            }
            if root_sig.dev != sig.dev {
                inv.protected.push(ProtectedZone {
                    path: rooted(&child_rel),
                    reason: ProtectedReason::MountBoundary,
                });
                continue;
            }
            stack.push(Some(Pending {
                parent: Rc::clone(&dir_fd),
                name: n.name.clone(),
                rel: child_rel,
                dev: sig.dev,
                ino: sig.ino,
            }));
        }
        ctl.progress.fetch_add(examined, Ordering::Relaxed);
    }

    inv.entries.sort_by(|a, b| a.path.rel.cmp(&b.path.rel));
    inv.errors.sort_by(|a, b| a.path.rel.cmp(&b.path.rel));
    inv.protected.sort_by(|a, b| a.path.rel.cmp(&b.path.rel));
    Ok(inv)
}
