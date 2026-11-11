//! Recording mutators and the `Mutators` bundle (architecture 6.4, ADR-0003).
//!
//! The recording set touches nothing: every method counts the call and answers with a neutral
//! value. The runner never invokes any mutator while simulating (it journals `Simulated` intents
//! and results instead), so these counters are a second barrier: a recording adapter that is
//! ever called is a bug that the guard tests catch (`calls()` must stay zero).
//!
//! The live set holds the real adapters. They can only be constructed with the
//! `removarr_config::LiveMode` token in `app::wiring`; this crate does not depend on `config`
//! (architecture 2.2), so `Mutators::live` takes the already-built adapters and the proof of
//! liveness is the existence of those values.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use removarr_domain::arr::{ArrMediaRef, ArrService, ManualImportRequest, ReleaseRef};
use removarr_domain::error::PortError;
use removarr_domain::fs::{DirSnapshot, EntrySignature, PhysicalId};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::PlanMode;
use removarr_domain::ports::{ArrCommands, FsMutator, JellyfinCommands, QbitControl};
use removarr_domain::torrent::TorrentHash;

/// Counts calls on a recording adapter (the guard tests assert zero).
#[derive(Debug, Default)]
pub struct CallCounter(AtomicUsize);

impl CallCounter {
    fn hit(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    #[must_use]
    pub fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Default)]
pub struct RecordingFsMutator {
    pub counter: CallCounter,
}

impl FsMutator for RecordingFsMutator {
    fn probe_hardlink(&self, _: &RootedPath, _: &RootedPath) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    fn replace_with_hardlink(
        &self,
        _: &RootedPath,
        _: &RootedPath,
        expect_source: &EntrySignature,
        _: &EntrySignature,
    ) -> Result<PhysicalId, PortError> {
        self.counter.hit();
        Ok(expect_source.phys())
    }

    fn remove_file(&self, _: &RootedPath, _: &EntrySignature) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    fn remove_tree(&self, _: &RootedPath, _: &DirSnapshot) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    fn remove_empty_dir(&self, _: &RootedPath, _: &EntrySignature) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    fn set_mode(&self, _: &RootedPath, _: &EntrySignature, _: u32) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RecordingQbitControl {
    pub counter: CallCounter,
}

#[async_trait]
impl QbitControl for RecordingQbitControl {
    async fn stop(&self, _: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn start(&self, _: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn recheck(&self, _: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn remove_tasks_keep_data(&self, _: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RecordingArrCommands {
    pub counter: CallCounter,
}

#[async_trait]
impl ArrCommands for RecordingArrCommands {
    async fn rescan(&self, _: &ArrMediaRef) -> Result<i64, PortError> {
        self.counter.hit();
        Ok(0)
    }

    async fn rescan_all(&self) -> Result<i64, PortError> {
        self.counter.hit();
        Ok(0)
    }

    async fn manual_import(&self, _: &ManualImportRequest) -> Result<i64, PortError> {
        self.counter.hit();
        Ok(0)
    }

    async fn grab(&self, _: &ReleaseRef) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn remove_queue_item_keep_media(&self, _: i64) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn set_monitored(&self, _: &ArrMediaRef, _: bool) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RecordingJellyfinCommands {
    pub counter: CallCounter,
}

#[async_trait]
impl JellyfinCommands for RecordingJellyfinCommands {
    async fn refresh_library(&self) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }

    async fn refresh_item(&self, _: &str) -> Result<(), PortError> {
        self.counter.hit();
        Ok(())
    }
}

/// The mutation adapters of a process. Built only in `app::wiring`.
#[derive(Clone)]
pub struct Mutators {
    pub mode: PlanMode,
    pub fs: Arc<dyn FsMutator>,
    pub qbit: Option<Arc<dyn QbitControl>>,
    pub arr: Vec<(ArrService, Arc<dyn ArrCommands>)>,
    pub jellyfin: Option<Arc<dyn JellyfinCommands>>,
}

impl std::fmt::Debug for Mutators {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mutators")
            .field("mode", &self.mode)
            .field("qbit", &self.qbit.is_some())
            .field("arr", &self.arr.iter().map(|(s, _)| *s).collect::<Vec<_>>())
            .field("jellyfin", &self.jellyfin.is_some())
            .finish()
    }
}

/// The recording set with its counters, for the dry-run guard tests.
#[derive(Debug)]
pub struct RecordingSet {
    pub fs: Arc<RecordingFsMutator>,
    pub qbit: Arc<RecordingQbitControl>,
    pub arr: Vec<(ArrService, Arc<RecordingArrCommands>)>,
    pub jellyfin: Arc<RecordingJellyfinCommands>,
}

impl RecordingSet {
    /// Total calls on every recording adapter (must be zero after any run).
    #[must_use]
    pub fn calls(&self) -> usize {
        self.fs.counter.calls()
            + self.qbit.counter.calls()
            + self
                .arr
                .iter()
                .map(|(_, a)| a.counter.calls())
                .sum::<usize>()
            + self.jellyfin.counter.calls()
    }
}

impl Mutators {
    /// Recording adapters: constructible in every process. Returns the counters too.
    #[must_use]
    pub fn recording(arr_services: &[ArrService]) -> (Self, RecordingSet) {
        let set = RecordingSet {
            fs: Arc::new(RecordingFsMutator::default()),
            qbit: Arc::new(RecordingQbitControl::default()),
            arr: arr_services
                .iter()
                .map(|s| (*s, Arc::new(RecordingArrCommands::default())))
                .collect(),
            jellyfin: Arc::new(RecordingJellyfinCommands::default()),
        };
        let m = Self {
            mode: PlanMode::DryRun,
            fs: set.fs.clone(),
            qbit: Some(set.qbit.clone()),
            arr: set
                .arr
                .iter()
                .map(|(s, a)| (*s, a.clone() as Arc<dyn ArrCommands>))
                .collect(),
            jellyfin: Some(set.jellyfin.clone()),
        };
        (m, set)
    }

    /// Live adapters, each built with `&LiveMode` by its own crate. `qbit` is `None` when
    /// qBittorrent is not configured (every torrent-touching action is then refused).
    #[must_use]
    pub fn live(
        fs: Arc<dyn FsMutator>,
        qbit: Option<Arc<dyn QbitControl>>,
        arr: Vec<(ArrService, Arc<dyn ArrCommands>)>,
        jellyfin: Option<Arc<dyn JellyfinCommands>>,
    ) -> Self {
        Self {
            mode: PlanMode::Live,
            fs,
            qbit,
            arr,
            jellyfin,
        }
    }
}
