#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A failed ARR history call is an explicit uncertainty (service not Ok, nothing observed),
//! never an empty "no history" success (security finding hardening note, fail-closed).

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use common::fakes::FakeArr;
use removarr_domain::arr::*;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::path::RootedPath;
use removarr_domain::ports::ArrRead;
use removarr_domain::session::ServiceState;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::TorrentHash;
use removarr_workflows::observe::read_arr;

struct FailingHistory(FakeArr);

#[async_trait]
impl ArrRead for FailingHistory {
    fn service(&self) -> ArrService {
        self.0.service()
    }
    async fn status(&self) -> Result<String, PortError> {
        self.0.status().await
    }
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError> {
        self.0.library().await
    }
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError> {
        self.0.queue().await
    }
    async fn history_for_download(
        &self,
        _: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError> {
        Err(PortError::new(ErrorClass::Unavailable, "history down"))
    }
    async fn quality_snapshot(&self, m: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError> {
        self.0.quality_snapshot(m).await
    }
    async fn manual_import_preview(
        &self,
        p: &RootedPath,
        m: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError> {
        self.0.manual_import_preview(p, m).await
    }
    async fn episodes(&self, a: i64, b: Option<i32>) -> Result<Vec<ArrEpisode>, PortError> {
        self.0.episodes(a, b).await
    }
    async fn search_releases(
        &self,
        m: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError> {
        self.0.search_releases(m).await
    }
    async fn command_status(&self, i: i64) -> Result<CommandState, PortError> {
        self.0.command_status(i).await
    }
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError> {
        self.0.media_management().await
    }
}

#[tokio::test]
async fn failed_history_call_marks_the_service_failed_not_empty_history() {
    let arr: Arc<dyn ArrRead> = Arc::new(FailingHistory(FakeArr {
        service: ArrService::Radarr,
        up: true,
        library: vec![],
        queue: vec![],
        calls: AtomicUsize::new(0),
    }));
    let hashes: BTreeSet<TorrentHash> = [TorrentHash::new("a".repeat(40)).unwrap()].into();
    let (obs, status) = read_arr(&arr, &hashes, Timestamp::from_millis(0), 2).await;
    assert!(matches!(obs.state, ServiceState::Unavailable { .. }));
    assert!(!status.state.is_ok());
    assert!(obs.history.is_empty() && obs.library.is_empty());
}
