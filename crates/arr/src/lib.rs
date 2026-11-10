//! Sonarr and Radarr v3 REST connectors.
//!
//! - [`ArrReader`] implements `ArrRead`: library, episodes (with or without files), queue,
//!   history, quality snapshots, manual-import previews, interactive release searches,
//!   command status and media management settings. It can only issue GET requests.
//! - [`ArrCommander`] implements `ArrCommands`. It can only be built with a `LiveMode` token,
//!   and each mutator checks its capability flag before sending anything.
//! - [`snapshot`] detects quality snapshot changes; [`correlate`] maps download hashes and
//!   paths to ARR media. Both are pure over domain types.
//!
//! ARR is the only quality authority: this crate reports ARR's profiles, Custom Format
//! scores, cutoffs and accept/reject verdicts verbatim and never ranks codecs, groups or
//! resolutions itself (INV-11).

mod commands;
mod dto;
mod error;
mod http;
mod read;
mod time;

pub mod correlate;
pub mod snapshot;

pub use commands::ArrCommander;
pub use error::ArrError;
pub use http::ArrSettings;
pub use read::{ArrReader, release_token};
