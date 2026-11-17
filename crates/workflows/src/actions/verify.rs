//! Verification steps on real re-reads (INV-17). A simulation reports `Simulated` for every
//! step, so `outcome_from_verification` can never yield `Verified` from a dry run.
//!
//! `Absent` passes only on a confirmed `NotFound` (a permission or other error is `NotRun`),
//! `TorrentAbsent` reads a fresh listing, `ModeEquals` and `InodeEquals` `stat` the path, the
//! service steps are judged by `evaluate_service_step` on the observation the batch collected.

use removarr_domain::fs::EntryKind;
use removarr_domain::journal::{
    ServiceObservation, VerificationResult, VerificationStatus, evaluate_service_step,
};
use removarr_domain::plan::{PlannedAction, VerificationStep};
use removarr_domain::torrent::RecheckStatus;

use super::journal::Ctx;
use super::qbit::TorrentTx;

impl Ctx<'_> {
    pub(crate) async fn verify_action(
        &self,
        a: &PlannedAction,
        tx: &TorrentTx,
        service: Option<&ServiceObservation>,
    ) -> Vec<VerificationResult> {
        let mut out = Vec::with_capacity(a.verification.len());
        for step in &a.verification {
            let status = if self.simulated {
                VerificationStatus::Simulated
            } else {
                self.verify_step(step, tx, service).await
            };
            out.push(VerificationResult {
                step: step.clone(),
                status,
                at: self.ports.clock.now(),
            });
        }
        out
    }

    async fn verify_step(
        &self,
        step: &VerificationStep,
        tx: &TorrentTx,
        service: Option<&ServiceObservation>,
    ) -> VerificationStatus {
        let failed = |reason: String| VerificationStatus::Failed { reason };
        match step {
            VerificationStep::InodeEquals { path, phys } => match self.stat_path(path).await {
                Ok(Some(s)) if s.kind == EntryKind::File && s.phys() == *phys => {
                    VerificationStatus::Passed
                }
                Ok(Some(s)) if s.kind != EntryKind::File => {
                    failed(format!("{path} is not a regular file"))
                }
                Ok(Some(_)) => failed(format!("{path} does not carry the expected inode")),
                Ok(None) => failed(format!("{path} is missing")),
                Err(e) => VerificationStatus::NotRun {
                    reason: format!("{path}: {}", e.detail),
                },
            },
            VerificationStep::Absent { path } => match self.stat_path(path).await {
                Ok(None) => VerificationStatus::Passed,
                Ok(Some(_)) => failed(format!("{path} still exists")),
                Err(e) => VerificationStatus::NotRun {
                    reason: format!("{path}: {}", e.detail),
                },
            },
            VerificationStep::ModeEquals { path, mode } => match self.stat_path(path).await {
                Ok(Some(s)) if s.mode == *mode => VerificationStatus::Passed,
                Ok(Some(s)) => failed(format!("{path} has mode {:o}, expected {mode:o}", s.mode)),
                Ok(None) => failed(format!("{path} is missing")),
                Err(e) => VerificationStatus::NotRun {
                    reason: format!("{path}: {}", e.detail),
                },
            },
            VerificationStep::TorrentAbsent { hashes } => {
                let Some(reader) = &self.ports.qbit else {
                    return VerificationStatus::NotRun {
                        reason: "qBittorrent is not configured".into(),
                    };
                };
                match reader.torrents().await {
                    Ok(list) => {
                        let present: Vec<&str> = hashes
                            .iter()
                            .filter(|h| list.iter().any(|t| &t.hash == *h))
                            .map(|h| h.as_str())
                            .collect();
                        if present.is_empty() {
                            VerificationStatus::Passed
                        } else {
                            failed(format!("still listed: {}", present.join(", ")))
                        }
                    }
                    Err(e) => VerificationStatus::NotRun {
                        reason: format!("qBittorrent listing: {}", e.detail),
                    },
                }
            }
            VerificationStep::RecheckVerified { hashes } => {
                for h in hashes {
                    if !tx.modified.contains(h) {
                        continue;
                    }
                    match tx.recheck.get(h) {
                        Some(RecheckStatus::Verified) => {}
                        Some(RecheckStatus::TimedOut) => {
                            return failed(format!("recheck of {} timed out", h.as_str()));
                        }
                        Some(RecheckStatus::Failed) => {
                            return failed(format!("recheck of {} failed", h.as_str()));
                        }
                        Some(RecheckStatus::Pending) | None => {
                            return VerificationStatus::NotRun {
                                reason: format!("no recheck result for {}", h.as_str()),
                            };
                        }
                    }
                }
                VerificationStatus::Passed
            }
            VerificationStep::ArrCommandCompleted { .. }
            | VerificationStep::JellyfinRefreshed { .. }
            | VerificationStep::ArrQueueItemAbsent { .. }
            | VerificationStep::ArrMonitored { .. } => match service {
                Some(obs) => evaluate_service_step(step, obs),
                None => VerificationStatus::NotRun {
                    reason: "no service observation was collected".to_string(),
                },
            },
            _ => VerificationStatus::NotRun {
                reason: "verification step not in this build".to_string(),
            },
        }
    }
}
