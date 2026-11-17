//! Explicit refresh commands. Separate type from the reader; the constructor needs the
//! `LiveMode` token, so a dry-run process cannot build one.
//!
//! Both commands only ask Jellyfin to rescan. They never rename, move or delete library
//! files, and the metadata refresh mode is the literal `Default` with no replace flags.
//! Not retried (mutating calls are never retried automatically).

use async_trait::async_trait;
use removarr_config::{Config, LiveMode, Secret};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ports::JellyfinCommands;
use url::Url;

use crate::http::{Core, HttpSettings};

#[derive(Debug, Clone)]
pub struct JellyfinCommander {
    core: Core,
}

/// Item ids are 32 hex digits (or a dashed GUID). Anything that could alter the URL path or
/// query is rejected before a request is made.
fn valid_item_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl JellyfinCommander {
    pub fn new(
        url: &Url,
        api_key: &Secret,
        settings: HttpSettings,
        _live: &LiveMode,
    ) -> Result<Self, PortError> {
        Ok(Self {
            core: Core::new(url, api_key, settings)?,
        })
    }

    /// `None` when Jellyfin is not configured.
    pub fn from_config(cfg: &Config, live: &LiveMode) -> Option<Result<Self, PortError>> {
        let svc = cfg.services.jellyfin.as_ref()?;
        let settings = HttpSettings {
            max_response_bytes: cfg.limits.max_response_bytes,
            ..HttpSettings::default()
        };
        Some(Self::new(&svc.url, &svc.api_key, settings, live))
    }
}

#[async_trait]
impl JellyfinCommands for JellyfinCommander {
    /// `POST /Library/Refresh`: accepted with 204. Jellyfin scans asynchronously.
    async fn refresh_library(&self) -> Result<(), PortError> {
        self.core
            .post_accepted("POST /Library/Refresh", &["Library", "Refresh"], &[])
            .await
    }

    /// `POST /Items/{id}/Refresh`: accepted with 204. No status endpoint exists, so callers
    /// verify with `items_by_paths` rather than polling.
    async fn refresh_item(&self, id: &str) -> Result<(), PortError> {
        if !valid_item_id(id) {
            return Err(PortError::new(
                ErrorClass::Validation,
                "jellyfin item id has an invalid format",
            ));
        }
        self.core
            .post_accepted(
                "POST /Items/{id}/Refresh",
                &["Items", id, "Refresh"],
                &[
                    ("Recursive", "true"),
                    ("MetadataRefreshMode", "Default"),
                    ("ImageRefreshMode", "Default"),
                    ("ReplaceAllMetadata", "false"),
                    ("ReplaceAllImages", "false"),
                ],
            )
            .await
    }
}
