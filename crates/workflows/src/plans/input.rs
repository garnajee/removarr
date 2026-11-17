//! Everything one planning run reads. Plain data: the pure builder performs no I/O, so the same
//! input always gives the same plan (and the same hash for the same id and clock).

use std::collections::BTreeMap;

use removarr_domain::arr::{ArrService, ReleaseSearchResult};
use removarr_domain::error::PortError;
use removarr_domain::issue::Issue;
use removarr_domain::session::AuditSession;

use crate::evaluate::ArrFacts;
use crate::issues::Observation;

/// ARR facts are keyed by `(service, media id)`.
pub type MediaKey = (ArrService, i64);

#[derive(Debug, Clone)]
pub struct PlanInput {
    /// The latest session (must allow planning, OQ-05). Its service states feed the
    /// `ServiceAvailable` preconditions and the auto perimeter.
    pub session: AuditSession,
    /// Stored inventories, the latest torrent snapshot, the in-memory ARR observations, the
    /// stored funnel output and the ignore list: the classifier's own input shape.
    pub observation: Observation,
    /// The selected issues as currently stored.
    pub issues: Vec<Issue>,
    /// Fresh ARR reads for ARR-dependent actions (`DeleteReplaced`, `ReplaceRelease`, grabs).
    pub arr: BTreeMap<MediaKey, Result<ArrFacts, PortError>>,
    /// Fresh interactive search for a grab request (CP1-F06). `None` for other requests.
    pub search: Option<Result<Vec<ReleaseSearchResult>, PortError>>,
}
