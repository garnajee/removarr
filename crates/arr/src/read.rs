//! `ArrRead` adapter: every Sonarr/Radarr read. Nothing here can mutate the service: the HTTP
//! core only offers GET without a `LiveMode` token.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use removarr_config::PathMapper;
use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrFileRef, ArrHistoryRecord, ArrLibraryEntry,
    ArrMediaManagement, ArrMediaRef, ArrQualitySnapshot, ArrQueueItem, ArrService, CommandState,
    DecisionSource, MediaIdentity, MediaKind, ReleaseRef, ReleaseSearchResult,
};
use removarr_domain::canonical::sha256_hex;
use removarr_domain::error::PortError;
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{ArrRead, Clock};
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::{MappedPath, TorrentHash};
use serde::de::DeserializeOwned;
use tokio::task::JoinSet;

use crate::dto::{
    self, CommandDto, EpisodeDto, FileDto, HistoryDto, ManualImportDto, MediaManagementDto,
    MovieDto, Paged, ProfileDto, ProfileItemDto, QueueDto, ReleaseDto, SeriesDto, SystemStatus,
};
use crate::error::ArrError;
use crate::http::{ArrSettings, Http, Timeout};
use crate::time::parse_rfc3339;

const PAGE_SIZE: u32 = 250;
const MAX_PAGES: u32 = 400;
/// Sonarr and Radarr keep interactive search results for about 30 minutes; a grab needs one.
const RELEASE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const RELEASE_CACHE_MAX: usize = 10_000;

/// Prefix of synthetic rejection reasons (fail-closed checks done by this adapter, never a
/// quality judgement). ARR's own reasons are kept verbatim.
pub(crate) const LOCAL_REASON: &str = "removarr: ";

/// Maps opaque release tokens (stored in plans) to ARR's raw release guid (kept in memory
/// only: guids can be tracker download URLs carrying a passkey, INV-31).
#[derive(Debug, Default)]
pub(crate) struct ReleaseCache {
    entries: Mutex<BTreeMap<String, (String, Instant)>>,
}

impl ReleaseCache {
    fn insert(&self, token: &str, guid: &str) {
        let Ok(mut m) = self.entries.lock() else {
            return;
        };
        let now = Instant::now();
        m.retain(|_, (_, at)| now.duration_since(*at) < RELEASE_CACHE_TTL);
        if m.len() >= RELEASE_CACHE_MAX {
            m.clear();
        }
        m.insert(token.to_string(), (guid.to_string(), now));
    }

    pub(crate) fn lookup(&self, token: &str) -> Option<String> {
        let m = self.entries.lock().ok()?;
        m.get(token)
            .filter(|(_, at)| at.elapsed() < RELEASE_CACHE_TTL)
            .map(|(g, _)| g.clone())
    }
}

/// Stable opaque token for a release: never the raw guid.
#[must_use]
pub fn release_token(indexer_id: i64, guid: &str) -> String {
    let h = sha256_hex(format!("release\0{indexer_id}\0{guid}").as_bytes());
    format!("rel-{}", &h[..32])
}

/// Read adapter for one Sonarr or Radarr instance. Clones share the HTTP pool, the
/// concurrency limit and the release cache.
#[derive(Clone)]
pub struct ArrReader {
    pub(crate) http: Http,
    paths: PathMapper,
    clock: Arc<dyn Clock>,
    pub(crate) releases: Arc<ReleaseCache>,
}

impl fmt::Debug for ArrReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArrReader")
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

