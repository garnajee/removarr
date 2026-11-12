//! The qBittorrent transaction (INV-14, architecture 7.4 steps 3, 6, 7): journaled stop,
//! recheck with timeout, and restoration of only the hashes whose data is proven intact.
//!
//! `removarr-qbit` has the same pure restore rule (`plan_restore`); it is mirrored here because
//! this crate may not depend on the connector (architecture 2.2).

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::journal::JournalStep;
use removarr_domain::torrent::{
    RecheckObservation, RecheckStatus, TorrentHash, TorrentInfo, TorrentState, evaluate_recheck,
    was_active,
};
use serde::{Deserialize, Serialize};

use super::events::ActionEvent;
use super::journal::{Ctx, StepRun, json};

/// Why a hash stays stopped after the action (reported, never hidden).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftStoppedReason {
    /// Stopped before the action: left as found.
    InitiallyStopped,
    /// Its data was modified and no recheck result exists (interrupted, qBittorrent down).
    NotRechecked,
    RecheckTimedOut,
    RecheckFailed,
    /// Modified, but no initial state was recorded.
    UnknownInitialState,
    /// The start command failed; the hash is stopped and must be started by hand.
    StartFailed,
    /// No live qBittorrent controller in this process (dry-run recovery).
    NoLiveController,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub start: BTreeSet<TorrentHash>,
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
}

/// Architecture 7.4 step 7: start a hash only when it was active initially and is either
/// unmodified or recheck-verified. Everything else stays stopped with its reason.
#[must_use]
pub fn plan_restore(
    initial: &BTreeMap<TorrentHash, TorrentState>,
    modified: &BTreeSet<TorrentHash>,
    recheck: &BTreeMap<TorrentHash, RecheckStatus>,
) -> RestorePlan {
    let mut plan = RestorePlan::default();
    for (h, state) in initial {
        if !was_active(state) {
            plan.left_stopped
                .insert(h.clone(), LeftStoppedReason::InitiallyStopped);
        } else if !modified.contains(h) {
            plan.start.insert(h.clone());
        } else {
            let reason = match recheck.get(h) {
                Some(RecheckStatus::Verified) => {
                    plan.start.insert(h.clone());
                    continue;
                }
                Some(RecheckStatus::TimedOut) => LeftStoppedReason::RecheckTimedOut,
                Some(RecheckStatus::Failed) => LeftStoppedReason::RecheckFailed,
                Some(RecheckStatus::Pending) | None => LeftStoppedReason::NotRechecked,
            };
            plan.left_stopped.insert(h.clone(), reason);
        }
    }
    for h in modified {
        if !initial.contains_key(h) {
            plan.left_stopped
                .insert(h.clone(), LeftStoppedReason::UnknownInitialState);
        }
    }
    plan
}

/// State of the torrent transaction of one plan.
#[derive(Debug, Clone, Default)]
pub(crate) struct TorrentTx {
    /// States at the moment of the stop intent (journaled in that intent's `observed`).
    pub initial: BTreeMap<TorrentHash, TorrentState>,
    /// Hashes whose data this run modified (or may have modified).
    pub modified: BTreeSet<TorrentHash>,
    pub recheck: BTreeMap<TorrentHash, RecheckStatus>,
    pub started: BTreeSet<TorrentHash>,
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
    /// The stop step happened live (so compensation must restore).
    pub stopped_live: bool,
    /// Tasks removed (`deleteFiles=false`) by this run: never rechecked nor started.
    pub removed: BTreeSet<TorrentHash>,
}

impl TorrentTx {
    /// The task of `h` is gone: it leaves the recheck and restore sets.
    pub fn removed(&mut self, h: &TorrentHash) {
        self.initial.remove(h);
        self.modified.remove(h);
        self.removed.insert(h.clone());
    }
}

/// The `observed` payload of a `QbitStop` intent (recovery reads it back).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StopObserved {
    pub initial_states: BTreeMap<TorrentHash, TorrentState>,
}

pub(crate) fn no_controller() -> PortError {
    PortError::new(ErrorClass::Config, "no live qBittorrent controller")
}

