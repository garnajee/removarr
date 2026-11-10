//! Domain types, state machines, pure policies and ports.
//!
//! This crate is infrastructure independent: no HTTP, SQLite, templates or operating-system
//! clients. Four concepts are kept apart on purpose and never share a type:
//! physical identity ([`fs::PhysicalId`]), content equality ([`evidence::ContentProof`]),
//! media identity ([`evidence::IdentityProof`], [`arr::MediaIdentity`]) and quality
//! ([`evidence::QualityEvidence`], [`arr::ArrQualitySnapshot`]).

pub mod arr;
pub mod candidate;
pub mod canonical;
pub mod error;
pub mod evidence;
pub mod fs;
pub mod gain;
pub mod ids;
pub mod issue;
pub mod jellyfin;
pub mod jobs;
pub mod journal;
pub mod media;
pub mod path;
pub mod plan;
pub mod policy;
pub mod ports;
pub mod session;
pub mod time;
pub mod torrent;

#[cfg(test)]
pub(crate) mod testutil;
