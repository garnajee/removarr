//! Deletions under the completed root (tier 3; architecture 7.4 "Delete under completed"):
//! `DeleteCompletedFile`, `DeleteCompletedFolder`, `DeleteJunk`, `DeleteIncompleteOrphan`, the
//! `RemoveEmptyParent` companion and the opt-in `RemoveTorrentTask` companion.
//!
//! Order: items -> revalidation pass 1 -> journaled stop of every affected hash -> pass 2 ->
//! per item: journaled `remove_file` / `remove_tree` / `remove_empty_dir` (dependencies first)
//! -> task removals, only when a fresh file list plus `lstat` show every wanted file absent
//! -> recheck of the modified hashes -> restore of the intact ones -> verification by re-read.
//! A failed removal is judged by what the disk shows (CP2T3-F01); a partially removed tree is
//! `FailedNeedsReview` and its hashes stay stopped (T3-4).

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::PortError;
use removarr_domain::fs::{DirSnapshot, EntryKind, EntrySignature};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, JournalStep, StepOutcome};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::plan::{ActionKind, ActionTarget, PlanOrigin, PlannedAction, Precondition};
use removarr_domain::torrent::{MappedPath, TorrentHash};
use serde::{Deserialize, Serialize};

use super::batch::{Batch, Mutation, TreeObserved, aborted, not_executed};
use super::events::ActionEvent;
use super::journal::{Ctx, StepRun, json};
use super::revalidate::LiveReads;

/// `target` payload of a `FsRemoveFile` / `FsRemoveEmptyDir` / `FsSetMode` intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EntryTarget {
    pub path: RootedPath,
    pub expect: EntrySignature,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// `target` payload of a `FsRemoveTree` intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TreeTarget {
    pub snapshot: DirSnapshot,
}

/// `final_state` of a removal result: what the disk showed after the call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoveObserved {
    pub removed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<TreeObserved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `target` payload of a `QbitRemoveKeepData` intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoveTaskTarget {
    pub hash: TorrentHash,
}

/// What one deletion item does on disk.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    File(EntrySignature),
    Tree(DirSnapshot),
    EmptyDir(EntrySignature),
    Task(TorrentHash),
}

pub(crate) struct Item<'a> {
    pub action: &'a PlannedAction,
    pub path: Option<RootedPath>,
    pub op: Op,
}

fn missing(p: &RootedPath) -> Blocker {
    Blocker::new(
        BlockerCode::ChangedSincePlanning,
        format!("the plan carries no usable signature for {p}"),
    )
}

/// The planned operation of a path target, from its pin: a `DirUnchanged` snapshot of the
/// path is a tree, an `EntryUnchanged` file signature is a file, a directory signature (the
/// `RemoveEmptyParent` pin) is an empty directory.
pub(crate) fn op_of(a: &PlannedAction, path: &RootedPath) -> Option<Op> {
    for p in &a.preconditions {
        match p {
            Precondition::DirUnchanged { snapshot } if &snapshot.dir == path => {
                return Some(Op::Tree(snapshot.clone()));
            }
            Precondition::EntryUnchanged { path: q, sig, .. } if q == path => {
                return match sig.kind {
                    EntryKind::File => Some(Op::File(*sig)),
                    EntryKind::Dir => Some(Op::EmptyDir(*sig)),
                    EntryKind::Symlink | EntryKind::Other => None,
                };
            }
            _ => {}
        }
    }
    None
}

