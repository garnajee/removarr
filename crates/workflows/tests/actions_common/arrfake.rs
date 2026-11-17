//! A scriptable ARR (Sonarr) for the tier-4 runner tests: it reads and commands through the
//! domain ports and acts on the real temporary directories, so an import really hardlinks,
//! copies or moves the completed source and the upgrade really removes the old media file.
//! It models the variants of the testkit's HTTP fake (V10 hardlink/copy/move import effect,
//! recycle bin and hardlink settings) at the port level; the HTTP adapter is covered by the
//! connector's own tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrFileRef, ArrHistoryRecord, ArrLibraryEntry,
    ArrMediaManagement, ArrMediaRef, ArrQualitySnapshot, ArrQueueItem, ArrService, CommandState,
    DecisionSource, ManualImportRequest, MediaIdentity, MediaKind, ReleaseRef, ReleaseSearchResult,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::ports::{ArrCommands, ArrRead};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{MappedPath, TorrentHash};

use super::CountingArr;

/// What a manual import does to the completed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportEffect {
    /// ARR hardlinks (`copyUsingHardlinks`): the library file shares the source inode.
    Hardlink,
    /// ARR copies the bytes (hardlinks disabled): a new inode.
    Copy,
    /// ARR moves the file despite the copy request: the source disappears.
    Move,
    /// The command is accepted and nothing happens.
    Nothing,
}

#[derive(Debug)]
pub struct State {
    pub completed: PathBuf,
    pub media: PathBuf,
    pub media_root: RootId,
    pub series_id: i64,
    pub episodes: Vec<ArrEpisode>,
    pub files: Vec<ArrFileRef>,
    pub queue: Vec<ArrQueueItem>,
    pub previews: BTreeMap<RootedPath, Vec<ArrCandidateDecision>>,
    pub effect: ImportEffect,
    /// Per source file name, overriding `effect`.
    pub effect_by_name: BTreeMap<String, ImportEffect>,
    pub hardlinks_enabled: bool,
    pub recycle_bin: bool,
    /// The upgrade import removes the old media files (Sonarr's behaviour).
    pub upgrade_deletes_old: bool,
    /// Where the imported file goes, relative to the media root.
    pub dest_dir: String,
    pub next_command: i64,
    pub command_state: CommandState,
    pub import_error: Option<ErrorClass>,
    /// The import runs, then the call reports a transport error.
    pub import_then_fail: bool,
    pub search: Vec<ReleaseSearchResult>,
    pub grab_then_fail: bool,
    /// The grab call errors before anything was queued.
    pub grab_error: Option<ErrorClass>,
    pub remove_then_fail: bool,
    pub monitor_then_fail: bool,
    pub fail_reads: bool,
    pub log: Vec<String>,
}

pub struct FakeArr {
    pub state: Mutex<State>,
    pub counter: Arc<CountingArr>,
}

impl std::fmt::Debug for FakeArr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeArr")
    }
}

pub fn media_ref(media_id: i64, episodes: &[i64]) -> ArrMediaRef {
    ArrMediaRef {
        service: ArrService::Sonarr,
        media_id,
        episode_ids: episodes.to_vec(),
    }
}

pub fn episode(id: i64, file_id: Option<i64>, monitored: bool) -> ArrEpisode {
    ArrEpisode {
        id,
        series_id: 5,
        season: 1,
        episode: i32::try_from(id % 100).unwrap_or(1),
        absolute: None,
        scene_absolute: None,
        scene_season: None,
        scene_episode: None,
        has_file: file_id.is_some(),
        file_id,
        monitored,
        aired: true,
    }
}

pub fn queue_item(
    id: i64,
    hash: Option<TorrentHash>,
    tracked: &str,
    media: Option<ArrMediaRef>,
) -> ArrQueueItem {
    ArrQueueItem {
        id,
        download_id: hash,
        media,
        status: "warning".into(),
        tracked_state: tracked.into(),
        messages: vec![],
    }
}

