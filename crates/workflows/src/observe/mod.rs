//! Observe (architecture 7.1): one scan session over every configured root and service, then
//! classification. Also the refresh path, which re-reads services through their APIs and
//! reclassifies from stored data with **zero filesystem calls** (CP1-F07, budget B6).
//!
//! Nothing here mutates files, torrents or ARR/Jellyfin. The only database writes are the scan
//! session, the observation snapshots and the issue set (the ports of `ScanStore`,
//! `SnapshotStore`, `MatchStore` and `IssueStore`).

mod services;

use std::sync::Arc;

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{ProtectedReason, RootInventory};
use removarr_domain::ids::SessionId;
use removarr_domain::path::{RootId, RootKind};
use removarr_domain::ports::{
    ArrRead, Clock, ContentReader, FsInventory, IdGen, IgnoreStore, IssueStore, JellyfinRead,
    MatchStore, QbitRead, ScanControl, ScanStore, SnapshotStore,
};
use removarr_domain::session::{
    AuditSession, RootStatus, ServiceState, ServiceStatus, SessionMetrics, SessionStatus,
};
use removarr_matching::{ContentVerifier, FunnelInput, MatchSettings, ReadLimiter, run_funnel};
use thiserror::Error;

pub use services::{completed_hashes, parse_major, read_arr, read_jellyfin, read_qbit};

use crate::issues::{
    ArrObservation, Classified, ClassifyError, IssueSettings, Observation, QbitObservation,
    classify,
};

#[derive(Debug, Error)]
pub enum ObserveError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error("no scan session exists yet")]
    NoSession,
    #[error(transparent)]
    Classify(#[from] ClassifyError),
}

/// Static inputs of an observation (built by the application from the validated config).
#[derive(Debug, Clone)]
pub struct ObserveConfig {
    /// Every configured root, in scan order (sequential, never parallel: HDD seek storms).
    pub roots: Vec<RootId>,
    pub settings: IssueSettings,
    pub match_settings: MatchSettings,
    pub compare_chunk_bytes: u32,
    pub content_read_concurrency: usize,
    pub http_concurrency: usize,
}

/// Every port the observe workflow uses. Services that are not configured are `None`/empty.
#[derive(Clone)]
pub struct ObservePorts {
    pub fs: Arc<dyn FsInventory>,
    pub content: Arc<dyn ContentReader>,
    pub qbit: Option<Arc<dyn QbitRead>>,
    pub arr: Vec<Arc<dyn ArrRead>>,
    pub jellyfin: Option<Arc<dyn JellyfinRead>>,
    pub scans: Arc<dyn ScanStore>,
    pub snapshots: Arc<dyn SnapshotStore>,
    pub matches: Arc<dyn MatchStore>,
    pub issues: Arc<dyn IssueStore>,
    pub ignores: Arc<dyn IgnoreStore>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

impl std::fmt::Debug for ObservePorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservePorts")
            .field("qbit", &self.qbit.is_some())
            .field("arr", &self.arr.len())
            .field("jellyfin", &self.jellyfin.is_some())
            .finish_non_exhaustive()
    }
}

/// Service observations kept in memory between a scan and later refreshes (ARR library and
/// queue are not persisted by any port). A missing entry is re-read through the API.
#[derive(Debug, Clone, Default)]
pub struct ServiceCache {
    pub qbit: Option<QbitObservation>,
    pub arr: Option<Vec<ArrObservation>>,
    pub jellyfin: Option<ServiceStatus>,
}

/// Which service a refresh re-reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTarget {
    /// Re-read nothing that is cached; reclassify only.
    ClassificationOnly,
    Qbittorrent,
    Arr,
    Jellyfin,
    All,
}

#[derive(Debug)]
pub struct ScanReport {
    pub session: AuditSession,
    /// `None` when the session did not finish (cancelled, failed): no issues are produced.
    pub classified: Option<Classified>,
    pub services: ServiceCache,
}

