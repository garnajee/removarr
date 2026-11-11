//! Verification steps on real re-reads (INV-17). A simulation reports `Simulated` for every
//! step, so `outcome_from_verification` can never yield `Verified` from a dry run.

use removarr_domain::fs::EntryKind;
use removarr_domain::journal::{VerificationResult, VerificationStatus};
use removarr_domain::plan::{PlannedAction, VerificationStep};
use removarr_domain::torrent::RecheckStatus;

use super::journal::Ctx;
use super::qbit::TorrentTx;

impl Ctx<'_> {
    pub(crate) async fn verify_action(
        &self,
        a: &PlannedAction,
        tx: &TorrentTx,
    ) -> Vec<VerificationResult> {
        let mut out = Vec::with_capacity(a.verification.len());
        for step in &a.verification {
            let status = if self.simulated {
                VerificationStatus::Simulated
            } else {
                self.verify_step(step, tx).await
            };
            out.push(VerificationResult {
                step: step.clone(),
                status,
                at: self.ports.clock.now(),
            });
        }
        out
    }

    async fn verify_step(&self, step: &VerificationStep, tx: &TorrentTx) -> VerificationStatus {
        let failed = |reason: String| VerificationStatus::Failed { reason };
        match step {
            VerificationStep::InodeEquals { path, phys } => {
                let fs = self.ports.fs.clone();
                let p = path.clone();
                match self.blocking(move || fs.stat(&p)).await {
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
            _ => VerificationStatus::NotRun {
                reason: "verification step not in this build".to_string(),
            },
        }
    }
}
