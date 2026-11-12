//! Verified release replacement (tier 4; architecture 7.4 `ReplaceRelease`, ADR-0010,
//! CP2-tier4 T4-01..T4-11, T4-13): ARR's manual import copies the completed release into the
//! library (the adapter always sends `importMode = "copy"`), and ARR's upgrade deletes the old
//! media file. Nothing here builds an ARR body; the runner only judges what the disk and ARR
//! show afterwards.
//!
//! Order: items (mandatory preconditions present, completed sources only, unrecoverable old
//! files acknowledged, samples) -> pass 1 -> ARR preview re-check (every candidate still
//! accepted with the planned episodes) -> journaled stop -> pass 2 -> `NoArrImport` read
//! immediately before the command -> journaled `ArrManualImport` -> source inode check (a move
//! is `FailedNeedsReview` + `ArrImportMoved`, everything else is cancelled) -> recheck of what
//! was modified, restore of what is proven intact -> verification by re-read: ARR's current file
//! is a new file (a), the source keeps its `(dev, ino)` (b), the library file shares it (c).
//! (a) and (b) without (c) is `ExecutedUnverified` with an `AddedCost`. `DeleteMediaFile`
//! actions run last, only after their import is `Verified` in this very run, through the
//! dedicated `remove_media_file` entry point.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::correlate::import_in_progress;
use removarr_domain::arr::{
    ArrMediaRef, ArrService, CommandState, ManualImportRequest, OldFileRecovery,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::fs::{EntryKind, EntrySignature, PhysicalId};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{
    ActionOutcome, JournalStep, ServiceObservation, StepOutcome, VerificationResult,
    VerificationStatus, outcome_from_verification,
};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionTarget, PlannedAction, Precondition, VerificationStep,
};
use removarr_domain::torrent::{MappedPath, TorrentHash};
use serde::{Deserialize, Serialize};

use super::batch::{Batch, aborted, not_executed};
use super::delete::EntryTarget;
use super::journal::{Ctx, StepRun, json};
use super::revalidate::LiveReads;

/// `target` payload of an `ArrManualImport` intent: what recovery re-observes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ImportTarget {
    pub service: ArrService,
    pub request: ManualImportRequest,
    /// Every completed source with the inode it must keep (a move is detected by it).
    pub sources: Vec<(RootedPath, PhysicalId)>,
    /// ARR's fingerprint right before the command (a failed call is judged against it).
    pub fingerprint_before: String,
}

/// `final_state` of an `ArrManualImport` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ImportDone {
    pub command_id: Option<i64>,
    /// `completed`, `failed`, `unobserved` or `observed_by_fingerprint`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One replacement, extracted and checked.
struct Replace<'a> {
    action: &'a PlannedAction,
    service: ArrService,
    media: ArrMediaRef,
    request: ManualImportRequest,
    sources: Vec<(RootedPath, EntrySignature)>,
    old: Vec<(RootedPath, PhysicalId, OldFileRecovery)>,
}

struct DeleteOld<'a> {
    action: &'a PlannedAction,
    path: RootedPath,
    expect: EntrySignature,
}

fn missing(what: &str) -> Blocker {
    Blocker::new(
        BlockerCode::ChangedSincePlanning,
        format!("the plan lacks a mandatory precondition: {what}"),
    )
}

/// What one replacement showed after the import.
struct Observed {
    results: Vec<VerificationResult>,
    moved: bool,
    shared: bool,
    new_ok: bool,
    source_ok: bool,
    /// Signature of the new library file, for `AddedCost`.
    added: Vec<EntrySignature>,
}

