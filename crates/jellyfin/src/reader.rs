//! Read-only adapter: health, version, libraries, items and media paths, refresh state.
//! It has no code path that issues anything but GET (the commander is a separate type).

use std::collections::BTreeMap;

use async_trait::async_trait;
use removarr_config::{Config, PathMapper, Secret};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::jellyfin::{JellyfinItemRef, JellyfinLibrary};
use removarr_domain::path::RootedPath;
use removarr_domain::ports::JellyfinRead;
use removarr_domain::session::ServiceKind;
use url::Url;

use crate::dto::{ItemsPage, RawItem, ScheduledTask, SystemInfo, VirtualFolder};
use crate::http::{Core, HttpSettings};

/// Item types Jellyfin reports for video files. `Video` is what a file becomes when the
/// library could not classify it, which is exactly what recognition checks look for.
const ITEM_TYPES: &str = "Movie,Episode,Video";
const PAGE_SIZE: usize = 500;
/// Hard bound on pages per listing (500 * 2000 = 1 M items).
const MAX_PAGES: usize = 2000;
/// Scheduled task key of "Scan Media Library".
const LIBRARY_SCAN_TASK_KEY: &str = "RefreshLibrary";

/// An item with the extra fields the domain `JellyfinItemRef` does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JellyfinItemDetail {
    pub item: JellyfinItemRef,
    pub series_id: Option<String>,
    /// `IndexNumberEnd` of a multi-episode file.
    pub episode_end: Option<i32>,
}

/// Result of correlating requested paths with Jellyfin items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemLookup {
    /// Every requested path is a key. An empty list means Jellyfin has no item at that path;
    /// more than one entry means Jellyfin lists the same file twice (ambiguous).
    pub matches: BTreeMap<RootedPath, Vec<JellyfinItemDetail>>,
    /// Items scanned in total.
    pub scanned: usize,
    /// Items whose Jellyfin path has no usable mapping (`NoRule`, `Invalid`, outside roots).
    /// Counted, never listed (paths are sensitive).
    pub unmapped: usize,
}

#[derive(Debug, Clone)]
pub struct JellyfinReader {
    core: Core,
    paths: PathMapper,
}

impl JellyfinReader {
    pub fn new(
        url: &Url,
        api_key: &Secret,
        paths: PathMapper,
        settings: HttpSettings,
    ) -> Result<Self, PortError> {
        Ok(Self {
            core: Core::new(url, api_key, settings)?,
            paths,
        })
    }

    /// `None` when Jellyfin is not configured.
    pub fn from_config(cfg: &Config) -> Option<Result<Self, PortError>> {
        let svc = cfg.services.jellyfin.as_ref()?;
        let settings = HttpSettings {
            max_response_bytes: cfg.limits.max_response_bytes,
            ..HttpSettings::default()
        };
        Some(Self::new(
            &svc.url,
            &svc.api_key,
            cfg.paths.clone(),
            settings,
        ))
    }

    /// `GET /health`: the server answers `Healthy` without authentication.
    pub async fn health(&self) -> Result<(), PortError> {
        let body = self.core.get_bytes("GET /health", &["health"], &[]).await?;
        if String::from_utf8_lossy(&body).trim() == "Healthy" {
            Ok(())
        } else {
            Err(PortError::new(
                ErrorClass::Unavailable,
                "jellyfin GET /health: server reports unhealthy",
            ))
        }
    }

    fn map_path(&self, raw: &str) -> removarr_domain::torrent::MappedPath {
        self.paths.to_app(ServiceKind::Jellyfin, raw)
    }

