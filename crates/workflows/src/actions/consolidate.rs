//! `ConsolidateHardlink` (tier 2; architecture 7.4, CONTEXTE 3.5 order, CON-15):
//! revalidate -> probe each item -> stop affected hashes -> revalidate again -> per item: probe
//! gate + full compare, then temporary link + atomic replace -> recheck modified hashes ->
//! restore intact hashes -> verify by `(dev, ino)` re-read.
//!
//! Manual plans continue after an item failure; automatic plans stop at the first one
//! (CON-26). Every failure leaves nothing half done: the mutator is atomic, and a failed
//! replace is judged by a `stat` of the target, never by guessing.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{EntrySignature, PhysicalId};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, JournalStep, VerificationResult};
use removarr_domain::path::{RelPath, RootedPath};
use removarr_domain::plan::{ActionTarget, PlanOrigin, PlannedAction, Precondition};
use removarr_matching::proof::{ContentVerdict, HardlinkProbeGate, ReadOnlyGate, VerifyRequest};
use removarr_matching::reject::Rejection;
use serde::{Deserialize, Serialize};

use super::events::ActionEvent;
use super::journal::{Ctx, StepRun, json};
use super::qbit::TorrentTx;
use super::revalidate::LiveReads;

/// `target` payload of a `FsReplaceWithHardlink` intent (recovery reads it back).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplaceTarget {
    pub source: RootedPath,
    pub target: RootedPath,
    pub expect_source: EntrySignature,
    pub expect_target: EntrySignature,
}

/// `target` payload of a `FsProbeHardlink` intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProbeTarget {
    pub source: RootedPath,
    pub target_dir: RootedPath,
    pub purpose: &'static str,
}

/// Result of one plan's consolidation batch.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub outcomes: BTreeMap<u32, (ActionOutcome, Vec<VerificationResult>)>,
    pub tx: TorrentTx,
    /// A revalidation pass failed: the plan is invalidated.
    pub invalidated: bool,
    /// Cancellation: no further mutation was issued.
    pub interrupted: bool,
}

struct Item<'a> {
    action: &'a PlannedAction,
    source: RootedPath,
    target: RootedPath,
    expect_source: EntrySignature,
    expect_target: EntrySignature,
}

fn pinned(a: &PlannedAction, path: &RootedPath) -> Option<EntrySignature> {
    a.preconditions.iter().find_map(|p| match p {
        Precondition::EntryUnchanged { path: q, sig, .. } if q == path => Some(*sig),
        _ => None,
    })
}

fn item(a: &PlannedAction) -> Result<Item<'_>, Blocker> {
    let ActionTarget::Consolidate { source, target } = &a.target else {
        return Err(Blocker::new(
            BlockerCode::NotInBuild,
            "not a consolidation target",
        ));
    };
    let missing = |p: &RootedPath| {
        Blocker::new(
            BlockerCode::ScanIncomplete,
            format!("the plan carries no signature for {p}"),
        )
    };
    Ok(Item {
        action: a,
        source: source.clone(),
        target: target.clone(),
        expect_source: pinned(a, source).ok_or_else(|| missing(source))?,
        expect_target: pinned(a, target).ok_or_else(|| missing(target))?,
    })
}

fn not_executed(blockers: Vec<Blocker>) -> ActionOutcome {
    ActionOutcome::NotExecuted { blockers }
}

fn aborted(reason: &str) -> ActionOutcome {
    not_executed(vec![Blocker::new(
        BlockerCode::FailedConsolidation,
        format!("not executed: {reason}"),
    )])
}

fn rejection_error(r: &Rejection) -> PortError {
    let class = match r {
        Rejection::TargetNotWritable { .. } => {
            ErrorClass::Io(removarr_domain::fs::IoKind::PermissionDenied)
        }
        Rejection::ReadFailed { class } => class.clone(),
        Rejection::SamplesDiffer
        | Rejection::ContentDiffers { .. }
        | Rejection::SizeDiffers { .. } => ErrorClass::Rejected,
        Rejection::Missing { .. }
        | Rejection::NotRegularFile { .. }
        | Rejection::ChangedBefore { .. }
        | Rejection::ChangedAfter { .. }
        | Rejection::ChangedDuring => ErrorClass::StateChanged,
    };
    PortError::new(class, format!("content verification refused: {}", r.code()))
}

