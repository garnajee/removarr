//! Tier-4 ARR commands (T4-12, T4-16): `ArrGrabRelease` (manual, through the opaque release
//! token of a fresh search), `ArrRemoveQueueItem` (keep media and client data, no blocklist,
//! only a proven-stale item) and `ArrSetMonitored` (targeted, the previous value journaled).
//! Every command is behind a durable intent and judged by what ARR shows afterwards: a call
//! that errors but whose effect is visible counts as done, and nothing is ever sent twice.

use std::collections::BTreeSet;

use removarr_domain::arr::correlate::import_in_progress;
use removarr_domain::arr::{ArrMediaRef, ArrQueueItem, ArrService, ReleaseRef};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::journal::{
    ActionOutcome, JournalStep, ServiceObservation, VerificationResult, VerificationStatus,
    outcome_from_verification,
};
use removarr_domain::plan::{ActionTarget, PlannedAction, VerificationStep};
use removarr_domain::ports::ArrRead;
use removarr_domain::session::ServiceKind;
use serde::{Deserialize, Serialize};

use super::batch::{Batch, aborted, not_executed};
use super::journal::{Ctx, StepRun, command_may_have_run, json};
use super::revalidate::LiveReads;

/// `target` payload of an `ArrGrab` intent; `observed` is [`GrabObserved`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GrabTarget {
    pub media: ArrMediaRef,
    pub release: ReleaseRef,
}

/// Queue ids before the grab: a new item for the media afterwards is the evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GrabObserved {
    pub queue_before: Vec<i64>,
}

/// `target` payload of an `ArrRemoveQueueItem` intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueTarget {
    pub service: ArrService,
    pub id: i64,
}

/// `target` payload of an `ArrSetMonitored` intent; `observed` is [`MonitorObserved`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MonitorTarget {
    pub media: ArrMediaRef,
    pub monitored: bool,
}

/// The value before the change, journaled so the change can be undone by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MonitorObserved {
    pub previous: bool,
}

