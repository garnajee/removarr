//! Pure accounting over scanned inventories: physical identity groups and unique blocks.
//!
//! Hardlink proof is the same `(dev, ino)` and nothing else (INV-01). `nlink` is carried as
//! data, never used as a proof. Allocated blocks of an inode are counted once however many
//! paths (in any number of roots) point to it.

use std::collections::BTreeMap;

use removarr_domain::fs::{EntryKind, FsEntry, PhysicalId, RootInventory};
use removarr_domain::path::RootedPath;

/// All paths seen for one physical inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalGroup {
    pub id: PhysicalId,
    /// Logical size of the inode (every path has the same one).
    pub size: u64,
    /// `st_blocks * 512`, counted once for the group.
    pub allocated: u64,
    /// `st_nlink` as last observed. Informational only.
    pub nlink: u64,
    /// Paths of the inode inside the inventories, sorted.
    pub paths: Vec<RootedPath>,
}

/// Groups regular files by `(dev, ino)` across one or several inventories.
#[must_use]
pub fn group_physical<'a>(
    inventories: impl IntoIterator<Item = &'a RootInventory>,
) -> BTreeMap<PhysicalId, PhysicalGroup> {
    let mut groups: BTreeMap<PhysicalId, PhysicalGroup> = BTreeMap::new();
    for inv in inventories {
        for e in inv.entries.iter().filter(|e| is_regular(e)) {
            let id = e.sig.phys();
            let g = groups.entry(id).or_insert_with(|| PhysicalGroup {
                id,
                size: e.sig.size,
                allocated: e.allocated,
                nlink: e.sig.nlink,
                paths: Vec::new(),
            });
            g.paths.push(e.path.clone());
        }
    }
    for g in groups.values_mut() {
        g.paths.sort();
        g.paths.dedup();
    }
    groups
}

fn is_regular(e: &FsEntry) -> bool {
    e.sig.kind == EntryKind::File
}

/// Allocated bytes of the distinct inodes of regular files (each counted once).
#[must_use]
pub fn unique_allocated_bytes<'a>(inventories: impl IntoIterator<Item = &'a RootInventory>) -> u64 {
    group_physical(inventories)
        .values()
        .map(|g| g.allocated)
        .sum()
}

/// Sum of logical sizes of every regular-file path (a hardlinked file counts per path).
#[must_use]
pub fn logical_bytes(inv: &RootInventory) -> u64 {
    inv.entries
        .iter()
        .filter(|e| is_regular(e))
        .map(|e| e.sig.size)
        .sum()
}

/// Inodes present in both inventory sets (the hardlink proof between two trees).
#[must_use]
pub fn shared_between(a: &RootInventory, b: &RootInventory) -> Vec<PhysicalId> {
    let left = group_physical([a]);
    let right = group_physical([b]);
    left.keys()
        .filter(|k| right.contains_key(k))
        .copied()
        .collect()
}
