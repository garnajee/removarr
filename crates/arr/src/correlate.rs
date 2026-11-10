//! Correlation of download hashes and media paths to ARR media identities, and queue item
//! classification. Pure functions over domain types (no I/O), so `issue-engine` can use the
//! same rules on stored data.
//!
//! Rules:
//! - several records naming the same media merge (their episode ids are united);
//! - records naming different media, or a mix of identified and unidentified records, are
//!   `Ambiguous` and never resolve to one media (fail-closed);
//! - a queue warning or error says nothing about hardlinks (INV-39): the queue classification
//!   below never feeds a hardlink verdict.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::{
    ArrHistoryRecord, ArrLibraryEntry, ArrMediaRef, ArrQueueItem, ArrService,
};
use removarr_domain::path::RootedPath;
use removarr_domain::torrent::{MappedPath, TorrentHash};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Correlation {
    /// ARR has no record for this hash or path.
    NotFound,
    /// Every record names this one media.
    Unique(ArrMediaRef),
    /// Records name several media, or mix identified and unidentified items.
    Ambiguous(Vec<ArrMediaRef>),
    /// ARR tracks the download but did not identify any media (unknown queue item).
    Unidentified,
}

impl Correlation {
    #[must_use]
    pub fn unique(&self) -> Option<&ArrMediaRef> {
        match self {
            Correlation::Unique(m) => Some(m),
            _ => None,
        }
    }
}

/// Index of the ARR records seen in one observation. Build it from `queue`, `history` and
/// `library` reads of every configured ARR service.
#[derive(Debug, Clone, Default)]
pub struct Correlator {
    by_hash: BTreeMap<TorrentHash, Vec<Option<ArrMediaRef>>>,
    by_path: BTreeMap<RootedPath, Vec<Option<ArrMediaRef>>>,
}

fn resolve(records: Option<&Vec<Option<ArrMediaRef>>>) -> Correlation {
    let Some(records) = records.filter(|r| !r.is_empty()) else {
        return Correlation::NotFound;
    };
    let unidentified = records.iter().any(Option::is_none);
    let mut merged: BTreeMap<(ArrService, i64), BTreeSet<i64>> = BTreeMap::new();
    for m in records.iter().flatten() {
        merged
            .entry((m.service, m.media_id))
            .or_default()
            .extend(m.episode_ids.iter().copied());
    }
    let refs: Vec<ArrMediaRef> = merged
        .into_iter()
        .map(|((service, media_id), eps)| ArrMediaRef {
            service,
            media_id,
            episode_ids: eps.into_iter().collect(),
        })
        .collect();
    match (refs.len(), unidentified) {
        (0, _) => Correlation::Unidentified,
        (1, false) => refs
            .into_iter()
            .next()
            .map_or(Correlation::NotFound, Correlation::Unique),
        _ => Correlation::Ambiguous(refs),
    }
}

impl Correlator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_queue(&mut self, items: &[ArrQueueItem]) {
        for q in items {
            if let Some(h) = &q.download_id {
                self.by_hash
                    .entry(h.clone())
                    .or_default()
                    .push(q.media.clone());
            }
        }
    }

    pub fn add_history(&mut self, records: &[ArrHistoryRecord]) {
        for r in records {
            if let Some(h) = &r.download_id {
                self.by_hash
                    .entry(h.clone())
                    .or_default()
                    .push(r.media.clone());
            }
        }
    }

    /// Indexes ARR's current files by their mapped path. Unmapped paths are skipped (they
    /// cannot be compared with application paths).
    pub fn add_library(&mut self, entries: &[ArrLibraryEntry]) {
        for e in entries {
            if let MappedPath::Mapped { path, .. } = &e.file.path {
                self.by_path
                    .entry(path.clone())
                    .or_default()
                    .push(Some(e.media.clone()));
            }
        }
    }

    #[must_use]
    pub fn by_hash(&self, hash: &TorrentHash) -> Correlation {
        resolve(self.by_hash.get(hash))
    }

    /// Media whose ARR current file is exactly `path`.
    #[must_use]
    pub fn by_path(&self, path: &RootedPath) -> Correlation {
        resolve(self.by_path.get(path))
    }

    /// One content unit: every hash of the torrents holding it, plus its known paths. All
    /// records found must agree on one media; paths and hashes ARR does not know are ignored.
    #[must_use]
    pub fn for_content<'a>(
        &self,
        hashes: impl IntoIterator<Item = &'a TorrentHash>,
        paths: impl IntoIterator<Item = &'a RootedPath>,
    ) -> Correlation {
        let mut all: Vec<Option<ArrMediaRef>> = Vec::new();
        for h in hashes {
            all.extend(self.by_hash.get(h).into_iter().flatten().cloned());
        }
        for p in paths {
            all.extend(self.by_path.get(p).into_iter().flatten().cloned());
        }
        resolve(Some(&all))
    }
}

