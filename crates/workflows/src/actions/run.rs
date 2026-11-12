//! `execute`: ticket intake, tier gate, plan transitions, dispatch, outcomes, gains.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::gain::{GainKind, GainRecord, ObservedDelta, gain_id};
use removarr_domain::ids::PlanId;
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, VerificationResult};
use removarr_domain::path::{RelPath, RootId};
use removarr_domain::plan::{ActionKind, ActionTarget, PlanMode, PlanStatus, Precondition};
use removarr_domain::ports::CancelFlag;
use removarr_domain::torrent::TorrentHash;
use serde::{Deserialize, Serialize};

use super::events::{ActionEvent, Reporter};
use super::journal::Ctx;
use super::qbit::LeftStoppedReason;
use super::{ActionPorts, ActionSettings, Family, IMPLEMENTED_TIER, Mutators, family, supported};
use crate::plans::ExecutionTicket;
use crate::plans::gate::action_tier;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub index: u32,
    pub kind: ActionKind,
    pub outcome: ActionOutcome,
    pub verification: Vec<VerificationResult>,
}

/// What one run did. For a simulation the outcomes are reported here only (the stored action
/// states and the plan status never change); live outcomes are also persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub plan: PlanId,
    pub simulated: bool,
    /// Final plan status of a live run; `None` for a simulation.
    pub status: Option<PlanStatus>,
    pub actions: Vec<ActionResult>,
    /// Hashes left stopped and visible, with the reason (INV-14, INV-20).
    pub left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
    pub interrupted: bool,
    pub events: Vec<ActionEvent>,
}

impl ExecutionReport {
    #[must_use]
    pub fn verified(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| a.outcome.is_success())
            .count()
    }
}

fn refused(detail: &str) -> PortError {
    PortError::new(ErrorClass::CapabilityDisabled, detail)
}

/// Final plan status of a live run from its outcomes.
fn final_status(outcomes: &[ActionOutcome], invalidated: bool, interrupted: bool) -> PlanStatus {
    if interrupted {
        return PlanStatus::Interrupted;
    }
    if invalidated {
        return PlanStatus::Invalidated;
    }
    let verified = outcomes.iter().filter(|o| o.is_success()).count();
    let executed = outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                ActionOutcome::Verified | ActionOutcome::ExecutedUnverified { .. }
            )
        })
        .count();
    if verified == outcomes.len() {
        PlanStatus::Executed
    } else if executed > 0 {
        PlanStatus::PartiallyExecuted
    } else {
        PlanStatus::Failed
    }
}

/// Executes (or simulates) one plan. `Err` only when the run could not start or the journal
/// failed mid-run; in the latter case a live plan stays `Executing` for recovery.
pub async fn execute(
    ports: &ActionPorts,
    mutators: &Mutators,
    settings: &ActionSettings,
    ticket: ExecutionTicket,
    cancel: &CancelFlag,
    reporter: &Reporter,
) -> Result<ExecutionReport, PortError> {
    execute_gated(
        ports,
        mutators,
        settings,
        ticket,
        cancel,
        reporter,
        IMPLEMENTED_TIER,
    )
    .await
}

/// Test-only entry with an explicit tier gate (lower-gate tests). It exists only under the `tier5-test-gate` feature, which this
/// crate enables solely through its own dev-dependency: no production build contains it.
#[cfg(feature = "tier5-test-gate")]
pub async fn execute_with_gate(
    ports: &ActionPorts,
    mutators: &Mutators,
    settings: &ActionSettings,
    ticket: ExecutionTicket,
    cancel: &CancelFlag,
    reporter: &Reporter,
    gate: removarr_domain::policy::RiskTier,
) -> Result<ExecutionReport, PortError> {
    execute_gated(ports, mutators, settings, ticket, cancel, reporter, gate).await
}