impl FakeArr {
    pub fn new(
        completed: PathBuf,
        media: PathBuf,
        media_root: RootId,
        counter: Arc<CountingArr>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                completed,
                media,
                media_root,
                series_id: 5,
                episodes: vec![],
                files: vec![],
                queue: vec![],
                previews: BTreeMap::new(),
                effect: ImportEffect::Hardlink,
                effect_by_name: BTreeMap::new(),
                hardlinks_enabled: true,
                recycle_bin: false,
                upgrade_deletes_old: true,
                dest_dir: "Movies/Rel".into(),
                next_command: 100,
                command_state: CommandState::Completed,
                import_error: None,
                import_then_fail: false,
                search: vec![],
                grab_then_fail: false,
                grab_error: None,
                remove_then_fail: false,
                monitor_then_fail: false,
                fail_reads: false,
                log: vec![],
            }),
            counter,
        })
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn calls(&self, what: &str) -> usize {
        self.log().iter().filter(|l| l.starts_with(what)).count()
    }

    fn hit(&self, s: &mut State, what: String) {
        self.counter.0.fetch_add(1, Ordering::SeqCst);
        s.log.push(what);
    }

    fn local(&self, s: &State, p: &RootedPath) -> PathBuf {
        if p.root == s.media_root {
            s.media.join(p.rel.as_str())
        } else {
            s.completed.join(p.rel.as_str())
        }
    }

    pub fn file_entry(&self, id: i64, rel: &str) -> ArrFileRef {
        let s = self.state.lock().unwrap();
        ArrFileRef {
            file_id: id,
            path: MappedPath::Mapped {
                raw: format!("/media/{rel}"),
                path: RootedPath::new(
                    s.media_root.clone(),
                    removarr_domain::path::RelPath::new(rel).unwrap(),
                ),
            },
            size: 1,
            quality: "WEB-1080p".into(),
            custom_formats: vec![],
            cf_score: 0,
            cutoff_not_met: true,
            languages: vec![],
        }
    }

    fn snapshot_of(&self, s: &State, media: &ArrMediaRef) -> ArrQualitySnapshot {
        ArrQualitySnapshot {
            service: ArrService::Sonarr,
            service_version: "4".into(),
            media: MediaIdentity {
                kind: MediaKind::Episode,
                arr: Some(ArrMediaRef {
                    service: media.service,
                    media_id: media.media_id,
                    episode_ids: vec![],
                }),
                external_ids: BTreeMap::new(),
                title: "T".into(),
                year: None,
                season: None,
                episode: None,
                absolute: None,
            },
            profile_id: 1,
            profile_name: "HD".into(),
            cutoff: "Bluray-1080p".into(),
            upgrade_allowed: true,
            min_format_score: 0,
            cutoff_format_score: 0,
            format_scores: BTreeMap::new(),
            current_files: s.files.clone(),
            candidates: vec![],
            media_management: Some(ArrMediaManagement {
                recycle_bin: s
                    .recycle_bin
                    .then(|| MappedPath::NoRule { raw: "/rb".into() }),
                recycle_bin_cleanup_days: Some(7),
                hardlinks_enabled: s.hardlinks_enabled,
                captured_at: Timestamp(1),
            }),
            captured_at: Timestamp(1),
            fingerprint: String::new(),
        }
        .seal()
        .unwrap()
    }

    /// The fingerprint a plan built now would pin.
    pub fn fingerprint(&self, media_id: i64) -> String {
        let s = self.state.lock().unwrap();
        self.snapshot_of(&s, &media_ref(media_id, &[])).fingerprint
    }
}

fn down() -> PortError {
    PortError::new(ErrorClass::Unavailable, "ARR is down")
}

