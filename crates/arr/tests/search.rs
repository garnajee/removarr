//! Interactive search returns ARR's verdicts and never grabs (INV-22, INV-38).
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_domain::arr::sort_search_results;
use removarr_domain::error::ErrorClass;
use removarr_domain::ports::ArrRead;
use removarr_testkit::fake_arr::PLANTED_PASSKEY;
use serde_json::json;

#[tokio::test]
async fn sonarr_search_returns_approved_and_rejected_results_without_grabbing() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let res = r.search_releases(&sonarr_ref(1, &[101])).await.unwrap();
    assert_eq!(res.len(), 2);

    let rec = c.fake.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(
        (rec[0].method.as_str(), rec[0].path.as_str()),
        ("GET", "/api/v3/release")
    );
    assert_eq!(rec[0].query.get("episodeId").unwrap(), "101");
    assert_eq!(
        c.fake.mutating_count(),
        0,
        "a search sends no POST/PUT/DELETE"
    );
    assert!(c.fake.grabs().is_empty());
    assert_eq!(c.fake.count("POST", "/api/v3/release"), 0);

    let ok = res.iter().find(|x| x.approved).unwrap();
    assert_eq!(ok.title, "Synthetic.Show.S01E01.1080p.BluRay-SYN");
    assert_eq!(
        (ok.seeders, ok.leechers, ok.age_days),
        (Some(12), Some(3), 4)
    );
    assert_eq!(ok.indexer, "SynthIndexer");
    assert_eq!(ok.protocol, "torrent");
    assert_eq!(ok.quality, "Bluray-1080p");
    assert_eq!(ok.cf_score, 50);
    assert!(ok.download_allowed && ok.rejections.is_empty());

    let bad = res.iter().find(|x| !x.approved).unwrap();
    assert_eq!(bad.rejections.len(), 2);
    assert!(bad.rejections[1].contains("CF-Penalty"));
    assert_eq!(bad.cf_score, -100);

    // Release guids can be tracker URLs with a passkey: results carry an opaque token only.
    for x in &res {
        assert!(x.release.guid.starts_with("rel-"));
    }
    assert!(!format!("{res:?}").contains(PLANTED_PASSKEY));
    assert!(!format!("{res:?}").contains("tracker.invalid"));
}

#[tokio::test]
async fn season_search_targets_one_season_only() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    r.search_releases(&sonarr_ref(1, &[101, 102, 103]))
        .await
        .unwrap();
    let last = c.fake.recorded().pop().unwrap();
    assert_eq!(last.path, "/api/v3/release");
    assert_eq!(last.query.get("seriesId").unwrap(), "1");
    assert_eq!(last.query.get("seasonNumber").unwrap(), "1");

    let e = r
        .search_releases(&sonarr_ref(1, &[100, 101]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Validation, "special + season 1");
    let e = r.search_releases(&sonarr_ref(1, &[])).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Validation);
    assert_eq!(c.fake.count("GET", "/api/v3/release"), 1);
    assert_eq!(c.fake.mutating_count(), 0);
}

#[tokio::test]
async fn radarr_search_and_presentation_order() {
    let c = radarr(&[]).await;
    let mut res = c.reader().search_releases(&radarr_ref(7)).await.unwrap();
    let rec = c.fake.recorded();
    assert_eq!(rec[0].query.get("movieId").unwrap(), "7");
    sort_search_results(&mut res, 5_000_000);
    assert!(res[0].approved);
    assert_eq!(
        res[1].rejections,
        vec!["Quality CAM is not wanted in profile".to_string()]
    );
    assert_eq!(c.fake.mutating_count(), 0);
}

#[tokio::test]
async fn approval_with_a_rejection_is_not_approval() {
    let c = radarr(&[]).await;
    c.fake.with_data(|d| {
        d.releases[0]["rejections"] = json!(["Indexer temporarily disabled"]);
    });
    let res = c.reader().search_releases(&radarr_ref(7)).await.unwrap();
    assert!(res.iter().all(|r| !r.approved));
}

#[tokio::test]
async fn search_for_another_service_is_refused_before_any_request() {
    let c = radarr(&[]).await;
    let e = c
        .reader()
        .search_releases(&sonarr_ref(1, &[101]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Validation);
    assert!(c.fake.recorded().is_empty());
}
