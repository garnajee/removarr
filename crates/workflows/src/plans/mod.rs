//! Plan and approve (architecture 7.3, agent 12). Plans are built by one pure function for
//! unit and bulk selections, manual and automatic origins, previews and persisted plans; they
//! are hashed, stored immutably and approved explicitly. Nothing here executes a mutation: the
//! action runner (13) receives an [`ExecutionTicket`].
//!
//! Rules enforced here:
//! - no implicit selection; unknown, stale, ignored or wrong-category ids reject the request;
//!   blocked items are excluded and listed with their blockers;
//! - every action carries evidence, preconditions captured from the stored observation and
//!   verification steps; plans never adapt to a changed world (they expire or are invalidated);
//! - a folder deletion is all-or-nothing and its snapshot lists every annex;
//! - repairs go from a completed source to a media target only;
//! - a release replacement needs ARR's approval, a complete coverage proof and recoverable old
//!   coverage; no deletion targets a sample without acknowledgement, a protected zone or an
//!   incompletely scanned area;
//! - every action is tagged with its risk tier and gated by the capability flags and the
//!   implemented tier; automatic plans satisfy every automatic rule (A1-A9).

mod arrplans;
mod auto;
pub(crate) mod build;
mod expiry;
mod facts;
mod fsplans;
pub mod gate;
mod input;
mod planner;
mod preview;
mod request;
mod service;
mod validate;

pub use arrplans::{decision as preview_decision, is_not_upgrade_reason};
pub use auto::{AutoContext, AutoPhase, auto_requests};
pub use build::{BuildContext, Built, ExpectedGain, build_plan};
pub use expiry::{ArrFingerprints, StaleReason, staleness, verdict};
pub use input::{MediaKey, PlanInput};
pub use planner::{
    ArrNeeds, AutoOutcome, AutoPlan, ExecutionTicket, PlanCheck, PlanPorts, Planner, StoredPlan,
    arr_needs,
};
pub use preview::{ActionPreview, PlanPreview, is_irreversible, preview};
pub use request::{
    ALL_CAPABILITIES, IssueAction, MediaAction, PlanError, PlanRequest, PlanSettings,
    SelectionFault,
};
pub use validate::check_plan;
