//! Shared pieces of every batch: the batch result, the revalidation pass, dependency and
//! sample checks, and the disk observations a failed step is judged by (CP2T3-F01).

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::PortError;
use removarr_domain::fs::{DirSnapshot, EntryKind, EntrySignature};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, VerificationResult};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::PlannedAction;
use removarr_domain::policy::is_sample;
use serde::{Deserialize, Serialize};

use super::events::ActionEvent;
use super::journal::Ctx;
use super::qbit::TorrentTx;
use super::revalidate::LiveReads;

/// Result of one plan's batch.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub outcomes: BTreeMap<u32, (ActionOutcome, Vec<VerificationResult>)>,
    pub tx: TorrentTx,
    /// A revalidation pass failed: the plan is invalidated.
    pub invalidated: bool,
    /// Cancellation: no further mutation was issued.
    pub interrupted: bool,
}

pub(crate) fn not_executed(blockers: Vec<Blocker>) -> ActionOutcome {
    ActionOutcome::NotExecuted { blockers }
}

pub(crate) fn aborted(reason: &str) -> ActionOutcome {
    not_executed(vec![Blocker::new(
        BlockerCode::Interrupted,
        format!("not executed: {reason}"),
    )])
}

/// What a step did to the disk, judged by the call and by a re-read.
pub(crate) enum Mutation {
    /// The mutation happened (`Some(e)` when the call reported an error afterwards).
    Done(Option<PortError>),
    Simulated,
    /// Nothing changed on disk; the outcome to record.
    Nothing(ActionOutcome),
    /// Part of the target changed, or the target could not be observed: manual review.
    Partial,
}

/// Where every planned entry of a tree stands after a failed `remove_tree` call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TreeObserved {
    pub removed: Vec<String>,
    pub remaining: Vec<String>,
    pub unobservable: Vec<String>,
}

impl TreeObserved {
    pub fn untouched(&self) -> bool {
        self.removed.is_empty() && self.unobservable.is_empty()
    }

    pub fn all_gone(&self) -> bool {
        self.remaining.is_empty() && self.unobservable.is_empty()
    }
}

impl Ctx<'_> {
    /// One revalidation pass over `actions`. Any failing action invalidates the whole plan:
    /// every action of the pass is `NotExecuted` and `false` is returned. A store or task
    /// failure is a blocker, never an abort (CP2T3-F11): after a stop, the compensating
    /// restore must still run. `extra` adds batch-specific checks per action.
    pub(crate) async fn revalidate_pass(
        &self,
        live: &mut LiveReads,
        actions: &[&PlannedAction],
        pass: u8,
        b: &mut Batch,
        extra: &BTreeMap<u32, Vec<Blocker>>,
    ) -> Result<bool, PortError> {
        self.reporter.stage("revalidate", 0, None);
        let mut failed = BTreeMap::new();
        for a in actions {
            let mut bl = match self.revalidate_action(live, a).await {
                Ok(bl) => bl,
                Err(e) => vec![Blocker::new(
                    BlockerCode::ChangedSincePlanning,
                    format!("revalidation could not read the stored state: {}", e.detail),
                )],
            };
            if let Some(x) = extra.get(&a.index) {
                bl.extend(x.iter().cloned());
            }
            if !bl.is_empty() {
                failed.insert(a.index, bl);
            }
        }
        self.reporter.emit(ActionEvent::Revalidated {
            plan: self.plan.id,
            pass,
            failed: failed.clone(),
        });
        if failed.is_empty() {
            return Ok(true);
        }
        for a in actions {
            let blockers = failed.remove(&a.index).unwrap_or_else(|| {
                vec![Blocker::new(
                    BlockerCode::ChangedSincePlanning,
                    "another action of the plan failed revalidation; the plan is invalidated",
                )]
            });
            b.outcomes.insert(a.index, (not_executed(blockers), vec![]));
        }
        b.invalidated = true;
        Ok(false)
    }

    /// T3-6: a sample path may only be touched when the approval names exactly it. A
    /// simulation (no approval yet) accepts the samples the plan itself declares.
    pub(crate) fn sample_blocker(&self, path: &RootedPath) -> Option<Blocker> {
        if !is_sample(&path.rel) {
            return None;
        }
        let acknowledged = self
            .approval
            .is_some_and(|a| a.acknowledged_samples.contains(path))
            || (self.simulated && self.plan.samples.contains(path));
        (!acknowledged).then(|| {
            Blocker::new(
                BlockerCode::Sample,
                format!("{path} is a sample and is not acknowledged in the approval"),
            )
        })
    }

    /// Every sample inside a planned tree needs its acknowledgement too.
    pub(crate) fn tree_sample_blockers(&self, snapshot: &DirSnapshot) -> Vec<Blocker> {
        snapshot
            .entries
            .iter()
            .filter(|(_, s)| s.kind != EntryKind::Dir)
            .filter_map(|(rel, _)| {
                let full = join(&snapshot.dir, rel.as_str())?;
                self.sample_blocker(&full)
            })
            .collect()
    }

    /// The dependencies of `a` must have mutated (live `Done`) or been simulated in this run.
    pub(crate) fn deps_done(&self, a: &PlannedAction, done: &BTreeSet<u32>) -> Option<Blocker> {
        let missing: Vec<u32> = a
            .depends_on
            .iter()
            .copied()
            .filter(|d| !done.contains(d))
            .collect();
        (!missing.is_empty()).then(|| {
            Blocker::new(
                BlockerCode::Interrupted,
                format!("not executed: an action it depends on was not executed ({missing:?})"),
            )
        })
    }

    pub(crate) async fn stat_path(
        &self,
        path: &RootedPath,
    ) -> Result<Option<EntrySignature>, PortError> {
        let fs = self.ports.fs.clone();
        let p = path.clone();
        self.blocking(move || fs.stat(&p)).await
    }

    /// Re-reads every planned entry of a tree.
    pub(crate) async fn observe_tree(&self, snapshot: &DirSnapshot) -> TreeObserved {
        let mut out = TreeObserved::default();
        for rel in snapshot.entries.keys() {
            let Some(full) = join(&snapshot.dir, rel.as_str()) else {
                out.unobservable.push(rel.as_str().to_string());
                continue;
            };
            match self.stat_path(&full).await {
                Ok(None) => out.removed.push(rel.as_str().to_string()),
                Ok(Some(_)) => out.remaining.push(rel.as_str().to_string()),
                Err(_) => out.unobservable.push(rel.as_str().to_string()),
            }
        }
        out
    }
}

/// `dir` joined with a snapshot key (`""` is the directory itself).
pub(crate) fn join(dir: &RootedPath, rel: &str) -> Option<RootedPath> {
    let mut out = dir.rel.clone();
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        out = out.join(comp).ok()?;
    }
    Some(RootedPath::new(dir.root.clone(), out))
}
