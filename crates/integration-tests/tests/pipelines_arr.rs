//! Task 3 (ARR release replacement): Radarr's loopback fake with a filesystem bridge imports a
//! completed release over the movie file it currently serves. Copy (hardlink on and off), a
//! "move" that must be detected, the three old-file recovery cases (recycle bin, other link,
//! unrecoverable), conflicting imports and ARR profile staleness. Assertions: tree and inodes,
//! the ARR command log (`importMode` literal), persisted journal, outcomes and plan status.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use removarr_domain::error::ErrorClass;
use removarr_domain::ids::PlanId;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::jobs::JobKind;
use removarr_domain::journal::ActionOutcome;
use removarr_domain::plan::{ApprovalRequest, PlanStatus};
use removarr_integration_tests::*;
use removarr_testkit::fake_arr::ImportEffect;
use removarr_workflows::plans::{IssueAction, PlanPreview, PlanRequest};

async fn replace_plan(s: &Stack, delete_old: bool) -> PlanPreview {
    let id = s
        .issue_id(
            C::HardlinkMissing,
            &format!("completed:{0}/{0}.mkv", zulu::REL),
        )
        .await;
    s.create_plan(&PlanRequest::issues(
        C::HardlinkMissing,
        vec![id],
        IssueAction::ReplaceRelease {
            delete_old_media: delete_old,
        },
    ))
    .await
}

fn imports(s: &Stack) -> Vec<serde_json::Value> {
    s.radarr
        .as_ref()
        .unwrap()
        .commands()
        .into_values()
        .filter(|c| c["name"] == "ManualImport")
        .collect()
}

#[tokio::test]
async fn unrecoverable_old_file_needs_an_acknowledgement_then_the_import_hardlinks_the_release() {
    let s = zulu::stack(None, true, ImportEffect::Copy, Options::live()).await;
    s.scan().await;
    let new = zulu::new_path(&s.fx);
    let old = zulu::old_path(&s.fx);
    let dest = zulu::dest_path(&s.fx);
    let i_new = ino(&new);
    let plan = replace_plan(&s, false).await;
    assert_eq!(
        plan.unrecoverable_to_acknowledge.len(),
        1,
        "no other link and no recycle bin"
    );
    // Without the acknowledgement the approval is refused.
    let err = s
        .app
        .state
        .plans
        .approve(
            plan.id,
            &ApprovalRequest {
                plan_hash: plan.hash.clone(),
                confirmed_count: plan.required_count,
                confirmed_torrent_removals: 0,
                acknowledged_samples: vec![],
                acknowledged_unrecoverable: vec![],
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.class, ErrorClass::Validation | ErrorClass::Conflict),
        "{err:?}"
    );
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Draft);
    assert!(
        imports(&s).is_empty(),
        "planning and refused approval send nothing to ARR"
    );

    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    // ARR imported by copy-or-hardlink: the completed release is untouched, the new library file
    // shares its inode, the old file was retired by ARR's upgrade.
    assert!(
        new.exists() && ino(&new) == i_new,
        "the completed release is never moved"
    );
    assert_eq!(ino(&dest), i_new, "hardlinked into the library");
    assert!(!old.exists());
    let cmds = imports(&s);
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    assert_eq!(
        cmds[0]["importMode"], "copy",
        "the import mode is the literal copy"
    );
    assert!(
        s.radarr.as_ref().unwrap().violations().is_empty(),
        "{:?}",
        s.radarr.as_ref().unwrap().violations()
    );
    // Persisted result.
    let j = s.journal(plan.id).await;
    assert!(
        j.iter()
            .any(|(st, r)| st == "ArrManualImport" && r.as_deref() == Some("Done")),
        "{j:?}"
    );
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
    // After a rescan the release is a healthy hardlink and the issue is gone.
    s.scan().await;
    assert!(
        s.try_issue_at(
            C::HardlinkMissing,
            &format!("completed:{0}/{0}.mkv", zulu::REL)
        )
        .await
        .is_none()
    );
}

