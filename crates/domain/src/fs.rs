//! Filesystem observations: physical identity, signatures, inventories, errors.
//!
//! Physical identity (`dev`, `ino`) is the only hardlink proof. `nlink` is accounting data and
//! never proves a hardlink (INV-01).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::path::{RelPath, RootId, RootedPath};
use crate::time::MtimeNs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Physical identity of an inode. Two paths are hardlinks of each other iff this is equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PhysicalId {
    pub dev: u64,
    pub ino: u64,
}

/// What `lstat` reported for an entry at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrySignature {
    pub kind: EntryKind,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub nlink: u64,
    pub mtime_ns: MtimeNs,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl EntrySignature {
    #[must_use]
    pub fn phys(&self) -> PhysicalId {
        PhysicalId {
            dev: self.dev,
            ino: self.ino,
        }
    }

    /// Revalidation equality: kind, dev, ino, size, mtime, mode, owner. `nlink` is compared only
    /// when `compare_nlink` is set (deletions, CON-14).
    #[must_use]
    pub fn matches(&self, other: &EntrySignature, compare_nlink: bool) -> bool {
        let mut a = *self;
        let mut b = *other;
        if !compare_nlink {
            a.nlink = 0;
            b.nlink = 0;
        }
        a == b
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    pub path: RootedPath,
    pub sig: EntrySignature,
    /// `st_blocks * 512`.
    pub allocated: u64,
    /// Display only. Symlinks are never followed.
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoKind {
    NotFound,
    PermissionDenied,
    CrossDevice,
    AlreadyExists,
    NotADirectory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanErrorRecord {
    pub path: RootedPath,
    pub io: IoKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedReason {
    QbitInternal,
    MountBoundary,
    NonUtf8Name,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedZone {
    pub path: RootedPath,
    pub reason: ProtectedReason,
}

/// Result of walking one root. `finished = false` means the walk was interrupted: the inventory
/// is incomplete and must never feed a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootInventory {
    pub root: RootId,
    pub root_sig: Option<EntrySignature>,
    pub entries: Vec<FsEntry>,
    pub errors: Vec<ScanErrorRecord>,
    pub protected: Vec<ProtectedZone>,
    pub finished: bool,
}

impl RootInventory {
    /// A scan is trustworthy for mutation only when finished and error free.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.finished
            && self.errors.is_empty()
            && !self
                .protected
                .iter()
                .any(|z| z.reason != ProtectedReason::QbitInternal)
    }

    /// True if a scan error or any protected zone lies at or under `dir` (or above it).
    #[must_use]
    pub fn has_issue_under(&self, dir: &RelPath) -> bool {
        let related = |p: &RootedPath| {
            p.root == self.root && (p.rel.starts_with(dir) || dir.starts_with(&p.rel))
        };
        self.errors.iter().any(|e| related(&e.path))
            || self.protected.iter().any(|z| related(&z.path))
    }
}

/// Signatures of a directory and of everything below it, recursively (keys relative to the
/// directory; `""` is the directory itself). Folder removal revalidates this whole subtree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirSnapshot {
    pub dir: RootedPath,
    pub entries: BTreeMap<RelPath, EntrySignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionKind {
    OwnerMismatch { uid: u32, gid: u32 },
    ParentNotWritable,
    NotWritableByOwner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionFinding {
    pub path: RootedPath,
    pub kind: PermissionKind,
}

/// Bounded content sampling: `count` windows of `size` bytes. Samples only eliminate candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleSpec {
    pub size: u32,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOutcome {
    Identical,
    Differs {
        offset: u64,
    },
    /// A signature changed while reading: the comparison proves nothing.
    Changed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
    pub unchanged: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sig(dev: u64, ino: u64, size: u64, nlink: u64) -> EntrySignature {
        EntrySignature {
            kind: EntryKind::File,
            dev,
            ino,
            size,
            nlink,
            mtime_ns: MtimeNs(1_000),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        }
    }

    #[test]
    fn physical_identity_ignores_nlink() {
        let a = sig(1, 5, 10, 1);
        let b = sig(1, 5, 10, 2);
        assert_eq!(a.phys(), b.phys());
        assert!(a.matches(&b, false));
        assert!(!a.matches(&b, true));
        assert!(!sig(1, 5, 10, 1).matches(&sig(2, 5, 10, 1), false));
        assert!(!sig(1, 5, 10, 1).matches(&sig(1, 5, 11, 1), false));
    }

    #[test]
    fn inventory_cleanliness() {
        let root = RootId::completed();
        let p = |s: &str| RootedPath::new(root.clone(), RelPath::new(s).unwrap());
        let mut inv = RootInventory {
            root: root.clone(),
            root_sig: None,
            entries: vec![],
            errors: vec![],
            protected: vec![ProtectedZone {
                path: p("BT_backup"),
                reason: ProtectedReason::QbitInternal,
            }],
            finished: true,
        };
        assert!(inv.is_clean());
        inv.errors.push(ScanErrorRecord {
            path: p("a/b"),
            io: IoKind::PermissionDenied,
            message: String::new(),
        });
        assert!(!inv.is_clean());
        assert!(inv.has_issue_under(&RelPath::new("a").unwrap()));
        assert!(inv.has_issue_under(&RelPath::new("a/b/c").unwrap()));
        assert!(!inv.has_issue_under(&RelPath::new("z").unwrap()));
        inv.errors.clear();
        inv.finished = false;
        assert!(!inv.is_clean());
    }
}
