//! Axum router, handlers, typed responses, templates and i18n (architecture 12).
//!
//! - [`state`]: the explicit interfaces handlers depend on (`ReadModel`, `JobControl`,
//!   `PlanControl`, `Readiness`, the `Guard` extension point) and [`state::AppState`].
//! - [`router::build`]: the route table and layers. Display routes are `GET` and read only;
//!   commands are `POST` behind the guard (session, CSRF) and become plans or queued jobs.
//! - [`error`]: safe error mapping (status from the error class, translated message,
//!   correlation id; raw text is logged sanitized, never rendered).
//! - [`forms`]: validation of every id, filter, page, search, locale and selection.
//! - [`health`]: liveness and readiness without scans or disk wake-ups.
//! - [`auth`]: the production guard (Argon2 login, sessions, CSRF, rate limit, trusted
//!   proxies, security headers).

pub mod assets;
pub mod auth;
pub mod error;
pub mod forms;
pub mod handlers;
pub mod health;
pub mod i18n;
pub mod router;
pub mod state;

pub use router::build;
pub use state::{
    Access, AppState, Denied, ExcludedItem, FailClosedGuard, Guard, JobControl, PlanControl,
    Principal, ReadModel, Readiness, UseCaseError, WebSettings,
};
