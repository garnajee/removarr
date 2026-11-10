#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use removarr_config::{RootPaths, Roots};
use removarr_domain::fs::{EntrySignature, RootInventory};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::ports::{Clock, FsInventory, ScanControl};
use removarr_domain::time::Timestamp;
use removarr_fs::{ConfinedFs, FsLimits};

pub fn roots(completed: &Path, media: &Path) -> Roots {
    Roots::validate(&RootPaths {
        completed: Some(completed.to_str().unwrap().into()),
        incomplete: None,
        media: vec![media.to_str().unwrap().into()],
    })
    .expect("valid roots")
}

pub fn media_id(media: &Path) -> RootId {
    RootId::media(media.to_str().unwrap())
}

pub fn rp(root: &RootId, rel: &str) -> RootedPath {
    RootedPath::new(root.clone(), RelPath::new(rel).unwrap())
}

pub fn scan(fs: &ConfinedFs, root: &RootId) -> RootInventory {
    fs.scan_root(root, &ScanControl::default()).expect("scan")
}

pub fn stat(fs: &ConfinedFs, p: &RootedPath) -> EntrySignature {
    fs.stat(p).unwrap().unwrap()
}

/// Two configured roots in a fresh canonical temporary directory.
pub struct Tree {
    _tmp: tempfile::TempDir,
    pub completed: PathBuf,
    pub media: PathBuf,
}

impl Tree {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let completed = base.join("completed");
        let media = base.join("media");
        std::fs::create_dir_all(&completed).unwrap();
        std::fs::create_dir_all(&media).unwrap();
        Self {
            _tmp: tmp,
            completed,
            media,
        }
    }

    pub fn write(&self, root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    pub fn fs(&self) -> ConfinedFs {
        ConfinedFs::new(&roots(&self.completed, &self.media), FsLimits::default())
    }

    pub fn media_id(&self) -> RootId {
        media_id(&self.media)
    }
}

pub struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp(1_700_000_000_000)
    }
}