fn kind_of(service: ArrService) -> ServiceKind {
    match service {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

fn sorted_ids(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let set: BTreeSet<i64> = ids.into_iter().collect();
    set.into_iter().collect()
}

/// Name of the profile cutoff (a quality or a quality group id). This reads ARR's own
/// settings; it never ranks qualities.
pub(crate) fn cutoff_name(profile: &ProfileDto) -> String {
    fn find(items: &[ProfileItemDto], id: i64) -> Option<String> {
        for it in items {
            if it.quality.is_none() && it.id == Some(id) {
                return it.name.clone();
            }
            if let Some(q) = &it.quality
                && q.id == id
            {
                return Some(q.name.clone());
            }
            if let Some(n) = find(&it.items, id) {
                return Some(n);
            }
        }
        None
    }
    find(&profile.items, profile.cutoff).unwrap_or_else(|| format!("id:{}", profile.cutoff))
}

impl ArrReader {
    pub fn new(
        settings: &ArrSettings,
        paths: PathMapper,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, PortError> {
        Ok(Self {
            http: Http::new(settings)?,
            paths,
            clock,
            releases: Arc::new(ReleaseCache::default()),
        })
    }

    fn svc(&self) -> ArrService {
        self.http.service()
    }

    fn api(&self) -> &'static str {
        "/api/v3"
    }

    fn map(&self, raw: &str) -> MappedPath {
        self.paths.to_app(kind_of(self.svc()), raw)
    }

    /// Service-side path of an application path (`None` when no mapping rule covers it).
    pub(crate) fn to_service(&self, p: &RootedPath) -> Option<String> {
        self.paths.to_service(kind_of(self.svc()), p)
    }

    pub(crate) fn scrub(&self, s: &str) -> String {
        self.http.scrub(s)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ArrError> {
        self.http.get(path, query, Timeout::Default).await
    }

    fn expect_service(&self, media: &ArrMediaRef) -> Result<(), ArrError> {
        if media.service == self.svc() {
            Ok(())
        } else {
            Err(ArrError::Validation(format!(
                "media belongs to {}, this connector is {}",
                media.service.as_str(),
                self.svc().as_str()
            )))
        }
    }

    fn sonarr_only(&self, what: &str) -> Result<(), ArrError> {
        if self.svc() == ArrService::Sonarr {
            Ok(())
        } else {
            Err(ArrError::Validation(format!(
                "{what} is a Sonarr-only read"
            )))
        }
    }

    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        extra: &[(&str, String)],
    ) -> Result<Vec<T>, ArrError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut q = vec![
                ("page", page.to_string()),
                ("pageSize", PAGE_SIZE.to_string()),
            ];
            q.extend(extra.iter().cloned());
            let p: Paged<T> = self.get(path, &q).await?;
            let n = p.records.len();
            out.extend(p.records);
            if n < PAGE_SIZE as usize || p.total_records.is_some_and(|t| out.len() as u64 >= t) {
                return Ok(out);
            }
        }
        Err(ArrError::TooLarge {
            op: format!("{} GET {path}", self.svc().as_str()),
            limit: u64::from(MAX_PAGES * PAGE_SIZE),
        })
    }

    pub(crate) fn file_ref(&self, f: &FileDto) -> ArrFileRef {
        let mut languages = dto::names(&f.languages);
        if languages.is_empty()
            && let Some(l) = &f.language
        {
            languages.push(l.name.clone());
        }
        ArrFileRef {
            file_id: f.id,
            path: self.map(f.path.as_deref().unwrap_or_default()),
            size: f.size,
            quality: dto::quality_name(f.quality.as_ref()),
            custom_formats: dto::names(&f.custom_formats),
            // Sonarr before v4 reports no file score; 0 is reported and decisions rely on
            // ARR's own accept/reject verdicts, never on this number.
            cf_score: f.custom_format_score.unwrap_or(0),
            cutoff_not_met: f.quality_cutoff_not_met.unwrap_or(false),
            languages,
        }
    }

    async fn series(&self, id: i64) -> Result<SeriesDto, ArrError> {
        self.get(&format!("{}/series/{id}", self.api()), &[]).await
    }

    async fn raw_episodes(
        &self,
        series_id: i64,
        season: Option<i32>,
    ) -> Result<Vec<EpisodeDto>, ArrError> {
        let mut q = vec![("seriesId", series_id.to_string())];
        if let Some(s) = season {
            q.push(("seasonNumber", s.to_string()));
        }
        self.get(&format!("{}/episode", self.api()), &q).await
    }

    async fn episode_files(&self, series_id: i64) -> Result<Vec<FileDto>, ArrError> {
        self.get(
            &format!("{}/episodefile", self.api()),
            &[("seriesId", series_id.to_string())],
        )
        .await
    }

    async fn movie(&self, id: i64) -> Result<MovieDto, ArrError> {
        self.get(&format!("{}/movie/{id}", self.api()), &[]).await
    }

    async fn movie_files(&self, movie_id: i64) -> Result<Vec<FileDto>, ArrError> {
        self.get(
            &format!("{}/moviefile", self.api()),
            &[("movieId", movie_id.to_string())],
        )
        .await
    }

    async fn profile(&self, id: i64) -> Result<ProfileDto, ArrError> {
        self.get(&format!("{}/qualityprofile/{id}", self.api()), &[])
            .await
    }

    fn series_ids(s: &SeriesDto) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        if let Some(v) = s.tvdb_id.filter(|v| *v > 0) {
            m.insert("tvdb".into(), v.to_string());
        }
        if let Some(v) = s.tmdb_id.filter(|v| *v > 0) {
            m.insert("tmdb".into(), v.to_string());
        }
        if let Some(v) = s.tv_maze_id.filter(|v| *v > 0) {
            m.insert("tvmaze".into(), v.to_string());
        }
        if let Some(v) = s.imdb_id.as_ref().filter(|v| !v.is_empty()) {
            m.insert("imdb".into(), v.clone());
        }
        m
    }

    fn movie_ids(m: &MovieDto) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(v) = m.tmdb_id.filter(|v| *v > 0) {
            out.insert("tmdb".into(), v.to_string());
        }
        if let Some(v) = m.imdb_id.as_ref().filter(|v| !v.is_empty()) {
            out.insert("imdb".into(), v.clone());
        }
        out
    }

    fn episode_identity(
        series: &SeriesDto,
        eps: &[&EpisodeDto],
        media: ArrMediaRef,
    ) -> MediaIdentity {
        let first = eps.first();
        let season = first.map(|e| e.season_number);
        let same_season = eps.iter().all(|e| Some(e.season_number) == season);
        MediaIdentity {
            kind: if season == Some(0) {
                MediaKind::Special
            } else {
                MediaKind::Episode
            },
            arr: Some(media),
            external_ids: Self::series_ids(series),
            title: series.title.clone(),
            year: series.year,
            season: season.filter(|_| same_season),
            episode: first.filter(|_| eps.len() == 1).map(|e| e.episode_number),
            absolute: first
                .filter(|_| eps.len() == 1)
                .and_then(|e| e.absolute_episode_number),
        }
    }

    fn movie_identity(m: &MovieDto) -> MediaIdentity {
        MediaIdentity {
            kind: MediaKind::Movie,
            arr: Some(ArrMediaRef {
                service: ArrService::Radarr,
                media_id: m.id,
                episode_ids: vec![],
            }),
            external_ids: Self::movie_ids(m),
            title: m.title.clone(),
            year: m.year,
            season: None,
            episode: None,
            absolute: None,
        }
    }

    /// Library entries of one series: one entry per episode file, with the episodes ARR
    /// assigns to it.
    async fn series_entries(self, series: SeriesDto) -> Result<Vec<ArrLibraryEntry>, ArrError> {
        let files = self.episode_files(series.id).await?;
        let episodes = self.raw_episodes(series.id, None).await?;
        let mut out = Vec::with_capacity(files.len());
        for f in &files {
            let mut eps: Vec<&EpisodeDto> = episodes
                .iter()
                .filter(|e| e.file_id() == Some(f.id))
                .collect();
            eps.sort_by_key(|e| (e.season_number, e.episode_number));
            let media = ArrMediaRef {
                service: ArrService::Sonarr,
                media_id: series.id,
                episode_ids: sorted_ids(eps.iter().map(|e| e.id)),
            };
            out.push(ArrLibraryEntry {
                identity: Self::episode_identity(&series, &eps, media.clone()),
                media,
                file: self.file_ref(f),
                // ARR may act on a file while any of its episodes is monitored.
                monitored: series.monitored && eps.iter().any(|e| e.monitored),
            });
        }
        Ok(out)
    }

    /// Raw manual-import preview rows for a service-side folder (GET only).
    pub(crate) async fn preview_raw(
        &self,
        folder: &str,
        media: Option<&ArrMediaRef>,
    ) -> Result<Vec<ManualImportDto>, ArrError> {
        let mut q = vec![
            ("folder", folder.to_string()),
            ("filterExistingFiles", "false".to_string()),
        ];
        if let Some(m) = media {
            q.push(match self.svc() {
                ArrService::Sonarr => ("seriesId", m.media_id.to_string()),
                ArrService::Radarr => ("movieId", m.media_id.to_string()),
            });
        }
        self.get(&format!("{}/manualimport", self.api()), &q).await
    }

    pub(crate) fn preview_media_id(&self, d: &ManualImportDto) -> Option<i64> {
        match self.svc() {
            ArrService::Sonarr => d.series.as_ref().map(|s| s.id),
            ArrService::Radarr => d.movie.as_ref().map(|m| m.id),
        }
    }

    fn decision(&self, d: &ManualImportDto, filter: Option<&ArrMediaRef>) -> ArrCandidateDecision {
        let mut rejections: Vec<String> = d
            .rejections
            .iter()
            .map(|r| self.scrub(&dto::rejection_text(r)))
            .collect();
        let media_id = self.preview_media_id(d);
        let episode_ids = sorted_ids(d.episodes.iter().map(|e| e.id));
        if media_id.is_none() {
            rejections.push(format!("{LOCAL_REASON}media not identified by ARR"));
        }
        if let (Some(f), Some(id)) = (filter, media_id)
            && f.media_id != id
        {
            rejections.push(format!("{LOCAL_REASON}ARR identified a different media"));
        }
        if self.svc() == ArrService::Sonarr && episode_ids.is_empty() {
            rejections.push(format!("{LOCAL_REASON}ARR assigned no episode"));
        }
        let subject = match self.map(&d.path) {
            MappedPath::Mapped { path, .. } => path.to_string(),
            other => {
                rejections.push(format!(
                    "{LOCAL_REASON}path does not map into a configured root"
                ));
                other.raw().to_string()
            }
        };
        let quality = d
            .quality
            .as_ref()
            .and_then(|q| q.pointer("/quality/name"))
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("Unknown")
            .to_string();
        ArrCandidateDecision {
            subject,
            size: d.size,
            quality,
            custom_formats: dto::names(&d.custom_formats),
            cf_score: d.custom_format_score.unwrap_or(0),
            episode_ids,
            approved: rejections.is_empty(),
            rejections,
            source: DecisionSource::ManualImportPreview,
        }
    }

    fn release(&self, r: ReleaseDto) -> ReleaseSearchResult {
        let rejections: Vec<String> = r
            .rejections
            .iter()
            .map(|v| self.scrub(&dto::rejection_text(v)))
            .collect();
        let token = release_token(r.indexer_id, &r.guid);
        self.releases.insert(&token, &r.guid);
        ReleaseSearchResult {
            release: ReleaseRef {
                guid: token,
                indexer_id: r.indexer_id,
            },
            indexer: r.indexer.unwrap_or_default(),
            title: self.scrub(&r.title),
            size: r.size,
            seeders: r.seeders.and_then(|v| u32::try_from(v).ok()),
            leechers: r.leechers.and_then(|v| u32::try_from(v).ok()),
            age_days: r.age.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
            protocol: r
                .protocol
                .as_ref()
                .map(dto::scalar_text)
                .unwrap_or_default(),
            quality: dto::quality_name(r.quality.as_ref()),
            custom_formats: dto::names(&r.custom_formats),
            cf_score: r.custom_format_score.unwrap_or(0),
            // A result counts as approved only when ARR approves it with no rejection.
            approved: r.approved && rejections.is_empty(),
            rejections,
            download_allowed: r.download_allowed,
        }
    }

    fn queue_item(&self, q: QueueDto) -> ArrQueueItem {
        let media = match self.svc() {
            ArrService::Sonarr => q.series_id.map(|id| ArrMediaRef {
                service: ArrService::Sonarr,
                media_id: id,
                episode_ids: q.episode_id.into_iter().collect(),
            }),
            ArrService::Radarr => q.movie_id.map(|id| ArrMediaRef {
                service: ArrService::Radarr,
                media_id: id,
                episode_ids: vec![],
            }),
        };
        let mut messages = Vec::new();
        for m in &q.status_messages {
            if let Some(t) = m.title.as_ref().filter(|t| !t.is_empty()) {
                messages.push(self.scrub(t));
            }
            messages.extend(m.messages.iter().map(|s| self.scrub(s)));
        }
        if let Some(e) = q.error_message.as_ref().filter(|e| !e.is_empty()) {
            messages.push(self.scrub(e));
        }
        ArrQueueItem {
            id: q.id,
            download_id: q.download_id.and_then(|d| TorrentHash::new(d).ok()),
            media,
            status: q
                .tracked_download_status
                .unwrap_or_else(|| "unknown".into()),
            tracked_state: q.tracked_download_state.unwrap_or_else(|| "unknown".into()),
            messages,
        }
    }

    fn history_record(&self, h: HistoryDto) -> Result<ArrHistoryRecord, ArrError> {
        let media = match self.svc() {
            ArrService::Sonarr => h.series_id.filter(|id| *id > 0).map(|id| ArrMediaRef {
                service: ArrService::Sonarr,
                media_id: id,
                episode_ids: h.episode_id.filter(|e| *e > 0).into_iter().collect(),
            }),
            ArrService::Radarr => h.movie_id.filter(|id| *id > 0).map(|id| ArrMediaRef {
                service: ArrService::Radarr,
                media_id: id,
                episode_ids: vec![],
            }),
        };
        let date = parse_rfc3339(&h.date).ok_or_else(|| ArrError::Decode {
            op: format!("{} GET {}/history", self.svc().as_str(), self.api()),
        })?;
        Ok(ArrHistoryRecord {
            download_id: h.download_id.and_then(|d| TorrentHash::new(d).ok()),
            media,
            event_type: dto::scalar_text(&h.event_type),
            source_title: self.scrub(h.source_title.as_deref().unwrap_or_default()),
            date,
        })
    }

    fn episode(&self, e: &EpisodeDto) -> ArrEpisode {
        let now = self.clock.now();
        ArrEpisode {
            id: e.id,
            series_id: e.series_id,
            season: e.season_number,
            episode: e.episode_number,
            absolute: e.absolute_episode_number,
            scene_absolute: e.scene_absolute_episode_number,
            scene_season: e.scene_season_number,
            scene_episode: e.scene_episode_number,
            has_file: e.has_file && e.file_id().is_some(),
            file_id: e.file_id(),
            monitored: e.monitored,
            // Unknown or unparsable air dates count as not aired.
            aired: e
                .air_date_utc
                .as_deref()
                .and_then(parse_rfc3339)
                .is_some_and(|t| t <= now),
        }
    }

    async fn sonarr_snapshot(
        &self,
        media: &ArrMediaRef,
    ) -> Result<(MediaIdentity, i64, Vec<ArrFileRef>), ArrError> {
        let series = self.series(media.media_id).await?;
        let episodes = self.raw_episodes(series.id, None).await?;
        let files = self.episode_files(series.id).await?;
        let wanted: BTreeSet<i64> = media.episode_ids.iter().copied().collect();
        let mut eps: Vec<&EpisodeDto> = episodes
            .iter()
            .filter(|e| wanted.is_empty() || wanted.contains(&e.id))
            .collect();
        if eps.len() != wanted.len() && !wanted.is_empty() {
            return Err(ArrError::StateChanged(format!(
                "sonarr series {} no longer lists every requested episode",
                series.id
            )));
        }
        eps.sort_by_key(|e| (e.season_number, e.episode_number));
        let file_ids: BTreeSet<i64> = eps.iter().filter_map(|e| e.file_id()).collect();
        let mut current: Vec<ArrFileRef> = files
            .iter()
            .filter(|f| wanted.is_empty() || file_ids.contains(&f.id))
            .map(|f| self.file_ref(f))
            .collect();
        current.sort_by_key(|f| f.file_id);
        let normalized = ArrMediaRef {
            service: ArrService::Sonarr,
            media_id: series.id,
            episode_ids: sorted_ids(wanted.iter().copied()),
        };
        let identity = if wanted.is_empty() {
            MediaIdentity {
                kind: MediaKind::Episode,
                arr: Some(normalized),
                external_ids: Self::series_ids(&series),
                title: series.title.clone(),
                year: series.year,
                season: None,
                episode: None,
                absolute: None,
            }
        } else {
            Self::episode_identity(&series, &eps, normalized)
        };
        Ok((identity, series.quality_profile_id, current))
    }

    async fn radarr_snapshot(
        &self,
        media: &ArrMediaRef,
    ) -> Result<(MediaIdentity, i64, Vec<ArrFileRef>), ArrError> {
        let movie = self.movie(media.media_id).await?;
        let mut current: Vec<ArrFileRef> = self
            .movie_files(movie.id)
            .await?
            .iter()
            .map(|f| self.file_ref(f))
            .collect();
        current.sort_by_key(|f| f.file_id);
        Ok((
            Self::movie_identity(&movie),
            movie.quality_profile_id,
            current,
        ))
    }

    async fn status_inner(&self) -> Result<String, ArrError> {
        let s: SystemStatus = self
            .get(&format!("{}/system/status", self.api()), &[])
            .await?;
        if let Some(app) = s.app_name.as_deref()
            && !app.eq_ignore_ascii_case(self.svc().as_str())
        {
            return Err(ArrError::Validation(format!(
                "{} url points to a different application",
                self.svc().as_str()
            )));
        }
        Ok(s.version)
    }

    async fn library_inner(&self) -> Result<Vec<ArrLibraryEntry>, ArrError> {
        let mut out = match self.svc() {
            ArrService::Radarr => {
                let movies: Vec<MovieDto> = self.get(&format!("{}/movie", self.api()), &[]).await?;
                movies
                    .iter()
                    .filter_map(|m| {
                        m.movie_file.as_ref().map(|f| ArrLibraryEntry {
                            media: ArrMediaRef {
                                service: ArrService::Radarr,
                                media_id: m.id,
                                episode_ids: vec![],
                            },
                            identity: Self::movie_identity(m),
                            file: self.file_ref(f),
                            monitored: m.monitored,
                        })
                    })
                    .collect::<Vec<_>>()
            }
            ArrService::Sonarr => {
                let series: Vec<SeriesDto> =
                    self.get(&format!("{}/series", self.api()), &[]).await?;
                // Fan out per series; the HTTP core bounds the in-flight requests.
                let mut set = JoinSet::new();
                for s in series {
                    let me = self.clone();
                    set.spawn(me.series_entries(s));
                }
                let mut all = Vec::new();
                while let Some(joined) = set.join_next().await {
                    let entries = joined.map_err(|_| ArrError::Unavailable {
                        op: "sonarr library".into(),
                        reason: "worker task failed",
                    })??;
                    all.extend(entries);
                }
                all
            }
        };
        out.sort_by_key(|e| (e.media.media_id, e.file.file_id));
        Ok(out)
    }

    async fn snapshot_inner(&self, media: &ArrMediaRef) -> Result<ArrQualitySnapshot, ArrError> {
        self.expect_service(media)?;
        let version = self.status_inner().await?;
        let (identity, profile_id, current_files) = match self.svc() {
            ArrService::Sonarr => self.sonarr_snapshot(media).await?,
            ArrService::Radarr => self.radarr_snapshot(media).await?,
        };
        let profile = self.profile(profile_id).await?;
        let mm = self.media_management_inner().await?;
        let format_scores: BTreeMap<String, i64> = profile
            .format_items
            .iter()
            .map(|f| {
                let name = f
                    .name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("cf:{}", f.format));
                (name, f.score)
            })
            .collect();
        let snap = ArrQualitySnapshot {
            service: self.svc(),
            service_version: version,
            media: identity,
            profile_id: profile.id,
            profile_name: profile.name.clone(),
            cutoff: cutoff_name(&profile),
            upgrade_allowed: profile.upgrade_allowed,
            min_format_score: profile.min_format_score.unwrap_or(0),
            cutoff_format_score: profile.cutoff_format_score.unwrap_or(0),
            format_scores,
            current_files,
            candidates: vec![],
            media_management: Some(mm),
            captured_at: self.clock.now(),
            fingerprint: String::new(),
        };
        snap.seal().map_err(|_| {
            ArrError::Config(format!(
                "{} snapshot could not be fingerprinted",
                self.svc().as_str()
            ))
        })
    }

    async fn media_management_inner(&self) -> Result<ArrMediaManagement, ArrError> {
        let m: MediaManagementDto = self
            .get(&format!("{}/config/mediamanagement", self.api()), &[])
            .await?;
        Ok(ArrMediaManagement {
            recycle_bin: m
                .recycle_bin
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| self.map(p)),
            recycle_bin_cleanup_days: m
                .recycle_bin_cleanup_days
                .and_then(|d| u32::try_from(d).ok()),
            // Unknown means disabled (pessimistic: the plan then warns before approval).
            hardlinks_enabled: m.copy_using_hardlinks.unwrap_or(false),
            captured_at: self.clock.now(),
        })
    }

    async fn search_inner(
        &self,
        media: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, ArrError> {
        self.expect_service(media)?;
        let query = match self.svc() {
            ArrService::Radarr => vec![("movieId", media.media_id.to_string())],
            ArrService::Sonarr => match media.episode_ids.as_slice() {
                [] => {
                    return Err(ArrError::Validation(
                        "a Sonarr search needs at least one episode id".into(),
                    ));
                }
                [one] => vec![("episodeId", one.to_string())],
                many => {
                    let episodes = self.raw_episodes(media.media_id, None).await?;
                    let wanted: BTreeSet<i64> = many.iter().copied().collect();
                    let seasons: BTreeSet<i32> = episodes
                        .iter()
                        .filter(|e| wanted.contains(&e.id))
                        .map(|e| e.season_number)
                        .collect();
                    let found = episodes.iter().filter(|e| wanted.contains(&e.id)).count();
                    match seasons.iter().next() {
                        Some(season) if seasons.len() == 1 && found == wanted.len() => vec![
                            ("seriesId", media.media_id.to_string()),
                            ("seasonNumber", season.to_string()),
                        ],
                        _ => {
                            return Err(ArrError::Validation(
                                "a multi-episode Sonarr search must target one season".into(),
                            ));
                        }
                    }
                }
            },
        };
        let raw: Vec<ReleaseDto> = self
            .http
            .get(&format!("{}/release", self.api()), &query, Timeout::Search)
            .await?;
        Ok(raw.into_iter().map(|r| self.release(r)).collect())
    }
}

