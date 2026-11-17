//! Local checks between the current library file and a different release proposed by ARR.
//!
//! These checks can only **block** a replacement (INV-12, INV-37). They never decide that a
//! release is better: ARR is the sole quality authority (INV-11), so resolution, codec, source
//! and group are reported as [`TechnicalFacts`] and are never compared or ranked here
//! (CON-02 supersedes the legacy `resolution_jellyfin_inferieure` rule; REJ-09, REJ-24).

use std::collections::BTreeSet;

use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::media::ProbeReport;
use serde::{Deserialize, Serialize};

use crate::language::{LangTag, languages_are_compatible, normalize_tag};
use crate::parse::{EpisodeNumbers, ParsedName};
use crate::reject::Side;

/// Duration tolerance: `max(seconds, max(d1, d2) * ratio)` (DR-16; defaults 5 s, 0.2 %).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DurationTolerance {
    pub seconds: f64,
    pub ratio: f64,
}

impl Default for DurationTolerance {
    fn default() -> Self {
        Self {
            seconds: 5.0,
            ratio: 0.002,
        }
    }
}

/// What is known about one file: its parsed name and, when probed, its ffprobe report.
#[derive(Debug, Clone, Copy)]
pub struct MediaFacts<'a> {
    pub parsed: &'a ParsedName,
    pub probe: Option<&'a ProbeReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Title,
    Numbering,
    Edition,
    Duration,
    AudioLanguage,
}

/// Why a check blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckFailure {
    TitleDiffers {
        current: String,
        candidate: String,
    },
    YearMissing,
    YearDiffers {
        current: u16,
        candidate: u16,
    },
    /// Numbering differs or is written in different schemes (absolute vs season/episode).
    EpisodesDiffer {
        current: Option<EpisodeNumbers>,
        candidate: Option<EpisodeNumbers>,
        current_absolute: Option<u32>,
        candidate_absolute: Option<u32>,
    },
    /// The candidate covers fewer episodes of the same season than the current file.
    CoverageReduced {
        season: u16,
        missing: Vec<u16>,
    },
    EditionDiffers {
        current: BTreeSet<String>,
        candidate: BTreeSet<String>,
    },
    DurationDiffers {
        current_s: f64,
        candidate_s: f64,
        difference_s: f64,
        tolerance_s: f64,
    },
    /// No usable duration (probe failed, not run, or no duration reported).
    DurationUnverified {
        side: Side,
    },
    /// A real audio language of the current file is absent from the candidate.
    AudioLanguageMissing {
        language: String,
    },
    /// Real audio cannot be compared (`und` tracks, or no probe).
    AudioLanguageUnverified {
        reason: String,
    },
    /// Without probes, the release-name languages are incompatible (DR-15).
    ReleaseLanguageIncompatible,
}

impl CheckFailure {
    #[must_use]
    pub fn blocker_code(&self) -> BlockerCode {
        match self {
            CheckFailure::TitleDiffers { .. }
            | CheckFailure::YearMissing
            | CheckFailure::YearDiffers { .. } => BlockerCode::ArrAmbiguous,
            CheckFailure::EpisodesDiffer { .. } => BlockerCode::NumberingMismatch,
            CheckFailure::CoverageReduced { .. } => BlockerCode::CoverageIncomplete,
            CheckFailure::EditionDiffers { .. } => BlockerCode::EditionMismatch,
            CheckFailure::DurationDiffers { .. } | CheckFailure::DurationUnverified { .. } => {
                BlockerCode::DurationMismatch
            }
            CheckFailure::AudioLanguageMissing { .. }
            | CheckFailure::AudioLanguageUnverified { .. }
            | CheckFailure::ReleaseLanguageIncompatible => BlockerCode::AudioLanguageMissing,
        }
    }

