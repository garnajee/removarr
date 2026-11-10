//! Gain accounting.
//!
//! Four quantities are kept apart: logical size, unique allocated space, attributable gain
//! (space that only this plan's removals free) and the observed free-space delta. A potential
//! gain is a label for display; it is never a deletion authorization.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::canonical::sha256_hex;
use crate::fs::PhysicalId;
use crate::ids::{ActionRef, GainId};
use crate::path::RootId;
use crate::time::{MtimeNs, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GainKind {
    AttributableDeletion,
    AttributableConsolidation,
    /// Extra space consumed (for example an ARR copy import).
    AddedCost,
}

/// One realized gain. `gain_id` is derived from the freed inode, so the same inode can be
/// recorded at most once: the database enforces UNIQUE and aggregates drop repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GainRecord {
    pub gain_id: GainId,
    pub action: ActionRef,
    pub kind: GainKind,
    pub phys: PhysicalId,
    pub bytes: u64,
    pub recorded_at: Timestamp,
}

/// Deterministic gain identity: first 32 hex chars of
/// `sha256("gain\0" + dev + "\0" + ino + "\0" + size + "\0" + mtime_ns)`.
#[must_use]
pub fn gain_id(phys: PhysicalId, size: u64, mtime_ns: MtimeNs) -> GainId {
    let input = format!("gain\0{}\0{}\0{}\0{}", phys.dev, phys.ino, size, mtime_ns.0);
    GainId(sha256_hex(input.as_bytes())[..32].to_string())
}

/// DR-04 / T-03. `selected` maps an inode to `(selected_links, nlink, allocated)`. An inode
/// counts its allocated bytes once, and only if no other link survives: it is not in
/// `held_elsewhere` (for example present in media) and the plan removes every link
/// (`selected_links >= nlink`). `nlink` is accounting data, never hardlink proof.
#[must_use]
pub fn estimate_reclaimable(
    selected: &BTreeMap<PhysicalId, (u32, u64, u64)>,
    held_elsewhere: &BTreeSet<PhysicalId>,
) -> u64 {
    selected
        .iter()
        .filter(|(phys, (links, nlink, _))| {
            !held_elsewhere.contains(phys) && *links > 0 && u64::from(*links) >= *nlink
        })
        .fold(0u64, |acc, (_, (_, _, allocated))| {
            acc.saturating_add(*allocated)
        })
}

/// Per-action expectation. Expected, not observed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GainEstimate {
    pub logical: u64,
    pub attributable: u64,
    pub added: u64,
}

/// Plan-level totals. `attributable` must come from one `estimate_reclaimable` over the whole
/// selection, not from summing per-action estimates, so each inode counts once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGains {
    pub logical: u64,
    pub attributable: u64,
    pub added: u64,
    pub net: i64,
}

impl PlanGains {
    #[must_use]
    pub fn new(logical: u64, attributable: u64, added: u64) -> Self {
        let net = i64::try_from(attributable)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(added).unwrap_or(i64::MAX));
        Self {
            logical,
            attributable,
            added,
            net,
        }
    }
}

/// Free-space observation around an execution (a distinct quantity from attributable gain;
/// snapshots and open files can distort it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedDelta {
    pub root: RootId,
    pub free_before: u64,
    pub free_after_local: u64,
    pub free_after_services: Option<u64>,
}

/// Totals over ledger records, counting each `gain_id` once (first record wins).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GainTotals {
    pub attributable: u64,
    pub added: u64,
    pub net: i64,
    /// Records skipped because their `gain_id` was already counted.
    pub duplicates: u64,
}

