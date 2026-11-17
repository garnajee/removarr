//! Content proof (funnel levels 4 and 5): samples only reject, full byte comparison is the only
//! way to declare two distinct files identical (INV-02, INV-03).
//!
//! [`ContentVerifier::verify`] is the single constructor of an "identical" verdict in this crate.
//! Its order is fixed: signatures re-checked, sizes compared, target write test, full compare,
//! signatures re-checked again. A change before, during or after the read voids the result.
//! It never mutates anything; the write test is delegated to a [`PreCompareGate`] supplied by the
//! caller (the hardlink probe is a mutation and belongs to the action runner, 01 handoff).

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{ContentProof, MatchCandidate};
use removarr_domain::fs::{CompareOutcome, EntryKind, EntrySignature, SampleSpec};
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{Clock, ContentReader, FsInventory, FsMutator, ScanControl};
use removarr_domain::time::Timestamp;
use serde::{Deserialize, Serialize};

use crate::limiter::ReadLimiter;
use crate::reject::{Rejection, Side};

/// Result of the write test run before an expensive full comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// The target directory accepts a hardlink from the source (probe passed).
    Writable,
    /// No write test was run (read-only verification); recorded in the verdict.
    NotTested { reason: String },
    /// The write test failed: the comparison is not run, no content is read.
    Refused { detail: String },
}

/// Write capability test of the target directory, run before any full comparison (DR-21, T-14).
pub trait PreCompareGate: Send + Sync {
    fn check(&self, source: &RootedPath, target: &RootedPath) -> GateOutcome;
}

/// Gate for read-only verification jobs (`VerifyContent`): never writes, always `NotTested`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOnlyGate;

impl PreCompareGate for ReadOnlyGate {
    fn check(&self, _source: &RootedPath, _target: &RootedPath) -> GateOutcome {
        GateOutcome::NotTested {
            reason: "read-only verification".into(),
        }
    }
}

/// Gate for execution: the live `FsMutator::probe_hardlink` (temporary hardlink of the source in
/// the target's directory). Only the action runner holds a mutator.
#[derive(Clone, Copy)]
pub struct HardlinkProbeGate<'a> {
    pub mutator: &'a dyn FsMutator,
}

impl std::fmt::Debug for HardlinkProbeGate<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HardlinkProbeGate")
    }
}

impl PreCompareGate for HardlinkProbeGate<'_> {
    fn check(&self, source: &RootedPath, target: &RootedPath) -> GateOutcome {
        let Some(dir) = target.rel.parent() else {
            return GateOutcome::Refused {
                detail: "target has no parent directory".into(),
            };
        };
        let dir = RootedPath::new(target.root.clone(), dir);
        match self.mutator.probe_hardlink(source, &dir) {
            Ok(()) => GateOutcome::Writable,
            Err(e) => GateOutcome::Refused { detail: e.detail },
        }
    }
}

/// Whether the target write test ran before the comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteCheck {
    Passed,
    NotTested { reason: String },
}

/// Outcome of a full verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentVerdict {
    /// Every byte compared equal and neither file changed before, during or after.
    Identical {
        compared_at: Timestamp,
        write_check: WriteCheck,
    },
    /// Both paths are the same inode already (hardlinks): nothing to compare.
    SamePhysical,
    Rejected(Rejection),
}

impl ContentVerdict {
    /// Domain proof to persist (`MatchStore::update_proof`). Anything but a completed compare
    /// is `ContentProof::None`.
    #[must_use]
    pub fn to_content_proof(&self) -> ContentProof {
        match self {
            ContentVerdict::Identical { compared_at, .. } => ContentProof::Identical {
                compared_at: *compared_at,
            },
            ContentVerdict::Rejected(Rejection::ContentDiffers { offset }) => {
                ContentProof::Differs { offset: *offset }
            }
            _ => ContentProof::None,
        }
    }

    #[must_use]
    pub fn is_identical(&self) -> bool {
        matches!(self, ContentVerdict::Identical { .. })
    }
}

/// Outcome of a sample check. There is deliberately no "equal" variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleVerdict {
    /// No sample window differed. This is a proposal, never equality.
    NotRejected,
    Rejected(Rejection),
}

