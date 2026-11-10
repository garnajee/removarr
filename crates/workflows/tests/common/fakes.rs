#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! In-memory ports for the observe tests: stores, a counting filesystem and service fakes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrHistoryRecord, ArrLibraryEntry, ArrMediaManagement,
    ArrMediaRef, ArrQualitySnapshot, ArrQueueItem, ArrService, CommandState, ReleaseSearchResult,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{ContentProof, Evidence, MatchCandidate};
use removarr_domain::fs::{
    CompareOutcome, DiffStats, DirSnapshot, EntrySignature, PermissionFinding, RootInventory,
    SampleSpec,
};
use removarr_domain::ids::{IssueId, SessionId, SubjectKey};
use removarr_domain::issue::Issue;
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::ports::{
    ArrRead, ContentReader, FsInventory, IgnoreStore, IssueStore, MatchStore, QbitRead,
    ScanControl, ScanStore, SnapshotStore,
};
use removarr_domain::session::{AuditSession, ServiceStatus};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot};

fn err(class: ErrorClass, d: &str) -> PortError {
    PortError::new(class, d)
}

// ------------------------------------------------------------------ filesystem

#[derive(Default)]
pub struct FakeFs {
    pub inventories: Mutex<BTreeMap<RootId, RootInventory>>,
    pub fail_roots: Mutex<BTreeSet<RootId>>,
    pub scans: Mutex<Vec<RootId>>,
    /// Every other call, counted: a refresh must leave all of them at zero.
    pub other_calls: AtomicUsize,
}

impl FakeFs {
    pub fn with(invs: Vec<RootInventory>) -> Arc<Self> {
        let f = Self::default();
        *f.inventories.lock().unwrap() = invs.into_iter().map(|i| (i.root.clone(), i)).collect();
        Arc::new(f)
    }
    pub fn total_calls(&self) -> usize {
        self.scans.lock().unwrap().len() + self.other_calls.load(Ordering::SeqCst)
    }
}

impl FsInventory for FakeFs {
    fn scan_root(&self, root: &RootId, ctl: &ScanControl) -> Result<RootInventory, PortError> {
        self.scans.lock().unwrap().push(root.clone());
        if self.fail_roots.lock().unwrap().contains(root) {
            return Err(err(ErrorClass::Confinement, "cannot open root"));
        }
        let mut inv = self
            .inventories
            .lock()
            .unwrap()
            .get(root)
            .cloned()
            .expect("inventory");
        if ctl.cancel.is_cancelled() {
            inv.finished = false;
        }
        Ok(inv)
    }
    fn stat(&self, _: &RootedPath) -> Result<Option<EntrySignature>, PortError> {
        self.other_calls.fetch_add(1, Ordering::SeqCst);
        Err(err(ErrorClass::Internal, "unexpected stat"))
    }
    fn snapshot_dir(&self, _: &RootedPath) -> Result<DirSnapshot, PortError> {
        self.other_calls.fetch_add(1, Ordering::SeqCst);
        Err(err(ErrorClass::Internal, "unexpected snapshot_dir"))
    }
    fn free_space(&self, _: &RootId) -> Result<u64, PortError> {
        self.other_calls.fetch_add(1, Ordering::SeqCst);
        Err(err(ErrorClass::Internal, "unexpected free_space"))
    }
    fn permission_findings(&self, _: &RootInventory, _: (u32, u32)) -> Vec<PermissionFinding> {
        self.other_calls.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

#[derive(Default)]
pub struct FakeContent(pub AtomicUsize);

impl ContentReader for FakeContent {
    fn samples_match(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        _: [&EntrySignature; 2],
        _: SampleSpec,
        _: &ScanControl,
    ) -> Result<bool, PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
    fn full_compare(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        _: [&EntrySignature; 2],
        _: u32,
        _: &ScanControl,
    ) -> Result<CompareOutcome, PortError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("a scan or refresh must never run a full comparison");
    }
}

// ------------------------------------------------------------------ stores

#[derive(Default)]
pub struct Mem {
    pub sessions: Mutex<Vec<AuditSession>>,
    pub inventories: Mutex<BTreeMap<RootId, RootInventory>>,
    pub torrents: Mutex<Option<TorrentSnapshot>>,
    pub service_status: Mutex<Vec<ServiceStatus>>,
    pub matches: Mutex<Vec<MatchCandidate>>,
    pub issues: Mutex<Vec<Issue>>,
    pub replace_calls: AtomicUsize,
    pub ignored: Mutex<BTreeSet<SubjectKey>>,
}

#[async_trait]
impl ScanStore for Mem {
    async fn begin_session(&self, s: &AuditSession) -> Result<(), PortError> {
        self.sessions.lock().unwrap().push(s.clone());
        Ok(())
    }
    async fn save_inventory(
        &self,
        _: SessionId,
        inv: &RootInventory,
    ) -> Result<DiffStats, PortError> {
        self.inventories
            .lock()
            .unwrap()
            .insert(inv.root.clone(), inv.clone());
        Ok(DiffStats::default())
    }
    async fn finish_session(&self, s: &AuditSession) -> Result<(), PortError> {
        let mut all = self.sessions.lock().unwrap();
        let slot = all
            .iter_mut()
            .find(|x| x.id == s.id)
            .expect("session begun");
        *slot = s.clone();
        Ok(())
    }
    async fn latest_session(&self) -> Result<Option<AuditSession>, PortError> {
        Ok(self.sessions.lock().unwrap().last().cloned())
    }
    async fn load_inventory(&self, root: &RootId) -> Result<RootInventory, PortError> {
        self.inventories
            .lock()
            .unwrap()
            .get(root)
            .cloned()
            .ok_or_else(|| err(ErrorClass::NotFound, "no inventory"))
    }
}

#[async_trait]
impl SnapshotStore for Mem {
    async fn save_torrents(&self, _: SessionId, s: &TorrentSnapshot) -> Result<(), PortError> {
        *self.torrents.lock().unwrap() = Some(s.clone());
        Ok(())
    }
    async fn latest_torrents(&self) -> Result<Option<TorrentSnapshot>, PortError> {
        Ok(self.torrents.lock().unwrap().clone())
    }
    async fn save_arr(&self, _: &ArrQualitySnapshot) -> Result<(), PortError> {
        Ok(())
    }
    async fn save_service_status(
        &self,
        _: SessionId,
        s: &[ServiceStatus],
    ) -> Result<(), PortError> {
        self.service_status.lock().unwrap().extend_from_slice(s);
        Ok(())
    }
}

#[async_trait]
impl MatchStore for Mem {
    async fn replace_for_session(
        &self,
        _: SessionId,
        c: &[MatchCandidate],
    ) -> Result<(), PortError> {
        *self.matches.lock().unwrap() = c.to_vec();
        Ok(())
    }
    async fn update_proof(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        _: &ContentProof,
    ) -> Result<(), PortError> {
        Ok(())
    }
    async fn current(&self) -> Result<Vec<MatchCandidate>, PortError> {
        Ok(self.matches.lock().unwrap().clone())
    }
}

#[async_trait]
impl IssueStore for Mem {
    async fn replace_current(&self, _: SessionId, issues: &[Issue]) -> Result<(), PortError> {
        self.replace_calls.fetch_add(1, Ordering::SeqCst);
        *self.issues.lock().unwrap() = issues.to_vec();
        Ok(())
    }
    async fn get_many(&self, ids: &[IssueId]) -> Result<Vec<Issue>, PortError> {
        Ok(self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|i| ids.contains(&i.id))
            .cloned()
            .collect())
    }
    async fn attach_evidence(&self, _: &IssueId, _: &[Evidence]) -> Result<(), PortError> {
        Ok(())
    }
}

