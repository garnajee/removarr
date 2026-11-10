//! Episode numbering as explanatory evidence (INV-36; owner of the rule: plan-engine).
//!
//! ARR's assignment of episode ids to a file (manual-import preview `episode_ids`) is the
//! authoritative mapping and becomes the plan's `CoverageProof`. This module explains it: it
//! interprets the numbers written in the release name under each explicit numbering scheme
//! (aired `SxxEyy`, scene `SxxEyy`, absolute, scene absolute, and "episode number used as
//! absolute" for packs that continue numbering across seasons) and says which scheme agrees
//! with ARR. A disagreement is reported, never resolved: plan-engine turns it into
//! `NumberingMismatch` (manual check).

use std::collections::BTreeSet;

use removarr_domain::arr::ArrEpisode;
use serde::{Deserialize, Serialize};

use crate::parse::ParsedName;

/// An explicit numbering scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberingScheme {
    /// `SxxEyy` in ARR's aired (TVDB) order, including specials (season 0).
    Aired,
    /// `SxxEyy` in ARR's scene numbering (`scene_season`/`scene_episode`).
    Scene,
    /// Absolute number (`Title - 25`) against ARR's `absolute`.
    Absolute,
    /// Absolute number against ARR's `scene_absolute`.
    SceneAbsolute,
    /// `SxxEyy` whose episode numbers are absolute numbers (e.g. `S02E25` for absolute 25).
    EpisodeAsAbsolute,
}

/// The ARR episode ids one scheme maps the parsed numbers to. Complete only when every parsed
/// number maps to exactly one episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interpretation {
    pub scheme: NumberingScheme,
    pub episode_ids: BTreeSet<i64>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberingAgreement {
    /// ARR's assignment equals the complete interpretation of these schemes.
    Agrees { schemes: Vec<NumberingScheme> },
    /// No complete interpretation equals ARR's assignment.
    Disagrees,
    /// The name carries no episode numbers: nothing to compare (not a mismatch by itself).
    NotParsed,
}

/// Explanation of a file's numbering against ARR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberingEvidence {
    pub interpretations: Vec<Interpretation>,
    pub arr_assigned: BTreeSet<i64>,
    pub agreement: NumberingAgreement,
}

impl NumberingEvidence {
    /// True when plan-engine must add `NumberingMismatch`.
    #[must_use]
    pub fn disagrees(&self) -> bool {
        self.agreement == NumberingAgreement::Disagrees
    }
}

fn interpret(
    scheme: NumberingScheme,
    numbers: &[(Option<i32>, i32)],
    episodes: &[ArrEpisode],
) -> Interpretation {
    let mut ids = BTreeSet::new();
    let mut complete = !numbers.is_empty();
    for &(season, n) in numbers {
        let hits: Vec<i64> = episodes
            .iter()
            .filter(|e| match scheme {
                NumberingScheme::Aired => Some(e.season) == season && e.episode == n,
                NumberingScheme::Scene => {
                    e.scene_season.is_some()
                        && e.scene_season == season
                        && e.scene_episode == Some(n)
                }
                NumberingScheme::Absolute | NumberingScheme::EpisodeAsAbsolute => {
                    e.absolute == Some(n)
                }
                NumberingScheme::SceneAbsolute => e.scene_absolute == Some(n),
            })
            .map(|e| e.id)
            .collect();
        if hits.len() == 1 {
            ids.extend(hits);
        } else {
            complete = false;
        }
    }
    Interpretation {
        scheme,
        episode_ids: ids,
        complete,
    }
}

