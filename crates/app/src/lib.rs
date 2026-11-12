//! Composition root of removarr: configuration, logging, wiring of every adapter and use
//! case, the job runtime and the HTTP server (architecture 2.1, 6, 8.2, 10.2).
//!
//! - [`wiring::build`]: the only place mutating adapters are constructed, and only from the
//!   `LiveMode` token (none in a dry-run process).
//! - [`services`]: production implementations of the web interfaces and job-runner inputs.
//! - [`serve`], [`cli`]: process entry points (`serve`, `healthcheck`).

pub mod cli;
pub mod logging;
pub mod serve;
pub mod services;
pub mod system;
pub mod wiring;
