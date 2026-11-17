//! Typed identifiers.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Re-exported so crates without their own `uuid` dependency can name the type.
pub use uuid::Uuid as RawUuid;

use crate::arr::ArrService;
use crate::canonical::sha256_hex;
use crate::fs::PhysicalId;
use crate::issue::IssueCategory;
use crate::path::{RootId, RootedPath};
use crate::torrent::TorrentHash;

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

uuid_id!(/// One audit (scan) session.
    SessionId);
uuid_id!(/// One immutable action plan.
    PlanId);
uuid_id!(/// One persisted job.
    JobId);
uuid_id!(/// One journal intent record.
    IntentId);

/// Position of an action inside a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ActionRef {
    pub plan: PlanId,
    pub index: u32,
}

/// Stable subject of an issue (`t:<hash>`, `p:<root>:<rel>`, `d:<dev>:<ino>-<dev>:<ino>`,
/// `q:<service>:<id>`). Used for ignores and for issue identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubjectKey(pub String);

impl SubjectKey {
    #[must_use]
    pub fn torrent(hash: &TorrentHash) -> Self {
        Self(format!("t:{}", hash.as_str()))
    }

    #[must_use]
    pub fn path(root: &RootId, rel: &crate::path::RelPath) -> Self {
        Self(format!("p:{}:{}", root.as_str(), rel.as_str()))
    }

    #[must_use]
    pub fn rooted(path: &RootedPath) -> Self {
        Self::path(&path.root, &path.rel)
    }

    /// Unordered pair of physical identities; the pair is sorted so `(a, b)` equals `(b, a)`.
    #[must_use]
    pub fn pair(a: PhysicalId, b: PhysicalId) -> Self {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        Self(format!("d:{}:{}-{}:{}", lo.dev, lo.ino, hi.dev, hi.ino))
    }

    #[must_use]
    pub fn queue(service: ArrService, id: i64) -> Self {
        Self(format!("q:{}:{id}", service.as_str()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Issue identity: first 32 hex chars of `sha256("issue\0" + category + "\0" + subject)`.
/// The same (category, subject) always yields the same id across scans.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssueId(pub String);

impl IssueId {
    #[must_use]
    pub fn derive(category: IssueCategory, subject: &SubjectKey) -> Self {
        let input = format!("issue\0{}\0{}", category.as_str(), subject.as_str());
        Self(sha256_hex(input.as_bytes())[..32].to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Unique id of a gain (see [`crate::gain::gain_id`]). The database enforces uniqueness.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GainId(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_id_is_stable_and_category_sensitive() {
        let s = SubjectKey("p:completed:a/b.mkv".into());
        let a = IssueId::derive(IssueCategory::Junk, &s);
        assert_eq!(a, IssueId::derive(IssueCategory::Junk, &s));
        assert_ne!(a, IssueId::derive(IssueCategory::OrphanCompleted, &s));
        assert_eq!(a.as_str().len(), 32);
    }

    #[test]
    fn pair_subject_is_order_independent() {
        let a = PhysicalId { dev: 1, ino: 9 };
        let b = PhysicalId { dev: 1, ino: 3 };
        assert_eq!(SubjectKey::pair(a, b), SubjectKey::pair(b, a));
        assert_eq!(SubjectKey::pair(a, b).as_str(), "d:1:3-1:9");
    }
}