#[tokio::test]
async fn a_recycle_bin_makes_the_old_file_recoverable_and_it_lands_there() {
    let s = zulu::stack(
        Some("/medias/.recycle"),
        true,
        ImportEffect::Copy,
        Options::live(),
    )
    .await;
    s.scan().await;
    let old = zulu::old_path(&s.fx);
    let old_ino = ino(&old);
    let plan = replace_plan(&s, false).await;
    assert!(
        plan.unrecoverable_to_acknowledge.is_empty(),
        "{:?}",
        plan.warnings
    );
    s.approve(&plan).await;
    s.execute(plan.id, false).await;
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    let binned = s.p("medias/.recycle/Zulu Movie (2022).mkv");
    assert!(binned.exists() && !old.exists());
    assert_eq!(
        ino(&binned),
        old_ino,
        "the old inode is kept, not copied or lost"
    );
}

#[tokio::test]
async fn another_link_to_the_old_file_also_makes_it_recoverable() {
    let s = zulu::stack(None, true, ImportEffect::Copy, Options::live()).await;
    fs::create_dir_all(s.p("medias/keep")).unwrap();
    fs::hard_link(
        zulu::old_path(&s.fx),
        s.p("medias/keep/zulu-old-backup.mkv"),
    )
    .unwrap();
    age_hours(&s.p("medias/keep/zulu-old-backup.mkv"), 72);
    age_hours(&s.p("medias/keep"), 72);
    s.scan().await;
    let plan = replace_plan(&s, false).await;
    assert!(
        plan.unrecoverable_to_acknowledge.is_empty(),
        "{:?}",
        plan.warnings
    );
    s.approve(&plan).await;
    s.execute(plan.id, false).await;
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert!(
        s.p("medias/keep/zulu-old-backup.mkv").exists(),
        "the other link keeps the old bytes"
    );
}

#[tokio::test]
async fn a_move_instead_of_a_copy_is_detected_and_needs_review() {
    let s = zulu::stack(None, true, ImportEffect::Move, Options::live()).await;
    // The real client would find the moved file missing on the recheck.
    s.qbit
        .set_recheck(removarr_testkit::fake_qbit::RecheckScript {
            checking_polls: 1,
            result: Some(("missingFiles".into(), 0.0)),
        });
    s.scan().await;
    let plan = replace_plan(&s, false).await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;
    // ARR moved the completed file into the library despite `importMode: copy`.
    assert!(!zulu::new_path(&s.fx).exists());
    assert!(zulu::dest_path(&s.fx).exists());
    let out = s.action_outcomes(plan.id).await;
    assert_eq!(out, vec![Some(ActionOutcome::FailedNeedsReview)], "{out:?}");
    assert_ne!(s.plan_status(plan.id).await, PlanStatus::Executed);
    // The seeding torrent is not resumed on a file that is gone, and nothing is removed.
    assert!(s.qbit.has_torrent(&hash('f')));
    let m = s.qbit_mutations();
    assert!(
        !m.iter()
            .any(|r| r.starts_with("delete ") || r.starts_with("start ")),
        "{m:?}"
    );
}

#[tokio::test]
async fn with_hardlinks_disabled_the_import_is_a_costly_copy_that_is_not_verified() {
    let s = zulu::stack(None, false, ImportEffect::Copy, Options::live()).await;
    s.scan().await;
    let plan = replace_plan(&s, false).await;
    assert!(
        plan.warnings
            .iter()
            .any(|w| format!("{:?}", w.code).contains("ArrHardlinksDisabled")),
        "{:?}",
        plan.warnings
    );
    s.approve(&plan).await;
    s.execute(plan.id, false).await;
    // Bytes were copied: a distinct inode in the library, the release untouched.
    assert_ne!(ino(&zulu::dest_path(&s.fx)), ino(&zulu::new_path(&s.fx)));
    assert_eq!(
        fs::read(zulu::dest_path(&s.fx)).unwrap(),
        fs::read(zulu::new_path(&s.fx)).unwrap()
    );
    let out = s.action_outcomes(plan.id).await;
    assert!(
        matches!(out[0], Some(ActionOutcome::ExecutedUnverified { .. })),
        "{out:?}"
    );
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::PartiallyExecuted);
}

