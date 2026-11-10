//! Classification context: indexes built once from an [`Observation`], and the issue builder.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use removarr_domain::arr::correlate::Correlator;
use removarr_domain::evidence::{Completeness, Evidence, EvidenceKind, IdentityProof, Provenance};
use removarr_domain::fs::{
    EntryKind, EntrySignature, FsEntry, PermissionFinding, PermissionKind, PhysicalId,
};
use removarr_domain::gain::estimate_reclaimable;
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Outcome, Severity, Unit};
use removarr_domain::path::{RelPath, RootId, RootKind, RootedPath};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{TorrentHash, TorrentInfo, TorrentSnapshot};

use super::files::stem;
use super::index::{MappingFault, PathField, Protection, TorrentIndex, mapping_faults, protection};
use super::input::{IssueSettings, Observation, QbitObservation};
use super::perm::permission_findings;
use super::scope::Scope;

/// Prefix of `Evidence` entries that carry a short stable note instead of a typed fact. The
/// domain `EvidenceKind` has no note variant, so a note is an `Identity` entry whose proof is
/// `IdentityProof::None` (it asserts nothing about identity). Consumers match the prefix.
pub const NOTE_PREFIX: &str = "note:";

pub(crate) struct Ctx<'a> {
    pub obs: &'a Observation,
    pub settings: &'a IssueSettings,
    pub scope: Scope<'a>,
    /// Every inventoried entry by path.
    pub entries: BTreeMap<&'a RootedPath, &'a FsEntry>,
    /// Regular files per root kind, sorted by path.
    pub completed_files: Vec<&'a FsEntry>,
    pub media_files: Vec<&'a FsEntry>,
    pub incomplete_files: Vec<&'a FsEntry>,
    pub completed_phys: BTreeSet<PhysicalId>,
    pub media_phys: BTreeSet<PhysicalId>,
    /// Regular-file paths seen per inode across every inventory (accounting, never proof).
    pub links_seen: BTreeMap<PhysicalId, u32>,
    pub snapshot: Option<&'a TorrentSnapshot>,
    pub index: Option<TorrentIndex>,
    pub faults: BTreeMap<TorrentHash, BTreeSet<(PathField, MappingFault)>>,
    pub protections: BTreeMap<TorrentHash, Option<Protection>>,
    pub findings: Vec<PermissionFinding>,
    pub unwritable_dirs: BTreeSet<RootedPath>,
    pub corr: Correlator,
    /// Lower-cased video stems per directory (orphan subtitle rule).
    pub video_stems: BTreeMap<(RootId, RelPath), BTreeSet<String>>,
}

pub(crate) fn mtime_ms(sig: &EntrySignature) -> Timestamp {
    Timestamp(sig.mtime_ns.0.div_euclid(1_000_000))
}

impl<'a> Ctx<'a> {
    pub fn new(obs: &'a Observation, settings: &'a IssueSettings) -> Self {
        let scope = Scope::new(obs);
        let mut entries = BTreeMap::new();
        let (mut completed_files, mut media_files, mut incomplete_files) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut links_seen: BTreeMap<PhysicalId, u32> = BTreeMap::new();
        let mut video_stems: BTreeMap<(RootId, RelPath), BTreeSet<String>> = BTreeMap::new();
        let mut findings = Vec::new();
        for inv in &obs.inventories {
            findings.extend(permission_findings(inv, settings.expected_owner));
            for e in &inv.entries {
                entries.insert(&e.path, e);
                if e.sig.kind != EntryKind::File {
                    continue;
                }
                match inv.root.kind() {
                    RootKind::Completed => completed_files.push(e),
                    RootKind::Media => media_files.push(e),
                    RootKind::Incomplete => incomplete_files.push(e),
                }
                let phys = e.sig.phys();
                *links_seen.entry(phys).or_default() += 1;
                if let Some(name) = e.path.rel.file_name()
                    && settings.is_video(name)
                    && let Some(dir) = e.path.rel.parent()
                {
                    video_stems
                        .entry((inv.root.clone(), dir))
                        .or_default()
                        .insert(stem(name).to_ascii_lowercase());
                }
            }
        }
        for v in [
            &mut completed_files,
            &mut media_files,
            &mut incomplete_files,
        ] {
            v.sort_by(|a, b| a.path.cmp(&b.path));
        }
        let completed_phys = completed_files.iter().map(|e| e.sig.phys()).collect();
        let media_phys = media_files.iter().map(|e| e.sig.phys()).collect();
        let unwritable_dirs = findings
            .iter()
            .filter(|f| f.kind == PermissionKind::ParentNotWritable)
            .map(|f| f.path.clone())
            .collect();

        let (snapshot, index, faults, protections) = match &obs.qbit {
            QbitObservation::Available(s) => {
                let mut idx = TorrentIndex::build(s);
                idx.bind_inventories(&obs.inventories);
                let faults = mapping_faults(s);
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
                (Some(s), Some(idx), faults, prot)
            }
            _ => (None, None, BTreeMap::new(), BTreeMap::new()),
        };

        let mut corr = Correlator::new();
        for a in obs.arr.iter().filter(|a| a.is_ok()) {
            corr.add_queue(&a.queue);
            corr.add_history(&a.history);
            corr.add_library(&a.library);
        }

        Self {
            obs,
            settings,
            scope,
            entries,
            completed_files,
            media_files,
            incomplete_files,
            completed_phys,
            media_phys,
            links_seen,
            snapshot,
            index,
            faults,
            protections,
            findings,
            unwritable_dirs,
            corr,
            video_stems,
        }
    }