async fn execute_gated(
    ports: &ActionPorts,
    mutators: &Mutators,
    settings: &ActionSettings,
    ticket: ExecutionTicket,
    cancel: &CancelFlag,
    reporter: &Reporter,
    gate: removarr_domain::policy::RiskTier,
) -> Result<ExecutionReport, PortError> {
    let plan = ticket.plan.plan();
    let simulated = ticket.simulate || mutators.mode == PlanMode::DryRun;
    if !ticket.simulate {
        if mutators.mode != PlanMode::Live {
            return Err(refused("a dry-run process can only simulate"));
        }
        if plan.mode != PlanMode::Live {
            return Err(refused("a dry-run plan can only be simulated"));
        }
        if ticket.status != PlanStatus::Approved {
            return Err(PortError::new(
                ErrorClass::Conflict,
                format!("plan is {:?}, not approved", ticket.status),
            ));
        }
        match &ticket.approval {
            Some(a) if &a.plan_hash == ticket.plan.hash() && a.plan_id == plan.id => {}
            _ => {
                return Err(PortError::new(
                    ErrorClass::Conflict,
                    "the approval does not cover this plan hash",
                ));
            }
        }
    } else if !matches!(ticket.status, PlanStatus::Draft | PlanStatus::Approved) {
        return Err(PortError::new(
            ErrorClass::Conflict,
            format!(
                "plan is {:?}; only draft or approved plans simulate",
                ticket.status
            ),
        ));
    }
    // CP2T3-F04: a simulation never holds a live adapter, whatever the caller passed. The
    // recording set replaces the mutators before the context exists.
    let recording = simulated.then(|| {
        let services: Vec<_> = mutators.arr.iter().map(|(s, _)| *s).collect();
        Mutators::recording(&services).0
    });
    let mutators = recording.as_ref().unwrap_or(mutators);
    let ctx = Ctx {
        ports,
        mutators,
        settings,
        plan,
        approval: ticket.approval.as_ref(),
        simulated,
        cancel,
        reporter,
    };
    let total = plan.actions.len() as u32;
    reporter.begin(Some(plan.id), simulated, total);
    reporter.emit(ActionEvent::PlanStarted {
        plan: plan.id,
        simulated,
        actions: total,
    });

    // Tier gate: an unsupported kind, or a plan mixing families, refuses the whole plan
    // before anything happens.
    let unsupported: Vec<&removarr_domain::plan::PlannedAction> = plan
        .actions
        .iter()
        .filter(|a| action_tier(a.kind, plan.origin) > gate || !supported(a.kind))
        .collect();
    let families: BTreeSet<Option<Family>> = plan.actions.iter().map(|a| family(a.kind)).collect();
    let mixed = families.len() > 1;
    let mut defects: BTreeMap<u32, Vec<Blocker>> = mandatory_defects(plan, settings)
        .into_iter()
        .map(|(i, b)| (i, vec![b]))
        .collect();
    // Tier 5 (reachable only through a raised gate): the automatic checks, pure then live.
    if plan.origin == removarr_domain::plan::PlanOrigin::AutoPolicy
        && unsupported.is_empty()
        && !mixed
        && defects.is_empty()
    {
        let now = ports.clock.now();
        defects = super::auto::automatic_defects(
            plan,
            &ticket.plan,
            ticket.approval.as_ref(),
            settings,
            now,
            simulated,
        );
        if defects.is_empty() {
            let actions: Vec<&removarr_domain::plan::PlannedAction> = plan.actions.iter().collect();
            defects = ctx.auto_live_checks(&actions).await;
        }
    }
    if !unsupported.is_empty() || mixed || !defects.is_empty() {
        let outcomes: Vec<(u32, ActionOutcome)> = plan
            .actions
            .iter()
            .map(|a| {
                if let Some(b) = defects.get(&a.index) {
                    return (
                        a.index,
                        ActionOutcome::NotExecuted {
                            blockers: b.clone(),
                        },
                    );
                }
                let detail = if unsupported.iter().any(|u| u.index == a.index) {
                    format!(
                        "{:?} is not executable in this build (implemented tier {:?})",
                        a.kind, gate
                    )
                } else if mixed {
                    "the plan mixes action families; one family per plan".to_string()
                } else {
                    "another action of the plan is not executable in this build".to_string()
                };
                (
                    a.index,
                    ActionOutcome::NotExecuted {
                        blockers: vec![Blocker::new(BlockerCode::NotInBuild, detail)],
                    },
                )
            })
            .collect();
        let status = if simulated {
            None
        } else {
            for (i, o) in &outcomes {
                ports.journal.record_outcome(ctx.action_ref(*i), o).await?;
            }
            ports
                .plans
                .transition(plan.id, PlanStatus::Approved, PlanStatus::Invalidated)
                .await?;
            Some(PlanStatus::Invalidated)
        };
        return Ok(finish(&ctx, status, outcomes, BTreeMap::new(), false));
    }

    let free_before = if simulated {
        BTreeMap::new()
    } else {
        ports
            .plans
            .transition(plan.id, PlanStatus::Approved, PlanStatus::Executing)
            .await?;
        free_space(&ctx, &plan.lock_roots).await
    };

    let actions: Vec<&removarr_domain::plan::PlannedAction> = plan.actions.iter().collect();
    let batch = match families.into_iter().next().flatten() {
        Some(Family::Consolidate) | None => ctx.consolidate(&actions).await?,
        Some(Family::Delete) => ctx.delete_batch(&actions).await?,
        Some(Family::StalledCleanup) => ctx.stalled_batch(&actions).await?,
        Some(Family::FixMode) => ctx.fix_mode_batch(&actions).await?,
        Some(Family::Service) => ctx.service_batch(&actions).await?,
        Some(Family::Replace) => ctx.replace_batch(&actions).await?,
        Some(Family::ArrCommand) => ctx.arr_command_batch(&actions).await?,
    };

    let mut outcomes: Vec<(u32, ActionOutcome)> = Vec::new();
    let mut results: BTreeMap<u32, Vec<VerificationResult>> = BTreeMap::new();
    for a in &plan.actions {
        let (o, v) = batch.outcomes.get(&a.index).cloned().unwrap_or_else(|| {
            (
                ActionOutcome::NotExecuted {
                    blockers: vec![Blocker::new(BlockerCode::Interrupted, "not reached")],
                },
                vec![],
            )
        });
        outcomes.push((a.index, o));
        results.insert(a.index, v);
    }

    let status = if simulated {
        None
    } else {
        for (i, o) in &outcomes {
            ports.journal.record_outcome(ctx.action_ref(*i), o).await?;
        }
        record_gains(&ctx, &outcomes, &batch.added_costs, free_before).await;
        let status = final_status(
            &outcomes.iter().map(|(_, o)| o.clone()).collect::<Vec<_>>(),
            batch.invalidated,
            batch.interrupted,
        );
        ports
            .plans
            .transition(plan.id, PlanStatus::Executing, status)
            .await?;
        Some(status)
    };
    let mut report = finish(
        &ctx,
        status,
        outcomes,
        batch.tx.left_stopped,
        batch.interrupted,
    );
    for a in &mut report.actions {
        a.verification = results.remove(&a.index).unwrap_or_default();
    }
    Ok(report)
}

