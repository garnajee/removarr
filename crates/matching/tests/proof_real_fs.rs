//! Content proof against the real confined filesystem: legacy comparison cases, late-byte
//! differences and files modified before, during and after the read.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use removarr_domain::error::PortError;
use removarr_domain::fs::{CompareOutcome, EntrySignature, SampleSpec};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::ports::{ContentReader, ScanControl};
use removarr_fs::ConfinedFs;
use removarr_matching::proof::SampleVerdict;
use removarr_matching::{
    ContentVerdict, ContentVerifier, FunnelInput, GateOutcome, MatchSettings, PreCompareGate,
    ReadLimiter, ReadOnlyGate, Rejection, Side, VerifyRequest, run_funnel,
};

fn verifier() -> ContentVerifier {
    ContentVerifier::new(ReadLimiter::new(1), 256)
}

struct Pair {
    a: RootedPath,
    b: RootedPath,
    sa: EntrySignature,
    sb: EntrySignature,
}

fn pair(t: &Tree, fs: &ConfinedFs, a: &str, b: &str) -> Pair {
    let a = rp(&RootId::completed(), a);
    let b = rp(&t.media_id(), b);
    let (sa, sb) = (stat(fs, &a), stat(fs, &b));
    Pair { a, b, sa, sb }
}

fn req(p: &Pair) -> VerifyRequest<'_> {
    VerifyRequest {
        a: &p.a,
        b: &p.b,
        sig_a: &p.sa,
        sig_b: &p.sb,
    }
}

fn verify(fs: &ConfinedFs, reader: &dyn ContentReader, p: &Pair) -> ContentVerdict {
    verifier()
        .verify(
            fs,
            reader,
            &ReadOnlyGate,
            req(p),
            &ScanControl::default(),
            &TestClock,
        )
        .unwrap()
}

/// T-11 `test_same_name_and_size_is_only_a_potential_duplicate`.
#[test]
fn t11_same_name_and_size_with_different_bytes_is_rejected() {
    let t = Tree::new();
    t.write(&t.completed, "Movie.Name.2024.mkv", b"AAAA");
    t.write(&t.media, "films/Movie.Name.2024.mkv", b"BBBB");
    let fs = t.fs();
    let src = [scan(&fs, &RootId::completed())];
    let tgt = [scan(&fs, &t.media_id())];
    let out = run_funnel(
        FunnelInput {
            sources: &src,
            targets: &tgt,
            previous: &[],
        },
        &MatchSettings::default(),
        &verifier(),
        &fs,
        &ScanControl::default(),
    )
    .unwrap();
    assert_eq!(out.pairs.len(), 1, "a candidate exists");
    let p = pair(&t, &fs, "Movie.Name.2024.mkv", "films/Movie.Name.2024.mkv");
    assert_eq!(
        verify(&fs, &fs, &p),
        ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 0 })
    );
}

/// T-12 `test_quick_match_can_never_bypass_final_exact_comparison`.
#[test]
fn t12_matching_samples_never_bypass_the_full_comparison() {
    let t = Tree::new();
    let src = vec![b'A'; 4096];
    let mut dst = src.clone();
    dst[1000] = b'B'; // outside the samples at 0, 2016 and 4032
    t.write(&t.completed, "Movie.Name.2024.1080p.mkv", &src);
    t.write(&t.media, "films/Movie.Name.2024.2160p.mkv", &dst);
    let fs = t.fs();
    let p = pair(
        &t,
        &fs,
        "Movie.Name.2024.1080p.mkv",
        "films/Movie.Name.2024.2160p.mkv",
    );
    let spec = SampleSpec { size: 64, count: 3 };
    assert_eq!(
        verifier()
            .sample_check(&fs, req(&p), spec, &ScanControl::default())
            .unwrap(),
        SampleVerdict::NotRejected
    );
    assert_eq!(
        verify(&fs, &fs, &p),
        ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 1000 })
    );
    assert_eq!(
        std::fs::read(t.media.join("films/Movie.Name.2024.2160p.mkv")).unwrap(),
        dst
    );
}

/// A difference in the last chunk, outside every sample window, is still found.
#[test]
fn late_byte_difference_in_the_last_chunk_is_found() {
    let t = Tree::new();
    let size = 300 * 1024 + 123;
    let src: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let mut dst = src.clone();
    let offset = size - 65; // just before the last 64-byte sample window
    dst[offset] ^= 0xFF;
    t.write(&t.completed, "Big.2020.mkv", &src);
    t.write(&t.media, "Big.2020.mkv", &dst);
    let fs = t.fs();
    let p = pair(&t, &fs, "Big.2020.mkv", "Big.2020.mkv");
    let spec = SampleSpec { size: 64, count: 5 };
    assert_eq!(
        verifier()
            .sample_check(&fs, req(&p), spec, &ScanControl::default())
            .unwrap(),
        SampleVerdict::NotRejected
    );
    // 64 KiB chunks (the reader's minimum): the difference sits in the fifth chunk.
    assert_eq!(
        verify(&fs, &fs, &p),
        ContentVerdict::Rejected(Rejection::ContentDiffers {
            offset: offset as u64
        })
    );
    // Identical copies of the same size are proven identical.
    t.write(&t.media, "Big.2020.mkv", &src);
    let p = pair(&t, &fs, "Big.2020.mkv", "Big.2020.mkv");
    assert!(verify(&fs, &fs, &p).is_identical());
}

