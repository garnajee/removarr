//! Separately authorized mutators: capability gating, literal parameters on the wire, and
//! the V10 fake behaviors (copy, move switch, upgrade retiring the old file).
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::*;
use removarr_domain::arr::{CommandState, ManualImportRequest, ReleaseRef};
use removarr_domain::error::ErrorClass;
use removarr_domain::ports::{ArrCommands, ArrRead};
use removarr_testkit::fake_arr::{FAKE_API_KEY, Fault, FsBridge, ImportEffect, V10_FOLDER};
use serde_json::json;

const PACK: &str = "Synthetic.Show.S01.1080p.BluRay-SYN";
const E1: &str = "Synthetic.Show.S01E01.1080p.BluRay-SYN.mkv";
const E2: &str = "Synthetic.Show.S01E02.1080p.BluRay-SYN.mkv";
const E3: &str = "Synthetic.Show.S01E03.1080p.BluRay-SYN.mkv";

fn import_req(files: &[(&str, &[i64])]) -> ManualImportRequest {
    ManualImportRequest {
        files: files
            .iter()
            .map(|(f, eps)| (completed(&format!("{PACK}/{f}")), sonarr_ref(1, eps)))
            .collect(),
    }
}

#[tokio::test]
async fn every_gated_mutator_is_refused_without_its_capability_and_sends_nothing() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    let class = |e: removarr_domain::error::PortError| e.class;
    assert_eq!(
        class(
            cmd.manual_import(&import_req(&[(E1, &[101])]))
                .await
                .unwrap_err()
        ),
        ErrorClass::CapabilityDisabled
    );
    let rel = ReleaseRef {
        guid: "rel-x".into(),
        indexer_id: 1,
    };
    assert_eq!(
        class(cmd.grab(&rel).await.unwrap_err()),
        ErrorClass::CapabilityDisabled
    );
    assert_eq!(
        class(cmd.remove_queue_item_keep_media(1).await.unwrap_err()),
        ErrorClass::CapabilityDisabled
    );
    assert_eq!(
        class(
            cmd.set_monitored(&sonarr_ref(1, &[101]), false)
                .await
                .unwrap_err()
        ),
        ErrorClass::CapabilityDisabled
    );
    assert!(
        c.fake.recorded().is_empty(),
        "nothing reaches ARR, not even a preview"
    );
}