/// CP2T4-F05: the runner does not trust the planner for the presence of mandatory
/// preconditions. The config fingerprint must match, every enabled capability must be on, and
/// every affected hash needs its wanted-file pin.
fn mandatory_defects(
    plan: &removarr_domain::plan::ActionPlan,
    settings: &ActionSettings,
) -> BTreeMap<u32, Blocker> {
    let mut out = BTreeMap::new();
    for a in &plan.actions {
        if plan.config_fingerprint != settings.config_fingerprint {
            out.insert(
                a.index,
                Blocker::new(
                    BlockerCode::ChangedSincePlanning,
                    "configuration changed since the plan was built; re-plan",
                ),
            );
        } else if let Some(c) = a.kind.capability()
            && !settings.capabilities.contains(&c)
        {
            out.insert(
                a.index,
                Blocker::new(
                    BlockerCode::CapabilityDisabled,
                    format!("capability {} is disabled", c.as_str()),
                ),
            );
        } else if let Some(h) = a
            .torrent_hashes
            .iter()
            .filter(|_| crate::plans::build::pins_torrents(a.kind))
            .find(|h| {
            !a.preconditions.iter().any(|p| {
                matches!(p, removarr_domain::plan::Precondition::TorrentWantedFiles { hash, .. } if hash == *h)
            })
        }) {
            out.insert(
                a.index,
                Blocker::new(
                    BlockerCode::ChangedSincePlanning,
                    format!("the plan lacks the wanted-file pin of torrent {}", h.as_str()),
                ),
            );
        }
    }
    out
}

fn finish(
    ctx: &Ctx<'_>,
    status: Option<PlanStatus>,
    outcomes: Vec<(u32, ActionOutcome)>,
    left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>,
    interrupted: bool,
) -> ExecutionReport {
    let plan = ctx.plan;
    let actions = outcomes
        .into_iter()
        .map(|(index, outcome)| {
            ctx.reporter.emit(ActionEvent::ActionFinished {
                action: ctx.action_ref(index),
                outcome: outcome.clone(),
            });
            ActionResult {
                index,
                kind: plan
                    .actions
                    .get(index as usize)
                    .map_or(ActionKind::ConsolidateHardlink, |a| a.kind),
                outcome,
                verification: vec![],
            }
        })
        .collect();
    ctx.reporter.emit(ActionEvent::PlanFinished {
        plan: plan.id,
        status,
    });
    ctx.reporter
        .stage("finished", plan.actions.len() as u32, None);
    ExecutionReport {
        plan: plan.id,
        simulated: ctx.simulated,
        status,
        actions,
        left_stopped,
        interrupted,
        events: ctx.reporter.events(),
    }
}

