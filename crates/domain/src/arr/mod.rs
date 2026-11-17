//! Sonarr/Radarr model. ARR is the only quality authority: nothing here ranks codecs or groups.

pub mod correlate;
pub mod snapshot;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::canonical::{canonical_json, sha256_hex};
use crate::error::DomainError;
use crate::path::RootedPath;
use crate::time::Timestamp;
use crate::torrent::{MappedPath, TorrentHash};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrService {
    Sonarr,
    Radarr,
}

impl ArrService {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ArrService::Sonarr => "sonarr",
            ArrService::Radarr => "radarr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ArrMediaRef {
    pub service: ArrService,
    pub media_id: i64,
    pub episode_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Episode,
    Special,
}

/// Media identity: what the file is, independent of its bytes and its quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaIdentity {
    pub kind: MediaKind,
    pub arr: Option<ArrMediaRef>,
    pub external_ids: BTreeMap<String, String>,
    pub title: String,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub absolute: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrFileRef {
    pub file_id: i64,
    pub path: MappedPath,
    pub size: u64,
    pub quality: String,
    pub custom_formats: Vec<String>,
    pub cf_score: i64,
    pub cutoff_not_met: bool,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionSource {
    ManualImportPreview,
    Parse,
}

/// ARR's own verdict on a candidate file or release. Never recomputed locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrCandidateDecision {
    pub subject: String,
    pub size: u64,
    pub quality: String,
    pub custom_formats: Vec<String>,
    pub cf_score: i64,
    /// Episodes ARR assigns to this file.
    pub episode_ids: Vec<i64>,
    pub approved: bool,
    pub rejections: Vec<String>,
    pub source: DecisionSource,
}

/// ARR media management settings relevant to replacement safety (CP1-F20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrMediaManagement {
    pub recycle_bin: Option<MappedPath>,
    pub recycle_bin_cleanup_days: Option<u32>,
    pub hardlinks_enabled: bool,
    pub captured_at: Timestamp,
}

/// Where an old media file's content survives after ARR's upgrade import deletes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OldFileRecovery {
    /// Another retained link (for example the completed copy) keeps the inode alive.
    OtherLink(RootedPath),
    /// ARR's recycle bin is configured.
    RecycleBin,
    /// Nothing keeps the old content. Only a manual, per-file acknowledgement lifts the block.
    Unrecoverable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrQualitySnapshot {
    pub service: ArrService,
    pub service_version: String,
    pub media: MediaIdentity,
    pub profile_id: i64,
    pub profile_name: String,
    pub cutoff: String,
    pub upgrade_allowed: bool,
    pub min_format_score: i64,
    pub cutoff_format_score: i64,
    pub format_scores: BTreeMap<String, i64>,
    pub current_files: Vec<ArrFileRef>,
    pub candidates: Vec<ArrCandidateDecision>,
    pub media_management: Option<ArrMediaManagement>,
    pub captured_at: Timestamp,
    pub fingerprint: String,
}

#[derive(Serialize)]
struct FingerprintInput<'a> {
    arr: &'a Option<ArrMediaRef>,
    profile_id: i64,
    profile_name: &'a str,
    cutoff: &'a str,
    upgrade_allowed: bool,
    min_format_score: i64,
    cutoff_format_score: i64,
    format_scores: &'a BTreeMap<String, i64>,
    current_files: &'a [ArrFileRef],
    // `captured_at` is excluded on purpose: only settings changes alter the fingerprint.
    recycle_bin: Option<&'a MappedPath>,
    recycle_bin_cleanup_days: Option<u32>,
    hardlinks_enabled: Option<bool>,
}

impl ArrQualitySnapshot {
    /// INV-34: sha256 of the canonical profile, scores, current files and media management.
    /// Candidates and capture times are excluded so a re-read of unchanged data matches.
    pub fn compute_fingerprint(&self) -> Result<String, DomainError> {
        let mm = self.media_management.as_ref();
        let input = FingerprintInput {
            arr: &self.media.arr,
            profile_id: self.profile_id,
            profile_name: &self.profile_name,
            cutoff: &self.cutoff,
            upgrade_allowed: self.upgrade_allowed,
            min_format_score: self.min_format_score,
            cutoff_format_score: self.cutoff_format_score,
            format_scores: &self.format_scores,
            current_files: &self.current_files,
            recycle_bin: mm.and_then(|m| m.recycle_bin.as_ref()),
            recycle_bin_cleanup_days: mm.and_then(|m| m.recycle_bin_cleanup_days),
            hardlinks_enabled: mm.map(|m| m.hardlinks_enabled),
        };
        Ok(sha256_hex(&canonical_json(&input)?))
    }

