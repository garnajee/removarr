//! `ArrCommands` adapter: the separately authorized mutators. Construction needs a `LiveMode`
//! token (dry-run processes cannot build one) and each method checks its capability flag
//! before any request is sent. Mutating requests are never retried.
//!
//! Manual import always asks ARR to copy (ARR hardlinks when its hardlink setting is on). The
//! request type has no mode field, so no other mode can be expressed (ADR-0010, CP1-F01).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use removarr_config::{Capabilities, LiveMode};
use removarr_domain::arr::{ArrMediaRef, ArrService, ManualImportRequest, ReleaseRef};
use removarr_domain::error::PortError;
use removarr_domain::policy::Capability;
use removarr_domain::ports::ArrCommands;
use reqwest::Method;
use serde_json::{Value, json};

use crate::dto::{self, CommandDto, ManualImportDto};
use crate::error::ArrError;
use crate::read::ArrReader;

#[derive(Debug, Clone, Copy)]
struct Granted {
    manual_import: bool,
    grab: bool,
    queue_remove: bool,
    set_monitored: bool,
}

/// Live mutator for one Sonarr or Radarr instance.
#[derive(Debug, Clone)]
pub struct ArrCommander {
    reader: ArrReader,
    granted: Granted,
    live: LiveMode,
}

impl ArrCommander {
    /// `reader` should be the same instance (or a clone) the evaluate step searches with:
    /// clones share the in-memory release cache that `grab` needs.
    #[must_use]
    pub fn new(reader: ArrReader, caps: &Capabilities, live: &LiveMode) -> Self {
        Self {
            reader,
            granted: Granted {
                manual_import: caps.is_enabled(Capability::ArrManualImport),
                grab: caps.is_enabled(Capability::ArrGrab),
                queue_remove: caps.is_enabled(Capability::ArrQueueRemove),
                set_monitored: caps.is_enabled(Capability::ArrSetMonitored),
            },
            live: live.clone(),
        }
    }

    fn svc(&self) -> ArrService {
        self.reader.http.service()
    }

    fn require(&self, on: bool, cap: Capability) -> Result<(), ArrError> {
        if on {
            Ok(())
        } else {
            Err(ArrError::CapabilityDisabled(cap.as_str()))
        }
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

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Vec<u8>, ArrError> {
        self.reader
            .http
            .send(&self.live, method, path, query, body)
            .await
    }

    async fn command(&self, body: &Value) -> Result<i64, ArrError> {
        let path = "/api/v3/command";
        let raw = self.send(Method::POST, path, &[], Some(body)).await?;
        let c: CommandDto = self.reader.http.decode(&Method::POST, path, &raw)?;
        Ok(c.id)
    }

    fn rescan_body(&self, media_id: Option<i64>) -> Value {
        match (self.svc(), media_id) {
            (ArrService::Sonarr, Some(id)) => json!({ "name": "RescanSeries", "seriesId": id }),
            (ArrService::Sonarr, None) => json!({ "name": "RescanSeries" }),
            (ArrService::Radarr, Some(id)) => json!({ "name": "RescanMovie", "movieId": id }),
            (ArrService::Radarr, None) => json!({ "name": "RescanMovie" }),
        }
    }

    /// Re-reads ARR's preview for every file and checks that ARR still accepts it for the
    /// requested media and episodes. Any difference refuses the import before a request is
    /// sent; ARR's own quality and language objects are passed back verbatim.
    async fn import_files(&self, req: &ManualImportRequest) -> Result<Vec<Value>, ArrError> {
        if req.files.is_empty() {
            return Err(ArrError::Validation("manual import without files".into()));
        }
        let mut by_folder: BTreeMap<String, Vec<(String, &ArrMediaRef)>> = BTreeMap::new();
        for (path, media) in &req.files {
            self.expect_service(media)?;
            if self.svc() == ArrService::Sonarr && media.episode_ids.is_empty() {
                return Err(ArrError::Validation(format!(
                    "manual import of {path} names no episode"
                )));
            }
            let svc_path = self.reader.to_service(path).ok_or_else(|| {
                ArrError::Validation(format!(
                    "no {} path mapping covers {path}",
                    self.svc().as_str()
                ))
            })?;
            let folder = svc_path
                .rsplit_once('/')
                .map(|(dir, _)| if dir.is_empty() { "/" } else { dir })
                .unwrap_or("/")
                .to_string();
            by_folder.entry(folder).or_default().push((svc_path, media));
        }
        let mut files = Vec::new();
        for (folder, wanted) in by_folder {
            let preview = self.reader.preview_raw(&folder, Some(wanted[0].1)).await?;
            for (svc_path, media) in wanted {
                let row = preview.iter().find(|d| d.path == svc_path).ok_or_else(|| {
                    ArrError::StateChanged(format!(
                        "{} preview no longer lists a requested file",
                        self.svc().as_str()
                    ))
                })?;
                self.check_row(row, media)?;
                files.push(self.file_entry(row, media));
            }
        }
        Ok(files)
    }

    fn check_row(&self, row: &ManualImportDto, media: &ArrMediaRef) -> Result<(), ArrError> {
        let svc = self.svc().as_str();
        if !row.rejections.is_empty() {
            let reasons: Vec<String> = row
                .rejections
                .iter()
                .map(|r| self.reader.scrub(&dto::rejection_text(r)))
                .collect();
            return Err(ArrError::StateChanged(format!(
                "{svc} rejects a requested file: {}",
                reasons.join("; ")
            )));
        }
        if self.reader.preview_media_id(row) != Some(media.media_id) {
            return Err(ArrError::StateChanged(format!(
                "{svc} identifies a requested file as a different media"
            )));
        }
        if self.svc() == ArrService::Sonarr {
            let got: BTreeSet<i64> = row.episodes.iter().map(|e| e.id).collect();
            let want: BTreeSet<i64> = media.episode_ids.iter().copied().collect();
            if got != want {
                return Err(ArrError::StateChanged(format!(
                    "{svc} assigns different episodes to a requested file"
                )));
            }
        }
        Ok(())
    }

    fn file_entry(&self, row: &ManualImportDto, media: &ArrMediaRef) -> Value {
        let mut f = serde_json::Map::new();
        f.insert("path".into(), json!(row.path));
        match self.svc() {
            ArrService::Sonarr => {
                f.insert("seriesId".into(), json!(media.media_id));
                let mut ids = media.episode_ids.clone();
                ids.sort_unstable();
                ids.dedup();
                f.insert("episodeIds".into(), json!(ids));
            }
            ArrService::Radarr => {
                f.insert("movieId".into(), json!(media.media_id));
            }
        }
        for (k, v) in [
            ("quality", row.quality.clone()),
            ("languages", row.languages.clone()),
            ("language", row.language.clone()),
            ("releaseGroup", row.release_group.clone().map(Value::String)),
            ("downloadId", row.download_id.clone().map(Value::String)),
        ] {
            if let Some(v) = v {
                f.insert(k.into(), v);
            }
        }
        Value::Object(f)
    }
}

#[async_trait]
impl ArrCommands for ArrCommander {
    async fn rescan(&self, media: &ArrMediaRef) -> Result<i64, PortError> {
        self.expect_service(media)?;
        Ok(self
            .command(&self.rescan_body(Some(media.media_id)))
            .await?)
    }