/// One pair to verify, with the signatures the candidate was built from.
#[derive(Debug, Clone, Copy)]
pub struct VerifyRequest<'a> {
    pub a: &'a RootedPath,
    pub b: &'a RootedPath,
    pub sig_a: &'a EntrySignature,
    pub sig_b: &'a EntrySignature,
}

impl<'a> VerifyRequest<'a> {
    #[must_use]
    pub fn from_candidate(c: &'a MatchCandidate) -> Self {
        Self {
            a: &c.a,
            b: &c.b,
            sig_a: &c.sig_a,
            sig_b: &c.sig_b,
        }
    }
}

/// Bounded sample and full-compare runner.
#[derive(Debug, Clone)]
pub struct ContentVerifier {
    limiter: ReadLimiter,
    chunk_bytes: u32,
}

fn cancelled() -> PortError {
    PortError::new(ErrorClass::Cancelled, "content verification cancelled")
}

fn check_current(
    fs: &dyn FsInventory,
    path: &RootedPath,
    expect: &EntrySignature,
    side: Side,
    after: bool,
) -> Result<Option<Rejection>, PortError> {
    let changed = if after {
        Rejection::ChangedAfter { side }
    } else {
        Rejection::ChangedBefore { side }
    };
    match fs.stat(path) {
        Ok(None) => Ok(Some(if after {
            changed
        } else {
            Rejection::Missing { side }
        })),
        Ok(Some(sig)) if sig.kind != EntryKind::File => {
            Ok(Some(Rejection::NotRegularFile { side }))
        }
        Ok(Some(sig)) if !sig.matches(expect, false) => Ok(Some(changed)),
        Ok(Some(_)) => Ok(None),
        Err(e) if e.class == ErrorClass::Cancelled => Err(e),
        Err(e) => Ok(Some(Rejection::ReadFailed { class: e.class })),
    }
}

fn read_error(e: PortError) -> Result<Rejection, PortError> {
    match e.class {
        ErrorClass::Cancelled => Err(e),
        ErrorClass::StateChanged => Ok(Rejection::ChangedDuring),
        class => Ok(Rejection::ReadFailed { class }),
    }
}

impl ContentVerifier {
    /// `chunk_bytes` is the full-compare buffer (the reader raises it to at least 64 KiB).
    #[must_use]
    pub fn new(limiter: ReadLimiter, chunk_bytes: u32) -> Self {
        Self {
            limiter,
            chunk_bytes,
        }
    }

    #[must_use]
    pub fn limiter(&self) -> &ReadLimiter {
        &self.limiter
    }

    /// Distributed samples: can only reject. Unequal sizes are rejected without any read.
    pub fn sample_check(
        &self,
        reader: &dyn ContentReader,
        req: VerifyRequest<'_>,
        spec: SampleSpec,
        ctl: &ScanControl,
    ) -> Result<SampleVerdict, PortError> {
        if ctl.cancel.is_cancelled() {
            return Err(cancelled());
        }
        if req.sig_a.size != req.sig_b.size {
            return Ok(SampleVerdict::Rejected(Rejection::SizeDiffers {
                a: req.sig_a.size,
                b: req.sig_b.size,
            }));
        }
        let result = {
            let _permit = self.limiter.acquire();
            reader.samples_match(req.a, req.b, [req.sig_a, req.sig_b], spec, ctl)
        };
        match result {
            Ok(true) => Ok(SampleVerdict::NotRejected),
            Ok(false) => Ok(SampleVerdict::Rejected(Rejection::SamplesDiffer)),
            Err(e) => read_error(e).map(SampleVerdict::Rejected),
        }
    }