/// What a queue item says about an import by ARR (INV-16, blocker `ImportInProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportActivity {
    /// `importPending` or `importing`: ARR is importing this media now.
    Active,
    /// `importBlocked`: ARR waits for a manual decision.
    Blocked,
    /// `downloading`, `imported`, `failedPending`, `failed`, `ignored`.
    Idle,
    /// A state this version does not know. Treated like `Active` by callers.
    Unknown,
}

#[must_use]
pub fn import_activity(item: &ArrQueueItem) -> ImportActivity {
    match item.tracked_state.as_str() {
        "importPending" | "importing" => ImportActivity::Active,
        "importBlocked" => ImportActivity::Blocked,
        "downloading" | "imported" | "failedPending" | "failed" | "ignored" => ImportActivity::Idle,
        _ => ImportActivity::Unknown,
    }
}

/// True when the item may be an import in progress (unknown states count, fail-closed).
#[must_use]
pub fn import_in_progress(item: &ArrQueueItem) -> bool {
    matches!(
        import_activity(item),
        ImportActivity::Active | ImportActivity::Unknown
    )
}

/// ARR's health flag for a queue item (`trackedDownloadStatus`). Informational only: a
/// warning or an error is never evidence that a hardlink is missing (INV-39).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueHealth {
    Ok,
    Warning,
    Error,
    Unknown,
}

#[must_use]
pub fn queue_health(item: &ArrQueueItem) -> QueueHealth {
    match item.status.as_str() {
        "ok" => QueueHealth::Ok,
        "warning" => QueueHealth::Warning,
        "error" => QueueHealth::Error,
        _ => QueueHealth::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> TorrentHash {
        TorrentHash::new(c.to_string().repeat(40)).unwrap()
    }

    fn sonarr(series: i64, eps: &[i64]) -> Option<ArrMediaRef> {
        Some(ArrMediaRef {
            service: ArrService::Sonarr,
            media_id: series,
            episode_ids: eps.to_vec(),
        })
    }

    fn queue(id: i64, hash: char, media: Option<ArrMediaRef>, state: &str) -> ArrQueueItem {
        ArrQueueItem {
            id,
            download_id: Some(h(hash)),
            media,
            status: "warning".into(),
            tracked_state: state.into(),
            messages: vec![],
        }
    }

    #[test]
    fn records_of_one_media_merge_episode_ids() {
        let mut c = Correlator::new();
        c.add_queue(&[
            queue(1, 'a', sonarr(1, &[103]), "importPending"),
            queue(2, 'a', sonarr(1, &[101]), "importPending"),
        ]);
        assert_eq!(
            c.by_hash(&h('a')),
            Correlation::Unique(sonarr(1, &[101, 103]).unwrap())
        );
        assert_eq!(c.by_hash(&h('f')), Correlation::NotFound);
    }

    #[test]
    fn different_media_or_unidentified_mix_is_ambiguous() {
        let mut c = Correlator::new();
        c.add_queue(&[
            queue(1, 'b', sonarr(1, &[102]), "downloading"),
            queue(2, 'b', sonarr(2, &[201]), "downloading"),
            queue(3, 'c', None, "importBlocked"),
            queue(4, 'd', sonarr(1, &[101]), "importBlocked"),
            queue(5, 'd', None, "importBlocked"),
        ]);
        assert!(matches!(c.by_hash(&h('b')), Correlation::Ambiguous(v) if v.len() == 2));
        assert_eq!(c.by_hash(&h('c')), Correlation::Unidentified);
        assert!(matches!(c.by_hash(&h('d')), Correlation::Ambiguous(v) if v.len() == 1));
        assert!(c.by_hash(&h('d')).unique().is_none());
    }

    #[test]
    fn queue_and_history_must_agree() {
        let mut c = Correlator::new();
        c.add_queue(&[queue(1, 'a', sonarr(1, &[101]), "importing")]);
        c.add_history(&[ArrHistoryRecord {
            download_id: Some(h('a')),
            media: sonarr(9, &[901]),
            event_type: "grabbed".into(),
            source_title: "x".into(),
            date: removarr_domain::time::Timestamp(0),
        }]);
        assert!(matches!(c.by_hash(&h('a')), Correlation::Ambiguous(_)));
    }

    #[test]
    fn import_activity_is_fail_closed() {
        let item = |s: &str| queue(1, 'a', None, s);
        assert_eq!(import_activity(&item("importing")), ImportActivity::Active);
        assert_eq!(
            import_activity(&item("importBlocked")),
            ImportActivity::Blocked
        );
        assert_eq!(import_activity(&item("imported")), ImportActivity::Idle);
        assert!(import_in_progress(&item("someFutureState")));
        assert!(import_in_progress(&item("importPending")));
        assert!(!import_in_progress(&item("downloading")));
        assert_eq!(queue_health(&item("x")), QueueHealth::Warning);
    }
}
