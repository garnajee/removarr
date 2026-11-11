#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Action-runner test support: temporary roots with the real confined filesystem adapter and
//! the real `LiveFsMutator`, the real SQLite repositories, an in-memory qBittorrent fake with
//! a scripted recheck, and crash hooks (a fake that hangs at a boundary so the test can abort
//! the execution task, which models a process crash). Nothing touches real storage or services.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use removarr_config::{LiveMode, RootPaths, Roots};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{Completeness, EvidenceLevel};
use removarr_domain::fs::{CompareOutcome, DirSnapshot, EntrySignature, PhysicalId, SampleSpec};
use removarr_domain::gain::{GainEstimate, PlanGains};
use removarr_domain::ids::{PlanId, RawUuid, SessionId};
use removarr_domain::journal::{IntentRecord, JournalStep, ResultRecord};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionPlan, ActionTarget, ApprovalRequest, HashedPlan, PlanMode, PlanOrigin,
    PlanStatus, PlannedAction, Precondition, VerificationStep,
};
use removarr_domain::ports::{
    ContentReader, FsInventory, FsMutator, IdGen, JournalStore, PlanStore, QbitControl, QbitRead,
    ScanControl, ScanStore, SnapshotStore,
};
use removarr_domain::session::{
    AuditSession, RootStatus, ServiceKind, ServiceState, ServiceStatus, SessionMetrics,
    SessionStatus,
};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot, TorrentState,
};
use removarr_fs::{ConfinedFs, FsLimits, LiveFsMutator};
use removarr_matching::ReadLimiter;
use removarr_matching::proof::ContentVerifier;
use removarr_storage::{Db, Repos};
use removarr_testkit::clock::FakeClock;
use removarr_testkit::ids::SeqIds;
use removarr_workflows::actions::{ActionPorts, ActionSettings, ExecutingPlans, Mutators};
use removarr_workflows::plans::ExecutionTicket;
use tokio::sync::Notify;

pub const NOW: Timestamp = Timestamp(1_700_000_000_000);
pub const CFG: &str = "cfg-fingerprint-1";
pub const FILE_BYTES: usize = 200_000;

pub fn is_root_user() -> bool {
    use std::os::unix::fs::MetadataExt;
    let f = tempfile::NamedTempFile::new().unwrap();
    fs::metadata(f.path()).unwrap().uid() == 0
}

pub fn hash(c: char) -> TorrentHash {
    TorrentHash::new(std::iter::repeat_n(c, 40).collect::<String>()).unwrap()
}

pub fn rp(root: &RootId, rel: &str) -> RootedPath {
    RootedPath::new(root.clone(), RelPath::new(rel).unwrap())
}

// ------------------------------------------------------------------ crash hooks

/// Where a fake hangs forever (the test then aborts the execution task: a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// In `record_intent`, before the row is written.
    BeforeIntent(JournalStep),
    /// In `record_intent`, after the row is durable (before the mutation).
    AfterIntent(JournalStep),
    /// In `record_result`, before the row is written (after the mutation).
    BeforeResult(JournalStep),
    /// In the torrent listing polled after a recheck command.
    DuringRecheck,
}

#[derive(Debug, Default)]
pub struct Hang {
    pub at: Mutex<Option<CrashPoint>>,
    /// Occurrences of the point to let through before hanging.
    pub skip: Mutex<usize>,
    pub hit: Notify,
    pub reached: Mutex<Option<CrashPoint>>,
}

impl Hang {
    pub fn arm(&self, p: CrashPoint) {
        self.arm_nth(p, 0);
    }

    /// Hangs at the `skip + 1`-th occurrence of `p`.
    pub fn arm_nth(&self, p: CrashPoint, skip: usize) {
        *self.at.lock().unwrap() = Some(p);
        *self.skip.lock().unwrap() = skip;
    }

    pub fn disarm(&self) {
        *self.at.lock().unwrap() = None;
    }

    /// True when this occurrence must hang (consumes one skip otherwise).
    fn should_hang(&self, p: CrashPoint) -> bool {
        if *self.at.lock().unwrap() != Some(p) {
            return false;
        }
        let mut skip = self.skip.lock().unwrap();
        if *skip > 0 {
            *skip -= 1;
            return false;
        }
        true
    }