#[async_trait]
impl ArrRead for FakeArr {
    fn service(&self) -> ArrService {
        ArrService::Sonarr
    }
    async fn status(&self) -> Result<String, PortError> {
        Ok("4.0".into())
    }
    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError> {
        Ok(vec![])
    }
    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError> {
        let s = self.state.lock().unwrap();
        if s.fail_reads {
            return Err(down());
        }
        Ok(s.queue.clone())
    }
    async fn history_for_download(
        &self,
        _: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError> {
        Ok(vec![])
    }
    async fn quality_snapshot(&self, media: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError> {
        let s = self.state.lock().unwrap();
        if s.fail_reads {
            return Err(down());
        }
        if s.episodes.is_empty() && s.files.is_empty() {
            return Err(PortError::new(ErrorClass::NotFound, "no media in the fake"));
        }
        Ok(self.snapshot_of(&s, media))
    }
    async fn manual_import_preview(
        &self,
        folder: &RootedPath,
        _: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError> {
        let s = self.state.lock().unwrap();
        if s.fail_reads {
            return Err(down());
        }
        Ok(s.previews.get(folder).cloned().unwrap_or_default())
    }
    async fn episodes(&self, _: i64, _: Option<i32>) -> Result<Vec<ArrEpisode>, PortError> {
        let s = self.state.lock().unwrap();
        if s.fail_reads {
            return Err(down());
        }
        Ok(s.episodes.clone())
    }
    async fn search_releases(
        &self,
        _: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError> {
        let s = self.state.lock().unwrap();
        if s.fail_reads {
            return Err(down());
        }
        Ok(s.search.clone())
    }
    async fn command_status(&self, _: i64) -> Result<CommandState, PortError> {
        Ok(self.state.lock().unwrap().command_state.clone())
    }
    async fn media_management(&self) -> Result<ArrMediaManagement, PortError> {
        let s = self.state.lock().unwrap();
        Ok(ArrMediaManagement {
            recycle_bin: None,
            recycle_bin_cleanup_days: None,
            hardlinks_enabled: s.hardlinks_enabled,
            captured_at: Timestamp(1),
        })
    }
}

#[async_trait]
impl ArrCommands for FakeArr {
    async fn rescan(&self, _: &ArrMediaRef) -> Result<i64, PortError> {
        let mut s = self.state.lock().unwrap();
        self.hit(&mut s, "rescan".into());
        Ok(1)
    }
    async fn rescan_all(&self) -> Result<i64, PortError> {
        let mut s = self.state.lock().unwrap();
        self.hit(&mut s, "rescan_all".into());
        Ok(1)
    }

    async fn manual_import(&self, req: &ManualImportRequest) -> Result<i64, PortError> {
        let mut s = self.state.lock().unwrap();
        self.hit(&mut s, "manual_import".into());
        if let Some(c) = s.import_error.clone() {
            return Err(PortError::new(c, "import refused"));
        }
        for (src, media) in &req.files {
            let from = self.local(&s, src);
            let name = src.rel.file_name().unwrap_or("x").to_string();
            let dest_rel = format!("{}/{}", s.dest_dir, name);
            let to = s.media.join(&dest_rel);
            if s.upgrade_deletes_old {
                // Only the files of the episodes this import covers are upgraded away.
                let ids: Vec<i64> = s
                    .episodes
                    .iter()
                    .filter(|e| media.episode_ids.contains(&e.id))
                    .filter_map(|e| e.file_id)
                    .collect();
                let old: Vec<PathBuf> = s
                    .files
                    .iter()
                    .filter(|f| ids.contains(&f.file_id))
                    .filter_map(|f| f.path.as_rooted().map(|p| s.media.join(p.rel.as_str())))
                    .collect();
                for o in old {
                    let _ = fs::remove_file(o);
                }
                s.files.retain(|f| !ids.contains(&f.file_id));
            }
            let effect = s.effect_by_name.get(&name).copied().unwrap_or(s.effect);
            match effect {
                ImportEffect::Hardlink => fs::hard_link(&from, &to).unwrap(),
                ImportEffect::Copy => {
                    fs::copy(&from, &to).unwrap();
                }
                ImportEffect::Move => fs::rename(&from, &to).unwrap(),
                ImportEffect::Nothing => continue,
            }
            let id = s
                .files
                .iter()
                .map(|f| f.file_id)
                .max()
                .unwrap_or(0)
                .max(999)
                + 1;
            let entry = {
                let rooted = RootedPath::new(
                    s.media_root.clone(),
                    removarr_domain::path::RelPath::new(&dest_rel).unwrap(),
                );
                ArrFileRef {
                    file_id: id,
                    path: MappedPath::Mapped {
                        raw: format!("/media/{dest_rel}"),
                        path: rooted,
                    },
                    size: 1,
                    quality: "Bluray-1080p".into(),
                    custom_formats: vec![],
                    cf_score: 10,
                    cutoff_not_met: false,
                    languages: vec![],
                }
            };
            s.files.push(entry);
            for ep in s.episodes.iter_mut() {
                if media.episode_ids.contains(&ep.id) {
                    ep.file_id = Some(id);
                    ep.has_file = true;
                }
            }
        }
        if s.import_then_fail {
            return Err(PortError::new(ErrorClass::Unavailable, "connection reset"));
        }
        s.next_command += 1;
        Ok(s.next_command)
    }

    async fn grab(&self, release: &ReleaseRef) -> Result<(), PortError> {
        let mut s = self.state.lock().unwrap();
        if !s.search.iter().any(|r| &r.release == release) {
            return Err(PortError::new(
                ErrorClass::StateChanged,
                "release is not in a recent search; search again before grabbing",
            ));
        }
        self.hit(&mut s, format!("grab {}", release.guid));
        if let Some(c) = s.grab_error.clone() {
            return Err(PortError::new(c, "grab failed"));
        }
        let id = 500 + s.queue.len() as i64;
        s.queue.push(queue_item(
            id,
            None,
            "downloading",
            Some(media_ref(5, &[11])),
        ));
        if s.grab_then_fail {
            return Err(PortError::new(ErrorClass::Unavailable, "connection reset"));
        }
        Ok(())
    }

    async fn remove_queue_item_keep_media(&self, id: i64) -> Result<(), PortError> {
        let mut s = self.state.lock().unwrap();
        self.hit(&mut s, format!("remove_queue {id}"));
        s.queue.retain(|i| i.id != id);
        if s.remove_then_fail {
            return Err(PortError::new(ErrorClass::Unavailable, "connection reset"));
        }
        Ok(())
    }

    async fn set_monitored(&self, media: &ArrMediaRef, monitored: bool) -> Result<(), PortError> {
        let mut s = self.state.lock().unwrap();
        self.hit(&mut s, format!("set_monitored {monitored}"));
        for ep in s.episodes.iter_mut() {
            if media.episode_ids.contains(&ep.id) {
                ep.monitored = monitored;
            }
        }
        if s.monitor_then_fail {
            return Err(PortError::new(ErrorClass::Unavailable, "connection reset"));
        }
        Ok(())
    }
}

pub fn decision(source: &RootedPath, episodes: &[i64]) -> ArrCandidateDecision {
    ArrCandidateDecision {
        subject: source.to_string(),
        size: 1,
        quality: "Bluray-1080p".into(),
        custom_formats: vec![],
        cf_score: 10,
        episode_ids: episodes.to_vec(),
        approved: true,
        rejections: vec![],
        source: DecisionSource::ManualImportPreview,
    }
}
