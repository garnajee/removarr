//! Host path -> every torrent hash that references it.
//!
//! Matching is by path only (`save_path` + `content_path` + file list), never by torrent name.
//! Several torrents sharing the same files all appear under the same path.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::path::RootedPath;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, is_working_file,
};

type Owners = BTreeMap<RootedPath, BTreeSet<TorrentHash>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathIndex {
    /// Every `Mapped` file candidate (with `.!qB` variants) and every `Mapped` `content_path`.
    exact: Owners,
    /// Content directories of incomplete multi-file torrents: everything below is owned.
    owned_dirs: Owners,
    /// Directories whose direct `.parts` files belong to an incomplete torrent.
    parts_dirs: Owners,
}

fn within(path: &RootedPath, ancestor: &RootedPath) -> bool {
    path.root == ancestor.root && path.rel.starts_with(&ancestor.rel)
}

fn strip_last(p: &RootedPath, n: usize) -> Option<RootedPath> {
    let mut rel = p.rel.clone();
    for _ in 0..n {
        rel = rel.parent()?;
    }
    Some(RootedPath::new(p.root.clone(), rel))
}

fn parent(p: &RootedPath) -> Option<RootedPath> {
    strip_last(p, 1)
}

impl PathIndex {
    /// Builds the index from a snapshot. Torrents without a file list contribute only their
    /// `content_path` (they own nothing they did not report).
    #[must_use]
    pub fn build(snapshot: &TorrentSnapshot) -> Self {
        let mut idx = Self::default();
        for t in &snapshot.torrents {
            let files = snapshot.files.get(&t.hash).map_or(&[][..], Vec::as_slice);
            idx.add_torrent(t, files);
        }
        idx
    }

    fn add_torrent(&mut self, t: &TorrentInfo, files: &[TorrentFile]) {
        if let MappedPath::Mapped { path, .. } = &t.content_path {
            add(&mut self.exact, path, &t.hash);
        }
        let complete = t.progress >= 1.0;
        let multi = files.len() > 1;
        for f in files {
            // Depth of the torrent's top folder inside the file name (0 = file at top level).
            let depth = f.name.split('/').count().saturating_sub(1);
            for c in &f.candidates {
                let MappedPath::Mapped { path, .. } = c else {
                    continue;
                };
                add(&mut self.exact, path, &t.hash);
                if complete {
                    continue;
                }
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

    /// Hashes owning `path` itself: an exact entry, an incomplete torrent's content directory
    /// above it, or (for `.parts` files) an incomplete torrent whose directory contains it.
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

    /// Hashes affected by removing or touching `dir` and everything below it.
    #[must_use]
    pub fn hashes_under(&self, dir: &RootedPath) -> BTreeSet<TorrentHash> {
        let mut out = self.hashes_for(dir);
        for map in [&self.exact, &self.owned_dirs] {
            for (p, h) in map {
                if within(p, dir) {
                    out.extend(h.iter().cloned());
                }
            }
        }
        for (p, h) in &self.parts_dirs {
            if within(p, dir) {
                out.extend(h.iter().cloned());
            }
        }
        out
    }

    /// Union of `hashes_for` over several paths (sources and targets of one action).
    #[must_use]
    pub fn hashes_for_all<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a RootedPath>,
    ) -> BTreeSet<TorrentHash> {
        paths.into_iter().flat_map(|p| self.hashes_for(p)).collect()
    }

    /// True when no torrent references the path (`NoTorrentReferences`).
    #[must_use]
    pub fn is_unreferenced(&self, path: &RootedPath) -> bool {
        self.hashes_for(path).is_empty()
    }
}

fn add(map: &mut Owners, path: &RootedPath, hash: &TorrentHash) {
    map.entry(path.clone()).or_default().insert(hash.clone());
}
