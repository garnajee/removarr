//! Read adapter: version, torrents, files, snapshots.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use removarr_config::Config;
use removarr_domain::error::PortError;
use removarr_domain::ports::QbitRead;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::http::Inner;
use crate::parse::{files_mapping_uncertain, parse_files};
use crate::settings::QbitSettings;

/// Read-only qBittorrent adapter. Cheap to clone; clones share the session.
#[derive(Debug, Clone)]
pub struct QbitReader {
    pub(crate) inner: Arc<Inner>,
}

impl QbitReader {
    pub fn new(settings: QbitSettings) -> Result<Self, PortError> {
        Ok(Self {
            inner: Arc::new(Inner::new(settings)?),
        })
    }

    /// `None` when the service is absent from the configuration (not configured is not an error).
    #[must_use]
    pub fn from_config(cfg: &Config) -> Option<Result<Self, PortError>> {
        QbitSettings::from_config(cfg).map(Self::new)
    }

    /// Torrents whose path mapping is uncertain (`NoRule` or `Invalid` on any path) as of the last
    /// listing or file query. Mutations on them are refused by the controller.
    #[must_use]
    pub fn mapping_uncertain(&self, hash: &TorrentHash) -> bool {
        self.inner.mapping_uncertain(hash)
    }

    /// One listing, then `files` for every torrent with bounded concurrency, taken against the
    /// same listing so paths and files are consistent. Cancel-safe: dropping the future aborts
    /// the in-flight requests.
    pub async fn snapshot(&self, taken_at: Timestamp) -> Result<TorrentSnapshot, PortError> {
        let version = self.inner.fetch_version().await?;
        let torrents = self.inner.list_torrents().await?;
        let sem = Arc::new(Semaphore::new(self.inner.concurrency));
        let mut set: JoinSet<Result<(TorrentHash, Vec<TorrentFile>), PortError>> = JoinSet::new();
        for t in &torrents {
            let inner = Arc::clone(&self.inner);
            let sem = Arc::clone(&sem);
            let info = t.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.ok();
                let files = files_of(&inner, &info).await?;
                Ok((info.hash, files))
            });
        }
        let mut files = BTreeMap::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok((h, f))) => {
                    files.insert(h, f);
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(PortError::new(
                        removarr_domain::error::ErrorClass::Internal,
                        "qbittorrent file listing task failed",
                    ));
                }
            }
        }
        Ok(TorrentSnapshot {
            taken_at,
            version: version.raw,
            major: version.major,
            torrents,
            files,
        })
    }
}

async fn files_of(inner: &Inner, info: &TorrentInfo) -> Result<Vec<TorrentFile>, PortError> {
    let body = inner
        .get("/api/v2/torrents/files", &[("hash", info.hash.as_str())])
        .await?;
    let files = parse_files(&body, info, &inner.paths)?;
    inner.set_file_uncertain(&info.hash, files_mapping_uncertain(&files));
    Ok(files)
}

#[async_trait]
impl QbitRead for QbitReader {
    /// Logs in if needed and always re-reads the server version (cached for mutations).
    async fn version(&self) -> Result<String, PortError> {
        Ok(self.inner.fetch_version().await?.raw)
    }

    async fn torrents(&self) -> Result<Vec<TorrentInfo>, PortError> {
        self.inner.list_torrents().await
    }

    /// Fetches the torrent's own record first so candidates are built from current paths.
    async fn files(&self, hash: &TorrentHash) -> Result<Vec<TorrentFile>, PortError> {
        let info = self.inner.one_torrent(hash).await?;
        files_of(&self.inner, &info).await
    }
}