#[tokio::test]
async fn manual_import_sends_copy_mode_and_arr_objects() {
    let c = sonarr(&["arr_manual_import"]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    c.fake.set_import_destination(
        &format!("{V10_FOLDER}/{E1}"),
        "/tv/Synthetic Show/Season 01/Synthetic Show - S01E01.mkv",
    );
    c.fake.set_import_destination(
        &format!("{V10_FOLDER}/{E2}"),
        "/tv/Synthetic Show/Season 01/Synthetic Show - S01E02.mkv",
    );
    let id = cmd
        .manual_import(&import_req(&[(E1, &[101]), (E2, &[102])]))
        .await
        .unwrap();
    assert!(c.fake.violations().is_empty(), "{:?}", c.fake.violations());
    let body = c.fake.commands().get(&id).cloned().unwrap();
    assert_eq!(body["name"], "ManualImport");
    assert_eq!(body["importMode"], "copy");
    let files = body["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], format!("{V10_FOLDER}/{E1}"));
    assert_eq!(files[0]["seriesId"], 1);
    assert_eq!(files[0]["episodeIds"], json!([101]));
    assert_eq!(files[0]["quality"]["quality"]["name"], "Bluray-1080p");
    assert_eq!(files[0]["releaseGroup"], "SYN");
    assert_eq!(c.fake.count("POST", "/api/v3/command"), 1);
    assert_eq!(r.command_status(id).await.unwrap(), CommandState::Completed);

    // The fake applied the upgrade: E01 now points at the new file.
    let s = r.quality_snapshot(&sonarr_ref(1, &[101])).await.unwrap();
    assert_eq!(s.current_files.len(), 1);
    assert_ne!(s.current_files[0].file_id, 1001);
}

#[tokio::test]
async fn manual_import_refuses_files_arr_no_longer_accepts() {
    let c = sonarr(&["arr_manual_import"]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    // E03 is rejected by ARR ("not an upgrade").
    let e = cmd
        .manual_import(&import_req(&[(E3, &[103])]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    assert!(e.detail.contains("Not an upgrade"), "{}", e.detail);
    // ARR assigns E01 to episode 101, not 102.
    let e = cmd
        .manual_import(&import_req(&[(E1, &[102])]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    // A file ARR no longer lists.
    let e = cmd
        .manual_import(&import_req(&[("gone.mkv", &[101])]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
    // No episode named, and a path outside every mapping.
    let e = cmd
        .manual_import(&import_req(&[(E1, &[])]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::Validation);
    let outside = ManualImportRequest {
        files: vec![(
            removarr_domain::path::RootedPath::new(
                c.media_root(),
                removarr_domain::path::RelPath::new("x.mkv").unwrap(),
            ),
            sonarr_ref(1, &[101]),
        )],
    };
    // Media root paths map too (`/tv`), so ARR is asked and does not list the file.
    assert!(cmd.manual_import(&outside).await.is_err());
    assert_eq!(
        c.fake.count("POST", "/api/v3/command"),
        0,
        "no import was ever sent"
    );
    let empty = ManualImportRequest { files: vec![] };
    assert_eq!(
        cmd.manual_import(&empty).await.unwrap_err().class,
        ErrorClass::Validation
    );
}

#[tokio::test]
async fn fake_refuses_any_import_mode_other_than_copy() {
    let c = sonarr(&[]).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/v3/command", c.fake.base_url()))
        .header("X-Api-Key", FAKE_API_KEY)
        .json(&json!({ "name": "ManualImport", "files": [], "importMode": "move" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(c.fake.violations().len(), 1);
    assert!(c.fake.commands().is_empty());
}

fn bridge(c: &Ctx) -> (std::path::PathBuf, std::path::PathBuf) {
    // Service `/downloads` -> completed root, `/tv` -> media root (same layout as the config).
    let downloads = c.base.join("completed");
    let tv = c.base.join("media1");
    fs::create_dir_all(downloads.join(PACK)).unwrap();
    fs::write(downloads.join(PACK).join(E1), b"new release bytes").unwrap();
    let season = tv.join("Synthetic Show/Season 01");
    fs::create_dir_all(&season).unwrap();
    fs::write(season.join("Synthetic Show - S01E01.mkv"), b"old release").unwrap();
    c.fake.set_fs_bridge(FsBridge {
        service_prefix: "/".into(),
        local_dir: c.base.join("svc"),
    });
    // Expose both trees under one service root so recycle paths resolve too.
    fs::create_dir_all(c.base.join("svc")).unwrap();
    std::os::unix::fs::symlink(&downloads, c.base.join("svc/downloads")).unwrap();
    std::os::unix::fs::symlink(&tv, c.base.join("svc/tv")).unwrap();
    (downloads.join(PACK).join(E1), season)
}

#[tokio::test]
async fn copy_import_hardlinks_and_upgrade_retires_old_file_to_recycle_bin() {
    let c = sonarr(&["arr_manual_import"]).await;
    let (src, season) = bridge(&c);
    c.fake.set_import_destination(
        &format!("{V10_FOLDER}/{E1}"),
        "/tv/Synthetic Show/Season 01/Synthetic Show - S01E01 [new].mkv",
    );
    c.fake
        .with_data(|d| d.media_management["recycleBin"] = json!("/tv/.recycle"));
    let r = c.reader();
    c.commander(&r)
        .manual_import(&import_req(&[(E1, &[101])]))
        .await
        .unwrap();
    assert!(c.fake.violations().is_empty(), "{:?}", c.fake.violations());
    let dest = season.join("Synthetic Show - S01E01 [new].mkv");
    assert_eq!(
        fs::metadata(&src).unwrap().ino(),
        fs::metadata(&dest).unwrap().ino()
    );
    assert!(!season.join("Synthetic Show - S01E01.mkv").exists());
    assert!(
        c.base
            .join("media1/.recycle/Synthetic Show - S01E01.mkv")
            .exists()
    );
}

#[tokio::test]
async fn move_switch_makes_the_source_disappear() {
    let c = sonarr(&["arr_manual_import"]).await;
    let (src, season) = bridge(&c);
    c.fake.set_import_effect(ImportEffect::Move);
    c.fake
        .with_data(|d| d.media_management["recycleBin"] = json!(""));
    c.fake.set_import_destination(
        &format!("{V10_FOLDER}/{E1}"),
        "/tv/Synthetic Show/Season 01/Synthetic Show - S01E01 [new].mkv",
    );
    let r = c.reader();
    c.commander(&r)
        .manual_import(&import_req(&[(E1, &[101])]))
        .await
        .unwrap();
    assert!(
        !src.exists(),
        "the move switch removes the completed source"
    );
    assert!(season.join("Synthetic Show - S01E01 [new].mkv").exists());
    assert!(
        !season.join("Synthetic Show - S01E01.mkv").exists(),
        "no recycle bin: old file gone"
    );
}

#[tokio::test]
async fn grab_needs_capability_and_a_recent_search_and_sends_the_raw_guid() {
    let c = sonarr(&["arr_grab"]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    let token = removarr_arr::release_token(1, "synthetic-guid-approved");
    let rel = ReleaseRef {
        guid: token.clone(),
        indexer_id: 1,
    };
    let e = cmd.grab(&rel).await.unwrap_err();
    assert_eq!(
        e.class,
        ErrorClass::StateChanged,
        "no search in this process yet"
    );
    assert_eq!(c.fake.mutating_count(), 0);

    let results = r.search_releases(&sonarr_ref(1, &[101])).await.unwrap();
    assert!(results.iter().any(|x| x.release == rel));
    assert!(c.fake.grabs().is_empty(), "search alone never grabs");
    cmd.grab(&rel).await.unwrap();
    let grabs = c.fake.grabs();
    assert_eq!(grabs.len(), 1);
    assert_eq!(
        grabs[0],
        json!({ "guid": "synthetic-guid-approved", "indexerId": 1 })
    );
    assert_eq!(c.fake.count("POST", "/api/v3/release"), 1);
}

#[tokio::test]
async fn queue_removal_keeps_data_and_never_blocklists() {
    let c = sonarr(&["arr_queue_remove"]).await;
    let r = c.reader();
    c.commander(&r)
        .remove_queue_item_keep_media(3)
        .await
        .unwrap();
    let rec = c.fake.recorded().pop().unwrap();
    assert_eq!(
        (rec.method.as_str(), rec.path.as_str()),
        ("DELETE", "/api/v3/queue/3")
    );
    assert_eq!(rec.query.get("removeFromClient").unwrap(), "false");
    assert_eq!(rec.query.get("blocklist").unwrap(), "false");
    assert!(c.fake.violations().is_empty());
    assert!(r.queue().await.unwrap().iter().all(|q| q.id != 3));
}

#[tokio::test]
async fn monitoring_changes_use_targeted_endpoints() {
    let c = sonarr(&["arr_set_monitored"]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    cmd.set_monitored(&sonarr_ref(1, &[103]), false)
        .await
        .unwrap();
    let rec = c.fake.recorded().pop().unwrap();
    assert_eq!(
        (rec.method.as_str(), rec.path.as_str()),
        ("PUT", "/api/v3/episode/monitor")
    );
    assert_eq!(
        rec.body.unwrap(),
        json!({ "episodeIds": [103], "monitored": false })
    );
    assert!(
        !r.episodes(1, None)
            .await
            .unwrap()
            .iter()
            .find(|e| e.id == 103)
            .unwrap()
            .monitored
    );
    cmd.set_monitored(&sonarr_ref(2, &[]), false).await.unwrap();
    assert_eq!(c.fake.count("PUT", "/api/v3/series/editor"), 1);

    let m = radarr(&["arr_set_monitored"]).await;
    let mr = m.reader();
    m.commander(&mr)
        .set_monitored(&radarr_ref(8), true)
        .await
        .unwrap();
    let rec = m.fake.recorded().pop().unwrap();
    assert_eq!(rec.path, "/api/v3/movie/editor");
    assert_eq!(
        rec.body.unwrap(),
        json!({ "movieIds": [8], "monitored": true })
    );
}

#[tokio::test]
async fn rescans_need_no_capability_and_report_command_state() {
    let c = radarr(&[]).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    let id = cmd.rescan(&radarr_ref(7)).await.unwrap();
    assert_eq!(
        c.fake.commands()[&id],
        json!({ "name": "RescanMovie", "movieId": 7 })
    );
    let all = cmd.rescan_all().await.unwrap();
    assert_eq!(c.fake.commands()[&all], json!({ "name": "RescanMovie" }));
    c.fake.set_command_status("started");
    assert_eq!(r.command_status(id).await.unwrap(), CommandState::Started);
    c.fake.set_command_status("failed");
    assert!(matches!(
        r.command_status(id).await.unwrap(),
        CommandState::Failed { .. }
    ));
    c.fake.set_command_status("someNewState");
    assert!(matches!(
        r.command_status(id).await.unwrap(),
        CommandState::Unknown(_)
    ));
    assert_eq!(
        cmd.rescan(&sonarr_ref(1, &[])).await.unwrap_err().class,
        ErrorClass::Validation
    );
}

#[tokio::test]
async fn mutating_calls_are_never_retried_but_reads_are() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    c.fake.set_fault(
        "command",
        Fault {
            status: Some(503),
            ..Fault::default()
        },
    );
    let e = c.commander(&r).rescan_all().await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Unavailable);
    assert_eq!(c.fake.count("POST", "/api/v3/command"), 1);

    c.fake.set_fault(
        "queue",
        Fault {
            status: Some(503),
            ..Fault::default()
        },
    );
    assert_eq!(r.queue().await.unwrap_err().class, ErrorClass::Unavailable);
    assert_eq!(c.fake.count("GET", "/api/v3/queue"), 4, "1 try + 3 retries");
}
