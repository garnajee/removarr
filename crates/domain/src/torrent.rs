//! qBittorrent model: hashes, states, mapped paths, snapshots and pure rules.
//!
//! Tracker URLs and magnet URIs are deliberately not modeled (INV-31).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::path::RootedPath;
use crate::time::Timestamp;

/// Lower-case hex torrent hash (40 chars for v1, 64 for v2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TorrentHash(String);

impl TorrentHash {
    /// Accepts upper or lower case hex and normalizes to lower case.
    pub fn new(s: impl AsRef<str>) -> Result<Self, DomainError> {
        let s = s.as_ref();
        let ok = (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        if ok {
            Ok(Self(s.to_ascii_lowercase()))
        } else {
            Err(DomainError::InvalidTorrentHash(s.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TorrentHash {
    type Error = DomainError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<TorrentHash> for String {
    fn from(h: TorrentHash) -> String {
        h.0
    }
}

/// Every documented qBittorrent 4.x and 5.x state. Anything else is `Unknown` and is never
/// eligible for any destructive action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TorrentState {
    Error,
    MissingFiles,
    Uploading,
    PausedUp,
    QueuedUp,
    StalledUp,
    CheckingUp,
    ForcedUp,
    Allocating,
    Downloading,
    MetaDl,
    PausedDl,
    QueuedDl,
    StalledDl,
    CheckingDl,
    ForcedDl,
    CheckingResumeData,
    Moving,
    // 5.x
    StoppedUp,
    StoppedDl,
    ForcedMetaDl,
    Unknown(String),
}

impl TorrentState {
    /// Case-sensitive parse of the API state string. Unrecognized strings become `Unknown`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "error" => Self::Error,
            "missingFiles" => Self::MissingFiles,
            "uploading" => Self::Uploading,
            "pausedUP" => Self::PausedUp,
            "queuedUP" => Self::QueuedUp,
            "stalledUP" => Self::StalledUp,
            "checkingUP" => Self::CheckingUp,
            "forcedUP" => Self::ForcedUp,
            "allocating" => Self::Allocating,
            "downloading" => Self::Downloading,
            "metaDL" => Self::MetaDl,
            "pausedDL" => Self::PausedDl,
            "queuedDL" => Self::QueuedDl,
            "stalledDL" => Self::StalledDl,
            "checkingDL" => Self::CheckingDl,
            "forcedDL" => Self::ForcedDl,
            "checkingResumeData" => Self::CheckingResumeData,
            "moving" => Self::Moving,
            "stoppedUP" => Self::StoppedUp,
            "stoppedDL" => Self::StoppedDl,
            "forcedMetaDL" => Self::ForcedMetaDl,
            other => Self::Unknown(other.to_string()),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Error => "error",
            Self::MissingFiles => "missingFiles",
            Self::Uploading => "uploading",
            Self::PausedUp => "pausedUP",
            Self::QueuedUp => "queuedUP",
            Self::StalledUp => "stalledUP",
            Self::CheckingUp => "checkingUP",
            Self::ForcedUp => "forcedUP",
            Self::Allocating => "allocating",
            Self::Downloading => "downloading",
            Self::MetaDl => "metaDL",
            Self::PausedDl => "pausedDL",
            Self::QueuedDl => "queuedDL",
            Self::StalledDl => "stalledDL",
            Self::CheckingDl => "checkingDL",
            Self::ForcedDl => "forcedDL",
            Self::CheckingResumeData => "checkingResumeData",
            Self::Moving => "moving",
            Self::StoppedUp => "stoppedUP",
            Self::StoppedDl => "stoppedDL",
            Self::ForcedMetaDl => "forcedMetaDL",
            Self::Unknown(s) => s,
        }
    }
}

impl From<String> for TorrentState {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

impl From<TorrentState> for String {
    fn from(s: TorrentState) -> String {
        s.as_str().to_string()
    }
}

/// INV-28 allowlist: destructive actions on completed data need `progress >= 1` and a seeding or
/// paused-complete state. Every other state, including `Unknown`, is protected.
#[must_use]
pub fn destructive_eligible(state: &TorrentState, progress: f64) -> bool {
    progress >= 1.0
        && matches!(
            state,
            TorrentState::Uploading
                | TorrentState::StalledUp
                | TorrentState::QueuedUp
                | TorrentState::ForcedUp
                | TorrentState::PausedUp
                | TorrentState::StoppedUp
        )
}

#[must_use]
pub fn is_checking(state: &TorrentState) -> bool {
    matches!(
        state,
        TorrentState::CheckingUp | TorrentState::CheckingDl | TorrentState::CheckingResumeData
    )
}

/// DR-27: "previously active" = state does not start with `paused` or `stopped`
/// (case-insensitive). Used to decide which torrents to resume.
#[must_use]
pub fn was_active(state: &TorrentState) -> bool {
    let s = state.as_str().to_ascii_lowercase();
    !(s.starts_with("paused") || s.starts_with("stopped"))
}

/// States in which a stalled-incomplete cleanup is refused (OQ-06).
#[must_use]
pub fn is_busy_for_incomplete_cleanup(state: &TorrentState) -> bool {
    is_checking(state)
        || matches!(
            state,
            TorrentState::Moving | TorrentState::Allocating | TorrentState::Unknown(_)
        )
}

/// One observation of a torrent during a recheck.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecheckObservation {
    pub state: TorrentState,
    pub progress: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecheckStatus {
    Verified,
    Pending,
    TimedOut,
    Failed,
}

/// CON-12. `observations` are polls taken after the recheck command, oldest first.
///
/// `Verified` only if a checking state was observed, it has ended, and the last observation has
/// `progress >= 1` in an allowlisted state. Never verified by elapsed time alone.
#[must_use]
pub fn evaluate_recheck(observations: &[RecheckObservation], timed_out: bool) -> RecheckStatus {
    let pending = if timed_out {
        RecheckStatus::TimedOut
    } else {
        RecheckStatus::Pending
    };
    let Some(last) = observations.last() else {
        return pending;
    };
    if matches!(last.state, TorrentState::Error | TorrentState::MissingFiles) {
        return RecheckStatus::Failed;
    }
    if !observations.iter().any(|o| is_checking(&o.state)) || is_checking(&last.state) {
        return pending;
    }
    if destructive_eligible(&last.state, last.progress) {
        RecheckStatus::Verified
    } else if last.progress < 1.0 || last.progress.is_nan() {
        RecheckStatus::Failed
    } else {
        pending
    }
}

/// A path as seen by a service, mapped to a configured root. Each variant keeps the raw service
/// string for display.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MappedPath {
    Mapped { raw: String, path: RootedPath },
    OutsideRoots { raw: String },
    DeclaredExternal { raw: String },
    NoRule { raw: String },
    Invalid { raw: String, reason: String },
}

impl MappedPath {
    #[must_use]
    pub fn as_rooted(&self) -> Option<&RootedPath> {
        match self {
            MappedPath::Mapped { path, .. } => Some(path),
            _ => None,
        }
    }

    #[must_use]
    pub fn raw(&self) -> &str {
        match self {
            MappedPath::Mapped { raw, .. }
            | MappedPath::OutsideRoots { raw }
            | MappedPath::DeclaredExternal { raw }
            | MappedPath::NoRule { raw }
            | MappedPath::Invalid { raw, .. } => raw,
        }
    }

    /// `NoRule` and `Invalid` block torrent-less deletions and automation (OQ-07).
    #[must_use]
    pub fn is_mapping_error(&self) -> bool {
        matches!(self, MappedPath::NoRule { .. } | MappedPath::Invalid { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentInfo {
    pub hash: TorrentHash,
    pub name: String,
    pub state: TorrentState,
    pub progress: f64,
    pub added_on: Timestamp,
    pub completion_on: Option<Timestamp>,
    pub category: String,
    pub size: u64,
    pub save_path: MappedPath,
    /// qBittorrent >= 4.4 temporary folder; `None` when unset or empty.
    pub download_path: Option<MappedPath>,
    pub content_path: MappedPath,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentFile {
    pub index: u32,
    pub name: String,
    pub size: u64,
    pub priority: i32,
    pub progress: f64,
    /// Every plausible on-disk location in DR-23 order: from `save_path`, `download_path`,
    /// `content_path`, each immediately followed by its `.!qB` variant.
    pub candidates: Vec<MappedPath>,
}

impl TorrentFile {
    /// A wanted file has a priority above zero (DR-26).
    #[must_use]
    pub fn is_wanted(&self) -> bool {
        self.priority > 0
    }
}

/// True for qBittorrent working files that belong to their torrent: `*.!qB` or a libtorrent
/// `.parts` file (`.parts` or `.<hash>.parts`). Only the last path component is examined.
#[must_use]
pub fn is_working_file(name: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    last.ends_with(".!qB") || (last.starts_with('.') && last.ends_with(".parts"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorrentSnapshot {
    pub taken_at: Timestamp,
    pub version: String,
    pub major: u8,
    pub torrents: Vec<TorrentInfo>,
    pub files: BTreeMap<TorrentHash, Vec<TorrentFile>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[&str] = &[
        "error",
        "missingFiles",
        "uploading",
        "pausedUP",
        "queuedUP",
        "stalledUP",
        "checkingUP",
        "forcedUP",
        "allocating",
        "downloading",
        "metaDL",
        "pausedDL",
        "queuedDL",
        "stalledDL",
        "checkingDL",
        "forcedDL",
        "checkingResumeData",
        "moving",
        "stoppedUP",
        "stoppedDL",
        "forcedMetaDL",
    ];

    #[test]
    fn every_documented_state_parses_and_round_trips() {
        for s in ALL {
            let st = TorrentState::parse(s);
            assert!(!matches!(st, TorrentState::Unknown(_)), "{s} must be known");
            assert_eq!(st.as_str(), *s);
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(serde_json::from_str::<TorrentState>(&json).unwrap(), st);
        }
    }

    #[test]
    fn unknown_states_are_never_eligible() {
        for s in [
            "unknown",
            "UPLOADING",
            "Uploading",
            "",
            "seeding",
            "pausedup",
        ] {
            let st = TorrentState::parse(s);
            assert!(matches!(st, TorrentState::Unknown(_)), "{s:?}");
            assert_eq!(st.as_str(), s);
            assert!(!destructive_eligible(&st, 1.0));
        }
    }

    #[test]
    fn allowlist_needs_complete_progress_and_seeding_state() {
        use TorrentState::*;
        for st in [
            Uploading, StalledUp, QueuedUp, ForcedUp, PausedUp, StoppedUp,
        ] {
            assert!(destructive_eligible(&st, 1.0), "{st:?}");
            assert!(!destructive_eligible(&st, 0.999), "{st:?}");
            assert!(!destructive_eligible(&st, f64::NAN));
        }
        for s in ALL {
            let st = TorrentState::parse(s);
            if !matches!(
                st,
                Uploading | StalledUp | QueuedUp | ForcedUp | PausedUp | StoppedUp
            ) {
                assert!(!destructive_eligible(&st, 1.0), "{s}");
            }
        }
    }

    #[test]
    fn activity_rule() {
        assert!(was_active(&TorrentState::Uploading));
        assert!(was_active(&TorrentState::Downloading));
        assert!(!was_active(&TorrentState::PausedUp));
        assert!(!was_active(&TorrentState::StoppedDl));
        assert!(!was_active(&TorrentState::Unknown("Paused-custom".into())));
        assert!(is_checking(&TorrentState::CheckingResumeData));
        assert!(!is_checking(&TorrentState::Moving));
    }

    fn obs(state: TorrentState, progress: f64) -> RecheckObservation {
        RecheckObservation { state, progress }
    }

    #[test]
    fn recheck_is_never_verified_without_observing_checking() {
        let o = [
            obs(TorrentState::PausedUp, 1.0),
            obs(TorrentState::PausedUp, 1.0),
        ];
        assert_eq!(evaluate_recheck(&o, false), RecheckStatus::Pending);
        assert_eq!(evaluate_recheck(&o, true), RecheckStatus::TimedOut);
        assert_eq!(evaluate_recheck(&[], true), RecheckStatus::TimedOut);
    }

    #[test]
    fn recheck_verified_after_checking_ended_complete() {
        let o = [
            obs(TorrentState::CheckingUp, 0.4),
            obs(TorrentState::CheckingUp, 0.9),
            obs(TorrentState::PausedUp, 1.0),
        ];
        assert_eq!(evaluate_recheck(&o, false), RecheckStatus::Verified);
        assert_eq!(evaluate_recheck(&o[..2], false), RecheckStatus::Pending);
        assert_eq!(evaluate_recheck(&o[..2], true), RecheckStatus::TimedOut);
    }

    #[test]
    fn recheck_fails_on_incomplete_or_error() {
        let short = [
            obs(TorrentState::CheckingUp, 0.5),
            obs(TorrentState::PausedUp, 0.97),
        ];
        assert_eq!(evaluate_recheck(&short, false), RecheckStatus::Failed);
        let missing = [
            obs(TorrentState::CheckingUp, 0.5),
            obs(TorrentState::MissingFiles, 0.5),
        ];
        assert_eq!(evaluate_recheck(&missing, false), RecheckStatus::Failed);
        let non_allow = [
            obs(TorrentState::CheckingUp, 0.5),
            obs(TorrentState::Moving, 1.0),
        ];
        assert_eq!(evaluate_recheck(&non_allow, false), RecheckStatus::Pending);
    }

    #[test]
    fn working_files() {
        assert!(is_working_file("Movie/file.mkv.!qB"));
        assert!(is_working_file(".parts"));
        assert!(is_working_file(
            "dir/.0123456789abcdef0123456789abcdef01234567.parts"
        ));
        assert!(!is_working_file("file.parts"));
        assert!(!is_working_file("dir.!qB/file.mkv"));
        assert!(!is_working_file("movie.mkv"));
    }

    #[test]
    fn hash_validation_normalizes_case() {
        let up = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        assert_eq!(
            TorrentHash::new(up).unwrap().as_str(),
            up.to_ascii_lowercase()
        );
        assert!(TorrentHash::new("abc").is_err());
        assert!(TorrentHash::new("g".repeat(40)).is_err());
        assert!(TorrentHash::new("a".repeat(64)).is_ok());
        assert!(serde_json::from_str::<TorrentHash>("\"zz\"").is_err());
    }

    #[test]
    fn mapped_path_helpers() {
        let m = MappedPath::NoRule { raw: "/x".into() };
        assert!(m.is_mapping_error());
        assert_eq!(m.raw(), "/x");
        assert!(m.as_rooted().is_none());
        assert!(!MappedPath::DeclaredExternal { raw: "/y".into() }.is_mapping_error());
    }
}
