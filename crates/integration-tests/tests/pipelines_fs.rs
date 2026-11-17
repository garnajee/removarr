//! Task 3 (filesystem and qBittorrent pipelines): scan -> issues -> plan -> approve -> execute ->
//! verify through the production wiring, a real SQLite database, a real temporary tree with real
//! hardlinks and the loopback qBittorrent fake. Assertions are on the tree (inodes), on the
//! qBittorrent request log, and on the persisted journal, outcomes, plan status and re-scan.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use removarr_domain::ids::IssueId;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::jobs::JobStatus;
use removarr_domain::journal::ActionOutcome;
use removarr_domain::plan::{ActionKind, PlanStatus};
use removarr_integration_tests::*;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

fn charlie_main() -> String {
    format!("{}/{}.mkv", fx::CHARLIE, fx::CHARLIE)
}
const CHARLIE_MEDIA: &str = "movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv";

#[tokio::test]
async fn consolidation_links_completed_toward_media_and_verifies_by_inode() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let comp = s.p(&format!("completed/{}", charlie_main()));
    let media = s.p(&format!("medias/{CHARLIE_MEDIA}"));
    let (i_comp, i_media) = (ino(&comp), ino(&media));
    assert_ne!(i_comp, i_media);
    let content = fs::read(&comp).unwrap();
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{}", charlie_main()))
        .await;

    let plan = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id.clone()],
            IssueAction::Consolidate,
        ))
        .await;
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(plan.status, Some(PlanStatus::Draft));
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Draft);
    // Planning is read-only: no mutation anywhere.
    assert!(s.qbit_mutations().is_empty());
    assert_eq!(ino(&media), i_media);

    s.approve(&plan).await;
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Approved);
    assert_eq!(s.execute(plan.id, false).await, JobStatus::Succeeded);

    // Direction and inode result: the media path now carries the completed inode; the
    // completed inode is unchanged; bytes identical; no temporary link left behind.
    assert_eq!(ino(&comp), i_comp, "the completed inode is never replaced");
    assert_eq!(
        ino(&media),
        i_comp,
        "the media path is now a hardlink of the completed file"
    );
    assert_eq!(nlink(&comp), 2);
    assert_eq!(fs::read(&media).unwrap(), content);
    let leftovers: Vec<_> = fs::read_dir(media.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "no temporary file next to the replaced one: {leftovers:?}"
    );

    // Persisted journal: every intent has a result, in the documented order.
    assert_eq!(
        s.journal(plan.id).await,
        [
            ("FsProbeHardlink", "Done"),
            ("QbitStop", "Done"),
            ("FsProbeHardlink", "Done"),
            ("FsReplaceWithHardlink", "Done"),
            ("QbitRecheck", "Done"),
            ("QbitStart", "Done"),
        ]
        .map(|(a, b)| (a.to_string(), Some(b.to_string())))
    );
    // qBittorrent saw exactly stop, recheck, start for H4 and never a delete.
    let m = s.qbit_mutations();
    assert_eq!(m.len(), 3, "{m:?}");
    assert!(
        m[0].starts_with("stop ") && m[1].starts_with("recheck ") && m[2].starts_with("start "),
        "{m:?}"
    );
    assert!(m.iter().all(|r| r.contains(&hash('4'))));
    assert_eq!(
        s.qbit.states()[&hash('4')],
        "uploading",
        "resumed after the verified recheck"
    );

    // Final verification state: persisted outcome and plan status, then a fresh scan agrees.
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
    s.scan().await;
    assert!(
        s.try_issue_at(C::HardlinkMissing, &format!("completed:{}", charlie_main()))
            .await
            .is_none()
    );
    assert!(
        s.try_issue_at(C::Duplicate, &format!("completed:{}", charlie_main()))
            .await
            .is_none()
    );
    assert!(
        !s.issue_exists(&id).await,
        "the resolved issue is no longer current"
    );
}

