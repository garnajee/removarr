//! Pure correlation of Jellyfin items with the canonical identity (from ARR) and the
//! reporting of recognition mismatches, specials and alternate numbering.
//!
//! Jellyfin exposes library state. It never decides release quality, and a mismatch here is a
//! finding to report, never something this crate repairs. Provider ids are compared only for
//! films: an episode item carries its own episode ids while ARR identities carry the series id,
//! so comparing them would produce false mismatches.

use std::collections::BTreeMap;

use removarr_domain::arr::{MediaIdentity, MediaKind};
use removarr_domain::error::PortError;
use removarr_domain::evidence::Completeness;
use serde::{Deserialize, Serialize};

use crate::reader::JellyfinItemDetail;

/// What the canonical sources say a file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedMedia {
    pub identity: MediaIdentity,
    /// Other (season, episode) numberings known for the same episode (scene or alternate order).
    pub alternate_numbering: Vec<(i32, i32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionStatus {
    /// One item, matching identity.
    Recognized,
    /// Jellyfin has no item at the path.
    NotFound,
    /// Jellyfin lists it, but the identity cannot be confirmed (duplicates, alternate or
    /// absolute numbering, title differences only).
    Ambiguous,
    /// Jellyfin lists it as something else (wrong numbering, kind, unrecognized video).
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finding {
    NotInLibrary,
    DuplicateItems {
        count: usize,
    },
    /// Jellyfin type `Video`: the file is indexed but not recognized as a film or episode.
    UnrecognizedVideo,
    KindMismatch {
        expected: String,
        actual: String,
    },
    MissingNumbering,
    NumberingMismatch {
        expected: (i32, i32),
        actual: (i32, i32),
    },
    /// Jellyfin uses a numbering listed as alternate for the same episode.
    AlternateNumbering {
        actual: (i32, i32),
    },
    /// Jellyfin's episode index equals the absolute number, not the season numbering.
    AbsoluteNumbering {
        actual: i32,
    },
    /// Expected a special (season 0), Jellyfin has it in a regular season.
    SpecialInRegularSeason {
        season: i32,
    },
    /// Expected a regular episode, Jellyfin has it in the specials season.
    RegularInSpecials,
    TitleMismatch,
    ProviderIdConflict {
        provider: String,
    },
    /// Informational: the file covers several episodes and includes the expected one.
    MultiEpisodeFile {
        start: i32,
        end: i32,
    },
}

impl Finding {
    fn status(&self) -> Option<RecognitionStatus> {
        match self {
            Finding::NotInLibrary => Some(RecognitionStatus::NotFound),
            Finding::MultiEpisodeFile { .. } => None,
            Finding::DuplicateItems { .. }
            | Finding::AlternateNumbering { .. }
            | Finding::AbsoluteNumbering { .. }
            | Finding::TitleMismatch => Some(RecognitionStatus::Ambiguous),
            _ => Some(RecognitionStatus::Mismatch),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecognitionReport {
    pub status: RecognitionStatus,
    pub findings: Vec<Finding>,
}

fn rank(s: &RecognitionStatus) -> u8 {
    match s {
        RecognitionStatus::Recognized => 0,
        RecognitionStatus::Ambiguous => 1,
        RecognitionStatus::Mismatch => 2,
        RecognitionStatus::NotFound => 3,
    }
}

/// Lower-case alphanumerics only, trailing `(year)` removed.
fn norm_title(s: &str) -> String {
    let mut t = s.trim();
    if let Some(i) = t.rfind('(') {
        let tail = &t[i..];
        if tail.len() == 6 && tail.ends_with(')') && tail[1..5].chars().all(|c| c.is_ascii_digit())
        {
            t = t[..i].trim_end();
        }
    }
    t.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn lower_ids(m: &BTreeMap<String, String>) -> BTreeMap<String, &str> {
    m.iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str()))
        .collect()
}

/// Compares what Jellyfin lists at one path with the expected identity.
#[must_use]
pub fn evaluate(expected: &ExpectedMedia, found: &[JellyfinItemDetail]) -> RecognitionReport {
    let mut findings = Vec::new();
    match found {
        [] => findings.push(Finding::NotInLibrary),
        [one] => check_item(expected, one, &mut findings),
        many => findings.push(Finding::DuplicateItems { count: many.len() }),
    }
    let status = findings
        .iter()
        .filter_map(Finding::status)
        .max_by_key(rank)
        .unwrap_or(RecognitionStatus::Recognized);
    RecognitionReport { status, findings }
}

fn check_item(exp: &ExpectedMedia, d: &JellyfinItemDetail, out: &mut Vec<Finding>) {
    let id = &exp.identity;
    let item = &d.item;
    if item.kind == "Video" {
        out.push(Finding::UnrecognizedVideo);
        return;
    }
    let want_kind = if id.kind == MediaKind::Movie {
        "Movie"
    } else {
        "Episode"
    };
    if item.kind != want_kind {
        out.push(Finding::KindMismatch {
            expected: want_kind.to_string(),
            actual: item.kind.clone(),
        });
        return;
    }
    if id.kind == MediaKind::Movie {
        let theirs = lower_ids(&item.provider_ids);
        for (k, v) in lower_ids(&id.external_ids) {
            if theirs.get(&k).is_some_and(|t| *t != v) {
                out.push(Finding::ProviderIdConflict { provider: k });
            }
        }
        if !id.title.is_empty()
            && !item.name.is_empty()
            && norm_title(&id.title) != norm_title(&item.name)
        {
            out.push(Finding::TitleMismatch);
        }
        return;
    }
    if let Some(series) = &item.series
        && !id.title.is_empty()
        && norm_title(&id.title) != norm_title(series)
    {
        out.push(Finding::TitleMismatch);
    }
    let (Some(season), Some(ep)) = (item.season, item.episode) else {
        // An episode without season or index is not identified, whatever its name says.
        out.push(Finding::MissingNumbering);
        return;
    };
    let (Some(want_s), Some(want_e)) = (id.season, id.episode) else {
        out.push(Finding::MissingNumbering);
        return;
    };
    let actual = (season, ep);
    let end = d.episode_end.filter(|e| *e > ep);
    let in_range = season == want_s && ep <= want_e && want_e <= end.unwrap_or(ep);
    if in_range {
        if let Some(end) = end {
            out.push(Finding::MultiEpisodeFile { start: ep, end });
        }
        return;
    }
    if id.kind == MediaKind::Special && season != 0 {
        out.push(Finding::SpecialInRegularSeason { season });
    } else if id.kind == MediaKind::Episode && season == 0 && want_s != 0 {
        out.push(Finding::RegularInSpecials);
    } else if exp.alternate_numbering.contains(&actual) {
        out.push(Finding::AlternateNumbering { actual });
    } else if id.absolute == Some(ep) && season <= 1 {
        out.push(Finding::AbsoluteNumbering { actual: ep });
    } else {
        out.push(Finding::NumberingMismatch {
            expected: (want_s, want_e),
            actual,
        });
    }
}

/// API failures make dependent verification incomplete, never successful.
#[must_use]
pub fn completeness_of<T>(result: &Result<T, PortError>) -> Completeness {
    match result {
        Ok(_) => Completeness::Complete,
        Err(e) => Completeness::Incomplete {
            missing: vec![format!("jellyfin: {:?}", e.class)],
        },
    }
}
