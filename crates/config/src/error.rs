//! Startup diagnostics. A [`Problem`] names a setting, never its value.

use std::fmt;

/// Process exit code used when configuration is invalid (FH-01).
pub const EXIT_CODE_CONFIG: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemKind {
    Missing,
    /// Static or generated reason that never quotes the offending value.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// TOML key (for example `roots.completed`) or file-level label.
    pub key: String,
    /// Environment variable that sets it, when there is one.
    pub env: Option<&'static str>,
    pub kind: ProblemKind,
}

impl Problem {
    #[must_use]
    pub fn missing(key: impl Into<String>, env: Option<&'static str>) -> Self {
        Self {
            key: key.into(),
            env,
            kind: ProblemKind::Missing,
        }
    }

    #[must_use]
    pub fn invalid(
        key: impl Into<String>,
        env: Option<&'static str>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            env,
            kind: ProblemKind::Invalid(reason.into()),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ProblemKind::Missing => write!(f, "missing required setting `{}`", self.key)?,
            ProblemKind::Invalid(r) => write!(f, "invalid setting `{}`: {r}", self.key)?,
        }
        if let Some(env) = self.env {
            write!(f, " (env {env})")?;
        }
        Ok(())
    }
}

/// Every problem found at startup, in a stable order. Display lists one per line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub problems: Vec<Problem>,
}

impl ConfigError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        EXIT_CODE_CONFIG
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid configuration ({} problem(s))",
            self.problems.len()
        )?;
        for p in &self.problems {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}
