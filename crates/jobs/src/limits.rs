//! Concurrency limits (architecture 8.4): scans, content comparisons, ffprobe processes, API
//! calls per service and mutations. Every limit is a semaphore, so a bound is a hard bound.

use std::sync::Arc;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::session::ServiceKind;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::LimitsConfig;

/// A limited resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Filesystem scans: sequential, fixed at 1.
    Scan,
    /// Full comparisons and sample reads (`content_read_concurrency`, max 4).
    Compare,
    /// ffprobe processes (`ffprobe_concurrency`, max 4).
    Probe,
    /// HTTP requests to one service (`http_concurrency_per_service`, max 16).
    Api(ServiceKind),
    /// Mutations: the single execution lane, fixed at 1 (ADR-0007).
    Mutation,
}

#[derive(Debug)]
pub struct Limits {
    scan: Arc<Semaphore>,
    compare: (Arc<Semaphore>, usize),
    probe: (Arc<Semaphore>, usize),
    api: Vec<(ServiceKind, Arc<Semaphore>, usize)>,
    mutation: Arc<Semaphore>,
}

impl Limits {
    #[must_use]
    pub fn new(cfg: &LimitsConfig) -> Self {
        let compare = cfg.content_read_concurrency.clamp(1, 4);
        let probe = cfg.ffprobe_concurrency.clamp(1, 4);
        let http = cfg.http_concurrency_per_service.clamp(1, 16);
        let sem = |n: usize| Arc::new(Semaphore::new(n));
        Self {
            scan: sem(1),
            compare: (sem(compare), compare),
            probe: (sem(probe), probe),
            api: [
                ServiceKind::Qbittorrent,
                ServiceKind::Sonarr,
                ServiceKind::Radarr,
                ServiceKind::Jellyfin,
            ]
            .into_iter()
            .map(|k| (k, sem(http), http))
            .collect(),
            mutation: sem(1),
        }
    }

    fn semaphore(&self, r: Resource) -> &Arc<Semaphore> {
        match r {
            Resource::Scan => &self.scan,
            Resource::Compare => &self.compare.0,
            Resource::Probe => &self.probe.0,
            Resource::Mutation => &self.mutation,
            Resource::Api(k) => self
                .api
                .iter()
                .find(|(s, _, _)| *s == k)
                .map_or(&self.mutation, |(_, s, _)| s),
        }
    }

    /// Maximum simultaneous holders.
    #[must_use]
    pub fn capacity(&self, r: Resource) -> usize {
        match r {
            Resource::Scan | Resource::Mutation => 1,
            Resource::Compare => self.compare.1,
            Resource::Probe => self.probe.1,
            Resource::Api(k) => self
                .api
                .iter()
                .find(|(s, _, _)| *s == k)
                .map_or(1, |(_, _, n)| *n),
        }
    }

    /// Waits for a slot; the permit releases it on drop.
    pub async fn acquire(&self, r: Resource) -> Result<OwnedSemaphorePermit, PortError> {
        Arc::clone(self.semaphore(r))
            .acquire_owned()
            .await
            .map_err(|_| PortError::new(ErrorClass::Internal, "limit semaphore closed"))
    }

    /// Free slots right now (tests and status only).
    #[must_use]
    pub fn available(&self, r: Resource) -> usize {
        self.semaphore(r).available_permits()
    }
}
