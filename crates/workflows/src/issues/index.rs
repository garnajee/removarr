//! Torrent ownership: which torrents hold a path or an inode (architecture 7.1 step 3).
//!
//! Matching is by path (`save_path` + `content_path` + file list after mapping), never by
//! torrent name (INV-29). The index mirrors the connector's `PathIndex` rules, kept here because
//! this crate may not depend on `removarr-qbit` (architecture 2.2), and adds the inode view and
//! a fail-closed rule for torrents whose file list is unknown.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::fs::{EntryKind, PhysicalId, RootInventory};
use removarr_domain::path::RootedPath;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, TorrentState,
    destructive_eligible, is_working_file,
};

type Owners = BTreeMap<RootedPath, BTreeSet<TorrentHash>>;

/// Why a torrent path could not be placed under a configured root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MappingFault {
    /// No mapping rule matched (OQ-07: blocks torrent-less deletions).
    NoRule,
    /// A prefix matched but the result escapes or is malformed (CON-07).
    Invalid,
    /// Declared external: information only.
    DeclaredExternal,
    /// Mapped but outside every configured root: information only.
    OutsideRoots,
}

impl MappingFault {
    #[must_use]
    pub fn is_error(self) -> bool {
        matches!(self, MappingFault::NoRule | MappingFault::Invalid)
    }

    fn of(p: &MappedPath) -> Option<Self> {
        match p {
            MappedPath::Mapped { .. } => None,
            MappedPath::OutsideRoots { .. } => Some(Self::OutsideRoots),
            MappedPath::DeclaredExternal { .. } => Some(Self::DeclaredExternal),
            MappedPath::NoRule { .. } => Some(Self::NoRule),
            MappedPath::Invalid { .. } => Some(Self::Invalid),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRule => "no_rule",
            Self::Invalid => "invalid",
            Self::DeclaredExternal => "declared_external",
            Self::OutsideRoots => "outside_roots",
        }
    }
}

/// Where a mapping fault was found (never the raw service path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathField {
    SavePath,
    DownloadPath,
    ContentPath,
    File,
}

impl PathField {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SavePath => "save_path",
            Self::DownloadPath => "download_path",
            Self::ContentPath => "content_path",
            Self::File => "file",
        }
    }
}

/// Mapping faults of every torrent (only torrents with at least one fault appear).
#[must_use]
pub fn mapping_faults(
    snapshot: &TorrentSnapshot,
) -> BTreeMap<TorrentHash, BTreeSet<(PathField, MappingFault)>> {
    let mut out: BTreeMap<TorrentHash, BTreeSet<(PathField, MappingFault)>> = BTreeMap::new();
    for t in &snapshot.torrents {
        let mut add = |field: PathField, p: &MappedPath| {
            if let Some(f) = MappingFault::of(p) {
                out.entry(t.hash.clone()).or_default().insert((field, f));
            }
        };
        add(PathField::SavePath, &t.save_path);
        if let Some(d) = &t.download_path {
            add(PathField::DownloadPath, d);
        }
        add(PathField::ContentPath, &t.content_path);
        for f in snapshot.files.get(&t.hash).map_or(&[][..], Vec::as_slice) {
            for c in &f.candidates {
                add(PathField::File, c);
            }
        }
    }
    out
}

/// Version-aware protection verdict (stricter than `destructive_eligible`, INV-28): `pausedUP`
/// is eligible on 4.x only, `stoppedUP` on 5.x only, every unknown major or state is protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    NotComplete,
    State(TorrentState),
    StateNotValidForVersion(TorrentState),
    MappingUncertain,
}

impl Protection {
    /// Short stable text for blocker details (no paths).
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::NotComplete => "progress below 1".to_string(),
            Self::State(s) => format!("state {}", s.as_str()),
            Self::StateNotValidForVersion(s) => {
                format!(
                    "state {} is not valid for this qBittorrent version",
                    s.as_str()
                )
            }
            Self::MappingUncertain => "path mapping uncertain".to_string(),
        }
    }
}

#[must_use]
pub fn protection(info: &TorrentInfo, major: u8, mapping_uncertain: bool) -> Option<Protection> {
    if !destructive_eligible(&info.state, info.progress) {
        return Some(if info.progress >= 1.0 {
            Protection::State(info.state.clone())
        } else {
            Protection::NotComplete
        });
    }
    let version_ok = match &info.state {
        TorrentState::PausedUp => major == 4,
        TorrentState::StoppedUp => major == 5,
        _ => matches!(major, 4 | 5),
    };
    if !version_ok {
        return Some(Protection::StateNotValidForVersion(info.state.clone()));
    }
    mapping_uncertain.then_some(Protection::MappingUncertain)
}

fn within(path: &RootedPath, ancestor: &RootedPath) -> bool {
    path.root == ancestor.root && path.rel.starts_with(&ancestor.rel)
}

fn parent(p: &RootedPath) -> Option<RootedPath> {
    Some(RootedPath::new(p.root.clone(), p.rel.parent()?))
}

fn strip_last(p: &RootedPath, n: usize) -> Option<RootedPath> {
    let mut rel = p.rel.clone();
    for _ in 0..n {
        rel = rel.parent()?;
    }
    Some(RootedPath::new(p.root.clone(), rel))
}

