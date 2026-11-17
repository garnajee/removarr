//! Issue detection and explanation (wave 7, agent 11). Pure: the classifier reads an
//! [`Observation`] and returns issues; it never touches a filesystem, a service or a database.
//! It performs no action: every issue carries blockers and evidence for `plan-engine` and
//! `reporting` to use, and a recommendation is only advice (see [`explain`]).
//!
//! Rules enforced here (fail-closed, MASTER 2 and 5):
//! - a cancelled, interrupted or failed session produces no issues ([`ClassifyError`]);
//! - an unfinished or erroring scope, an unmapped torrent, an unavailable or unconfigured
//!   service, a protected zone and a non-allowlisted torrent are blockers, never silent gaps;
//! - samples are protected; files under media roots are never deletion candidates;
//! - ignored subjects stay visible with the `Ignored` blocker and leave every total;
//! - issue ids are deterministic: `sha256("issue\0" + category + "\0" + subject)` (domain).

mod aggregate;
mod arrgate;
mod arrqueue;
mod ctx;
mod explain;
mod files;
mod hardlink;
mod incomplete;
mod index;
mod input;
mod library;
mod orphan;
mod perm;
mod scope;
mod torrents;

use std::collections::BTreeSet;

use removarr_domain::ids::IssueId;
use removarr_domain::issue::Issue;
use removarr_domain::session::SessionStatus;

pub use aggregate::{Aggregates, CategoryTotals, InodeLedger, aggregate, is_ignored};
pub use ctx::NOTE_PREFIX;
pub use explain::{
    CandidateFact, Explanation, Fact, ProvenFact, Recommendation, RecommendationKind,
    evidence_level, explain,
};
pub use files::{extension, is_orphan_subtitle, is_release_part_dir, release_dir};
pub use index::{MappingFault, PathField, Protection, TorrentIndex, mapping_faults, protection};
pub use input::{ArrObservation, IssueSettings, Observation, QbitObservation};
pub use perm::{can_modify_dir, permission_findings};
pub use scope::{Perimeter, QbitPerimeter, RootPerimeter, perimeter};

use ctx::Ctx;

/// Why nothing was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyError {
    /// Plans and issues are never built from a session that did not finish (OQ-05).
    #[error("session status {0:?} does not allow classification")]
    SessionNotClassifiable(SessionStatus),
}

/// Result of one classification.
#[derive(Debug, Clone)]
pub struct Classified {
    /// Every issue, ignored ones included (they carry the `Ignored` blocker), sorted by
    /// category and subject. This is what `IssueStore::replace_current` receives.
    pub issues: Vec<Issue>,
    pub perimeter: Perimeter,
    pub aggregates: Aggregates,
    pub ledger: InodeLedger,
}

impl Classified {
    /// Issues for lists, bulk actions and automatic mode: the ignored are excluded.
    pub fn active(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| !is_ignored(i))
    }

    /// The "Ignored" view.
    pub fn ignored(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| is_ignored(i))
    }
}

/// Classifies one observation. Deterministic: the same observation gives the same issues, in
/// the same order, with the same ids.
pub fn classify(obs: &Observation, settings: &IssueSettings) -> Result<Classified, ClassifyError> {
    if !obs.session_status.allows_planning() {
        return Err(ClassifyError::SessionNotClassifiable(obs.session_status));
    }
    let ctx = Ctx::new(obs, settings);
    let mut issues: Vec<Issue> = Vec::new();
    incomplete::stalled(&ctx, &mut issues);
    incomplete::orphans(&ctx, &mut issues);
    hardlink::hardlink_missing(&ctx, &mut issues);
    hardlink::duplicates(&ctx, &mut issues);
    orphan::orphan_completed(&ctx, &mut issues);
    torrents::missing_files(&ctx, &mut issues);
    library::media_without_source(&ctx, &mut issues);
    library::permissions(&ctx, &mut issues);
    orphan::junk(&ctx, &mut issues);
    torrents::protected(&ctx, &mut issues);
    torrents::unmapped(&ctx, &mut issues);
    library::symlinks(&ctx, &mut issues);
    arrqueue::queue(&ctx, &mut issues);

    issues.sort_by(|a, b| (a.category, &a.subject).cmp(&(b.category, &b.subject)));
    // One issue per (category, subject): the id is derived from exactly that pair.
    let mut seen: BTreeSet<IssueId> = BTreeSet::new();
    issues.retain(|i| seen.insert(i.id.clone()));

    let ledger = InodeLedger::from_inventories(&obs.inventories);
    let aggregates = aggregate(&issues, &ledger);
    Ok(Classified {
        issues,
        perimeter: perimeter(obs),
        aggregates,
        ledger,
    })
}
