//! Plan expiry and staleness. A plan never adapts to a changed world: time, configuration
//! (including path mappings), files, folders, scan errors, torrents, mappings, services or ARR
//! snapshots that differ from what the plan captured make it `Expired` or `Invalidated`.
//!
//! This check compares the plan with the latest *stored* observations (approval time, status
//! refresh). The action runner revalidates again against live reads before the first mutation.

use std::collections::BTreeMap;

use removarr_domain::arr::ArrMediaRef;
use removarr_domain::error::PortError;
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{ActionPlan, ActionTarget, PlanStatus, Precondition};
use removarr_domain::session::{AuditSession, ServiceKind};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    TorrentHash, TorrentInfo, TorrentState, is_busy_for_incomplete_cleanup,
};
use serde::{Deserialize, Serialize};

use super::facts::Facts;
use super::input::{MediaKey, PlanInput};
use super::request::PlanSettings;
use crate::issues::{MappingFault, QbitObservation, mapping_faults, protection};

/// One reason a plan no longer matches the world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    Expired { expires_at: Timestamp },
    ConfigChanged,
    SessionNotPlannable,
    EntryChanged { path: RootedPath },
    EntryMissing { path: RootedPath },
    DirChanged { dir: RootedPath },
    ScanErrorUnder { path: RootedPath },
    DeviceChanged { a: RootedPath, b: RootedPath },
    TorrentChanged { hash: TorrentHash },
    TorrentMissing { hash: TorrentHash },
    MappingChanged { hash: TorrentHash },
    TorrentReferences { path: RootedPath },
    ServiceNotOk { service: ServiceKind },
    ArrChanged { media: ArrMediaRef },
    ArrUnavailable { media: ArrMediaRef },
}

/// Current ARR fingerprints per media (fresh reads), for plans with ARR preconditions.
pub type ArrFingerprints = BTreeMap<MediaKey, Result<String, PortError>>;

/// Every reason `plan` is stale against the current stored state. Empty = still valid.
#[must_use]
pub fn staleness(
    plan: &ActionPlan,
    now: Timestamp,
    current: &PlanInput,
    settings: &PlanSettings,
    arr: &ArrFingerprints,
) -> Vec<StaleReason> {
    let mut out = Vec::new();
    if plan.is_expired(now) {
        out.push(StaleReason::Expired {
            expires_at: plan.expires_at,
        });
    }
    if plan.config_fingerprint != settings.config_fingerprint {
        out.push(StaleReason::ConfigChanged);
    }
    if !current.session.status.allows_planning() {
        out.push(StaleReason::SessionNotPlannable);
        return out;
    }
    let f = Facts::new(current, settings);
    let faults = match &current.observation.qbit {
        QbitObservation::Available(s) => mapping_faults(s),
        _ => BTreeMap::new(),
    };
    for hash in plan.torrent_states.keys() {
        if faults.get(hash).is_some_and(|set| {
            set.iter()
                .any(|(_, k)| matches!(k, MappingFault::NoRule | MappingFault::Invalid))
        }) {
            out.push(StaleReason::MappingChanged { hash: hash.clone() });
        }
    }
    for a in &plan.actions {
        let own: Option<&TorrentHash> = match &a.target {
            ActionTarget::Torrent { hash, .. } => Some(hash),
            _ => None,
        };
        for p in &a.preconditions {
            check(p, &f, &current.session, own, arr, &mut out);
        }
    }
    out.sort_by_key(|r| serde_json::to_string(r).unwrap_or_default());
    out.dedup();
    out
}

