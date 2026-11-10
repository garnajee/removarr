//! Fail-closed scope rules (OQ-05, OQ-07, CP1-F10, CP1-F19) and the automatic-mode perimeter.

use std::collections::BTreeMap;

use removarr_domain::evidence::Completeness;
use removarr_domain::fs::{ProtectedReason, RootInventory};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_domain::session::{ServiceKind, SessionStatus};
use serde::{Deserialize, Serialize};

use super::index::MappingFault;
use super::input::{Observation, QbitObservation};

/// State of one configured root as seen by this observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootPerimeter {
    pub root: RootId,
    /// An inventory exists for the root.
    pub scanned: bool,
    /// The walk finished (not cancelled).
    pub finished: bool,
    pub error_count: u32,
    /// `MountBoundary` and `NonUtf8Name` zones (they degrade the session and block auto mode).
    pub degrading_zones: u32,
    /// `QbitInternal` zones: expected and never touched; they do not degrade the root (A2) but
    /// any unit containing one is blocked.
    pub qbit_internal_zones: u32,
}

impl RootPerimeter {
    /// Whole-root condition for automatic actions (A2): finished, zero errors, zero degrading
    /// zones.
    #[must_use]
    pub fn auto_clean(&self) -> bool {
        self.scanned && self.finished && self.error_count == 0 && self.degrading_zones == 0
    }
}

/// qBittorrent side of the perimeter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QbitPerimeter {
    NotConfigured,
    Unavailable,
    Available {
        /// Torrents with a `NoRule` path.
        no_rule: u32,
        /// Torrents with an `Invalid` path.
        invalid: u32,
    },
}

/// Everything plan-engine needs to apply the whole-root automatic rule (A2, OQ-05). Pure data:
/// recomputed from stored inventories and the latest qBittorrent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Perimeter {
    pub session_status: SessionStatus,
    pub roots: Vec<RootPerimeter>,
    pub qbit: QbitPerimeter,
}

impl Perimeter {
    #[must_use]
    pub fn root(&self, root: &RootId) -> Option<&RootPerimeter> {
        self.roots.iter().find(|r| &r.root == root)
    }

    /// Reasons that block every automatic action touching `roots` (empty = none). Manual
    /// actions are not blocked by the whole-root rule, only by per-target blockers.
    #[must_use]
    pub fn auto_blockers(&self, roots: &[RootId]) -> Vec<Blocker> {
        let mut out = Vec::new();
        if self.session_status != SessionStatus::Complete {
            out.push(Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("session status {:?}", self.session_status),
            ));
        }
        for r in roots {
            match self.root(r) {
                None => out.push(Blocker::new(
                    BlockerCode::ScanIncomplete,
                    format!("root {r} was not observed"),
                )),
                Some(p) if !p.scanned || !p.finished => out.push(Blocker::new(
                    BlockerCode::ScanIncomplete,
                    format!("root {r} scan did not finish"),
                )),
                Some(p) => {
                    if p.error_count > 0 {
                        out.push(Blocker::new(
                            BlockerCode::ScanErrorUnder,
                            format!("root {r} has {} scan errors", p.error_count),
                        ));
                    }
                    if p.degrading_zones > 0 {
                        out.push(Blocker::new(
                            BlockerCode::ProtectedZone,
                            format!("root {r} has {} unreadable zones", p.degrading_zones),
                        ));
                    }
                }
            }
        }
        match &self.qbit {
            QbitPerimeter::NotConfigured => out.push(Blocker::new(
                BlockerCode::ServiceNotConfigured(ServiceKind::Qbittorrent),
                "torrent ownership is unknown",
            )),
            QbitPerimeter::Unavailable => out.push(Blocker::new(
                BlockerCode::ServiceUnavailable(ServiceKind::Qbittorrent),
                "torrent ownership is unknown",
            )),
            QbitPerimeter::Available { no_rule, invalid } => {
                if *no_rule > 0 {
                    out.push(Blocker::new(
                        BlockerCode::UnmappedTorrents,
                        format!("{no_rule} torrents have a path with no mapping rule"),
                    ));
                }
                if *invalid > 0 {
                    out.push(Blocker::new(
                        BlockerCode::MappingError,
                        format!("{invalid} torrents have an invalid mapped path"),
                    ));
                }
            }
        }
        out
    }
}

/// Per-root lookup used while classifying.
#[derive(Debug)]
pub(crate) struct Scope<'a> {
    pub configured: Vec<RootId>,
    inventories: BTreeMap<&'a RootId, &'a RootInventory>,
}

impl<'a> Scope<'a> {
    pub fn new(obs: &'a Observation) -> Self {
        Self {
            configured: obs.configured_roots.clone(),
            inventories: obs.inventories.iter().map(|i| (&i.root, i)).collect(),
        }
    }

