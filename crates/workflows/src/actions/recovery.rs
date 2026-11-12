//! Recovery after a restart (architecture 7.5): every plan with a live intent that has no
//! result is resolved from observations, never by redoing a mutation.
//!
//! - fs intents without result (or with a `Failed` result, CP2T3-F01): the target is re-read
//!   and the observed state becomes the result (the replacement or removal happened, it did
//!   not, or only part of a tree went: manual review);
//! - a task removal without result: the listing decides (gone = done); the data is never
//!   deleted by recovery, it stays as an orphan for a later plan;
//! - hashes stopped by the plan whose data was never modified: started if active initially;
//! - hashes with a mutation intent: rechecked, started only when the recheck is `Verified`,
//!   otherwise left stopped and reported;
//! - the plan becomes `Interrupted` and can never resume (the user re-plans).
//!
//! Plans still `Executing` without any unfinished intent (killed between a result and the next
//! intent) are resolved the same way from their journal. Simulated intents are ignored
//! (`JournalStore::unfinished` filters them). Without a live qBittorrent controller (dry-run
//! process) torrents are left stopped and reported.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::EntryKind;
use removarr_domain::ids::PlanId;
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{
    ActionOutcome, IntentRecord, JournalStep, ResultRecord, StepOutcome,
};
use removarr_domain::plan::{ActionPlan, PlanStatus};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::{TorrentHash, TorrentState, was_active};
use serde::{Deserialize, Serialize};

use super::consolidate::ReplaceTarget;
use super::delete::{EntryTarget, RemoveObserved, TreeTarget};
use super::events::{ActionEvent, Reporter};
use super::journal::Ctx;
use super::qbit::{LeftStoppedReason, StopObserved, TorrentTx};
use super::{ActionPorts, ActionSettings, Mutators};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredPlan {
    pub plan: PlanId,
    /// `Some(Interrupted)` when the transition was made; `None` when the plan was already
    /// terminal.
    pub status: Option<PlanStatus>,
    pub resumed: BTreeSet<TorrentHash>,
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
    pub actions: Vec<(u32, ActionOutcome)>,
    /// Human-readable observations (leftover temporary links to inspect, unknown steps).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub plans: Vec<RecoveredPlan>,
    pub events: Vec<ActionEvent>,
}

/// What recovery concluded about an action's mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Executed {
    Untouched,
    Mutated,
    /// The target could not be observed: possibly modified, manual review.
    Unobservable,
}

fn interrupted() -> ActionOutcome {
    ActionOutcome::NotExecuted {
        blockers: vec![Blocker::new(
            BlockerCode::Interrupted,
            "not executed: the process stopped before this action; re-plan",
        )],
    }
}

/// Resolves every plan with unfinished live intents. `Err(Unavailable)` when qBittorrent did
/// not answer: the filesystem results written so far stay, and the caller retries with backoff
/// (nothing else may execute meanwhile).
pub async fn recover(
    ports: &ActionPorts,
    mutators: &Mutators,
    settings: &ActionSettings,
    reporter: &Reporter,
) -> Result<RecoveryReport, PortError> {
    let mut plans: BTreeSet<PlanId> = ports
        .journal
        .unfinished()
        .await?
        .into_iter()
        .map(|u| u.intent.action.plan)
        .collect();
    plans.extend(ports.executing.executing().await?);
    reporter.begin(None, false, plans.len() as u32);
    reporter.emit(ActionEvent::RecoveryStarted {
        plans: plans.len() as u32,
    });
    let mut report = RecoveryReport::default();
    for plan_id in plans {
        let Some((plan, _, status, _)) = ports.plans.get(plan_id).await? else {
            continue;
        };
        if status != PlanStatus::Executing {
            // A live intent exists only for an executing plan (storage trigger); anything
            // else here is already terminal and is left alone.
            continue;
        }
        let cancel = CancelFlag::new();
        let ctx = Ctx {
            ports,
            mutators,
            settings,
            plan: &plan,
            approval: None,
            simulated: false,
            cancel: &cancel,
            reporter,
        };
        let recovered = recover_plan(&ctx, &plan).await?;
        reporter.emit(ActionEvent::RecoveredPlan {
            plan: plan_id,
            status: recovered.status,
        });
        report.plans.push(recovered);
    }
    report.events = reporter.events();
    Ok(report)
}

