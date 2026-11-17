#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use removarr_arr::snapshot::{SnapshotChange, SnapshotValidity, detect_changes, revalidate};
use removarr_domain::error::ErrorClass;
use removarr_domain::ports::ArrRead;
use removarr_testkit::fake_arr::{Fault, media_management};
use serde_json::json;

fn season() -> removarr_domain::arr::ArrMediaRef {
    sonarr_ref(1, &[103, 101, 102])
}

#[tokio::test]
async fn sonarr_snapshot_holds_profile_scores_files_and_media_management() {
    let c = sonarr(&[]).await;
    let s = c.reader().quality_snapshot(&season()).await.unwrap();
    assert!(s.fingerprint_is_valid());
    assert_eq!(s.service_version, "4.0.0.0-synthetic");
    assert_eq!(
        s.media.arr,
        Some(sonarr_ref(1, &[101, 102, 103])),
        "episode ids normalized"
    );
    assert_eq!(s.media.season, Some(1));
    assert_eq!((s.profile_id, s.profile_name.as_str()), (1, "Synthetic-HD"));
    assert_eq!(s.cutoff, "Group-HD");
    assert!(s.upgrade_allowed);
    assert_eq!((s.min_format_score, s.cutoff_format_score), (0, 100));
    assert_eq!(s.format_scores.get("CF-Penalty"), Some(&-100));
    assert_eq!(s.format_scores.len(), 3);
    let ids: Vec<i64> = s.current_files.iter().map(|f| f.file_id).collect();
    assert_eq!(
        ids,
        vec![1001, 1003],
        "E02 has no file; the special is not requested"
    );
    assert!(s.candidates.is_empty());
    let mm = s.media_management.as_ref().unwrap();
    assert!(mm.hardlinks_enabled);
    assert_eq!(c.fake.mutating_count(), 0);
}

#[tokio::test]
async fn radarr_snapshot_reads_the_movie_file() {
    let c = radarr(&[]).await;
    let s = c.reader().quality_snapshot(&radarr_ref(7)).await.unwrap();
    assert_eq!(s.profile_name, "Synthetic-Movies");
    assert_eq!(s.current_files.len(), 1);
    assert_eq!(s.current_files[0].file_id, 70);
    assert_eq!(s.media.title, "Synthetic Movie");
    assert!(s.media_management.as_ref().unwrap().recycle_bin.is_none());
}

#[tokio::test]
async fn unchanged_data_revalidates_even_after_time_passes() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.clock.advance_hours(5);
    assert_eq!(revalidate(&r, &stored).await, SnapshotValidity::Unchanged);
}

#[tokio::test]
async fn custom_format_score_change_invalidates() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.fake.with_data(|d| {
        d.quality_profiles[0]["formatItems"][1]["score"] = json!(60);
    });
    match revalidate(&r, &stored).await {
        SnapshotValidity::Changed(ch) => assert_eq!(
            ch,
            vec![SnapshotChange::FormatScoreChanged {
                name: "CF-Beta".into(),
                from: Some(50),
                to: Some(60)
            }]
        ),
        other => panic!("expected a change, got {other:?}"),
    }
}

#[tokio::test]
async fn profile_cutoff_and_threshold_changes_invalidate() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.fake.with_data(|d| {
        let p = &mut d.quality_profiles[0];
        p["cutoff"] = json!(7);
        p["upgradeAllowed"] = json!(false);
        p["minFormatScore"] = json!(20);
        p["formatItems"].as_array_mut().unwrap().pop();
        p["name"] = json!("Renamed");
    });
    let SnapshotValidity::Changed(ch) = revalidate(&r, &stored).await else {
        panic!("expected a change");
    };
    assert!(ch.contains(&SnapshotChange::CutoffChanged {
        from: "Group-HD".into(),
        to: "Bluray-1080p".into()
    }));
    assert!(ch.contains(&SnapshotChange::UpgradeAllowedChanged {
        from: true,
        to: false
    }));
    assert!(ch.contains(&SnapshotChange::MinFormatScoreChanged { from: 0, to: 20 }));
    assert!(ch.contains(&SnapshotChange::FormatScoreChanged {
        name: "CF-Penalty".into(),
        from: Some(-100),
        to: None
    }));
    assert!(
        ch.iter()
            .any(|x| matches!(x, SnapshotChange::ProfileChanged { .. }))
    );
}

#[tokio::test]
async fn profile_reassignment_invalidates() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.fake.with_data(|d| {
        let mut other = d.quality_profiles[0].clone();
        other["id"] = json!(9);
        other["name"] = json!("Other");
        d.quality_profiles.push(other);
        d.series[0]["qualityProfileId"] = json!(9);
    });
    let SnapshotValidity::Changed(ch) = revalidate(&r, &stored).await else {
        panic!("expected a change");
    };
    assert_eq!(
        ch,
        vec![SnapshotChange::ProfileChanged {
            from: (1, "Synthetic-HD".into()),
            to: (9, "Other".into())
        }]
    );
}

#[tokio::test]
async fn current_file_and_media_management_changes_invalidate() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.fake.with_data(|d| {
        d.episode_files[1]["customFormatScore"] = json!(11);
        d.media_management = media_management(None, 7, true);
    });
    let SnapshotValidity::Changed(ch) = revalidate(&r, &stored).await else {
        panic!("expected a change");
    };
    assert!(ch.contains(&SnapshotChange::CurrentFilesChanged {
        added: vec![],
        removed: vec![],
        modified: vec![1001]
    }));
    assert!(ch.contains(&SnapshotChange::MediaManagementChanged));
}

#[tokio::test]
async fn outage_or_auth_failure_is_never_unchanged() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    c.fake.set_fault(
        "*",
        Fault {
            status: Some(503),
            ..Fault::default()
        },
    );
    let v = revalidate(&r, &stored).await;
    assert!(!v.is_unchanged());
    assert!(
        matches!(v, SnapshotValidity::Unavailable(ref e) if e.class == ErrorClass::Unavailable)
    );
    c.fake.set_fault(
        "*",
        Fault {
            status: Some(401),
            ..Fault::default()
        },
    );
    let v = revalidate(&r, &stored).await;
    assert!(
        matches!(v, SnapshotValidity::Unavailable(ref e) if e.class == ErrorClass::Unauthorized)
    );
}

#[tokio::test]
async fn tampered_or_foreign_stored_snapshots_are_rejected() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let stored = r.quality_snapshot(&season()).await.unwrap();
    let mut tampered = stored.clone();
    tampered.cutoff_format_score = -1000;
    let SnapshotValidity::Changed(ch) = revalidate(&r, &tampered).await else {
        panic!("expected a change");
    };
    assert!(ch.contains(&SnapshotChange::StoredFingerprintInvalid));

    let mut no_ref = stored.clone();
    no_ref.media.arr = None;
    assert_eq!(
        revalidate(&r, &no_ref).await,
        SnapshotValidity::Changed(vec![SnapshotChange::NoMediaRef])
    );

    let radarr_ctx = radarr(&[]).await;
    assert_eq!(
        revalidate(&radarr_ctx.reader(), &stored).await,
        SnapshotValidity::Changed(vec![SnapshotChange::ServiceChanged])
    );
    assert!(detect_changes(&stored, &stored).is_empty());
}

#[tokio::test]
async fn a_vanished_episode_fails_the_snapshot() {
    let c = sonarr(&[]).await;
    let e = c
        .reader()
        .quality_snapshot(&sonarr_ref(1, &[101, 999]))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::StateChanged);
}