async fn free_space(ctx: &Ctx<'_>, roots: &BTreeSet<RootId>) -> BTreeMap<RootId, u64> {
    let mut out = BTreeMap::new();
    for r in roots {
        let fs = ctx.ports.fs.clone();
        let id = r.clone();
        if let Ok(Ok(v)) = tokio::task::spawn_blocking(move || fs.free_space(&id)).await {
            out.insert(r.clone(), v);
        }
    }
    out
}

/// Attributable gains of verified consolidations (the replaced media inode) and deletions
/// (the removed target: a file's inode, or the directory inode of a removed tree or of the
/// first data path of a stalled cleanup, with the action's planned attributable bytes), each
/// counted once by its gain id, plus the observed free-space delta. A duplicate gain id is a
/// `Conflict` the store refuses: it is logged, never counted twice.
async fn record_gains(
    ctx: &Ctx<'_>,
    outcomes: &[(u32, ActionOutcome)],
    added_costs: &[(u32, removarr_domain::fs::EntrySignature)],
    free_before: BTreeMap<RootId, u64>,
) {
    let now = ctx.ports.clock.now();
    let mut records = Vec::new();
    let mut roots = BTreeSet::new();
    // T4-15: library files ARR copied instead of hardlinking, one record per new inode.
    for (index, sig) in added_costs {
        records.push(GainRecord {
            gain_id: gain_id(sig.phys(), sig.size, sig.mtime_ns),
            action: ctx.action_ref(*index),
            kind: GainKind::AddedCost,
            phys: sig.phys(),
            bytes: sig.size,
            recorded_at: now,
        });
    }
    for (index, o) in outcomes {
        if !o.is_success() {
            continue;
        }
        let Some(a) = ctx.plan.actions.get(*index as usize) else {
            continue;
        };
        let (target, kind) = match (a.kind, &a.target) {
            (ActionKind::ConsolidateHardlink, ActionTarget::Consolidate { target, .. }) => {
                (target, GainKind::AttributableConsolidation)
            }
            (
                ActionKind::DeleteCompletedFile
                | ActionKind::DeleteCompletedFolder
                | ActionKind::DeleteJunk
                | ActionKind::DeleteIncompleteOrphan
                | ActionKind::DeleteMediaFile,
                ActionTarget::Path { path },
            ) => (path, GainKind::AttributableDeletion),
            (ActionKind::CleanupStalledIncomplete, ActionTarget::Torrent { data_paths, .. }) => {
                match data_paths.first() {
                    Some(p) => (p, GainKind::AttributableDeletion),
                    None => continue,
                }
            }
            _ => continue,
        };
        roots.insert(target.root.clone());
        let sig = a.preconditions.iter().find_map(|p| match p {
            Precondition::EntryUnchanged { path, sig, .. } if path == target => Some(*sig),
            Precondition::DirUnchanged { snapshot } if &snapshot.dir == target => {
                snapshot.entries.get(&RelPath::root()).copied()
            }
            _ => None,
        });
        if let Some(sig) = sig
            && a.expected_gain.attributable > 0
        {
            records.push(GainRecord {
                gain_id: gain_id(sig.phys(), sig.size, sig.mtime_ns),
                action: ctx.action_ref(*index),
                kind,
                phys: sig.phys(),
                bytes: a.expected_gain.attributable,
                recorded_at: now,
            });
        }
    }
    if records.is_empty() {
        return;
    }
    let after = free_space(ctx, &roots).await;
    let delta = roots.iter().find_map(|r| {
        Some(ObservedDelta {
            root: r.clone(),
            free_before: *free_before.get(r)?,
            free_after_local: *after.get(r)?,
            free_after_services: None,
        })
    });
    if let Err(e) = ctx.ports.gains.record(&records, delta.as_ref()).await {
        tracing::warn!(class = ?e.class, "gain record refused: {}", e.detail);
    }
}