    fn detail(&self, raw: RawItem, path: &str) -> JellyfinItemDetail {
        let is_episode = raw.kind == "Episode";
        let provider_ids = raw
            .provider_ids
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k, v)))
            .collect();
        JellyfinItemDetail {
            item: JellyfinItemRef {
                id: raw.id,
                kind: raw.kind,
                name: raw.name,
                path: self.map_path(path),
                series: raw.series_name,
                season: if is_episode {
                    raw.parent_index_number
                } else {
                    None
                },
                episode: if is_episode { raw.index_number } else { None },
                provider_ids,
            },
            series_id: raw.series_id,
            episode_end: raw.index_number_end,
        }
    }

    /// Lists every video item (bounded paging) and correlates the requested paths.
    ///
    /// Fails (never a partial success) when a page fails, is malformed, or the listing is
    /// larger than the page bound.
    pub async fn lookup(&self, paths: &[RootedPath]) -> Result<ItemLookup, PortError> {
        // Requested path -> the string Jellyfin knows it by.
        let mut wanted: BTreeMap<RootedPath, Vec<JellyfinItemDetail>> = BTreeMap::new();
        for p in paths {
            if self.paths.to_service(ServiceKind::Jellyfin, p).is_none() {
                return Err(PortError::new(
                    ErrorClass::Config,
                    "jellyfin: no path mapping covers a requested path",
                ));
            }
            wanted.insert(p.clone(), Vec::new());
        }
        let mut out = ItemLookup::default();
        let mut start = 0usize;
        for _ in 0..MAX_PAGES {
            let (s, l) = (start.to_string(), PAGE_SIZE.to_string());
            let page: ItemsPage = self
                .core
                .get_json(
                    "GET /Items",
                    &["Items"],
                    &[
                        ("Recursive", "true"),
                        ("IncludeItemTypes", ITEM_TYPES),
                        ("Fields", "Path,ProviderIds"),
                        ("EnableTotalRecordCount", "true"),
                        ("StartIndex", &s),
                        ("Limit", &l),
                    ],
                )
                .await?;
            let n = page.items.len();
            for raw in page.items {
                out.scanned += 1;
                let Some(path) = raw.path.clone() else {
                    continue;
                };
                let detail = self.detail(raw, &path);
                match detail.item.path.as_rooted() {
                    Some(rooted) => {
                        if let Some(slot) = wanted.get_mut(rooted) {
                            slot.push(detail);
                        }
                    }
                    None => out.unmapped += 1,
                }
            }
            start += n;
            let done = n == 0 || page.total_record_count.is_some_and(|t| start as u64 >= t);
            if done {
                for v in wanted.values_mut() {
                    v.sort_by(|a, b| a.item.id.cmp(&b.item.id));
                }
                out.matches = wanted;
                return Ok(out);
            }
        }
        Err(PortError::new(
            ErrorClass::Rejected,
            "jellyfin GET /Items: listing exceeds the page bound",
        ))
    }
}

#[async_trait]
impl JellyfinRead for JellyfinReader {
    /// Server version from the authenticated `GET /System/Info` (so it also proves the token).
    async fn status(&self) -> Result<String, PortError> {
        let info: SystemInfo = self
            .core
            .get_json("GET /System/Info", &["System", "Info"], &[])
            .await?;
        Ok(info.version)
    }

    async fn libraries(&self) -> Result<Vec<JellyfinLibrary>, PortError> {
        let folders: Vec<VirtualFolder> = self
            .core
            .get_json(
                "GET /Library/VirtualFolders",
                &["Library", "VirtualFolders"],
                &[],
            )
            .await?;
        Ok(folders
            .into_iter()
            .map(|f| JellyfinLibrary {
                name: f.name,
                locations: f.locations.iter().map(|l| self.map_path(l)).collect(),
            })
            .collect())
    }

    /// An item is returned only when exactly one item has that path. No item, or an ambiguous
    /// duplicate, yields `None`; use [`JellyfinReader::lookup`] to tell them apart.
    async fn items_by_paths(
        &self,
        paths: &[RootedPath],
    ) -> Result<BTreeMap<RootedPath, Option<JellyfinItemRef>>, PortError> {
        let lookup = self.lookup(paths).await?;
        Ok(lookup
            .matches
            .into_iter()
            .map(|(p, mut v)| {
                let item = if v.len() == 1 {
                    v.pop().map(|d| d.item)
                } else {
                    None
                };
                (p, item)
            })
            .collect())
    }

    async fn library_refresh_running(&self) -> Result<bool, PortError> {
        let tasks: Vec<ScheduledTask> = self
            .core
            .get_json(
                "GET /ScheduledTasks",
                &["ScheduledTasks"],
                &[("isHidden", "false")],
            )
            .await?;
        let task = tasks
            .iter()
            .find(|t| t.key.as_deref() == Some(LIBRARY_SCAN_TASK_KEY))
            .ok_or_else(|| {
                PortError::new(
                    ErrorClass::NotFound,
                    "jellyfin GET /ScheduledTasks: library scan task not listed",
                )
            })?;
        Ok(matches!(
            task.state.as_deref(),
            Some("Running" | "Cancelling")
        ))
    }
}