    async fn maybe(&self, p: CrashPoint) {
        if self.should_hang(p) {
            *self.reached.lock().unwrap() = Some(p);
            self.hit.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

/// Journal wrapper with crash hooks around the real repository.
pub struct HangJournal {
    inner: Arc<dyn JournalStore>,
    pub hang: Arc<Hang>,
    /// Test seam (CP2T3-F01): a `Done` result of this step is stored as `Failed`, so the
    /// journal says the mutation failed while the disk says it happened.
    pub rewrite_done_as_failed: Arc<Mutex<Option<JournalStep>>>,
}

#[async_trait]
impl JournalStore for HangJournal {
    async fn record_intent(&self, i: &IntentRecord) -> Result<(), PortError> {
        self.hang.maybe(CrashPoint::BeforeIntent(i.step)).await;
        self.inner.record_intent(i).await?;
        self.hang.maybe(CrashPoint::AfterIntent(i.step)).await;
        Ok(())
    }
    async fn record_result(&self, r: &ResultRecord) -> Result<(), PortError> {
        let step = self.step_of(r.intent_id).await;
        if let Some(step) = step {
            self.hang.maybe(CrashPoint::BeforeResult(step)).await;
        }
        let rewrite = *self.rewrite_done_as_failed.lock().unwrap();
        if rewrite.is_some()
            && step == rewrite
            && matches!(
                r.outcome,
                removarr_domain::journal::StepOutcome::Done { .. }
            )
        {
            let mut failed = r.clone();
            failed.outcome = removarr_domain::journal::StepOutcome::Failed {
                class: ErrorClass::Io(removarr_domain::fs::IoKind::Other),
                message: "recorded as failed by the test seam".into(),
            };
            return self.inner.record_result(&failed).await;
        }
        self.inner.record_result(r).await
    }
    async fn record_outcome(
        &self,
        a: removarr_domain::ids::ActionRef,
        o: &removarr_domain::journal::ActionOutcome,
    ) -> Result<(), PortError> {
        self.inner.record_outcome(a, o).await
    }
    async fn unfinished(
        &self,
    ) -> Result<Vec<removarr_domain::journal::UnfinishedIntent>, PortError> {
        self.inner.unfinished().await
    }
    async fn for_plan(
        &self,
        id: PlanId,
    ) -> Result<Vec<(IntentRecord, Option<ResultRecord>)>, PortError> {
        self.inner.for_plan(id).await
    }
}

impl HangJournal {
    async fn step_of(&self, id: removarr_domain::ids::IntentId) -> Option<JournalStep> {
        let armed = *self.hang.at.lock().unwrap();
        let wanted = matches!(armed, Some(CrashPoint::BeforeResult(_)))
            || self.rewrite_done_as_failed.lock().unwrap().is_some();
        if !wanted {
            return None;
        }
        // Only a hang on results or the rewrite seam needs the step: look it up through the
        // unfinished list.
        self.inner
            .unfinished()
            .await
            .ok()?
            .into_iter()
            .find(|u| u.intent.id == id)
            .map(|u| u.intent.step)
    }
}

// ------------------------------------------------------------------ qBittorrent fake

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecheckResult {
    #[default]
    Verified,
    Failed,
    /// Stays in `checkingUP` forever (timeout).
    Stuck,
}

#[derive(Debug, Clone)]
struct Entry {
    info: TorrentInfo,
    files: Vec<TorrentFile>,
    /// Remaining polls in `checkingUP` after a recheck.
    checking: Option<u32>,
}

#[derive(Default)]
pub struct FakeQbit {
    torrents: Mutex<BTreeMap<TorrentHash, Entry>>,
    pub calls: Mutex<Vec<(String, BTreeSet<TorrentHash>)>>,
    pub recheck_result: Mutex<RecheckResult>,
    pub checking_polls: Mutex<u32>,
    pub listings: AtomicUsize,
    pub fail_reads: Mutex<bool>,
    pub fail_mutations: Mutex<Option<String>>,
    rechecked: Mutex<bool>,
    pub hang: Arc<Hang>,
}

impl std::fmt::Debug for FakeQbit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeQbit")
    }
}

impl FakeQbit {
    pub fn new(hang: Arc<Hang>) -> Arc<Self> {
        Arc::new(Self {
            checking_polls: Mutex::new(2),
            hang,
            ..Self::default()
        })
    }

    /// A complete, seeding torrent holding `files` (name, root-relative path) under `root`.
    pub fn add(&self, h: char, root: &RootId, content: &str, files: &[(&str, &str)]) {
        let mapped = |rel: &str| MappedPath::Mapped {
            raw: format!("/data/{rel}"),
            path: rp(root, rel),
        };
        let info = TorrentInfo {
            hash: hash(h),
            name: format!("torrent-{h}"),
            state: TorrentState::Uploading,
            progress: 1.0,
            added_on: Timestamp(NOW.0 - 86_400_000 * 10),
            completion_on: Some(Timestamp(NOW.0 - 86_400_000 * 9)),
            category: String::new(),
            size: FILE_BYTES as u64,
            save_path: MappedPath::Mapped {
                raw: "/data".into(),
                path: RootedPath::new(root.clone(), RelPath::root()),
            },
            download_path: None,
            content_path: mapped(content),
        };
        let files = files
            .iter()
            .enumerate()
            .map(|(i, (name, rel))| TorrentFile {
                index: i as u32,
                name: (*name).to_string(),
                size: FILE_BYTES as u64,
                priority: 1,
                progress: 1.0,
                candidates: vec![mapped(rel)],
            })
            .collect();
        self.torrents.lock().unwrap().insert(
            hash(h),
            Entry {
                info,
                files,
                checking: None,
            },
        );
    }

