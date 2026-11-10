#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_arr::correlate::{
    Correlation, Correlator, ImportActivity, QueueHealth, import_activity, import_in_progress,
    queue_health,
};
use removarr_domain::arr::{ArrService, DecisionSource, MediaKind};
use removarr_domain::error::ErrorClass;
use removarr_domain::path::RelPath;
use removarr_domain::ports::ArrRead;
use removarr_domain::torrent::{MappedPath, TorrentHash};
use removarr_testkit::fake_arr::{
    HASH_AMBIGUOUS, HASH_SEASON, HASH_UNKNOWN, PLANTED_PASSKEY, V10_FOLDER, media_management,
};
use serde_json::json;

const PACK: &str = "Synthetic.Show.S01.1080p.BluRay-SYN";

#[tokio::test]
async fn status_reports_version_and_refuses_the_wrong_application() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    assert_eq!(r.service(), ArrService::Sonarr);
    assert_eq!(r.status().await.unwrap(), "4.0.0.0-synthetic");
    c.fake.with_data(|d| d.app_name = "Radarr".into());
    assert_eq!(r.status().await.unwrap_err().class, ErrorClass::Validation);
}

#[tokio::test]
async fn library_has_one_entry_per_file_including_specials() {
    let c = sonarr(&[]).await;
    let lib = c.reader().library().await.unwrap();
    assert_eq!(lib.len(), 3, "series 2 has no file, E02/E04 have no file");
    let special = lib.iter().find(|e| e.file.file_id == 1000).unwrap();
    assert_eq!(special.identity.kind, MediaKind::Special);
    assert_eq!(special.media.episode_ids, vec![100]);
    assert!(!special.monitored, "unmonitored special");
    let e1 = lib.iter().find(|e| e.file.file_id == 1001).unwrap();
    assert_eq!(e1.identity.kind, MediaKind::Episode);
    assert_eq!(
        (e1.identity.season, e1.identity.episode),
        (Some(1), Some(1))
    );
    assert_eq!(e1.identity.absolute, Some(1));
    assert_eq!(e1.identity.external_ids.get("tvdb").unwrap(), "100001");
    assert!(e1.monitored);
    assert_eq!(e1.file.quality, "WEBDL-1080p");
    assert_eq!(e1.file.custom_formats, vec!["CF-Alpha".to_string()]);
    assert_eq!(e1.file.cf_score, 10);
    assert!(e1.file.cutoff_not_met);
    assert_eq!(e1.file.languages, vec!["English".to_string()]);
    match &e1.file.path {
        MappedPath::Mapped { path, .. } => {
            assert_eq!(path.root, c.media_root());
            assert_eq!(
                path.rel,
                RelPath::new("Synthetic Show/Season 01/Synthetic Show - S01E01.mkv").unwrap()
            );
        }
        other => panic!("expected a mapped path, got {other:?}"),
    }
    assert_eq!(c.fake.mutating_count(), 0);
}

#[tokio::test]
async fn episodes_include_missing_files_specials_scene_numbers_and_air_state() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let all = r.episodes(1, None).await.unwrap();
    let ids: Vec<i64> = all.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![100, 101, 102, 103, 104]);
    let e2 = all.iter().find(|e| e.id == 102).unwrap();
    assert!(!e2.has_file && e2.file_id.is_none() && e2.aired && e2.monitored);
    let e3 = all.iter().find(|e| e.id == 103).unwrap();
    assert_eq!(e3.absolute, Some(3));
    assert_eq!(
        (e3.scene_absolute, e3.scene_season, e3.scene_episode),
        (Some(4), Some(2), Some(1))
    );
    assert_eq!(e3.file_id, Some(1003));
    let e4 = all.iter().find(|e| e.id == 104).unwrap();
    assert!(!e4.aired, "airs in 2099");
    let special = all.iter().find(|e| e.id == 100).unwrap();
    assert_eq!(special.season, 0);
    let s1 = r.episodes(1, Some(1)).await.unwrap();
    assert_eq!(s1.len(), 4);
    let rec = c.fake.recorded();
    assert_eq!(rec.last().unwrap().query.get("seasonNumber").unwrap(), "1");
}

#[tokio::test]
async fn episodes_is_sonarr_only() {
    let c = radarr(&[]).await;
    let e = c.reader().episodes(1, None).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Validation);
    assert!(c.fake.recorded().is_empty());
}

