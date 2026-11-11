//! Live revalidation of every precondition (architecture 7.4, CON-11): real `stat` and
//! `snapshot_dir`, a fresh torrent listing and file lists, ARR queue and fingerprints, the
//! config fingerprint and the stored scan state. Nothing adapts: any difference is a blocker.
//!
//! The plan has no slot for a torrent's wanted-file set (12 handoff, follow-up 4), so for
//! every affected hash the live wanted set is compared with the stored snapshot's, and the hash
//! must still hold one of the action's paths.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::correlate::import_in_progress;
use removarr_domain::arr::{ArrMediaRef, ArrQueueItem, ArrService};
use removarr_domain::error::PortError;
use removarr_domain::fs::{EntryKind, EntrySignature, RootInventory};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::plan::{ActionTarget, PlannedAction, Precondition};
use removarr_domain::session::{AuditSession, ServiceKind};
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, TorrentState,
    is_busy_for_incomplete_cleanup,
};

use super::journal::Ctx;
use crate::issues::{TorrentIndex, protection};

/// Paths an action reads or writes on disk.
#[must_use]
pub(crate) fn action_paths(a: &PlannedAction) -> Vec<RootedPath> {
    match &a.target {
        ActionTarget::Consolidate { source, target } => vec![source.clone(), target.clone()],
        ActionTarget::Path { path } | ActionTarget::FixMode { path, .. } => vec![path.clone()],
        ActionTarget::Torrent { data_paths, .. } => data_paths.clone(),
        ActionTarget::Replace { request } => request.files.iter().map(|(p, _)| p.clone()).collect(),
        _ => Vec::new(),
    }
}

