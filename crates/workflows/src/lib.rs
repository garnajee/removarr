//! Use cases: observe, evaluate, plan, execute and verify.
//!
//! This crate depends only on the domain (ports and pure rules) and on `removarr-matching`
//! (architecture 2.2). Wave 7 (issue-engine) owns [`observe`] (scan session and refresh) and
//! [`issues`] (classification, explanation, aggregation). Neither performs an action: they read
//! observations and write only the issue set.

pub mod issues;
pub mod observe;