#[tokio::test]
async fn queue_paginates_includes_unknown_items_and_scrubs_messages() {
    let c = sonarr(&[]).await;
    c.fake.with_data(|d| {
        for i in 0..300 {
            d.queue.push(json!({ "id": 1000 + i, "seriesId": 2, "episodeId": 201,
                "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading" }));
        }
        d.queue.push(json!({ "id": 9999, "trackedDownloadStatus": "warning",
            "trackedDownloadState": "importBlocked",
            "statusMessages": [{ "title": "t", "messages": [
                format!("Import failed for http://u:pw@host/x?apikey={}", removarr_testkit::fake_arr::FAKE_API_KEY)
            ] }] }));
    });
    let q = c.reader().queue().await.unwrap();
    assert_eq!(q.len(), 306);
    assert_eq!(c.fake.count("GET", "/api/v3/queue"), 2);
    let first = c
        .fake
        .recorded()
        .into_iter()
        .find(|r| r.path == "/api/v3/queue")
        .unwrap();
    assert_eq!(
        first.query.get("includeUnknownSeriesItems").unwrap(),
        "true"
    );
    let item1 = q.iter().find(|i| i.id == 1).unwrap();
    assert_eq!(
        item1.download_id,
        Some(TorrentHash::new(HASH_SEASON.to_ascii_lowercase()).unwrap())
    );
    assert_eq!(item1.media, Some(sonarr_ref(1, &[101])));
    assert_eq!(item1.status, "warning");
    assert_eq!(item1.tracked_state, "importPending");
    assert!(
        item1
            .messages
            .iter()
            .any(|m| m.contains("eligible for import"))
    );
    let echoed = q.iter().find(|i| i.id == 9999).unwrap();
    let joined = echoed.messages.join(" ");
    assert!(
        !joined.contains(removarr_testkit::fake_arr::FAKE_API_KEY),
        "{joined}"
    );
    assert!(!joined.contains("pw@"), "{joined}");
}

#[tokio::test]
async fn queue_correlation_merges_detects_ambiguity_and_unknown_items() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let q = r.queue().await.unwrap();
    let mut cor = Correlator::new();
    cor.add_queue(&q);
    let h = |s: &str| TorrentHash::new(s).unwrap();
    assert_eq!(
        cor.by_hash(&h(HASH_SEASON)),
        Correlation::Unique(sonarr_ref(1, &[101, 103]))
    );
    match cor.by_hash(&h(HASH_AMBIGUOUS)) {
        Correlation::Ambiguous(refs) => {
            let ids: Vec<i64> = refs.iter().map(|m| m.media_id).collect();
            assert_eq!(ids, vec![1, 2]);
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
    assert_eq!(cor.by_hash(&h(HASH_UNKNOWN)), Correlation::Unidentified);

    // Paths: ARR's current files resolve to their media; unknown paths are not found.
    let lib = r.library().await.unwrap();
    cor.add_library(&lib);
    let e1_path = lib.iter().find(|e| e.file.file_id == 1001).unwrap();
    let e1_path = e1_path.file.path.as_rooted().unwrap().clone();
    assert_eq!(
        cor.by_path(&e1_path),
        Correlation::Unique(sonarr_ref(1, &[101]))
    );
    assert_eq!(
        cor.by_path(&completed("nothing.mkv")),
        Correlation::NotFound
    );
    // A content unit whose hash and path agree stays unique; a conflicting hash does not.
    assert!(
        cor.for_content([&h(HASH_SEASON)], [&e1_path])
            .unique()
            .is_some()
    );
    assert!(
        cor.for_content([&h(HASH_AMBIGUOUS)], [&e1_path])
            .unique()
            .is_none()
    );

    // Queue classification: warnings are health signals, never hardlink verdicts.
    let item = |id| q.iter().find(|i| i.id == id).unwrap();
    assert_eq!(import_activity(item(1)), ImportActivity::Active);
    assert!(import_in_progress(item(1)));
    assert_eq!(queue_health(item(1)), QueueHealth::Warning);
    assert_eq!(import_activity(item(5)), ImportActivity::Blocked);
    assert_eq!(queue_health(item(5)), QueueHealth::Error);
    assert!(!import_in_progress(item(3)));
}

#[tokio::test]
async fn history_sends_upper_case_id_and_never_reads_data() {
    let c = sonarr(&[]).await;
    let hash = TorrentHash::new(HASH_SEASON.to_ascii_lowercase()).unwrap();
    let h = c.reader().history_for_download(&hash).await.unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].event_type, "grabbed");
    assert_eq!(h[0].media, Some(sonarr_ref(1, &[101])));
    assert_eq!(h[0].download_id.as_ref(), Some(&hash));
    assert_eq!(h[0].date.as_millis(), 1_714_558_830_123);
    let rec = c.fake.recorded();
    assert_eq!(rec[0].query.get("downloadId").unwrap(), HASH_SEASON);
    assert!(!format!("{h:?}").contains(PLANTED_PASSKEY));

    let mut cor = Correlator::new();
    cor.add_history(&h);
    assert_eq!(
        cor.by_hash(&hash),
        Correlation::Unique(sonarr_ref(1, &[101]))
    );
}

