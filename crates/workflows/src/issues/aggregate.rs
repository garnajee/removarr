//! Dashboard aggregates without double counting (MASTER 5, INV-21, OQ-04).
//!
//! Potentially recoverable space is the allocated size of each *distinct inode* behind the
//! active issues, counted once across every category. It is a label, never a guaranteed gain and
//! never an authorization. Ignored issues are excluded from every total and counted separately.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::fs::{EntryKind, PhysicalId, RootInventory};
use removarr_domain::issue::BlockerCode;
use removarr_domain::issue::{Issue, IssueCategory, Severity, has_refusal};
use removarr_domain::path::RootKind;
use serde::{Deserialize, Serialize};

/// Allocated bytes per inode and the inodes shared between the completed and media trees.
/// Built from the inventories; never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InodeLedger {
    alloc: BTreeMap<PhysicalId, u64>,
    shared: BTreeSet<PhysicalId>,
}

impl InodeLedger {
    #[must_use]
    pub fn from_inventories(inventories: &[RootInventory]) -> Self {
        let mut alloc = BTreeMap::new();
        let (mut completed, mut media) = (BTreeSet::new(), BTreeSet::new());
        for inv in inventories {
            for e in inv.entries.iter().filter(|e| e.sig.kind == EntryKind::File) {
                let phys = e.sig.phys();
                alloc.entry(phys).or_insert(e.allocated);
                match inv.root.kind() {
                    RootKind::Completed => {
                        completed.insert(phys);
                    }
                    RootKind::Media => {
                        media.insert(phys);
                    }
                    RootKind::Incomplete => {}
                }
            }
        }
        let shared = completed.intersection(&media).copied().collect();
        Self { alloc, shared }
    }

    #[must_use]
    pub fn allocated(&self, phys: &PhysicalId) -> u64 {
        self.alloc.get(phys).copied().unwrap_or(0)
    }

    /// Space shared by hardlinks: each inode present in both trees, counted once.
    #[must_use]
    pub fn shared_bytes(&self) -> u64 {
        self.shared.iter().map(|p| self.allocated(p)).sum()
    }

    fn unique_bytes<'a>(&self, ids: impl Iterator<Item = &'a PhysicalId>) -> u64 {
        ids.copied()
            .collect::<BTreeSet<_>>()
            .iter()
            .map(|p| self.allocated(p))
            .fold(0u64, u64::saturating_add)
    }
}

/// True for an issue on the ignore list (it carries the `Ignored` blocker).
#[must_use]
pub fn is_ignored(issue: &Issue) -> bool {
    issue
        .blockers
        .iter()
        .any(|b| b.code == BlockerCode::Ignored)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryTotals {
    /// Active (not ignored) issues.
    pub count: u64,
    /// Active issues with at least one refusal.
    pub blocked: u64,
    /// Issues on the ignore list.
    pub ignored: u64,
    pub logical_bytes: u64,
    /// Distinct inodes of this category's active issues.
    pub potential_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregates {
    pub per_category: BTreeMap<IssueCategory, CategoryTotals>,
    pub by_severity: BTreeMap<Severity, u64>,
    pub ignored_count: u64,
    /// Distinct inodes across every category (the only honest global figure).
    pub potential_bytes: u64,
    /// The same, restricted to issues with no refusal.
    pub potential_unblocked_bytes: u64,
    /// Sum of per-issue figures, which double counts shared inodes. Kept to show the overlap.
    pub naive_potential_sum: u64,
    pub shared_bytes: u64,
}

impl Aggregates {
    /// Total active issues over all categories.
    #[must_use]
    pub fn total_count(&self) -> u64 {
        self.per_category.values().map(|c| c.count).sum()
    }
}

#[must_use]
pub fn aggregate(issues: &[Issue], ledger: &InodeLedger) -> Aggregates {
    let mut agg = Aggregates {
        shared_bytes: ledger.shared_bytes(),
        ..Aggregates::default()
    };
    let mut cat_ids: BTreeMap<IssueCategory, BTreeSet<PhysicalId>> = BTreeMap::new();
    let (mut all, mut unblocked) = (BTreeSet::new(), BTreeSet::new());
    for c in IssueCategory::ALL {
        agg.per_category.insert(c, CategoryTotals::default());
    }
    for i in issues {
        let t = agg.per_category.entry(i.category).or_default();
        if is_ignored(i) {
            t.ignored += 1;
            agg.ignored_count += 1;
            continue;
        }
        t.count += 1;
        t.logical_bytes = t.logical_bytes.saturating_add(i.logical_bytes);
        if has_refusal(&i.blockers) {
            t.blocked += 1;
        } else {
            unblocked.extend(i.physical_ids.iter().copied());
        }
        *agg.by_severity.entry(i.severity).or_default() += 1;
        agg.naive_potential_sum = agg.naive_potential_sum.saturating_add(i.potential_bytes);
        cat_ids
            .entry(i.category)
            .or_default()
            .extend(i.physical_ids.iter().copied());
        all.extend(i.physical_ids.iter().copied());
    }
    for (c, ids) in &cat_ids {
        if let Some(t) = agg.per_category.get_mut(c) {
            t.potential_bytes = ledger.unique_bytes(ids.iter());
        }
    }
    agg.potential_bytes = ledger.unique_bytes(all.iter());
    agg.potential_unblocked_bytes = ledger.unique_bytes(unblocked.iter());
    agg
}
