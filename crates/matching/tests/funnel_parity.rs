//! The funnel on the legacy parity fixture (`docs/legacy-parity.md` §7.6, §7.7) with the real
//! confined filesystem adapter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use removarr_domain::evidence::{ContentProof, IdentityProof};
use removarr_domain::fs::SampleSpec;
use removarr_domain::ids::{RawUuid, SessionId};
use removarr_domain::path::RootId;
use removarr_domain::ports::ScanControl;
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_matching::funnel::Stage;
use removarr_matching::{
    ContentVerdict, ContentVerifier, FunnelInput, FunnelOutput, MatchSettings, ReadLimiter,
    ReadOnlyGate, Rejection, VerifyRequest, run_funnel,
};
use removarr_testkit::fixture::ParityFixture;

const BRAVO: &str = "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv";
const NOVEMBER: &str = "November.Movie.2010.1080p.WEB.x264-GRP.mkv";
const C_MAIN: &str =
    "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP/Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv";
const C_COPY: &str = "movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv";
const DELTA: &str = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";

fn settings() -> MatchSettings {
    MatchSettings {
        // Legacy fixture windows: 5 x 4096 (V7 flips byte 10000, outside every window).
        sample: SampleSpec {
            size: 4096,
            count: 5,
        },
        ..MatchSettings::default()
    }
}

fn fs_of(fx: &ParityFixture) -> ConfinedFs {
    let roots = removarr_config::Roots::validate(&removarr_config::RootPaths {
        completed: Some(fx.completed().to_str().unwrap().into()),
        incomplete: Some(fx.incomplete().to_str().unwrap().into()),
        media: vec![fx.medias().to_str().unwrap().into()],
    })
    .unwrap();
    ConfinedFs::new(&roots, FsLimits::default())
}

fn funnel(fx: &ParityFixture, fs: &ConfinedFs) -> FunnelOutput {
    let src = [scan(fs, &RootId::completed())];
    let tgt = [scan(fs, &media_id(&fx.medias()))];
    let verifier = ContentVerifier::new(ReadLimiter::new(1), 65536);
    run_funnel(
        FunnelInput {
            sources: &src,
            targets: &tgt,
            previous: &[],
        },
        &settings(),
        &verifier,
        fs,
        &ScanControl::default(),
    )
    .unwrap()
}

fn pair<'a>(out: &'a FunnelOutput, a: &str) -> Vec<&'a removarr_matching::FunnelPair> {
    out.pairs.iter().filter(|p| p.a.rel.as_str() == a).collect()
}

#[test]
fn parity_fixture_funnel_matches_the_expected_candidates() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_of(&fx);
    let out = funnel(&fx, &fs);

    // Stage 1: I_A, I_F1, I_K are real hardlinks (shared inodes = 3, §7.5).
    assert_eq!(out.shared.len(), 3);
    assert_eq!(out.stats.already_linked, 3);

    // R1 and R2: exact name + size, no content read (T-10).
    for name in [BRAVO, NOVEMBER] {
        let p = pair(&out, name);
        assert_eq!(p.len(), 1, "{name}");
        assert_eq!(p[0].stage, Stage::NameSize);
        assert_eq!(p[0].content, ContentProof::NameSize);
    }
    // FX-06 -> FX-09: parsing + samples (CON-16 adds what the legacy default missed).
    let c = pair(&out, C_MAIN);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].b.rel.as_str(), C_COPY);
    assert_eq!(c[0].stage, Stage::Semantic);
    assert_eq!(c[0].content, ContentProof::Sampled);
    assert_eq!(
        c[0].identity,
        IdentityProof::NameParse { score_milli: 1000 }
    );
    // FX-10 -> FX-11: same title, different release (size differs): identity only, ARR decides.
    let d = pair(&out, DELTA);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].identity, IdentityProof::NameParse { score_milli: 950 });
    assert_eq!(d[0].content, ContentProof::None);
    assert!(matches!(
        d[0].rejection,
        Some(Rejection::SizeDiffers { .. })
    ));
    // Samples (FX-07, FX-27) never receive a replacement candidate (CON-01).
    assert_eq!(out.stats.samples_skipped, 2);
    // Nothing else; the funnel never claims identity of distinct files.
    assert_eq!(out.pairs.len(), 4);
    assert!(out.pairs.iter().all(|p| !p.content.proves_equal()));
    assert_eq!(out.stats.sample_reads, 1);

    // Persisted rows (MatchStore::replace_for_session) keep reasons and signatures.
    let rows = out.match_candidates(SessionId::new(RawUuid::nil()));
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter()
            .any(|r| r.reasons.contains(&"samples_not_rejected".into()))
    );
}

