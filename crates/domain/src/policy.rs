//! Pure policies: sample protection, risk tiers and capability flags.

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// OQ-01 sample rule. Case-insensitive. True if the file name contains the token `sample` or
/// `samples` (tokens split on `.`, `-`, `_`, space, `[`, `]`, `(`, `)`), **or** any ancestor
/// directory component equals `sample` or `samples`. Fixed, not configurable: a false positive
/// only protects a file.
#[must_use]
pub fn is_sample(rel: &RelPath) -> bool {
    let is_token = |s: &str| s.eq_ignore_ascii_case("sample") || s.eq_ignore_ascii_case("samples");
    let comps: Vec<&str> = rel.components().collect();
    let Some((name, ancestors)) = comps.split_last() else {
        return false;
    };
    if ancestors.iter().any(|d| is_token(d)) {
        return true;
    }
    name.split(['.', '-', '_', ' ', '[', ']', '(', ')'])
        .any(is_token)
}

/// Risk tiers (MASTER section 10). Ordered by increasing risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RiskTier {
    T1,
    T2,
    T3,
    T4,
    T5,
}

/// Capability flags (`[capabilities]`, architecture section 13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Consolidate,
    DeleteCompleted,
    CleanupIncomplete,
    FixPermissions,
    ArrManualImport,
    ArrQueueRemove,
    ArrSetMonitored,
    ArrGrab,
    /// Policy flag for auto plans only; no action kind maps to it.
    AutoRemoveEmptyTorrents,
}

impl Capability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Consolidate => "consolidate",
            Capability::DeleteCompleted => "delete_completed",
            Capability::CleanupIncomplete => "cleanup_incomplete",
            Capability::FixPermissions => "fix_permissions",
            Capability::ArrManualImport => "arr_manual_import",
            Capability::ArrQueueRemove => "arr_queue_remove",
            Capability::ArrSetMonitored => "arr_set_monitored",
            Capability::ArrGrab => "arr_grab",
            Capability::AutoRemoveEmptyTorrents => "auto_remove_empty_torrents",
        }
    }

    /// Documented default (section 13.2). Configuration may override it.
    #[must_use]
    pub fn default_enabled(self) -> bool {
        matches!(
            self,
            Capability::Consolidate
                | Capability::DeleteCompleted
                | Capability::CleanupIncomplete
                | Capability::FixPermissions
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &str) -> bool {
        is_sample(&RelPath::new(p).unwrap())
    }

    #[test]
    fn fx07_sample_subfolder() {
        assert!(s(
            "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"
        ));
        assert!(s("Rel/Sample/anything.mkv"));
        assert!(s("Rel/SAMPLES/x/y.mkv"));
    }

    #[test]
    fn fx27_file_level_sample() {
        assert!(s(
            "Kilo.Movie.2012.1080p.BluRay.x264-GRP/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"
        ));
        assert!(s("a/movie_sample.mkv"));
        assert!(s("a/movie [sample].mkv"));
        assert!(s("a/Movie (Sample).mkv"));
        assert!(s("a/SAMPLE.mkv"));
        assert!(s("sample.mkv"));
    }

    #[test]
    fn negatives() {
        assert!(!s("Alpha.Movie.2020/Alpha.Movie.2020.mkv"));
        assert!(!s("a/Sampler.2020.mkv"));
        assert!(!s("a/example.mkv"));
        assert!(!s("a/resample.mkv"));
        assert!(
            !s("a/Samplesize/movie.mkv"),
            "only exact directory names match"
        );
        assert!(!s("Tango.Movie.2007.1080p.WEB.x264-GRP.mkv"));
        assert!(!is_sample(&RelPath::root()));
    }

    #[test]
    fn the_sample_folder_itself_is_a_sample() {
        assert!(s("Rel/Sample"));
        assert!(s("Rel/Sample.mkv"));
    }

    #[test]
    fn tiers_order() {
        assert!(RiskTier::T1 < RiskTier::T3 && RiskTier::T4 < RiskTier::T5);
    }

    #[test]
    fn capability_defaults_match_the_table() {
        for c in [
            Capability::ArrManualImport,
            Capability::ArrQueueRemove,
            Capability::ArrSetMonitored,
            Capability::ArrGrab,
            Capability::AutoRemoveEmptyTorrents,
        ] {
            assert!(!c.default_enabled(), "{}", c.as_str());
        }
        assert!(Capability::Consolidate.default_enabled());
        assert_eq!(
            serde_json::to_string(&Capability::ArrGrab).unwrap(),
            "\"arr_grab\""
        );
    }
}