    /// A torrent whose paths have no mapping rule (fail-closed case).
    pub fn add_unmapped(&self, h: char) {
        self.add(
            h,
            &RootId::completed(),
            "Other",
            &[("Other/x.mkv", "Other/x.mkv")],
        );
        let mut t = self.torrents.lock().unwrap();
        let e = t.get_mut(&hash(h)).unwrap();
        e.info.save_path = MappedPath::NoRule {
            raw: "/srv/elsewhere".into(),
        };
        e.info.content_path = MappedPath::NoRule {
            raw: "/srv/elsewhere/Other".into(),
        };
        for f in &mut e.files {
            f.candidates = vec![MappedPath::NoRule {
                raw: "/srv/elsewhere/Other/x.mkv".into(),
            }];
        }
    }

    pub fn set_state(&self, h: char, state: &str) {
        let mut t = self.torrents.lock().unwrap();
        t.get_mut(&hash(h)).unwrap().info.state = TorrentState::parse(state);
    }

    pub fn state(&self, h: char) -> TorrentState {
        self.torrents.lock().unwrap()[&hash(h)].info.state.clone()
    }

    pub fn set_priority(&self, h: char, index: u32, priority: i32) {
        let mut t = self.torrents.lock().unwrap();
        for f in &mut t.get_mut(&hash(h)).unwrap().files {
            if f.index == index {
                f.priority = priority;
            }
        }
    }

    pub fn snapshot(&self) -> TorrentSnapshot {
        let t = self.torrents.lock().unwrap();
        TorrentSnapshot {
            taken_at: NOW,
            version: "v5.0.4".into(),
            major: 5,
            torrents: t.values().map(|e| e.info.clone()).collect(),
            files: t
                .iter()
                .map(|(h, e)| (h.clone(), e.files.clone()))
                .collect(),
        }
    }