#[tokio::test]
async fn an_arr_profile_change_after_planning_invalidates_the_plan_before_any_import() {
    let s = zulu::stack(
        Some("/medias/.recycle"),
        true,
        ImportEffect::Copy,
        Options::live(),
    )
    .await;
    s.scan().await;
    let plan = replace_plan(&s, false).await;
    s.approve(&plan).await;
    // The profile now scores the custom format the new release carries at -100.
    s.radarr.as_ref().unwrap().with_data(|d| {
        for p in &mut d.quality_profiles {
            if let Some(items) = p["formatItems"].as_array_mut() {
                for i in items {
                    if i["name"] == "CF-Beta" {
                        i["score"] = (-100).into();
                    }
                }
            }
        }
    });
    let before = world(&s.fx);
    s.execute(plan.id, false).await;
    assert!(imports(&s).is_empty(), "no import was sent");
    assert_eq!(world(&s.fx), before);
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Invalidated);
    assert!(zulu::old_path(&s.fx).exists());
}

#[tokio::test]
async fn two_plans_for_the_same_media_import_at_most_once() {
    let s = zulu::stack(
        Some("/medias/.recycle"),
        true,
        ImportEffect::Copy,
        Options::live(),
    )
    .await;
    s.scan().await;
    let a = replace_plan(&s, false).await;
    let b = replace_plan(&s, false).await;
    assert_ne!(a.id, b.id);
    s.approve(&a).await;
    s.approve(&b).await;
    let ids: Vec<PlanId> = vec![a.id, b.id];
    for id in &ids {
        s.app
            .runtime
            .submit(JobKind::ExecutePlan {
                plan_id: *id,
                simulate: false,
            })
            .await
            .unwrap();
    }
    s.wait_idle().await;
    assert_eq!(
        imports(&s).len(),
        1,
        "one importer per media: {:?}",
        imports(&s)
    );
    let mut executed = 0;
    for id in &ids {
        if s.plan_status(*id).await == PlanStatus::Executed {
            executed += 1;
        }
    }
    assert_eq!(executed, 1);
    let other = if s.plan_status(a.id).await == PlanStatus::Executed {
        b.id
    } else {
        a.id
    };
    assert_ne!(s.plan_status(other).await, PlanStatus::Executed);
    assert!(
        !matches!(
            s.action_outcomes(other).await[0],
            Some(ActionOutcome::Verified)
        ),
        "the second plan did not claim success"
    );
    assert!(s.radarr.as_ref().unwrap().violations().is_empty());
    assert!(zulu::dest_path(&s.fx).exists());
}

/// Radarr also knows "Alpha Movie" (movie 9), whose library file is the healthy hardlink of the
/// Alpha torrent, with a leftover queue entry for that download.
fn add_alpha_queue_entry(s: &Stack) {
    s.radarr.as_ref().unwrap().with_data(|d| {
        d.movies.push(serde_json::json!({ "id": 9, "title": "Alpha Movie", "year": 2020, "tmdbId": 200_009,
            "path": "/medias/movies/Alpha Movie (2020)", "monitored": true, "qualityProfileId": 2 }));
        let mut f = d.movie_files[0].clone();
        f["id"] = 90.into();
        f["movieId"] = 9.into();
        f["path"] = "/medias/movies/Alpha Movie (2020)/Alpha Movie (2020).mkv".into();
        f["relativePath"] = "Alpha Movie (2020).mkv".into();
        d.movie_files.push(f);
        d.queue = vec![serde_json::json!({ "id": 21, "downloadId": "1".repeat(40), "movieId": 9,
            "status": "warning", "trackedDownloadStatus": "warning", "trackedDownloadState": "importBlocked",
            "statusMessages": [{ "title": "x", "messages": ["Not an upgrade for existing movie file"] }] })];
    });
}

