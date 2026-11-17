//! Pass 4 parity: reads never mutate, an ARR outage never reads as "unchanged", ARR remains the
//! sole quality authority, and the copy literal is the only import mode that can reach the wire.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_arr::snapshot::{SnapshotValidity, revalidate};
use removarr_domain::arr::{
    ManualImportRequest, ReleaseRef, ReleaseSearchResult, sort_search_results,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ports::{ArrCommands, ArrRead};
use removarr_domain::torrent::TorrentHash;
use removarr_testkit::fake_arr::{FAKE_API_KEY, Fault, HASH_SEASON};

const PACK: &str = "Synthetic.Show.S01.1080p.BluRay-SYN";
const E1: &str = "Synthetic.Show.S01E01.1080p.BluRay-SYN.mkv";

fn assert_no_key(e: &PortError) {
    let text = format!("{e} {e:?}");
    assert!(!text.contains(FAKE_API_KEY), "{text}");
}

/// Every read method, with every mutating capability granted, issues GET requests only.
#[tokio::test]
async fn reads_send_only_get_even_when_every_capability_is_granted() {
    let c = sonarr(ALL_CAPS).await;
    let r = c.reader();
    let hash = TorrentHash::new(HASH_SEASON.to_ascii_lowercase()).unwrap();
    r.status().await.unwrap();
    r.library().await.unwrap();
    r.queue().await.unwrap();
    r.history_for_download(&hash).await.unwrap();
    r.episodes(1, None).await.unwrap();
    r.quality_snapshot(&sonarr_ref(1, &[101])).await.unwrap();
    r.manual_import_preview(&completed(PACK), Some(&sonarr_ref(1, &[])))
        .await
        .unwrap();
    r.search_releases(&sonarr_ref(1, &[101])).await.unwrap();
    r.media_management().await.unwrap();
    assert!(!c.fake.recorded().is_empty());
    assert!(c.fake.recorded().iter().all(|x| x.method == "GET"));
    assert_eq!(c.fake.mutating_count(), 0);
    assert!(c.fake.grabs().is_empty());
    assert!(c.fake.commands().is_empty());
    assert!(c.fake.violations().is_empty());

    let c = radarr(ALL_CAPS).await;
    let r = c.reader();
    r.library().await.unwrap();
    r.queue().await.unwrap();
    r.search_releases(&radarr_ref(7)).await.unwrap();
    r.quality_snapshot(&radarr_ref(7)).await.unwrap();
    assert_eq!(c.fake.mutating_count(), 0);
    assert!(c.fake.grabs().is_empty());
}

/// A search followed by nothing never turns into a grab, even with the grab capability on.
#[tokio::test]
async fn search_with_the_grab_capability_still_never_grabs() {
    let c = sonarr(ALL_CAPS).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    let res = r.search_releases(&sonarr_ref(1, &[101])).await.unwrap();
    assert!(res.iter().any(|x| x.approved));
    assert!(c.fake.grabs().is_empty());
    assert_eq!(c.fake.count("POST", "/api/v3/release"), 0);
    // A release never returned by a search cannot be grabbed either.
    let unknown = ReleaseRef {
        guid: removarr_arr::release_token(1, "never-searched"),
        indexer_id: 1,
    };
    assert!(cmd.grab(&unknown).await.is_err());
    assert!(c.fake.grabs().is_empty());
}

/// An outage on any read is an error with no partial data, and the key stays out of it.
#[tokio::test]
async fn every_read_fails_closed_during_an_outage_without_leaking_the_key() {
    for status in [500u16, 502, 503] {
        let c = sonarr(&[]).await;
        let r = c.reader();
        let hash = TorrentHash::new(HASH_SEASON.to_ascii_lowercase()).unwrap();
        c.fake.set_fault(
            "*",
            Fault {
                status: Some(status),
                ..Fault::default()
            },
        );
        let errs = [
            r.status().await.err(),
            r.library().await.err(),
            r.queue().await.err(),
            r.history_for_download(&hash).await.err(),
            r.quality_snapshot(&sonarr_ref(1, &[101])).await.err(),
            r.manual_import_preview(&completed(PACK), None).await.err(),
            r.episodes(1, None).await.err(),
            r.search_releases(&sonarr_ref(1, &[101])).await.err(),
            r.media_management().await.err(),
            r.command_status(1).await.err(),
        ];
        for e in errs {
            let e = e.unwrap_or_else(|| panic!("a read succeeded during an outage ({status})"));
            assert_eq!(e.class, ErrorClass::Unavailable, "status {status}");
            assert_no_key(&e);
        }
    }
}

/// Hostile 200 bodies and timeouts are never "unchanged" for a stored snapshot.
#[tokio::test]
async fn malformed_or_empty_answers_never_revalidate_a_snapshot() {
    let c = radarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&radarr_ref(7)).await.unwrap();
    assert!(revalidate(&r, &stored).await.is_unchanged());
    for body in ["", "null", "[]", "{}", "<html>login</html>", "not json"] {
        c.fake.set_fault(
            "*",
            Fault {
                raw_body: Some(body.into()),
                ..Fault::default()
            },
        );
        let v = revalidate(&r, &stored).await;
        assert!(
            !v.is_unchanged(),
            "body {body:?} must not read as unchanged"
        );
        assert!(
            matches!(
                v,
                SnapshotValidity::Unavailable(_) | SnapshotValidity::Changed(_)
            ),
            "body {body:?}"
        );
    }
    c.fake.clear_faults();
    assert!(revalidate(&r, &stored).await.is_unchanged(), "control");
}