impl Ctx<'_> {
    /// Runner-side refusals (defense in depth, T3-2, T3-6): never a media root, never a root
    /// itself, the right root kind for the kind, a pin that describes the target, no sample
    /// without its acknowledgement.
    fn delete_item<'a>(&self, a: &'a PlannedAction) -> Result<Item<'a>, Vec<Blocker>> {
        let (path, want) = match (a.kind, &a.target) {
            (ActionKind::RemoveTorrentTask, ActionTarget::Torrent { hash, .. }) => {
                return Ok(Item {
                    action: a,
                    path: None,
                    op: Op::Task(hash.clone()),
                });
            }
            (ActionKind::DeleteIncompleteOrphan, ActionTarget::Path { path }) => {
                (path, RootKind::Incomplete)
            }
            (
                ActionKind::DeleteCompletedFile
                | ActionKind::DeleteCompletedFolder
                | ActionKind::DeleteJunk
                | ActionKind::RemoveEmptyParent,
                ActionTarget::Path { path },
            ) => (path, RootKind::Completed),
            _ => {
                return Err(vec![Blocker::new(
                    BlockerCode::NotInBuild,
                    format!("{:?} is not a deletion this batch executes", a.kind),
                )]);
            }
        };
        if path.root.kind() == RootKind::Media {
            return Err(vec![Blocker::new(
                BlockerCode::MediaRootDeletion,
                "nothing is ever deleted under a media root",
            )]);
        }
        if path.root.kind() != want || path.rel.is_root() {
            return Err(vec![Blocker::new(
                BlockerCode::ProtectedZone,
                format!("{:?} cannot target {path}", a.kind),
            )]);
        }
        let op = op_of(a, path).ok_or_else(|| vec![missing(path)])?;
        let mut blockers = Vec::new();
        match (&op, a.kind) {
            (Op::EmptyDir(_), ActionKind::RemoveEmptyParent) => {}
            (Op::EmptyDir(_), _) | (Op::Task(_), _) => blockers.push(missing(path)),
            (Op::File(_), ActionKind::RemoveEmptyParent) => blockers.push(missing(path)),
            (Op::File(_), _) => blockers.extend(self.sample_blocker(path)),
            (Op::Tree(snap), _) => blockers.extend(self.tree_sample_blockers(snap)),
        }
        if !blockers.is_empty() {
            return Err(blockers);
        }
        Ok(Item {
            action: a,
            path: Some(path.clone()),
            op,
        })
    }

    /// The deletion batch of a plan. `Err` only when the journal (or a blocking task) failed:
    /// the plan stays `Executing` for recovery.
    pub(crate) async fn delete_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let auto = self.plan.origin == PlanOrigin::AutoPolicy;
        let mut items = Vec::new();
        for a in actions {
            match self.delete_item(a) {
                Ok(i) => items.push(i),
                Err(bl) => {
                    b.outcomes.insert(a.index, (not_executed(bl), vec![]));
                }
            }
        }
        if items.is_empty() {
            // Nothing the approval covers can run: the plan as approved is void.
            b.invalidated = true;
            return Ok(b);
        }

        // 1. Revalidate everything before any mutation.
        let mut live = LiveReads::default();
        let acts: Vec<&PlannedAction> = items.iter().map(|i| i.action).collect();
        if !self
            .revalidate_pass(&mut live, &acts, 1, &mut b, &BTreeMap::new())
            .await?
        {
            return Ok(b);
        }
        let lead = items[0].action.index;

        // 2. Stop every affected hash (journaled), then revalidate again.
        if self.cancel.is_cancelled() {
            for it in &items {
                b.outcomes.insert(
                    it.action.index,
                    (aborted("cancelled before the stop"), vec![]),
                );
            }
            b.interrupted = true;
            return Ok(b);
        }
        let hashes: BTreeSet<_> = items
            .iter()
            .flat_map(|it| it.action.torrent_hashes.iter().cloned())
            .collect();
        let listing = live.listing().to_vec();
        if let StepRun::Failed(e) = self
            .stop_torrents(lead, &hashes, &listing, &mut b.tx)
            .await?
        {
            self.reporter.emit(ActionEvent::BatchInterrupted {
                plan: self.plan.id,
                reason: format!("stop failed: {}", e.detail),
            });
            self.restore(lead, &mut b.tx).await?;
            for it in &items {
                b.outcomes.insert(
                    it.action.index,
                    (
                        ActionOutcome::Failed {
                            class: e.class.clone(),
                        },
                        vec![],
                    ),
                );
            }
            return Ok(b);
        }
        let mut live2 = LiveReads::default();
        if !self
            .revalidate_pass(&mut live2, &acts, 2, &mut b, &BTreeMap::new())
            .await?
        {
            self.restore(lead, &mut b.tx).await?;
            return Ok(b);
        }

        // 3-4. Removals in plan order (companions depend on earlier actions), then 5. task
        // removals once the disk proves every wanted file gone.
        let mut done: BTreeSet<u32> = BTreeSet::new();
        let mut mutated: Vec<(&PlannedAction, Option<PortError>, bool)> = Vec::new();
        let mut abort: Option<String> = None;
        let total = items.len() as u32;
        let (removals, tasks): (Vec<&Item<'_>>, Vec<&Item<'_>>) =
            items.iter().partition(|it| !matches!(it.op, Op::Task(_)));
        for (n, it) in removals.iter().chain(tasks.iter()).enumerate() {
            if self.cancel.is_cancelled() && abort.is_none() {
                abort = Some("cancelled between items".into());
                b.interrupted = true;
                self.reporter.emit(ActionEvent::BatchInterrupted {
                    plan: self.plan.id,
                    reason: "cancelled; compensation runs".into(),
                });
            }
            if let Some(reason) = &abort {
                b.outcomes
                    .insert(it.action.index, (aborted(reason), vec![]));
                continue;
            }
            if let Some(bl) = self.deps_done(it.action, &done) {
                b.outcomes
                    .insert(it.action.index, (not_executed(vec![bl]), vec![]));
                if auto {
                    abort = Some("automatic plan stopped at the first failure".into());
                }
                continue;
            }
            let stage = if matches!(it.op, Op::Task(_)) {
                "remove_task"
            } else {
                "delete"
            };
            self.reporter
                .stage(stage, n as u32, it.path.as_ref().map(ToString::to_string));
            let result = match &it.op {
                Op::Task(hash) => self.remove_task(it, hash, &done, &mut b).await?,
                _ => self.remove_step(it, &mut b).await?,
            };
            match result {
                Mutation::Done(err) => {
                    done.insert(it.action.index);
                    if auto && err.is_some() {
                        // A step that reported an error is an uncertainty: stop here.
                        abort = Some("automatic plan stopped at the first failure".into());
                    }
                    mutated.push((it.action, err, false));
                }
                Mutation::Simulated => {
                    done.insert(it.action.index);
                    b.outcomes
                        .insert(it.action.index, (ActionOutcome::Simulated, vec![]));
                    mutated.push((it.action, None, false));
                }
                Mutation::Partial => {
                    mutated.push((it.action, None, true));
                    if auto {
                        abort = Some("automatic plan stopped at the first failure".into());
                    }
                }
                Mutation::Nothing(outcome) => {
                    b.outcomes.insert(it.action.index, (outcome, vec![]));
                    if auto {
                        abort = Some("automatic plan stopped at the first failure".into());
                    }
                }
            }
        }

        // 6-7. Recheck what was modified, restore what is proven intact.
        self.reporter.stage("recheck", total, None);
        self.recheck_modified(lead, &mut b.tx).await?;
        self.restore(lead, &mut b.tx).await?;

        // 8. Verify by re-reading.
        self.reporter.stage("verify", total, None);
        for (a, step_error, partial) in mutated {
            let results = self.verify_action(a, &b.tx, None).await;
            let outcome = if self.simulated {
                ActionOutcome::Simulated
            } else if partial {
                ActionOutcome::FailedNeedsReview
            } else {
                match (
                    step_error,
                    removarr_domain::journal::outcome_from_verification(&results),
                ) {
                    // A failed step never becomes a success, whatever the re-read says.
                    (Some(e), _) => ActionOutcome::ExecutedUnverified {
                        reason: format!("step failed: {}", e.detail),
                    },
                    (None, o) => o,
                }
            };
            b.outcomes.insert(a.index, (outcome, results));
        }
        Ok(b)
    }

    /// One journaled removal (file, tree or empty directory), judged by the disk afterwards.
    pub(crate) async fn remove_step(
        &self,
        it: &Item<'_>,
        b: &mut Batch,
    ) -> Result<Mutation, PortError> {
        let Some(path) = &it.path else {
            return Ok(Mutation::Nothing(not_executed(vec![missing(
                &RootedPath::new(
                    removarr_domain::path::RootId::completed(),
                    removarr_domain::path::RelPath::root(),
                ),
            )])));
        };
        let hashes = it.action.torrent_hashes.clone();
        let (step, target) = match &it.op {
            Op::File(sig) => (
                JournalStep::FsRemoveFile,
                json(&EntryTarget {
                    path: path.clone(),
                    expect: *sig,
                    mode: None,
                }),
            ),
            Op::EmptyDir(sig) => (
                JournalStep::FsRemoveEmptyDir,
                json(&EntryTarget {
                    path: path.clone(),
                    expect: *sig,
                    mode: None,
                }),
            ),
            Op::Tree(snapshot) => (
                JournalStep::FsRemoveTree,
                json(&TreeTarget {
                    snapshot: snapshot.clone(),
                }),
            ),
            Op::Task(_) => unreachable!("task removals have their own step"),
        };
        let intent = self
            .intent(
                it.action.index,
                step,
                target,
                serde_json::Value::Null,
                hashes.clone(),
            )
            .await?;
        if self.simulated {
            self.result(intent, it.action.index, step, StepOutcome::Simulated)
                .await?;
            b.tx.modified.extend(hashes);
            return Ok(Mutation::Simulated);
        }
        let fs = self.mutators.fs.clone();
        let (p, op) = (path.clone(), it.op.clone());
        let call = self
            .blocking(move || match &op {
                Op::File(sig) => fs.remove_file(&p, sig),
                Op::EmptyDir(sig) => fs.remove_empty_dir(&p, sig),
                Op::Tree(snapshot) => fs.remove_tree(&p, snapshot),
                Op::Task(_) => unreachable!(),
            })
            .await;
        // The journal records what the disk shows, never only what the call returned
        // (CP2T3-F01).
        let (outcome, mutation) = match call {
            Ok(()) => (
                StepOutcome::Done {
                    final_state: json(&RemoveObserved {
                        removed: true,
                        tree: None,
                        error: None,
                    }),
                },
                Mutation::Done(None),
            ),
            Err(e) => self.judge_failed_removal(it, path, e).await,
        };
        self.result(intent, it.action.index, step, outcome).await?;
        if !matches!(mutation, Mutation::Nothing(_)) {
            b.tx.modified.extend(hashes);
        }
        Ok(mutation)
    }

    async fn judge_failed_removal(
        &self,
        it: &Item<'_>,
        path: &RootedPath,
        e: PortError,
    ) -> (StepOutcome, Mutation) {
        let failed = |msg: String| StepOutcome::Failed {
            class: e.class.clone(),
            message: msg,
        };
        match &it.op {
            Op::Tree(snapshot) => {
                let obs = self.observe_tree(snapshot).await;
                if obs.untouched() {
                    (
                        failed(format!("{} (nothing removed)", e.detail)),
                        Mutation::Nothing(ActionOutcome::Failed {
                            class: e.class.clone(),
                        }),
                    )
                } else {
                    let all_gone = obs.all_gone();
                    (
                        StepOutcome::Done {
                            final_state: json(&RemoveObserved {
                                removed: all_gone,
                                tree: Some(obs),
                                error: Some(e.detail.clone()),
                            }),
                        },
                        if all_gone {
                            Mutation::Done(Some(e))
                        } else {
                            Mutation::Partial
                        },
                    )
                }
            }
            _ => match self.stat_path(path).await {
                Ok(None) => (
                    StepOutcome::Done {
                        final_state: json(&RemoveObserved {
                            removed: true,
                            tree: None,
                            error: Some(e.detail.clone()),
                        }),
                    },
                    Mutation::Done(Some(e)),
                ),
                Ok(Some(_)) => (
                    failed(format!("{} (target unchanged)", e.detail)),
                    Mutation::Nothing(ActionOutcome::Failed {
                        class: e.class.clone(),
                    }),
                ),
                Err(_) => (
                    failed(format!("{} (target could not be observed)", e.detail)),
                    Mutation::Partial,
                ),
            },
        }
    }

    /// T3-9: the task goes only when a fresh file list plus `lstat` of every candidate of
    /// every wanted file show nothing left (a simulation counts the paths its own simulated
    /// removals cover). Journaled `QbitRemoveKeepData`; the adapter has no delete-files flag.
    async fn remove_task(
        &self,
        it: &Item<'_>,
        hash: &TorrentHash,
        done: &BTreeSet<u32>,
        b: &mut Batch,
    ) -> Result<Mutation, PortError> {
        if !self.simulated
            && self
                .approval
                .is_none_or(|a| a.confirmed_torrent_removals < 1)
        {
            return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                BlockerCode::ProtectedZone,
                "torrent task removals need their own typed confirmation",
            )])));
        }
        let Some(reader) = &self.ports.qbit else {
            return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                BlockerCode::ServiceNotConfigured(
                    removarr_domain::session::ServiceKind::Qbittorrent,
                ),
                "qBittorrent is not configured",
            )])));
        };
        let files = match reader.files(hash).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                    BlockerCode::ServiceUnavailable(
                        removarr_domain::session::ServiceKind::Qbittorrent,
                    ),
                    format!("file list of {}: {}", hash.as_str(), e.detail),
                )])));
            }
        };
        if files.is_empty() {
            return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                BlockerCode::ChangedSincePlanning,
                format!(
                    "torrent {} lists no files; nothing proves its data is gone",
                    hash.as_str()
                ),
            )])));
        }
        let covered: Vec<RootedPath> = self
            .plan
            .actions
            .iter()
            .filter(|a| done.contains(&a.index))
            .filter_map(|a| match &a.target {
                ActionTarget::Path { path } => Some(path.clone()),
                _ => None,
            })
            .collect();
        for f in files.iter().filter(|f| f.is_wanted()) {
            for c in &f.candidates {
                let MappedPath::Mapped { path, .. } = c else {
                    return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                        BlockerCode::MappingError,
                        format!("wanted file {} of {} is not mapped", f.index, hash.as_str()),
                    )])));
                };
                let absent = if self.simulated {
                    covered
                        .iter()
                        .any(|r| r.root == path.root && path.rel.starts_with(&r.rel))
                } else {
                    matches!(self.stat_path(path).await, Ok(None))
                };
                if !absent {
                    return Ok(Mutation::Nothing(not_executed(vec![Blocker::new(
                        BlockerCode::ChangedSincePlanning,
                        format!(
                            "wanted file {} of {} is still present at {path}; the task is kept",
                            f.index,
                            hash.as_str()
                        ),
                    )])));
                }
            }
        }
        let set: BTreeSet<TorrentHash> = [hash.clone()].into();
        let run = self
            .step(
                it.action.index,
                JournalStep::QbitRemoveKeepData,
                json(&RemoveTaskTarget { hash: hash.clone() }),
                serde_json::Value::Null,
                set.clone(),
                || self.remove_tasks_observed(&set),
            )
            .await?;
        Ok(match run {
            StepRun::Done(()) => {
                b.tx.removed(hash);
                Mutation::Done(None)
            }
            StepRun::Simulated => {
                b.tx.removed(hash);
                Mutation::Simulated
            }
            StepRun::Failed(e) => Mutation::Nothing(ActionOutcome::Failed { class: e.class }),
        })
    }
}