fn service_of(s: ArrService) -> ServiceKind {
    match s {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

fn changed(path: &RootedPath, what: &str) -> Blocker {
    Blocker::new(
        BlockerCode::ScanIncomplete,
        format!("{what} changed since the plan was built: {path}; rescan and re-plan"),
    )
}

fn unavailable(k: ServiceKind, e: &PortError) -> Blocker {
    Blocker::new(
        BlockerCode::ServiceUnavailable(k),
        format!("{} did not answer: {}", k.as_str(), e.detail),
    )
}

/// Major version from `app/version` text (`v5.0.4`, `4.6.7`). Unknown -> 0 (protects all).
fn major_of(version: &str) -> u8 {
    version
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

/// Orchestrator rule (12 follow-up 4): same eligibility class under the version-aware
/// allowlist and same progress; a protected torrent that became busy also changed.
fn torrent_changed(t: &TorrentInfo, major: u8, state: &TorrentState, progress: f64) -> bool {
    let mut planned = t.clone();
    planned.state = state.clone();
    planned.progress = progress;
    let eligible = |x: &TorrentInfo| protection(x, major, false).is_none();
    eligible(&planned) != eligible(t)
        || (t.progress - progress).abs() > f64::EPSILON
        || (is_busy_for_incomplete_cleanup(&t.state) && !is_busy_for_incomplete_cleanup(state))
}

fn wanted_set(files: &[TorrentFile]) -> BTreeSet<(u32, String)> {
    files
        .iter()
        .filter(|f| f.is_wanted())
        .map(|f| (f.index, f.name.clone()))
        .collect()
}

fn holds(t: &TorrentInfo, files: &[TorrentFile], paths: &[RootedPath]) -> bool {
    let under_content = |p: &RootedPath| match &t.content_path {
        MappedPath::Mapped { path: c, .. } => p.root == c.root && p.rel.starts_with(&c.rel),
        _ => false,
    };
    paths.iter().any(|p| {
        under_content(p)
            || files.iter().any(|f| {
                f.candidates
                    .iter()
                    .any(|c| matches!(c, MappedPath::Mapped { path, .. } if path == p))
            })
    })
}

/// Fresh reads of one revalidation pass, taken lazily and at most once per pass.
#[derive(Default)]
pub(crate) struct LiveReads {
    pub torrents: Option<Result<(Vec<TorrentInfo>, u8), PortError>>,
    files: BTreeMap<TorrentHash, Result<Vec<TorrentFile>, PortError>>,
    stored: Option<Option<TorrentSnapshot>>,
    session: Option<Option<AuditSession>>,
    inventories: BTreeMap<RootId, Option<RootInventory>>,
    queues: BTreeMap<ArrService, Result<Vec<ArrQueueItem>, PortError>>,
    fingerprints: BTreeMap<(ArrService, i64), Result<String, PortError>>,
    status: BTreeMap<ServiceKind, Result<(), PortError>>,
}

impl LiveReads {
    /// The listing of this pass (empty when qBittorrent did not answer).
    pub fn listing(&self) -> &[TorrentInfo] {
        match &self.torrents {
            Some(Ok((l, _))) => l,
            _ => &[],
        }
    }
}

impl Ctx<'_> {
    async fn torrents(&self, live: &mut LiveReads) -> Result<(Vec<TorrentInfo>, u8), PortError> {
        if live.torrents.is_none() {
            let r = match &self.ports.qbit {
                None => Err(PortError::new(
                    removarr_domain::error::ErrorClass::Config,
                    "qBittorrent is not configured",
                )),
                Some(q) => match q.version().await {
                    Err(e) => Err(e),
                    Ok(v) => q.torrents().await.map(|l| (l, major_of(&v))),
                },
            };
            live.torrents = Some(r);
        }
        live.torrents.clone().unwrap_or_else(|| {
            Err(PortError::new(
                removarr_domain::error::ErrorClass::Internal,
                "",
            ))
        })
    }

    async fn files(
        &self,
        live: &mut LiveReads,
        hash: &TorrentHash,
    ) -> Result<Vec<TorrentFile>, PortError> {
        if !live.files.contains_key(hash) {
            let r = match &self.ports.qbit {
                None => Err(PortError::new(
                    removarr_domain::error::ErrorClass::Config,
                    "qBittorrent is not configured",
                )),
                Some(q) => q.files(hash).await,
            };
            live.files.insert(hash.clone(), r);
        }
        live.files.get(hash).cloned().unwrap_or_else(|| {
            Err(PortError::new(
                removarr_domain::error::ErrorClass::Internal,
                "",
            ))
        })
    }

    async fn stored_snapshot(
        &self,
        live: &mut LiveReads,
    ) -> Result<Option<TorrentSnapshot>, PortError> {
        if live.stored.is_none() {
            live.stored = Some(self.ports.snapshots.latest_torrents().await?);
        }
        Ok(live.stored.clone().unwrap_or(None))
    }

    async fn stat(&self, path: &RootedPath) -> Result<Option<EntrySignature>, PortError> {
        let fs = self.ports.fs.clone();
        let p = path.clone();
        self.blocking(move || fs.stat(&p)).await
    }

    async fn service_ok(&self, live: &mut LiveReads, k: ServiceKind) -> Result<(), PortError> {
        if let Some(r) = live.status.get(&k) {
            return r.clone();
        }
        let not_configured = || {
            PortError::new(
                removarr_domain::error::ErrorClass::Config,
                format!("{} is not configured", k.as_str()),
            )
        };
        let r = match k {
            ServiceKind::Qbittorrent => match &self.ports.qbit {
                Some(q) => q.version().await.map(|_| ()),
                None => Err(not_configured()),
            },
            ServiceKind::Sonarr | ServiceKind::Radarr => {
                let want = if k == ServiceKind::Sonarr {
                    ArrService::Sonarr
                } else {
                    ArrService::Radarr
                };
                match self.ports.arr.iter().find(|a| a.service() == want) {
                    Some(a) => a.status().await.map(|_| ()),
                    None => Err(not_configured()),
                }
            }
            ServiceKind::Jellyfin => match &self.ports.jellyfin {
                Some(j) => j.status().await.map(|_| ()),
                None => Err(not_configured()),
            },
        };
        live.status.insert(k, r.clone());
        r
    }

    async fn queue(
        &self,
        live: &mut LiveReads,
        s: ArrService,
    ) -> Result<Vec<ArrQueueItem>, PortError> {
        if let Some(r) = live.queues.get(&s) {
            return r.clone();
        }
        let r = match self.ports.arr.iter().find(|a| a.service() == s) {
            Some(a) => a.queue().await,
            None => Err(PortError::new(
                removarr_domain::error::ErrorClass::Config,
                format!("{} is not configured", s.as_str()),
            )),
        };
        live.queues.insert(s, r.clone());
        r
    }

    async fn fingerprint(
        &self,
        live: &mut LiveReads,
        media: &ArrMediaRef,
    ) -> Result<String, PortError> {
        let key = (media.service, media.media_id);
        if let Some(r) = live.fingerprints.get(&key) {
            return r.clone();
        }
        let whole = ArrMediaRef {
            service: media.service,
            media_id: media.media_id,
            episode_ids: vec![],
        };
        let r = match self.ports.arr.iter().find(|a| a.service() == media.service) {
            Some(a) => a.quality_snapshot(&whole).await.map(|s| s.fingerprint),
            None => Err(PortError::new(
                removarr_domain::error::ErrorClass::Config,
                format!("{} is not configured", media.service.as_str()),
            )),
        };
        live.fingerprints.insert(key, r.clone());
        r
    }

    async fn session(&self, live: &mut LiveReads) -> Result<Option<AuditSession>, PortError> {
        if live.session.is_none() {
            live.session = Some(self.ports.scans.latest_session().await?);
        }
        Ok(live.session.clone().unwrap_or(None))
    }

    async fn inventory(
        &self,
        live: &mut LiveReads,
        root: &RootId,
    ) -> Result<Option<RootInventory>, PortError> {
        if !live.inventories.contains_key(root) {
            let r = match self.ports.scans.load_inventory(root).await {
                Ok(i) => Some(i),
                Err(e) if e.class == removarr_domain::error::ErrorClass::NotFound => None,
                Err(e) => return Err(e),
            };
            live.inventories.insert(root.clone(), r);
        }
        Ok(live.inventories.get(root).cloned().unwrap_or(None))
    }

    /// Every blocker of one action against live reads. An empty list means every precondition
    /// still holds. Read failures are blockers too (fail closed), never silently skipped.
    pub async fn revalidate_action(
        &self,
        live: &mut LiveReads,
        a: &PlannedAction,
    ) -> Result<Vec<Blocker>, PortError> {
        let mut out = Vec::new();
        let paths = action_paths(a);
        for p in &a.preconditions {
            self.check(live, a, &paths, p, &mut out).await?;
        }
        self.check_wanted_files(live, a, &paths, &mut out).await?;
        self.check_live_holders(live, a, &paths, &mut out).await?;
        Ok(out)
    }

    /// CP2T3-F02: the live set of torrents holding any path of the action (by path through
    /// the full ownership index, and by inode through the stored inventories) must equal the
    /// planned `torrent_hashes` (the action's own hash excepted). A holder that appeared after
    /// planning, or a mapping fault on a torrent near the paths, refuses the action.
    async fn check_live_holders(
        &self,
        live: &mut LiveReads,
        a: &PlannedAction,
        paths: &[RootedPath],
        out: &mut Vec<Blocker>,
    ) -> Result<(), PortError> {
        if paths.is_empty() {
            return Ok(());
        }
        let own = match &a.target {
            ActionTarget::Torrent { hash, .. } => Some(hash.clone()),
            _ => None,
        };
        let (list, major) = match self.torrents(live).await {
            Ok(x) => x,
            Err(e) => {
                out.push(unavailable(ServiceKind::Qbittorrent, &e));
                return Ok(());
            }
        };
        let related = |p: &RootedPath, q: &RootedPath| {
            p.root == q.root && (p.rel.starts_with(&q.rel) || q.rel.starts_with(&p.rel))
        };
        let mut near = Vec::new();
        for t in &list {
            let mut bases = vec![&t.save_path, &t.content_path];
            bases.extend(t.download_path.iter());
            let mut faulty = false;
            let mut is_near = false;
            for b in bases {
                match b {
                    MappedPath::Mapped { path, .. } => {
                        let dir = RootedPath::new(
                            path.root.clone(),
                            path.rel.parent().unwrap_or_else(RelPath::root),
                        );
                        if paths.iter().any(|p| related(p, path) || related(p, &dir)) {
                            is_near = true;
                        }
                    }
                    MappedPath::NoRule { .. } | MappedPath::Invalid { .. } => faulty = true,
                    MappedPath::OutsideRoots { .. } | MappedPath::DeclaredExternal { .. } => {}
                }
            }
            if faulty {
                out.push(Blocker::new(
                    BlockerCode::MappingError,
                    format!("torrent {} has a path mapping fault", t.hash.as_str()),
                ));
                return Ok(());
            }
            if is_near {
                near.push(t.clone());
            }
        }
        let mut files = BTreeMap::new();
        for t in &near {
            match self.files(live, &t.hash).await {
                Ok(f) => {
                    if f.iter().flat_map(|f| f.candidates.iter()).any(|c| {
                        matches!(c, MappedPath::NoRule { .. } | MappedPath::Invalid { .. })
                    }) {
                        out.push(Blocker::new(
                            BlockerCode::MappingError,
                            format!("torrent {} has a file mapping fault", t.hash.as_str()),
                        ));
                        return Ok(());
                    }
                    files.insert(t.hash.clone(), f);
                }
                Err(e) => {
                    out.push(unavailable(ServiceKind::Qbittorrent, &e));
                    return Ok(());
                }
            }
        }
        let snapshot = TorrentSnapshot {
            taken_at: self.ports.clock.now(),
            version: String::new(),
            major,
            torrents: near,
            files,
        };
        let mut idx = TorrentIndex::build(&snapshot);
        let mut inventories = Vec::new();
        for root in paths.iter().map(|p| &p.root).collect::<BTreeSet<_>>() {
            if let Some(inv) = self.inventory(live, root).await? {
                inventories.push(inv);
            }
        }
        idx.bind_inventories(&inventories);
        let mut holders: BTreeSet<TorrentHash> = BTreeSet::new();
        for p in paths {
            holders.extend(idx.hashes_under(p));
            if let Ok(Some(sig)) = self.stat(p).await
                && sig.kind == EntryKind::File
            {
                holders.extend(idx.hashes_for_inode(&sig.phys()));
            }
        }
        let extra: Vec<_> = holders
            .iter()
            .filter(|h| !a.torrent_hashes.contains(h) && Some(*h) != own.as_ref())
            .collect();
        if !extra.is_empty() {
            out.push(Blocker::new(
                BlockerCode::TorrentReferencesTarget,
                format!(
                    "torrents not in the plan hold a path or inode of this action: {}",
                    extra
                        .iter()
                        .map(|h| h.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        Ok(())
    }

    async fn check(
        &self,
        live: &mut LiveReads,
        a: &PlannedAction,
        paths: &[RootedPath],
        p: &Precondition,
        out: &mut Vec<Blocker>,
    ) -> Result<(), PortError> {
        match p {
            Precondition::EntryUnchanged {
                path,
                sig,
                compare_nlink,
            } => match self.stat(path).await {
                Ok(None) => out.push(changed(path, "entry missing")),
                Ok(Some(cur)) if cur.kind == EntryKind::Symlink => out.push(Blocker::new(
                    BlockerCode::Symlink,
                    format!("{path} is now a symlink; symlinks are never followed"),
                )),
                Ok(Some(cur)) if !sig.matches(&cur, *compare_nlink) => {
                    out.push(changed(path, "entry"));
                }
                Ok(Some(_)) => {}
                Err(e) => out.push(Blocker::new(
                    BlockerCode::ScanErrorUnder,
                    format!("{path}: {}", e.detail),
                )),
            },
            Precondition::DirUnchanged { snapshot } => {
                let fs = self.ports.fs.clone();
                let dir = snapshot.dir.clone();
                match self.blocking(move || fs.snapshot_dir(&dir)).await {
                    Ok(cur) if &cur == snapshot => {}
                    Ok(_) => out.push(changed(&snapshot.dir, "folder")),
                    Err(e) => out.push(Blocker::new(
                        BlockerCode::ScanErrorUnder,
                        format!("{}: {}", snapshot.dir, e.detail),
                    )),
                }
            }
            Precondition::NoScanErrorUnder { path } => {
                let session = self.session(live).await?;
                let ok_session = session.as_ref().is_some_and(|s| s.status.allows_planning());
                if !ok_session {
                    out.push(Blocker::new(
                        BlockerCode::ScanIncomplete,
                        "no complete scan session",
                    ));
                    return Ok(());
                }
                match self.inventory(live, &path.root).await? {
                    Some(inv) if inv.finished && !inv.has_issue_under(&path.rel) => {}
                    Some(_) => out.push(Blocker::new(
                        BlockerCode::ScanErrorUnder,
                        format!("scan error or protected zone at or around {path}"),
                    )),
                    None => out.push(Blocker::new(
                        BlockerCode::ScanIncomplete,
                        format!("root {} has no stored inventory", path.root),
                    )),
                }
            }
            Precondition::SameDevice { a: x, b: y } => {
                match (self.stat(x).await, self.stat(y).await) {
                    (Ok(Some(sx)), Ok(Some(sy))) if sx.dev == sy.dev => {}
                    (Ok(Some(_)), Ok(Some(_))) => out.push(Blocker::new(
                        BlockerCode::CrossDevice,
                        "source and target are on different devices",
                    )),
                    _ => out.push(changed(x, "entry")),
                }
            }
            Precondition::TorrentStates { states } => match self.torrents(live).await {
                Err(e) => out.push(unavailable(ServiceKind::Qbittorrent, &e)),
                Ok((list, major)) => {
                    for (hash, (state, progress)) in states {
                        match list.iter().find(|t| &t.hash == hash) {
                            None => out.push(Blocker::new(
                                BlockerCode::TorrentProtectedState,
                                format!("torrent {} is no longer listed", hash.as_str()),
                            )),
                            Some(t) if torrent_changed(t, major, state, *progress) => {
                                out.push(Blocker::new(
                                    BlockerCode::TorrentProtectedState,
                                    format!(
                                        "torrent {} changed: {} {} -> {} {}",
                                        hash.as_str(),
                                        state.as_str(),
                                        progress,
                                        t.state.as_str(),
                                        t.progress
                                    ),
                                ));
                            }
                            Some(_) => {}
                        }
                    }
                }
            },
            Precondition::NoTorrentReferences { path } => {
                let own = match &a.target {
                    ActionTarget::Torrent { hash, .. } => Some(hash.clone()),
                    _ => None,
                };
                match self.torrents(live).await {
                    Err(e) => out.push(unavailable(ServiceKind::Qbittorrent, &e)),
                    Ok((list, major)) => {
                        let mut files = BTreeMap::new();
                        for t in &list {
                            match self.files(live, &t.hash).await {
                                Ok(f) => {
                                    files.insert(t.hash.clone(), f);
                                }
                                Err(e) => {
                                    out.push(unavailable(ServiceKind::Qbittorrent, &e));
                                    return Ok(());
                                }
                            }
                        }
                        let snapshot = TorrentSnapshot {
                            taken_at: self.ports.clock.now(),
                            version: String::new(),
                            major,
                            torrents: list,
                            files,
                        };
                        let idx = TorrentIndex::build(&snapshot);
                        if idx
                            .hashes_under(path)
                            .iter()
                            .any(|h| Some(h) != own.as_ref())
                        {
                            out.push(Blocker::new(
                                BlockerCode::TorrentReferencesTarget,
                                format!("a torrent references {path}"),
                            ));
                        }
                    }
                }
            }
            Precondition::ServiceAvailable(k) => {
                if let Err(e) = self.service_ok(live, *k).await {
                    out.push(if e.class == removarr_domain::error::ErrorClass::Config {
                        Blocker::new(BlockerCode::ServiceNotConfigured(*k), e.detail)
                    } else {
                        unavailable(*k, &e)
                    });
                }
            }
            Precondition::NoArrImport { media, hashes } => {
                match self.queue(live, media.service).await {
                    Err(e) => out.push(unavailable(service_of(media.service), &e)),
                    Ok(queue) => {
                        let busy = queue.iter().any(|item| {
                            import_in_progress(item)
                                && (item.media.as_ref().is_some_and(|m| {
                                    m.service == media.service && m.media_id == media.media_id
                                }) || item
                                    .download_id
                                    .as_ref()
                                    .is_some_and(|h| hashes.contains(h)))
                        });
                        if busy {
                            out.push(Blocker::new(
                                BlockerCode::ImportInProgress,
                                format!(
                                    "{} is importing media {}",
                                    media.service.as_str(),
                                    media.media_id
                                ),
                            ));
                        }
                    }
                }
            }
            Precondition::ArrFingerprint { media, fingerprint } => {
                match self.fingerprint(live, media).await {
                    Ok(fp) if &fp == fingerprint => {}
                    Ok(_) => out.push(Blocker::new(
                        BlockerCode::ArrAmbiguous,
                        format!(
                            "{} settings or files changed for media {}",
                            media.service.as_str(),
                            media.media_id
                        ),
                    )),
                    Err(e) => out.push(unavailable(service_of(media.service), &e)),
                }
            }
            // Fail closed until the runner compares the pin (stage B follow-up, CP2T3-F03).
            Precondition::TorrentWantedFiles { hash, .. } => out.push(Blocker::new(
                BlockerCode::ChangedSincePlanning,
                format!(
                    "the pinned wanted files of {} cannot be checked by this runner yet",
                    hash.as_str()
                ),
            )),
            Precondition::OldCoverageRecoverable { old_files } => {
                for (path, phys, _) in old_files {
                    match self.stat(path).await {
                        Ok(Some(s)) if s.phys() == *phys => {}
                        _ => out.push(changed(path, "old media file")),
                    }
                }
            }
            Precondition::ConfigFingerprint(fp) => {
                if fp != &self.settings.config_fingerprint {
                    out.push(Blocker::new(
                        BlockerCode::ScanIncomplete,
                        "configuration changed since the plan was built; re-plan",
                    ));
                }
            }
            // Covered by its paired `EntryUnchanged` and `ArrFingerprint`.
            Precondition::ArrReplacementPresent { .. } => {}
        }
        let _ = paths;
        Ok(())
    }

    /// Live wanted-file set of every affected hash equals the stored snapshot's, and the hash
    /// still holds one of the action's paths.
    async fn check_wanted_files(
        &self,
        live: &mut LiveReads,
        a: &PlannedAction,
        paths: &[RootedPath],
        out: &mut Vec<Blocker>,
    ) -> Result<(), PortError> {
        if a.torrent_hashes.is_empty() {
            return Ok(());
        }
        let (list, _) = match self.torrents(live).await {
            Ok(x) => x,
            Err(e) => {
                out.push(unavailable(ServiceKind::Qbittorrent, &e));
                return Ok(());
            }
        };
        let stored = self.stored_snapshot(live).await?;
        for hash in &a.torrent_hashes {
            let Some(t) = list.iter().find(|t| &t.hash == hash) else {
                out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("torrent {} is no longer listed", hash.as_str()),
                ));
                continue;
            };
            let files = match self.files(live, hash).await {
                Ok(f) => f,
                Err(e) => {
                    out.push(unavailable(ServiceKind::Qbittorrent, &e));
                    continue;
                }
            };
            let stored_files = stored.as_ref().and_then(|s| s.files.get(hash));
            match stored_files {
                None => out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("no stored file list for torrent {}", hash.as_str()),
                )),
                Some(sf) if wanted_set(sf) != wanted_set(&files) => out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!("wanted file set of torrent {} changed", hash.as_str()),
                )),
                Some(_) => {}
            }
            if !holds(t, &files, paths) {
                out.push(Blocker::new(
                    BlockerCode::TorrentProtectedState,
                    format!(
                        "torrent {} no longer holds a path of this action",
                        hash.as_str()
                    ),
                ));
            }
        }
        Ok(())
    }
}