#[tokio::test]
async fn consolidation_of_a_file_shared_by_two_torrents_resumes_only_the_active_one() {
    let s = Stack::parity(opts()).await;
    // H3 is stopped before planning (5.x name): it must stay stopped afterwards.
    s.tweak_torrents(|v| {
        v.iter_mut().find(|t| t.hash == hash('3')).unwrap().state = "stoppedUP".into();
    });
    s.scan().await;
    let comp = s.p(&fx::bravo_completed());
    let media = s.p(&fx::bravo_media());
    let i_comp = ino(&comp);
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{}.mkv", fx::BRAVO))
        .await;
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    assert_eq!(ino(&media), i_comp);
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
    let m = s.qbit_mutations();
    let of = |p: &str| m.iter().filter(|r| r.starts_with(p)).collect::<Vec<_>>();
    let (stops, rechecks, starts) = (of("stop "), of("recheck "), of("start "));
    assert_eq!(
        (stops.len(), rechecks.len(), starts.len()),
        (1, 1, 1),
        "{m:?}"
    );
    for h in [hash('2'), hash('3')] {
        assert!(stops[0].contains(&h) && rechecks[0].contains(&h), "{m:?}");
    }
    assert!(
        starts[0].contains(&hash('2')) && !starts[0].contains(&hash('3')),
        "{m:?}"
    );
    let st = s.qbit.states();
    assert_eq!(st[&hash('2')], "uploading");
    assert_eq!(
        st[&hash('3')],
        "stoppedUP",
        "the torrent that was not active is not resumed"
    );
}

#[tokio::test]
async fn recheck_timeout_leaves_the_torrent_stopped_and_the_outcome_unverified() {
    let s = Stack::parity(opts().env("REMOVARR_RECHECK_TIMEOUT_SECONDS", "30")).await;
    s.qbit
        .set_recheck(removarr_testkit::fake_qbit::RecheckScript {
            checking_polls: u32::MAX,
            result: None,
        });
    s.scan().await;
    let comp = s.p(&format!("completed/{}", charlie_main()));
    let media = s.p(&format!("medias/{CHARLIE_MEDIA}"));
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{}", charlie_main()))
        .await;
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    // The replacement happened (the inode is the completed one) but it is not proven.
    assert_eq!(ino(&media), ino(&comp));
    let m = s.qbit_mutations();
    assert!(
        m.iter().any(|r| r.starts_with("stop ")) && m.iter().any(|r| r.starts_with("recheck "))
    );
    assert!(
        !m.iter()
            .any(|r| r.starts_with("start ") || r.starts_with("delete ")),
        "{m:?}"
    );
    assert_eq!(
        s.qbit.states()[&hash('4')],
        "stoppedUP",
        "left stopped, reported, never removed"
    );
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::PartiallyExecuted);
    let out = s.action_outcomes(plan.id).await;
    assert!(
        matches!(out[0], Some(ActionOutcome::ExecutedUnverified { .. })),
        "{out:?}"
    );
}

#[tokio::test]
async fn junk_deletion_removes_files_and_the_emptied_parent_only() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let before = world(&s.fx);
    let mut ids = Vec::new();
    for f in ["readme.txt", "archive.rar", "Unrelated.Film.2010.srt"] {
        ids.push(s.issue_id(C::Junk, &format!("completed:stray/{f}")).await);
    }
    let plan = s
        .create_plan(&PlanRequest::issues(C::Junk, ids, IssueAction::Delete))
        .await;
    let kinds: Vec<_> = plan.actions.iter().map(|a| a.kind).collect();
    assert!(kinds.contains(&ActionKind::RemoveEmptyParent), "{kinds:?}");
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    assert!(
        !s.p("completed/stray").exists(),
        "the emptied directory is removed"
    );
    let after = world(&s.fx);
    let gone: Vec<_> = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .cloned()
        .collect();
    assert_eq!(gone.len(), 4, "{gone:?}");
    for k in after.keys() {
        if !k.starts_with("completed") {
            assert_eq!(after[k], before[k], "{k}");
        }
    }
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    assert!(
        s.action_outcomes(plan.id)
            .await
            .iter()
            .all(|o| *o == Some(ActionOutcome::Verified))
    );
    assert!(s.qbit_mutations().is_empty(), "no torrent involved");
    s.scan().await;
    assert!(s.issues(C::Junk).await.is_empty());
}

#[tokio::test]
async fn task_removal_never_deletes_files_through_qbittorrent() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let delta = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{delta}"))
        .await;
    let plan = s
        .create_plan(&PlanRequest::Issues {
            category: C::HardlinkMissing,
            issue_ids: vec![id],
            action: IssueAction::Delete,
            remove_torrent_tasks: true,
            include_specials: false,
        })
        .await;
    assert_eq!(plan.required_torrent_removals, 1);
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    assert!(!s.p(&format!("completed/{delta}")).exists());
    let m = s.qbit_mutations();
    let del: Vec<_> = m.iter().filter(|r| r.starts_with("delete ")).collect();
    assert_eq!(del.len(), 1, "{m:?}");
    assert!(
        del[0].contains("deleteFiles=false") && del[0].contains(&hash('5')),
        "{m:?}"
    );
    assert!(!s.qbit.has_torrent(&hash('5')), "the task is gone");
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    let j = s.journal(plan.id).await;
    assert!(
        j.iter()
            .any(|(st, r)| st == "QbitRemoveKeepData" && r.as_deref() == Some("Done")),
        "{j:?}"
    );
}

