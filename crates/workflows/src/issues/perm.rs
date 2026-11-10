//! Permission and owner findings, derived from stored signatures only.
//!
//! This is the same pure rule as `FsInventory::permission_findings` (the fs adapter), repeated
//! here so a refresh needs no filesystem port at all: it reads the stored inventory and never
//! touches a disk. A test compares both implementations on the parity fixture. Nothing here
//! changes a mode or an owner (INV-32).

use std::collections::BTreeSet;

use removarr_domain::fs::{
    EntryKind, EntrySignature, PermissionFinding, PermissionKind, RootInventory,
};
use removarr_domain::path::{RelPath, RootedPath};

/// True when `(uid, gid)` can create, rename and unlink entries in a directory with this
/// signature (write and search permission, first matching permission class). Supplementary
/// groups, ACLs and read-only mounts are invisible to `stat`: the verdict means "cannot be
/// proven writable".
#[must_use]
pub fn can_modify_dir(sig: &EntrySignature, uid: u32, gid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    let class_bits = if sig.uid == uid {
        (sig.mode >> 6) & 0o7
    } else if sig.gid == gid {
        (sig.mode >> 3) & 0o7
    } else {
        sig.mode & 0o7
    };
    class_bits & 0o3 == 0o3
}

#[must_use]
pub fn permission_findings(inv: &RootInventory, owner: (u32, u32)) -> Vec<PermissionFinding> {
    let (uid, gid) = owner;
    let mut out = Vec::new();
    let parents: BTreeSet<RelPath> = inv
        .entries
        .iter()
        .filter_map(|e| e.path.rel.parent())
        .collect();
    if let Some(root_sig) = &inv.root_sig {
        dir_findings(
            &mut out,
            RootedPath::new(inv.root.clone(), RelPath::root()),
            root_sig,
            owner,
            parents.contains(&RelPath::root()),
        );
    }
    for e in &inv.entries {
        match e.sig.kind {
            EntryKind::Dir => {
                dir_findings(
                    &mut out,
                    e.path.clone(),
                    &e.sig,
                    owner,
                    parents.contains(&e.path.rel),
                );
            }
            EntryKind::File => {
                if (e.sig.uid, e.sig.gid) != (uid, gid) {
                    out.push(PermissionFinding {
                        path: e.path.clone(),
                        kind: PermissionKind::OwnerMismatch {
                            uid: e.sig.uid,
                            gid: e.sig.gid,
                        },
                    });
                }
                if e.sig.uid == uid && e.sig.mode & 0o200 == 0 {
                    out.push(PermissionFinding {
                        path: e.path.clone(),
                        kind: PermissionKind::NotWritableByOwner,
                    });
                }
            }
            EntryKind::Symlink | EntryKind::Other => {}
        }
    }
    out
}

fn dir_findings(
    out: &mut Vec<PermissionFinding>,
    path: RootedPath,
    sig: &EntrySignature,
    (uid, gid): (u32, u32),
    has_children: bool,
) {
    if (sig.uid, sig.gid) != (uid, gid) {
        out.push(PermissionFinding {
            path: path.clone(),
            kind: PermissionKind::OwnerMismatch {
                uid: sig.uid,
                gid: sig.gid,
            },
        });
    }
    if has_children && !can_modify_dir(sig, uid, gid) {
        out.push(PermissionFinding {
            path,
            kind: PermissionKind::ParentNotWritable,
        });
    }
}
