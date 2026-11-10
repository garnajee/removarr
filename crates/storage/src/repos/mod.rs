//! One repository per port. They are separate types because several ports share method names
//! (`get`), and each is cheap to clone (pools are reference counted).

mod gains;
mod ignores;
mod issues;
mod jobs;
mod journal;
mod matches;
mod plans;
mod scan;
mod snapshot;
mod state;

pub use gains::GainRepo;
pub use ignores::IgnoreRepo;
pub use issues::IssueRepo;
pub use jobs::{Enqueued, JobRepo};
pub use journal::JournalRepo;
pub use matches::MatchRepo;
pub use plans::{ActionRow, PlanRepo};
pub use scan::ScanRepo;
pub use snapshot::{SnapshotRepo, arr_media_key};
pub use state::StateRepo;
