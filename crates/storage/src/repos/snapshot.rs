//! `SnapshotStore`: current torrent snapshot (upsert-if-changed), ARR snapshots, service status.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use removarr_domain::arr::ArrQualitySnapshot;
use removarr_domain::canonical::sha256_hex;
use removarr_domain::error::PortError;
use removarr_domain::ids::SessionId;
use removarr_domain::ports::SnapshotStore;
use removarr_domain::session::ServiceStatus;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{TorrentFile, TorrentHash, TorrentInfo, TorrentSnapshot};

use crate::codec::{col, dec, enc, enum_text, to_i64, uuid_text};
use crate::db::Db;
use crate::error::{integrity, invalid, map_sqlx};
use crate::repos::scan::save_services;

#[derive(Clone, Debug)]
pub struct SnapshotRepo {
    db: Db,
}

impl SnapshotRepo {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Latest stored snapshot for one media item, by capture time.
    pub async fn latest_arr(
        &self,
        media_key: &str,
    ) -> Result<Option<ArrQualitySnapshot>, PortError> {
        let row = sqlx::query(
            "SELECT body FROM arr_snapshots WHERE media_key = ? ORDER BY captured_at DESC, id DESC LIMIT 1",
        )
        .bind(media_key)
        .fetch_optional(self.db.reader())
        .await
        .map_err(map_sqlx)?;
        row.map(|r| dec(&col::<String>(&r, "body")?)).transpose()
    }
}

/// Stable key of the media an ARR snapshot describes.
#[must_use]
pub fn arr_media_key(s: &ArrQualitySnapshot) -> String {
    match &s.media.arr {
        Some(r) => format!("{}:{}:{:?}", r.service.as_str(), r.media_id, r.episode_ids),
        None => {
            let h = enc(&s.media)
                .map(|j| sha256_hex(j.as_bytes()))
                .unwrap_or_default();
            format!("unmatched:{}", &h[..h.len().min(32)])
        }
    }
}

#[async_trait]
impl SnapshotStore for SnapshotRepo {
    async fn save_torrents(
        &self,
        session: SessionId,
        s: &TorrentSnapshot,
    ) -> Result<(), PortError> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut incoming: Vec<(&TorrentInfo, String, String)> =
            Vec::with_capacity(s.torrents.len());
        for t in &s.torrents {
            if !seen.insert(t.hash.as_str()) {
                return Err(invalid("snapshot lists the same torrent twice"));
            }
            let files: &[TorrentFile] = s.files.get(&t.hash).map_or(&[], Vec::as_slice);
            incoming.push((t, enc(t)?, enc(files)?));
        }
        let mut tx = self.db.write_tx().await?;
        let rows = sqlx::query("SELECT hash, info, files FROM torrents")
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let mut existing: HashMap<String, (String, String)> = HashMap::with_capacity(rows.len());
        for r in &rows {
            existing.insert(col(r, "hash")?, (col(r, "info")?, col(r, "files")?));
        }
        for (t, info, files) in &incoming {
            if existing
                .get(t.hash.as_str())
                .is_some_and(|(i, f)| i == info && f == files)
            {
                continue;
            }
            sqlx::query(
                "INSERT INTO torrents (hash, name, state, category, size, info, files) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (hash) DO UPDATE SET name = excluded.name, state = excluded.state, \
                   category = excluded.category, size = excluded.size, info = excluded.info, \
                   files = excluded.files",
            )
            .bind(t.hash.as_str())
            .bind(&t.name)
            .bind(enum_text(&t.state)?)
            .bind(&t.category)
            .bind(to_i64(t.size))
            .bind(info)
            .bind(files)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }
        for hash in existing.keys().filter(|h| !seen.contains(h.as_str())) {
            sqlx::query("DELETE FROM torrents WHERE hash = ?")
                .bind(hash)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        }
        sqlx::query(
            "INSERT INTO torrent_snapshot (id, session_id, taken_at, version, major) \
             VALUES (1, ?, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET session_id = excluded.session_id, \
               taken_at = excluded.taken_at, version = excluded.version, major = excluded.major",
        )
        .bind(uuid_text(session.0))
        .bind(s.taken_at.0)
        .bind(&s.version)
        .bind(i64::from(s.major))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn latest_torrents(&self) -> Result<Option<TorrentSnapshot>, PortError> {
        let mut tx = self.db.read_tx().await?;
        let Some(meta) =
            sqlx::query("SELECT taken_at, version, major FROM torrent_snapshot WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
        else {
            return Ok(None);
        };
        let rows = sqlx::query("SELECT hash, info, files FROM torrents ORDER BY hash")
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let mut torrents = Vec::with_capacity(rows.len());
        let mut files: BTreeMap<TorrentHash, Vec<TorrentFile>> = BTreeMap::new();
        for r in &rows {
            let info: TorrentInfo = dec(&col::<String>(r, "info")?)?;
            let f: Vec<TorrentFile> = dec(&col::<String>(r, "files")?)?;
            if !f.is_empty() {
                files.insert(info.hash.clone(), f);
            }
            torrents.push(info);
        }
        let major = u8::try_from(col::<i64>(&meta, "major")?)
            .map_err(|_| integrity("stored major version out of range"))?;
        Ok(Some(TorrentSnapshot {
            taken_at: Timestamp(col(&meta, "taken_at")?),
            version: col(&meta, "version")?,
            major,
            torrents,
            files,
        }))
    }

    async fn save_arr(&self, s: &ArrQualitySnapshot) -> Result<(), PortError> {
        if !s.fingerprint_is_valid() {
            return Err(invalid(
                "ARR snapshot fingerprint does not match its content",
            ));
        }
        let body = enc(s)?;
        let mut tx = self.db.write_tx().await?;
        sqlx::query(
            "INSERT INTO arr_snapshots (service, media_key, fingerprint, captured_at, body) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (media_key, fingerprint) DO UPDATE SET captured_at = excluded.captured_at \
             WHERE excluded.captured_at > arr_snapshots.captured_at",
        )
        .bind(s.service.as_str())
        .bind(arr_media_key(s))
        .bind(&s.fingerprint)
        .bind(s.captured_at.0)
        .bind(body)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)
    }

    async fn save_service_status(
        &self,
        session: SessionId,
        s: &[ServiceStatus],
    ) -> Result<(), PortError> {
        let mut tx = self.db.write_tx().await?;
        save_services(&mut tx, &uuid_text(session.0), s).await?;
        tx.commit().await.map_err(map_sqlx)
    }
}