fn service_kind(s: ArrService) -> ServiceKind {
    match s {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

/// ARR's monitored flag for `media`, read fresh. Sonarr with episodes: every named episode;
/// without: any library entry of the series. Radarr: the movie's library entry.
pub(crate) async fn read_monitored(
    reader: &dyn ArrRead,
    media: &ArrMediaRef,
) -> Result<bool, PortError> {
    if media.service == ArrService::Sonarr && !media.episode_ids.is_empty() {
        let eps = reader.episodes(media.media_id, None).await?;
        let mut all = true;
        for id in &media.episode_ids {
            let ep = eps
                .iter()
                .find(|e| e.id == *id)
                .ok_or_else(|| PortError::new(ErrorClass::NotFound, format!("episode {id}")))?;
            all &= ep.monitored;
        }
        return Ok(all);
    }
    let lib = reader.library().await?;
    let mine: Vec<_> = lib
        .iter()
        .filter(|e| e.media.service == media.service && e.media.media_id == media.media_id)
        .collect();
    if mine.is_empty() {
        return Err(PortError::new(
            ErrorClass::NotFound,
            "the media has no library entry to read the monitored flag from",
        ));
    }
    Ok(mine.iter().any(|e| e.monitored))
}

impl Ctx<'_> {
    /// `Some(reason)` when INV-39 cannot be proved for `item` right now.
    async fn queue_item_proof(&self, item: &ArrQueueItem, service: ArrService) -> Option<String> {
        use removarr_domain::fs::EntryKind;
        use removarr_domain::path::{RootKind, RootedPath};
        let (Some(media), Some(hash)) = (&item.media, &item.download_id) else {
            return Some("the item names no media or download: no proof the import is done".into());
        };
        let Some(qbit) = &self.ports.qbit else {
            return Some("qBittorrent is not configured: the import cannot be proved".into());
        };
        let files = match qbit.files(hash).await {
            Ok(f) => f,
            Err(e) => return Some(format!("the torrent's files cannot be read: {}", e.detail)),
        };
        // Per wanted video file: its candidates inside a completed root (DR-23 order).
        let per_file: Vec<Vec<RootedPath>> = files
            .iter()
            .filter(|f| f.is_wanted())
            // Like the issue engine: only videos are import sources (sidecars never are).
            .filter(|f| f.name.rsplit('/').next().is_some_and(super::auto::is_video))
            .map(|f| {
                f.candidates
                    .iter()
                    .filter_map(|c| c.as_rooted().cloned())
                    .filter(|p| p.root.kind() == RootKind::Completed)
                    .collect()
            })
            .collect();
        if per_file.is_empty() {
            return Some("no wanted completed file proves the import".into());
        }
        let current = match self.arr_current_files(service, media).await {
            Ok(m) => m,
            Err(e) => return Some(format!("ARR's current files cannot be read: {}", e.detail)),
        };
        let mut phys = BTreeSet::new();
        for (ep, p) in &current {
            if !media.episode_ids.is_empty() && !media.episode_ids.contains(ep) {
                continue;
            }
            match self.stat_path(p).await {
                Ok(Some(s)) if s.kind == EntryKind::File => {
                    phys.insert(s.phys());
                }
                _ => return Some(format!("ARR's current file {p} is not a readable file")),
            }
        }
        if phys.is_empty() {
            return Some("ARR has no current file for the media".into());
        }
        let mut live = LiveReads::default();
        for cands in &per_file {
            // The resolved path is the candidate that exists as a complete file; a `.!qB`
            // partial never counts, and several existing candidates must agree.
            let mut resolved: Option<(&RootedPath, _)> = None;
            let mut partial = None;
            for c in cands {
                let st = match self.stat_path(c).await {
                    Ok(st) => st,
                    Err(e) => return Some(format!("{c} cannot be read: {}", e.detail)),
                };
                let Some(st) = st else { continue };
                if c.rel.to_string().ends_with(".!qB") {
                    partial = Some(c);
                    continue;
                }
                if st.kind != EntryKind::File {
                    return Some(format!("{c} is not a regular file"));
                }
                match &resolved {
                    Some((_, phys0)) if *phys0 != st.phys() => {
                        return Some(format!("{c} disagrees with another on-disk candidate"));
                    }
                    Some(_) => {}
                    None => resolved = Some((c, st.phys())),
                }
            }
            if let Some(p) = partial {
                return Some(format!(
                    "{p} is a partial download: the file is not complete"
                ));
            }
            let Some((c, phys_c)) = resolved else {
                return Some("a wanted file has no complete copy on disk".into());
            };
            if !phys.contains(&phys_c) {
                return Some(format!(
                    "{c} does not share an inode with ARR's current file"
                ));
            }
            match self.inventory(&mut live, &c.root).await {
                Ok(Some(inv)) if inv.finished && !inv.has_issue_under(&c.rel) => {}
                _ => return Some(format!("a scan or permission finding lies at {c}")),
            }
        }
        None
    }
}

fn refuse(
    code: BlockerCode,
    detail: impl Into<String>,
) -> (ActionOutcome, Vec<VerificationResult>) {
    (not_executed(vec![Blocker::new(code, detail)]), vec![])
}

impl Ctx<'_> {
    pub(crate) async fn arr_command_batch(
        &self,
        actions: &[&PlannedAction],
    ) -> Result<Batch, PortError> {
        let mut b = Batch::default();
        let mut live = LiveReads::default();
        if !self
            .revalidate_pass(&mut live, actions, 1, &mut b, &Default::default())
            .await?
        {
            return Ok(b);
        }
        for (n, a) in actions.iter().enumerate() {
            if self.cancel.is_cancelled() {
                b.interrupted = true;
                b.outcomes
                    .insert(a.index, (aborted("cancelled between commands"), vec![]));
                continue;
            }
            self.reporter.stage("arr_command", n as u32, None);
            let r = match &a.target {
                ActionTarget::Release { media, release } => self.grab(a, media, release).await?,
                ActionTarget::Queue { service, id } => self.remove_queue(a, *service, *id).await?,
                ActionTarget::Monitor { media, monitored } => {
                    self.set_monitored(a, media, *monitored).await?
                }
                _ => refuse(BlockerCode::NotInBuild, "not an ARR command"),
            };
            b.outcomes.insert(a.index, r);
        }
        Ok(b)
    }

    fn arr_reader(&self, s: ArrService) -> Option<std::sync::Arc<dyn ArrRead>> {
        self.ports.arr.iter().find(|r| r.service() == s).cloned()
    }

    fn arr_commander(
        &self,
        s: ArrService,
    ) -> Option<std::sync::Arc<dyn removarr_domain::ports::ArrCommands>> {
        self.mutators
            .arr
            .iter()
            .find(|(x, _)| *x == s)
            .map(|(_, c)| c.clone())
    }

    fn not_configured(&self, s: ArrService) -> (ActionOutcome, Vec<VerificationResult>) {
        refuse(
            BlockerCode::ServiceNotConfigured(service_kind(s)),
            format!("{} is not configured", s.as_str()),
        )
    }

    async fn grab(
        &self,
        a: &PlannedAction,
        media: &ArrMediaRef,
        release: &ReleaseRef,
    ) -> Result<(ActionOutcome, Vec<VerificationResult>), PortError> {
        let Some(reader) = self.arr_reader(media.service) else {
            return Ok(self.not_configured(media.service));
        };
        // A fresh search: the token must still be a release ARR just offered (T4-12).
        match reader.search_releases(media).await {
            Ok(found) if found.iter().any(|r| &r.release == release) => {}
            Ok(_) => {
                return Ok(refuse(
                    BlockerCode::ReleaseNotInSearch,
                    "the release is not in a fresh search result",
                ));
            }
            Err(e) => {
                return Ok(refuse(
                    BlockerCode::ServiceUnavailable(service_kind(media.service)),
                    format!("the search failed: {}", e.detail),
                ));
            }
        }
        let before: Vec<i64> = match reader.queue().await {
            Ok(q) => q.iter().map(|i| i.id).collect(),
            Err(e) => {
                return Ok(refuse(
                    BlockerCode::ServiceUnavailable(service_kind(media.service)),
                    format!("the queue could not be read: {}", e.detail),
                ));
            }
        };
        let cmd = self.arr_commander(media.service);
        let (m, rel, rd, qb) = (
            media.clone(),
            release.clone(),
            reader.clone(),
            before.clone(),
        );
        let run = self
            .step(
                a.index,
                JournalStep::ArrGrab,
                json(&GrabTarget {
                    media: media.clone(),
                    release: release.clone(),
                }),
                json(&GrabObserved {
                    queue_before: before.clone(),
                }),
                a.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(ErrorClass::Config, "no live ARR command adapter")
                    })?;
                    match cmd.grab(&rel).await {
                        Ok(()) => Ok(()),
                        // The adapter refused an unknown token: nothing was sent.
                        Err(e) if e.class == ErrorClass::StateChanged => Err(e),
                        Err(e) => match new_queue_item(&*rd, &m, &qb).await {
                            Some(_) => Ok(()),
                            // CP2T5-F03: possibly accepted; the queue is polled afterwards and
                            // an unobserved grab stays `ExecutedUnverified`, never `Failed`.
                            None if command_may_have_run(&e.class) => Ok(()),
                            None => Err(e),
                        },
                    }
                },
            )
            .await?;
        match run {
            StepRun::Simulated => Ok((ActionOutcome::Simulated, vec![])),
            StepRun::Failed(e) if e.class == ErrorClass::StateChanged => Ok(refuse(
                BlockerCode::ReleaseNotInSearch,
                "the adapter has no such release in its recent search",
            )),
            StepRun::Failed(e) => Ok((ActionOutcome::Failed { class: e.class }, vec![])),
            StepRun::Done(()) => {
                // Verified only by a new queue item for the media, within the timeout.
                let started = tokio::time::Instant::now();
                let status = loop {
                    if new_queue_item(&*reader, media, &before).await.is_some() {
                        break VerificationStatus::Passed;
                    }
                    if started.elapsed() >= self.settings.recheck_timeout {
                        break VerificationStatus::NotRun {
                            reason: "no new queue item appeared before the timeout".into(),
                        };
                    }
                    tokio::time::sleep(self.settings.recheck_interval).await;
                };
                let results: Vec<VerificationResult> = a
                    .verification
                    .iter()
                    .map(|s| VerificationResult {
                        step: s.clone(),
                        status: if matches!(s, VerificationStep::ArrQueueHasRelease { .. }) {
                            status.clone()
                        } else {
                            VerificationStatus::NotRun {
                                reason: "not a grab verification".into(),
                            }
                        },
                        at: self.ports.clock.now(),
                    })
                    .collect();
                Ok((outcome_from_verification(&results), results))
            }
        }
    }

    async fn remove_queue(
        &self,
        a: &PlannedAction,
        service: ArrService,
        id: i64,
    ) -> Result<(ActionOutcome, Vec<VerificationResult>), PortError> {
        let Some(reader) = self.arr_reader(service) else {
            return Ok(self.not_configured(service));
        };
        let item: ArrQueueItem = match reader.queue().await {
            Ok(q) => match q.into_iter().find(|i| i.id == id) {
                Some(i) => i,
                None => {
                    return Ok(refuse(
                        BlockerCode::ChangedSincePlanning,
                        format!("queue item {id} is already gone"),
                    ));
                }
            },
            Err(e) => {
                return Ok(refuse(
                    BlockerCode::ServiceUnavailable(service_kind(service)),
                    format!("the queue could not be read: {}", e.detail),
                ));
            }
        };
        if import_in_progress(&item) {
            return Ok(refuse(
                BlockerCode::ImportInProgress,
                format!("queue item {id} may be an import in progress"),
            ));
        }
        // INV-39 re-proved from fresh reads (CP2T5-F04): the import is already correct. Every
        // wanted completed file of the item's torrent shares its inode with ARR's current file
        // of the media, and no scan or permission finding lies at it. An item without a
        // download id or media, or whose torrent cannot be read, has no proof and stays.
        if let Some(bl) = self.queue_item_proof(&item, service).await {
            return Ok(refuse(BlockerCode::QueueItemHidesError, bl));
        }
        let cmd = self.arr_commander(service);
        let rd = reader.clone();
        let run = self
            .step(
                a.index,
                JournalStep::ArrRemoveQueueItem,
                json(&QueueTarget { service, id }),
                json(&item),
                a.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(ErrorClass::Config, "no live ARR command adapter")
                    })?;
                    match cmd.remove_queue_item_keep_media(id).await {
                        Ok(()) => Ok(()),
                        Err(e) => match rd.queue().await {
                            Ok(q) if q.iter().all(|i| i.id != id) => Ok(()),
                            _ => Err(e),
                        },
                    }
                },
            )
            .await?;
        match run {
            StepRun::Simulated => Ok((ActionOutcome::Simulated, vec![])),
            StepRun::Failed(e) => Ok((ActionOutcome::Failed { class: e.class }, vec![])),
            StepRun::Done(()) => {
                let obs = match reader.queue().await {
                    Ok(q) => ServiceObservation::QueueItemPresent(q.iter().any(|i| i.id == id)),
                    Err(e) => ServiceObservation::Unobserved {
                        reason: format!("queue: {}", e.detail),
                    },
                };
                let results = self.verify_action(a, &Default::default(), Some(&obs)).await;
                Ok((outcome_from_verification(&results), results))
            }
        }
    }

    async fn set_monitored(
        &self,
        a: &PlannedAction,
        media: &ArrMediaRef,
        monitored: bool,
    ) -> Result<(ActionOutcome, Vec<VerificationResult>), PortError> {
        let Some(reader) = self.arr_reader(media.service) else {
            return Ok(self.not_configured(media.service));
        };
        let previous = match read_monitored(&*reader, media).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(refuse(
                    BlockerCode::ArrMissing,
                    format!("the current monitored flag could not be read: {}", e.detail),
                ));
            }
        };
        let cmd = self.arr_commander(media.service);
        let (m, rd) = (media.clone(), reader.clone());
        let run = self
            .step(
                a.index,
                JournalStep::ArrSetMonitored,
                json(&MonitorTarget {
                    media: media.clone(),
                    monitored,
                }),
                json(&MonitorObserved { previous }),
                a.torrent_hashes.clone(),
                || async move {
                    let cmd = cmd.ok_or_else(|| {
                        PortError::new(ErrorClass::Config, "no live ARR command adapter")
                    })?;
                    match cmd.set_monitored(&m, monitored).await {
                        Ok(()) => Ok(()),
                        Err(e) => match read_monitored(&*rd, &m).await {
                            Ok(now) if now == monitored && previous != monitored => Ok(()),
                            _ => Err(e),
                        },
                    }
                },
            )
            .await?;
        match run {
            StepRun::Simulated => Ok((ActionOutcome::Simulated, vec![])),
            StepRun::Failed(e) => Ok((ActionOutcome::Failed { class: e.class }, vec![])),
            StepRun::Done(()) => {
                let obs = match read_monitored(&*reader, media).await {
                    Ok(now) => ServiceObservation::Monitored(now),
                    Err(e) => ServiceObservation::Unobserved {
                        reason: format!("monitored flag: {}", e.detail),
                    },
                };
                let results = self.verify_action(a, &Default::default(), Some(&obs)).await;
                Ok((outcome_from_verification(&results), results))
            }
        }
    }
}

/// A queue item for `media` whose id was not in `before`.
async fn new_queue_item(reader: &dyn ArrRead, media: &ArrMediaRef, before: &[i64]) -> Option<i64> {
    let known: BTreeSet<i64> = before.iter().copied().collect();
    let q = reader.queue().await.ok()?;
    q.into_iter()
        .find(|i| {
            !known.contains(&i.id)
                && i.media
                    .as_ref()
                    .is_some_and(|m| m.service == media.service && m.media_id == media.media_id)
        })
        .map(|i| i.id)
}