#[async_trait]
impl IgnoreStore for Mem {
    async fn ignore(
        &self,
        s: &[SubjectKey],
        _: Option<&str>,
        _: Timestamp,
    ) -> Result<(), PortError> {
        self.ignored.lock().unwrap().extend(s.iter().cloned());
        Ok(())
    }
    async fn restore(&self, s: &[SubjectKey]) -> Result<(), PortError> {
        for k in s {
            self.ignored.lock().unwrap().remove(k);
        }
        Ok(())
    }
    async fn ignored(&self) -> Result<BTreeSet<SubjectKey>, PortError> {
        Ok(self.ignored.lock().unwrap().clone())
    }
}

// ------------------------------------------------------------------ services

pub struct FakeQbit {
    pub snapshot: Mutex<Option<TorrentSnapshot>>,
    pub calls: AtomicUsize,
}

impl FakeQbit {
    pub fn new(s: Option<TorrentSnapshot>) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(s),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl QbitRead for FakeQbit {
    async fn version(&self) -> Result<String, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &*self.snapshot.lock().unwrap() {
            Some(s) => Ok(s.version.clone()),
            None => Err(err(ErrorClass::Unavailable, "connection refused")),
        }
    }
    async fn torrents(&self) -> Result<Vec<TorrentInfo>, PortError> {
        Ok(self
            .snapshot
            .lock()
            .unwrap()
            .as_ref()
            .expect("up")
            .torrents
            .clone())
    }
    async fn files(&self, h: &TorrentHash) -> Result<Vec<TorrentFile>, PortError> {
        Ok(self
            .snapshot
            .lock()
            .unwrap()
            .as_ref()
            .expect("up")
            .files
            .get(h)
            .cloned()
            .unwrap_or_default())
    }
}

pub struct FakeArr {
    pub service: ArrService,
    pub up: bool,
    pub library: Vec<ArrLibraryEntry>,
    pub queue: Vec<ArrQueueItem>,
    pub calls: AtomicUsize,
}

#[async_trait]
impl ArrRead for FakeArr {
    fn service(&self) -> ArrService {
        self.service
    }
    async fn status(&self) -> Result<String, PortError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.up {
            Ok("5.0".into())
        } else {
            Err(err(ErrorClass::Unavailable, "down"))
        }
    }
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError> {
        Ok(self.library.clone())
    }
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError> {
        Ok(self.queue.clone())
    }
    async fn history_for_download(
        &self,
        _: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError> {
        Ok(vec![])
    }
    async fn quality_snapshot(&self, _: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError> {
        panic!("a scan never asks for a quality snapshot");
    }
    async fn manual_import_preview(
        &self,
        _: &RootedPath,
        _: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError> {
        panic!("a scan never previews an import");
    }
    async fn episodes(&self, _: i64, _: Option<i32>) -> Result<Vec<ArrEpisode>, PortError> {
        panic!("a scan never reads episodes");
    }
    async fn search_releases(
        &self,
        _: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError> {
        panic!("a scan never searches releases");
    }
    async fn command_status(&self, _: i64) -> Result<CommandState, PortError> {
        panic!("a scan never reads commands");
    }
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError> {
        panic!("a scan never reads media management");
    }
}
