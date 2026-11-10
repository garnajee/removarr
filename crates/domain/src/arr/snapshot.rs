//! Quality snapshot change detection (INV-34). A plan stores the `ArrQualitySnapshot` it was
//! built from; before execution the snapshot is read again and any difference, a tampered
//! stored fingerprint, or an unreachable ARR invalidates the plan. Nothing here ever reports
//! "unchanged" without a fresh, equal fingerprint.
//!
//! This module depends only on domain types and the `ArrRead` port.

use std::collections::{BTreeMap, BTreeSet};

use crate::arr::{ArrFileRef, ArrQualitySnapshot};
use crate::error::PortError;
use crate::ports::ArrRead;
use serde::{Deserialize, Serialize};

/// One reason a stored snapshot no longer matches ARR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotChange {
    /// The stored snapshot's fingerprint does not match its own content.
    StoredFingerprintInvalid,
    /// The stored snapshot has no ARR media reference to re-read.
    NoMediaRef,
    ServiceChanged,
    MediaChanged,
    ProfileChanged {
        from: (i64, String),
        to: (i64, String),
    },
    CutoffChanged {
        from: String,
        to: String,
    },
    UpgradeAllowedChanged {
        from: bool,
        to: bool,
    },
    MinFormatScoreChanged {
        from: i64,
        to: i64,
    },
    CutoffFormatScoreChanged {
        from: i64,
        to: i64,
    },
    /// A Custom Format score changed, appeared (`from: None`) or disappeared (`to: None`).
    FormatScoreChanged {
        name: String,
        from: Option<i64>,
        to: Option<i64>,
    },
    CurrentFilesChanged {
        added: Vec<i64>,
        removed: Vec<i64>,
        modified: Vec<i64>,
    },
    MediaManagementChanged,
    /// Fingerprints differ for a reason none of the field checks names.
    FingerprintChanged,
}

/// Field-level differences between a stored and a fresh snapshot. Empty only when both
/// fingerprints are valid and equal.
#[must_use]
pub fn detect_changes(
    stored: &ArrQualitySnapshot,
    current: &ArrQualitySnapshot,
) -> Vec<SnapshotChange> {
    use SnapshotChange as C;
    let mut out = Vec::new();
    if !stored.fingerprint_is_valid() {
        out.push(C::StoredFingerprintInvalid);
    }
    if stored.service != current.service {
        out.push(C::ServiceChanged);
    }
    if stored.media.arr != current.media.arr {
        out.push(C::MediaChanged);
    }
    if (stored.profile_id, &stored.profile_name) != (current.profile_id, &current.profile_name) {
        out.push(C::ProfileChanged {
            from: (stored.profile_id, stored.profile_name.clone()),
            to: (current.profile_id, current.profile_name.clone()),
        });
    }
    if stored.cutoff != current.cutoff {
        out.push(C::CutoffChanged {
            from: stored.cutoff.clone(),
            to: current.cutoff.clone(),
        });
    }
    if stored.upgrade_allowed != current.upgrade_allowed {
        out.push(C::UpgradeAllowedChanged {
            from: stored.upgrade_allowed,
            to: current.upgrade_allowed,
        });
    }
    if stored.min_format_score != current.min_format_score {
        out.push(C::MinFormatScoreChanged {
            from: stored.min_format_score,
            to: current.min_format_score,
        });
    }
    if stored.cutoff_format_score != current.cutoff_format_score {
        out.push(C::CutoffFormatScoreChanged {
            from: stored.cutoff_format_score,
            to: current.cutoff_format_score,
        });
    }
    let names: BTreeSet<&String> = stored
        .format_scores
        .keys()
        .chain(current.format_scores.keys())
        .collect();
    for name in names {
        let (a, b) = (
            stored.format_scores.get(name).copied(),
            current.format_scores.get(name).copied(),
        );
        if a != b {
            out.push(C::FormatScoreChanged {
                name: name.clone(),
                from: a,
                to: b,
            });
        }
    }
    if let Some(c) = files_change(&stored.current_files, &current.current_files) {
        out.push(c);
    }
    let mm = |s: &ArrQualitySnapshot| {
        s.media_management.as_ref().map(|m| {
            (
                m.recycle_bin.clone(),
                m.recycle_bin_cleanup_days,
                m.hardlinks_enabled,
            )
        })
    };
    if mm(stored) != mm(current) {
        out.push(C::MediaManagementChanged);
    }
    if out.is_empty() && stored.fingerprint != current.fingerprint {
        out.push(C::FingerprintChanged);
    }
    out
}

fn files_change(a: &[ArrFileRef], b: &[ArrFileRef]) -> Option<SnapshotChange> {
    let ma: BTreeMap<i64, &ArrFileRef> = a.iter().map(|f| (f.file_id, f)).collect();
    let mb: BTreeMap<i64, &ArrFileRef> = b.iter().map(|f| (f.file_id, f)).collect();
    let added: Vec<i64> = mb.keys().filter(|k| !ma.contains_key(k)).copied().collect();
    let removed: Vec<i64> = ma.keys().filter(|k| !mb.contains_key(k)).copied().collect();
    let modified: Vec<i64> = ma
        .iter()
        .filter(|(k, f)| mb.get(k).is_some_and(|g| g != *f))
        .map(|(k, _)| *k)
        .collect();
    (!added.is_empty() || !removed.is_empty() || !modified.is_empty()).then_some(
        SnapshotChange::CurrentFilesChanged {
            added,
            removed,
            modified,
        },
    )
}

/// Result of re-reading a stored snapshot. Only `Unchanged` allows a plan to proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotValidity {
    Unchanged,
    Changed(Vec<SnapshotChange>),
    /// ARR could not be read: never treated as unchanged (fail-closed).
    Unavailable(PortError),
}

impl SnapshotValidity {
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, SnapshotValidity::Unchanged)
    }
}

/// Re-reads the snapshot of `stored` through `read` and compares it.
pub async fn revalidate(read: &dyn ArrRead, stored: &ArrQualitySnapshot) -> SnapshotValidity {
    let Some(media) = stored.media.arr.as_ref() else {
        return SnapshotValidity::Changed(vec![SnapshotChange::NoMediaRef]);
    };
    if media.service != read.service() || stored.service != read.service() {
        return SnapshotValidity::Changed(vec![SnapshotChange::ServiceChanged]);
    }
    match read.quality_snapshot(media).await {
        Err(e) => SnapshotValidity::Unavailable(e),
        Ok(current) => {
            let changes = detect_changes(stored, &current);
            if changes.is_empty()
                && current.fingerprint_is_valid()
                && current.fingerprint == stored.fingerprint
            {
                SnapshotValidity::Unchanged
            } else if changes.is_empty() {
                SnapshotValidity::Changed(vec![SnapshotChange::FingerprintChanged])
            } else {
                SnapshotValidity::Changed(changes)
            }
        }
    }
}
