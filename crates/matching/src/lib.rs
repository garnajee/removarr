//! Media matching and content proof (agent 07).
//!
//! Staged funnel (CONTEXTE §3.3, architecture 7.1 step 6): physical identity -> normalized
//! name + size -> scene/P2P parsing and a bounded semantic ranking -> samples (reject only).
//! Full byte comparison ([`proof::ContentVerifier::verify`]) is the only path to "identical".
//! Local replacement checks ([`checks`]), ffprobe ([`ffprobe`]) and numbering ([`numbering`])
//! produce explanatory evidence and blockers. Nothing here ranks quality (ARR is the sole
//! authority), plans or executes an action.

pub mod checks;
pub mod ffprobe;
pub mod funnel;
pub mod language;
pub mod limiter;
pub mod normalize;
pub mod numbering;
pub mod parse;
pub mod proof;
pub mod reject;
pub mod similarity;

pub use checks::{DurationTolerance, MediaFacts, ReplacementAssessment, assess_replacement};
pub use ffprobe::{FfprobeProbe, FfprobeSettings};
pub use funnel::{FunnelInput, FunnelOutput, FunnelPair, MatchSettings, run_funnel};
pub use limiter::ReadLimiter;
pub use parse::{EpisodeNumbers, ParsedName, parse_scene_name};
pub use proof::{
    ContentVerdict, ContentVerifier, GateOutcome, HardlinkProbeGate, PreCompareGate, ReadOnlyGate,
    VerifyRequest,
};
pub use reject::{Rejection, Side};