#[async_trait]
impl ArrRead for ArrReader {
    fn service(&self) -> ArrService {
        self.svc()
    }

    async fn status(&self) -> Result<String, PortError> {
        Ok(self.status_inner().await?)
    }

    async fn library(&self) -> Result<Vec<ArrLibraryEntry>, PortError> {
        Ok(self.library_inner().await?)
    }

    async fn queue(&self) -> Result<Vec<ArrQueueItem>, PortError> {
        let unknown = match self.svc() {
            ArrService::Sonarr => "includeUnknownSeriesItems",
            ArrService::Radarr => "includeUnknownMovieItems",
        };
        let raw: Vec<QueueDto> = self
            .paged(
                &format!("{}/queue", self.api()),
                &[(unknown, "true".to_string())],
            )
            .await?;
        Ok(raw.into_iter().map(|q| self.queue_item(q)).collect())
    }

    async fn history_for_download(
        &self,
        hash: &TorrentHash,
    ) -> Result<Vec<ArrHistoryRecord>, PortError> {
        // ARR stores qBittorrent download ids in upper case and filters case-sensitively.
        let raw: Vec<HistoryDto> = self
            .paged(
                &format!("{}/history", self.api()),
                &[("downloadId", hash.as_str().to_ascii_uppercase())],
            )
            .await?;
        let mut out = Vec::with_capacity(raw.len());
        for h in raw {
            let rec = self.history_record(h)?;
            if rec.download_id.as_ref() == Some(hash) {
                out.push(rec);
            }
        }
        Ok(out)
    }

