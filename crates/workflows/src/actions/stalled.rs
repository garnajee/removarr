//! `CleanupStalledIncomplete` (tier 3, manual only, OQ-06, ADR-0008): per torrent, stop ->
//! revalidate -> journaled task removal with `deleteFiles=false` (the adapter's literal) ->
//! fresh listing and file list (nobody references the data) -> re-`stat` and confinement ->
//! journaled removals under the incomplete root -> verification (torrent absent, paths
//! absent). A crash after the task removal leaves the data as an orphan for a later manual
//! plan: recovery observes, it never deletes.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::PortError;
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{ActionOutcome, JournalStep};
use removarr_domain::path::RootKind;
use removarr_domain::plan::{ActionKind, ActionTarget, PlanOrigin, PlannedAction};
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::{MappedPath, TorrentHash};

use super::batch::{Batch, Mutation, aborted, not_executed};
use super::delete::{Item, Op, RemoveTaskTarget, op_of};
use super::events::ActionEvent;
use super::journal::{Ctx, StepRun, json};
use super::revalidate::LiveReads;

struct Stalled<'a> {
    action: &'a PlannedAction,
    hash: TorrentHash,
    data: Vec<Item<'a>>,
}

impl Ctx<'_> {
    fn stalled_item<'a>(&self, a: &'a PlannedAction) -> Result<Stalled<'a>, Vec<Blocker>> {
        let (ActionKind::CleanupStalledIncomplete, ActionTarget::Torrent { hash, data_paths }) =
            (a.kind, &a.target)
        else {
            return Err(vec![Blocker::new(
                BlockerCode::NotInBuild,
                "not a stalled-incomplete cleanup",
            )]);
        };
        if self.plan.origin != PlanOrigin::User {
            return Err(vec![Blocker::new(
                BlockerCode::ProtectedZone,
                "stalled-incomplete cleanup is manual only",
            )]);
        }
        let mut blockers = Vec::new();
        let mut data = Vec::new();
        for p in data_paths {
            if p.root.kind() != RootKind::Incomplete || p.rel.is_root() {
                blockers.push(Blocker::new(
                    BlockerCode::ProtectedZone,
                    format!("stalled data is deleted under the incomplete root only: {p}"),
                ));
                continue;
            }
            match op_of(a, p) {
                Some(op @ Op::File(_)) => {
                    blockers.extend(self.sample_blocker(p));
                    data.push(Item {
                        action: a,
                        path: Some(p.clone()),
                        op,
                    });
                }
                Some(op @ Op::Tree(_)) => {
                    if let Op::Tree(snap) = &op {
                        blockers.extend(self.tree_sample_blockers(snap));
                    }
                    data.push(Item {
                        action: a,
                        path: Some(p.clone()),
                        op,
                    });
                }
                _ => blockers.push(Blocker::new(
                    BlockerCode::ChangedSincePlanning,
                    format!("the plan carries no usable signature for {p}"),
                )),
            }
        }
        if !blockers.is_empty() {
            return Err(blockers);
        }
        Ok(Stalled {
            action: a,
            hash: hash.clone(),
            data,
        })
    }

    /// Every file of the torrent must map under the incomplete root (fresh read).
    async fn stalled_extra(&self, live: &mut LiveReads, it: &Stalled<'_>) -> Vec<Blocker> {
        let Some(reader) = &self.ports.qbit else {
            return vec![Blocker::new(
                BlockerCode::ServiceNotConfigured(ServiceKind::Qbittorrent),
                "qBittorrent is not configured",
            )];
        };
        let _ = live;
        match reader.files(&it.hash).await {
            Err(e) => vec![Blocker::new(
                BlockerCode::ServiceUnavailable(ServiceKind::Qbittorrent),
                format!("file list of {}: {}", it.hash.as_str(), e.detail),
            )],
            Ok(files) => {
                let mut out = Vec::new();
                for f in &files {
                    for c in &f.candidates {
                        match c {
                            MappedPath::Mapped { path, .. }
                                if path.root.kind() == RootKind::Incomplete => {}
                            MappedPath::Mapped { path, .. } => out.push(Blocker::new(
                                BlockerCode::ProtectedZone,
                                format!("file {} of the torrent maps outside the incomplete root: {path}", f.index),
                            )),
                            other => out.push(Blocker::new(
                                BlockerCode::MappingError,
                                format!("file {} of the torrent is not mapped: {}", f.index, other.raw()),
                            )),
                        }
                    }
                }
                out
            }
        }
    }

    pub(crate) async fn stalled_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let mut items = Vec::new();
        for a in actions {
            match self.stalled_item(a) {
                Ok(i) => items.push(i),
                Err(bl) => {
                    b.outcomes.insert(a.index, (not_executed(bl), vec![]));
                }
            }
        }
        if items.is_empty() {
            return Ok(b);
        }
        let mut live = LiveReads::default();
        let acts: Vec<&PlannedAction> = items.iter().map(|i| i.action).collect();
        let mut extra = BTreeMap::new();
        for it in &items {
            extra.insert(it.action.index, self.stalled_extra(&mut live, it).await);
        }
        if !self
            .revalidate_pass(&mut live, &acts, 1, &mut b, &extra)
            .await?
        {
            return Ok(b);
        }
        let total = items.len() as u32;
        let mut abort: Option<String> = None;
        for (n, it) in items.iter().enumerate() {
            if self.cancel.is_cancelled() && abort.is_none() {
                abort = Some("cancelled between items".into());
                b.interrupted = true;
                self.reporter.emit(ActionEvent::BatchInterrupted {
                    plan: self.plan.id,
                    reason: "cancelled".into(),
                });
            }
            if let Some(reason) = &abort {
                b.outcomes
                    .insert(it.action.index, (aborted(reason), vec![]));
                continue;
            }
            self.reporter
                .stage("remove_task", n as u32, Some(it.hash.as_str().to_string()));
            self.one_stalled(it, &mut b).await?;
        }
        self.reporter.stage("verify", total, None);
        Ok(b)
    }

    async fn one_stalled(&self, it: &Stalled<'_>, b: &mut Batch) -> Result<(), PortError> {
        let idx = it.action.index;
        let hashes: BTreeSet<TorrentHash> = [it.hash.clone()].into();
        // Stop, then revalidate with fresh reads (state pins included).
        let mut live = LiveReads::default();
        let listing = match self.ports.qbit.as_ref() {
            Some(q) => q.torrents().await.unwrap_or_default(),
            None => Vec::new(),
        };
        if let StepRun::Failed(e) = self
            .stop_torrents(idx, &hashes, &listing, &mut b.tx)
            .await?
        {
            self.restore(idx, &mut b.tx).await?;
            b.outcomes
                .insert(idx, (ActionOutcome::Failed { class: e.class }, vec![]));
            return Ok(());
        }
        let mut extra = BTreeMap::new();
        extra.insert(idx, self.stalled_extra(&mut live, it).await);
        let mut pass = Batch::default();
        if !self
            .revalidate_pass(&mut live, &[it.action], 2, &mut pass, &extra)
            .await?
        {
            self.restore(idx, &mut b.tx).await?;
            b.outcomes.extend(pass.outcomes);
            b.invalidated = true;
            return Ok(());
        }
        // Remove the task (never the data), journaled.
        let ctl = self.mutators.qbit.clone();
        let set = hashes.clone();
        let run = self
            .step(
                idx,
                JournalStep::QbitRemoveKeepData,
                json(&RemoveTaskTarget {
                    hash: it.hash.clone(),
                }),
                serde_json::Value::Null,
                hashes.clone(),
                || async move {
                    let ctl = ctl.ok_or_else(super::qbit::no_controller)?;
                    ctl.remove_tasks_keep_data(&set).await
                },
            )
            .await?;
        match run {
            StepRun::Done(()) | StepRun::Simulated => b.tx.removed(&it.hash),
            StepRun::Failed(e) => {
                self.restore(idx, &mut b.tx).await?;
                b.outcomes
                    .insert(idx, (ActionOutcome::Failed { class: e.class }, vec![]));
                return Ok(());
            }
        }
        // Fresh listing and files: nobody references the data; signatures and confinement
        // hold. Otherwise the data stays as an orphan for a later plan.
        let mut live3 = LiveReads::default();
        let blockers = match self.revalidate_fs_only(&mut live3, it.action).await {
            Ok(bl) => bl,
            Err(e) => vec![Blocker::new(
                BlockerCode::ChangedSincePlanning,
                format!("revalidation after the task removal failed: {}", e.detail),
            )],
        };
        if !blockers.is_empty() {
            b.outcomes.insert(
                idx,
                (
                    ActionOutcome::ExecutedUnverified {
                        reason: format!(
                            "task removed; data left in place as an orphan: {}",
                            blockers
                                .iter()
                                .map(|x| x.detail.clone())
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                    },
                    vec![],
                ),
            );
            return Ok(());
        }
        let mut step_error: Option<PortError> = None;
        let mut partial = false;
        for d in &it.data {
            self.reporter
                .stage("delete", 0, d.path.as_ref().map(ToString::to_string));
            match self.remove_step(d, b).await? {
                Mutation::Done(None) | Mutation::Simulated => {}
                Mutation::Done(Some(e)) => step_error = Some(e),
                Mutation::Partial => partial = true,
                Mutation::Nothing(ActionOutcome::Failed { class }) => {
                    step_error = Some(PortError::new(class, "a data path was not removed"));
                }
                Mutation::Nothing(_) => partial = true,
            }
        }
        let results = self.verify_action(it.action, &b.tx, None).await;
        let outcome = if self.simulated {
            ActionOutcome::Simulated
        } else if partial {
            ActionOutcome::FailedNeedsReview
        } else {
            match (
                step_error,
                removarr_domain::journal::outcome_from_verification(&results),
            ) {
                (Some(e), _) => ActionOutcome::ExecutedUnverified {
                    reason: format!("task removed; a removal failed: {}", e.detail),
                },
                (None, o) => o,
            }
        };
        b.outcomes.insert(idx, (outcome, results));
        Ok(())
    }
}