/// VerifyContent: full comparison proves the three real duplicates of §7.7.
#[test]
fn parity_real_duplicates_are_proven_only_by_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_of(&fx);
    let out = funnel(&fx, &fs);
    let verifier = ContentVerifier::new(ReadLimiter::new(1), 65536);
    let proven: Vec<_> = out
        .pairs
        .iter()
        .filter(|p| p.rejection.is_none())
        .map(|p| {
            let v = verifier
                .verify(
                    &fs,
                    &fs,
                    &ReadOnlyGate,
                    VerifyRequest {
                        a: &p.a,
                        b: &p.b,
                        sig_a: &p.sig_a,
                        sig_b: &p.sig_b,
                    },
                    &ScanControl::default(),
                    &TestClock,
                )
                .unwrap();
            (p.a.rel.as_str().to_string(), v.is_identical())
        })
        .collect();
    assert_eq!(proven.len(), 3);
    assert!(proven.iter().all(|(_, ok)| *ok), "{proven:?}");
}

/// V7: the funnel keeps the pair through sampling; only the full comparison rejects it.
#[test]
fn v7_late_byte_survives_samples_and_is_rejected_by_full_comparison() {
    let fx = ParityFixture::build().unwrap();
    fx.apply_v7().unwrap();
    let fs = fs_of(&fx);
    let out = funnel(&fx, &fs);
    let c = pair(&out, C_MAIN);
    assert_eq!(
        c[0].content,
        ContentProof::Sampled,
        "samples only eliminate"
    );
    let v = ContentVerifier::new(ReadLimiter::new(1), 65536)
        .verify(
            &fs,
            &fs,
            &ReadOnlyGate,
            VerifyRequest {
                a: &c[0].a,
                b: &c[0].b,
                sig_a: &c[0].sig_a,
                sig_b: &c[0].sig_b,
            },
            &ScanControl::default(),
            &TestClock,
        )
        .unwrap();
    assert_eq!(
        v,
        ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 10000 })
    );
    assert_eq!(
        v.to_content_proof(),
        ContentProof::Differs { offset: 10000 }
    );
}

/// V6 after the scan: FX-04 rewritten with the same size; the stored candidate is void.
#[test]
fn v6_rewrite_after_scan_voids_the_name_size_candidate() {
    let fx = ParityFixture::build().unwrap();
    let fs = fs_of(&fx);
    let out = funnel(&fx, &fs);
    let b = pair(&out, BRAVO)[0].clone();
    fx.apply_v6().unwrap();
    let verifier = ContentVerifier::new(ReadLimiter::new(1), 65536);
    let v = verifier
        .verify(
            &fs,
            &fs,
            &ReadOnlyGate,
            VerifyRequest {
                a: &b.a,
                b: &b.b,
                sig_a: &b.sig_a,
                sig_b: &b.sig_b,
            },
            &ScanControl::default(),
            &TestClock,
        )
        .unwrap();
    assert!(
        matches!(v, ContentVerdict::Rejected(Rejection::ChangedBefore { .. })),
        "{v:?}"
    );
    // A rescan rebuilds the candidate; the full comparison then finds the first byte differs.
    let again = funnel(&fx, &fs);
    let b2 = pair(&again, BRAVO)[0].clone();
    let v = verifier
        .verify(
            &fs,
            &fs,
            &ReadOnlyGate,
            VerifyRequest {
                a: &b2.a,
                b: &b2.b,
                sig_a: &b2.sig_a,
                sig_b: &b2.sig_b,
            },
            &ScanControl::default(),
            &TestClock,
        )
        .unwrap();
    assert_eq!(
        v,
        ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 0 })
    );
}