    /// Human-readable English detail (no paths).
    #[must_use]
    pub fn detail(&self) -> String {
        let set = |s: &BTreeSet<String>| {
            if s.is_empty() {
                "none".to_string()
            } else {
                s.iter().cloned().collect::<Vec<_>>().join(",")
            }
        };
        match self {
            CheckFailure::TitleDiffers { current, candidate } => {
                format!("parsed titles differ: '{current}' vs '{candidate}'")
            }
            CheckFailure::YearMissing => "year missing on one side".into(),
            CheckFailure::YearDiffers { current, candidate } => {
                format!("years differ: {current} vs {candidate}")
            }
            CheckFailure::EpisodesDiffer { .. } => "parsed episode numbering differs".into(),
            CheckFailure::CoverageReduced { season, missing } => {
                format!("candidate lacks season {season} episodes {missing:?}")
            }
            CheckFailure::EditionDiffers { current, candidate } => {
                format!("editions differ: {} vs {}", set(current), set(candidate))
            }
            CheckFailure::DurationDiffers {
                difference_s,
                tolerance_s,
                ..
            } => format!("duration differs by {difference_s:.3} s (tolerance {tolerance_s:.3} s)"),
            CheckFailure::DurationUnverified { side } => format!(
                "duration not verified for the {} file",
                match side {
                    Side::A => "current",
                    Side::B => "candidate",
                }
            ),
            CheckFailure::AudioLanguageMissing { language } => {
                format!("audio language '{language}' of the current file is missing")
            }
            CheckFailure::AudioLanguageUnverified { reason } => {
                format!("audio language not verified: {reason}")
            }
            CheckFailure::ReleaseLanguageIncompatible => {
                "release-name languages are incompatible".into()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Block(CheckFailure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub kind: CheckKind,
    pub outcome: CheckOutcome,
}

/// Technical description of one file. Explanatory only: never compared, never ranked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TechnicalFacts {
    pub resolution: Option<String>,
    pub codec: Option<String>,
    pub source: Option<String>,
    pub group: Option<String>,
    pub languages: BTreeSet<String>,
    pub video_codec: Option<String>,
    pub video_height: Option<u32>,
    pub audio_languages: Vec<String>,
}

impl TechnicalFacts {
    #[must_use]
    pub fn of(facts: MediaFacts<'_>) -> Self {
        let p = facts.parsed;
        Self {
            resolution: p.resolution.clone(),
            codec: p.codec.clone(),
            source: p.source.clone(),
            group: p.group.clone(),
            languages: p.languages.clone(),
            video_codec: facts
                .probe
                .and_then(|r| r.video.first().map(|v| v.codec.clone())),
            video_height: facts
                .probe
                .and_then(|r| r.video.first().and_then(|v| v.height)),
            audio_languages: facts
                .probe
                .map(|r| {
                    r.audio
                        .iter()
                        .map(|a| a.language.clone().unwrap_or_else(|| "und".into()))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// All local checks for one current/candidate pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplacementAssessment {
    pub checks: Vec<CheckResult>,
    pub current: TechnicalFacts,
    pub candidate: TechnicalFacts,
}

impl ReplacementAssessment {
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckFailure> {
        self.checks
            .iter()
            .filter_map(|c| match &c.outcome {
                CheckOutcome::Block(f) => Some(f),
                CheckOutcome::Pass => None,
            })
            .collect()
    }

    #[must_use]
    pub fn is_blocked(&self) -> bool {
        !self.failures().is_empty()
    }

    #[must_use]
    pub fn blockers(&self) -> Vec<Blocker> {
        self.failures()
            .into_iter()
            .map(|f| Blocker::new(f.blocker_code(), f.detail()))
            .collect()
    }
}

fn block(kind: CheckKind, f: CheckFailure) -> CheckResult {
    CheckResult {
        kind,
        outcome: CheckOutcome::Block(f),
    }
}

fn pass(kind: CheckKind) -> CheckResult {
    CheckResult {
        kind,
        outcome: CheckOutcome::Pass,
    }
}

/// Runs every local check. The result can block; passing all checks authorizes nothing by
/// itself (ARR approval, coverage proof and snapshots remain required, plan-engine).
#[must_use]
pub fn assess_replacement(
    current: MediaFacts<'_>,
    candidate: MediaFacts<'_>,
    tolerance: DurationTolerance,
) -> ReplacementAssessment {
    let checks = vec![
        title_check(current.parsed, candidate.parsed),
        numbering_check(current.parsed, candidate.parsed),
        edition_check(current.parsed, candidate.parsed),
        duration_check(current.probe, candidate.probe, tolerance),
        audio_language_check(current, candidate),
    ];
    ReplacementAssessment {
        checks,
        current: TechnicalFacts::of(current),
        candidate: TechnicalFacts::of(candidate),
    }
}

/// T-07: the normalized title must be exactly equal (and non-empty).
#[must_use]
pub fn title_check(cur: &ParsedName, cand: &ParsedName) -> CheckResult {
    if !cur.title_tokens.is_empty() && cur.title_tokens == cand.title_tokens {
        pass(CheckKind::Title)
    } else {
        block(
            CheckKind::Title,
            CheckFailure::TitleDiffers {
                current: cur.title(),
                candidate: cand.title(),
            },
        )
    }
}

/// Episodes for series (coverage of the current file must be kept), year for movies.
#[must_use]
pub fn numbering_check(cur: &ParsedName, cand: &ParsedName) -> CheckResult {
    let differ = || {
        block(
            CheckKind::Numbering,
            CheckFailure::EpisodesDiffer {
                current: cur.episode.clone(),
                candidate: cand.episode.clone(),
                current_absolute: cur.absolute,
                candidate_absolute: cand.absolute,
            },
        )
    };
    if let Some(ce) = &cur.episode {
        let Some(ne) = &cand.episode else {
            return differ();
        };
        if ce.season != ne.season {
            return differ();
        }
        let missing: Vec<u16> = ce
            .episodes
            .iter()
            .copied()
            .filter(|e| !ne.episodes.contains(e))
            .collect();
        if missing.is_empty() {
            return pass(CheckKind::Numbering);
        }
        if missing.len() < ce.episodes.len() {
            return block(
                CheckKind::Numbering,
                CheckFailure::CoverageReduced {
                    season: ce.season,
                    missing,
                },
            );
        }
        return differ();
    }
    if let Some(a) = cur.absolute {
        return if cand.absolute == Some(a) {
            pass(CheckKind::Numbering)
        } else {
            differ()
        };
    }
    if cand.episode.is_some() || cand.absolute.is_some() {
        return differ();
    }
    match (cur.year, cand.year) {
        (Some(a), Some(b)) if a == b => pass(CheckKind::Numbering),
        (Some(a), Some(b)) => block(
            CheckKind::Numbering,
            CheckFailure::YearDiffers {
                current: a,
                candidate: b,
            },
        ),
        _ => block(CheckKind::Numbering, CheckFailure::YearMissing),
    }
}

/// T-06: different (or one-sided) editions block.
#[must_use]
pub fn edition_check(cur: &ParsedName, cand: &ParsedName) -> CheckResult {
    if cur.editions == cand.editions {
        pass(CheckKind::Edition)
    } else {
        block(
            CheckKind::Edition,
            CheckFailure::EditionDiffers {
                current: cur.editions.clone(),
                candidate: cand.editions.clone(),
            },
        )
    }
}

fn usable(d: Option<f64>) -> Option<f64> {
    d.filter(|d| d.is_finite() && *d > 0.0)
}

/// T-09 / DR-16: a duration difference beyond tolerance blocks; an unknown duration blocks too.
#[must_use]
pub fn duration_check(
    cur: Option<&ProbeReport>,
    cand: Option<&ProbeReport>,
    tol: DurationTolerance,
) -> CheckResult {
    let Some(d1) = usable(cur.and_then(|p| p.duration_s)) else {
        return block(
            CheckKind::Duration,
            CheckFailure::DurationUnverified { side: Side::A },
        );
    };
    let Some(d2) = usable(cand.and_then(|p| p.duration_s)) else {
        return block(
            CheckKind::Duration,
            CheckFailure::DurationUnverified { side: Side::B },
        );
    };
    let difference_s = (d1 - d2).abs();
    let tolerance_s = tol.seconds.max(d1.max(d2) * tol.ratio);
    if difference_s <= tolerance_s {
        pass(CheckKind::Duration)
    } else {
        block(
            CheckKind::Duration,
            CheckFailure::DurationDiffers {
                current_s: d1,
                candidate_s: d2,
                difference_s,
                tolerance_s,
            },
        )
    }
}

fn audio_languages(p: &ProbeReport) -> (BTreeSet<String>, bool) {
    let mut known = BTreeSet::new();
    let mut undetermined = false;
    for a in &p.audio {
        match normalize_tag(a.language.as_deref()) {
            LangTag::Known(l) => {
                known.insert(l);
            }
            LangTag::Undetermined => undetermined = true,
            LangTag::NoLinguisticContent => {}
        }
    }
    (known, undetermined)
}

/// INV-37: every real audio language of the current file must be present in the candidate.
/// `und` tracks on either side make the answer unknown, which blocks (manual check). Without
/// probes the release-name rule (DR-15) is evaluated, and the result still blocks as unverified.
#[must_use]
pub fn audio_language_check(cur: MediaFacts<'_>, cand: MediaFacts<'_>) -> CheckResult {
    let kind = CheckKind::AudioLanguage;
    let (Some(cp), Some(np)) = (cur.probe, cand.probe) else {
        return if languages_are_compatible(cur.parsed, cand.parsed) {
            block(
                kind,
                CheckFailure::AudioLanguageUnverified {
                    reason: "real audio not probed (release names compatible)".into(),
                },
            )
        } else {
            block(kind, CheckFailure::ReleaseLanguageIncompatible)
        };
    };
    if cp.audio.is_empty() {
        return pass(kind);
    }
    let (required, cur_und) = audio_languages(cp);
    let (available, cand_und) = audio_languages(np);
    if np.audio.is_empty() {
        return block(
            kind,
            CheckFailure::AudioLanguageMissing {
                language: required
                    .iter()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "und".into()),
            },
        );
    }
    let missing: Vec<&String> = required
        .iter()
        .filter(|l| !available.contains(*l))
        .collect();
    if let Some(l) = missing.first() {
        return if cand_und {
            block(
                kind,
                CheckFailure::AudioLanguageUnverified {
                    reason: format!("candidate has undetermined tracks; '{l}' not confirmed"),
                },
            )
        } else {
            block(
                kind,
                CheckFailure::AudioLanguageMissing {
                    language: (*l).clone(),
                },
            )
        };
    }
    if cur_und {
        return block(
            kind,
            CheckFailure::AudioLanguageUnverified {
                reason: "current file has undetermined audio tracks".into(),
            },
        );
    }
    pass(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_scene_name;
    use removarr_domain::media::{AudioStream, VideoStream};

    fn probe(duration: Option<f64>, langs: &[Option<&str>]) -> ProbeReport {
        ProbeReport {
            duration_s: duration,
            video: vec![VideoStream {
                codec: "hevc".into(),
                width: Some(1920),
                height: Some(1080),
            }],
            audio: langs
                .iter()
                .map(|l| AudioStream {
                    codec: "aac".into(),
                    language: l.map(str::to_string),
                    channels: Some(2),
                })
                .collect(),
            subtitles: vec![],
        }
    }

    fn assess(
        a: &str,
        b: &str,
        pa: Option<&ProbeReport>,
        pb: Option<&ProbeReport>,
    ) -> ReplacementAssessment {
        let (x, y) = (parse_scene_name(a), parse_scene_name(b));
        assess_replacement(
            MediaFacts {
                parsed: &x,
                probe: pa,
            },
            MediaFacts {
                parsed: &y,
                probe: pb,
            },
            DurationTolerance::default(),
        )
    }

    fn codes(r: &ReplacementAssessment) -> Vec<BlockerCode> {
        r.blockers().into_iter().map(|b| b.code).collect()
    }

    /// T-06 `test_auto_candidate_refuses_different_editions`.
    #[test]
    fn t06_different_editions_block() {
        let p = probe(Some(7200.0), &[Some("fre")]);
        let r = assess(
            "The.Movie.2024.EXTENDED.MULTi.1080p.mkv",
            "The.Movie.2024.MULTi.1080p.mkv",
            Some(&p),
            Some(&p),
        );
        assert_eq!(codes(&r), vec![BlockerCode::EditionMismatch]);
    }

    /// T-07 `test_auto_candidate_requires_exact_normalized_title`.
    #[test]
    fn t07_title_must_be_exactly_equal() {
        let p = probe(Some(7200.0), &[Some("fre")]);
        let r = assess(
            "Movie.Name.2024.MULTi.1080p.mkv",
            "Movie.Name.Returns.2024.MULTi.1080p.mkv",
            Some(&p),
            Some(&p),
        );
        assert!(matches!(
            r.failures()[..],
            [CheckFailure::TitleDiffers { .. }]
        ));
        assert_eq!(codes(&r), vec![BlockerCode::ArrAmbiguous]);
    }

    /// T-09 `test_duration_verification_blocks_a_different_cut`.
    #[test]
    fn t09_duration_mismatch_or_probe_failure_blocks() {
        let tol = DurationTolerance::default();
        let long = probe(Some(7200.0), &[]);
        let cut = probe(Some(6900.0), &[]);
        let r = duration_check(Some(&long), Some(&cut), tol);
        let CheckOutcome::Block(CheckFailure::DurationDiffers {
            difference_s,
            tolerance_s,
            ..
        }) = r.outcome
        else {
            panic!("expected a duration mismatch, got {r:?}");
        };
        assert!((difference_s - 300.0).abs() < 1e-9);
        assert!((tolerance_s - 14.4).abs() < 1e-9, "max(5 s, 7200 * 0.2 %)");
        // Within tolerance.
        let near = probe(Some(7210.0), &[]);
        assert_eq!(
            duration_check(Some(&long), Some(&near), tol).outcome,
            CheckOutcome::Pass
        );
        let short = probe(Some(100.0), &[]);
        let short2 = probe(Some(104.9), &[]);
        assert_eq!(
            duration_check(Some(&short), Some(&short2), tol).outcome,
            CheckOutcome::Pass
        );
        // Probe error (no report), missing, zero or non-finite duration: blocked as unverified.
        for bad in [
            None,
            Some(probe(None, &[])),
            Some(probe(Some(0.0), &[])),
            Some(probe(Some(f64::NAN), &[])),
        ] {
            let r = duration_check(Some(&long), bad.as_ref(), tol);
            assert_eq!(
                r.outcome,
                CheckOutcome::Block(CheckFailure::DurationUnverified { side: Side::B })
            );
        }
        assert_eq!(
            CheckFailure::DurationUnverified { side: Side::A }.blocker_code(),
            BlockerCode::DurationMismatch
        );
    }

    /// T-19 replacement (superseded by CON-02): resolution and codec never block or promote;
    /// identity, edition, language and duration remain local blockers.
    #[test]
    fn t19_technical_tags_are_explanatory_only() {
        let fr = probe(Some(7200.0), &[Some("fre"), Some("eng")]);
        let up = assess(
            "Movie.Name.2024.MULTi.1080p.WEB.H264-GRP.mkv",
            "Movie.Name.2024.MULTi.2160p.WEB.H265-OTHER.mkv",
            Some(&fr),
            Some(&fr),
        );
        assert!(!up.is_blocked());
        assert_eq!(up.current.resolution.as_deref(), Some("1080p"));
        assert_eq!(up.candidate.resolution.as_deref(), Some("2160p"));
        // A lower resolution is not a local blocker: ARR decides quality.
        let down = assess(
            "Movie.Name.2024.MULTi.1080p.WEB.H264-GRP.mkv",
            "Movie.Name.2024.MULTi.720p.WEB.H264-OTHER.mkv",
            Some(&fr),
            Some(&fr),
        );
        assert!(!down.is_blocked());
        // An English-only candidate for a French+English current file is blocked by real audio.
        let en = probe(Some(7200.0), &[Some("eng")]);
        let lost = assess(
            "Movie.Name.2024.MULTi.1080p.mkv",
            "Movie.Name.2024.MULTi.2160p.mkv",
            Some(&fr),
            Some(&en),
        );
        assert_eq!(
            lost.failures(),
            vec![&CheckFailure::AudioLanguageMissing {
                language: "fr".into()
            }]
        );
        // Without probes: legacy language rule (MULTi source vs FRENCH candidate) blocks.
        let names = assess(
            "Movie.Name.2024.MULTi.1080p.WEB.H264-GRP.mkv",
            "Movie.Name.2024.FRENCH.720p.WEB.H264-OTHER.mkv",
            None,
            None,
        );
        assert!(
            names
                .failures()
                .contains(&&CheckFailure::ReleaseLanguageIncompatible)
        );
    }

    #[test]
    fn undetermined_audio_requires_caution() {
        let n = parse_scene_name("M.2020.mkv");
        fn facts<'a>(n: &'a ParsedName, p: &'a ProbeReport) -> MediaFacts<'a> {
            MediaFacts {
                parsed: n,
                probe: Some(p),
            }
        }
        let f = |p| facts(&n, p);
        let fr = probe(Some(1.0), &[Some("fre")]);
        let und = probe(Some(1.0), &[None]);
        let fr_und = probe(Some(1.0), &[Some("fra"), Some("und")]);
        // Candidate only `und`: French cannot be confirmed.
        assert!(matches!(
            audio_language_check(f(&fr), f(&und)).outcome,
            CheckOutcome::Block(CheckFailure::AudioLanguageUnverified { .. })
        ));
        // Current has an `und` track: what it holds is unknown.
        assert!(matches!(
            audio_language_check(f(&fr_und), f(&fr)).outcome,
            CheckOutcome::Block(CheckFailure::AudioLanguageUnverified { .. })
        ));
        // fre vs fra are the same language.
        assert_eq!(
            audio_language_check(f(&fr), f(&fr_und)).outcome,
            CheckOutcome::Pass
        );
        // Candidate without audio.
        let silent = probe(Some(1.0), &[]);
        assert!(matches!(
            audio_language_check(f(&fr), f(&silent)).outcome,
            CheckOutcome::Block(CheckFailure::AudioLanguageMissing { .. })
        ));
        // No probe at all: blocked as unverified even with compatible names.
        let m = parse_scene_name("M.2020.MULTi.mkv");
        let np = MediaFacts {
            parsed: &m,
            probe: None,
        };
        assert!(matches!(
            audio_language_check(np, np).outcome,
            CheckOutcome::Block(CheckFailure::AudioLanguageUnverified { .. })
        ));
    }

    #[test]
    fn numbering_and_coverage() {
        let n =
            |a: &str, b: &str| numbering_check(&parse_scene_name(a), &parse_scene_name(b)).outcome;
        assert_eq!(
            n("Show.S01E01.mkv", "Show.S01E01.2160p.mkv"),
            CheckOutcome::Pass
        );
        assert_eq!(
            n("Show.S01E01.mkv", "Show.S01E01E02.mkv"),
            CheckOutcome::Pass
        );
        assert_eq!(
            n("Show.S01E01E02.mkv", "Show.S01E01.mkv"),
            CheckOutcome::Block(CheckFailure::CoverageReduced {
                season: 1,
                missing: vec![2]
            })
        );
        assert!(matches!(
            n("Show.S01E01.mkv", "Show.S01E02.mkv"),
            CheckOutcome::Block(CheckFailure::EpisodesDiffer { .. })
        ));
        assert!(matches!(
            n("Show.S02E25.mkv", "Show - 25.mkv"),
            CheckOutcome::Block(CheckFailure::EpisodesDiffer { .. })
        ));
        assert_eq!(
            n("Anime - 25.mkv", "Anime - 25 [1080p].mkv"),
            CheckOutcome::Pass
        );
        assert_eq!(n("M.2020.mkv", "M.2020.mkv"), CheckOutcome::Pass);
        assert_eq!(
            n("M.mkv", "M.2020.mkv"),
            CheckOutcome::Block(CheckFailure::YearMissing)
        );
        assert_eq!(
            CheckFailure::CoverageReduced {
                season: 1,
                missing: vec![]
            }
            .blocker_code(),
            BlockerCode::CoverageIncomplete
        );
    }
}
