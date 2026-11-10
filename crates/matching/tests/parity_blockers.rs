//! Pass 4 parity: local checks can only block, never rank. Identity, edition, coverage and real
//! audio block a replacement; resolution, codec, source and group never matter (CON-02).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use removarr_domain::evidence::ContentProof;
use removarr_domain::issue::BlockerCode;
use removarr_domain::media::{AudioStream, ProbeReport, VideoStream};
use removarr_domain::time::Timestamp;
use removarr_matching::checks::{
    CheckFailure, CheckKind, CheckOutcome, DurationTolerance, MediaFacts, assess_replacement,
};
use removarr_matching::proof::WriteCheck;
use removarr_matching::similarity::similarity;
use removarr_matching::{ContentVerdict, Rejection, parse_scene_name};

fn probe(duration: f64, langs: &[Option<&str>]) -> ProbeReport {
    ProbeReport {
        duration_s: Some(duration),
        video: vec![VideoStream {
            codec: "h264".into(),
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

fn codes(
    cur: &str,
    cand: &str,
    pc: Option<&ProbeReport>,
    pn: Option<&ProbeReport>,
) -> Vec<BlockerCode> {
    let (a, b) = (parse_scene_name(cur), parse_scene_name(cand));
    assess_replacement(
        MediaFacts {
            parsed: &a,
            probe: pc,
        },
        MediaFacts {
            parsed: &b,
            probe: pn,
        },
        DurationTolerance::default(),
    )
    .blockers()
    .iter()
    .map(|b| b.code.clone())
    .collect()
}

const CUR: &str = "Synthetic.Show.S01E01.1080p.BluRay.x264-AAA.mkv";

#[test]
fn control_everything_matching_and_probed_passes_every_check() {
    let p = probe(1500.0, &[Some("eng")]);
    assert!(
        codes(
            CUR,
            "Synthetic.Show.S01E01.720p.WEB.x265-BBB.mkv",
            Some(&p),
            Some(&p)
        )
        .is_empty()
    );
}

#[test]
fn edition_coverage_title_and_numbering_each_block_alone() {
    let p = probe(1500.0, &[Some("eng")]);
    let movie = "Synthetic.Movie.2020.1080p.BluRay-AAA.mkv";
    assert_eq!(
        codes(
            movie,
            "Synthetic.Movie.2020.Extended.1080p.BluRay-AAA.mkv",
            Some(&p),
            Some(&p)
        ),
        vec![BlockerCode::EditionMismatch]
    );
    assert_eq!(
        codes(
            "Synthetic.Movie.2020.Directors.Cut.mkv",
            "Synthetic.Movie.2020.Extended.mkv",
            Some(&p),
            Some(&p)
        ),
        vec![BlockerCode::EditionMismatch]
    );
    assert_eq!(
        codes(
            "Synthetic.Show.S01E01E02.mkv",
            "Synthetic.Show.S01E01.mkv",
            Some(&p),
            Some(&p)
        ),
        vec![BlockerCode::CoverageIncomplete]
    );
    assert_eq!(
        codes(
            CUR,
            "Synthetic.Show.S01E02.1080p.BluRay.x264-AAA.mkv",
            Some(&p),
            Some(&p)
        ),
        vec![BlockerCode::NumberingMismatch]
    );
    assert_eq!(
        codes(
            CUR,
            "Other.Show.S01E01.1080p.BluRay.x264-AAA.mkv",
            Some(&p),
            Some(&p)
        ),
        vec![BlockerCode::ArrAmbiguous]
    );
}

#[test]
fn real_audio_missing_and_und_tracks_block() {
    let cur = probe(1500.0, &[Some("fra"), Some("eng")]);
    let name = "Synthetic.Show.S01E01.1080p-AAA.mkv";
    let audio = || BlockerCode::AudioLanguageMissing;
    // Candidate lacks French.
    assert_eq!(
        codes(name, name, Some(&cur), Some(&probe(1500.0, &[Some("eng")]))),
        vec![audio()]
    );
    // Candidate without any audio track.
    assert_eq!(
        codes(name, name, Some(&cur), Some(&probe(1500.0, &[]))),
        vec![audio()]
    );
    // Undetermined on either side never reads as compatible, even with the same names.
    for (c, n) in [
        (probe(1500.0, &[None]), probe(1500.0, &[Some("eng")])),
        (probe(1500.0, &[Some("und")]), probe(1500.0, &[Some("und")])),
        (
            probe(1500.0, &[Some("eng"), Some("und")]),
            probe(1500.0, &[Some("eng")]),
        ),
    ] {
        assert_eq!(
            codes(name, name, Some(&c), Some(&n)),
            vec![audio()],
            "{c:?} vs {n:?}"
        );
    }
}

/// An extra undetermined track on the candidate does not hide anything: every language the
/// current file requires is confirmed present. (Doc comment of the check is broader than the
/// behavior; reported in the handoff as an observation.)
#[test]
fn extra_und_track_on_the_candidate_is_fine_when_all_required_languages_are_confirmed() {
    let name = "Synthetic.Show.S01E01.1080p-AAA.mkv";
    let cur = probe(1500.0, &[Some("eng")]);
    let cand = probe(1500.0, &[Some("eng"), None]);
    assert!(codes(name, name, Some(&cur), Some(&cand)).is_empty());
}

#[test]
fn missing_probes_or_durations_always_block() {
    let p = probe(1500.0, &[Some("eng")]);
    for (a, b) in [(None, None), (Some(&p), None), (None, Some(&p))] {
        let c = codes(CUR, CUR, a, b);
        assert!(c.contains(&BlockerCode::DurationMismatch), "{c:?}");
        assert!(c.contains(&BlockerCode::AudioLanguageMissing), "{c:?}");
    }
    let mut no_dur = p.clone();
    no_dur.duration_s = None;
    assert!(codes(CUR, CUR, Some(&p), Some(&no_dur)).contains(&BlockerCode::DurationMismatch));
    no_dur.duration_s = Some(f64::NAN);
    assert!(codes(CUR, CUR, Some(&p), Some(&no_dur)).contains(&BlockerCode::DurationMismatch));
}

fn tag(opts: &'static [&'static str]) -> impl Strategy<Value = &'static str> {
    prop::sample::select(opts)
}

proptest! {
    /// No local quality ranking: swapping resolution, source, codec or group never changes a
    /// single check outcome, in either direction, and never adds or removes a blocker.
    #[test]
    fn technical_tags_never_change_any_check(
        r1 in tag(&["480p", "720p", "1080p", "2160p"]), r2 in tag(&["480p", "720p", "1080p", "2160p"]),
        s1 in tag(&["DVDRip", "WEB-DL", "BluRay", "REMUX"]), s2 in tag(&["DVDRip", "WEB-DL", "BluRay", "REMUX"]),
        c1 in tag(&["x264", "x265", "HEVC", "XviD"]), c2 in tag(&["x264", "x265", "HEVC", "XviD"]),
        g1 in tag(&["AAA", "BBB", "YIFY", "FraMeSToR"]), g2 in tag(&["AAA", "BBB", "YIFY", "FraMeSToR"]),
    ) {
        let p = probe(1500.0, &[Some("eng")]);
        let cur = format!("Synthetic.Show.S01E01.{r1}.{s1}.{c1}-{g1}.mkv");
        let cand = format!("Synthetic.Show.S01E01.{r2}.{s2}.{c2}-{g2}.mkv");
        let (a, b) = (parse_scene_name(&cur), parse_scene_name(&cand));
        let run = |x: &_, y: &_| assess_replacement(
            MediaFacts { parsed: x, probe: Some(&p) },
            MediaFacts { parsed: y, probe: Some(&p) },
            DurationTolerance::default(),
        );
        let fwd = run(&a, &b);
        let rev = run(&b, &a);
        prop_assert!(fwd.checks.iter().all(|c| c.outcome == CheckOutcome::Pass), "{:?}", fwd.checks);
        prop_assert!(rev.checks.iter().all(|c| c.outcome == CheckOutcome::Pass));
        prop_assert!(!fwd.is_blocked() && !rev.is_blocked());
        // Same names with probes missing: the same blockers whatever the tags are.
        let base = parse_scene_name("Synthetic.Show.S01E01.mkv");
        let tagged = assess_replacement(
            MediaFacts { parsed: &a, probe: None }, MediaFacts { parsed: &b, probe: None },
            DurationTolerance::default());
        let plain = assess_replacement(
            MediaFacts { parsed: &base, probe: None }, MediaFacts { parsed: &base, probe: None },
            DurationTolerance::default());
        let kinds = |x: &removarr_matching::checks::ReplacementAssessment|
            x.checks.iter().map(|c| (c.kind, c.outcome == CheckOutcome::Pass)).collect::<Vec<_>>();
        prop_assert_eq!(kinds(&tagged), kinds(&plain));
        let unverified = tagged.checks.iter().any(|c| c.kind == CheckKind::Duration
            && matches!(c.outcome, CheckOutcome::Block(CheckFailure::DurationUnverified { .. })));
        prop_assert!(unverified);
    }

    /// The identification score ignores technical tags entirely: it is not a quality score.
    #[test]
    fn similarity_ignores_technical_tags(
        r in tag(&["480p", "720p", "1080p", "2160p"]), s in tag(&["DVDRip", "WEB-DL", "BluRay", "REMUX"]),
        c in tag(&["x264", "x265", "HEVC"]), g in tag(&["AAA", "YIFY", "FraMeSToR"]),
        size in 1u64..10_000_000,
    ) {
        let plain = parse_scene_name("Synthetic.Show.S01E01.mkv");
        let tagged = parse_scene_name(&format!("Synthetic.Show.S01E01.{r}.{s}.{c}-{g}.mkv"));
        prop_assert_eq!(
            similarity(&plain, size, &plain, size).score_milli(),
            similarity(&plain, size, &tagged, size).score_milli()
        );
        // Equal names and sizes are a high score and still carry no proof value.
        prop_assert!(similarity(&plain, size, &tagged, size).score_milli() <= 1000);
    }
}

/// Only a completed byte comparison persists as proof of identity; nothing else does.
#[test]
fn only_a_completed_full_compare_becomes_identical_proof() {
    let identical = ContentVerdict::Identical {
        compared_at: Timestamp(5),
        write_check: WriteCheck::Passed,
    };
    assert_eq!(
        identical.to_content_proof(),
        ContentProof::Identical {
            compared_at: Timestamp(5)
        }
    );
    for v in [
        ContentVerdict::SamePhysical,
        ContentVerdict::Rejected(Rejection::SamplesDiffer),
        ContentVerdict::Rejected(Rejection::ChangedDuring),
        ContentVerdict::Rejected(Rejection::SizeDiffers { a: 1, b: 2 }),
    ] {
        assert!(!v.is_identical());
        assert_eq!(v.to_content_proof(), ContentProof::None, "{v:?}");
    }
    assert_eq!(
        ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 9 }).to_content_proof(),
        ContentProof::Differs { offset: 9 }
    );
}