    pub fn calls_of(&self, kind: &str) -> Vec<BTreeSet<TorrentHash>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, h)| h.clone())
            .collect()
    }

    pub fn mutation_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn mutate(&self, kind: &str, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        if let Some(m) = self.fail_mutations.lock().unwrap().clone() {
            return Err(PortError::new(ErrorClass::Unavailable, m));
        }
        self.calls
            .lock()
            .unwrap()
            .push((kind.to_string(), hashes.clone()));
        let mut t = self.torrents.lock().unwrap();
        for h in hashes {
            let e = t.get_mut(h).ok_or_else(|| {
                PortError::new(ErrorClass::NotFound, "torrent not in the current listing")
            })?;
            match kind {
                "stop" => e.info.state = TorrentState::StoppedUp,
                "start" => e.info.state = TorrentState::Uploading,
                "recheck" => {
                    e.checking = Some(*self.checking_polls.lock().unwrap());
                    *self.rechecked.lock().unwrap() = true;
                }
                "remove" => panic!("stage A never removes a torrent task"),
                _ => unreachable!(),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl QbitRead for FakeQbit {
    async fn version(&self) -> Result<String, PortError> {
        if *self.fail_reads.lock().unwrap() {
            return Err(PortError::new(ErrorClass::Unavailable, "qbittorrent down"));
        }
        Ok("v5.0.4".into())
    }
    async fn torrents(&self) -> Result<Vec<TorrentInfo>, PortError> {
        if *self.fail_reads.lock().unwrap() {
            return Err(PortError::new(ErrorClass::Unavailable, "qbittorrent down"));
        }
        if *self.rechecked.lock().unwrap() {
            self.hang.maybe(CrashPoint::DuringRecheck).await;
        }
        self.listings.fetch_add(1, Ordering::SeqCst);
        let result = *self.recheck_result.lock().unwrap();
        let mut t = self.torrents.lock().unwrap();
        let mut out = Vec::new();
        for e in t.values_mut() {
            let mut info = e.info.clone();
            match (e.checking, result) {
                (Some(_), RecheckResult::Stuck) => {
                    e.info.state = TorrentState::CheckingUp;
                    info = e.info.clone();
                }
                (Some(0), RecheckResult::Verified) => {
                    e.checking = None;
                    e.info.state = TorrentState::StoppedUp;
                    info = e.info.clone();
                }
                (Some(0), RecheckResult::Failed) => {
                    e.checking = None;
                    e.info.state = TorrentState::MissingFiles;
                    info = e.info.clone();
                }
                (Some(n), _) => {
                    e.checking = Some(n - 1);
                    info.state = TorrentState::CheckingUp;
                }
                (None, _) => {}
            }
            out.push(info);
        }
        Ok(out)
    }
    async fn files(&self, h: &TorrentHash) -> Result<Vec<TorrentFile>, PortError> {
        if *self.fail_reads.lock().unwrap() {
            return Err(PortError::new(ErrorClass::Unavailable, "qbittorrent down"));
        }
        self.torrents
            .lock()
            .unwrap()
            .get(h)
            .map(|e| e.files.clone())
            .ok_or_else(|| PortError::new(ErrorClass::NotFound, "no such torrent"))
    }
}

#[async_trait]
impl QbitControl for FakeQbit {
    async fn stop(&self, h: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate("stop", h)
    }
    async fn start(&self, h: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate("start", h)
    }
    async fn recheck(&self, h: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate("recheck", h)
    }
    async fn remove_tasks_keep_data(&self, h: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate("remove", h)
    }
}

// ------------------------------------------------------------------ ARR / Jellyfin fakes

/// Counting ARR commands (CP2T3-F05): every call is a mutation the guards must not see.
#[derive(Debug, Default)]
pub struct CountingArr(pub AtomicUsize);

#[async_trait]
impl removarr_domain::ports::ArrCommands for CountingArr {
    async fn rescan(&self, _: &removarr_domain::arr::ArrMediaRef) -> Result<i64, PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
    async fn rescan_all(&self) -> Result<i64, PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
    async fn manual_import(
        &self,
        _: &removarr_domain::arr::ManualImportRequest,
    ) -> Result<i64, PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
    async fn grab(&self, _: &removarr_domain::arr::ReleaseRef) -> Result<(), PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn remove_queue_item_keep_media(&self, _: i64) -> Result<(), PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_monitored(
        &self,
        _: &removarr_domain::arr::ArrMediaRef,
        _: bool,
    ) -> Result<(), PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct CountingJellyfin(pub AtomicUsize);

#[async_trait]
impl removarr_domain::ports::JellyfinCommands for CountingJellyfin {
    async fn refresh_library(&self) -> Result<(), PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn refresh_item(&self, _: &str) -> Result<(), PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A reachable Sonarr with an empty queue (read side only).
#[derive(Debug)]
pub struct ArrReadFake;

#[async_trait]
impl removarr_domain::ports::ArrRead for ArrReadFake {
    fn service(&self) -> removarr_domain::arr::ArrService {
        removarr_domain::arr::ArrService::Sonarr
    }
    async fn status(&self) -> Result<String, PortError> {
        Ok("4.0".into())
    }
    async fn library(&self) -> Result<Vec<removarr_domain::arr::ArrLibraryEntry>, PortError> {
        Ok(vec![])
    }
    async fn queue(&self) -> Result<Vec<removarr_domain::arr::ArrQueueItem>, PortError> {
        Ok(vec![])
    }
    async fn history_for_download(
        &self,
        _: &TorrentHash,
    ) -> Result<Vec<removarr_domain::arr::ArrHistoryRecord>, PortError> {
        Ok(vec![])
    }
    async fn quality_snapshot(
        &self,
        _: &removarr_domain::arr::ArrMediaRef,
    ) -> Result<removarr_domain::arr::ArrQualitySnapshot, PortError> {
        Err(PortError::new(ErrorClass::NotFound, "no media in the fake"))
    }
    async fn manual_import_preview(
        &self,
        _: &RootedPath,
        _: Option<&removarr_domain::arr::ArrMediaRef>,
    ) -> Result<Vec<removarr_domain::arr::ArrCandidateDecision>, PortError> {
        Ok(vec![])
    }
    async fn episodes(
        &self,
        _: i64,
        _: Option<i32>,
    ) -> Result<Vec<removarr_domain::arr::ArrEpisode>, PortError> {
        Ok(vec![])
    }
    async fn search_releases(
        &self,
        _: &removarr_domain::arr::ArrMediaRef,
    ) -> Result<Vec<removarr_domain::arr::ReleaseSearchResult>, PortError> {
        Ok(vec![])
    }
    async fn command_status(
        &self,
        _: i64,
    ) -> Result<removarr_domain::arr::CommandState, PortError> {
        Ok(removarr_domain::arr::CommandState::Completed)
    }
    async fn media_management(
        &self,
    ) -> Result<removarr_domain::arr::ArrMediaManagement, PortError> {
        Err(PortError::new(ErrorClass::NotFound, "not in the fake"))
    }
}

#[derive(Debug)]
pub struct JellyfinReadFake;

#[async_trait]
impl removarr_domain::ports::JellyfinRead for JellyfinReadFake {
    async fn status(&self) -> Result<String, PortError> {
        Ok("10.9".into())
    }
    async fn libraries(
        &self,
    ) -> Result<Vec<removarr_domain::jellyfin::JellyfinLibrary>, PortError> {
        Ok(vec![])
    }
    async fn items_by_paths(
        &self,
        _: &[RootedPath],
    ) -> Result<BTreeMap<RootedPath, Option<removarr_domain::jellyfin::JellyfinItemRef>>, PortError>
    {
        Ok(BTreeMap::new())
    }
    async fn library_refresh_running(&self) -> Result<bool, PortError> {
        Ok(false)
    }
}

/// `ScanStore` wrapper whose `latest_session` fails after `allow` calls (CP2T3-F11).
pub struct FailingScans {
    inner: removarr_storage::ScanRepo,
    pub allow: AtomicUsize,
    pub calls: AtomicUsize,
}

#[async_trait]
impl ScanStore for FailingScans {
    async fn begin_session(&self, s: &AuditSession) -> Result<(), PortError> {
        self.inner.begin_session(s).await
    }
    async fn save_inventory(
        &self,
        session: SessionId,
        inv: &removarr_domain::fs::RootInventory,
    ) -> Result<removarr_domain::fs::DiffStats, PortError> {
        self.inner.save_inventory(session, inv).await
    }
    async fn finish_session(&self, s: &AuditSession) -> Result<(), PortError> {
        self.inner.finish_session(s).await
    }
    async fn latest_session(&self) -> Result<Option<AuditSession>, PortError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.allow.load(Ordering::SeqCst) {
            return Err(PortError::new(ErrorClass::Storage, "database read failed"));
        }
        self.inner.latest_session().await
    }
    async fn load_inventory(
        &self,
        root: &RootId,
    ) -> Result<removarr_domain::fs::RootInventory, PortError> {
        self.inner.load_inventory(root).await
    }
}

// ------------------------------------------------------------------ fs wrappers

/// Counts mutator calls; can run a hook after a replacement and fail it afterwards.
pub struct CountingMutator {
    inner: Arc<dyn FsMutator>,
    pub probes: AtomicUsize,
    pub replaces: AtomicUsize,
    pub others: AtomicUsize,
    pub after_replace: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Perform the replacement, then report an error (the link is in place).
    pub fail_after_replace: Mutex<bool>,
}

impl CountingMutator {
    pub fn new(inner: Arc<dyn FsMutator>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            probes: AtomicUsize::new(0),
            replaces: AtomicUsize::new(0),
            others: AtomicUsize::new(0),
            after_replace: Mutex::new(None),
            fail_after_replace: Mutex::new(false),
        })
    }
    pub fn mutations(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
            + self.replaces.load(Ordering::SeqCst)
            + self.others.load(Ordering::SeqCst)
    }
}

impl FsMutator for CountingMutator {
    fn probe_hardlink(&self, s: &RootedPath, d: &RootedPath) -> Result<(), PortError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        self.inner.probe_hardlink(s, d)
    }
    fn replace_with_hardlink(
        &self,
        s: &RootedPath,
        t: &RootedPath,
        es: &EntrySignature,
        et: &EntrySignature,
    ) -> Result<PhysicalId, PortError> {
        self.replaces.fetch_add(1, Ordering::SeqCst);
        let r = self.inner.replace_with_hardlink(s, t, es, et);
        if let Some(f) = self.after_replace.lock().unwrap().as_ref() {
            f();
        }
        if *self.fail_after_replace.lock().unwrap() {
            return Err(PortError::new(
                ErrorClass::Io(removarr_domain::fs::IoKind::Other),
                "fsync failed after the rename",
            ));
        }
        r
    }
    fn remove_file(&self, t: &RootedPath, e: &EntrySignature) -> Result<(), PortError> {
        self.others.fetch_add(1, Ordering::SeqCst);
        self.inner.remove_file(t, e)
    }
    fn remove_tree(&self, d: &RootedPath, e: &DirSnapshot) -> Result<(), PortError> {
        self.others.fetch_add(1, Ordering::SeqCst);
        self.inner.remove_tree(d, e)
    }
    fn remove_empty_dir(&self, d: &RootedPath, e: &EntrySignature) -> Result<(), PortError> {
        self.others.fetch_add(1, Ordering::SeqCst);
        self.inner.remove_empty_dir(d, e)
    }
    fn set_mode(&self, t: &RootedPath, e: &EntrySignature, m: u32) -> Result<(), PortError> {
        self.others.fetch_add(1, Ordering::SeqCst);
        self.inner.set_mode(t, e, m)
    }
}

/// Counts full comparisons and can override the outcome of the next ones.
pub struct ScriptedContent {
    inner: Arc<ConfinedFs>,
    pub compares: AtomicUsize,
    pub override_next: Mutex<Vec<CompareOutcome>>,
}

impl ContentReader for ScriptedContent {
    fn samples_match(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        e: [&EntrySignature; 2],
        s: SampleSpec,
        c: &ScanControl,
    ) -> Result<bool, PortError> {
        self.inner.samples_match(a, b, e, s, c)
    }
    fn full_compare(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        e: [&EntrySignature; 2],
        chunk: u32,
        c: &ScanControl,
    ) -> Result<CompareOutcome, PortError> {
        self.compares.fetch_add(1, Ordering::SeqCst);
        let mut o = self.override_next.lock().unwrap();
        if !o.is_empty() {
            return Ok(o.remove(0));
        }
        drop(o);
        self.inner.full_compare(a, b, e, chunk, c)
    }
}

/// `ExecutingPlans` over the storage repository.
pub struct Executing(pub removarr_storage::PlanRepo);

#[async_trait]
impl ExecutingPlans for Executing {
    async fn executing(&self) -> Result<Vec<PlanId>, PortError> {
        self.0.ids_with_status(PlanStatus::Executing).await
    }
}

// ------------------------------------------------------------------ the lab

pub struct Lab {
    _tmp: tempfile::TempDir,
    pub base: PathBuf,
    pub roots: Roots,
    pub media: RootId,
    pub fs: Arc<ConfinedFs>,
    pub content: Arc<ScriptedContent>,
    pub repos: Repos,
    pub clock: Arc<FakeClock>,
    pub ids: Arc<SeqIds>,
    pub hang: Arc<Hang>,
    pub qbit: Arc<FakeQbit>,
    pub session: SessionId,
    pub arr_cmd: Arc<CountingArr>,
    pub jellyfin_cmd: Arc<CountingJellyfin>,
    pub scans: Arc<FailingScans>,
    pub rewrite_done_as_failed: Arc<Mutex<Option<JournalStep>>>,
}

impl Lab {
    /// Two roots with one identical pair (`Rel/a.mkv` in completed and media) seeded by torrent
    /// `a`, plus a second identical pair (`Rel/b.mkv`) seeded by torrent `b`.
    pub async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let completed = base.join("completed");
        let media = base.join("media");
        fs::create_dir_all(completed.join("Rel")).unwrap();
        fs::create_dir_all(media.join("Movies/Rel")).unwrap();
        for name in ["a", "b"] {
            let bytes: Vec<u8> = (0..FILE_BYTES)
                .map(|i| (i % 251) as u8 ^ name.as_bytes()[0])
                .collect();
            fs::write(completed.join(format!("Rel/{name}.mkv")), &bytes).unwrap();
            fs::write(media.join(format!("Movies/Rel/{name}.mkv")), &bytes).unwrap();
        }
        let roots = Roots::validate(&RootPaths {
            completed: Some(completed.to_str().unwrap().into()),
            incomplete: None,
            media: vec![media.to_str().unwrap().into()],
        })
        .expect("roots");
        let media_id = RootId::media(media.to_str().unwrap());
        let fs = Arc::new(ConfinedFs::new(&roots, FsLimits::default()));
        let content = Arc::new(ScriptedContent {
            inner: fs.clone(),
            compares: AtomicUsize::new(0),
            override_next: Mutex::new(Vec::new()),
        });
        let db = Db::open(&base.join("removarr.db")).await.unwrap();
        let clock = Arc::new(FakeClock::new(NOW));
        let repos = Repos::new(&db, clock.clone());
        let repos_scans = repos.scans.clone();
        let hang = Arc::new(Hang::default());
        let qbit = FakeQbit::new(hang.clone());
        qbit.add(
            'a',
            &RootId::completed(),
            "Rel",
            &[("Rel/a.mkv", "Rel/a.mkv")],
        );
        qbit.add(
            'b',
            &RootId::completed(),
            "Rel",
            &[("Rel/b.mkv", "Rel/b.mkv")],
        );
        let lab = Self {
            _tmp: tmp,
            base,
            roots,
            media: media_id,
            fs,
            content,
            repos,
            clock,
            ids: Arc::new(SeqIds::new()),
            hang,
            qbit,
            session: SessionId::new(RawUuid::from_u128(7)),
            arr_cmd: Arc::new(CountingArr::default()),
            jellyfin_cmd: Arc::new(CountingJellyfin::default()),
            scans: Arc::new(FailingScans {
                inner: repos_scans,
                allow: AtomicUsize::new(usize::MAX),
                calls: AtomicUsize::new(0),
            }),
            rewrite_done_as_failed: Arc::new(Mutex::new(None)),
        };
        lab.store_observation().await;
        lab
    }

    /// Stores a complete session with the real inventories and the fake's torrent snapshot.
    pub async fn store_observation(&self) {
        self.store_session(self.session).await;
    }

    /// Stores a newer complete session (the filesystem changed since the first one).
    pub async fn store_observation_again(&self) {
        self.store_session(SessionId::new(RawUuid::from_u128(8)))
            .await;
    }

    async fn store_session(&self, id: SessionId) {
        let ok = |k: ServiceKind| ServiceStatus {
            service: k,
            state: ServiceState::Ok {
                version: "1".into(),
            },
            observed_at: NOW,
        };
        let mut s = AuditSession {
            id,
            started_at: Timestamp(NOW.0 + i64::try_from(id.0.as_u128()).unwrap()),
            finished_at: None,
            status: SessionStatus::Running,
            roots: [RootId::completed(), self.media.clone()]
                .iter()
                .map(|r| RootStatus {
                    root: r.clone(),
                    scanned: true,
                    error_count: 0,
                    protected_count: 0,
                })
                .collect(),
            services: vec![ok(ServiceKind::Qbittorrent)],
            metrics: SessionMetrics::default(),
        };
        self.repos.scans.begin_session(&s).await.unwrap();
        for r in [RootId::completed(), self.media.clone()] {
            let inv = self.fs.scan_root(&r, &ScanControl::default()).unwrap();
            self.repos.scans.save_inventory(s.id, &inv).await.unwrap();
        }
        self.repos
            .snapshots
            .save_torrents(s.id, &self.qbit.snapshot())
            .await
            .unwrap();
        s.finish(SessionStatus::Complete, NOW).unwrap();
        self.repos.scans.finish_session(&s).await.unwrap();
    }

    pub fn completed_path(&self, rel: &str) -> PathBuf {
        self.base.join("completed").join(rel)
    }

    pub fn media_path(&self, rel: &str) -> PathBuf {
        self.base.join("media").join(rel)
    }

    pub fn sig(&self, p: &RootedPath) -> EntrySignature {
        self.fs.stat(p).unwrap().expect("exists")
    }

    pub fn phys(&self, path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(path).unwrap();
        (m.dev(), m.ino())
    }

    /// A consolidation plan: one action per `(source rel, target rel, hashes)`.
    pub fn plan(
        &self,
        items: &[(&str, &str, &[char])],
        mode: PlanMode,
        origin: PlanOrigin,
    ) -> HashedPlan {
        let listing = self.qbit.snapshot();
        let mut actions = Vec::new();
        let mut states = BTreeMap::new();
        let (mut logical, mut attributable) = (0, 0);
        for (i, (src, tgt, hashes)) in items.iter().enumerate() {
            let source = rp(&RootId::completed(), src);
            let target = rp(&self.media, tgt);
            let (ss, ts) = (self.sig(&source), self.sig(&target));
            let hs: BTreeSet<TorrentHash> = hashes.iter().map(|c| hash(*c)).collect();
            let mut st = BTreeMap::new();
            for h in &hs {
                let t = listing.torrents.iter().find(|t| &t.hash == h).unwrap();
                st.insert(h.clone(), (t.state.clone(), t.progress));
            }
            states.extend(st.clone());
            let mut pre = vec![
                Precondition::EntryUnchanged {
                    path: source.clone(),
                    sig: ss,
                    compare_nlink: false,
                },
                Precondition::EntryUnchanged {
                    path: target.clone(),
                    sig: ts,
                    compare_nlink: false,
                },
                Precondition::SameDevice {
                    a: source.clone(),
                    b: target.clone(),
                },
                Precondition::NoScanErrorUnder {
                    path: source.clone(),
                },
                Precondition::NoScanErrorUnder {
                    path: target.clone(),
                },
                Precondition::ServiceAvailable(ServiceKind::Qbittorrent),
                Precondition::ConfigFingerprint(CFG.into()),
            ];
            if !st.is_empty() {
                pre.push(Precondition::TorrentStates { states: st });
            }
            let mut verify = vec![
                VerificationStep::InodeEquals {
                    path: target.clone(),
                    phys: ss.phys(),
                },
                VerificationStep::InodeEquals {
                    path: source.clone(),
                    phys: ss.phys(),
                },
            ];
            if !hs.is_empty() {
                verify.push(VerificationStep::RecheckVerified { hashes: hs.clone() });
            }
            logical += ts.size;
            attributable += ts.size;
            actions.push(PlannedAction {
                index: i as u32,
                kind: ActionKind::ConsolidateHardlink,
                target: ActionTarget::Consolidate { source, target },
                preconditions: pre,
                verification: verify,
                expected_gain: GainEstimate {
                    logical: ts.size,
                    attributable: ts.size,
                    added: 0,
                },
                depends_on: vec![],
                torrent_hashes: hs,
                evidence_level: EvidenceLevel::Full,
                completeness: Completeness::Complete,
            });
        }
        let plan = ActionPlan {
            id: PlanId::new(self.ids.uuid()),
            created_at: NOW,
            expires_at: NOW.plus_hours(24),
            mode,
            origin,
            session: self.session,
            config_fingerprint: CFG.into(),
            actions,
            excluded: vec![],
            lock_roots: [RootId::completed(), self.media.clone()]
                .into_iter()
                .collect(),
            torrent_states: states,
            arr_snapshots: vec![],
            samples: vec![],
            gains: PlanGains::new(logical, attributable, 0),
        };
        HashedPlan::seal(plan).expect("valid plan")
    }

    /// A plan with one tier-3 action (refused in stage A).
    pub fn tier3_plan(&self) -> HashedPlan {
        let target = rp(&RootId::completed(), "Rel/a.mkv");
        let sig = self.sig(&target);
        let plan = ActionPlan {
            id: PlanId::new(self.ids.uuid()),
            created_at: NOW,
            expires_at: NOW.plus_hours(24),
            mode: PlanMode::Live,
            origin: PlanOrigin::User,
            session: self.session,
            config_fingerprint: CFG.into(),
            actions: vec![PlannedAction {
                index: 0,
                kind: ActionKind::DeleteCompletedFile,
                target: ActionTarget::Path {
                    path: target.clone(),
                },
                preconditions: vec![Precondition::EntryUnchanged {
                    path: target.clone(),
                    sig,
                    compare_nlink: true,
                }],
                verification: vec![VerificationStep::Absent { path: target }],
                expected_gain: GainEstimate::default(),
                depends_on: vec![],
                torrent_hashes: BTreeSet::new(),
                evidence_level: EvidenceLevel::None,
                completeness: Completeness::Complete,
            }],
            excluded: vec![],
            lock_roots: [RootId::completed()].into_iter().collect(),
            torrent_states: BTreeMap::new(),
            arr_snapshots: vec![],
            samples: vec![],
            gains: PlanGains::default(),
        };
        HashedPlan::seal(plan).unwrap()
    }

    /// Stores the plan, approves it (live plans), returns the ticket.
    pub async fn ticket(&self, plan: HashedPlan, simulate: bool) -> ExecutionTicket {
        let (p, h) = plan.clone().into_parts();
        self.repos.plans.insert(&p, &h).await.unwrap();
        let req = ApprovalRequest {
            plan_hash: h.clone(),
            confirmed_count: p.required_count(),
            confirmed_torrent_removals: 0,
            acknowledged_samples: vec![],
            acknowledged_unrecoverable: vec![],
        };
        let approval = plan.approve(&req, NOW).unwrap();
        self.repos.plans.approve(&approval).await.unwrap();
        ExecutionTicket {
            plan,
            status: PlanStatus::Approved,
            approval: Some(approval),
            simulate,
        }
    }

    pub async fn status(&self, id: PlanId) -> PlanStatus {
        self.repos.plans.get(id).await.unwrap().unwrap().2
    }

    pub async fn journal(&self, id: PlanId) -> Vec<(IntentRecord, Option<ResultRecord>)> {
        self.repos.journal.for_plan(id).await.unwrap()
    }

    pub async fn steps(&self, id: PlanId) -> Vec<JournalStep> {
        self.journal(id)
            .await
            .into_iter()
            .map(|(i, _)| i.step)
            .collect()
    }

    pub fn ports(&self) -> ActionPorts {
        ActionPorts {
            plans: Arc::new(self.repos.plans.clone()),
            executing: Arc::new(Executing(self.repos.plans.clone())),
            journal: Arc::new(HangJournal {
                inner: Arc::new(self.repos.journal.clone()),
                hang: self.hang.clone(),
                rewrite_done_as_failed: self.rewrite_done_as_failed.clone(),
            }),
            gains: Arc::new(self.repos.gains.clone()),
            scans: self.scans.clone(),
            snapshots: Arc::new(self.repos.snapshots.clone()),
            fs: self.fs.clone(),
            content: self.content.clone(),
            qbit: Some(self.qbit.clone()),
            arr: vec![Arc::new(ArrReadFake)],
            jellyfin: Some(Arc::new(JellyfinReadFake)),
            clock: self.clock.clone(),
            ids: self.ids.clone(),
        }
    }

    pub fn settings(&self) -> ActionSettings {
        ActionSettings {
            config_fingerprint: CFG.into(),
            recheck_timeout: Duration::from_millis(80),
            recheck_interval: Duration::from_millis(5),
            verifier: ContentVerifier::new(ReadLimiter::new(1), 65_536),
        }
    }

    /// Live mutators: the real `LiveFsMutator` behind a counter, the fake qBittorrent.
    pub fn live(&self) -> (Mutators, Arc<CountingMutator>) {
        let live = LiveMode::for_tests();
        let counting = CountingMutator::new(Arc::new(LiveFsMutator::new(&self.roots, &live)));
        let m = Mutators::live(
            counting.clone(),
            Some(self.qbit.clone()),
            vec![(
                removarr_domain::arr::ArrService::Sonarr,
                self.arr_cmd.clone(),
            )],
            Some(self.jellyfin_cmd.clone()),
        );
        (m, counting)
    }

    /// Calls seen by the live ARR and Jellyfin command fakes (must stay zero in every guard).
    pub fn service_mutations(&self) -> usize {
        self.arr_cmd.0.load(Ordering::SeqCst) + self.jellyfin_cmd.0.load(Ordering::SeqCst)
    }

    /// Adds a torrent `h` that also holds `rel` under `root` (a cross-seed after planning).
    pub fn add_holder(&self, h: char, root: &RootId, rel: &str) {
        self.qbit.add(h, root, rel, &[(rel, rel)]);
    }

    /// The journaled `observed` payload of the last `QbitStart` intent.
    pub async fn last_start_observed(&self, id: PlanId) -> Option<serde_json::Value> {
        self.journal(id)
            .await
            .into_iter()
            .rev()
            .find(|(i, _)| i.step == JournalStep::QbitStart)
            .map(|(i, _)| i.observed)
    }
}
