//! Indexes built once per planning run from a [`PlanInput`]. Every precondition is captured
//! from these stored observations (scan session, torrent snapshot, ARR facts), never guessed.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use removarr_domain::arr::correlate::{Correlation, Correlator};
use removarr_domain::arr::{ArrMediaRef, ArrService};
use removarr_domain::fs::{
    DirSnapshot, EntryKind, EntrySignature, FsEntry, PermissionKind, PhysicalId,
};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::{RelPath, RootedPath};
use removarr_domain::plan::Precondition;
use removarr_domain::session::{ServiceKind, ServiceState};
use removarr_domain::torrent::{TorrentHash, TorrentSnapshot, TorrentState};

use super::input::PlanInput;
use super::request::PlanSettings;
use crate::issues::{
    MappingFault, Perimeter, Protection, QbitObservation, TorrentIndex, mapping_faults, perimeter,
    permission_findings, protection,
};

pub(crate) fn arr_kind(s: ArrService) -> ServiceKind {
    match s {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

pub(crate) fn short(h: &TorrentHash) -> &str {
    &h.as_str()[..8]
}

pub(crate) struct Facts<'a> {
    pub input: &'a PlanInput,
    pub settings: &'a PlanSettings,
    entries: BTreeMap<&'a RootedPath, &'a FsEntry>,
    /// Regular-file paths per inode across every inventory (accounting, never proof).
    links: BTreeMap<PhysicalId, Vec<&'a FsEntry>>,
    pub snapshot: Option<&'a TorrentSnapshot>,
    pub index: Option<TorrentIndex>,
    protections: BTreeMap<TorrentHash, Option<Protection>>,
    no_rule: usize,
    invalid: usize,
    unwritable: BTreeSet<RootedPath>,
    pub perimeter: Perimeter,
    pub corr: Correlator,
}

impl<'a> Facts<'a> {
    pub fn new(input: &'a PlanInput, settings: &'a PlanSettings) -> Self {
        let obs = &input.observation;
        let mut entries = BTreeMap::new();
        let mut links: BTreeMap<PhysicalId, Vec<&FsEntry>> = BTreeMap::new();
        let mut unwritable = BTreeSet::new();
        for inv in &obs.inventories {
            for f in permission_findings(inv, settings.files.expected_owner) {
                if f.kind == PermissionKind::ParentNotWritable {
                    unwritable.insert(f.path);
                }
            }
            for e in &inv.entries {
                entries.insert(&e.path, e);
                if e.sig.kind == EntryKind::File {
                    links.entry(e.sig.phys()).or_default().push(e);
                }
            }
        }
        let (snapshot, index, protections, no_rule, invalid) = match &obs.qbit {
            QbitObservation::Available(s) => {
                let mut idx = TorrentIndex::build(s);
                idx.bind_inventories(&obs.inventories);
                let faults = mapping_faults(s);
                let count = |f: MappingFault| {
                    faults
                        .values()
                        .filter(|set| set.iter().any(|(_, k)| *k == f))
                        .count()
                };
                let prot = s
                    .torrents
                    .iter()
                    .map(|t| {
                        let uncertain = faults
                            .get(&t.hash)
                            .is_some_and(|f| f.iter().any(|(_, k)| k.is_error()));
                        (t.hash.clone(), protection(t, s.major, uncertain))
                    })
                    .collect();
                (
                    Some(s),
                    Some(idx),
                    prot,
                    count(MappingFault::NoRule),
                    count(MappingFault::Invalid),
                )
            }
            _ => (None, None, BTreeMap::new(), 0, 0),
        };
        let mut corr = Correlator::new();
        for a in obs.arr.iter().filter(|a| a.is_ok()) {
            corr.add_queue(&a.queue);
            corr.add_history(&a.history);
            corr.add_library(&a.library);
        }
        Self {
            input,
            settings,
            entries,
            links,
            snapshot,
            index,
            protections,
            no_rule,
            invalid,
            unwritable,
            perimeter: perimeter(obs),
            corr,
        }
    }

    /// Current entry at `path`. The root directory itself has no entry: its signature is the
    /// inventory's `root_sig`.
    pub fn sig(&self, path: &RootedPath) -> Option<EntrySignature> {
        if path.rel.is_root() {
            return self
                .input
                .observation
                .inventories
                .iter()
                .find(|i| i.root == path.root)
                .and_then(|i| i.root_sig);
        }
        self.entries.get(path).map(|e| e.sig)
    }

    pub fn entry(&self, path: &RootedPath) -> Option<&'a FsEntry> {
        self.entries.get(path).copied()
    }

    /// Entries strictly below `dir`, in path order.
    pub fn descendants(&self, dir: &RootedPath) -> Vec<&'a FsEntry> {
        let Ok(upper_rel) = RelPath::new(format!("{}0", dir.rel.as_str())) else {
            return Vec::new();
        };
        let upper = RootedPath::new(dir.root.clone(), upper_rel);
        self.entries
            .range::<RootedPath, _>((Bound::Included(dir), Bound::Excluded(&upper)))
            .filter(|(p, _)| ***p != *dir && p.root == dir.root && p.rel.starts_with(&dir.rel))
            .map(|(_, e)| *e)
            .collect()
    }

    /// Every regular-file path of an inode seen in the inventories.
    pub fn links_of(&self, phys: &PhysicalId) -> &[&'a FsEntry] {
        self.links.get(phys).map_or(&[], Vec::as_slice)
    }

    /// Filesystem scope blockers for a path or subtree (OQ-05 manual rule): the root must be
    /// scanned to the end, and no scan error or protected zone may lie at, above or below it.
    pub fn scope_blockers(&self, path: &RootedPath) -> Vec<Blocker> {
        let inv = self
            .input
            .observation
            .inventories
            .iter()
            .find(|i| i.root == path.root);
        let Some(inv) = inv else {
            return vec![Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("root {} was not scanned", path.root),
            )];
        };
        if !inv.finished {
            return vec![Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("root {} scan did not finish", path.root),
            )];
        }
        let related = |p: &RootedPath| {
            p.root == path.root && (p.rel.starts_with(&path.rel) || path.rel.starts_with(&p.rel))
        };
        let mut out = Vec::new();
        if inv.errors.iter().any(|e| related(&e.path)) {
            out.push(Blocker::new(
                BlockerCode::ScanErrorUnder,
                format!("a scan error lies at or around {path}"),
            ));
        }
        if inv.protected.iter().any(|z| related(&z.path)) {
            out.push(Blocker::new(
                BlockerCode::ProtectedZone,
                format!("a protected zone lies at or around {path}"),
            ));
        }
        out
    }

    /// The directory holding `path` cannot be proven writable by the expected owner.
    pub fn parent_unwritable(&self, path: &RootedPath) -> Option<Blocker> {
        let dir = RootedPath::new(path.root.clone(), path.rel.parent()?);
        self.unwritable.contains(&dir).then(|| {
            Blocker::new(
                BlockerCode::PermissionDenied,
                format!("directory {dir} is not writable by the expected owner"),
            )
        })
    }

    /// Torrents holding `path` itself (affected hashes, CP1-F09).
    pub fn holders(&self, path: &RootedPath) -> BTreeSet<TorrentHash> {
        self.index
            .as_ref()
            .map(|i| i.hashes_for(path))
            .unwrap_or_default()
    }

    /// Torrents holding `dir` or anything below it.
    pub fn holders_under(&self, dir: &RootedPath) -> BTreeSet<TorrentHash> {
        self.index
            .as_ref()
            .map(|i| i.hashes_under(dir))
            .unwrap_or_default()
    }

    /// Torrents holding the inode through any of their paths.
    pub fn inode_holders(&self, phys: &PhysicalId) -> BTreeSet<TorrentHash> {
        self.index
            .as_ref()
            .map(|i| i.hashes_for_inode(phys))
            .unwrap_or_default()
    }

    /// Wanted files (`priority > 0`) of a torrent in the planning snapshot, as `(index, name)`
    /// pairs (`name` = the file name inside the torrent, domain patch 3). `None` when the
    /// torrent, its file list or every wanted file is missing: nothing can be pinned.
    pub fn wanted_files(&self, hash: &TorrentHash) -> Option<BTreeSet<(u32, String)>> {
        let files = self.snapshot?.files.get(hash)?;
        let wanted: BTreeSet<(u32, String)> = files
            .iter()
            .filter(|f| f.is_wanted())
            .map(|f| (f.index, f.name.clone()))
            .collect();
        (!wanted.is_empty()).then_some(wanted)
    }

    /// qBittorrent service state (CP1-F10, CON-20).
    pub fn qbit_blockers(&self) -> Vec<Blocker> {
        match &self.input.observation.qbit {
            QbitObservation::NotConfigured => vec![Blocker::new(
                BlockerCode::ServiceNotConfigured(ServiceKind::Qbittorrent),
                "torrent ownership is unknown",
            )],
            QbitObservation::Unavailable { .. } => vec![Blocker::new(
                BlockerCode::ServiceUnavailable(ServiceKind::Qbittorrent),
                "torrent ownership is unknown",
            )],
            QbitObservation::Available(_) => vec![],
        }
    }

    /// Blockers of a torrent-less deletion: ownership must be known for every torrent (OQ-05,
    /// OQ-07).
    pub fn ownership_blockers(&self) -> Vec<Blocker> {
        let mut out = self.qbit_blockers();
        if self.no_rule > 0 {
            out.push(Blocker::new(
                BlockerCode::UnmappedTorrents,
                format!("{} torrents have a path with no mapping rule", self.no_rule),
            ));
        }
        if self.invalid > 0 {
            out.push(Blocker::new(
                BlockerCode::MappingError,
                format!("{} torrents have an invalid mapped path", self.invalid),
            ));
        }
        out
    }

    /// Allowlist verdict of every holder (INV-28), and their captured states.
    pub fn torrent_states(
        &self,
        hashes: &BTreeSet<TorrentHash>,
    ) -> (BTreeMap<TorrentHash, (TorrentState, f64)>, Vec<Blocker>) {
        let mut states = BTreeMap::new();
        let mut out = if hashes.is_empty() {
            vec![]
        } else {
            self.qbit_blockers()
        };
        for h in hashes {
            match self.protections.get(h) {
                Some(p) => {
                    if let Some(p) = p {
                        out.push(Blocker::new(
                            BlockerCode::TorrentProtectedState,
                            format!("torrent {}: {}", short(h), p.detail()),
                        ));
                    }
                    if let Some(t) = self
                        .snapshot
                        .and_then(|s| s.torrents.iter().find(|t| &t.hash == h))
                    {
                        states.insert(h.clone(), (t.state.clone(), t.progress));
                    }
                }
                None => out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("torrent {}: not in the observation", short(h)),
                )),
            }
        }
        (states, out)
    }

    /// Unique ARR media for content held by `hashes` or matching `paths` (CON-17).
    pub fn identity(
        &self,
        hashes: &BTreeSet<TorrentHash>,
        paths: &[&RootedPath],
    ) -> Result<ArrMediaRef, Blocker> {
        if !self.input.observation.arr.iter().any(|a| a.is_ok()) {
            return Err(Blocker::new(
                BlockerCode::ArrMissing,
                "no ARR observation is available",
            ));
        }
        match self.corr.for_content(hashes.iter(), paths.iter().copied()) {
            Correlation::Unique(m) => Ok(ArrMediaRef {
                service: m.service,
                media_id: m.media_id,
                episode_ids: vec![],
            }),
            Correlation::Ambiguous(_) | Correlation::Unidentified => Err(Blocker::new(
                BlockerCode::ArrAmbiguous,
                "ARR identity is ambiguous",
            )),
            Correlation::NotFound => Err(Blocker::new(
                BlockerCode::ArrMissing,
                "ARR does not know this content",
            )),
        }
    }

    /// Session state of a service, as observed by the planning session.
    pub fn service_ok(&self, kind: ServiceKind) -> bool {
        self.input
            .session
            .services
            .iter()
            .any(|s| s.service == kind && s.state.is_ok())
    }

    /// Whether the service is configured at all (a configured-but-down service still blocks).
    pub fn service_configured(&self, kind: ServiceKind) -> bool {
        self.input
            .session
            .services
            .iter()
            .any(|s| s.service == kind && s.state != ServiceState::NotConfigured)
    }

    /// Folder snapshot from the inventory: keys relative to `dir`, `""` = the directory.
    pub fn dir_snapshot(&self, dir: &RootedPath) -> Option<DirSnapshot> {
        let mut entries = BTreeMap::new();
        entries.insert(RelPath::root(), self.sig(dir)?);
        for e in self.descendants(dir) {
            let rest = e
                .path
                .rel
                .as_str()
                .strip_prefix(dir.rel.as_str())
                .map(|r| r.trim_start_matches('/'))?;
            entries.insert(RelPath::new(rest).ok()?, e.sig);
        }
        Some(DirSnapshot {
            dir: dir.clone(),
            entries,
        })
    }

    /// `EntryUnchanged` for a current entry.
    pub fn unchanged(&self, path: &RootedPath, compare_nlink: bool) -> Option<Precondition> {
        Some(Precondition::EntryUnchanged {
            path: path.clone(),
            sig: self.sig(path)?,
            compare_nlink,
        })
    }
}