impl Ctx<'_> {
    /// Journaled stop of `hashes`. `listing` is the fresh listing the revalidation just read;
    /// the initial states come from it.
    pub async fn stop_torrents(
        &self,
        action: u32,
        hashes: &BTreeSet<TorrentHash>,
        listing: &[TorrentInfo],
        tx: &mut TorrentTx,
    ) -> Result<StepRun<()>, PortError> {
        if hashes.is_empty() {
            return Ok(StepRun::Done(()));
        }
        tx.initial = listing
            .iter()
            .filter(|t| hashes.contains(&t.hash))
            .map(|t| (t.hash.clone(), t.state.clone()))
            .collect();
        let observed = StopObserved {
            initial_states: tx.initial.clone(),
        };
        let ctl = self.mutators.qbit.clone();
        let set = hashes.clone();
        let run = self
            .step(
                action,
                JournalStep::QbitStop,
                json(hashes),
                json(&observed),
                hashes.clone(),
                || async move {
                    let ctl = ctl.ok_or_else(no_controller)?;
                    ctl.stop(&set).await
                },
            )
            .await?;
        if matches!(run, StepRun::Done(())) {
            tx.stopped_live = true;
            self.reporter.emit(ActionEvent::TorrentsStopped {
                plan: self.plan.id,
                hashes: hashes.clone(),
            });
        }
        Ok(run)
    }

    /// Journaled recheck of every modified hash, polled until verified, failed or timed out.
    /// A failure leaves the hashes without a recheck result (they stay stopped).
    pub async fn recheck_modified(
        &self,
        action: u32,
        tx: &mut TorrentTx,
    ) -> Result<StepRun<BTreeMap<TorrentHash, RecheckStatus>>, PortError> {
        if tx.modified.is_empty() {
            return Ok(StepRun::Done(BTreeMap::new()));
        }
        let hashes = tx.modified.clone();
        let run = self
            .step(
                action,
                JournalStep::QbitRecheck,
                json(&hashes),
                serde_json::Value::Null,
                hashes.clone(),
                || self.recheck_and_wait(&hashes),
            )
            .await?;
        match &run {
            StepRun::Done(statuses) => tx.recheck = statuses.clone(),
            // A simulation assumes the would-be recheck verifies, so the restore step shows
            // the would-be start; the verification steps still report `Simulated`.
            StepRun::Simulated => {
                tx.recheck = hashes
                    .iter()
                    .map(|h| (h.clone(), RecheckStatus::Verified))
                    .collect();
            }
            StepRun::Failed(_) => {}
        }
        Ok(run)
    }

    /// Sends the recheck, then polls the listing every `recheck_interval` until no hash is
    /// pending or `recheck_timeout` elapses. `Verified` needs an observed checking state that
    /// ended in a complete, allowlisted state (`evaluate_recheck`, CON-12). The cancel flag is
    /// deliberately not consulted: this is compensation, bounded by the timeout (8.2).
    async fn recheck_and_wait(
        &self,
        hashes: &BTreeSet<TorrentHash>,
    ) -> Result<BTreeMap<TorrentHash, RecheckStatus>, PortError> {
        let ctl = self.mutators.qbit.clone().ok_or_else(no_controller)?;
        let reader =
            self.ports.qbit.clone().ok_or_else(|| {
                PortError::new(ErrorClass::Config, "qBittorrent is not configured")
            })?;
        ctl.recheck(hashes).await?;
        let started = tokio::time::Instant::now();
        let timeout = self.settings.recheck_timeout;
        let mut obs: BTreeMap<TorrentHash, Vec<RecheckObservation>> =
            hashes.iter().map(|h| (h.clone(), Vec::new())).collect();
        let mut gone = BTreeSet::new();
        loop {
            let list = reader.torrents().await?;
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
            let timed_out = started.elapsed() >= timeout;
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
            let remaining = timeout.saturating_sub(started.elapsed());
            tokio::time::sleep(self.settings.recheck_interval.min(remaining)).await;
        }
    }

    /// Journaled restoration: starts the hashes `plan_restore` allows and records, durably,
    /// which hashes stay stopped and why (CP2T3-F09): the `QbitStart` intent carries the start
    /// set as target and `{ recheck, left_stopped }` as observed, and is written even when
    /// nothing is started. A failed start leaves its hashes stopped (`StartFailed`).
    pub async fn restore(&self, action: u32, tx: &mut TorrentTx) -> Result<(), PortError> {
        if tx.initial.is_empty() && tx.modified.is_empty() {
            return Ok(());
        }
        let restore = plan_restore(&tx.initial, &tx.modified, &tx.recheck);
        tx.left_stopped.extend(restore.left_stopped.clone());
        let observed = RestoreObserved {
            recheck: tx.recheck.clone(),
            left_stopped: tx.left_stopped.clone(),
        };
        let ctl = self.mutators.qbit.clone();
        let set = restore.start.clone();
        let run = self
            .step(
                action,
                JournalStep::QbitStart,
                json(&restore.start),
                json(&observed),
                restore.start.clone(),
                || async move {
                    if set.is_empty() {
                        return Ok(RestoreResult {
                            started: BTreeSet::new(),
                        });
                    }
                    let ctl = ctl.ok_or_else(no_controller)?;
                    ctl.start(&set).await?;
                    Ok(RestoreResult { started: set })
                },
            )
            .await?;
        match run {
            StepRun::Done(r) => {
                if !r.started.is_empty() {
                    tx.started.extend(r.started.iter().cloned());
                    self.reporter.emit(ActionEvent::TorrentsStarted {
                        plan: self.plan.id,
                        hashes: r.started,
                    });
                }
            }
            StepRun::Simulated => {}
            StepRun::Failed(_) => {
                for h in &restore.start {
                    tx.left_stopped
                        .insert(h.clone(), LeftStoppedReason::StartFailed);
                }
            }
        }
        if !tx.left_stopped.is_empty() && !self.simulated {
            self.reporter.emit(ActionEvent::TorrentsLeftStopped {
                plan: self.plan.id,
                hashes: tx.left_stopped.clone(),
            });
        }
        Ok(())
    }
}

