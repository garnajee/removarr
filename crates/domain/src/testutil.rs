//! Test builders shared by the unit tests of this crate.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use crate::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, EvidenceLevel, Provenance,
};
use crate::fs::{EntryKind, EntrySignature};
use crate::gain::{GainEstimate, PlanGains};
use crate::ids::{IssueId, PlanId, SessionId, SubjectKey};
use crate::issue::{Issue, IssueCategory, Outcome, Severity, Unit};
use crate::path::{RootId, RootedPath};
use crate::plan::{
    ActionKind, ActionPlan, ActionTarget, PlanHash, PlanMode, PlanOrigin, PlannedAction,
};
use crate::time::{MtimeNs, Timestamp};

pub(crate) fn p(root: &str, rel: &str) -> RootedPath {
    RootedPath::parse(root, rel).unwrap()
}

pub(crate) fn sig(dev: u64, ino: u64, size: u64, nlink: u64) -> EntrySignature {
    EntrySignature {
        kind: EntryKind::File,
        dev,
        ino,
        size,
        nlink,
        mtime_ns: MtimeNs(1_000),
        mode: 0o644,
        uid: 1000,
        gid: 1000,
    }
}

pub(crate) fn rooted_hash(s: &str) -> PlanHash {
    PlanHash(crate::canonical::sha256_hex(s.as_bytes()))
}

/// An action with complete, unknown-free defaults (`Sampled`, `Complete`).
pub(crate) fn action(index: u32, kind: ActionKind, target: ActionTarget) -> PlannedAction {
    PlannedAction {
        index,
        kind,
        target,
        preconditions: vec![],
        verification: vec![],
        expected_gain: GainEstimate::default(),
        depends_on: vec![],
        torrent_hashes: BTreeSet::new(),
        evidence_level: EvidenceLevel::Sampled,
        completeness: Completeness::Complete,
    }
}

/// A draft user plan covering the `completed` and `media-00000000` roots, valid for 24 hours.
pub(crate) fn plan(actions: Vec<PlannedAction>) -> ActionPlan {
    ActionPlan {
        id: PlanId::new(Uuid::from_u128(1)),
        created_at: Timestamp(1_000_000),
        expires_at: Timestamp(1_000_000).plus_hours(24),
        mode: PlanMode::Live,
        origin: PlanOrigin::User,
        session: SessionId::new(Uuid::from_u128(2)),
        config_fingerprint: "cfg".into(),
        actions,
        excluded: vec![],
        lock_roots: BTreeSet::from([RootId::completed(), RootId::new("media-00000000").unwrap()]),
        torrent_states: BTreeMap::new(),
        arr_snapshots: vec![],
        samples: vec![],
        gains: PlanGains::default(),
    }
}

/// An unblocked issue with complete evidence.
pub(crate) fn issue() -> Issue {
    let subject = SubjectKey("p:completed:a.mkv".into());
    Issue {
        id: IssueId::derive(IssueCategory::HardlinkMissing, &subject),
        category: IssueCategory::HardlinkMissing,
        subject,
        unit: Unit::File(p("completed", "a.mkv")),
        severity: Severity::Warning,
        outcome: Outcome::Consolidate,
        evidence: vec![Evidence {
            kind: EvidenceKind::Content {
                a: p("completed", "a.mkv"),
                b: p("media-00000000", "a.mkv"),
                proof: ContentProof::Sampled,
            },
            provenance: Provenance::Computed,
            observed_at: Timestamp(0),
            completeness: Completeness::Complete,
        }],
        blockers: vec![],
        torrent_hashes: BTreeSet::new(),
        logical_bytes: 10,
        potential_bytes: 10,
        age_basis: Timestamp(0),
        first_seen_at: Timestamp(0),
        session: SessionId::new(Uuid::from_u128(2)),
        physical_ids: BTreeSet::new(),
    }
}
