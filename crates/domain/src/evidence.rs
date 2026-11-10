//! Evidence: proof level, provenance, observation time and completeness.
//!
//! Unknown or partial knowledge is representable (`None`, `Unknown`, `Incomplete`) and is never
//! upgraded by guessing. Content equality, media identity and quality are separate dimensions.

use serde::{Deserialize, Serialize};

use crate::arr::ArrService;
use crate::fs::{EntrySignature, PhysicalId};
use crate::ids::SessionId;
use crate::path::RootedPath;
use crate::session::ServiceKind;
use crate::time::Timestamp;
use crate::torrent::{TorrentHash, TorrentState};

/// Strength of a content-equality proof, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    None,
    NameSize,
    Semantic,
    Sampled,
    /// Full byte comparison done (the result may be equal or different).
    Full,
}

/// Content equality of two files. Only `Identical` proves equal bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentProof {
    None,
    NameSize,
    Semantic { score_milli: u32 },
    Sampled,
    Identical { compared_at: Timestamp },
    Differs { offset: u64 },
}

impl ContentProof {
    #[must_use]
    pub fn level(&self) -> EvidenceLevel {
        match self {
            ContentProof::None => EvidenceLevel::None,
            ContentProof::NameSize => EvidenceLevel::NameSize,
            ContentProof::Semantic { .. } => EvidenceLevel::Semantic,
            ContentProof::Sampled => EvidenceLevel::Sampled,
            ContentProof::Identical { .. } | ContentProof::Differs { .. } => EvidenceLevel::Full,
        }
    }

    /// Only a full comparison that found no difference proves equal content.
    #[must_use]
    pub fn proves_equal(&self) -> bool {
        matches!(self, ContentProof::Identical { .. })
    }

    #[must_use]
    pub fn proves_different(&self) -> bool {
        matches!(self, ContentProof::Differs { .. })
    }
}

/// Media identity (which title/episode this is). Never implies equal content or quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityProof {
    None,
    NameParse { score_milli: u32 },
    ArrMatched,
    Ambiguous,
}

impl IdentityProof {
    /// `None` and `Ambiguous` are manual-decision states.
    #[must_use]
    pub fn is_unambiguous(&self) -> bool {
        matches!(
            self,
            IdentityProof::NameParse { .. } | IdentityProof::ArrMatched
        )
    }
}

/// Quality knowledge. Only ARR provides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityEvidence {
    None,
    ArrSnapshot { fingerprint: String },
}

/// Where an observation came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Filesystem {
        session: SessionId,
    },
    Qbittorrent {
        taken_at: Timestamp,
    },
    Arr {
        service: ArrService,
        captured_at: Timestamp,
    },
    Jellyfin {
        at: Timestamp,
    },
    Ffprobe {
        at: Timestamp,
    },
    Computed,
}

/// How much of the intended observation was actually obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completeness {
    Complete,
    /// Some inputs are missing; `missing` names them (never guessed).
    Incomplete {
        missing: Vec<String>,
    },
    Unknown,
}

impl Completeness {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, Completeness::Complete)
    }

    /// The weakest of two completeness values: `Unknown` < `Incomplete` < `Complete`.
    #[must_use]
    pub fn and(self, other: Completeness) -> Completeness {
        match (self, other) {
            (Completeness::Unknown, _) | (_, Completeness::Unknown) => Completeness::Unknown,
            (
                Completeness::Incomplete { mut missing },
                Completeness::Incomplete { missing: m2 },
            ) => {
                missing.extend(m2);
                Completeness::Incomplete { missing }
            }
            (i @ Completeness::Incomplete { .. }, Completeness::Complete)
            | (Completeness::Complete, i @ Completeness::Incomplete { .. }) => i,
            (Completeness::Complete, Completeness::Complete) => Completeness::Complete,
        }
    }
}

/// Typed evidence payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EvidenceKind {
    Physical {
        path: RootedPath,
        sig: EntrySignature,
    },
    /// Two paths share (or do not share) a physical identity. Never derived from `nlink`.
    SamePhysical { a: PhysicalId, b: PhysicalId },
    Content {
        a: RootedPath,
        b: RootedPath,
        proof: ContentProof,
    },
    Identity {
        subject: String,
        proof: IdentityProof,
    },
    Quality {
        subject: String,
        proof: QualityEvidence,
    },
    TorrentHolds {
        hash: TorrentHash,
        state: TorrentState,
        progress: f64,
        path: RootedPath,
    },
    ServiceUnavailable {
        service: ServiceKind,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub provenance: Provenance,
    pub observed_at: Timestamp,
    pub completeness: Completeness,
}

impl Evidence {
    /// INV-42: filesystem evidence is current only for the session that observed it.
    /// Evidence of other provenance carries its own timestamp and is never "current by default".
    #[must_use]
    pub fn is_from_session(&self, session: SessionId) -> bool {
        matches!(&self.provenance, Provenance::Filesystem { session: s } if *s == session)
    }