    pub fn torrent(&self, hash: &TorrentHash) -> Option<&'a TorrentInfo> {
        self.snapshot?.torrents.iter().find(|t| &t.hash == hash)
    }

    pub fn owners_of_path(&self, path: &RootedPath) -> BTreeSet<TorrentHash> {
        self.index
            .as_ref()
            .map(|i| i.hashes_for(path))
            .unwrap_or_default()
    }

    /// Torrents holding the path or its inode.
    pub fn owners_of_entry(&self, e: &FsEntry) -> BTreeSet<TorrentHash> {
        let Some(i) = &self.index else {
            return BTreeSet::new();
        };
        let mut h = i.hashes_for(&e.path);
        h.extend(i.hashes_for_inode(&e.sig.phys()));
        h
    }

    /// Entries strictly below `dir`, in path order (the subtree of a directory unit).
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

    pub fn held_inodes(&self) -> BTreeSet<PhysicalId> {
        self.index
            .as_ref()
            .map(TorrentIndex::held_inodes)
            .unwrap_or_default()
    }

    /// qBittorrent service state as a blocker (CP1-F10, CON-20).
    pub fn qbit_service_blockers(&self) -> Vec<Blocker> {
        match &self.obs.qbit {
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

    /// Blockers for any torrent-less deletion (orphans, junk, incomplete orphans): the service
    /// state and unmapped torrents (OQ-05, OQ-07), because ownership is then uncertain.
    pub fn ownership_blockers(&self) -> Vec<Blocker> {
        let mut out = self.qbit_service_blockers();
        let (mut no_rule, mut invalid) = (0usize, 0usize);
        for set in self.faults.values() {
            no_rule += usize::from(set.iter().any(|(_, k)| *k == MappingFault::NoRule));
            invalid += usize::from(set.iter().any(|(_, k)| *k == MappingFault::Invalid));
        }
        if no_rule > 0 {
            out.push(Blocker::new(
                BlockerCode::UnmappedTorrents,
                format!("{no_rule} torrents have a path with no mapping rule"),
            ));
        }
        if invalid > 0 {
            out.push(Blocker::new(
                BlockerCode::MappingError,
                format!("{invalid} torrents have an invalid mapped path"),
            ));
        }
        out
    }

    /// Blockers for an action that touches content held by `hashes`: the service state and the
    /// allowlist verdict of every holding torrent (INV-28).
    pub fn torrent_blockers(&self, hashes: &BTreeSet<TorrentHash>) -> Vec<Blocker> {
        let mut out = self.qbit_service_blockers();
        for h in hashes {
            match self.protections.get(h) {
                Some(Some(p)) => out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("torrent {}: {}", short(h), p.detail()),
                )),
                Some(None) => {}
                // A holder that is not in the snapshot cannot be classified: protected.
                None => out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("torrent {}: not in the observation", short(h)),
                )),
            }
        }
        out
    }

    /// Potentially recoverable bytes and the inodes behind them (DR-04, T-03). Each inode counts
    /// once, only when every link seen is selected and no other holder exists.
    pub fn potential(
        &self,
        selected: &[&FsEntry],
        held_elsewhere: &BTreeSet<PhysicalId>,
    ) -> (u64, BTreeSet<PhysicalId>) {
        let mut sel: BTreeMap<PhysicalId, (u32, u64, u64)> = BTreeMap::new();
        for e in selected.iter().filter(|e| e.sig.kind == EntryKind::File) {
            let slot = sel.entry(e.sig.phys()).or_insert((
                0,
                e.sig
                    .nlink
                    .max(u64::from(self.links_seen_of(&e.sig.phys()))),
                e.allocated,
            ));
            slot.0 += 1;
        }
        let bytes = estimate_reclaimable(&sel, held_elsewhere);
        let phys = sel
            .iter()
            .filter(|(p, (links, nlink, _))| {
                !held_elsewhere.contains(p) && *links > 0 && u64::from(*links) >= *nlink
            })
            .map(|(p, _)| *p)
            .collect();
        (bytes, phys)
    }

    fn links_seen_of(&self, p: &PhysicalId) -> u32 {
        self.links_seen.get(p).copied().unwrap_or(0)
    }

    pub fn fs_evidence(&self, e: &FsEntry) -> Evidence {
        Evidence {
            kind: EvidenceKind::Physical {
                path: e.path.clone(),
                sig: e.sig,
            },
            provenance: Provenance::Filesystem {
                session: self.obs.session,
            },
            observed_at: self.obs.observed_at,
            completeness: self.scope.completeness(&e.path),
        }
    }

    pub fn qbit_evidence(&self, t: &TorrentInfo, path: RootedPath) -> Evidence {
        Evidence {
            kind: EvidenceKind::TorrentHolds {
                hash: t.hash.clone(),
                state: t.state.clone(),
                progress: t.progress,
                path,
            },
            provenance: Provenance::Qbittorrent {
                taken_at: self.snapshot.map_or(self.obs.observed_at, |s| s.taken_at),
            },
            observed_at: self.snapshot.map_or(self.obs.observed_at, |s| s.taken_at),
            completeness: Completeness::Complete,
        }
    }

    /// A short stable note (see [`NOTE_PREFIX`]).
    pub fn note(
        &self,
        text: impl Into<String>,
        provenance: Provenance,
        completeness: Completeness,
    ) -> Evidence {
        Evidence {
            kind: EvidenceKind::Identity {
                subject: format!("{NOTE_PREFIX}{}", text.into()),
                proof: IdentityProof::None,
            },
            provenance,
            observed_at: self.obs.observed_at,
            completeness,
        }
    }

    pub fn fs_provenance(&self) -> Provenance {
        Provenance::Filesystem {
            session: self.obs.session,
        }
    }

    /// Completeness of evidence that rests on torrent ownership.
    pub fn ownership_completeness(&self) -> Completeness {
        match &self.obs.qbit {
            QbitObservation::Available(_) => Completeness::Complete,
            QbitObservation::NotConfigured => Completeness::Incomplete {
                missing: vec!["qbittorrent not configured".into()],
            },
            QbitObservation::Unavailable { .. } => Completeness::Incomplete {
                missing: vec!["qbittorrent unavailable".into()],
            },
        }
    }
}

