//! Pure safety rules: state classification, task removal decision, restore planning.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::path::RootedPath;
use removarr_domain::torrent::{
    MappedPath, RecheckStatus, TorrentFile, TorrentHash, TorrentInfo, TorrentState,
    destructive_eligible, was_active,
};

/// Why a torrent is protected from destructive consideration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    /// `progress < 1` (or NaN).
    NotComplete,
    /// State outside the allowlist, including `Unknown` and `missingFiles`.
    State(TorrentState),
    /// `pausedUP` on a 5.x server or `stoppedUP` on a 4.x server: not a name that version emits.
    StateNotValidForVersion(TorrentState),
    /// `save_path`, `content_path` or `download_path` is `NoRule` or `Invalid`.
    MappingUncertain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eligibility {
    Eligible,
    Protected(Protection),
}

/// Fail-closed allowlist: `progress == 1` and `uploading`, `stalledUP`, `queuedUP`, `forcedUP`,
/// `pausedUP` (4.x only) or `stoppedUP` (5.x only). `stalledUP`, `stoppedUP` and `missingFiles`
/// are distinct states; everything else, including unknown strings and unknown majors, is
/// protected.
#[must_use]
pub fn classify(info: &TorrentInfo, major: u8) -> Eligibility {
    if !destructive_eligible(&info.state, info.progress) {
        return Eligibility::Protected(if info.progress >= 1.0 {
            Protection::State(info.state.clone())
        } else {
            Protection::NotComplete
        });
    }
    let version_ok = match &info.state {
        TorrentState::PausedUp => major == 4,
        TorrentState::StoppedUp => major == 5,
        _ => matches!(major, 4 | 5),
    };
    if !version_ok {
        return Eligibility::Protected(Protection::StateNotValidForVersion(info.state.clone()));
    }
    if crate::parse::info_mapping_uncertain(info) {
        return Eligibility::Protected(Protection::MappingUncertain);
    }
    Eligibility::Eligible
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepReason {
    /// The plan does not authorize removal of this task.
    NotAuthorized,
    /// No file list: nothing proves the data is gone.
    NoFilesListed,
    /// A wanted file still exists at one of its candidates.
    WantedFilePresent { index: u32 },
    /// A wanted file could not be checked: a candidate is not a mapped path or the presence
    /// check returned `None`.
    WantedFileUncertain { index: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalDecision {
    Remove,
    Keep(KeepReason),
}

/// A task may be removed only when the plan authorizes it and every wanted file (priority > 0)
/// is absent at every candidate location. `exists` answers `Some(true/false)` or `None` when the
/// filesystem could not tell; `None` and unmapped candidates keep the task. Unwanted files
/// (priority 0) do not matter.
pub fn decide_removal(
    files: &[TorrentFile],
    authorized: bool,
    exists: impl Fn(&RootedPath) -> Option<bool>,
) -> RemovalDecision {
    if !authorized {
        return RemovalDecision::Keep(KeepReason::NotAuthorized);
    }
    if files.is_empty() {
        return RemovalDecision::Keep(KeepReason::NoFilesListed);
    }
    for f in files.iter().filter(|f| f.is_wanted()) {
        for c in &f.candidates {
            let MappedPath::Mapped { path, .. } = c else {
                return RemovalDecision::Keep(KeepReason::WantedFileUncertain { index: f.index });
            };
            match exists(path) {
                Some(false) => {}
                Some(true) => {
                    return RemovalDecision::Keep(KeepReason::WantedFilePresent { index: f.index });
                }
                None => {
                    return RemovalDecision::Keep(KeepReason::WantedFileUncertain {
                        index: f.index,
                    });
                }
            }
        }
    }
    RemovalDecision::Remove
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftStoppedReason {
    /// Stopped before the action: restored as found, reported as intentionally left stopped.
    InitiallyStopped,
    /// Modified, not rechecked (or no result): partially affected, manual review.
    NotRechecked,
    RecheckTimedOut,
    RecheckFailed,
    /// Hash was modified but has no recorded initial state.
    UnknownInitialState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// Resume these (previously active and unmodified or verified).
    pub start: BTreeSet<TorrentHash>,
    /// Everything else stays stopped, with the reason (report and manual review).
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
}

/// Architecture 7.4 step 7: start a hash only when it was active initially and is either
/// unmodified or recheck-verified. Hashes the action removed must not be in `initial`.
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
            match recheck.get(h) {
                Some(RecheckStatus::Verified) => {
                    plan.start.insert(h.clone());
                }
                Some(RecheckStatus::TimedOut | RecheckStatus::Pending) => {
                    plan.left_stopped
                        .insert(h.clone(), LeftStoppedReason::RecheckTimedOut);
                }
                Some(RecheckStatus::Failed) => {
                    plan.left_stopped
                        .insert(h.clone(), LeftStoppedReason::RecheckFailed);
                }
                None => {
                    plan.left_stopped
                        .insert(h.clone(), LeftStoppedReason::NotRechecked);
                }
            }
        }
    }
    for h in modified.difference(&initial.keys().cloned().collect()) {
        plan.left_stopped
            .insert(h.clone(), LeftStoppedReason::UnknownInitialState);
    }
    plan
}
