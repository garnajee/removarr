//! Recovery after a restart (architecture 7.5): every plan with a live intent that has no
//! result is resolved from observations, never by redoing a mutation.
//!
//! - fs intents without result: the target is `stat`ed and the observed state becomes the
//!   result (the replacement happened, or it did not);
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

fn interrupted() -> ActionOutcome {
    ActionOutcome::NotExecuted {
        blockers: vec![Blocker::new(
            BlockerCode::FailedConsolidation,
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
    let mut executed: BTreeMap<u32, bool> = BTreeMap::new(); // action -> mutation observed
    let mut unknown_restart: BTreeSet<TorrentHash> = BTreeSet::new();

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
                let done = match result.as_ref().map(|r| &r.outcome) {
                    Some(StepOutcome::Done { .. }) => true,
                    Some(_) => false,
                    None => {
                        let observed = ctx.observe_replace(intent).await;
                        let (outcome, mutated) = match observed {
                            Some(true) => (
                                StepOutcome::Done {
                                    final_state: serde_json::json!({"recovered": "target carries the source inode"}),
                                },
                                true,
                            ),
                            Some(false) => (
                                StepOutcome::Skipped {
                                    reason: "interrupted before the replacement; target unchanged"
                                        .into(),
                                },
                                false,
                            ),
                            None => (
                                StepOutcome::Failed {
                                    class: ErrorClass::StateChanged,
                                    message: "interrupted; the target could not be observed".into(),
                                },
                                true,
                            ),
                        };
                        ctx.write_result(intent, outcome).await?;
                        mutated
                    }
                };
                executed
                    .entry(intent.action.index)
                    .and_modify(|m| *m |= done)
                    .or_insert(done);
                if done {
                    tx.modified.extend(intent.hashes.iter().cloned());
                }
            }
            JournalStep::FsProbeHardlink => {
                if result.is_none() {
                    out.notes.push(format!(
                        "action {}: interrupted during a hardlink probe; a temporary \
                         `.<name>.removarr-probe-*` link may remain in the target directory",
                        intent.action.index
                    ));
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
            _ => {
                if result.is_none() {
                    out.notes.push(format!(
                        "action {}: unfinished {:?} intent is not resolvable in this build",
                        intent.action.index, intent.step
                    ));
                    ctx.write_result(
                        intent,
                        StepOutcome::Skipped {
                            reason: "interrupted; not resolvable in this build".into(),
                        },
                    )
                    .await?;
                }
            }
        }
    }

    // 2. Torrents: only live reads and a live controller may act.
    let hashes: BTreeSet<TorrentHash> = stopped
        .iter()
        .chain(tx.modified.iter())
        .chain(unknown_restart.iter())
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
            Some(true) => ActionOutcome::ExecutedUnverified {
                reason: "interrupted before verification".into(),
            },
            _ => interrupted(),
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
