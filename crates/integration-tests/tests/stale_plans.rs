//! Task 6: plans go stale when the filesystem, the path mapping or a service changes between
//! planning and execution. Each scenario asserts that nothing was mutated (tree, qBittorrent
//! request log) and what the database says afterwards (plan status, journal, outcomes).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::PlanStatus;
use removarr_integration_tests::*;
use removarr_testkit::fake_qbit::FakeTorrent;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

fn charlie() -> String {
    format!("completed:{}/{}.mkv", fx::CHARLIE, fx::CHARLIE)
}

async fn approved_consolidation(s: &Stack) -> removarr_workflows::plans::PlanPreview {
    let id = s.issue_id(C::HardlinkMissing, &charlie()).await;
    let p = s
        .create_plan(&PlanRequest::issues(
            C::HardlinkMissing,
            vec![id],
            IssueAction::Consolidate,
        ))
        .await;
    s.approve(&p).await;
    p
}

async fn assert_refused_untouched(
    s: &Stack,
    id: removarr_domain::ids::PlanId,
    before: &std::collections::BTreeMap<String, Node>,
) {
    assert_eq!(&world(&s.fx), before, "the tree is untouched");
    assert!(
        !s.qbit_mutations().iter().any(|m| !m.starts_with("start ")),
        "no stop, recheck or delete reached the client: {:?}",
        s.qbit_mutations()
    );
    let st = s.plan_status(id).await;
    assert!(
        matches!(
            st,
            PlanStatus::Invalidated | PlanStatus::Expired | PlanStatus::Approved
        ),
        "{st:?}"
    );
    assert_ne!(st, PlanStatus::Executed);
    let out = s.action_outcomes(id).await;
    assert!(
        out.iter()
            .all(|o| !matches!(o, Some(removarr_domain::journal::ActionOutcome::Verified))),
        "{out:?}"
    );
    let j = s.journal(id).await;
    assert!(
        !j.iter()
            .any(|(step, _)| step == "FsReplaceWithHardlink" || step == "QbitStop"),
        "no mutating step was even intended: {j:?}"
    );
}

#[tokio::test]
async fn a_filesystem_change_after_approval_refuses_execution_before_any_stop() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let p = approved_consolidation(&s).await;
    // The media copy is rewritten (same size, different bytes: V6-like).
    let media = s.p("medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv");
    std::fs::write(&media, removarr_testkit::fixture::bytes("other", 98_304)).unwrap();
    let before = world(&s.fx);
    s.execute(p.id, false).await;
    assert_refused_untouched(&s, p.id, &before).await;
    assert_eq!(s.plan_status(p.id).await, PlanStatus::Invalidated);
    assert_ne!(
        ino(&media),
        ino(&s.p(&format!("completed/{}/{}.mkv", fx::CHARLIE, fx::CHARLIE)))
    );
    // The torrent was never touched.
    assert_eq!(s.qbit.states()[&hash('4')], "uploading");
}

#[tokio::test]
async fn a_filesystem_change_seen_by_a_new_scan_refuses_the_approval() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let p = s
        .create_plan(&PlanRequest::issues(C::Junk, vec![id], IssueAction::Delete))
        .await;
    // Plan staleness is judged on stored state (a newer scan), the live tree is re-checked by
    // the runner at execution (previous scenario).
    std::fs::write(s.p("completed/stray/readme.txt"), b"edited").unwrap();
    s.scan().await;
    let err = s
        .app
        .state
        .plans
        .approve(
            p.id,
            &removarr_domain::plan::ApprovalRequest {
                plan_hash: p.hash.clone(),
                confirmed_count: p.required_count,
                confirmed_torrent_removals: 0,
                acknowledged_samples: vec![],
                acknowledged_unrecoverable: vec![],
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err.class,
            removarr_domain::error::ErrorClass::StateChanged
                | removarr_domain::error::ErrorClass::Conflict
        ),
        "{err:?}"
    );
    assert_ne!(s.plan_status(p.id).await, PlanStatus::Approved);
    assert!(s.p("completed/stray/readme.txt").exists());
}

#[tokio::test]
async fn a_torrent_that_left_the_allowlist_refuses_execution_and_touches_nothing() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let p = approved_consolidation(&s).await;
    // H4 is now checking (a service change after planning).
    s.tweak_torrents(|v| {
        v.iter_mut().find(|t| t.hash == hash('4')).unwrap().state = "checkingUP".into();
    });
    let before = world(&s.fx);
    s.execute(p.id, false).await;
    assert_refused_untouched(&s, p.id, &before).await;
    assert_eq!(s.qbit.states()[&hash('4')], "checkingUP");
}

#[tokio::test]
async fn qbittorrent_unavailable_at_execution_refuses_and_mutates_nothing() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let p = approved_consolidation(&s).await;
    s.qbit.force_status("torrents/info", 503);
    let before = world(&s.fx);
    s.execute(p.id, false).await;
    s.qbit.force_status("torrents/info", 200);
    assert_refused_untouched(&s, p.id, &before).await;
}

#[tokio::test]
async fn a_new_holder_torrent_refuses_a_deletion_plan() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let id = s.issue_id(C::Junk, "completed:stray/readme.txt").await;
    let p = s
        .create_plan(&PlanRequest::issues(C::Junk, vec![id], IssueAction::Delete))
        .await;
    s.approve(&p).await;
    // A torrent now claims the file.
    let mut t = FakeTorrent::new(&hash('e'), "readme.txt", "uploading", 1.0)
        .paths("/data/completed/stray", "/data/completed/stray/readme.txt")
        .added_on(now_secs() - 86_400);
    t = t.file(removarr_testkit::fake_qbit::FakeFile::new(
        "readme.txt",
        128,
    ));
    s.qbit.add_torrent(t);
    s.execute(p.id, false).await;
    assert!(
        s.p("completed/stray/readme.txt").exists(),
        "a file a torrent holds is never deleted"
    );
    assert_ne!(s.plan_status(p.id).await, PlanStatus::Executed);
}

#[tokio::test]
async fn a_changed_path_mapping_after_planning_invalidates_the_plan_across_a_restart() {
    let s = Stack::parity(opts()).await;
    s.scan().await;
    let p = approved_consolidation(&s).await;
    let before = world(&s.fx);
    // Restart over the same database with a different mapping (an extra external prefix).
    let s = s
        .restart_with(|e| {
            e.insert(
                "REMOVARR_QBIT_EXTERNAL_PREFIXES".into(),
                "/srv/other,/srv/more".into(),
            );
        })
        .await;
    s.execute(p.id, false).await;
    assert_refused_untouched(&s, p.id, &before).await;
    assert_eq!(s.plan_status(p.id).await, PlanStatus::Invalidated);
}