fn check(
    p: &Precondition,
    f: &Facts<'_>,
    session: &AuditSession,
    own: Option<&TorrentHash>,
    arr: &ArrFingerprints,
    out: &mut Vec<StaleReason>,
) {
    match p {
        Precondition::EntryUnchanged {
            path,
            sig,
            compare_nlink,
        } => match f.sig(path) {
            None => out.push(StaleReason::EntryMissing { path: path.clone() }),
            Some(cur) if !sig.matches(&cur, *compare_nlink) => {
                out.push(StaleReason::EntryChanged { path: path.clone() });
            }
            Some(_) => {}
        },
        Precondition::DirUnchanged { snapshot } => {
            if f.dir_snapshot(&snapshot.dir).as_ref() != Some(snapshot) {
                out.push(StaleReason::DirChanged {
                    dir: snapshot.dir.clone(),
                });
            }
        }
        Precondition::NoScanErrorUnder { path } => {
            if !f.scope_blockers(path).is_empty() {
                out.push(StaleReason::ScanErrorUnder { path: path.clone() });
            }
        }
        Precondition::SameDevice { a, b } => {
            if let (Some(x), Some(y)) = (f.sig(a), f.sig(b))
                && x.dev != y.dev
            {
                out.push(StaleReason::DeviceChanged {
                    a: a.clone(),
                    b: b.clone(),
                });
            }
        }
        Precondition::TorrentStates { states } => {
            for (hash, (state, progress)) in states {
                let Some((t, major)) = f.snapshot.and_then(|s| {
                    s.torrents
                        .iter()
                        .find(|t| &t.hash == hash)
                        .map(|t| (t, s.major))
                }) else {
                    out.push(StaleReason::TorrentMissing { hash: hash.clone() });
                    continue;
                };
                if torrent_changed(t, major, state, *progress) {
                    out.push(StaleReason::TorrentChanged { hash: hash.clone() });
                }
            }
        }
        Precondition::NoTorrentReferences { path } => {
            if f.snapshot.is_none() {
                out.push(StaleReason::ServiceNotOk {
                    service: ServiceKind::Qbittorrent,
                });
            } else if f.holders_under(path).iter().any(|h| Some(h) != own) {
                out.push(StaleReason::TorrentReferences { path: path.clone() });
            }
        }
        Precondition::ServiceAvailable(k) => {
            if !session
                .services
                .iter()
                .any(|s| s.service == *k && s.state.is_ok())
            {
                out.push(StaleReason::ServiceNotOk { service: *k });
            }
        }
        Precondition::ArrFingerprint { media, fingerprint } => {
            match arr.get(&(media.service, media.media_id)) {
                Some(Ok(fp)) if fp == fingerprint => {}
                Some(Ok(_)) => out.push(StaleReason::ArrChanged {
                    media: media.clone(),
                }),
                Some(Err(_)) | None => out.push(StaleReason::ArrUnavailable {
                    media: media.clone(),
                }),
            }
        }
        Precondition::OldCoverageRecoverable { old_files } => {
            for (path, phys, _) in old_files {
                match f.sig(path) {
                    Some(s) if s.phys() == *phys => {}
                    Some(_) => out.push(StaleReason::EntryChanged { path: path.clone() }),
                    None => out.push(StaleReason::EntryMissing { path: path.clone() }),
                }
            }
        }
        // Import activity and configuration are revalidated live by the runner; the config
        // fingerprint is compared above. `ArrReplacementPresent` is revalidated by its paired
        // `EntryUnchanged` and `ArrFingerprint`.
        Precondition::NoArrImport { .. }
        | Precondition::ArrReplacementPresent { .. }
        | Precondition::ConfigFingerprint(_) => {}
    }
}

/// Orchestrator rule (after domain patch 2): a torrent stays valid while it keeps the same
/// eligibility class under the version-aware allowlist (eligible or protected) and the same
/// progress; `uploading` <-> `stalledUP` is no change. A protected torrent that became busy
/// (checking, moving, allocating, unknown) also changes (stalled cleanup refuses those states).
fn torrent_changed(t: &TorrentInfo, major: u8, state: &TorrentState, progress: f64) -> bool {
    let mut planned = t.clone();
    planned.state = state.clone();
    planned.progress = progress;
    let eligible = |x: &TorrentInfo| protection(x, major, false).is_none();
    eligible(&planned) != eligible(t)
        || (t.progress - progress).abs() > f64::EPSILON
        || (is_busy_for_incomplete_cleanup(&t.state) && !is_busy_for_incomplete_cleanup(state))
}

/// Status a stale plan moves to: `Expired` when time is the only reason, `Invalidated`
/// otherwise. `None` = still valid.
#[must_use]
pub fn verdict(reasons: &[StaleReason]) -> Option<PlanStatus> {
    if reasons.is_empty() {
        None
    } else if reasons
        .iter()
        .all(|r| matches!(r, StaleReason::Expired { .. }))
    {
        Some(PlanStatus::Expired)
    } else {
        Some(PlanStatus::Invalidated)
    }
}
