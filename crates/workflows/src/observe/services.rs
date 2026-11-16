//! Reads of the external services for an observation (qBittorrent, ARR, Jellyfin). API calls
//! only: nothing here touches a filesystem, and an outage is recorded, never fatal (CON-20).

use std::collections::BTreeSet;
use std::sync::Arc;

use removarr_domain::arr::ArrService;
use removarr_domain::error::ErrorClass;
use removarr_domain::path::RootKind;
use removarr_domain::ports::{ArrRead, JellyfinRead, QbitRead};
use removarr_domain::session::{ServiceKind, ServiceState, ServiceStatus};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    MappedPath, TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot,
};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::issues::{ArrObservation, QbitObservation, mapping_faults};

/// qBittorrent major version from a version string such as `v5.0.4` or `4.6.7`. Unknown or
/// unparsable gives 0, which no allowlist state accepts (fail-closed).
#[must_use]
pub fn parse_major(version: &str) -> u8 {
    let v = version.trim().trim_start_matches(['v', 'V']);
    v.split('.')
        .next()
        .and_then(|m| m.parse::<u8>().ok())
        .unwrap_or(0)
}

fn status(service: ServiceKind, state: ServiceState, at: Timestamp) -> ServiceStatus {
    ServiceStatus {
        service,
        state,
        observed_at: at,
    }
}

fn failure_state(class: &ErrorClass, detail: &str) -> ServiceState {
    match class {
        ErrorClass::Unauthorized | ErrorClass::Forbidden => ServiceState::AuthRejected,
        _ => ServiceState::Unavailable {
            reason: detail.to_string(),
        },
    }
}

/// Reads qBittorrent: version, one listing, and the file list of every torrent with bounded
/// concurrency. A torrent whose file list cannot be read stays in the snapshot without files,
/// and ownership then falls back to its whole content directory (fail-closed).
pub async fn read_qbit(
    qbit: Option<&Arc<dyn QbitRead>>,
    now: Timestamp,
    concurrency: usize,
) -> (QbitObservation, ServiceStatus) {
    let Some(q) = qbit else {
        return (
            QbitObservation::NotConfigured,
            status(ServiceKind::Qbittorrent, ServiceState::NotConfigured, now),
        );
    };
    let unavailable = |class: &ErrorClass, detail: &str| {
        (
            QbitObservation::Unavailable {
                reason: detail.to_string(),
            },
            status(ServiceKind::Qbittorrent, failure_state(class, detail), now),
        )
    };
    let version = match q.version().await {
        Ok(v) => v,
        Err(e) => return unavailable(&e.class, &e.detail),
    };
    let torrents: Vec<TorrentInfo> = match q.torrents().await {
        Ok(t) => t,
        Err(e) => return unavailable(&e.class, &e.detail),
    };
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set: JoinSet<(TorrentHash, Option<Vec<TorrentFile>>)> = JoinSet::new();
    for t in &torrents {
        let (q, sem, hash) = (Arc::clone(q), Arc::clone(&sem), t.hash.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let files = q.files(&hash).await.ok();
            (hash, files)
        });
    }
    let mut files = std::collections::BTreeMap::new();
    while let Some(res) = set.join_next().await {
        if let Ok((hash, Some(f))) = res {
            files.insert(hash, f);
        }
    }
    let snapshot = TorrentSnapshot {
        taken_at: now,
        major: parse_major(&version),
        version: version.clone(),
        torrents,
        files,
    };
    let faults = mapping_faults(&snapshot)
        .values()
        .filter(|f| f.iter().any(|(_, k)| k.is_error()))
        .count();
    let state = if faults > 0 {
        ServiceState::MappingErrors {
            count: u32::try_from(faults).unwrap_or(u32::MAX),
        }
    } else {
        ServiceState::Ok { version }
    };
    (
        QbitObservation::Available(snapshot),
        status(ServiceKind::Qbittorrent, state, now),
    )
}

/// Hashes of torrents that hold anything under a completed root (the ones ARR history is
/// worth asking about).
#[must_use]
pub fn completed_hashes(snapshot: &TorrentSnapshot) -> BTreeSet<TorrentHash> {
    let is_completed = |p: &MappedPath| {
        p.as_rooted()
            .is_some_and(|r| r.root.kind() == RootKind::Completed)
    };
    snapshot
        .torrents
        .iter()
        .filter(|t| {
            is_completed(&t.save_path)
                || is_completed(&t.content_path)
                || snapshot
                    .files
                    .get(&t.hash)
                    .is_some_and(|fs| fs.iter().flat_map(|f| &f.candidates).any(is_completed))
        })
        .map(|t| t.hash.clone())
        .collect()
}

fn arr_kind(s: ArrService) -> ServiceKind {
    match s {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

/// Reads one ARR service: status, library, queue, and the history of `hashes` (bounded). Only
/// reads: no quality snapshot, preview or search here (those run in Evaluate).
pub async fn read_arr(
    arr: &Arc<dyn ArrRead>,
    hashes: &BTreeSet<TorrentHash>,
    now: Timestamp,
    concurrency: usize,
) -> (ArrObservation, ServiceStatus) {
    let service = arr.service();
    let kind = arr_kind(service);
    let empty = |state: ServiceState| ArrObservation {
        service,
        state,
        captured_at: now,
        library: Vec::new(),
        queue: Vec::new(),
        history: Vec::new(),
    };
    let version = match arr.status().await {
        Ok(v) => v,
        Err(e) => {
            let state = failure_state(&e.class, &e.detail);
            return (empty(state.clone()), status(kind, state, now));
        }
    };
    let (library, queue) = match (arr.library().await, arr.queue().await) {
        (Ok(l), Ok(q)) => (l, q),
        (Err(e), _) | (_, Err(e)) => {
            let state = failure_state(&e.class, &e.detail);
            return (empty(state.clone()), status(kind, state, now));
        }
    };
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set = JoinSet::new();
    for h in hashes {
        let (arr, sem, h) = (Arc::clone(arr), Arc::clone(&sem), h.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            arr.history_for_download(&h).await
        });
    }
    let mut history = Vec::new();
    // A failed history call (or a panicked task) is an uncertainty, not "no history": the whole
    // service is reported as failed so nothing derived from it can be proposed (fail-closed).
    let mut failure: Option<ServiceState> = None;
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(h)) => history.extend(h),
            Ok(Err(e)) => failure = failure.or_else(|| Some(failure_state(&e.class, &e.detail))),
            Err(e) => {
                failure = failure.or_else(|| {
                    Some(ServiceState::Unavailable {
                        reason: format!("history read aborted: {e}"),
                    })
                });
            }
        }
    }
    if let Some(state) = failure {
        return (empty(state.clone()), status(kind, state, now));
    }
    let state = ServiceState::Ok { version };
    (
        ArrObservation {
            service,
            state: state.clone(),
            captured_at: now,
            library,
            queue,
            history,
        },
        status(kind, state, now),
    )
}

/// Jellyfin health and version only (recognition checks run in Evaluate and verification).
pub async fn read_jellyfin(jf: Option<&Arc<dyn JellyfinRead>>, now: Timestamp) -> ServiceStatus {
    let Some(j) = jf else {
        return status(ServiceKind::Jellyfin, ServiceState::NotConfigured, now);
    };
    let state = match j.status().await {
        Ok(version) => match j.libraries().await {
            Ok(_) => ServiceState::Ok { version },
            Err(e) => failure_state(&e.class, &e.detail),
        },
        Err(e) => failure_state(&e.class, &e.detail),
    };
    status(ServiceKind::Jellyfin, state, now)
}
