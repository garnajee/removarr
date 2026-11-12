//! Ports: the traits adapters implement. Filesystem ports are synchronous (callers use
//! `spawn_blocking`); network, persistence and process ports are async and object safe.
//!
//! Read methods must be safe to drop (cancellation). Mutating async methods must not be dropped
//! mid-flight (architecture section 8.2). `QbitControl` has no delete-files parameter: the
//! adapter sends `deleteFiles=false` as a literal (INV-06).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;

use crate::arr::{
    ArrCandidateDecision, ArrEpisode, ArrHistoryRecord, ArrLibraryEntry, ArrMediaManagement,
    ArrMediaRef, ArrQualitySnapshot, ArrQueueItem, ArrService, CommandState, ManualImportRequest,
    ReleaseRef, ReleaseSearchResult,
};
use crate::error::PortError;
use crate::evidence::{ContentProof, Evidence, MatchCandidate};
use crate::fs::{
    CompareOutcome, DiffStats, DirSnapshot, EntrySignature, PermissionFinding, PhysicalId,
    RootInventory, SampleSpec,
};
use crate::gain::{GainRecord, ObservedDelta};
use crate::ids::{ActionRef, IssueId, JobId, PlanId, SessionId, SubjectKey};
use crate::issue::Issue;
use crate::jellyfin::{JellyfinItemRef, JellyfinLibrary};
use crate::jobs::{JobOutcome, JobRecord, Lane};
use crate::journal::{ActionOutcome, IntentRecord, ResultRecord, UnfinishedIntent};
use crate::media::ProbeReport;
use crate::path::{RootId, RootedPath};
use crate::plan::{ActionPlan, Approval, PlanHash, PlanStatus};
use crate::session::{AuditSession, ServiceStatus};
use crate::time::Timestamp;
use crate::torrent::{TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot};

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub trait IdGen: Send + Sync {
    fn uuid(&self) -> uuid::Uuid;
}

/// Cooperative cancellation for blocking filesystem work.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Cancellation plus a progress counter (in memory only; never written per file).
#[derive(Debug, Clone, Default)]
pub struct ScanControl {
    pub cancel: CancelFlag,
    pub progress: Arc<AtomicU64>,
}

// ---- Filesystem (impl: removarr-fs). Every path is a RootedPath; implementations resolve it
// from the root directory handle with no-follow semantics and reject symlinked components.
pub trait FsInventory: Send + Sync {
    fn scan_root(&self, root: &RootId, ctl: &ScanControl) -> Result<RootInventory, PortError>;
    /// `None` means the entry does not exist.
    fn stat(&self, path: &RootedPath) -> Result<Option<EntrySignature>, PortError>;
    fn snapshot_dir(&self, dir: &RootedPath) -> Result<DirSnapshot, PortError>;
    fn free_space(&self, root: &RootId) -> Result<u64, PortError>;
    fn permission_findings(&self, inv: &RootInventory, owner: (u32, u32))
    -> Vec<PermissionFinding>;
}

pub trait ContentReader: Send + Sync {
    /// Samples can only eliminate a candidate; a positive result is never equality.
    fn samples_match(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        spec: SampleSpec,
        ctl: &ScanControl,
    ) -> Result<bool, PortError>;
    fn full_compare(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        chunk_bytes: u32,
        ctl: &ScanControl,
    ) -> Result<CompareOutcome, PortError>;
}

/// The live implementation requires a `LiveMode` token (removarr-config).
pub trait FsMutator: Send + Sync {
    fn probe_hardlink(&self, source: &RootedPath, target_dir: &RootedPath)
    -> Result<(), PortError>;
    /// Temporary link, atomic rename, verify, fsync the directory.
    fn replace_with_hardlink(
        &self,
        source: &RootedPath,
        target: &RootedPath,
        expect_source: &EntrySignature,
        expect_target: &EntrySignature,
    ) -> Result<PhysicalId, PortError>;
    fn remove_file(&self, target: &RootedPath, expect: &EntrySignature) -> Result<(), PortError>;
    fn remove_tree(&self, dir: &RootedPath, expect: &DirSnapshot) -> Result<(), PortError>;
    fn remove_empty_dir(&self, dir: &RootedPath, expect: &EntrySignature) -> Result<(), PortError>;
    /// Tier 4 only (`DeleteMediaFile`, CP2T4 T4-13): removes one regular media-root file whose
    /// signature (including `nlink`) matches. A dedicated entry point, so `remove_file` keeps
    /// refusing media roots. The default refuses; only the live adapter implements it.
    fn remove_media_file(
        &self,
        _target: &RootedPath,
        _expect: &EntrySignature,
    ) -> Result<(), PortError> {
        Err(PortError::new(
            crate::error::ErrorClass::CapabilityDisabled,
            "removing a media file is not available in this adapter",
        ))
    }
    fn set_mode(
        &self,
        target: &RootedPath,
        expect: &EntrySignature,
        mode: u32,
    ) -> Result<(), PortError>;
}