    /// Age of the observation in whole days.
    #[must_use]
    pub fn age_days(&self, now: Timestamp) -> u64 {
        self.observed_at.age_days(now)
    }
}

/// Combined completeness of a set of evidence. An empty set is `Unknown`, never `Complete`.
#[must_use]
pub fn combined_completeness(evidence: &[Evidence]) -> Completeness {
    let mut it = evidence.iter().map(|e| e.completeness.clone());
    match it.next() {
        None => Completeness::Unknown,
        Some(first) => it.fold(first, Completeness::and),
    }
}

/// Persisted funnel output for a candidate pair. The proof is valid only while both current
/// inventory signatures equal `sig_a` and `sig_b` (CP1-F07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCandidate {
    pub a: RootedPath,
    pub b: RootedPath,
    pub sig_a: EntrySignature,
    pub sig_b: EntrySignature,
    pub identity: IdentityProof,
    pub content: ContentProof,
    pub reasons: Vec<String>,
    pub session: SessionId,
}

impl MatchCandidate {
    /// True only while both files still carry the signatures the proof was computed against.
    /// `nlink` is ignored: linking one file changes it without changing content.
    #[must_use]
    pub fn proof_still_valid(
        &self,
        current_a: &EntrySignature,
        current_b: &EntrySignature,
    ) -> bool {
        self.sig_a.matches(current_a, false) && self.sig_b.matches(current_b, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{p, sig};

    #[test]
    fn proof_levels_are_ordered_and_only_identical_proves_equality() {
        let proofs = [
            ContentProof::None,
            ContentProof::NameSize,
            ContentProof::Semantic { score_milli: 900 },
            ContentProof::Sampled,
            ContentProof::Identical {
                compared_at: Timestamp(1),
            },
        ];
        for w in proofs.windows(2) {
            assert!(w[0].level() < w[1].level());
        }
        assert!(proofs.iter().filter(|p| p.proves_equal()).count() == 1);
        let d = ContentProof::Differs { offset: 3 };
        assert!(d.proves_different() && !d.proves_equal());
        assert_eq!(d.level(), EvidenceLevel::Full);
    }

    #[test]
    fn identity_ambiguity() {
        assert!(!IdentityProof::None.is_unambiguous());
        assert!(!IdentityProof::Ambiguous.is_unambiguous());
        assert!(IdentityProof::ArrMatched.is_unambiguous());
    }

    #[test]
    fn completeness_takes_the_weakest() {
        use Completeness::*;
        let inc = |s: &str| Incomplete {
            missing: vec![s.into()],
        };
        assert_eq!(Complete.and(Complete), Complete);
        assert_eq!(Complete.and(inc("a")), inc("a"));
        assert_eq!(
            inc("a").and(inc("b")),
            Incomplete {
                missing: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(inc("a").and(Unknown), Unknown);
        assert_eq!(Complete.and(Unknown), Unknown);
    }

    fn ev(c: Completeness) -> Evidence {
        Evidence {
            kind: EvidenceKind::Identity {
                subject: "s".into(),
                proof: IdentityProof::None,
            },
            provenance: Provenance::Computed,
            observed_at: Timestamp(0),
            completeness: c,
        }
    }

    #[test]
    fn empty_evidence_is_unknown_not_complete() {
        assert_eq!(combined_completeness(&[]), Completeness::Unknown);
        assert!(combined_completeness(&[ev(Completeness::Complete)]).is_complete());
        assert!(
            !combined_completeness(&[ev(Completeness::Complete), ev(Completeness::Unknown)])
                .is_complete()
        );
    }

    #[test]
    fn filesystem_evidence_is_tied_to_its_session() {
        let s1 = SessionId::new(uuid::Uuid::from_u128(1));
        let s2 = SessionId::new(uuid::Uuid::from_u128(2));
        let mut e = ev(Completeness::Complete);
        e.provenance = Provenance::Filesystem { session: s1 };
        assert!(e.is_from_session(s1));
        assert!(!e.is_from_session(s2));
        e.provenance = Provenance::Computed;
        assert!(!e.is_from_session(s1));
    }

    #[test]
    fn match_proof_expires_when_a_signature_changes() {
        let m = MatchCandidate {
            a: p("completed", "a.mkv"),
            b: p("media-00000000", "b.mkv"),
            sig_a: sig(1, 1, 100, 1),
            sig_b: sig(1, 2, 100, 1),
            identity: IdentityProof::ArrMatched,
            content: ContentProof::Identical {
                compared_at: Timestamp(1),
            },
            reasons: vec![],
            session: SessionId::new(uuid::Uuid::from_u128(1)),
        };
        assert!(m.proof_still_valid(&sig(1, 1, 100, 2), &sig(1, 2, 100, 1)));
        assert!(!m.proof_still_valid(&sig(1, 1, 101, 1), &sig(1, 2, 100, 1)));
        let mut changed = sig(1, 2, 100, 1);
        changed.mtime_ns = crate::time::MtimeNs(5);
        assert!(!m.proof_still_valid(&sig(1, 1, 100, 1), &changed));
    }
}