/// The `observed` payload of a `QbitStart` intent (durable left-stopped record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestoreObserved {
    pub recheck: BTreeMap<TorrentHash, RecheckStatus>,
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
}

/// The `final_state` of a `QbitStart` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestoreResult {
    pub started: BTreeSet<TorrentHash>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> TorrentHash {
        TorrentHash::new(std::iter::repeat_n(c, 40).collect::<String>()).unwrap()
    }

    #[test]
    fn restore_starts_only_active_hashes_that_are_untouched_or_recheck_verified() {
        let initial: BTreeMap<_, _> = [
            (h('a'), TorrentState::Uploading),
            (h('b'), TorrentState::StalledUp),
            (h('c'), TorrentState::PausedUp),
            (h('d'), TorrentState::Uploading),
            (h('e'), TorrentState::Uploading),
            (h('f'), TorrentState::Uploading),
        ]
        .into_iter()
        .collect();
        let modified: BTreeSet<_> = [h('b'), h('d'), h('e'), h('f'), h('0')]
            .into_iter()
            .collect();
        let recheck: BTreeMap<_, _> = [
            (h('b'), RecheckStatus::Verified),
            (h('d'), RecheckStatus::TimedOut),
            (h('e'), RecheckStatus::Failed),
        ]
        .into_iter()
        .collect();
        let r = plan_restore(&initial, &modified, &recheck);
        assert_eq!(r.start, [h('a'), h('b')].into_iter().collect());
        assert_eq!(
            r.left_stopped,
            [
                (h('c'), LeftStoppedReason::InitiallyStopped),
                (h('d'), LeftStoppedReason::RecheckTimedOut),
                (h('e'), LeftStoppedReason::RecheckFailed),
                (h('f'), LeftStoppedReason::NotRechecked),
                (h('0'), LeftStoppedReason::UnknownInitialState),
            ]
            .into_iter()
            .collect()
        );
    }
}