fn join_error(e: &tokio::task::JoinError) -> PortError {
    PortError::new(ErrorClass::Internal, format!("blocking task failed: {e}"))
}

fn root_status(inv: &RootInventory) -> RootStatus {
    RootStatus {
        root: inv.root.clone(),
        scanned: inv.finished,
        error_count: u32::try_from(inv.errors.len()).unwrap_or(u32::MAX),
        protected_count: u32::try_from(inv.protected.len()).unwrap_or(u32::MAX),
    }
}

/// `CompleteWithErrors` for any scan error, `MountBoundary`/`NonUtf8Name` zone or configured
/// service that is not `Ok`. `QbitInternal` zones are expected and do not degrade the session
/// (CP1-F19); a service that is simply not configured does not either.
fn final_status(inventories: &[RootInventory], services: &[ServiceStatus]) -> SessionStatus {
    let degraded_scan = inventories.iter().any(|i| {
        !i.errors.is_empty()
            || i.protected
                .iter()
                .any(|z| z.reason != ProtectedReason::QbitInternal)
    });
    let degraded_service = services
        .iter()
        .any(|s| !s.state.is_ok() && s.state != ServiceState::NotConfigured);
    if degraded_scan || degraded_service {
        SessionStatus::CompleteWithErrors
    } else {
        SessionStatus::Complete
    }
}

async fn finish(
    ports: &ObservePorts,
    session: &mut AuditSession,
    status: SessionStatus,
) -> Result<(), PortError> {
    session
        .finish(status, ports.clock.now())
        .map_err(|e| PortError::new(e.class(), e.to_string()))?;
    session.metrics.duration_ms = u64::try_from(session.finished_at.map_or(0, |f| {
        f.as_millis().saturating_sub(session.started_at.as_millis())
    }))
    .unwrap_or(0);
    ports.scans.finish_session(session).await
}