async fn recover_plan(ctx: &Ctx<'_>, plan: &ActionPlan) -> Result<RecoveredPlan, PortError> {
    let ports = ctx.ports;
    let entries = ports.journal.for_plan(plan.id).await?;
    let mut out = RecoveredPlan {
        plan: plan.id,
        status: None,
        resumed: BTreeSet::new(),
        left_stopped: BTreeMap::new(),
        actions: Vec::new(),
        notes: Vec::new(),
    };
    let mut tx = TorrentTx::default();
    let mut stopped: BTreeSet<TorrentHash> = BTreeSet::new();
    let mut executed: BTreeMap<u32, Executed> = BTreeMap::new();
    let mut unknown_restart: BTreeSet<TorrentHash> = BTreeSet::new();
    let mut removed: BTreeSet<TorrentHash> = BTreeSet::new();
    let mut listing: Option<Vec<removarr_domain::torrent::TorrentInfo>> = None;
    let mark = |executed: &mut BTreeMap<u32, Executed>, index: u32, what: Executed| {
        let slot = executed.entry(index).or_insert(Executed::Untouched);
        match what {
            Executed::Unobservable => *slot = Executed::Unobservable,
            Executed::Mutated if *slot != Executed::Unobservable => *slot = Executed::Mutated,
            _ => {}
        }
    };

    // 1. Read what the journal says; resolve filesystem intents from observations.
    for (intent, result) in &entries {
        if intent.mode != removarr_domain::journal::JournalMode::Live {
            continue;
        }
        match intent.step {
            JournalStep::QbitStop => {
                if let Ok(o) = serde_json::from_value::<StopObserved>(intent.observed.clone()) {
                    tx.initial.extend(o.initial_states);
                }
                stopped.extend(intent.hashes.iter().cloned());
                if result.is_none() {
                    // Outcome unknown: the listing below decides (a stopped hash is restored).
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted during the stop; resolved by recovery".into(),
                        },
                    )
                    .await?;
                }
            }
            JournalStep::FsReplaceWithHardlink => {
                // CP2T3-F01: decide from the disk, never from a recorded `Failed` result. Only
                // a definitive `Skipped` ("target unchanged") rules the mutation out.
                let (done, needs_review) = match result.as_ref().map(|r| &r.outcome) {
                    Some(StepOutcome::Done { .. }) => (true, false),
                    Some(StepOutcome::Skipped { .. } | StepOutcome::Simulated) => (false, false),
                    Some(StepOutcome::Failed { .. }) | None => {
                        let observed = ctx.observe_replace(intent).await;
                        let (outcome, done, review) = match observed {
                            Some(true) => (
                                StepOutcome::Done {
                                    final_state: serde_json::json!({"recovered": "target carries the source inode"}),
                                },
                                true,
                                false,
                            ),
                            Some(false) => {
                                out.notes.push(format!(
                                    "action {}: replacement interrupted before the rename; a \
                                     temporary `.<name>.removarr-link-*` link may remain next to \
                                     the target (remove it by hand)",
                                    intent.action.index
                                ));
                                (
                                    StepOutcome::Skipped {
                                        reason:
                                            "interrupted before the replacement; target unchanged"
                                                .into(),
                                    },
                                    false,
                                    false,
                                )
                            }
                            None => (
                                StepOutcome::Failed {
                                    class: ErrorClass::StateChanged,
                                    message: "interrupted; the target could not be observed".into(),
                                },
                                true,
                                true,
                            ),
                        };
                        if result.is_none() {
                            ctx.write_result(intent, outcome).await?;
                        }
                        (done, review)
                    }
                };
                let slot = executed
                    .entry(intent.action.index)
                    .or_insert(Executed::Untouched);
                if needs_review {
                    *slot = Executed::Unobservable;
                } else if done && *slot != Executed::Unobservable {
                    *slot = Executed::Mutated;
                }
                if done {
                    tx.modified.extend(intent.hashes.iter().cloned());
                }
            }
            JournalStep::FsProbeHardlink => {
                let leftover = match result.as_ref().map(|r| &r.outcome) {
                    None => true,
                    Some(StepOutcome::Failed { message, .. }) => {
                        message.contains("removing the probe link")
                    }
                    Some(_) => false,
                };
                if leftover {
                    out.notes.push(format!(
                        "action {}: a hardlink probe did not complete; a temporary \
                         `.<name>.removarr-probe-*` link may remain in the target directory \
                         (remove it by hand)",
                        intent.action.index
                    ));
                }
                if result.is_none() {
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted during the probe".into(),
                        },
                    )
                    .await?;
                }
            }
            JournalStep::QbitRecheck => {
                tx.modified.extend(intent.hashes.iter().cloned());
                if result.is_none() {
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted during the recheck; rechecked by recovery".into(),
                        },
                    )
                    .await?;
                }
            }
            JournalStep::QbitStart => {
                if result.is_none() {
                    unknown_restart.extend(intent.hashes.iter().cloned());
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted during the start; resolved by recovery".into(),
                        },
                    )
                    .await?;
                }
            }
            JournalStep::FsRemoveFile
            | JournalStep::FsRemoveTree
            | JournalStep::FsRemoveEmptyDir
            | JournalStep::FsSetMode => {
                let what = match result.as_ref().map(|r| &r.outcome) {
                    Some(StepOutcome::Done { final_state }) => {
                        // A tree result that did not remove everything is a partial removal.
                        let partial = intent.step == JournalStep::FsRemoveTree
                            && serde_json::from_value::<RemoveObserved>(final_state.clone())
                                .is_ok_and(|o| !o.removed);
                        if partial {
                            Executed::Unobservable
                        } else {
                            Executed::Mutated
                        }
                    }
                    Some(StepOutcome::Skipped { .. } | StepOutcome::Simulated) => {
                        Executed::Untouched
                    }
                    Some(StepOutcome::Failed { .. }) | None => {
                        let what = ctx.observe_fs_intent(intent).await;
                        if result.is_none() {
                            let outcome = match what {
                                Executed::Mutated => StepOutcome::Done {
                                    final_state: serde_json::json!({"recovered": "the target shows the mutation"}),
                                },
                                Executed::Untouched => StepOutcome::Skipped {
                                    reason: "interrupted before the mutation; target unchanged"
                                        .into(),
                                },
                                Executed::Unobservable => StepOutcome::Failed {
                                    class: ErrorClass::StateChanged,
                                    message: "interrupted; the target is partially removed or could not be observed".into(),
                                },
                            };
                            ctx.write_result(intent, outcome).await?;
                        }
                        what
                    }
                };
                mark(&mut executed, intent.action.index, what);
                if what != Executed::Untouched {
                    tx.modified.extend(intent.hashes.iter().cloned());
                }
            }
            JournalStep::QbitRemoveKeepData => {
                // The listing decides; the data is never touched here.
                let gone = match result.as_ref().map(|r| &r.outcome) {
                    Some(StepOutcome::Done { .. }) => true,
                    Some(StepOutcome::Skipped { .. } | StepOutcome::Simulated) => false,
                    Some(StepOutcome::Failed { .. }) | None => {
                        if listing.is_none() {
                            let Some(reader) = &ports.qbit else {
                                out.notes.push(format!(
                                    "action {}: a task removal cannot be checked without \
                                     qBittorrent; its torrents stay stopped",
                                    intent.action.index
                                ));
                                mark(&mut executed, intent.action.index, Executed::Unobservable);
                                continue;
                            };
                            listing = Some(reader.torrents().await?);
                        }
                        let listed = |h: &TorrentHash| {
                            listing
                                .as_ref()
                                .is_some_and(|l| l.iter().any(|t| &t.hash == h))
                        };
                        let gone = !intent.hashes.iter().any(listed);
                        if result.is_none() {
                            let outcome = if gone {
                                StepOutcome::Done {
                                    final_state: serde_json::json!({"recovered": "the task is no longer listed"}),
                                }
                            } else {
                                StepOutcome::Skipped {
                                    reason: "interrupted before the task removal; still listed"
                                        .into(),
                                }
                            };
                            ctx.write_result(intent, outcome).await?;
                        }
                        gone
                    }
                };
                if gone {
                    removed.extend(intent.hashes.iter().cloned());
                    mark(&mut executed, intent.action.index, Executed::Mutated);
                    out.notes.push(format!(
                        "action {}: the torrent task was removed; any data not yet deleted \
                         stays in place as an orphan for a later plan",
                        intent.action.index
                    ));
                }
            }
            JournalStep::ArrRescan | JournalStep::JellyfinRefresh => {
                if result.is_none() {
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted; the command may have been sent".into(),
                        },
                    )
                    .await?;
                    mark(&mut executed, intent.action.index, Executed::Mutated);
                }
            }
            JournalStep::ArrManualImport
            | JournalStep::ArrGrab
            | JournalStep::ArrRemoveQueueItem
            | JournalStep::ArrSetMonitored => {
                // T4-17: re-observe through ARR and the disk, never re-issue a command.
                let (what, write, note) = ctx
                    .observe_arr_intent(intent, result.as_ref().map(|r| &r.outcome))
                    .await;
                if result.is_none() {
                    ctx.write_result(intent, write).await?;
                }
                if let Some(n) = note {
                    out.notes
                        .push(format!("action {}: {n}", intent.action.index));
                }
                mark(&mut executed, intent.action.index, what);
                // Only a moved or unreadable import source can have changed torrent data.
                if what == Executed::Unobservable {
                    tx.modified.extend(intent.hashes.iter().cloned());
                }
            }
        }
    }

    // 2. Torrents: only live reads and a live controller may act. Removed tasks are gone.
    for h in &removed {
        tx.removed(h);
    }
    let hashes: BTreeSet<TorrentHash> = stopped
        .iter()
        .chain(tx.modified.iter())
        .chain(unknown_restart.iter())
        .filter(|h| !removed.contains(h))
        .cloned()
        .collect();
    if !hashes.is_empty() {
        let lead = entries.first().map_or(0, |(i, _)| i.action.index);
        match (&ctx.mutators.qbit, &ports.qbit) {
            (Some(_), Some(reader))
                if ctx.mutators.mode == removarr_domain::plan::PlanMode::Live =>
            {
                let listing = reader.torrents().await?;
                let live_stopped = |h: &TorrentHash| {
                    listing
                        .iter()
                        .find(|t| &t.hash == h)
                        .is_some_and(|t| !was_active(&t.state))
                };
                // Initial state unknown (stop intent never written): take it from the listing
                // only for hashes that are not stopped now, so that no stopped hash of
                // unknown origin is ever started.
                for h in &hashes {
                    tx.initial.entry(h.clone()).or_insert_with(|| {
                        if live_stopped(h) {
                            TorrentState::Unknown("stopped-before-recovery".into())
                        } else {
                            TorrentState::Uploading
                        }
                    });
                }
                // A hash whose data was never modified and that is already running needs
                // nothing; one that is stopped is restored by `restore`.
                tx.initial
                    .retain(|h, _| tx.modified.contains(h) || live_stopped(h));
                ctx.recheck_modified(lead, &mut tx).await?;
                ctx.restore(lead, &mut tx).await?;
                out.resumed = tx.started.clone();
                out.left_stopped = tx.left_stopped.clone();
            }
            _ => {
                for h in &hashes {
                    out.left_stopped
                        .insert(h.clone(), LeftStoppedReason::NoLiveController);
                }
                ctx.reporter.emit(ActionEvent::TorrentsLeftStopped {
                    plan: plan.id,
                    hashes: out.left_stopped.clone(),
                });
            }
        }
    }

    // 3. Action outcomes and the plan status. Outcomes already final stay (Conflict ignored).
    for a in &plan.actions {
        let outcome = match executed.get(&a.index) {
            Some(Executed::Mutated) => ActionOutcome::ExecutedUnverified {
                reason: "interrupted before verification".into(),
            },
            Some(Executed::Unobservable) => ActionOutcome::FailedNeedsReview,
            Some(Executed::Untouched) | None => interrupted(),
        };
        match ports
            .journal
            .record_outcome(ctx.action_ref(a.index), &outcome)
            .await
        {
            Ok(()) => out.actions.push((a.index, outcome)),
            Err(e) if e.class == ErrorClass::Conflict => {}
            Err(e) => return Err(e),
        }
    }
    match ports
        .plans
        .transition(plan.id, PlanStatus::Executing, PlanStatus::Interrupted)
        .await
    {
        Ok(()) => out.status = Some(PlanStatus::Interrupted),
        Err(e) if matches!(e.class, ErrorClass::Conflict | ErrorClass::Validation) => {}
        Err(e) => return Err(e),
    }
    Ok(out)
}