    pub fn inventory(&self, root: &RootId) -> Option<&'a RootInventory> {
        self.inventories.get(root).copied()
    }

    /// Blockers for one path in the filesystem scope.
    pub fn path_blockers(&self, path: &RootedPath) -> Vec<Blocker> {
        self.subtree_blockers(&path.root, &path.rel)
    }

    /// Blockers for a subtree (a directory unit) or a file.
    pub fn subtree_blockers(&self, root: &RootId, rel: &RelPath) -> Vec<Blocker> {
        let Some(inv) = self.inventory(root) else {
            return vec![Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("root {root} was not scanned"),
            )];
        };
        if !inv.finished {
            return vec![Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("root {root} scan did not finish"),
            )];
        }
        let related =
            |p: &RootedPath| p.root == *root && (p.rel.starts_with(rel) || rel.starts_with(&p.rel));
        let mut out = Vec::new();
        if inv.errors.iter().any(|e| related(&e.path)) {
            out.push(Blocker::new(
                BlockerCode::ScanErrorUnder,
                format!("a scan error lies at or around {root}:{rel}"),
            ));
        }
        if inv.protected.iter().any(|z| related(&z.path)) {
            out.push(Blocker::new(
                BlockerCode::ProtectedZone,
                format!("a protected zone lies at or around {root}:{rel}"),
            ));
        }
        out
    }

    /// Blockers that apply to a claim about the *absence* of something in a whole root (for
    /// example "no hardlink in the media tree"): an unfinished walk or any scan error could be
    /// hiding the very entry that would disprove the claim.
    pub fn absence_blockers(&self, roots: impl Iterator<Item = RootId>) -> Vec<Blocker> {
        let mut out = Vec::new();
        for r in roots {
            match self.inventory(&r) {
                None => out.push(Blocker::new(
                    BlockerCode::ScanIncomplete,
                    format!("root {r} was not scanned"),
                )),
                Some(i) if !i.finished => out.push(Blocker::new(
                    BlockerCode::ScanIncomplete,
                    format!("root {r} scan did not finish"),
                )),
                Some(i) if !i.errors.is_empty() => out.push(Blocker::new(
                    BlockerCode::ScanErrorUnder,
                    format!("root {r} has scan errors that may hide entries"),
                )),
                Some(_) => {}
            }
        }
        out
    }

    /// Completeness of filesystem evidence about `path`.
    pub fn completeness(&self, path: &RootedPath) -> Completeness {
        let Some(inv) = self.inventory(&path.root) else {
            return Completeness::Incomplete {
                missing: vec![format!("inventory of root {}", path.root)],
            };
        };
        if !inv.finished {
            return Completeness::Incomplete {
                missing: vec![format!("finished scan of root {}", path.root)],
            };
        }
        if inv.has_issue_under(&path.rel) {
            return Completeness::Incomplete {
                missing: vec![format!("error-free scan around {}", path)],
            };
        }
        Completeness::Complete
    }
}

/// Computes the perimeter of an observation (also usable by plan-engine on loaded data).
#[must_use]
pub fn perimeter(obs: &Observation) -> Perimeter {
    let roots = obs
        .configured_roots
        .iter()
        .map(|r| {
            let inv = obs.inventories.iter().find(|i| &i.root == r);
            RootPerimeter {
                root: r.clone(),
                scanned: inv.is_some(),
                finished: inv.is_some_and(|i| i.finished),
                error_count: inv.map_or(0, |i| u32::try_from(i.errors.len()).unwrap_or(u32::MAX)),
                degrading_zones: inv
                    .map_or(0, |i| count(i, |z| z != ProtectedReason::QbitInternal)),
                qbit_internal_zones: inv
                    .map_or(0, |i| count(i, |z| z == ProtectedReason::QbitInternal)),
            }
        })
        .collect();
    let qbit = match &obs.qbit {
        QbitObservation::NotConfigured => QbitPerimeter::NotConfigured,
        QbitObservation::Unavailable { .. } => QbitPerimeter::Unavailable,
        QbitObservation::Available(s) => {
            let faults = super::index::mapping_faults(s);
            let has = |f: MappingFault| {
                faults
                    .values()
                    .filter(|set| set.iter().any(|(_, k)| *k == f))
                    .count()
            };
            QbitPerimeter::Available {
                no_rule: u32::try_from(has(MappingFault::NoRule)).unwrap_or(u32::MAX),
                invalid: u32::try_from(has(MappingFault::Invalid)).unwrap_or(u32::MAX),
            }
        }
    };
    Perimeter {
        session_status: obs.session_status,
        roots,
        qbit,
    }
}

fn count(inv: &RootInventory, pred: impl Fn(ProtectedReason) -> bool) -> u32 {
    u32::try_from(inv.protected.iter().filter(|z| pred(z.reason)).count()).unwrap_or(u32::MAX)
}
