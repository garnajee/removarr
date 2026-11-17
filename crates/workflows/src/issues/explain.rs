//! Explanation of an issue: observed facts, candidates, proven matches and a recommendation,
//! kept apart (MASTER 5, INV-12). Structured codes, not prose: the web layer translates them
//! (i18n). Nothing here performs or schedules an action.

use removarr_domain::evidence::{
    Completeness, ContentProof, EvidenceKind, EvidenceLevel, IdentityProof, Provenance,
    combined_completeness,
};
use removarr_domain::fs::{EntryKind, PhysicalId};
use removarr_domain::ids::IssueId;
use removarr_domain::issue::{
    Blocker, BlockerCode, Issue, IssueCategory, Outcome, Severity, has_refusal,
};
use removarr_domain::path::RootedPath;
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use serde::{Deserialize, Serialize};

use super::aggregate::is_ignored;
use super::ctx::NOTE_PREFIX;

/// A fact read from the filesystem, qBittorrent, ARR or a service status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Fact {
    Entry {
        path: RootedPath,
        kind: EntryKind,
        size: u64,
        nlink: u64,
        uid: u32,
        gid: u32,
        mode: u32,
    },
    TorrentHolds {
        hash: String,
        state: String,
        progress: f64,
        path: RootedPath,
    },
    ServiceUnavailable {
        service: ServiceKind,
        reason: String,
    },
    /// Two paths were observed on different inodes.
    DistinctInodes { a: PhysicalId, b: PhysicalId },
    /// A short stable code, for example `no_torrent_holds_path` (the `note:` prefix removed).
    Note(String),
}

/// A pair that might be the same content or the same media. Never a proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CandidateFact {
    Content {
        a: RootedPath,
        b: RootedPath,
        level: EvidenceLevel,
        score_milli: Option<u32>,
    },
    Identity {
        subject: String,
        score_milli: u32,
    },
}

/// What is established beyond doubt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProvenFact {
    /// Same `(dev, ino)`: a real hardlink.
    SameInode { phys: PhysicalId },
    /// A full byte comparison found no difference.
    ContentIdentical {
        a: RootedPath,
        b: RootedPath,
        compared_at: Timestamp,
    },
    /// A full byte comparison found a difference.
    ContentDiffers {
        a: RootedPath,
        b: RootedPath,
        offset: u64,
    },
    /// ARR identified the media.
    IdentityByArr { subject: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationKind {
    /// A full byte comparison already proved the copy: consolidation can be planned.
    ConsolidateProven,
    /// Same-looking content, not yet proven: run VerifyContent before planning.
    VerifyContentFirst,
    /// Protected sample: keep unless acknowledged one by one.
    KeepSample,
    /// Needs a human decision (ARR identity or quality unknown, no replacement, protected).
    ReviewManually,
    /// A deletion may be proposed under completed, after review and an approved plan.
    ManualDeletionReview,
    /// A permission problem to fix by a targeted approved plan or outside the application.
    FixPermission,
    /// The ARR queue item looks stale: the inode check proved the import.
    QueueItemLooksStale,
    /// Informational only: never actionable.
    InformationOnly,
    /// On the ignore list: excluded from lists, bulk actions and automatic mode.
    Ignored,
}

/// A recommendation is advice. `actionable` is false whenever any blocker refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub kind: RecommendationKind,
    pub actionable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    pub issue: IssueId,
    pub category: IssueCategory,
    pub severity: Severity,
    pub observed: Vec<Fact>,
    pub candidates: Vec<CandidateFact>,
    pub proven: Vec<ProvenFact>,
    pub recommendation: Recommendation,
    pub blockers: Vec<Blocker>,
    /// Strongest content-proof level present (`None` for issues without a content question).
    pub evidence_level: EvidenceLevel,
    pub completeness: Completeness,
    /// Distinct provenances of the evidence.
    pub provenance: Vec<Provenance>,
    /// Latest observation instant among the evidence (falls back to `first_seen_at`).
    pub observed_at: Timestamp,
    pub age_days: u64,
    pub first_seen_at: Timestamp,
}

/// Strongest content-proof level among the evidence.
#[must_use]
pub fn evidence_level(issue: &Issue) -> EvidenceLevel {
    issue
        .evidence
        .iter()
        .filter_map(|e| match &e.kind {
            EvidenceKind::Content { proof, .. } => Some(proof.level()),
            _ => None,
        })
        .max()
        .unwrap_or(EvidenceLevel::None)
}