/// First eight characters of a hash, for blocker details.
pub(crate) fn short(h: &TorrentHash) -> &str {
    &h.as_str()[..8]
}

/// An issue under construction.
pub(crate) struct Draft {
    pub category: IssueCategory,
    pub subject: SubjectKey,
    pub unit: Unit,
    pub severity: Severity,
    pub outcome: Outcome,
    pub evidence: Vec<Evidence>,
    pub blockers: Vec<Blocker>,
    pub torrent_hashes: BTreeSet<TorrentHash>,
    pub logical_bytes: u64,
    pub potential_bytes: u64,
    pub age_basis: Timestamp,
    pub physical_ids: BTreeSet<PhysicalId>,
}

impl Draft {
    pub fn new(
        category: IssueCategory,
        subject: SubjectKey,
        unit: Unit,
        severity: Severity,
        age_basis: Timestamp,
    ) -> Self {
        Self {
            category,
            subject,
            unit,
            severity,
            outcome: Outcome::KeepManual,
            evidence: Vec::new(),
            blockers: Vec::new(),
            torrent_hashes: BTreeSet::new(),
            logical_bytes: 0,
            potential_bytes: 0,
            age_basis,
            physical_ids: BTreeSet::new(),
        }
    }

    pub fn block(&mut self, b: impl IntoIterator<Item = Blocker>) {
        self.blockers.extend(b);
    }

    /// Finalizes: derives the deterministic id, normalizes blockers and applies the ignore list.
    /// A subject, or any member of its unit, on the ignore list marks the whole issue ignored
    /// (it leaves lists, bulk actions and automatic mode; the Ignored view shows it).
    pub fn finish(mut self, ctx: &Ctx<'_>) -> Issue {
        for s in related_subjects(&self.subject, &self.unit) {
            if ctx.obs.ignored.contains(&s) {
                self.blockers.push(Blocker::new(
                    BlockerCode::Ignored,
                    if s == self.subject {
                        "subject is on the ignore list".to_string()
                    } else {
                        format!("member {} is on the ignore list", s.as_str())
                    },
                ));
            }
        }
        self.blockers
            .sort_by(|a, b| (&a.code, &a.detail).cmp(&(&b.code, &b.detail)));
        self.blockers.dedup();
        Issue {
            id: IssueId::derive(self.category, &self.subject),
            category: self.category,
            subject: self.subject,
            unit: self.unit,
            severity: self.severity,
            outcome: self.outcome,
            evidence: self.evidence,
            blockers: self.blockers,
            torrent_hashes: self.torrent_hashes,
            logical_bytes: self.logical_bytes,
            potential_bytes: self.potential_bytes,
            age_basis: self.age_basis,
            first_seen_at: ctx.obs.observed_at,
            session: ctx.obs.session,
            physical_ids: self.physical_ids,
        }
    }
}

/// Subjects that, when ignored, ignore the issue.
fn related_subjects(subject: &SubjectKey, unit: &Unit) -> Vec<SubjectKey> {
    let mut out = vec![subject.clone()];
    match unit {
        Unit::File(p) => out.push(SubjectKey::rooted(p)),
        Unit::Folder {
            dir,
            members,
            annexes,
        } => {
            out.push(SubjectKey::rooted(dir));
            out.extend(members.iter().chain(annexes).map(SubjectKey::rooted));
        }
        Unit::Torrent(h) => out.push(SubjectKey::torrent(h)),
        Unit::Pair { a, b } => out.extend([SubjectKey::rooted(a), SubjectKey::rooted(b)]),
    }
    out.sort();
    out.dedup();
    out
}