    /// Full verification of one pair. `Err` only on cancellation; every other failure is a typed
    /// [`Rejection`] that proves nothing about equality.
    pub fn verify(
        &self,
        fs: &dyn FsInventory,
        reader: &dyn ContentReader,
        gate: &dyn PreCompareGate,
        req: VerifyRequest<'_>,
        ctl: &ScanControl,
        clock: &dyn Clock,
    ) -> Result<ContentVerdict, PortError> {
        if ctl.cancel.is_cancelled() {
            return Err(cancelled());
        }
        // Before: both files exist, are regular and still carry the candidate's signatures.
        for (path, sig, side) in [(req.a, req.sig_a, Side::A), (req.b, req.sig_b, Side::B)] {
            if let Some(r) = check_current(fs, path, sig, side, false)? {
                return Ok(ContentVerdict::Rejected(r));
            }
        }
        if req.sig_a.phys() == req.sig_b.phys() {
            return Ok(ContentVerdict::SamePhysical);
        }
        if req.sig_a.size != req.sig_b.size {
            return Ok(ContentVerdict::Rejected(Rejection::SizeDiffers {
                a: req.sig_a.size,
                b: req.sig_b.size,
            }));
        }
        // Write capability first: a refused target costs no content read (T-14).
        let write_check = match gate.check(req.a, req.b) {
            GateOutcome::Writable => WriteCheck::Passed,
            GateOutcome::NotTested { reason } => WriteCheck::NotTested { reason },
            GateOutcome::Refused { detail } => {
                return Ok(ContentVerdict::Rejected(Rejection::TargetNotWritable {
                    detail,
                }));
            }
        };
        // During: the reader re-checks both signatures around its reads.
        let outcome = {
            let _permit = self.limiter.acquire();
            reader.full_compare(req.a, req.b, [req.sig_a, req.sig_b], self.chunk_bytes, ctl)
        };
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => return read_error(e).map(ContentVerdict::Rejected),
        };
        if outcome == CompareOutcome::Changed {
            return Ok(ContentVerdict::Rejected(Rejection::ChangedDuring));
        }
        // After: an unchanged signature on both sides is required for any conclusion.
        for (path, sig, side) in [(req.a, req.sig_a, Side::A), (req.b, req.sig_b, Side::B)] {
            if let Some(r) = check_current(fs, path, sig, side, true)? {
                return Ok(ContentVerdict::Rejected(r));
            }
        }
        Ok(match outcome {
            CompareOutcome::Identical => ContentVerdict::Identical {
                compared_at: clock.now(),
                write_check,
            },
            CompareOutcome::Differs { offset } => {
                ContentVerdict::Rejected(Rejection::ContentDiffers { offset })
            }
            CompareOutcome::Changed => ContentVerdict::Rejected(Rejection::ChangedDuring),
        })
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    //! In-memory fakes of the filesystem ports for unit tests.
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use removarr_domain::error::{ErrorClass, PortError};
    use removarr_domain::fs::{
        CompareOutcome, DirSnapshot, EntryKind, EntrySignature, PermissionFinding, RootInventory,
        SampleSpec,
    };
    use removarr_domain::path::{RootId, RootedPath};
    use removarr_domain::ports::{Clock, ContentReader, FsInventory, ScanControl};
    use removarr_domain::time::{MtimeNs, Timestamp};

    pub fn sig(ino: u64, size: u64) -> EntrySignature {
        EntrySignature {
            kind: EntryKind::File,
            dev: 1,
            ino,
            size,
            nlink: 1,
            mtime_ns: MtimeNs(1),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        }
    }

