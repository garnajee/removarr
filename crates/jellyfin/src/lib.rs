//! Jellyfin REST adapters (architecture sections 5 and 6, agent 10).
//!
//! - [`JellyfinReader`] implements `JellyfinRead` (GET only: health, version, libraries,
//!   items and media paths, library scan state).
//! - [`JellyfinCommander`] implements `JellyfinCommands` (library and item refresh). Its
//!   constructor needs a `LiveMode`. Reads and refresh are different types on purpose.
//! - [`wait_for_library_refresh`] bounds the polling of the asynchronous library scan.
//! - [`recognition`] is pure: it reports mismatches, specials and alternate numbering.
//!
//! Jellyfin exposes library state; it never decides release quality. Nothing here touches
//! library files. Errors and logs never contain the token, headers, host or paths.

mod commander;
mod dto;
mod http;
mod poll;
mod reader;
pub mod recognition;

pub use commander::JellyfinCommander;
pub use http::HttpSettings;
pub use poll::{PollConfig, RefreshPoll, wait_for_library_refresh};
pub use reader::{ItemLookup, JellyfinItemDetail, JellyfinReader};
pub use recognition::{
    ExpectedMedia, Finding, RecognitionReport, RecognitionStatus, completeness_of, evaluate,
};