async fn queue_plan(s: &Stack) -> PlanPreview {
    add_alpha_queue_entry(s);
    s.scan().await;
    let rows = s.issues(C::ArrQueue).await;
    assert_eq!(rows.len(), 1);
    let id = removarr_domain::ids::IssueId(rows[0].id.clone());
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::ArrQueue,
            vec![id],
            IssueAction::RemoveQueueItem,
        ))
        .await;
    s.approve(&plan).await;
    plan
}

fn queue_opts() -> Options {
    Options::live().env("REMOVARR_CAP_ARR_QUEUE_REMOVE", "true")
}

/// DEFECT (owner: 13 action-runner, `actions/arrcmd.rs::queue_item_proof`): the runner requires
/// every path of `QbitFile::candidates` to share an inode with ARR's file. The candidates of a
/// qBittorrent file include the `<name>.!qB` incomplete-download variant, which never exists for
/// a completed file, so a proven stale queue entry (which the issue engine accepts and the planner
/// plans) is always refused at execution. Expected: the entry is removed with
/// `removeFromClient=false&blocklist=false`.
#[tokio::test]
#[ignore = "defect: runner queue_item_proof stats the .!qB candidate of a completed file (owner 13); see handoff 22"]
async fn a_proven_stale_queue_entry_is_removed_keeping_media_torrent_and_blocklist_untouched() {
    let s = zulu::stack(None, true, ImportEffect::Copy, queue_opts()).await;
    let plan = queue_plan(&s).await;
    let before = world(&s.fx);
    s.execute(plan.id, false).await;
    assert_eq!(world(&s.fx), before, "no file is touched");
    assert!(s.qbit_mutations().is_empty(), "the torrent is not touched");
    let radarr = s.radarr.as_ref().unwrap();
    let del: Vec<_> = radarr
        .recorded()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .collect();
    assert_eq!(del.len(), 1, "{del:?}");
    assert_eq!(
        del[0].query.get("removeFromClient").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        del[0].query.get("blocklist").map(String::as_str),
        Some("false")
    );
    assert!(radarr.data().queue.is_empty());
    assert!(radarr.violations().is_empty());
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
}

/// Current behavior of the same flow: safe (fail closed). Nothing is sent to ARR, the plan is
/// invalidated with the blocker, and no file or torrent changes.
#[tokio::test]
async fn today_the_queue_removal_is_refused_by_the_runner_without_any_mutation() {
    let s = zulu::stack(None, true, ImportEffect::Copy, queue_opts()).await;
    let plan = queue_plan(&s).await;
    let before = world(&s.fx);
    s.execute(plan.id, false).await;
    assert_eq!(world(&s.fx), before);
    assert!(s.qbit_mutations().is_empty());
    let radarr = s.radarr.as_ref().unwrap();
    assert!(radarr.recorded().iter().all(|r| r.method == "GET"));
    assert_eq!(radarr.data().queue.len(), 1);
    let out = s.action_outcomes(plan.id).await;
    assert!(
        matches!(out[0], Some(ActionOutcome::NotExecuted { .. })),
        "{out:?}"
    );
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Failed);
}