    pub struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp(42)
        }
    }

    type Hook = Box<dyn Fn(&FakeFs) + Send + Sync>;

    /// Stat table plus a scripted reader.
    #[derive(Default)]
    pub struct FakeFs {
        pub stats: Mutex<BTreeMap<RootedPath, EntrySignature>>,
        pub samples: Mutex<Option<Result<bool, PortError>>>,
        pub compare: Mutex<Option<Result<CompareOutcome, PortError>>>,
        pub sample_calls: AtomicUsize,
        pub compare_calls: AtomicUsize,
        /// Runs inside `full_compare`, before it returns (simulates a concurrent writer).
        pub during_compare: Mutex<Option<Hook>>,
    }

    impl std::fmt::Debug for FakeFs {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("FakeFs")
        }
    }

    impl FakeFs {
        pub fn set(&self, p: &RootedPath, s: EntrySignature) {
            self.stats.lock().unwrap().insert(p.clone(), s);
        }
        pub fn remove(&self, p: &RootedPath) {
            self.stats.lock().unwrap().remove(p);
        }
        pub fn reads(&self) -> usize {
            self.sample_calls.load(Ordering::SeqCst) + self.compare_calls.load(Ordering::SeqCst)
        }
    }

    impl FsInventory for FakeFs {
        fn scan_root(&self, _: &RootId, _: &ScanControl) -> Result<RootInventory, PortError> {
            Err(PortError::new(ErrorClass::Internal, "not used"))
        }
        fn stat(&self, path: &RootedPath) -> Result<Option<EntrySignature>, PortError> {
            Ok(self.stats.lock().unwrap().get(path).copied())
        }
        fn snapshot_dir(&self, _: &RootedPath) -> Result<DirSnapshot, PortError> {
            Err(PortError::new(ErrorClass::Internal, "not used"))
        }
        fn free_space(&self, _: &RootId) -> Result<u64, PortError> {
            Ok(0)
        }
        fn permission_findings(&self, _: &RootInventory, _: (u32, u32)) -> Vec<PermissionFinding> {
            vec![]
        }
    }

    impl ContentReader for FakeFs {
        fn samples_match(
            &self,
            _: &RootedPath,
            _: &RootedPath,
            _: [&EntrySignature; 2],
            _: SampleSpec,
            _: &ScanControl,
        ) -> Result<bool, PortError> {
            self.sample_calls.fetch_add(1, Ordering::SeqCst);
            self.samples.lock().unwrap().clone().unwrap_or(Ok(true))
        }
        fn full_compare(
            &self,
            _: &RootedPath,
            _: &RootedPath,
            _: [&EntrySignature; 2],
            _: u32,
            _: &ScanControl,
        ) -> Result<CompareOutcome, PortError> {
            self.compare_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(h) = self.during_compare.lock().unwrap().as_ref() {
                h(self);
            }
            self.compare
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(CompareOutcome::Identical))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use removarr_domain::time::MtimeNs;

    fn rp(root: &str, rel: &str) -> RootedPath {
        RootedPath::parse(root, rel).unwrap()
    }

    struct Setup {
        fs: FakeFs,
        a: RootedPath,
        b: RootedPath,
        sa: EntrySignature,
        sb: EntrySignature,
    }

    fn setup() -> Setup {
        let fs = FakeFs::default();
        let a = rp("completed", "Movie.2024.mkv");
        let b = rp("media-0123abcd", "films/Movie.2024.mkv");
        let (sa, sb) = (sig(1, 4096), sig(2, 4096));
        fs.set(&a, sa);
        fs.set(&b, sb);
        Setup { fs, a, b, sa, sb }
    }

    fn verify_with(s: &Setup, gate: &dyn PreCompareGate) -> ContentVerdict {
        let v = ContentVerifier::new(ReadLimiter::new(1), 65536);
        let req = VerifyRequest {
            a: &s.a,
            b: &s.b,
            sig_a: &s.sa,
            sig_b: &s.sb,
        };
        v.verify(
            &s.fs,
            &s.fs,
            gate,
            req,
            &ScanControl::default(),
            &FixedClock,
        )
        .unwrap()
    }

    struct Refuse;
    impl PreCompareGate for Refuse {
        fn check(&self, _: &RootedPath, _: &RootedPath) -> GateOutcome {
            GateOutcome::Refused {
                detail: "EACCES on temporary link".into(),
            }
        }
    }
    struct Allow;
    impl PreCompareGate for Allow {
        fn check(&self, _: &RootedPath, _: &RootedPath) -> GateOutcome {
            GateOutcome::Writable
        }
    }

    #[test]
    fn identical_only_after_full_compare_and_records_the_write_check() {
        let s = setup();
        let v = verify_with(&s, &Allow);
        assert_eq!(
            v,
            ContentVerdict::Identical {
                compared_at: Timestamp(42),
                write_check: WriteCheck::Passed
            }
        );
        assert!(v.to_content_proof().proves_equal());
        let ro = verify_with(&s, &ReadOnlyGate);
        assert!(matches!(
            ro,
            ContentVerdict::Identical {
                write_check: WriteCheck::NotTested { .. },
                ..
            }
        ));
    }

    /// T-14 analogue: the write test precedes the full comparison and a refusal reads nothing.
    #[test]
    fn refused_write_test_reads_no_content() {
        let s = setup();
        let v = verify_with(&s, &Refuse);
        assert!(matches!(
            v,
            ContentVerdict::Rejected(Rejection::TargetNotWritable { .. })
        ));
        assert_eq!(s.fs.reads(), 0);
        assert_eq!(v.to_content_proof(), ContentProof::None);
    }

    #[test]
    fn late_difference_is_reported_with_its_offset() {
        let s = setup();
        *s.fs.compare.lock().unwrap() = Some(Ok(CompareOutcome::Differs { offset: 4095 }));
        let v = verify_with(&s, &Allow);
        assert_eq!(
            v,
            ContentVerdict::Rejected(Rejection::ContentDiffers { offset: 4095 })
        );
        assert_eq!(v.to_content_proof(), ContentProof::Differs { offset: 4095 });
    }

    #[test]
    fn change_before_reading_is_detected_without_reading() {
        let s = setup();
        let mut changed = s.sa;
        changed.mtime_ns = MtimeNs(99);
        s.fs.set(&s.a, changed);
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::ChangedBefore { side: Side::A })
        );
        s.fs.set(&s.a, s.sa);
        s.fs.remove(&s.b);
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::Missing { side: Side::B })
        );
        assert_eq!(s.fs.reads(), 0);
    }

    #[test]
    fn change_during_reading_voids_the_result() {
        let s = setup();
        *s.fs.compare.lock().unwrap() = Some(Ok(CompareOutcome::Changed));
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::ChangedDuring)
        );
        *s.fs.compare.lock().unwrap() = Some(Err(PortError::new(ErrorClass::StateChanged, "x")));
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::ChangedDuring)
        );
    }

    #[test]
    fn change_after_reading_voids_an_identical_result() {
        let s = setup();
        let (b, sb) = (s.b.clone(), s.sb);
        *s.fs.during_compare.lock().unwrap() = Some(Box::new(move |fs: &FakeFs| {
            let mut grown = sb;
            grown.size += 1;
            fs.set(&b, grown);
        }));
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::ChangedAfter { side: Side::B })
        );
    }

    #[test]
    fn hardlinks_sizes_and_kinds_short_circuit() {
        let s = setup();
        let mut same = s.sa;
        same.nlink = 2;
        s.fs.set(&s.b, same);
        let req = VerifyRequest {
            a: &s.a,
            b: &s.b,
            sig_a: &s.sa,
            sig_b: &same,
        };
        let v = ContentVerifier::new(ReadLimiter::new(1), 1)
            .verify(
                &s.fs,
                &s.fs,
                &Allow,
                req,
                &ScanControl::default(),
                &FixedClock,
            )
            .unwrap();
        assert_eq!(v, ContentVerdict::SamePhysical);

        let s = setup();
        let mut link = s.sb;
        link.kind = EntryKind::Symlink;
        s.fs.set(&s.b, link);
        assert_eq!(
            verify_with(&s, &Allow),
            ContentVerdict::Rejected(Rejection::NotRegularFile { side: Side::B })
        );
        assert_eq!(s.fs.reads(), 0);
    }

    #[test]
    fn cancellation_is_an_error_not_a_verdict() {
        let s = setup();
        let ctl = ScanControl::default();
        ctl.cancel.cancel();
        let req = VerifyRequest {
            a: &s.a,
            b: &s.b,
            sig_a: &s.sa,
            sig_b: &s.sb,
        };
        let v = ContentVerifier::new(ReadLimiter::new(1), 1);
        let e = v
            .verify(&s.fs, &s.fs, &Allow, req, &ctl, &FixedClock)
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Cancelled);
        assert_eq!(s.fs.reads(), 0);
    }

    #[test]
    fn samples_only_reject() {
        let s = setup();
        let v = ContentVerifier::new(ReadLimiter::new(1), 1);
        let spec = SampleSpec { size: 64, count: 3 };
        let req = VerifyRequest {
            a: &s.a,
            b: &s.b,
            sig_a: &s.sa,
            sig_b: &s.sb,
        };
        let ctl = ScanControl::default();
        assert_eq!(
            v.sample_check(&s.fs, req, spec, &ctl).unwrap(),
            SampleVerdict::NotRejected
        );
        *s.fs.samples.lock().unwrap() = Some(Ok(false));
        assert_eq!(
            v.sample_check(&s.fs, req, spec, &ctl).unwrap(),
            SampleVerdict::Rejected(Rejection::SamplesDiffer)
        );
        let small = sig(3, 10);
        let req2 = VerifyRequest {
            sig_b: &small,
            ..req
        };
        let before = s.fs.reads();
        assert!(matches!(
            v.sample_check(&s.fs, req2, spec, &ctl).unwrap(),
            SampleVerdict::Rejected(Rejection::SizeDiffers { .. })
        ));
        assert_eq!(s.fs.reads(), before, "different sizes read nothing");
    }
}
