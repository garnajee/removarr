//! Single-step actions: `FixMode` (tier 3, T3-18: targeted, never recursive, owner only,
//! signature revalidated, `ModeEquals` verified) and the tier-2 service commands `ArrRescan`
//! and `JellyfinRefresh` (journaled command, then the completion is observed by polling the
//! service, bounded by `recheck_timeout`; `evaluate_service_step` judges the observation).

use std::collections::BTreeMap;

use removarr_domain::arr::{ArrMediaRef, ArrService, CommandState};
use removarr_domain::error::PortError;
use removarr_domain::fs::EntryKind;
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, JournalStep, ServiceObservation, StepOutcome};
use removarr_domain::plan::{ActionKind, ActionTarget, PlannedAction, Precondition};
use serde::{Deserialize, Serialize};

use super::batch::{Batch, aborted, not_executed};
use super::delete::EntryTarget;
use super::journal::{Ctx, StepRun, json};
use super::revalidate::LiveReads;

/// `target` payload of an `ArrRescan` intent; the result carries the command id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RescanTarget {
    pub service: ArrService,
    pub media: Option<ArrMediaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CommandStarted {
    pub command_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RefreshTarget {
    pub item_id: Option<String>,
}

/// `final_state` of a `FsSetMode` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModeObserved {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Ctx<'_> {
    pub(crate) async fn fix_mode_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let mut items = Vec::new();
        for a in actions {
            let (ActionKind::FixMode, ActionTarget::FixMode { path, mode }) = (a.kind, &a.target)
            else {
                b.outcomes.insert(
                    a.index,
                    (
                        not_executed(vec![Blocker::new(
                            BlockerCode::NotInBuild,
                            "not a mode fix",
                        )]),
                        vec![],
                    ),
                );
                continue;
            };
            let sig = a.preconditions.iter().find_map(|p| match p {
                Precondition::EntryUnchanged { path: q, sig, .. } if q == path => Some(*sig),
                _ => None,
            });
            match sig {
                Some(sig) if matches!(sig.kind, EntryKind::File | EntryKind::Dir) => {
                    items.push((*a, path.clone(), sig, *mode));
                }
                _ => {
                    b.outcomes.insert(
                        a.index,
                        (
                            not_executed(vec![Blocker::new(
                                BlockerCode::ChangedSincePlanning,
                                format!("the plan carries no usable signature for {path}"),
                            )]),
                            vec![],
                        ),
                    );
                }
            }
        }
        if items.is_empty() {
            return Ok(b);
        }
        let mut live = LiveReads::default();
        let acts: Vec<&PlannedAction> = items.iter().map(|i| i.0).collect();
        if !self
            .revalidate_pass(&mut live, &acts, 1, &mut b, &BTreeMap::new())
            .await?
        {
            return Ok(b);
        }
        for (n, (a, path, sig, mode)) in items.iter().enumerate() {
            if self.cancel.is_cancelled() {
                b.interrupted = true;
                b.outcomes
                    .insert(a.index, (aborted("cancelled between items"), vec![]));
                continue;
            }
            self.reporter
                .stage("fix_mode", n as u32, Some(path.to_string()));
            let intent = self
                .intent(
                    a.index,
                    JournalStep::FsSetMode,
                    json(&EntryTarget {
                        path: path.clone(),
                        expect: *sig,
                        mode: Some(*mode),
                    }),
                    serde_json::Value::Null,
                    a.torrent_hashes.clone(),
                )
                .await?;
            if self.simulated {
                self.result(
                    intent,
                    a.index,
                    JournalStep::FsSetMode,
                    StepOutcome::Simulated,
                )
                .await?;
                b.outcomes
                    .insert(a.index, (ActionOutcome::Simulated, vec![]));
                continue;
            }
            let fs = self.mutators.fs.clone();
            let (p, s, m) = (path.clone(), *sig, *mode);
            let call = self.blocking(move || fs.set_mode(&p, &s, m)).await;
            let (record, error) = match call {
                Ok(()) => (
                    StepOutcome::Done {
                        final_state: json(&ModeObserved {
                            applied: true,
                            error: None,
                        }),
                    },
                    None,
                ),
                Err(e) => match self.stat_path(path).await {
                    Ok(Some(now)) if now.mode == *mode => (
                        StepOutcome::Done {
                            final_state: json(&ModeObserved {
                                applied: true,
                                error: Some(e.detail.clone()),
                            }),
                        },
                        Some(e),
                    ),
                    _ => (
                        StepOutcome::Failed {
                            class: e.class.clone(),
                            message: format!("{} (mode unchanged)", e.detail),
                        },
                        Some(e),
                    ),
                },
            };
            let mutated = matches!(record, StepOutcome::Done { .. });
            self.result(intent, a.index, JournalStep::FsSetMode, record)
                .await?;
            let outcome = match (mutated, error) {
                (false, Some(e)) => ActionOutcome::Failed { class: e.class },
                (true, Some(e)) => ActionOutcome::ExecutedUnverified {
                    reason: format!("step failed: {}", e.detail),
                },
                (_, None) => {
                    let results = self.verify_action(a, &b.tx, None).await;
                    let o = removarr_domain::journal::outcome_from_verification(&results);
                    b.outcomes.insert(a.index, (o, results));
                    continue;
                }
            };
            b.outcomes.insert(a.index, (outcome, vec![]));
        }
        Ok(b)
    }

    pub(crate) async fn service_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let mut live = LiveReads::default();
        if !self
            .revalidate_pass(&mut live, actions, 1, &mut b, &BTreeMap::new())
            .await?
        {
            return Ok(b);
        }
        for (n, a) in actions.iter().enumerate() {
            if self.cancel.is_cancelled() {
                b.interrupted = true;
                b.outcomes
                    .insert(a.index, (aborted("cancelled between items"), vec![]));
                continue;
            }
            self.reporter.stage("service", n as u32, None);
            let (outcome, results) = match &a.target {
                ActionTarget::Rescan { service, media } => {
                    self.arr_rescan(a, *service, media.as_ref()).await?
                }
                ActionTarget::JellyfinRefresh { item_id } => {
                    self.jellyfin_refresh(a, item_id.as_deref()).await?
                }
                _ => (
                    not_executed(vec![Blocker::new(
                        BlockerCode::NotInBuild,
                        "not a service command",
                    )]),
                    vec![],
                ),
            };
            b.outcomes.insert(a.index, (outcome, results));
        }
        Ok(b)
    }

    async fn arr_rescan(
        &self,
        a: &PlannedAction,
        service: ArrService,
        media: Option<&ArrMediaRef>,
    ) -> Result<
        (
            ActionOutcome,
            Vec<removarr_domain::journal::VerificationResult>,
        ),
        PortError,
    > {
        let cmd = self
            .mutators
            .arr
            .iter()
            .find(|(s, _)| *s == service)
            .map(|(_, c)| c.clone());
        let m = media.cloned();
        let run = self
            .step(
                a.index,
                JournalStep::ArrRescan,
                json(&RescanTarget {
                    service,
                    media: m.clone(),
                }),
                serde_json::Value::Null,
                a.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(
                            removarr_domain::error::ErrorClass::Config,
                            format!("no live {} command adapter", service.as_str()),
                        )
                    })?;
                    let id = match &m {
                        Some(media) => cmd.rescan(media).await?,
                        None => cmd.rescan_all().await?,
                    };
                    Ok(CommandStarted { command_id: id })
                },
            )
            .await?;
        let observation = match run {
            StepRun::Simulated => {
                return Ok((ActionOutcome::Simulated, vec![]));
            }
            StepRun::Failed(e) => {
                return Ok((ActionOutcome::Failed { class: e.class }, vec![]));
            }
            StepRun::Done(started) => self.observe_arr_command(service, started.command_id).await,
        };
        let results = self
            .verify_action(a, &Default::default(), Some(&observation))
            .await;
        Ok((
            removarr_domain::journal::outcome_from_verification(&results),
            results,
        ))
    }

    /// Polls `command_status` until the command is finished or `recheck_timeout` elapses.
    async fn observe_arr_command(&self, service: ArrService, id: i64) -> ServiceObservation {
        let Some(reader) = self.ports.arr.iter().find(|r| r.service() == service) else {
            return ServiceObservation::Unobserved {
                reason: format!("{} has no read adapter", service.as_str()),
            };
        };
        let started = tokio::time::Instant::now();
        loop {
            match reader.command_status(id).await {
                Ok(CommandState::Completed) => return ServiceObservation::CommandCompleted,
                Ok(CommandState::Failed { message }) => {
                    return ServiceObservation::CommandFailed { reason: message };
                }
                Ok(CommandState::Queued | CommandState::Started) => {}
                Ok(CommandState::Unknown(s)) => {
                    return ServiceObservation::Unobserved {
                        reason: format!("unknown command state {s}"),
                    };
                }
                Err(e) => {
                    return ServiceObservation::Unobserved {
                        reason: format!("command status: {}", e.detail),
                    };
                }
            }
            if started.elapsed() >= self.settings.recheck_timeout {
                return ServiceObservation::Unobserved {
                    reason: "the command was still running at the timeout".into(),
                };
            }
            tokio::time::sleep(self.settings.recheck_interval).await;
        }
    }

    async fn jellyfin_refresh(
        &self,
        a: &PlannedAction,
        item_id: Option<&str>,
    ) -> Result<
        (
            ActionOutcome,
            Vec<removarr_domain::journal::VerificationResult>,
        ),
        PortError,
    > {
        let cmd = self.mutators.jellyfin.clone();
        let id = item_id.map(ToString::to_string);
        let run = self
            .step(
                a.index,
                JournalStep::JellyfinRefresh,
                json(&RefreshTarget {
                    item_id: id.clone(),
                }),
                serde_json::Value::Null,
                a.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(
                            removarr_domain::error::ErrorClass::Config,
                            "no live Jellyfin command adapter",
                        )
                    })?;
                    match &id {
                        Some(i) => cmd.refresh_item(i).await,
                        None => cmd.refresh_library().await,
                    }
                },
            )
            .await?;
        let observation = match run {
            StepRun::Simulated => return Ok((ActionOutcome::Simulated, vec![])),
            StepRun::Failed(e) => return Ok((ActionOutcome::Failed { class: e.class }, vec![])),
            StepRun::Done(()) => self.observe_jellyfin_refresh().await,
        };
        let results = self
            .verify_action(a, &Default::default(), Some(&observation))
            .await;
        Ok((
            removarr_domain::journal::outcome_from_verification(&results),
            results,
        ))
    }

    /// Polls `library_refresh_running` until it reads `false` or `recheck_timeout` elapses.
    async fn observe_jellyfin_refresh(&self) -> ServiceObservation {
        let Some(reader) = &self.ports.jellyfin else {
            return ServiceObservation::Unobserved {
                reason: "Jellyfin has no read adapter".into(),
            };
        };
        let started = tokio::time::Instant::now();
        loop {
            match reader.library_refresh_running().await {
                Ok(false) => return ServiceObservation::CommandCompleted,
                Ok(true) => {}
                Err(e) => {
                    return ServiceObservation::Unobserved {
                        reason: format!("refresh status: {}", e.detail),
                    };
                }
            }
            if started.elapsed() >= self.settings.recheck_timeout {
                return ServiceObservation::Unobserved {
                    reason: "the refresh was still running at the timeout".into(),
                };
            }
            tokio::time::sleep(self.settings.recheck_interval).await;
        }
    }
}