// ---- qBittorrent (impl: removarr-qbit)
#[async_trait]
pub trait QbitRead: Send + Sync {
    async fn version(&self) -> Result<String, PortError>;
    async fn torrents(&self) -> Result<Vec<TorrentInfo>, PortError>;
    async fn files(&self, hash: &TorrentHash) -> Result<Vec<TorrentFile>, PortError>;
}

#[async_trait]
pub trait QbitControl: Send + Sync {
    async fn stop(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;
    async fn start(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;
    async fn recheck(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError>;
    /// Removes tasks and never data: there is no boolean parameter.
    async fn remove_tasks_keep_data(&self, hashes: &BTreeSet<TorrentHash>)
    -> Result<(), PortError>;
}

// ---- Sonarr / Radarr (impl: removarr-arr), one instance per service
#[async_trait]
pub trait ArrRead: Send + Sync {
    fn service(&self) -> ArrService;
    async fn status(&self) -> Result<String, PortError>;
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError>;
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError>;
    async fn history_for_download(
        &self,
        hash: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError>;
    async fn quality_snapshot(&self, media: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError>;
    /// GET only: never imports.
    async fn manual_import_preview(
        &self,
        folder: &RootedPath,
        media: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError>;
    /// Sonarr: ALL expected episodes, with or without files.
    async fn episodes(
        &self,
        series_id: i64,
        season: Option<i32>,
    ) -> Result<Vec<ArrEpisode>, PortError>;
    /// GET `/release`: never grabs.
    async fn search_releases(
        &self,
        media: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError>;
    async fn command_status(&self, id: i64) -> Result<CommandState, PortError>;
    /// GET `config/mediamanagement`: recycle bin and hardlink setting.
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError>;
}

/// The live implementation requires a `LiveMode` token and a per-method capability.
#[async_trait]
pub trait ArrCommands: Send + Sync {
    async fn rescan(&self, media: &ArrMediaRef) -> Result<i64, PortError>;
    async fn rescan_all(&self) -> Result<i64, PortError>;
    /// The request has no mode field: the adapter always sends `importMode = "copy"`.
    async fn manual_import(&self, req: &ManualImportRequest) -> Result<i64, PortError>;
    async fn grab(&self, release: &ReleaseRef) -> Result<(), PortError>;
    async fn remove_queue_item_keep_media(&self, id: i64) -> Result<(), PortError>;
    async fn set_monitored(&self, media: &ArrMediaRef, monitored: bool) -> Result<(), PortError>;
}

// ---- Jellyfin (impl: removarr-jellyfin)
#[async_trait]
pub trait JellyfinRead: Send + Sync {
    async fn status(&self) -> Result<String, PortError>;
    async fn libraries(&self) -> Result<Vec<JellyfinLibrary>, PortError>;
    async fn items_by_paths(
        &self,
        paths: &[RootedPath],
    ) -> Result<BTreeMap<RootedPath, Option<JellyfinItemRef>>, PortError>;
    async fn library_refresh_running(&self) -> Result<bool, PortError>;
}

#[async_trait]
pub trait JellyfinCommands: Send + Sync {
    async fn refresh_library(&self) -> Result<(), PortError>;
    async fn refresh_item(&self, id: &str) -> Result<(), PortError>;
}

// ---- ffprobe (impl: removarr-matching::ffprobe)
#[async_trait]
pub trait MediaProbe: Send + Sync {
    async fn probe(&self, path: &RootedPath) -> Result<ProbeReport, PortError>;
}

// ---- Persistence (impl: removarr-storage). Each method is ONE SQLite transaction.
#[async_trait]
pub trait ScanStore: Send + Sync {
    async fn begin_session(&self, s: &AuditSession) -> Result<(), PortError>;
    async fn save_inventory(
        &self,
        session: SessionId,
        inv: &RootInventory,
    ) -> Result<DiffStats, PortError>;
    async fn finish_session(&self, s: &AuditSession) -> Result<(), PortError>;
    async fn latest_session(&self) -> Result<Option<AuditSession>, PortError>;
    async fn load_inventory(&self, root: &RootId) -> Result<RootInventory, PortError>;
}

#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn save_torrents(&self, session: SessionId, s: &TorrentSnapshot)
    -> Result<(), PortError>;
    async fn latest_torrents(&self) -> Result<Option<TorrentSnapshot>, PortError>;
    async fn save_arr(&self, s: &ArrQualitySnapshot) -> Result<(), PortError>;
    async fn save_service_status(
        &self,
        session: SessionId,
        s: &[ServiceStatus],
    ) -> Result<(), PortError>;
}

#[async_trait]
pub trait MatchStore: Send + Sync {
    async fn replace_for_session(
        &self,
        session: SessionId,
        c: &[MatchCandidate],
    ) -> Result<(), PortError>;
    async fn update_proof(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        proof: &ContentProof,
    ) -> Result<(), PortError>;
    async fn current(&self) -> Result<Vec<MatchCandidate>, PortError>;
}

#[async_trait]
pub trait IssueStore: Send + Sync {
    /// Keeps `first_seen_at` of issues that already exist.
    async fn replace_current(&self, session: SessionId, issues: &[Issue]) -> Result<(), PortError>;
    async fn get_many(&self, ids: &[IssueId]) -> Result<Vec<Issue>, PortError>;
    async fn attach_evidence(&self, id: &IssueId, ev: &[Evidence]) -> Result<(), PortError>;
}

#[async_trait]
pub trait IgnoreStore: Send + Sync {
    async fn ignore(
        &self,
        subjects: &[SubjectKey],
        reason: Option<&str>,
        at: Timestamp,
    ) -> Result<(), PortError>;
    async fn restore(&self, subjects: &[SubjectKey]) -> Result<(), PortError>;
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError>;
}

#[async_trait]
pub trait PlanStore: Send + Sync {
    /// Plan and actions atomically.
    async fn insert(&self, plan: &ActionPlan, hash: &PlanHash) -> Result<(), PortError>;
    async fn get(
        &self,
        id: PlanId,
    ) -> Result<Option<(ActionPlan, PlanHash, PlanStatus, Option<Approval>)>, PortError>;
    /// `Conflict` unless the plan is `Draft`, the hash is equal and the plan is not expired.
    async fn approve(&self, a: &Approval) -> Result<(), PortError>;
    /// Compare-and-swap on the status.
    async fn transition(
        &self,
        id: PlanId,
        from: PlanStatus,
        to: PlanStatus,
    ) -> Result<(), PortError>;
}

#[async_trait]
pub trait JournalStore: Send + Sync {
    /// Durable (`synchronous = FULL`) before returning.
    async fn record_intent(&self, i: &IntentRecord) -> Result<(), PortError>;
    async fn record_result(&self, r: &ResultRecord) -> Result<(), PortError>;
    async fn record_outcome(&self, a: ActionRef, o: &ActionOutcome) -> Result<(), PortError>;
    /// Live intents only; simulated intents are ignored.
    async fn unfinished(&self) -> Result<Vec<UnfinishedIntent>, PortError>;
    async fn for_plan(
        &self,
        id: PlanId,
    ) -> Result<Vec<(IntentRecord, Option<ResultRecord>)>, PortError>;
}

#[async_trait]
pub trait GainStore: Send + Sync {
    /// `gain_id` is UNIQUE: a duplicate is a `Conflict`.
    async fn record(
        &self,
        gains: &[GainRecord],
        delta: Option<&ObservedDelta>,
    ) -> Result<(), PortError>;
}

#[async_trait]
pub trait JobStore: Send + Sync {
    /// UNIQUE `dedupe_key` while queued or running.
    async fn enqueue(&self, job: &JobRecord) -> Result<JobId, PortError>;
    async fn claim_next(&self, lane: Lane) -> Result<Option<JobRecord>, PortError>;
    /// Stage changes only, never per-file progress.
    async fn set_stage(&self, id: JobId, stage: &str) -> Result<(), PortError>;
    async fn finish(&self, id: JobId, outcome: &JobOutcome) -> Result<(), PortError>;
    /// At startup: marks running jobs `Interrupted`.
    async fn interrupt_running(&self) -> Result<Vec<JobRecord>, PortError>;
}

/// Small `app_state` key/value (for example `auto_paused`).
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, PortError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flag_is_shared_between_clones() {
        let f = CancelFlag::new();
        let g = f.clone();
        assert!(!g.is_cancelled());
        f.cancel();
        assert!(g.is_cancelled());
    }

    // Compile-time check: every async port is object safe.
    #[allow(dead_code)]
    struct ObjectSafe {
        qbit_read: Arc<dyn QbitRead>,
        qbit_control: Arc<dyn QbitControl>,
        arr_read: Arc<dyn ArrRead>,
        arr_commands: Arc<dyn ArrCommands>,
        jellyfin_read: Arc<dyn JellyfinRead>,
        jellyfin_commands: Arc<dyn JellyfinCommands>,
        probe: Arc<dyn MediaProbe>,
        fs: Arc<dyn FsInventory>,
        reader: Arc<dyn ContentReader>,
        mutator: Arc<dyn FsMutator>,
        scan: Arc<dyn ScanStore>,
        snapshots: Arc<dyn SnapshotStore>,
        matches: Arc<dyn MatchStore>,
        issues: Arc<dyn IssueStore>,
        ignores: Arc<dyn IgnoreStore>,
        plans: Arc<dyn PlanStore>,
        journal: Arc<dyn JournalStore>,
        gains: Arc<dyn GainStore>,
        jobs: Arc<dyn JobStore>,
        state: Arc<dyn StateStore>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    }
}
