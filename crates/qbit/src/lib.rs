//! qBittorrent Web API adapters (versions 4.x and 5.x).
//!
//! * [`QbitReader`] implements `QbitRead` and builds [`TorrentSnapshot`]s. It never mutates.
//! * [`QbitController`] implements `QbitControl` and can only be built from a reader plus the
//!   `LiveMode` token. It sends pause/resume to 4.x and stop/start to 5.x, and removes tasks with
//!   a fixed "keep data" flag: no API in this crate can ask qBittorrent to delete files.
//! * [`PathIndex`] maps host paths to every associated torrent hash.
//! * [`classify`], [`decide_removal`] and [`plan_restore`] are the pure safety rules.
//!
//! Contract: `docs/handoffs/08-qbit-api.md`.

mod control;
mod http;
mod index;
mod parse;
mod reader;
mod safety;
mod settings;

pub use control::{QbitController, RecheckWait};
pub use index::PathIndex;
pub use reader::QbitReader;
pub use safety::{
    Eligibility, KeepReason, LeftStoppedReason, Protection, RemovalDecision, RestorePlan, classify,
    decide_removal, plan_restore,
};
pub use settings::{QbitSettings, RetryPolicy};

pub use removarr_domain::torrent::TorrentSnapshot;
