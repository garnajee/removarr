//! Use cases: observe, evaluate, plan, execute and verify.
//!
//! This crate depends only on the domain (ports and pure rules) and on `removarr-matching`
//! (architecture 2.2). Wave 7 (issue-engine) owns [`observe`] (scan session and refresh) and
//! [`issues`] (classification, explanation, aggregation). Neither performs an action: they read
//! observations and write only the issue set. Wave 8 (plan-engine) owns [`evaluate`]
//! (read-only ARR, ffprobe and full-compare evidence) and [`plans`] (planning, approval,
//! expiry); neither executes a mutation. Wave 9 (action-runner) owns [`actions`]: the only
//! module that reaches a mutating adapter, always behind a durable intent.

pub mod actions;
pub mod evaluate;
pub mod issues;
pub mod observe;
pub mod plans;