impl Ctx<'_> {
    /// Runs the consolidation batch of a plan. `Err` only when the journal (or a blocking
    /// task) failed: the plan stays `Executing` for recovery.
    pub(crate) async fn consolidate(&self, actions: &[&PlannedAction]) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let auto = self.plan.origin == PlanOrigin::AutoPolicy;
        let mut items = Vec::new();
        for a in actions {
            match item(a) {
                Ok(i) => items.push(i),
                Err(bl) => {
                    b.outcomes.insert(a.index, (not_executed(vec![bl]), vec![]));
                }
            }
        }

        // 1. Revalidate everything before any mutation.
        let mut live = LiveReads::default();
        if !self.revalidate_pass(&mut live, &items, 1, &mut b).await? {
            return Ok(b);
        }

        // 2. Permission probe per item, before any content read and before the stop.
        let mut remaining = Vec::new();
        let mut abort: Option<String> = None;
        for it in items {
            if self.cancel.is_cancelled() {
                abort = Some("cancelled before the probe".into());
            }
            if let Some(reason) = &abort {
                b.outcomes
                    .insert(it.action.index, (aborted(reason), vec![]));
                continue;
            }
            self.reporter.stage(
                "probe",
                b.outcomes.len() as u32,
                Some(it.target.to_string()),
            );
            match self.probe(&it, "permission probe").await? {
                StepRun::Failed(e) => {
                    b.outcomes.insert(
                        it.action.index,
                        (ActionOutcome::Failed { class: e.class }, vec![]),
                    );
                    if auto {
                        abort = Some("automatic plan stopped at the first failure".into());
                    }
                }
                StepRun::Done(()) | StepRun::Simulated => remaining.push(it),
            }
        }
        if remaining.is_empty() {
            b.interrupted = self.cancel.is_cancelled();
            return Ok(b);
        }
        let lead = remaining[0].action.index;

        // 3. Stop every affected hash (journaled), then revalidate again.
        if self.cancel.is_cancelled() {
            for it in &remaining {
                b.outcomes.insert(
                    it.action.index,
                    (aborted("cancelled before the stop"), vec![]),
                );
            }
            b.interrupted = true;
            return Ok(b);
        }
        let hashes: BTreeSet<_> = remaining
            .iter()
            .flat_map(|it| it.action.torrent_hashes.iter().cloned())
            .collect();
        let listing = live.listing().to_vec();
        if let StepRun::Failed(e) = self
            .stop_torrents(lead, &hashes, &listing, &mut b.tx)
            .await?
        {
            // Outcome unknown: nothing was modified, so every initially active hash may be
            // started again; anything that fails to start stays stopped and reported.
            self.reporter.emit(ActionEvent::BatchInterrupted {
                plan: self.plan.id,
                reason: format!("stop failed: {}", e.detail),
            });
            self.restore(lead, &mut b.tx).await?;
            for it in &remaining {
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
            .revalidate_pass(&mut live2, &remaining, 2, &mut b)
            .await?
        {
            self.restore(lead, &mut b.tx).await?;
            return Ok(b);
        }

        // 4-5. Per item: probe gate + full compare, then temporary link + atomic replace.
        let mut mutated: Vec<(&PlannedAction, Option<PortError>)> = Vec::new();
        let mut abort: Option<String> = None;
        let total = remaining.len() as u32;
        for (n, it) in remaining.iter().enumerate() {
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
            self.reporter
                .stage("consolidate", n as u32, Some(it.target.to_string()));
            let _ = total;
            match self.compare_then_replace(it, &mut b).await? {
                Replaced::Mutated(err) => mutated.push((it.action, err)),
                Replaced::Simulated => {
                    b.outcomes
                        .insert(it.action.index, (ActionOutcome::Simulated, vec![]));
                    mutated.push((it.action, None));
                }
                Replaced::Nothing(outcome) => {
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
        for (a, step_error) in mutated {
            let results = self.verify_action(a, &b.tx).await;
            let outcome = if self.simulated {
                ActionOutcome::Simulated
            } else {
                match (
                    step_error,
                    removarr_domain::journal::outcome_from_verification(&results),
                ) {
                    // A failed replace step never becomes a success, whatever the re-read says.
                    (Some(e), _) => ActionOutcome::ExecutedUnverified {
                        reason: format!("replace step failed: {}", e.detail),
                    },
                    (None, o) => o,
                }
            };
            b.outcomes.insert(a.index, (outcome, results));
        }
        Ok(b)
    }

    /// One revalidation pass. Any failing action invalidates the whole plan: every action of
    /// the pass is `NotExecuted` and `false` is returned.
    async fn revalidate_pass(
        &self,
        live: &mut LiveReads,
        items: &[Item<'_>],
        pass: u8,
        b: &mut Batch,
    ) -> Result<bool, PortError> {
        self.reporter.stage("revalidate", 0, None);
        let mut failed = BTreeMap::new();
        for it in items {
            let bl = self.revalidate_action(live, it.action).await?;
            if !bl.is_empty() {
                failed.insert(it.action.index, bl);
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
        for it in items {
            let blockers = failed.remove(&it.action.index).unwrap_or_else(|| {
                vec![Blocker::new(
                    BlockerCode::ScanIncomplete,
                    "another action of the plan failed revalidation; the plan is invalidated",
                )]
            });
            b.outcomes
                .insert(it.action.index, (not_executed(blockers), vec![]));
        }
        b.invalidated = true;
        Ok(false)
    }

    async fn probe(&self, it: &Item<'_>, purpose: &'static str) -> Result<StepRun<()>, PortError> {
        let dir = RootedPath::new(
            it.target.root.clone(),
            it.target.rel.parent().unwrap_or_else(RelPath::root),
        );
        let fs = self.mutators.fs.clone();
        let (src, d) = (it.source.clone(), dir.clone());
        self.step(
            it.action.index,
            JournalStep::FsProbeHardlink,
            json(&ProbeTarget {
                source: it.source.clone(),
                target_dir: dir,
                purpose,
            }),
            serde_json::Value::Null,
            it.action.torrent_hashes.clone(),
            || self.blocking(move || fs.probe_hardlink(&src, &d)),
        )
        .await
    }

    /// Full comparison behind the hardlink probe gate (live) or the read-only gate
    /// (simulation), then the journaled replacement.
    async fn compare_then_replace(
        &self,
        it: &Item<'_>,
        b: &mut Batch,
    ) -> Result<Replaced, PortError> {
        let hashes = it.action.torrent_hashes.clone();
        let verdict = if self.simulated {
            self.probe(it, "pre-compare gate").await?;
            self.compare(it, false).await?
        } else {
            let run = self
                .step(
                    it.action.index,
                    JournalStep::FsProbeHardlink,
                    json(&ProbeTarget {
                        source: it.source.clone(),
                        target_dir: RootedPath::new(
                            it.target.root.clone(),
                            it.target.rel.parent().unwrap_or_else(RelPath::root),
                        ),
                        purpose: "pre-compare gate",
                    }),
                    serde_json::Value::Null,
                    hashes.clone(),
                    || async {
                        let v = self.compare(it, true).await?;
                        match &v {
                            ContentVerdict::Rejected(r @ Rejection::TargetNotWritable { .. }) => {
                                Err(rejection_error(r))
                            }
                            _ => Ok(v),
                        }
                    },
                )
                .await?;
            match run {
                StepRun::Done(v) => v,
                StepRun::Failed(e) => {
                    return Ok(Replaced::Nothing(ActionOutcome::Failed { class: e.class }));
                }
                StepRun::Simulated => return Ok(Replaced::Simulated),
            }
        };
        match &verdict {
            ContentVerdict::Identical { .. } => {}
            ContentVerdict::Rejected(r) => {
                let e = rejection_error(r);
                return Ok(Replaced::Nothing(if self.simulated {
                    not_executed(vec![Blocker::new(
                        r.blocker_code().unwrap_or(BlockerCode::ContentDiffers),
                        e.detail,
                    )])
                } else {
                    ActionOutcome::Failed { class: e.class }
                }));
            }
            ContentVerdict::SamePhysical => {
                return Ok(Replaced::Nothing(ActionOutcome::Failed {
                    class: ErrorClass::StateChanged,
                }));
            }
        }
        let fs = self.mutators.fs.clone();
        let payload = ReplaceTarget {
            source: it.source.clone(),
            target: it.target.clone(),
            expect_source: it.expect_source,
            expect_target: it.expect_target,
        };
        let p = payload.clone();
        let run = self
            .step(
                it.action.index,
                JournalStep::FsReplaceWithHardlink,
                json(&payload),
                json(&verdict),
                hashes.clone(),
                || {
                    self.blocking(move || {
                        fs.replace_with_hardlink(
                            &p.source,
                            &p.target,
                            &p.expect_source,
                            &p.expect_target,
                        )
                    })
                },
            )
            .await?;
        Ok(match run {
            StepRun::Done(_phys) => {
                b.tx.modified.extend(hashes);
                Replaced::Mutated(None)
            }
            StepRun::Simulated => {
                b.tx.modified.extend(hashes);
                Replaced::Simulated
            }
            StepRun::Failed(e) => {
                // Never guess: the target's inode says whether the replacement happened.
                match self.observe_replaced(it).await {
                    Some(true) => {
                        b.tx.modified.extend(hashes);
                        Replaced::Mutated(Some(e))
                    }
                    Some(false) => Replaced::Nothing(ActionOutcome::Failed { class: e.class }),
                    None => {
                        b.tx.modified.extend(hashes);
                        Replaced::Nothing(ActionOutcome::FailedNeedsReview)
                    }
                }
            }
        })
    }

    /// The bounded full comparison, off the async runtime. `live_gate` selects the hardlink
    /// probe gate (the only mutation in this step: a temporary link that is removed again);
    /// a simulation uses the read-only gate and reads the same bytes.
    async fn compare(&self, it: &Item<'_>, live_gate: bool) -> Result<ContentVerdict, PortError> {
        let (fs, content, clock, verifier) = (
            self.ports.fs.clone(),
            self.ports.content.clone(),
            self.ports.clock.clone(),
            self.settings.verifier.clone(),
        );
        let mutator = self.mutators.fs.clone();
        let (a, b, sa, sb) = (
            it.source.clone(),
            it.target.clone(),
            it.expect_source,
            it.expect_target,
        );
        let ctl = removarr_domain::ports::ScanControl {
            cancel: self.cancel.clone(),
            progress: Default::default(),
        };
        self.blocking(move || {
            let req = VerifyRequest {
                a: &a,
                b: &b,
                sig_a: &sa,
                sig_b: &sb,
            };
            if live_gate {
                let gate = HardlinkProbeGate { mutator: &*mutator };
                verifier.verify(&*fs, &*content, &gate, req, &ctl, &*clock)
            } else {
                verifier.verify(&*fs, &*content, &ReadOnlyGate, req, &ctl, &*clock)
            }
        })
        .await
    }

    /// `Some(true)` when the target now carries the source inode.
    async fn observe_replaced(&self, it: &Item<'_>) -> Option<bool> {
        let fs = self.ports.fs.clone();
        let t = it.target.clone();
        let want: PhysicalId = it.expect_source.phys();
        match self.blocking(move || fs.stat(&t)).await {
            Ok(Some(s)) => Some(s.phys() == want),
            Ok(None) => None,
            Err(_) => None,
        }
    }
}

enum Replaced {
    /// The replacement happened (`Some(e)` when the step reported an error afterwards).
    Mutated(Option<PortError>),
    Simulated,
    /// Nothing was changed on disk; the outcome to record.
    Nothing(ActionOutcome),
}
