#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_arr::correlate::{Correlation, Correlator};
use removarr_domain::arr::{ArrService, MediaKind};
use removarr_domain::ports::ArrRead;
use removarr_domain::torrent::TorrentHash;
use removarr_testkit::fake_arr::HASH_MOVIE;

#[tokio::test]
async fn library_lists_movies_with_files_only() {
    let c = radarr(&[]).await;
    let r = c.reader();
    assert_eq!(r.service(), ArrService::Radarr);
    let lib = r.library().await.unwrap();
    assert_eq!(lib.len(), 1);
    let m = &lib[0];
    assert_eq!(m.media, radarr_ref(7));
    assert_eq!(m.identity.kind, MediaKind::Movie);
    assert_eq!(m.identity.year, Some(2021));
    assert_eq!(m.identity.external_ids.get("imdb").unwrap(), "tt0000007");
    assert_eq!(m.file.file_id, 70);
    assert_eq!(m.file.size, 5_000_000);
    assert_eq!(m.file.path.as_rooted().unwrap().root, c.media_root());
    assert!(m.monitored);
}

#[tokio::test]
async fn queue_history_and_preview_for_a_movie() {
    let c = radarr(&[]).await;
    let r = c.reader();
    let q = r.queue().await.unwrap();
    let first = c
        .fake
        .recorded()
        .into_iter()
        .find(|x| x.path == "/api/v3/queue")
        .unwrap();
    assert_eq!(first.query.get("includeUnknownMovieItems").unwrap(), "true");
    let hash = TorrentHash::new(HASH_MOVIE).unwrap();
    let h = r.history_for_download(&hash).await.unwrap();
    let mut cor = Correlator::new();
    cor.add_queue(&q);
    cor.add_history(&h);
    assert_eq!(cor.by_hash(&hash), Correlation::Unique(radarr_ref(7)));

    let d = r
        .manual_import_preview(
            &completed("Synthetic.Movie.2021.1080p.BluRay-SYN"),
            Some(&radarr_ref(7)),
        )
        .await
        .unwrap();
    assert_eq!(d.len(), 1);
    assert!(d[0].approved, "{:?}", d[0].rejections);
    assert!(d[0].episode_ids.is_empty());
    assert_eq!(c.fake.mutating_count(), 0);
}