/// Explains `parsed` against the series' ARR episodes and ARR's assignment for this file.
#[must_use]
pub fn explain_numbering(
    parsed: &ParsedName,
    episodes: &[ArrEpisode],
    arr_assigned: &BTreeSet<i64>,
) -> NumberingEvidence {
    let mut interpretations = Vec::new();
    if let Some(ep) = &parsed.episode {
        let season = Some(i32::from(ep.season));
        let pairs: Vec<(Option<i32>, i32)> = ep
            .episodes
            .iter()
            .map(|e| (season, i32::from(*e)))
            .collect();
        let abs: Vec<(Option<i32>, i32)> =
            ep.episodes.iter().map(|e| (None, i32::from(*e))).collect();
        interpretations.push(interpret(NumberingScheme::Aired, &pairs, episodes));
        interpretations.push(interpret(NumberingScheme::Scene, &pairs, episodes));
        interpretations.push(interpret(
            NumberingScheme::EpisodeAsAbsolute,
            &abs,
            episodes,
        ));
    }
    if let Some(a) = parsed.absolute.and_then(|a| i32::try_from(a).ok()) {
        let n = [(None, a)];
        interpretations.push(interpret(NumberingScheme::Absolute, &n, episodes));
        interpretations.push(interpret(NumberingScheme::SceneAbsolute, &n, episodes));
    }
    let agreement = if interpretations.is_empty() {
        NumberingAgreement::NotParsed
    } else {
        let schemes: Vec<NumberingScheme> = interpretations
            .iter()
            .filter(|i| i.complete && &i.episode_ids == arr_assigned)
            .map(|i| i.scheme)
            .collect();
        if schemes.is_empty() {
            NumberingAgreement::Disagrees
        } else {
            NumberingAgreement::Agrees { schemes }
        }
    };
    NumberingEvidence {
        interpretations,
        arr_assigned: arr_assigned.clone(),
        agreement,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_scene_name;

    fn ep(id: i64, season: i32, episode: i32, absolute: Option<i32>) -> ArrEpisode {
        ArrEpisode {
            id,
            series_id: 1,
            season,
            episode,
            absolute,
            scene_absolute: None,
            scene_season: None,
            scene_episode: None,
            has_file: false,
            file_id: None,
            monitored: true,
            aired: true,
        }
    }

    fn ids(v: &[i64]) -> BTreeSet<i64> {
        v.iter().copied().collect()
    }

    /// ONE-01 / CONTEXTE §4.11 (Jujutsu Kaisen): a pack named `S02E25` for TVDB `S02E01`
    /// (absolute 25) agrees with ARR only through the explicit absolute interpretation.
    #[test]
    fn continued_season_numbering_maps_through_absolute() {
        let episodes: Vec<ArrEpisode> = (1..=24)
            .map(|e| ep(i64::from(e), 1, e, Some(e)))
            .chain((1..=23).map(|e| ep(100 + i64::from(e), 2, e, Some(24 + e))))
            .collect();
        let parsed = parse_scene_name("Jujutsu.Kaisen.S02E25.1080p.WEB.mkv");
        let ev = explain_numbering(&parsed, &episodes, &ids(&[101]));
        assert_eq!(
            ev.agreement,
            NumberingAgreement::Agrees {
                schemes: vec![NumberingScheme::EpisodeAsAbsolute]
            }
        );
        let aired = &ev.interpretations[0];
        assert_eq!(aired.scheme, NumberingScheme::Aired);
        assert!(!aired.complete, "S02E25 does not exist in aired order");
        // ARR assigning another episode is a disagreement, reported not resolved.
        assert!(explain_numbering(&parsed, &episodes, &ids(&[102])).disagrees());
    }

    #[test]
    fn aired_scene_absolute_specials_and_multi_episode() {
        let mut e3 = ep(3, 1, 3, Some(3));
        e3.scene_season = Some(2);
        e3.scene_episode = Some(1);
        e3.scene_absolute = Some(4);
        let episodes = vec![
            ep(1, 1, 1, Some(1)),
            ep(2, 1, 2, Some(2)),
            e3,
            ep(9, 0, 1, None),
        ];

        let multi = parse_scene_name("Show.S01E01E02.mkv");
        let ev = explain_numbering(&multi, &episodes, &ids(&[1, 2]));
        assert!(
            matches!(&ev.agreement, NumberingAgreement::Agrees { schemes } if schemes.contains(&NumberingScheme::Aired))
        );

        let scene = parse_scene_name("Show.S02E01.mkv");
        let ev = explain_numbering(&scene, &episodes, &ids(&[3]));
        assert_eq!(
            ev.agreement,
            NumberingAgreement::Agrees {
                schemes: vec![NumberingScheme::Scene]
            }
        );

        let abs = parse_scene_name("[Grp] Show - 04 [1080p].mkv");
        let ev = explain_numbering(&abs, &episodes, &ids(&[3]));
        assert_eq!(
            ev.agreement,
            NumberingAgreement::Agrees {
                schemes: vec![NumberingScheme::SceneAbsolute]
            }
        );

        let special = parse_scene_name("Show.S00E01.Christmas.mkv");
        assert!(!explain_numbering(&special, &episodes, &ids(&[9])).disagrees());

        let none = parse_scene_name("Show.Movie.2020.mkv");
        assert_eq!(
            explain_numbering(&none, &episodes, &ids(&[1])).agreement,
            NumberingAgreement::NotParsed
        );
    }
}