/// Aggregates by reference to gain ids: a repeated id never adds bytes twice.
#[must_use]
pub fn aggregate_unique(records: &[GainRecord]) -> GainTotals {
    let mut seen = BTreeSet::new();
    let (mut attributable, mut added, mut duplicates) = (0u64, 0u64, 0u64);
    for r in records {
        if !seen.insert(&r.gain_id) {
            duplicates += 1;
            continue;
        }
        match r.kind {
            GainKind::AttributableDeletion | GainKind::AttributableConsolidation => {
                attributable = attributable.saturating_add(r.bytes);
            }
            GainKind::AddedCost => added = added.saturating_add(r.bytes),
        }
    }
    let net = i64::try_from(attributable)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(added).unwrap_or(i64::MAX));
    GainTotals {
        attributable,
        added,
        net,
        duplicates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PlanId;

    fn ph(ino: u64) -> PhysicalId {
        PhysicalId { dev: 1, ino }
    }

    /// T-03 / FX-34 + FX-35: two paths of the same inode I_T (nlink 2).
    #[test]
    fn gain_counted_only_when_every_hardlink_is_selected() {
        let t = ph(34);
        let none = BTreeSet::new();
        let one = BTreeMap::from([(t, (1, 2, 24_576))]);
        assert_eq!(estimate_reclaimable(&one, &none), 0);
        let both = BTreeMap::from([(t, (2, 2, 24_576))]);
        assert_eq!(estimate_reclaimable(&both, &none), 24_576);
    }

    #[test]
    fn inode_held_elsewhere_is_never_counted() {
        let t = ph(1);
        let sel = BTreeMap::from([(t, (2, 2, 100)), (ph(2), (1, 1, 50))]);
        assert_eq!(estimate_reclaimable(&sel, &BTreeSet::from([t])), 50);
    }

    #[test]
    fn zero_selected_links_and_overflow_are_safe() {
        let sel = BTreeMap::from([
            (ph(1), (0, 0, 100)),
            (ph(2), (1, 1, u64::MAX)),
            (ph(3), (1, 1, 5)),
        ]);
        assert_eq!(estimate_reclaimable(&sel, &BTreeSet::new()), u64::MAX);
    }

    #[test]
    fn gain_id_is_deterministic_and_sensitive() {
        let a = gain_id(ph(1), 10, MtimeNs(5));
        assert_eq!(a, gain_id(ph(1), 10, MtimeNs(5)));
        assert_ne!(a, gain_id(ph(2), 10, MtimeNs(5)));
        assert_ne!(a, gain_id(ph(1), 11, MtimeNs(5)));
        assert_ne!(a, gain_id(PhysicalId { dev: 2, ino: 1 }, 10, MtimeNs(5)));
        assert_ne!(a, gain_id(ph(1), 10, MtimeNs(6)));
        assert_eq!(a.0.len(), 32);
    }

    fn rec(plan: u128, idx: u32, kind: GainKind, phys: PhysicalId, bytes: u64) -> GainRecord {
        GainRecord {
            gain_id: gain_id(phys, bytes, MtimeNs(0)),
            action: ActionRef {
                plan: PlanId::new(uuid::Uuid::from_u128(plan)),
                index: idx,
            },
            kind,
            phys,
            bytes,
            recorded_at: Timestamp(0),
        }
    }

    #[test]
    fn same_inode_recorded_by_two_actions_is_not_double_counted() {
        let r = [
            rec(1, 0, GainKind::AttributableDeletion, ph(7), 1000),
            rec(2, 3, GainKind::AttributableConsolidation, ph(7), 1000),
            rec(1, 1, GainKind::AttributableDeletion, ph(8), 500),
            rec(1, 2, GainKind::AddedCost, ph(9), 200),
        ];
        let t = aggregate_unique(&r);
        assert_eq!(t.attributable, 1500);
        assert_eq!(t.added, 200);
        assert_eq!(t.net, 1300);
        assert_eq!(t.duplicates, 1);
    }

    #[test]
    fn plan_gains_net_can_be_negative() {
        let g = PlanGains::new(10, 5, 20);
        assert_eq!(g.net, -15);
        assert_eq!(PlanGains::new(0, 0, 0).net, 0);
    }
}