#[test]
fn modified_before_the_read_is_detected() {
    let t = Tree::new();
    t.write(&t.completed, "M.2020.mkv", b"same-bytes");
    t.write(&t.media, "M.2020.mkv", b"same-bytes");
    let fs = t.fs();
    let p = pair(&t, &fs, "M.2020.mkv", "M.2020.mkv");
    t.write(&t.media, "M.2020.mkv", b"same-bytez");
    // Even if mtime granularity hides the rewrite, the reader's own signature check or the
    // byte comparison rejects it; with a visible change it is caught before any read.
    let v = verify(&fs, &fs, &p);
    assert!(
        matches!(
            v,
            ContentVerdict::Rejected(
                Rejection::ChangedBefore { side: Side::B } | Rejection::ContentDiffers { .. }
            )
        ),
        "{v:?}"
    );
    std::fs::remove_file(t.completed.join("M.2020.mkv")).unwrap();
    assert_eq!(
        verify(&fs, &fs, &p),
        ContentVerdict::Rejected(Rejection::Missing { side: Side::A })
    );
}

/// Wraps the real reader and rewrites a file at a chosen moment.
struct Writer {
    inner: Arc<ConfinedFs>,
    path: std::path::PathBuf,
    data: Vec<u8>,
    after: bool,
}

impl ContentReader for Writer {
    fn samples_match(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        spec: SampleSpec,
        ctl: &ScanControl,
    ) -> Result<bool, PortError> {
        self.inner.samples_match(a, b, expect, spec, ctl)
    }
    fn full_compare(
        &self,
        a: &RootedPath,
        b: &RootedPath,
        expect: [&EntrySignature; 2],
        chunk: u32,
        ctl: &ScanControl,
    ) -> Result<CompareOutcome, PortError> {
        let rewrite = || {
            // Grow the file so the change is visible regardless of mtime granularity.
            std::fs::write(&self.path, &self.data).unwrap();
        };
        if self.after {
            let r = self.inner.full_compare(a, b, expect, chunk, ctl);
            rewrite();
            r
        } else {
            rewrite();
            self.inner.full_compare(a, b, expect, chunk, ctl)
        }
    }
}

#[test]
fn modified_during_the_read_voids_the_comparison() {
    let t = Tree::new();
    t.write(&t.completed, "M.2020.mkv", &[7u8; 100_000]);
    t.write(&t.media, "M.2020.mkv", &[7u8; 100_000]);
    let fs = Arc::new(t.fs());
    let p = pair(&t, &fs, "M.2020.mkv", "M.2020.mkv");
    let reader = Writer {
        inner: fs.clone(),
        path: t.media.join("M.2020.mkv"),
        data: vec![7u8; 100_001],
        after: false,
    };
    assert_eq!(
        verify(&fs, &reader, &p),
        ContentVerdict::Rejected(Rejection::ChangedDuring)
    );
}

#[test]
fn modified_after_the_read_voids_an_identical_result() {
    let t = Tree::new();
    t.write(&t.completed, "M.2020.mkv", &[7u8; 100_000]);
    t.write(&t.media, "M.2020.mkv", &[7u8; 100_000]);
    let fs = Arc::new(t.fs());
    let p = pair(&t, &fs, "M.2020.mkv", "M.2020.mkv");
    let reader = Writer {
        inner: fs.clone(),
        path: t.completed.join("M.2020.mkv"),
        data: vec![7u8; 100_001],
        after: true,
    };
    assert_eq!(
        verify(&fs, &reader, &p),
        ContentVerdict::Rejected(Rejection::ChangedAfter { side: Side::A })
    );
}

/// T-14 analogue with the real reader: a refused write test leaves both files unread.
#[test]
fn refused_write_test_precedes_any_read() {
    struct Refuse;
    impl PreCompareGate for Refuse {
        fn check(&self, _: &RootedPath, _: &RootedPath) -> GateOutcome {
            GateOutcome::Refused {
                detail: "temporary hardlink refused (EPERM)".into(),
            }
        }
    }
    struct Panics;
    impl ContentReader for Panics {
        fn samples_match(
            &self,
            _: &RootedPath,
            _: &RootedPath,
            _: [&EntrySignature; 2],
            _: SampleSpec,
            _: &ScanControl,
        ) -> Result<bool, PortError> {
            panic!("no sample read allowed")
        }
        fn full_compare(
            &self,
            _: &RootedPath,
            _: &RootedPath,
            _: [&EntrySignature; 2],
            _: u32,
            _: &ScanControl,
        ) -> Result<CompareOutcome, PortError> {
            panic!("no content read allowed before the write test")
        }
    }
    let t = Tree::new();
    t.write(&t.completed, "M.2020.mkv", b"x");
    t.write(&t.media, "M.2020.mkv", b"x");
    let fs = t.fs();
    let p = pair(&t, &fs, "M.2020.mkv", "M.2020.mkv");
    let v = verifier()
        .verify(
            &fs,
            &Panics,
            &Refuse,
            req(&p),
            &ScanControl::default(),
            &TestClock,
        )
        .unwrap();
    assert!(matches!(
        v,
        ContentVerdict::Rejected(Rejection::TargetNotWritable { .. })
    ));
}
