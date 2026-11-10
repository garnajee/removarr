//! Typed rejection reasons of the content funnel and the content proof.

use removarr_domain::error::ErrorClass;
use removarr_domain::issue::BlockerCode;
use serde::{Deserialize, Serialize};

/// Which file of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The completed-side (source) file.
    A,
    /// The library-side (target) file.
    B,
}

/// Why a pair is not (or no longer) a content-equality candidate. A rejection never proves the
/// files are different unless it is `SamplesDiffer` or `ContentDiffers`; every other variant means
/// "nothing proven".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// At least one sample window differs: the files are different (no exact offset known).
    SamplesDiffer,
    /// Full comparison found a difference at this byte offset.
    ContentDiffers {
        offset: u64,
    },
    SizeDiffers {
        a: u64,
        b: u64,
    },
    /// The file does not exist any more.
    Missing {
        side: Side,
    },
    /// The path is not a regular file (symlink, directory, other).
    NotRegularFile {
        side: Side,
    },
    /// The signature differs from the one the candidate was built from, before reading.
    ChangedBefore {
        side: Side,
    },
    /// The reader saw a signature change or a short read while reading.
    ChangedDuring,
    /// The signature changed after the read: whatever was read proves nothing.
    ChangedAfter {
        side: Side,
    },
    /// The pre-compare write test of the target directory failed (no content was read).
    TargetNotWritable {
        detail: String,
    },
    /// A read failed for another reason (permission, I/O, confinement).
    ReadFailed {
        class: ErrorClass,
    },
}

impl Rejection {
    /// Stable short code for persistence (`MatchCandidate.reasons`) and logs.
    #[must_use]
    pub fn code(&self) -> String {
        let side = |s: &Side| match s {
            Side::A => "a",
            Side::B => "b",
        };
        match self {
            Rejection::SamplesDiffer => "rejected:samples_differ".into(),
            Rejection::ContentDiffers { offset } => format!("rejected:content_differs@{offset}"),
            Rejection::SizeDiffers { a, b } => format!("rejected:size_differs={a}/{b}"),
            Rejection::Missing { side: s } => format!("rejected:missing_{}", side(s)),
            Rejection::NotRegularFile { side: s } => format!("rejected:not_regular_{}", side(s)),
            Rejection::ChangedBefore { side: s } => format!("rejected:changed_before_{}", side(s)),
            Rejection::ChangedDuring => "rejected:changed_during_read".into(),
            Rejection::ChangedAfter { side: s } => format!("rejected:changed_after_{}", side(s)),
            Rejection::TargetNotWritable { .. } => "rejected:target_not_writable".into(),
            Rejection::ReadFailed { class } => format!("rejected:read_failed:{class:?}"),
        }
    }

    /// True when the rejection proves the two files have different bytes.
    #[must_use]
    pub fn proves_different(&self) -> bool {
        matches!(
            self,
            Rejection::SamplesDiffer
                | Rejection::ContentDiffers { .. }
                | Rejection::SizeDiffers { .. }
        )
    }

    /// Suggested blocker for issue/plan code. `None` means "re-observe": the inputs changed or
    /// vanished, so the pair must be rebuilt from a new scan rather than blocked.
    #[must_use]
    pub fn blocker_code(&self) -> Option<BlockerCode> {
        match self {
            Rejection::SamplesDiffer
            | Rejection::ContentDiffers { .. }
            | Rejection::SizeDiffers { .. } => Some(BlockerCode::ContentDiffers),
            Rejection::TargetNotWritable { .. } => Some(BlockerCode::PermissionDenied),
            Rejection::NotRegularFile { .. } => Some(BlockerCode::Symlink),
            Rejection::ReadFailed { class } => match class {
                ErrorClass::Io(removarr_domain::fs::IoKind::PermissionDenied)
                | ErrorClass::Forbidden => Some(BlockerCode::PermissionDenied),
                _ => Some(BlockerCode::ScanErrorUnder),
            },
            Rejection::Missing { .. }
            | Rejection::ChangedBefore { .. }
            | Rejection::ChangedDuring
            | Rejection::ChangedAfter { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_only_byte_evidence_proves_difference() {
        assert_eq!(Rejection::SamplesDiffer.code(), "rejected:samples_differ");
        assert_eq!(
            Rejection::ContentDiffers { offset: 7 }.code(),
            "rejected:content_differs@7"
        );
        assert_eq!(
            Rejection::ChangedAfter { side: Side::B }.code(),
            "rejected:changed_after_b"
        );
        assert!(Rejection::SamplesDiffer.proves_different());
        assert!(!Rejection::ChangedDuring.proves_different());
        assert!(
            !Rejection::TargetNotWritable {
                detail: String::new()
            }
            .proves_different()
        );
        assert_eq!(Rejection::ChangedDuring.blocker_code(), None);
        assert_eq!(
            Rejection::TargetNotWritable {
                detail: String::new()
            }
            .blocker_code(),
            Some(BlockerCode::PermissionDenied)
        );
    }
}