fn add(map: &mut Owners, path: &RootedPath, hash: &TorrentHash) {
    map.entry(path.clone()).or_default().insert(hash.clone());
}

/// Path and inode ownership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TorrentIndex {
    exact: Owners,
    owned_dirs: Owners,
    parts_dirs: Owners,
    inode_owners: BTreeMap<PhysicalId, BTreeSet<TorrentHash>>,
    inode_example: BTreeMap<PhysicalId, RootedPath>,
}

impl TorrentIndex {
    /// Builds the path part. A torrent whose file list was not obtained owns its whole content
    /// directory (fail-closed: it may hold anything below).
    #[must_use]
    pub fn build(snapshot: &TorrentSnapshot) -> Self {
        let mut idx = Self::default();
        for t in &snapshot.torrents {
            match snapshot.files.get(&t.hash) {
                Some(files) if !files.is_empty() => idx.add_torrent(t, files),
                _ => idx.add_without_files(t),
            }
        }
        idx
    }

    fn add_without_files(&mut self, t: &TorrentInfo) {
        if let MappedPath::Mapped { path, .. } = &t.content_path {
            add(&mut self.exact, path, &t.hash);
            add(&mut self.owned_dirs, path, &t.hash);
        }
    }

    fn add_torrent(&mut self, t: &TorrentInfo, files: &[TorrentFile]) {
        if let MappedPath::Mapped { path, .. } = &t.content_path {
            add(&mut self.exact, path, &t.hash);
        }
        let complete = t.progress >= 1.0;
        let multi = files.len() > 1;
        for f in files {
            // Depth of the torrent's top folder inside the file name (0 = top level).
            let depth = f.name.split('/').count().saturating_sub(1);
            for c in &f.candidates {
                let MappedPath::Mapped { path, .. } = c else {
                    continue;
                };
                add(&mut self.exact, path, &t.hash);
                if complete {
                    continue;
                }
                // CP1-F21: an incomplete multi-file torrent owns its content directory and the
                // `.parts` files next to it; a single-file torrent owns only its candidates and
                // the `.parts` files in their directories, never the parent directory.
                if multi && depth > 0 {
                    if let Some(dir) = strip_last(path, depth) {
                        add(&mut self.owned_dirs, &dir, &t.hash);
                        if let Some(p) = parent(&dir) {
                            add(&mut self.parts_dirs, &p, &t.hash);
                        }
                    }
                } else if let Some(p) = parent(path) {
                    add(&mut self.parts_dirs, &p, &t.hash);
                }
            }
        }
    }

    /// Adds the inode view: the inode of every inventoried regular file held by path.
    pub fn bind_inventories(&mut self, inventories: &[RootInventory]) {
        for inv in inventories {
            for e in inv.entries.iter().filter(|e| e.sig.kind == EntryKind::File) {
                let h = self.hashes_for(&e.path);
                if h.is_empty() {
                    continue;
                }
                let phys = e.sig.phys();
                self.inode_owners.entry(phys).or_default().extend(h);
                self.inode_example
                    .entry(phys)
                    .or_insert_with(|| e.path.clone());
            }
        }
    }

    /// Torrents holding `path` itself: an exact entry, an unfinished torrent's content
    /// directory above it, or (working files) an unfinished torrent whose directory holds it.
    #[must_use]
    pub fn hashes_for(&self, path: &RootedPath) -> BTreeSet<TorrentHash> {
        let mut out = BTreeSet::new();
        if let Some(h) = self.exact.get(path) {
            out.extend(h.iter().cloned());
        }
        for (dir, h) in &self.owned_dirs {
            if within(path, dir) {
                out.extend(h.iter().cloned());
            }
        }
        if path.rel.file_name().is_some_and(is_working_file)
            && let Some(p) = parent(path)
            && let Some(h) = self.parts_dirs.get(&p)
        {
            out.extend(h.iter().cloned());
        }
        out
    }

    /// Torrents holding the inode through any of their paths.
    #[must_use]
    pub fn hashes_for_inode(&self, phys: &PhysicalId) -> BTreeSet<TorrentHash> {
        self.inode_owners.get(phys).cloned().unwrap_or_default()
    }

    /// A torrent-held path with this inode (evidence for OQ-08).
    #[must_use]
    pub fn inode_example(&self, phys: &PhysicalId) -> Option<&RootedPath> {
        self.inode_example.get(phys)
    }

    /// Every inode held by a torrent.
    #[must_use]
    pub fn held_inodes(&self) -> BTreeSet<PhysicalId> {
        self.inode_owners.keys().copied().collect()
    }

    /// Torrents affected by touching `dir` and everything below it.
    #[must_use]
    pub fn hashes_under(&self, dir: &RootedPath) -> BTreeSet<TorrentHash> {
        let mut out = self.hashes_for(dir);
        for map in [&self.exact, &self.owned_dirs, &self.parts_dirs] {
            for (p, h) in map {
                if within(p, dir) {
                    out.extend(h.iter().cloned());
                }
            }
        }
        out
    }
}