/// Ordering is presentation only and uses ARR's verdict and score: resolution, source, codec or
/// group words in the quality label or title never reorder results.
#[tokio::test]
async fn presentation_order_ignores_quality_labels_and_release_words() {
    let c = sonarr(&[]).await;
    let base = c
        .reader()
        .search_releases(&sonarr_ref(1, &[101]))
        .await
        .unwrap();
    let mut a = base.clone();
    sort_search_results(&mut a, 5_000_000);
    let order = |v: &[ReleaseSearchResult]| v.iter().map(|x| x.release.clone()).collect::<Vec<_>>();
    for label in ["Remux-2160p", "DVD", "CAM", "HDTV-480p", ""] {
        let mut b = base.clone();
        for x in &mut b {
            x.quality = label.into();
            x.custom_formats = vec![format!("{label} x265 HDR")];
            x.indexer = format!("{label}-indexer");
            x.protocol = "usenet".into();
        }
        sort_search_results(&mut b, 5_000_000);
        assert_eq!(order(&a), order(&b), "label {label:?} changed the order");
    }
    // Approval and score dominate: a rejected high-score release never outranks an approved one.
    let mut m = base;
    m.iter_mut().for_each(|x| x.cf_score = 0);
    m[0].approved = false;
    m[0].cf_score = 10_000;
    m[1].approved = true;
    sort_search_results(&mut m, 1);
    assert!(m[0].approved);
}

/// All ManualImport commands that ever reach the wire carry the literal `copy`.
#[tokio::test]
async fn every_command_body_on_the_wire_uses_import_mode_copy() {
    let c = sonarr(ALL_CAPS).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    c.fake.set_import_destination(
        &format!("{}/{E1}", removarr_testkit::fake_arr::V10_FOLDER),
        "/tv/Synthetic Show/Season 01/Synthetic Show - S01E01.mkv",
    );
    let req = ManualImportRequest {
        files: vec![(completed(&format!("{PACK}/{E1}")), sonarr_ref(1, &[101]))],
    };
    cmd.manual_import(&req).await.unwrap();
    cmd.rescan(&sonarr_ref(1, &[101])).await.unwrap();
    cmd.rescan_all().await.unwrap();
    let posts: Vec<_> = c
        .fake
        .recorded()
        .into_iter()
        .filter(|x| x.method == "POST" && x.path == "/api/v3/command")
        .collect();
    assert_eq!(posts.len(), 3);
    for p in &posts {
        let body = p.body.as_ref().unwrap();
        if body["name"] == "ManualImport" {
            assert_eq!(body["importMode"], "copy");
        } else {
            assert!(body.get("importMode").is_none(), "{body}");
        }
        let raw = body.to_string().to_lowercase();
        assert!(
            !raw.contains("\"move\"") && !raw.contains("hardlink\""),
            "{raw}"
        );
    }
    assert!(c.fake.violations().is_empty());
}
