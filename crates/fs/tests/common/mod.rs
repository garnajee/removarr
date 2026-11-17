#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use removarr_config::{RootPaths, Roots};
use removarr_domain::fs::{EntryKind, FsEntry, RootInventory};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::{FsInventory, ScanControl};
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_testkit::fixture::ParityFixture;

pub fn roots_for(completed: &Path, incomplete: Option<&Path>, media: &Path) -> Roots {
    Roots::validate(&RootPaths {
        completed: Some(completed.to_str().unwrap().into()),
        incomplete: incomplete.map(|p| p.to_str().unwrap().into()),
        media: vec![media.to_str().unwrap().into()],
    })
    .expect("valid roots")
}

pub fn fixture_roots(fx: &ParityFixture) -> Roots {
    roots_for(&fx.completed(), Some(&fx.incomplete()), &fx.medias())
}

pub fn media_id(fx: &ParityFixture) -> RootId {
    RootId::media(fx.medias().to_str().unwrap())
}

pub fn fs_for(roots: &Roots) -> ConfinedFs {
    ConfinedFs::new(roots, FsLimits::default())
}

pub fn scan(fs: &ConfinedFs, root: &RootId) -> RootInventory {
    fs.scan_root(root, &ScanControl::default()).expect("scan")
}

pub fn rp(root: &RootId, rel: &str) -> RootedPath {
    RootedPath::new(root.clone(), RelPath::new(rel).unwrap())
}

pub fn entry<'a>(inv: &'a RootInventory, rel: &str) -> &'a FsEntry {
    inv.entries
        .iter()
        .find(|e| e.path.rel.as_str() == rel)
        .unwrap_or_else(|| panic!("no entry {rel}"))
}

pub fn count(inv: &RootInventory, kind: EntryKind) -> usize {
    inv.entries.iter().filter(|e| e.sig.kind == kind).count()
}

pub fn is_root_user() -> bool {
    rustix::process::geteuid().is_root()
}

pub fn shared(fs: ConfinedFs) -> Arc<ConfinedFs> {
    Arc::new(fs)
}
