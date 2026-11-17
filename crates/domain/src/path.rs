//! Root-relative paths. Plans, issues and the database never store absolute host paths.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::canonical::sha256_hex;
use crate::error::DomainError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Completed,
    Incomplete,
    Media,
}

/// Identifier of a configured root: `completed`, `incomplete` or `media-<8 hex>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RootId(String);

impl RootId {
    pub const COMPLETED: &'static str = "completed";
    pub const INCOMPLETE: &'static str = "incomplete";

    pub fn new(s: impl Into<String>) -> Result<Self, DomainError> {
        let s = s.into();
        let ok = s == Self::COMPLETED
            || s == Self::INCOMPLETE
            || s.strip_prefix("media-").is_some_and(|h| {
                h.len() == 8
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        if ok {
            Ok(Self(s))
        } else {
            Err(DomainError::InvalidRootId(s))
        }
    }

    #[must_use]
    pub fn completed() -> Self {
        Self(Self::COMPLETED.to_string())
    }

    #[must_use]
    pub fn incomplete() -> Self {
        Self(Self::INCOMPLETE.to_string())
    }

    /// Media root id from its canonical host path: `media-<8 hex of sha256(path)>`.
    #[must_use]
    pub fn media(canonical_path: &str) -> Self {
        Self(format!(
            "media-{}",
            &sha256_hex(canonical_path.as_bytes())[..8]
        ))
    }

    #[must_use]
    pub fn kind(&self) -> RootKind {
        match self.0.as_str() {
            Self::COMPLETED => RootKind::Completed,
            Self::INCOMPLETE => RootKind::Incomplete,
            _ => RootKind::Media,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RootId {
    type Error = DomainError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<RootId> for String {
    fn from(r: RootId) -> String {
        r.0
    }
}

impl fmt::Display for RootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// UTF-8 `/`-separated path relative to a root. Components are non-empty and never `.` or `..`;
/// no NUL, no leading or trailing `/`. The root itself is the empty path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    pub fn new(s: impl Into<String>) -> Result<Self, DomainError> {
        let s = s.into();
        if s.is_empty() {
            return Ok(Self(s));
        }
        if s.contains('\0') || s.starts_with('/') || s.ends_with('/') {
            return Err(DomainError::InvalidRelPath(s));
        }
        if s.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            return Err(DomainError::InvalidRelPath(s));
        }
        Ok(Self(s))
    }

    #[must_use]
    pub fn root() -> Self {
        Self(String::new())
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    #[must_use]
    pub fn parent(&self) -> Option<RelPath> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rfind('/') {
            Some(i) => Self(self.0[..i].to_string()),
            None => Self::root(),
        })
    }

    pub fn join(&self, component: &str) -> Result<RelPath, DomainError> {
        if component.is_empty() || component.contains('/') {
            return Err(DomainError::InvalidRelPath(component.to_string()));
        }
        if self.is_root() {
            Self::new(component)
        } else {
            Self::new(format!("{}/{component}", self.0))
        }
    }

    /// True when `self` is `ancestor` or lies below it.
    #[must_use]
    pub fn starts_with(&self, ancestor: &RelPath) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || self
                .0
                .strip_prefix(&ancestor.0)
                .is_some_and(|r| r.starts_with('/'))
    }
}

impl TryFrom<String> for RelPath {
    type Error = DomainError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<RelPath> for String {
    fn from(p: RelPath) -> String {
        p.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A path inside a configured root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RootedPath {
    pub root: RootId,
    pub rel: RelPath,
}

impl RootedPath {
    #[must_use]
    pub fn new(root: RootId, rel: RelPath) -> Self {
        Self { root, rel }
    }

    pub fn parse(root: &str, rel: &str) -> Result<Self, DomainError> {
        Ok(Self {
            root: RootId::new(root)?,
            rel: RelPath::new(rel)?,
        })
    }
}

impl fmt::Display for RootedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.root, self.rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_validation() {
        for bad in ["/a", "a/", "a//b", "a/./b", "../a", "a/..", "a\0b", "."] {
            assert!(RelPath::new(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["", "a", "a/b c/d.mkv", "..a/b..", "é/ü"] {
            assert!(RelPath::new(good).is_ok(), "{good:?} must be accepted");
        }
        assert!(RelPath::root().is_root());
    }

    #[test]
    fn rel_path_navigation() {
        let p = RelPath::new("a/b/c.mkv").unwrap();
        assert_eq!(p.file_name(), Some("c.mkv"));
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert!(RelPath::new("a").unwrap().parent().unwrap().is_root());
        assert!(p.starts_with(&RelPath::new("a/b").unwrap()));
        assert!(!p.starts_with(&RelPath::new("a/bb").unwrap()));
        assert!(p.starts_with(&RelPath::root()));
        assert!(RelPath::root().join("x").is_ok());
        assert!(p.join("x/y").is_err());
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<RelPath>("\"../x\"").is_err());
        assert!(serde_json::from_str::<RootId>("\"media-zz\"").is_err());
        assert!(serde_json::from_str::<RootId>("\"media-0123abcd\"").is_ok());
    }

    #[test]
    fn root_ids() {
        assert_eq!(RootId::completed().kind(), RootKind::Completed);
        let m = RootId::media("/srv/media/library");
        assert_eq!(m.kind(), RootKind::Media);
        assert_eq!(m, RootId::media("/srv/media/library"));
        assert!(RootId::new("Completed").is_err());
    }
}
