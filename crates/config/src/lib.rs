//! Configuration loading, validation, secrets and the live-mode token.
//!
//! Contract: `docs/architecture.md` section 11 and `docs/handoffs/05-config-secrets.md`.

mod error;
mod file;
mod lexpath;
mod live;
mod load;
mod mapping;
mod model;
mod proxy;
mod redact;
mod roots;
mod secret;

pub use error::{ConfigError, EXIT_CODE_CONFIG, Problem, ProblemKind};
pub use lexpath::{PathReject, normalize_abs};
pub use live::LiveMode;
pub use load::{DEFAULT_CONFIG_FILE, ENV_CONFIG_FILE, Env};
pub use mapping::{MapRule, PathMapper};
pub use model::{
    AnchorUtc, ApiService, AuthConfig, Capabilities, Config, FileRules, Limits, LogLevel,
    MatchingConfig, Mode, Owner, QbitService, ScanConfig, ServerConfig, Services, Thresholds,
};
pub use proxy::TrustedProxies;
pub use redact::{
    is_sensitive_header, is_sensitive_key, redact_header, redact_text, redact_tracker_url,
    redact_url,
};
pub use roots::{Root, RootPaths, Roots};
pub use secret::{REDACTED, Secret};
