//! Ownership and permission anomaly detection. Pure: it reads signatures already in the
//! inventory and never touches the filesystem, never changes ownership or modes.
//!
//! Heuristic limits (documented for the issue engine): classic owner/group/other mode bits are
//! evaluated for the expected identity. Supplementary groups, POSIX ACLs, immutable flags and
//! read-only mounts are invisible to `stat`, so the verdict is "cannot be proven writable".

use std::collections::BTreeSet;

use removarr_domain::fs::{
    EntryKind, EntrySignature, PermissionFinding, PermissionKind, RootInventory,
};
use removarr_domain::path::{RelPath, RootedPath};

/// True when `(uid, gid)` can create, rename and unlink entries in a directory with this
/// signature (write and search permission), using the first matching permission class.
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
pub fn findings(inv: &RootInventory, owner: (u32, u32)) -> Vec<PermissionFinding> {
    let (uid, gid) = owner;
    let mut out = Vec::new();

    // Directories (with the root as ""), and the set of those that contain something.
    let mut parents_with_children: BTreeSet<RelPath> = BTreeSet::new();
    for e in &inv.entries {
        if let Some(p) = e.path.rel.parent() {
            parents_with_children.insert(p);
        }
    }

    if let Some(root_sig) = &inv.root_sig {
        let root_path = RootedPath::new(inv.root.clone(), RelPath::root());
        push_dir_findings(
            &mut out,
            root_path,
            root_sig,
            uid,
            gid,
            parents_with_children.contains(&RelPath::root()),
        );
    }
    for e in &inv.entries {
        match e.sig.kind {
            EntryKind::Dir => push_dir_findings(
                &mut out,
                e.path.clone(),
                &e.sig,
                uid,
                gid,
                parents_with_children.contains(&e.path.rel),
            ),
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
            // Symlink ownership is irrelevant to unlink and rename; other kinds are never acted on.
            EntryKind::Symlink | EntryKind::Other => {}
        }
    }
    out
}

fn push_dir_findings(
    out: &mut Vec<PermissionFinding>,
    path: RootedPath,
    sig: &EntrySignature,
    uid: u32,
    gid: u32,
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
