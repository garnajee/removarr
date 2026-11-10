//! Inputs of the classifier. Everything is plain data: classification performs no I/O, so a
//! refresh can re-run it from stored observations with zero filesystem calls.

use std::collections::BTreeSet;

use removarr_domain::arr::{ArrHistoryRecord, ArrLibraryEntry, ArrQueueItem, ArrService};
use removarr_domain::evidence::MatchCandidate;
use removarr_domain::fs::RootInventory;
use removarr_domain::ids::{SessionId, SubjectKey};
use removarr_domain::path::RootId;
use removarr_domain::session::{ServiceState, SessionStatus};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::TorrentSnapshot;

/// Thresholds and file-class lists (architecture section 11). Built by the application from the
/// validated configuration; this crate never reads configuration itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSettings {
    /// `expected_owner` as `(uid, gid)`.
    pub expected_owner: (u32, u32),
    /// `thresholds.incomplete_stalled_days`.
    pub incomplete_stalled_days: u32,
    /// Lower-case, dot-prefixed (`".mkv"`).
    pub video_extensions: BTreeSet<String>,
    /// Junk extensions. The `.r00` to `.r99` multi-part pattern is always junk too (OQ-02).
    pub junk_extensions: BTreeSet<String>,
    pub subtitle_extensions: BTreeSet<String>,
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

impl IssueSettings {
    /// Architecture defaults (OQ-02, OQ-13) for the given owner.
    #[must_use]
    pub fn new(expected_owner: (u32, u32)) -> Self {
        Self {
            expected_owner,
            incomplete_stalled_days: 14,
            video_extensions: set(&[".mkv", ".mp4", ".avi", ".m4v", ".ts", ".m2ts", ".wmv"]),
            junk_extensions: set(&[
                ".nfo", ".txt", ".sfv", ".md5", ".url", ".rar", ".zip", ".7z", ".par2", ".jpg",
                ".jpeg", ".png",
            ]),
            subtitle_extensions: set(&[".srt", ".ass", ".ssa", ".sub", ".idx", ".vtt"]),
        }
    }
}

/// What qBittorrent said in the observation this classification is built on.
#[derive(Debug, Clone, PartialEq)]
pub enum QbitObservation {
    /// Not configured: every classification that needs torrent ownership carries
    /// `ServiceNotConfigured(Qbittorrent)` (CP1-F10).
    NotConfigured,
    /// Configured but not readable: `ServiceUnavailable(Qbittorrent)`. Nothing is guessed from
    /// an older snapshot.
    Unavailable {
        reason: String,
    },
    Available(TorrentSnapshot),
}

/// One ARR service as read in the observation (queue, library, history of completed hashes).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrObservation {
    pub service: ArrService,
    pub state: ServiceState,
    pub captured_at: Timestamp,
    pub library: Vec<ArrLibraryEntry>,
    pub queue: Vec<ArrQueueItem>,
    pub history: Vec<ArrHistoryRecord>,
}

impl ArrObservation {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.state.is_ok()
    }
}

/// Everything the classifier needs. Built by [`crate::observe`] from live reads (scan) or from
/// stored data (refresh).
#[derive(Debug, Clone)]
pub struct Observation {
    /// Session that produced the inventories (INV-42: filesystem evidence belongs to it).
    pub session: SessionId,
    pub session_status: SessionStatus,
    /// Instant of the observation (provenance, `first_seen_at` default).
    pub observed_at: Timestamp,
    /// Instant used for ages.
    pub now: Timestamp,
    /// Every configured root. A configured root without a finished inventory is incomplete.
    pub configured_roots: Vec<RootId>,
    pub inventories: Vec<RootInventory>,
    pub qbit: QbitObservation,
    pub arr: Vec<ArrObservation>,
    /// Stored funnel output (`MatchStore::current`).
    pub matches: Vec<MatchCandidate>,
    /// Persisted ignore list (`IgnoreStore::ignored`).
    pub ignored: BTreeSet<SubjectKey>,
}