/// Runs one scan session: roots, services, funnel, classification, issue persistence. Returns
/// `Ok` with `classified: None` when the session was cancelled or a root could not be read
/// (nothing is classified from such a session). Never mutates anything but the stores.
pub async fn run_scan(
    ports: &ObservePorts,
    cfg: &ObserveConfig,
    ctl: &ScanControl,
) -> Result<ScanReport, ObserveError> {
    let started = ports.clock.now();
    let id = SessionId::new(ports.ids.uuid());
    let mut session = AuditSession {
        id,
        started_at: started,
        finished_at: None,
        status: SessionStatus::Running,
        roots: cfg
            .roots
            .iter()
            .map(|r| RootStatus {
                root: r.clone(),
                scanned: false,
                error_count: 0,
                protected_count: 0,
            })
            .collect(),
        services: Vec::new(),
        metrics: SessionMetrics::default(),
    };
    ports.scans.begin_session(&session).await?;

    // Roots, one at a time.
    let mut inventories: Vec<RootInventory> = Vec::new();
    let mut stop: Option<SessionStatus> = None;
    for (slot, root) in cfg.roots.iter().enumerate() {
        if ctl.cancel.is_cancelled() {
            stop = Some(SessionStatus::Cancelled);
            break;
        }
        let (fs, r, c) = (Arc::clone(&ports.fs), root.clone(), ctl.clone());
        let scanned = tokio::task::spawn_blocking(move || fs.scan_root(&r, &c))
            .await
            .map_err(|e| join_error(&e))?;
        match scanned {
            Ok(inv) => {
                ports.scans.save_inventory(id, &inv).await?;
                session.roots[slot] = root_status(&inv);
                session.metrics.entries_scanned += inv.entries.len() as u64;
                session.metrics.scan_errors += inv.errors.len() as u64;
                let finished = inv.finished;
                inventories.push(inv);
                if !finished {
                    stop = Some(SessionStatus::Cancelled);
                    break;
                }
            }
            Err(_) => {
                // A root that cannot be opened is not an inventory: nothing is classified.
                stop = Some(SessionStatus::Failed);
                break;
            }
        }
    }
    if let Some(status) = stop {
        finish(ports, &mut session, status).await?;
        return Ok(ScanReport {
            session,
            classified: None,
            services: ServiceCache::default(),
        });
    }

    // Services (an outage never aborts the scan, CON-20).
    let now = ports.clock.now();
    let (qbit, qbit_status) = read_qbit(ports.qbit.as_ref(), now, cfg.http_concurrency).await;
    let hashes = match &qbit {
        QbitObservation::Available(s) => completed_hashes(s),
        _ => Default::default(),
    };
    let mut statuses = vec![qbit_status];
    let mut arr_obs = Vec::new();
    for a in &ports.arr {
        let (obs, st) = read_arr(a, &hashes, now, cfg.http_concurrency).await;
        arr_obs.push(obs);
        statuses.push(st);
    }
    let jf_status = read_jellyfin(ports.jellyfin.as_ref(), now).await;
    statuses.push(jf_status.clone());
    session.services = statuses.clone();
    if let QbitObservation::Available(s) = &qbit {
        ports.snapshots.save_torrents(id, s).await?;
    }

    // Funnel: identity -> name+size -> semantic -> samples. No full comparison in a scan.
    let previous = ports.matches.current().await?;
    // Inventories move into the blocking task and come back in scan order: no copy is made.
    let (content, ms, chunk, conc, fctl) = (
        Arc::clone(&ports.content),
        cfg.match_settings.clone(),
        cfg.compare_chunk_bytes,
        cfg.content_read_concurrency,
        ctl.clone(),
    );
    let funnel_task = tokio::task::spawn_blocking(move || {
        let mut slots: Vec<Option<RootInventory>> = vec![None; inventories.len()];
        let (mut sources, mut targets) = (Vec::new(), Vec::new());
        for (i, inv) in inventories.into_iter().enumerate() {
            match inv.root.kind() {
                RootKind::Completed => sources.push((i, inv)),
                RootKind::Media => targets.push((i, inv)),
                _ => slots[i] = Some(inv),
            }
        }
        let (src_idx, sources): (Vec<usize>, Vec<RootInventory>) = sources.into_iter().unzip();
        let (tgt_idx, targets): (Vec<usize>, Vec<RootInventory>) = targets.into_iter().unzip();
        let verifier = ContentVerifier::new(ReadLimiter::new(conc), chunk);
        let out = run_funnel(
            FunnelInput {
                sources: &sources,
                targets: &targets,
                previous: &previous,
            },
            &ms,
            &verifier,
            &*content,
            &fctl,
        );
        drop(previous);
        for (i, inv) in src_idx.into_iter().zip(sources) {
            slots[i] = Some(inv);
        }
        for (i, inv) in tgt_idx.into_iter().zip(targets) {
            slots[i] = Some(inv);
        }
        let back: Vec<RootInventory> = slots.into_iter().flatten().collect();
        (out, back)
    })
    .await
    .map_err(|e| join_error(&e))?;
    let (funnel, inventories) = funnel_task;
    let candidates = match funnel {
        Ok(out) => out.match_candidates(id),
        Err(e) => {
            let status = if e.class == ErrorClass::Cancelled {
                SessionStatus::Cancelled
            } else {
                SessionStatus::Failed
            };
            finish(ports, &mut session, status).await?;
            return Ok(ScanReport {
                session,
                classified: None,
                services: ServiceCache::default(),
            });
        }
    };
    ports.matches.replace_for_session(id, &candidates).await?;

    // Classification and persistence.
    let status = final_status(&inventories, &statuses);
    let obs = Observation {
        session: id,
        session_status: status,
        observed_at: ports.clock.now(),
        now: ports.clock.now(),
        configured_roots: cfg.roots.clone(),
        inventories,
        qbit,
        arr: arr_obs,
        matches: candidates,
        ignored: ports.ignores.ignored().await?,
    };
    let classified = classify(&obs, &cfg.settings)?;
    // Take the service observations back out for the cache; drop the rest now.
    let Observation {
        qbit, arr: arr_obs, ..
    } = obs;
    ports.issues.replace_current(id, &classified.issues).await?;
    ports.snapshots.save_service_status(id, &statuses).await?;
    finish(ports, &mut session, status).await?;
    Ok(ScanReport {
        session,
        classified: Some(classified),
        services: ServiceCache {
            qbit: Some(qbit),
            arr: Some(arr_obs),
            jellyfin: Some(jf_status),
        },
    })
}