/// GAP (owners: 16 backend-api `PlannerControl::arr_observations` returns no observation for
/// `PlanRequest::Release`, 14 job-runner keeps no search results): in the production wiring a
/// searched release can never be planned (`ReleaseNotInSearch`), so the grab flow cannot run end
/// to end. The runner side is covered by `actions_tier4.rs` with a port-level fake.
#[tokio::test]
#[ignore = "defect: production wiring cannot plan a grab, no search observation reaches the planner (owners 16 and 14); see handoff 22"]
async fn a_search_never_grabs_and_an_approved_grab_sends_exactly_that_release() {
    use removarr_domain::arr::{ArrMediaRef, ArrService, ReleaseRef};
    let s = zulu::stack(
        None,
        true,
        ImportEffect::Copy,
        Options::live().env("REMOVARR_CAP_ARR_GRAB", "true"),
    )
    .await;
    s.scan().await;
    let media = ArrMediaRef {
        service: ArrService::Radarr,
        media_id: 7,
        episode_ids: vec![],
    };
    let radarr = s.radarr.as_ref().unwrap();

    // A grab without a prior search is refused at planning.
    let release = ReleaseRef {
        guid: "movie-guid-approved".into(),
        indexer_id: 2,
    };
    let req = PlanRequest::Release {
        media: media.clone(),
        release: release.clone(),
    };
    assert!(
        s.try_create_plan(&req).await.is_err(),
        "an unsearched release cannot be grabbed"
    );

    // The search is read-only.
    let st = s
        .run_job(JobKind::EvaluateMedia {
            media: media.clone(),
            search: true,
        })
        .await;
    assert_eq!(st, removarr_domain::jobs::JobStatus::Succeeded);
    assert_eq!(
        radarr.mutating_count(),
        0,
        "a search sends no command and no grab"
    );
    assert!(radarr.grabs().is_empty());

    let plan = s.create_plan(&req).await;
    s.approve(&plan).await;
    assert!(
        radarr.grabs().is_empty(),
        "planning and approving grab nothing"
    );

    // ARR starts the download shortly after the grab: a queue item appears.
    let adder = async {
        for _ in 0..600 {
            if !radarr.grabs().is_empty() {
                radarr.with_data(|d| {
                    d.queue.push(serde_json::json!({ "id": 31, "downloadId": "9".repeat(40), "movieId": 7,
                        "status": "ok", "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading" }));
                });
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let (_, _) = tokio::join!(adder, s.execute(plan.id, false));
    let grabs = radarr.grabs();
    assert_eq!(grabs.len(), 1, "{grabs:?}");
    assert_eq!(grabs[0]["guid"], "movie-guid-approved");
    assert!(radarr.violations().is_empty());
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
}

/// What does hold today: a search is read-only, and without an observation of its results a
/// release cannot be planned (fail closed), so nothing is ever grabbed by accident.
#[tokio::test]
async fn today_a_search_is_read_only_and_a_release_cannot_be_planned() {
    use removarr_domain::arr::{ArrMediaRef, ArrService, ReleaseRef};
    let s = zulu::stack(
        None,
        true,
        ImportEffect::Copy,
        Options::live().env("REMOVARR_CAP_ARR_GRAB", "true"),
    )
    .await;
    s.scan().await;
    let media = ArrMediaRef {
        service: ArrService::Radarr,
        media_id: 7,
        episode_ids: vec![],
    };
    let radarr = s.radarr.as_ref().unwrap();
    let req = PlanRequest::Release {
        media: media.clone(),
        release: ReleaseRef {
            guid: "movie-guid-approved".into(),
            indexer_id: 2,
        },
    };
    assert!(s.try_create_plan(&req).await.is_err());
    let st = s
        .run_job(JobKind::EvaluateMedia {
            media,
            search: true,
        })
        .await;
    assert_eq!(st, removarr_domain::jobs::JobStatus::Succeeded);
    assert!(
        radarr
            .recorded()
            .iter()
            .any(|r| r.path.ends_with("/release") && r.method == "GET")
    );
    assert_eq!(
        radarr.mutating_count(),
        0,
        "a search sends no command and no grab"
    );
    assert!(radarr.grabs().is_empty());
    assert!(s.try_create_plan(&req).await.is_err());
    assert!(radarr.grabs().is_empty());
}