#[tokio::test]
async fn media_management_settings_are_parsed_pessimistically() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let mm = r.media_management().await.unwrap();
    assert!(mm.hardlinks_enabled);
    assert_eq!(mm.recycle_bin_cleanup_days, Some(7));
    assert!(matches!(mm.recycle_bin, Some(MappedPath::NoRule { ref raw }) if raw == "/recycle"));

    c.fake
        .with_data(|d| d.media_management = media_management(Some("/tv/.recycle"), 0, false));
    let mm = r.media_management().await.unwrap();
    assert!(!mm.hardlinks_enabled);
    assert_eq!(mm.recycle_bin_cleanup_days, Some(0));
    let bin = mm.recycle_bin.unwrap();
    assert_eq!(bin.as_rooted().unwrap().root, c.media_root());

    c.fake
        .with_data(|d| d.media_management = json!({ "recycleBin": "" }));
    let mm = r.media_management().await.unwrap();
    assert!(mm.recycle_bin.is_none());
    assert!(
        !mm.hardlinks_enabled,
        "a missing setting counts as disabled"
    );
}

#[tokio::test]
async fn manual_import_preview_captures_arr_decisions_without_importing() {
    let c = sonarr(&[]).await;
    c.fake.with_data(|d| {
        d.manual_import.get_mut(V10_FOLDER).unwrap().push(json!({
            "path": "/elsewhere/stray.mkv", "size": 1, "series": { "id": 1 },
            "episodes": [{ "id": 104 }], "rejections": []
        }));
    });
    let r = c.reader();
    let d = r
        .manual_import_preview(&completed(PACK), Some(&sonarr_ref(1, &[])))
        .await
        .unwrap();
    assert_eq!(d.len(), 4);
    let rec = c.fake.recorded();
    assert_eq!(rec[0].query.get("folder").unwrap(), V10_FOLDER);
    assert_eq!(rec[0].query.get("seriesId").unwrap(), "1");
    assert_eq!(c.fake.mutating_count(), 0);

    let e1 = d
        .iter()
        .find(|x| {
            x.subject == format!("completed:{PACK}/Synthetic.Show.S01E01.1080p.BluRay-SYN.mkv")
        })
        .unwrap();
    assert!(e1.approved && e1.rejections.is_empty());
    assert_eq!(e1.episode_ids, vec![101]);
    assert_eq!(e1.quality, "Bluray-1080p");
    assert_eq!(e1.custom_formats, vec!["CF-Beta".to_string()]);
    assert_eq!(e1.cf_score, 50);
    assert_eq!(e1.source, DecisionSource::ManualImportPreview);

    let e3 = d.iter().find(|x| x.episode_ids == vec![103]).unwrap();
    assert!(!e3.approved);
    assert_eq!(
        e3.rejections,
        vec!["Not an upgrade for existing episode file(s)".to_string()]
    );

    let stray = d
        .iter()
        .find(|x| x.subject == "/elsewhere/stray.mkv")
        .unwrap();
    assert!(!stray.approved, "an unmapped path is never approved");
    assert!(stray.rejections.iter().any(|r| r.starts_with("removarr: ")));
}

#[tokio::test]
async fn unidentified_preview_rows_are_never_approved() {
    let c = sonarr(&[]).await;
    c.fake.with_data(|d| {
        d.manual_import.insert(
            "/downloads/unknown".into(),
            vec![json!({ "path": "/downloads/unknown/a.mkv", "size": 1, "rejections": [] })],
        );
    });
    let d = c
        .reader()
        .manual_import_preview(&completed("unknown"), None)
        .await
        .unwrap();
    assert_eq!(d.len(), 1);
    assert!(!d[0].approved);
    assert_eq!(
        d[0].rejections.len(),
        2,
        "no media and no episode: {:?}",
        d[0].rejections
    );
}