#[must_use]
pub fn explain(issue: &Issue, now: Timestamp) -> Explanation {
    let (mut observed, mut candidates, mut proven) = (Vec::new(), Vec::new(), Vec::new());
    let mut provenance: Vec<Provenance> = Vec::new();
    for e in &issue.evidence {
        if !provenance.contains(&e.provenance) {
            provenance.push(e.provenance.clone());
        }
        match &e.kind {
            EvidenceKind::Physical { path, sig } => observed.push(Fact::Entry {
                path: path.clone(),
                kind: sig.kind,
                size: sig.size,
                nlink: sig.nlink,
                uid: sig.uid,
                gid: sig.gid,
                mode: sig.mode,
            }),
            EvidenceKind::SamePhysical { a, b } if a == b => {
                proven.push(ProvenFact::SameInode { phys: *a });
            }
            EvidenceKind::SamePhysical { a, b } => {
                observed.push(Fact::DistinctInodes { a: *a, b: *b });
            }
            EvidenceKind::Content { a, b, proof } => match proof {
                ContentProof::Identical { compared_at } => {
                    proven.push(ProvenFact::ContentIdentical {
                        a: a.clone(),
                        b: b.clone(),
                        compared_at: *compared_at,
                    })
                }
                ContentProof::Differs { offset } => proven.push(ProvenFact::ContentDiffers {
                    a: a.clone(),
                    b: b.clone(),
                    offset: *offset,
                }),
                ContentProof::None => {}
                other => candidates.push(CandidateFact::Content {
                    a: a.clone(),
                    b: b.clone(),
                    level: other.level(),
                    score_milli: match other {
                        ContentProof::Semantic { score_milli } => Some(*score_milli),
                        _ => None,
                    },
                }),
            },
            EvidenceKind::Identity { subject, proof } => {
                if let Some(code) = subject.strip_prefix(NOTE_PREFIX) {
                    observed.push(Fact::Note(code.to_string()));
                } else {
                    match proof {
                        IdentityProof::ArrMatched => proven.push(ProvenFact::IdentityByArr {
                            subject: subject.clone(),
                        }),
                        IdentityProof::NameParse { score_milli } => {
                            candidates.push(CandidateFact::Identity {
                                subject: subject.clone(),
                                score_milli: *score_milli,
                            });
                        }
                        IdentityProof::None | IdentityProof::Ambiguous => {}
                    }
                }
            }
            EvidenceKind::Quality { .. } => {}
            EvidenceKind::TorrentHolds {
                hash,
                state,
                progress,
                path,
            } => observed.push(Fact::TorrentHolds {
                hash: hash.as_str().to_string(),
                state: state.as_str().to_string(),
                progress: *progress,
                path: path.clone(),
            }),
            EvidenceKind::ServiceUnavailable { service, reason } => {
                observed.push(Fact::ServiceUnavailable {
                    service: *service,
                    reason: reason.clone(),
                });
            }
        }
    }
    let observed_at = issue
        .evidence
        .iter()
        .map(|e| e.observed_at)
        .max()
        .unwrap_or(issue.first_seen_at);
    Explanation {
        issue: issue.id.clone(),
        category: issue.category,
        severity: issue.severity,
        recommendation: recommend(issue, &proven),
        blockers: issue.blockers.clone(),
        evidence_level: evidence_level(issue),
        completeness: combined_completeness(&issue.evidence),
        observed,
        candidates,
        proven,
        provenance,
        observed_at,
        age_days: issue.age_basis.age_days(now),
        first_seen_at: issue.first_seen_at,
    }
}

fn recommend(issue: &Issue, proven: &[ProvenFact]) -> Recommendation {
    use RecommendationKind as K;
    let refused = has_refusal(&issue.blockers);
    let kind = if is_ignored(issue) {
        K::Ignored
    } else {
        match issue.category {
            IssueCategory::HardlinkMissing | IssueCategory::Duplicate => match issue.outcome {
                Outcome::KeepSample => K::KeepSample,
                Outcome::Consolidate => {
                    if proven
                        .iter()
                        .any(|p| matches!(p, ProvenFact::ContentIdentical { .. }))
                    {
                        K::ConsolidateProven
                    } else {
                        K::VerifyContentFirst
                    }
                }
                Outcome::KeepManual | Outcome::DeleteReplaced => K::ReviewManually,
            },
            IssueCategory::OrphanCompleted
            | IssueCategory::Junk
            | IssueCategory::IncompleteOrphan
            | IssueCategory::IncompleteStalled => K::ManualDeletionReview,
            IssueCategory::Permission => K::FixPermission,
            IssueCategory::ArrQueue => {
                if issue
                    .blockers
                    .iter()
                    .any(|b| b.code == BlockerCode::QueueItemHidesError)
                {
                    K::ReviewManually
                } else {
                    K::QueueItemLooksStale
                }
            }
            IssueCategory::MediaWithoutSource
            | IssueCategory::TorrentMissingFiles
            | IssueCategory::TorrentProtected
            | IssueCategory::UnmappedTorrent
            | IssueCategory::Symlink => K::InformationOnly,
        }
    };
    let never = matches!(kind, K::Ignored | K::InformationOnly | K::KeepSample);
    Recommendation {
        kind,
        actionable: !refused && !never,
    }
}