impl Ctx<'_> {
    fn replace_item<'a>(&self, a: &'a PlannedAction) -> Result<Replace<'a>, Vec<Blocker>> {
        let ActionTarget::Replace { request } = &a.target else {
            return Err(vec![missing("a replacement request")]);
        };
        let Some((_, first)) = request.files.first() else {
            return Err(vec![missing("at least one file to import")]);
        };
        let service = first.service;
        let media = ArrMediaRef {
            service,
            media_id: first.media_id,
            episode_ids: vec![],
        };
        let mut blockers = Vec::new();
        let mut sources = Vec::new();
        for (path, _) in &request.files {
            if path.root.kind() != RootKind::Completed {
                blockers.push(Blocker::new(
                    BlockerCode::ProtectedZone,
                    format!("{path} is not under the completed root; only completed releases are imported"),
                ));
                continue;
            }
            blockers.extend(self.sample_blocker(path));
            let sig = a.preconditions.iter().find_map(|p| match p {
                Precondition::EntryUnchanged {
                    path: q,
                    sig,
                    compare_nlink: false,
                } if q == path => Some(*sig),
                _ => None,
            });
            match sig {
                Some(s) if s.kind == EntryKind::File => sources.push((path.clone(), s)),
                _ => blockers.push(missing(&format!("the signature of {path}"))),
            }
            if !a
                .preconditions
                .iter()
                .any(|p| matches!(p, Precondition::SameDevice { a: x, .. } if x == path))
            {
                blockers.push(missing(&format!("the same-device check of {path}")));
            }
        }
        let has = |f: &dyn Fn(&Precondition) -> bool| a.preconditions.iter().any(f);
        if !has(&|p| {
            matches!(p, Precondition::ArrFingerprint { media: m, .. }
                if m.service == service && m.media_id == media.media_id)
        }) {
            blockers.push(missing("ArrFingerprint"));
        }
        if !has(&|p| {
            matches!(p, Precondition::NoArrImport { media: m, .. }
                if m.service == service && m.media_id == media.media_id)
        }) {
            blockers.push(missing("NoArrImport"));
        }
        let old = a
            .preconditions
            .iter()
            .find_map(|p| match p {
                Precondition::OldCoverageRecoverable { old_files } => Some(old_files.clone()),
                _ => None,
            })
            .unwrap_or_else(|| {
                blockers.push(missing("OldCoverageRecoverable"));
                Vec::new()
            });
        // T4-07: every unrecoverable old file needs its own acknowledgement (a simulation
        // without an approval shows the blocker too).
        for (path, _, recovery) in &old {
            if *recovery == OldFileRecovery::Unrecoverable
                && !self
                    .approval
                    .is_some_and(|ap| ap.acknowledged_unrecoverable.contains(path))
            {
                blockers.push(Blocker::new(
                    BlockerCode::OldCoverageUnrecoverable,
                    format!("{path} would be lost and is not acknowledged in the approval"),
                ));
            }
        }
        if !self.simulated && !self.mutators.arr.iter().any(|(s, _)| *s == service) {
            blockers.push(Blocker::new(
                BlockerCode::ServiceNotConfigured(match service {
                    ArrService::Sonarr => removarr_domain::session::ServiceKind::Sonarr,
                    ArrService::Radarr => removarr_domain::session::ServiceKind::Radarr,
                }),
                "no live ARR command adapter",
            ));
        }
        if blockers.is_empty() {
            Ok(Replace {
                action: a,
                service,
                media,
                request: request.clone(),
                sources,
                old,
            })
        } else {
            Err(blockers)
        }
    }

    fn media_delete_item<'a>(&self, a: &'a PlannedAction) -> Result<DeleteOld<'a>, Vec<Blocker>> {
        let ActionTarget::Path { path } = &a.target else {
            return Err(vec![missing("a media path")]);
        };
        if path.root.kind() != RootKind::Media || path.rel.is_root() {
            return Err(vec![Blocker::new(
                BlockerCode::ProtectedZone,
                format!("DeleteMediaFile only removes files under a media root: {path}"),
            )]);
        }
        let expect = a.preconditions.iter().find_map(|p| match p {
            Precondition::EntryUnchanged {
                path: q,
                sig,
                compare_nlink: true,
            } if q == path => Some(*sig),
            _ => None,
        });
        match expect {
            Some(s) if s.kind == EntryKind::File => Ok(DeleteOld {
                action: a,
                path: path.clone(),
                expect: s,
            }),
            _ => Err(vec![missing(&format!(
                "EntryUnchanged with the link count of {path}"
            ))]),
        }
    }

    /// The replacement family of a plan.
    pub(crate) async fn replace_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let mut replaces = Vec::new();
        let mut deletes = Vec::new();
        for a in actions {
            let r = match a.kind {
                ActionKind::ReplaceRelease => self.replace_item(a).map(|i| replaces.push(i)),
                ActionKind::DeleteMediaFile => self.media_delete_item(a).map(|i| deletes.push(i)),
                _ => Err(vec![Blocker::new(
                    BlockerCode::NotInBuild,
                    "not a replacement",
                )]),
            };
            if let Err(bl) = r {
                b.outcomes.insert(a.index, (not_executed(bl), vec![]));
            }
        }
        if !b.outcomes.is_empty() || replaces.is_empty() {
            // One refused action refuses the plan: a replacement is global (INV-35).
            for a in actions {
                b.outcomes.entry(a.index).or_insert_with(|| {
                    (
                        not_executed(vec![Blocker::new(
                            BlockerCode::ChangedSincePlanning,
                            "another action of the plan was refused; the plan is void",
                        )]),
                        vec![],
                    )
                });
            }
            b.invalidated = true;
            return Ok(b);
        }

        // 1. Revalidate everything, then ARR's own preview of every candidate.
        let mut live = LiveReads::default();
        let acts: Vec<&PlannedAction> = actions.to_vec();
        let mut extra: BTreeMap<u32, Vec<Blocker>> = BTreeMap::new();
        for r in &replaces {
            let bl = self.preview_blockers(r).await;
            if !bl.is_empty() {
                extra.insert(r.action.index, bl);
            }
        }
        if !self
            .revalidate_pass(&mut live, &acts, 1, &mut b, &extra)
            .await?
        {
            return Ok(b);
        }

        // 2. Stop every affected hash (journaled), then revalidate the replacements again.
        if self.cancel.is_cancelled() {
            for a in actions {
                b.outcomes
                    .insert(a.index, (aborted("cancelled before the stop"), vec![]));
            }
            b.interrupted = true;
            return Ok(b);
        }
        let lead = replaces[0].action.index;
        let hashes: BTreeSet<TorrentHash> = replaces
            .iter()
            .flat_map(|r| r.action.torrent_hashes.iter().cloned())
            .collect();
        let listing = live.listing().to_vec();
        if let StepRun::Failed(e) = self
            .stop_torrents(lead, &hashes, &listing, &mut b.tx)
            .await?
        {
            self.restore(lead, &mut b.tx).await?;
            for a in actions {
                b.outcomes.insert(
                    a.index,
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
        let rep_actions: Vec<&PlannedAction> = replaces.iter().map(|r| r.action).collect();
        if !self
            .revalidate_pass(&mut live2, &rep_actions, 2, &mut b, &BTreeMap::new())
            .await?
        {
            self.restore(lead, &mut b.tx).await?;
            for d in &deletes {
                b.outcomes.entry(d.action.index).or_insert_with(|| {
                    (
                        not_executed(vec![Blocker::new(
                            BlockerCode::ChangedSincePlanning,
                            "the import was refused; nothing is deleted",
                        )]),
                        vec![],
                    )
                });
            }
            return Ok(b);
        }

        // 3. Import, one media at a time (INV-11), each behind its own durable intent.
        let mut imported: Vec<(&Replace<'_>, ImportRun)> = Vec::new();
        let mut halted: Option<Blocker> = None;
        for (n, r) in replaces.iter().enumerate() {
            if self.cancel.is_cancelled() && halted.is_none() {
                b.interrupted = true;
                self.reporter
                    .emit(super::events::ActionEvent::BatchInterrupted {
                        plan: self.plan.id,
                        reason: "cancelled between imports".into(),
                    });
                halted = Some(Blocker::new(
                    BlockerCode::Interrupted,
                    "cancelled between imports",
                ));
            }
            if let Some(h) = &halted {
                b.outcomes
                    .insert(r.action.index, (not_executed(vec![h.clone()]), vec![]));
                continue;
            }
            self.reporter
                .stage("import", n as u32, Some(r.media.media_id.to_string()));
            let run = self.import_one(r).await?;
            if let ImportRun::Refused(bl) = &run {
                b.outcomes
                    .insert(r.action.index, (not_executed(bl.clone()), vec![]));
                halted = Some(Blocker::new(
                    BlockerCode::Interrupted,
                    "not executed: an earlier import of the plan was refused",
                ));
                continue;
            }
            if let ImportRun::Failed(e) = &run {
                b.outcomes.insert(
                    r.action.index,
                    (
                        ActionOutcome::Failed {
                            class: e.class.clone(),
                        },
                        vec![],
                    ),
                );
                continue;
            }
            // T4-05: the completed sources must still be there, untouched.
            let moved = !self.simulated && self.sources_moved(r).await;
            if moved {
                b.tx.modified
                    .extend(r.action.torrent_hashes.iter().cloned());
                halted = Some(Blocker::new(
                    BlockerCode::ArrImportMoved,
                    "ARR moved or changed a completed source; every remaining step is cancelled",
                ));
            }
            imported.push((r, run));
        }

        // 4. Recheck what was modified (only a move), restore what is proven intact.
        self.reporter.stage("recheck", 0, None);
        self.recheck_modified(lead, &mut b.tx).await?;
        self.restore(lead, &mut b.tx).await?;

        // 5. Verify by re-reading ARR and the disk.
        self.reporter.stage("verify", 0, None);
        let mut verified: BTreeSet<u32> = BTreeSet::new();
        for (r, run) in &imported {
            let (outcome, results, added) = self.verdict(r, run, &b).await;
            if matches!(outcome, ActionOutcome::Verified | ActionOutcome::Simulated) {
                verified.insert(r.action.index);
            }
            for sig in added {
                b.added_costs.push((r.action.index, sig));
            }
            b.outcomes.insert(r.action.index, (outcome, results));
        }

        // 6. Old media files, only behind a verified import of this run.
        for d in &deletes {
            if let Some(h) = &halted
                && !d.action.depends_on.iter().all(|x| verified.contains(x))
            {
                b.outcomes
                    .insert(d.action.index, (not_executed(vec![h.clone()]), vec![]));
                continue;
            }
            if self.cancel.is_cancelled() {
                b.interrupted = true;
                b.outcomes.insert(
                    d.action.index,
                    (aborted("cancelled between deletions"), vec![]),
                );
                continue;
            }
            let (outcome, results) = self.delete_old(d, &replaces, &verified, &b).await?;
            b.outcomes.insert(d.action.index, (outcome, results));
        }
        Ok(b)
    }

    /// T4-03/T4-10: ARR's preview, read right before the command, must still accept every
    /// candidate with exactly the planned episodes.
    async fn preview_blockers(&self, r: &Replace<'_>) -> Vec<Blocker> {
        let Some(reader) = self.ports.arr.iter().find(|x| x.service() == r.service) else {
            return vec![Blocker::new(
                BlockerCode::ServiceNotConfigured(match r.service {
                    ArrService::Sonarr => removarr_domain::session::ServiceKind::Sonarr,
                    ArrService::Radarr => removarr_domain::session::ServiceKind::Radarr,
                }),
                "no ARR read adapter",
            )];
        };
        let mut out = Vec::new();
        for (path, planned) in &r.request.files {
            let Some(parent) = path.rel.parent() else {
                continue;
            };
            let folder = RootedPath::new(path.root.clone(), parent);
            let decisions = match reader.manual_import_preview(&folder, Some(planned)).await {
                Ok(d) => d,
                Err(e) => {
                    out.push(Blocker::new(
                        BlockerCode::ArrMissing,
                        format!("ARR preview of {path} failed: {}", e.detail),
                    ));
                    continue;
                }
            };
            let subject = path.to_string();
            match decisions.iter().find(|d| d.subject == subject) {
                None => out.push(Blocker::new(
                    BlockerCode::ArrMissing,
                    format!("ARR's preview no longer lists {path}"),
                )),
                Some(d) if !d.approved || !d.rejections.is_empty() => out.push(Blocker::new(
                    BlockerCode::ArrRejected,
                    format!("ARR now rejects {path}: {}", d.rejections.join("; ")),
                )),
                Some(d) => {
                    let mut got = d.episode_ids.clone();
                    got.sort_unstable();
                    let mut want = planned.episode_ids.clone();
                    want.sort_unstable();
                    if got != want {
                        out.push(Blocker::new(
                            BlockerCode::NumberingMismatch,
                            format!("ARR's episodes for {path} differ from the plan"),
                        ));
                    }
                }
            }
        }
        out
    }

    /// `NoArrImport` immediately before the command: nothing importing the media or the hashes.
    async fn import_busy(&self, r: &Replace<'_>) -> Option<Blocker> {
        let reader = self.ports.arr.iter().find(|x| x.service() == r.service)?;
        match reader.queue().await {
            Err(e) => Some(Blocker::new(
                BlockerCode::ImportInProgress,
                format!("the ARR queue could not be read: {}", e.detail),
            )),
            Ok(queue) => queue
                .iter()
                .any(|item| {
                    import_in_progress(item)
                        && (item.media.as_ref().is_some_and(|m| {
                            m.service == r.service && m.media_id == r.media.media_id
                        }) || item
                            .download_id
                            .as_ref()
                            .is_some_and(|h| r.action.torrent_hashes.contains(h)))
                })
                .then(|| {
                    Blocker::new(
                        BlockerCode::ImportInProgress,
                        "ARR is importing this media; one importer at a time",
                    )
                }),
        }
    }

    async fn fingerprint_now(&self, r: &Replace<'_>) -> Result<String, PortError> {
        let reader = self
            .ports
            .arr
            .iter()
            .find(|x| x.service() == r.service)
            .ok_or_else(|| PortError::new(ErrorClass::Config, "no ARR read adapter"))?;
        reader
            .quality_snapshot(&r.media)
            .await
            .map(|s| s.fingerprint)
    }

    async fn import_one(&self, r: &Replace<'_>) -> Result<ImportRun, PortError> {
        if let Some(bl) = self.import_busy(r).await {
            return Ok(ImportRun::Refused(vec![bl]));
        }
        let before = match self.fingerprint_now(r).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(ImportRun::Refused(vec![Blocker::new(
                    BlockerCode::ArrMissing,
                    format!("ARR settings could not be read: {}", e.detail),
                )]));
            }
        };
        let target = ImportTarget {
            service: r.service,
            request: r.request.clone(),
            sources: r
                .sources
                .iter()
                .map(|(p, s)| (p.clone(), s.phys()))
                .collect(),
            fingerprint_before: before.clone(),
        };
        let cmd = self
            .mutators
            .arr
            .iter()
            .find(|(s, _)| *s == r.service)
            .map(|(_, c)| c.clone());
        let reader = self
            .ports
            .arr
            .iter()
            .find(|x| x.service() == r.service)
            .cloned();
        let req = r.request.clone();
        let media = r.media.clone();
        let timeout = self.settings.recheck_timeout;
        let interval = self.settings.recheck_interval;
        let run = self
            .step(
                r.action.index,
                JournalStep::ArrManualImport,
                json(&target),
                json(&r.old),
                r.action.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(ErrorClass::Config, "no live ARR command adapter")
                    })?;
                    match cmd.manual_import(&req).await {
                        Ok(id) => {
                            let state =
                                poll_command(reader.as_deref(), id, timeout, interval).await;
                            Ok(ImportDone {
                                command_id: Some(id),
                                state,
                                error: None,
                            })
                        }
                        Err(e) => {
                            // CP2T4-F02/T4-20: judge by what ARR shows. A changed fingerprint
                            // (a new current file) means the command ran despite the error.
                            let after = match &reader {
                                Some(rd) => {
                                    rd.quality_snapshot(&media).await.map(|s| s.fingerprint)
                                }
                                None => Err(e.clone()),
                            };
                            match after {
                                Ok(fp) if fp != before => Ok(ImportDone {
                                    command_id: None,
                                    state: "observed_by_fingerprint".into(),
                                    error: Some(e.detail),
                                }),
                                _ => Err(e),
                            }
                        }
                    }
                },
            )
            .await?;
        Ok(match run {
            StepRun::Done(d) => ImportRun::Done(d),
            StepRun::Simulated => ImportRun::Simulated,
            StepRun::Failed(e) => ImportRun::Failed(e),
        })
    }

    /// True when any completed source is gone or carries another inode.
    async fn sources_moved(&self, r: &Replace<'_>) -> bool {
        for (p, sig) in &r.sources {
            match self.stat_path(p).await {
                Ok(Some(s)) if s.phys() == sig.phys() => {}
                _ => return true,
            }
        }
        false
    }

    /// ARR's current file per episode (Sonarr) or for the movie (Radarr), fresh.
    async fn arr_files(&self, r: &Replace<'_>) -> Result<BTreeMap<i64, RootedPath>, PortError> {
        let reader = self
            .ports
            .arr
            .iter()
            .find(|x| x.service() == r.service)
            .ok_or_else(|| PortError::new(ErrorClass::Config, "no ARR read adapter"))?;
        let snapshot = reader.quality_snapshot(&r.media).await?;
        let mapped = |file_id: i64| {
            snapshot
                .current_files
                .iter()
                .find(|c| c.file_id == file_id)
                .and_then(|c| match &c.path {
                    MappedPath::Mapped { path, .. } => Some(path.clone()),
                    _ => None,
                })
        };
        let mut out = BTreeMap::new();
        match r.service {
            ArrService::Radarr => {
                if let Some(p) = snapshot
                    .current_files
                    .first()
                    .and_then(|c| mapped(c.file_id))
                {
                    out.insert(r.media.media_id, p);
                }
            }
            ArrService::Sonarr => {
                for ep in reader.episodes(r.media.media_id, None).await? {
                    if let Some(p) = ep.file_id.and_then(mapped) {
                        out.insert(ep.id, p);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The outcome of one replacement from fresh reads (T4-06, T4-07, T4-08).
    async fn verdict(
        &self,
        r: &Replace<'_>,
        run: &ImportRun,
        b: &Batch,
    ) -> (ActionOutcome, Vec<VerificationResult>, Vec<EntrySignature>) {
        if self.simulated {
            let results = self.verify_action(r.action, &b.tx, None).await;
            return (ActionOutcome::Simulated, results, vec![]);
        }
        let observation = match run {
            ImportRun::Done(d) => match d.state.as_str() {
                "completed" | "observed_by_fingerprint" => ServiceObservation::CommandCompleted,
                "failed" => ServiceObservation::CommandFailed {
                    reason: d
                        .error
                        .clone()
                        .unwrap_or_else(|| "the import command failed".into()),
                },
                other => ServiceObservation::Unobserved {
                    reason: format!("the import command was {other} at the timeout"),
                },
            },
            _ => ServiceObservation::Unobserved {
                reason: "no import command ran".into(),
            },
        };
        let o = self.observe_import(r, &observation).await;
        let outcome = if o.moved {
            ActionOutcome::FailedNeedsReview
        } else {
            let base = outcome_from_verification(&o.results);
            if base == ActionOutcome::Verified {
                base
            } else if o.new_ok && o.source_ok && !o.shared {
                ActionOutcome::ExecutedUnverified {
                    reason: "ARR copied the file instead of hardlinking it (AddedCost); the old \
                             media file is never deleted by this run"
                        .into(),
                }
            } else {
                base
            }
        };
        let added = if o.new_ok && o.source_ok && !o.shared {
            o.added
        } else {
            vec![]
        };
        (outcome, o.results, added)
    }

    async fn observe_import(&self, r: &Replace<'_>, obs: &ServiceObservation) -> Observed {
        let now = || self.ports.clock.now();
        let mk = |step: VerificationStep, status: VerificationStatus| VerificationResult {
            step,
            status,
            at: now(),
        };
        let failed = |reason: String| VerificationStatus::Failed { reason };
        let files = self.arr_files(r).await;
        let old_phys: BTreeSet<PhysicalId> = r.old.iter().map(|(_, p, _)| *p).collect();
        let mut results = Vec::new();
        let mut shared = true;
        let mut new_ok = true;
        let mut source_ok = true;
        let mut moved = false;
        let mut added = Vec::new();
        for ((src, planned), (_, sig)) in r.request.files.iter().zip(r.sources.iter()) {
            // (a) ARR's current file for this source's episodes is a new library file.
            let keys: Vec<i64> = match r.service {
                ArrService::Radarr => vec![r.media.media_id],
                ArrService::Sonarr => planned.episode_ids.clone(),
            };
            let (new_path, a_status) = match &files {
                Err(e) => (
                    None,
                    VerificationStatus::NotRun {
                        reason: format!("ARR could not be read: {}", e.detail),
                    },
                ),
                Ok(map) => {
                    let paths: BTreeSet<&RootedPath> =
                        keys.iter().filter_map(|k| map.get(k)).collect();
                    let all_mapped = keys.iter().all(|k| map.contains_key(k));
                    match (paths.iter().next(), paths.len(), all_mapped) {
                        (Some(p), 1, true) => {
                            let p = (*p).clone();
                            match self.stat_path(&p).await {
                                Ok(Some(s))
                                    if s.kind == EntryKind::File
                                        && p.root.kind() == RootKind::Media
                                        && !old_phys.contains(&s.phys()) =>
                                {
                                    added.push(s);
                                    (Some((p, s)), VerificationStatus::Passed)
                                }
                                Ok(Some(_)) => (
                                    None,
                                    failed("ARR's current file is not a new library file".into()),
                                ),
                                Ok(None) => (None, failed("ARR's current file is missing".into())),
                                Err(e) => (None, VerificationStatus::NotRun { reason: e.detail }),
                            }
                        }
                        _ => (
                            None,
                            failed("ARR does not map the episodes to one current file".into()),
                        ),
                    }
                }
            };
            if a_status != VerificationStatus::Passed {
                new_ok = false;
            }
            results.push(mk(
                VerificationStep::ArrCurrentFile {
                    media: planned.clone(),
                    path: src.clone(),
                },
                a_status,
            ));
            // (b) the source keeps its inode.
            let b_status = match self.stat_path(src).await {
                Ok(Some(s)) if s.phys() == sig.phys() => VerificationStatus::Passed,
                Ok(_) => {
                    moved = true;
                    failed(
                        "ArrImportMoved: the completed source is gone or carries another inode"
                            .into(),
                    )
                }
                Err(e) => VerificationStatus::NotRun {
                    reason: format!("{src}: {}", e.detail),
                },
            };
            if b_status != VerificationStatus::Passed {
                source_ok = false;
            }
            results.push(mk(
                VerificationStep::PathStillHasInode {
                    path: src.clone(),
                    phys: sig.phys(),
                },
                b_status,
            ));
            // (c) the library file shares the source inode.
            if let Some((np, ns)) = &new_path {
                let c_status = if ns.phys() == sig.phys() {
                    VerificationStatus::Passed
                } else {
                    shared = false;
                    failed(format!("{np} is a copy, not a hardlink of {src}"))
                };
                results.push(mk(
                    VerificationStep::InodeEquals {
                        path: np.clone(),
                        phys: sig.phys(),
                    },
                    c_status,
                ));
            } else {
                shared = false;
            }
        }
        // The command itself: a completion that was not observed is never a success.
        results.push(mk(
            VerificationStep::ArrCommandCompleted {
                service: r.service,
                media: Some(r.media.clone()),
            },
            removarr_domain::journal::evaluate_service_step(
                &VerificationStep::ArrCommandCompleted {
                    service: r.service,
                    media: Some(r.media.clone()),
                },
                obs,
            ),
        ));
        // Where every old file went (T4-07).
        for step in &r.action.verification {
            let VerificationStep::OldFileAccounted { path, recovery } = step else {
                continue;
            };
            let status = self.old_accounted(r, path, recovery).await;
            results.push(mk(step.clone(), status));
        }
        Observed {
            results,
            moved,
            shared,
            new_ok,
            source_ok,
            added,
        }
    }

    async fn old_accounted(
        &self,
        r: &Replace<'_>,
        path: &RootedPath,
        recovery: &OldFileRecovery,
    ) -> VerificationStatus {
        let failed = |reason: String| VerificationStatus::Failed { reason };
        let Some((_, phys, _)) = r.old.iter().find(|(p, _, _)| p == path) else {
            return failed(format!("{path} is not in the planned old coverage"));
        };
        match self.stat_path(path).await {
            Ok(Some(s)) if s.phys() == *phys => VerificationStatus::Passed, // kept in place
            Ok(_) => match recovery {
                OldFileRecovery::OtherLink(other) => match self.stat_path(other).await {
                    Ok(Some(s)) if s.phys() == *phys => VerificationStatus::Passed,
                    Ok(_) => failed(format!("the other link {other} no longer keeps {path}")),
                    Err(e) => VerificationStatus::NotRun { reason: e.detail },
                },
                OldFileRecovery::RecycleBin => VerificationStatus::Passed,
                OldFileRecovery::Unrecoverable => {
                    if self
                        .approval
                        .is_some_and(|a| a.acknowledged_unrecoverable.contains(path))
                    {
                        VerificationStatus::Passed
                    } else {
                        failed(format!("{path} is lost and was never acknowledged"))
                    }
                }
            },
            Err(e) => VerificationStatus::NotRun {
                reason: format!("{path}: {}", e.detail),
            },
        }
    }

    /// One `DeleteMediaFile`: only behind a verified import of this run, never the new inode,
    /// never ARR's current file, `nlink` unchanged, through the dedicated mutator entry point.
    async fn delete_old(
        &self,
        d: &DeleteOld<'_>,
        replaces: &[Replace<'_>],
        verified: &BTreeSet<u32>,
        b: &Batch,
    ) -> Result<(ActionOutcome, Vec<VerificationResult>), PortError> {
        let refuse = |code: BlockerCode, detail: String| {
            Ok((not_executed(vec![Blocker::new(code, detail)]), vec![]))
        };
        if !d.action.depends_on.iter().all(|x| verified.contains(x)) {
            return refuse(
                BlockerCode::Interrupted,
                "not executed: its import is not verified in this run".into(),
            );
        }
        let sources: BTreeSet<PhysicalId> = replaces
            .iter()
            .filter(|r| d.action.depends_on.contains(&r.action.index))
            .flat_map(|r| r.sources.iter().map(|(_, s)| s.phys()))
            .collect();
        if !self.simulated {
            match self.stat_path(&d.path).await {
                Ok(None) => {
                    return refuse(
                        BlockerCode::ChangedSincePlanning,
                        format!(
                            "{} is already gone (ARR's upgrade removed it); nothing to delete",
                            d.path
                        ),
                    );
                }
                Ok(Some(s)) if !s.matches(&d.expect, true) => {
                    return refuse(
                        BlockerCode::ChangedSincePlanning,
                        format!("{} changed since the plan was built", d.path),
                    );
                }
                Ok(Some(s)) if sources.contains(&s.phys()) => {
                    return refuse(
                        BlockerCode::ProtectedZone,
                        format!("{} carries the new inode and is never deleted", d.path),
                    );
                }
                Ok(Some(_)) => {}
                Err(e) => {
                    return refuse(
                        BlockerCode::ScanErrorUnder,
                        format!("{}: {}", d.path, e.detail),
                    );
                }
            }
            // ARR's current file of the media is never deleted.
            for r in replaces
                .iter()
                .filter(|r| d.action.depends_on.contains(&r.action.index))
            {
                match self.arr_files(r).await {
                    Ok(m) if m.values().any(|p| p == &d.path) => {
                        return refuse(
                            BlockerCode::ProtectedZone,
                            format!("{} is ARR's current file", d.path),
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        return refuse(
                            BlockerCode::ArrMissing,
                            format!("ARR's current files could not be read: {}", e.detail),
                        );
                    }
                }
            }
        }
        let fs = self.mutators.fs.clone();
        let (p, s) = (d.path.clone(), d.expect);
        let intent = self
            .intent(
                d.action.index,
                JournalStep::FsRemoveFile,
                json(&EntryTarget {
                    path: d.path.clone(),
                    expect: d.expect,
                    mode: None,
                }),
                serde_json::Value::Null,
                d.action.torrent_hashes.clone(),
            )
            .await?;
        if self.simulated {
            self.result(
                intent,
                d.action.index,
                JournalStep::FsRemoveFile,
                StepOutcome::Simulated,
            )
            .await?;
            let results = self.verify_action(d.action, &b.tx, None).await;
            return Ok((ActionOutcome::Simulated, results));
        }
        let call = self.blocking(move || fs.remove_media_file(&p, &s)).await;
        let (record, error) = match call {
            Ok(()) => (
                StepOutcome::Done {
                    final_state: serde_json::json!({"removed": true}),
                },
                None,
            ),
            Err(e) => match self.stat_path(&d.path).await {
                Ok(None) => (
                    StepOutcome::Done {
                        final_state: serde_json::json!({"removed": true, "error": e.detail}),
                    },
                    Some(e),
                ),
                _ => (
                    StepOutcome::Failed {
                        class: e.class.clone(),
                        message: format!("{} (target not observed as removed)", e.detail),
                    },
                    Some(e),
                ),
            },
        };
        let mutated = matches!(record, StepOutcome::Done { .. });
        self.result(intent, d.action.index, JournalStep::FsRemoveFile, record)
            .await?;
        Ok(match (mutated, error) {
            (false, Some(e)) => (ActionOutcome::Failed { class: e.class }, vec![]),
            (true, Some(e)) => (
                ActionOutcome::ExecutedUnverified {
                    reason: format!("step failed: {}", e.detail),
                },
                vec![],
            ),
            (_, None) => {
                let results = self.verify_action(d.action, &b.tx, None).await;
                (outcome_from_verification(&results), results)
            }
        })
    }
}

/// What one import did.
pub(crate) enum ImportRun {
    Done(ImportDone),
    Simulated,
    Failed(PortError),
    Refused(Vec<Blocker>),
}

/// Polls `command_status` until the command is finished or `timeout` elapses.
async fn poll_command(
    reader: Option<&dyn removarr_domain::ports::ArrRead>,
    id: i64,
    timeout: std::time::Duration,
    interval: std::time::Duration,
) -> String {
    let Some(reader) = reader else {
        return "unobserved".into();
    };
    let started = tokio::time::Instant::now();
    loop {
        match reader.command_status(id).await {
            Ok(CommandState::Completed) => return "completed".into(),
            Ok(CommandState::Failed { .. }) => return "failed".into(),
            Ok(CommandState::Queued | CommandState::Started) => {}
            Ok(CommandState::Unknown(_)) | Err(_) => return "unobserved".into(),
        }
        if started.elapsed() >= timeout {
            return "unobserved".into();
        }
        tokio::time::sleep(interval).await;
    }
}