    async fn quality_snapshot(&self, media: &ArrMediaRef) -> Result<ArrQualitySnapshot, PortError> {
        Ok(self.snapshot_inner(media).await?)
    }

    async fn manual_import_preview(
        &self,
        folder: &RootedPath,
        media: Option<&ArrMediaRef>,
    ) -> Result<Vec<ArrCandidateDecision>, PortError> {
        if let Some(m) = media {
            self.expect_service(m)?;
        }
        let svc_folder = self.to_service(folder).ok_or_else(|| {
            ArrError::Validation(format!(
                "no {} path mapping covers {folder}",
                self.svc().as_str()
            ))
        })?;
        let raw = self.preview_raw(&svc_folder, media).await?;
        let mut out: Vec<ArrCandidateDecision> =
            raw.iter().map(|d| self.decision(d, media)).collect();
        out.sort_by(|a, b| a.subject.cmp(&b.subject));
        Ok(out)
    }

    async fn episodes(
        &self,
        series_id: i64,
        season: Option<i32>,
    ) -> Result<Vec<ArrEpisode>, PortError> {
        self.sonarr_only("episodes")?;
        let raw = self.raw_episodes(series_id, season).await?;
        let mut out: Vec<ArrEpisode> = raw.iter().map(|e| self.episode(e)).collect();
        out.sort_by_key(|e| (e.season, e.episode, e.id));
        Ok(out)
    }