#[tokio::test]
async fn deleting_one_file_of_a_two_file_torrent_leaves_it_stopped_and_never_removes_it() {
    let s = Stack::parity(opts()).await;
    // After the deletion the real client would find the torrent half complete.
    s.qbit
        .set_recheck(removarr_testkit::fake_qbit::RecheckScript {
            checking_polls: 1,
            result: Some(("stoppedUP".into(), 0.5)),
        });
    s.scan().await;
    let e02 = format!("{}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv", fx::FOXTROT);
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{e02}"))
        .await;
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Delete,
        ))
        .await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    assert!(!s.p(&format!("completed/{e02}")).exists());
    assert!(
        s.p(&format!(
            "completed/{}/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv",
            fx::FOXTROT
        ))
        .exists()
    );
    assert!(
        s.qbit.has_torrent(&hash('6')),
        "the torrent is never removed here"
    );
    assert!(!s.qbit_mutations().iter().any(|r| r.starts_with("delete ")));
    assert_eq!(
        s.qbit.states()[&hash('6')],
        "stoppedUP",
        "left stopped and reported"
    );
    // The deletion itself is verified on disk; the half-complete torrent is only reported
    // (left stopped), the action is not "unverified".
    assert_eq!(
        s.action_outcomes(plan.id).await,
        vec![Some(ActionOutcome::Verified)]
    );
    let j = s.journal(plan.id).await;
    assert!(j.iter().any(|(st, _)| st == "QbitRecheck"), "{j:?}");
    assert!(
        !s.qbit_mutations().iter().any(|r| r.starts_with("start ")),
        "never restarted: {:?} {j:?}",
        s.qbit_mutations()
    );
}

#[tokio::test]
async fn stalled_incomplete_cleanup_stops_removes_the_task_keep_data_then_deletes_the_data() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let issues = s.issues(C::IncompleteStalled).await;
    assert_eq!(issues.len(), 1);
    let id = IssueId(issues[0].id.clone());
    let plan = s
        .create_plan(&PlanRequest::issues(
            C::IncompleteStalled,
            vec![id],
            IssueAction::CleanupStalled,
        ))
        .await;
    s.approve(&plan).await;
    s.execute(plan.id, false).await;

    assert!(
        !s.p("incomplete/Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv")
            .exists()
    );
    assert!(
        s.p("incomplete/abandoned.partial.mkv").exists(),
        "unrelated data is untouched"
    );
    let m = s.qbit_mutations();
    assert!(m[0].starts_with("stop "), "{m:?}");
    let del = m
        .iter()
        .find(|r| r.starts_with("delete "))
        .expect("task removal");
    assert!(
        del.contains("deleteFiles=false") && del.contains(&hash('d')),
        "{m:?}"
    );
    assert!(!s.qbit.has_torrent(&hash('d')));
    assert_eq!(s.plan_status(plan.id).await, PlanStatus::Executed);
    let j = s.journal(plan.id).await;
    let order: Vec<&str> = j.iter().map(|(a, _)| a.as_str()).collect();
    let pos = |n: &str| {
        order
            .iter()
            .position(|x| *x == n)
            .unwrap_or_else(|| panic!("{n} in {order:?}"))
    };
    assert!(
        pos("QbitStop") < pos("QbitRemoveKeepData")
            && pos("QbitRemoveKeepData") < pos("FsRemoveFile")
    );
    assert!(j.iter().all(|(_, r)| r.as_deref() == Some("Done")), "{j:?}");
}

#[tokio::test]
async fn an_out_of_scope_torrent_blocks_every_plan_fail_closed_until_declared_external() {
    // T_Q (save path under no mapping rule) makes ownership unknown: the base fixture refuses to
    // plan (CON-07), and nothing is mutated.
    let s = Stack::parity(Options::live()).await;
    s.scan().await;
    let before = world(&s.fx);
    let id = s
        .issue_id(C::HardlinkMissing, &format!("completed:{}", charlie_main()))
        .await;
    let err = s
        .try_create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await
        .unwrap_err();
    assert!(!err.excluded.is_empty());
    assert!(
        err.excluded[0]
            .blockers
            .iter()
            .any(|b| b.contains("ServiceUnavailable")),
        "{err:?}"
    );
    let junk = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    assert!(
        s.try_create_plan(&PlanRequest::issues(
            C::Junk,
            vec![junk],
            IssueAction::Delete
        ))
        .await
        .is_err()
    );
    assert_eq!(world(&s.fx), before);
    assert!(s.qbit_mutations().is_empty());
}