    async fn rescan_all(&self) -> Result<i64, PortError> {
        Ok(self.command(&self.rescan_body(None)).await?)
    }

    async fn manual_import(&self, req: &ManualImportRequest) -> Result<i64, PortError> {
        self.require(self.granted.manual_import, Capability::ArrManualImport)?;
        let files = self.import_files(req).await?;
        let body = json!({ "name": "ManualImport", "files": files, "importMode": "copy" });
        Ok(self.command(&body).await?)
    }

    async fn grab(&self, release: &ReleaseRef) -> Result<(), PortError> {
        self.require(self.granted.grab, Capability::ArrGrab)?;
        let guid = self.reader.releases.lookup(&release.guid).ok_or_else(|| {
            ArrError::StateChanged(format!(
                "release is not in a recent {} search; search again before grabbing",
                self.svc().as_str()
            ))
        })?;
        let body = json!({ "guid": guid, "indexerId": release.indexer_id });
        self.send(Method::POST, "/api/v3/release", &[], Some(&body))
            .await?;
        Ok(())
    }

    async fn remove_queue_item_keep_media(&self, id: i64) -> Result<(), PortError> {
        self.require(self.granted.queue_remove, Capability::ArrQueueRemove)?;
        // Literals: the download client keeps the data and the release is not blocklisted.
        let query = [
            ("removeFromClient", "false".to_string()),
            ("blocklist", "false".to_string()),
        ];
        self.send(Method::DELETE, &format!("/api/v3/queue/{id}"), &query, None)
            .await?;
        Ok(())
    }

    async fn set_monitored(&self, media: &ArrMediaRef, monitored: bool) -> Result<(), PortError> {
        self.require(self.granted.set_monitored, Capability::ArrSetMonitored)?;
        self.expect_service(media)?;
        let (path, body) = match (self.svc(), media.episode_ids.is_empty()) {
            (ArrService::Sonarr, false) => (
                "/api/v3/episode/monitor",
                json!({ "episodeIds": media.episode_ids, "monitored": monitored }),
            ),
            (ArrService::Sonarr, true) => (
                "/api/v3/series/editor",
                json!({ "seriesIds": [media.media_id], "monitored": monitored }),
            ),
            (ArrService::Radarr, _) => (
                "/api/v3/movie/editor",
                json!({ "movieIds": [media.media_id], "monitored": monitored }),
            ),
        };
        self.send(Method::PUT, path, &[], Some(&body)).await?;
        Ok(())
    }
}