    async fn search_releases(
        &self,
        media: &ArrMediaRef,
    ) -> Result<Vec<ReleaseSearchResult>, PortError> {
        Ok(self.search_inner(media).await?)
    }

    async fn command_status(&self, id: i64) -> Result<CommandState, PortError> {
        let c: CommandDto = self
            .get(&format!("{}/command/{id}", self.api()), &[])
            .await?;
        let msg = || self.scrub(c.message.as_deref().unwrap_or_default());
        Ok(match c.status.to_ascii_lowercase().as_str() {
            "queued" => CommandState::Queued,
            "started" => CommandState::Started,
            "completed" if c.result.as_deref() == Some("unsuccessful") => {
                CommandState::Failed { message: msg() }
            }
            "completed" => CommandState::Completed,
            "failed" | "aborted" | "cancelled" | "orphaned" => CommandState::Failed {
                message: format!("{}: {}", c.status, msg()),
            },
            _ => CommandState::Unknown(self.scrub(&c.status)),
        })
    }

    async fn media_management(&self) -> Result<ArrMediaManagement, PortError> {
        Ok(self.media_management_inner().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(cutoff: i64) -> ProfileDto {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": "P", "upgradeAllowed": true, "cutoff": cutoff,
            "items": [
                { "quality": { "id": 4, "name": "HDTV-720p" }, "items": [], "allowed": true },
                { "id": 1001, "name": "Group-HD", "allowed": true, "items": [
                    { "quality": { "id": 3, "name": "WEBDL-1080p" }, "items": [] },
                    { "quality": { "id": 7, "name": "Bluray-1080p" }, "items": [] }
                ]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn cutoff_resolves_group_quality_or_unknown_id() {
        assert_eq!(cutoff_name(&profile(1001)), "Group-HD");
        assert_eq!(cutoff_name(&profile(7)), "Bluray-1080p");
        assert_eq!(cutoff_name(&profile(4)), "HDTV-720p");
        assert_eq!(cutoff_name(&profile(99)), "id:99");
    }

    #[test]
    fn release_token_is_opaque_and_stable() {
        let guid = "https://tracker.invalid/dl/1?passkey=SECRET";
        let t = release_token(3, guid);
        assert_eq!(t, release_token(3, guid));
        assert_ne!(t, release_token(4, guid));
        assert!(t.starts_with("rel-") && !t.contains("SECRET") && t.len() == 36);
    }

    #[test]
    fn release_cache_round_trip() {
        let c = ReleaseCache::default();
        c.insert("rel-a", "guid-a");
        assert_eq!(c.lookup("rel-a").as_deref(), Some("guid-a"));
        assert_eq!(c.lookup("rel-b"), None);
    }
}