    /// Recomputes and stores the fingerprint.
    pub fn seal(mut self) -> Result<Self, DomainError> {
        self.fingerprint = self.compute_fingerprint()?;
        Ok(self)
    }

    #[must_use]
    pub fn fingerprint_is_valid(&self) -> bool {
        self.compute_fingerprint()
            .is_ok_and(|f| f == self.fingerprint)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrQueueItem {
    pub id: i64,
    pub download_id: Option<TorrentHash>,
    pub media: Option<ArrMediaRef>,
    pub status: String,
    pub tracked_state: String,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrHistoryRecord {
    pub download_id: Option<TorrentHash>,
    pub media: Option<ArrMediaRef>,
    pub event_type: String,
    pub source_title: String,
    pub date: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrLibraryEntry {
    pub media: ArrMediaRef,
    pub identity: MediaIdentity,
    pub file: ArrFileRef,
    pub monitored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrEpisode {
    pub id: i64,
    pub series_id: i64,
    pub season: i32,
    pub episode: i32,
    pub absolute: Option<i32>,
    pub scene_absolute: Option<i32>,
    pub scene_season: Option<i32>,
    pub scene_episode: Option<i32>,
    pub has_file: bool,
    pub file_id: Option<i64>,
    pub monitored: bool,
    pub aired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleaseRef {
    pub guid: String,
    pub indexer_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSearchResult {
    pub release: ReleaseRef,
    pub indexer: String,
    pub title: String,
    pub size: u64,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub age_days: u32,
    pub protocol: String,
    pub quality: String,
    pub custom_formats: Vec<String>,
    pub cf_score: i64,
    pub approved: bool,
    pub rejections: Vec<String>,
    pub download_allowed: bool,
}

/// Orders search results: ARR-approved first, then higher custom-format score, then larger
/// estimated gain (`current_allocated - size`). Ties break on title then guid, so the order is
/// deterministic. Ordering is a presentation aid and never grants a grab.
pub fn sort_search_results(results: &mut [ReleaseSearchResult], current_allocated: u64) {
    let gain = |r: &ReleaseSearchResult| i128::from(current_allocated) - i128::from(r.size);
    results.sort_by(|a, b| {
        (
            Reverse(a.approved),
            Reverse(a.cf_score),
            Reverse(gain(a)),
            &a.title,
            &a.release.guid,
        )
            .cmp(&(
                Reverse(b.approved),
                Reverse(b.cf_score),
                Reverse(gain(b)),
                &b.title,
                &b.release.guid,
            ))
    });
}

/// Proof that a media's expected episodes are all mapped to files (CP1-F04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageProof {
    pub media: ArrMediaRef,
    pub expected: BTreeSet<i64>,
    pub mapping: BTreeMap<i64, RootedPath>,
    pub specials_included: bool,
}

impl CoverageProof {
    /// Expected episode ids that no file covers.
    #[must_use]
    pub fn missing(&self) -> BTreeSet<i64> {
        self.expected
            .iter()
            .filter(|id| !self.mapping.contains_key(id))
            .copied()
            .collect()
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.expected.is_empty() && self.missing().is_empty()
    }
}

/// `ManualImportRequest` has no mode field: the adapter always sends `importMode = "copy"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualImportRequest {
    pub files: Vec<(RootedPath, ArrMediaRef)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandState {
    Queued,
    Started,
    Completed,
    Failed { message: String },
    Unknown(String),
}

impl CommandState {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self, CommandState::Completed | CommandState::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(
        guid: &str,
        title: &str,
        size: u64,
        score: i64,
        approved: bool,
    ) -> ReleaseSearchResult {
        ReleaseSearchResult {
            release: ReleaseRef {
                guid: guid.into(),
                indexer_id: 1,
            },
            indexer: "i".into(),
            title: title.into(),
            size,
            seeders: None,
            leechers: None,
            age_days: 1,
            protocol: "torrent".into(),
            quality: "q".into(),
            custom_formats: vec![],
            cf_score: score,
            approved,
            rejections: vec![],
            download_allowed: approved,
        }
    }

    #[test]
    fn search_results_sort_by_approval_score_then_gain() {
        let mut r = vec![
            release("1", "rejected-high", 10, 900, false),
            release("2", "approved-low", 50, 10, true),
            release("3", "approved-high-big", 90, 100, true),
            release("4", "approved-high-small", 20, 100, true),
        ];
        sort_search_results(&mut r, 100);
        let order: Vec<_> = r.iter().map(|x| x.release.guid.as_str()).collect();
        assert_eq!(order, ["4", "3", "2", "1"]);
    }

    #[test]
    fn search_sort_is_deterministic_on_ties() {
        let mut a = vec![release("b", "t", 1, 1, true), release("a", "t", 1, 1, true)];
        let mut b = vec![release("a", "t", 1, 1, true), release("b", "t", 1, 1, true)];
        sort_search_results(&mut a, 10);
        sort_search_results(&mut b, 10);
        assert_eq!(a, b);
    }

    fn snapshot(recycle: bool, captured: i64) -> ArrQualitySnapshot {
        ArrQualitySnapshot {
            service: ArrService::Radarr,
            service_version: "5".into(),
            media: MediaIdentity {
                kind: MediaKind::Movie,
                arr: Some(ArrMediaRef {
                    service: ArrService::Radarr,
                    media_id: 7,
                    episode_ids: vec![],
                }),
                external_ids: BTreeMap::new(),
                title: "T".into(),
                year: Some(2020),
                season: None,
                episode: None,
                absolute: None,
            },
            profile_id: 1,
            profile_name: "HD".into(),
            cutoff: "Bluray-1080p".into(),
            upgrade_allowed: true,
            min_format_score: 0,
            cutoff_format_score: 0,
            format_scores: BTreeMap::from([("x".to_string(), 5)]),
            current_files: vec![],
            candidates: vec![],
            media_management: Some(ArrMediaManagement {
                recycle_bin: recycle.then(|| MappedPath::NoRule { raw: "/rb".into() }),
                recycle_bin_cleanup_days: Some(7),
                hardlinks_enabled: true,
                captured_at: Timestamp(captured),
            }),
            captured_at: Timestamp(captured),
            fingerprint: String::new(),
        }
    }

    #[test]
    fn fingerprint_ignores_capture_time_but_not_settings() {
        let a = snapshot(false, 1).seal().unwrap();
        let b = snapshot(false, 999).seal().unwrap();
        let c = snapshot(true, 1).seal().unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.fingerprint, c.fingerprint);
        assert!(a.fingerprint_is_valid());
        let mut tampered = a.clone();
        tampered.cutoff = "other".into();
        assert!(!tampered.fingerprint_is_valid());
    }

    #[test]
    fn coverage_proof_reports_missing_episodes() {
        let media = ArrMediaRef {
            service: ArrService::Sonarr,
            media_id: 1,
            episode_ids: vec![],
        };
        let p = |s: &str| RootedPath::parse("completed", s).unwrap();
        let mut c = CoverageProof {
            media,
            expected: BTreeSet::from([1, 2, 3]),
            mapping: BTreeMap::from([(1, p("a")), (2, p("b"))]),
            specials_included: false,
        };
        assert_eq!(c.missing(), BTreeSet::from([3]));
        assert!(!c.is_complete());
        c.mapping.insert(3, p("c"));
        assert!(c.is_complete());
        c.expected.clear();
        assert!(!c.is_complete(), "an empty expectation proves nothing");
    }

    #[test]
    fn command_state_finished() {
        assert!(CommandState::Completed.is_finished());
        assert!(
            CommandState::Failed {
                message: "x".into()
            }
            .is_finished()
        );
        assert!(!CommandState::Started.is_finished());
        assert!(!CommandState::Unknown("?".into()).is_finished());
    }
}
