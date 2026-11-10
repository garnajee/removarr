//! Mutating adapter. Constructible only with the `LiveMode` token.
//!
//! Mutations are never retried automatically: after a transport error the outcome is unknown and
//! the caller must re-read the torrent list before deciding anything.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use removarr_config::LiveMode;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ports::{CancelFlag, QbitControl};
use removarr_domain::torrent::{RecheckObservation, RecheckStatus, TorrentHash, evaluate_recheck};
use reqwest::Method;

use crate::http::Inner;
use crate::reader::QbitReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Stop,
    Start,
    Recheck,
    RemoveTask,
}

impl Cmd {
    /// 4.x: `pause`/`resume`. 5.x: `stop`/`start`. Other majors are not driven.
    fn endpoint(self, major: u8) -> Option<&'static str> {
        match (self, major) {
            (Cmd::Stop, 4) => Some("/api/v2/torrents/pause"),
            (Cmd::Stop, 5) => Some("/api/v2/torrents/stop"),
            (Cmd::Start, 4) => Some("/api/v2/torrents/resume"),
            (Cmd::Start, 5) => Some("/api/v2/torrents/start"),
            (Cmd::Recheck, 4 | 5) => Some("/api/v2/torrents/recheck"),
            (Cmd::RemoveTask, 4 | 5) => Some("/api/v2/torrents/delete"),
            _ => None,
        }
    }
}

/// Polling parameters for [`QbitController::recheck_and_wait`]. The architecture polls every
/// 2 s with `thresholds.recheck_timeout_seconds` as the timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecheckWait {
    pub timeout: Duration,
    pub interval: Duration,
}

#[derive(Debug, Clone)]
pub struct QbitController {
    inner: Arc<Inner>,
}

impl QbitController {
    /// Shares the reader's session. The `LiveMode` token proves `dry_run = false`.
    #[must_use]
    pub fn new(reader: &QbitReader, _live: &LiveMode) -> Self {
        Self {
            inner: Arc::clone(&reader.inner),
        }
    }

    async fn mutate(&self, cmd: Cmd, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        if hashes.is_empty() {
            return Ok(());
        }
        // Authentication uncertainty blocks mutation: login and version discovery must succeed.
        let mut version = self.inner.version().await?;
        // Fail closed: mapping status always comes from a listing taken right now, never from an
        // earlier call. Every hash must be listed and have certain paths.
        let listed: BTreeSet<TorrentHash> = self
            .inner
            .list_torrents()
            .await?
            .into_iter()
            .map(|t| t.hash)
            .collect();
        if hashes.iter().any(|h| !listed.contains(h)) {
            return Err(PortError::new(
                ErrorClass::NotFound,
                "torrent not in the current listing; mutation refused",
            ));
        }
        let uncertain = hashes
            .iter()
            .filter(|h| self.inner.mapping_uncertain(h))
            .count();
        if uncertain > 0 {
            return Err(PortError::new(
                ErrorClass::Rejected,
                format!("path mapping is uncertain for {uncertain} torrent(s); mutation refused"),
            ));
        }
        let joined = hashes
            .iter()
            .map(TorrentHash::as_str)
            .collect::<Vec<_>>()
            .join("|");
        for attempt in 0..2 {
            let Some(endpoint) = cmd.endpoint(version.major) else {
                return Err(PortError::new(
                    ErrorClass::Rejected,
                    format!("qbittorrent {} is not supported for mutation", version.raw),
                ));
            };
            let mut form: Vec<(&str, &str)> = vec![("hashes", joined.as_str())];
            if cmd == Cmd::RemoveTask {
                // The only place a delete flag is ever sent, and it is the literal false.
                form.push(("deleteFiles", "false"));
            }
            match self
                .inner
                .authed(Method::POST, endpoint, &[], Some(&form))
                .await
            {
                Ok(_) => return Ok(()),
                // 404 on stop/start: the server may have been upgraded. Nothing was executed,
                // so one repeat against the re-discovered version is safe.
                Err(e)
                    if e.class == ErrorClass::NotFound
                        && attempt == 0
                        && matches!(cmd, Cmd::Stop | Cmd::Start) =>
                {
                    let fresh = self.inner.fetch_version().await?;
                    if fresh.major == version.major {
                        return Err(e);
                    }
                    version = fresh;
                }
                Err(mut e) => {
                    if e.class == ErrorClass::Unavailable {
                        e.detail.push_str("; outcome unknown, not retried");
                    }
                    return Err(e);
                }
            }
        }
        Err(PortError::new(
            ErrorClass::Rejected,
            "qbittorrent version changed repeatedly",
        ))
    }

    /// Sends the recheck, then polls the torrent list until every hash is `Verified`, `Failed`,
    /// or `timeout` elapses (`TimedOut` for the rest). Verification follows
    /// `evaluate_recheck`: a checking state must have been observed and ended.
    ///
    /// Never starts or removes anything. On `Err` (cancelled, transport failure) the caller
    /// leaves the hashes stopped. A hash missing from a listing is `Failed`.
    pub async fn recheck_and_wait(
        &self,
        hashes: &BTreeSet<TorrentHash>,
        wait: &RecheckWait,
        cancel: &CancelFlag,
    ) -> Result<BTreeMap<TorrentHash, RecheckStatus>, PortError> {
        if cancel.is_cancelled() {
            return Err(PortError::new(ErrorClass::Cancelled, "recheck cancelled"));
        }
        self.recheck(hashes).await?;
        let started = Instant::now();
        let mut obs: BTreeMap<TorrentHash, Vec<RecheckObservation>> =
            hashes.iter().map(|h| (h.clone(), Vec::new())).collect();
        let mut gone: BTreeSet<TorrentHash> = BTreeSet::new();
        loop {
            if cancel.is_cancelled() {
                return Err(PortError::new(
                    ErrorClass::Cancelled,
                    "recheck polling cancelled",
                ));
            }
            let list = self.inner.list_torrents().await?;
            for h in hashes {
                match list.iter().find(|t| &t.hash == h) {
                    Some(t) => {
                        if let Some(v) = obs.get_mut(h) {
                            v.push(RecheckObservation {
                                state: t.state.clone(),
                                progress: t.progress,
                            });
                        }
                    }
                    None => {
                        gone.insert(h.clone());
                    }
                }
            }
            let timed_out = started.elapsed() >= wait.timeout;
            let result: BTreeMap<TorrentHash, RecheckStatus> = obs
                .iter()
                .map(|(h, o)| {
                    let st = if gone.contains(h) {
                        RecheckStatus::Failed
                    } else {
                        evaluate_recheck(o, timed_out)
                    };
                    (h.clone(), st)
                })
                .collect();
            if timed_out || result.values().all(|s| *s != RecheckStatus::Pending) {
                return Ok(result);
            }
            let remaining = wait.timeout.saturating_sub(started.elapsed());
            tokio::time::sleep(wait.interval.min(remaining)).await;
        }
    }
}

#[async_trait]
impl QbitControl for QbitController {
    async fn stop(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate(Cmd::Stop, hashes).await
    }

    async fn start(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate(Cmd::Start, hashes).await
    }

    async fn recheck(&self, hashes: &BTreeSet<TorrentHash>) -> Result<(), PortError> {
        self.mutate(Cmd::Recheck, hashes).await
    }

    async fn remove_tasks_keep_data(
        &self,
        hashes: &BTreeSet<TorrentHash>,
    ) -> Result<(), PortError> {
        self.mutate(Cmd::RemoveTask, hashes).await
    }
}