impl Ctx<'_> {
    async fn write_result(
        &self,
        intent: &IntentRecord,
        outcome: StepOutcome,
    ) -> Result<(), PortError> {
        self.ports
            .journal
            .record_result(&ResultRecord {
                intent_id: intent.id,
                outcome: outcome.clone(),
                at: self.ports.clock.now(),
            })
            .await?;
        self.reporter.emit(ActionEvent::StepResult {
            action: intent.action,
            step: intent.step,
            intent: intent.id,
            outcome,
        });
        Ok(())
    }

    /// What ARR and the disk show for a tier-4 command intent, the result to write when none
    /// exists, and a note. A command is never sent again (T4-17).
    async fn observe_arr_intent(
        &self,
        intent: &IntentRecord,
        result: Option<&StepOutcome>,
    ) -> (Executed, StepOutcome, Option<String>) {
        use super::arrcmd::{MonitorObserved, MonitorTarget, QueueTarget, read_monitored};
        use super::replace::ImportTarget;
        let done = |why: &str| StepOutcome::Done {
            final_state: serde_json::json!({ "recovered": why }),
        };
        let skipped = |why: &str| StepOutcome::Skipped { reason: why.into() };
        let unobservable = |why: &str| StepOutcome::Failed {
            class: ErrorClass::StateChanged,
            message: why.into(),
        };
        let ruled_out = matches!(
            result,
            Some(StepOutcome::Skipped { .. } | StepOutcome::Simulated)
        );
        match intent.step {
            JournalStep::ArrManualImport => {
                let Ok(t) = serde_json::from_value::<ImportTarget>(intent.target.clone()) else {
                    return (
                        Executed::Unobservable,
                        unobservable("the import target cannot be read"),
                        None,
                    );
                };
                let mut moved = false;
                for (p, phys) in &t.sources {
                    match self.stat_path(p).await {
                        Ok(Some(s)) if s.phys() == *phys => {}
                        _ => moved = true,
                    }
                }
                if moved {
                    return (
                        Executed::Unobservable,
                        unobservable("ArrImportMoved: a completed source is gone or changed"),
                        Some(
                            "ArrImportMoved: ARR may have moved a completed source; its torrents \
                             stay stopped for review"
                                .into(),
                        ),
                    );
                }
                let what = match result {
                    _ if ruled_out => Executed::Untouched,
                    Some(StepOutcome::Failed { .. }) => Executed::Untouched,
                    _ => Executed::Mutated,
                };
                let note = (what == Executed::Mutated && result.is_none()).then(|| {
                    "the import command may have been sent; it is never sent again".to_string()
                });
                (what, done("the import may have run; sources intact"), note)
            }
            JournalStep::ArrGrab => {
                let what = match result {
                    _ if ruled_out => Executed::Untouched,
                    Some(StepOutcome::Failed { .. }) => Executed::Untouched,
                    _ => Executed::Mutated,
                };
                let note = (what == Executed::Mutated && result.is_none())
                    .then(|| "the grab may have been sent; it is never sent again".to_string());
                (what, done("the grab may have been sent"), note)
            }
            JournalStep::ArrRemoveQueueItem => {
                if matches!(result, Some(StepOutcome::Done { .. })) {
                    return (Executed::Mutated, done("recorded done"), None);
                }
                if ruled_out {
                    return (Executed::Untouched, skipped("not sent"), None);
                }
                let Ok(t) = serde_json::from_value::<QueueTarget>(intent.target.clone()) else {
                    return (
                        Executed::Unobservable,
                        unobservable("unreadable target"),
                        None,
                    );
                };
                let queue = match self.ports.arr.iter().find(|r| r.service() == t.service) {
                    Some(r) => r.queue().await.ok(),
                    None => None,
                };
                match queue {
                    Some(q) if q.iter().all(|i| i.id != t.id) => {
                        (Executed::Mutated, done("the queue item is gone"), None)
                    }
                    Some(_) => (
                        Executed::Untouched,
                        skipped("interrupted before the removal; the item is still queued"),
                        None,
                    ),
                    None => (
                        Executed::Unobservable,
                        unobservable("the queue could not be read"),
                        None,
                    ),
                }
            }
            _ => {
                if matches!(result, Some(StepOutcome::Done { .. })) {
                    return (Executed::Mutated, done("recorded done"), None);
                }
                if ruled_out {
                    return (Executed::Untouched, skipped("not sent"), None);
                }
                let (Ok(t), Ok(o)) = (
                    serde_json::from_value::<MonitorTarget>(intent.target.clone()),
                    serde_json::from_value::<MonitorObserved>(intent.observed.clone()),
                ) else {
                    return (
                        Executed::Unobservable,
                        unobservable("unreadable target"),
                        None,
                    );
                };
                let now = match self
                    .ports
                    .arr
                    .iter()
                    .find(|r| r.service() == t.media.service)
                {
                    Some(r) => read_monitored(&**r, &t.media).await.ok(),
                    None => None,
                };
                match now {
                    Some(v) if v == t.monitored && o.previous != t.monitored => (
                        Executed::Mutated,
                        done("the flag shows the requested value"),
                        None,
                    ),
                    Some(_) => (
                        Executed::Untouched,
                        skipped("interrupted before the change; the flag is unchanged"),
                        None,
                    ),
                    None => (
                        Executed::Unobservable,
                        unobservable("the monitored flag could not be read"),
                        None,
                    ),
                }
            }
        }
    }

    /// What the disk shows for a removal or mode intent (CP2T3-F01, T3-15).
    async fn observe_fs_intent(&self, intent: &IntentRecord) -> Executed {
        if intent.step == JournalStep::FsRemoveTree {
            let Ok(t) = serde_json::from_value::<TreeTarget>(intent.target.clone()) else {
                return Executed::Unobservable;
            };
            let obs = self.observe_tree(&t.snapshot).await;
            return if obs.untouched() {
                Executed::Untouched
            } else if obs.all_gone() {
                Executed::Mutated
            } else {
                Executed::Unobservable
            };
        }
        let Ok(t) = serde_json::from_value::<EntryTarget>(intent.target.clone()) else {
            return Executed::Unobservable;
        };
        match (intent.step, self.stat_path(&t.path).await) {
            (JournalStep::FsSetMode, Ok(Some(s))) => {
                if Some(s.mode) == t.mode {
                    Executed::Mutated
                } else if s.phys() == t.expect.phys() {
                    Executed::Untouched
                } else {
                    Executed::Unobservable
                }
            }
            (JournalStep::FsSetMode, _) => Executed::Unobservable,
            (_, Ok(None)) => Executed::Mutated,
            (_, Ok(Some(s))) if s.phys() == t.expect.phys() => Executed::Untouched,
            (_, _) => Executed::Unobservable,
        }
    }

    /// `Some(true)` when the target of a replace intent carries the source inode now.
    async fn observe_replace(&self, intent: &IntentRecord) -> Option<bool> {
        let t: ReplaceTarget = serde_json::from_value(intent.target.clone()).ok()?;
        let fs = self.ports.fs.clone();
        let target = t.target.clone();
        match self.blocking(move || fs.stat(&target)).await {
            Ok(Some(s)) if s.kind == EntryKind::File => Some(s.phys() == t.expect_source.phys()),
            Ok(Some(_)) => Some(false),
            Ok(None) => None,
            Err(_) => None,
        }
    }
}