/// Refresh: re-reads the targeted services (API calls only), then reclassifies from the stored
/// inventories, stored funnel output and the ignore list. **No filesystem port is used**: the
/// stored data is the only source for files. A service missing from `cache` is read too.
pub async fn refresh(
    ports: &ObservePorts,
    cfg: &ObserveConfig,
    target: RefreshTarget,
    cache: &mut ServiceCache,
) -> Result<Classified, ObserveError> {
    let session = ports
        .scans
        .latest_session()
        .await?
        .ok_or(ObserveError::NoSession)?;
    if !session.status.allows_planning() {
        return Err(ClassifyError::SessionNotClassifiable(session.status).into());
    }
    let now = ports.clock.now();
    let mut fresh: Vec<ServiceStatus> = Vec::new();

    let want = |t: RefreshTarget| target == RefreshTarget::All || target == t;
    if want(RefreshTarget::Qbittorrent) || cache.qbit.is_none() {
        let (q, st) = read_qbit(ports.qbit.as_ref(), now, cfg.http_concurrency).await;
        if let QbitObservation::Available(s) = &q {
            ports.snapshots.save_torrents(session.id, s).await?;
        }
        cache.qbit = Some(q);
        fresh.push(st);
    }
    if want(RefreshTarget::Arr) || cache.arr.is_none() {
        let hashes = match &cache.qbit {
            Some(QbitObservation::Available(s)) => completed_hashes(s),
            _ => Default::default(),
        };
        let mut all = Vec::new();
        for a in &ports.arr {
            let (obs, st) = read_arr(a, &hashes, now, cfg.http_concurrency).await;
            all.push(obs);
            fresh.push(st);
        }
        cache.arr = Some(all);
    }
    if want(RefreshTarget::Jellyfin) || cache.jellyfin.is_none() {
        let st = read_jellyfin(ports.jellyfin.as_ref(), now).await;
        cache.jellyfin = Some(st.clone());
        fresh.push(st);
    }
    if !fresh.is_empty() {
        ports
            .snapshots
            .save_service_status(session.id, &fresh)
            .await?;
    }

    let mut inventories = Vec::new();
    for root in &cfg.roots {
        match ports.scans.load_inventory(root).await {
            Ok(inv) => inventories.push(inv),
            // A root with no stored inventory stays absent: it classifies as not scanned.
            Err(e) if e.class == ErrorClass::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let matches = ports.matches.current().await?;
    let ignored = ports.ignores.ignored().await?;
    // The cache lends its observations to the classifier and gets them back: no copy.
    let had_qbit = cache.qbit.is_some();
    let had_arr = cache.arr.is_some();
    let obs = Observation {
        session: session.id,
        session_status: session.status,
        observed_at: now,
        now,
        configured_roots: cfg.roots.clone(),
        inventories,
        qbit: cache.qbit.take().unwrap_or(QbitObservation::NotConfigured),
        arr: cache.arr.take().unwrap_or_default(),
        matches,
        ignored,
    };
    let classified = classify(&obs, &cfg.settings);
    let Observation { qbit, arr, .. } = obs;
    cache.qbit = had_qbit.then_some(qbit);
    cache.arr = had_arr.then_some(arr);
    let classified = classified?;
    ports
        .issues
        .replace_current(session.id, &classified.issues)
        .await?;
    Ok(classified)
}
