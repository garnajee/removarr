//! Task 5: crash injection. The application runtime is killed while a request to the loopback
//! qBittorrent fake is in flight (after the intent was journaled, before or after earlier
//! mutations), then a new composition root starts over the same SQLite file and the same tree.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use removarr_domain::issue::IssueCategory as C;
use removarr_domain::jobs::JobKind;
use removarr_integration_tests::*;
use removarr_workflows::plans::{IssueAction, PlanRequest};

fn opts() -> Options {
    Options::live().env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
}

const CH: &str = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
const MEDIA: &str = "medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv";

/// Plans and approves the Charlie consolidation, makes `hang` hang, starts the execution and
/// waits until that request is in flight. Returns the plan id.
fn start_hanging(lab: &Lab, hang: &str) -> removarr_domain::ids::PlanId {
    let id = lab.app(async {
        let s = lab.stack();
        s.scan().await;
        let iss = s
            .issue_id(C::HardlinkMissing, &format!("completed:{CH}/{CH}.mkv"))
            .await;
        let p = s
            .create_plan(&PlanRequest::issues(
                C::HardlinkMissing,
                vec![iss],
                IssueAction::Consolidate,
            ))
            .await;
        s.approve(&p).await;
        p.id
    });
    lab.stack()
        .qbit
        .set_path_delay(hang, Duration::from_secs(3600));
    lab.app(async {
        lab.stack()
            .app
            .runtime
            .submit(JobKind::ExecutePlan {
                plan_id: id,
                simulate: false,
            })
            .await
            .unwrap();
    });
    let h = hang.to_string();
    lab.wait_for(hang, move |s| !s.qbit.requests_to(&h).is_empty());
    id
}

fn restart_and_settle(lab: &mut Lab) {
    lab.crash_and_restart();
    lab.release();
    // Recovery runs at start and before every execution; wait for it to be clean.
    lab.wait_for("recovery", |s| {
        matches!(
            s.app.runtime.status().recovery,
            removarr_jobs::RecoveryStatus::Clean
        )
    });
    // Let a recovery pass that resumes a torrent finish its 2 s recheck poll.
    std::thread::sleep(Duration::from_secs(5));
}

fn no_unfinished(lab: &Lab) {
    let un = lab
        .app(removarr_domain::ports::JournalStore::unfinished(
            &lab.stack().repos.journal,
        ))
        .unwrap();
    assert!(
        un.is_empty(),
        "an intent is still unresolved after recovery: {un:?}"
    );
}

fn no_destructive_delete(s: &Stack) {
    for m in s.qbit_mutations() {
        assert!(!m.contains("deleteFiles=true"), "{m}");
    }
}

#[test]
fn crash_after_the_stop_intent_before_the_stop_took_effect() {
    let mut lab = Lab::parity(opts());
    let id = start_hanging(&lab, "torrents/stop");
    let media = lab.stack().p(MEDIA);
    let before_ino = ino(&media);
    let j = lab.app(lab.stack().journal(id));
    assert_eq!(
        j.last().unwrap(),
        &("QbitStop".to_string(), None),
        "intent journaled, no result"
    );
    restart_and_settle(&mut lab);
    let s = lab.stack();
    // Nothing was mutated: the media copy is still its own inode, the torrent still runs.
    assert_eq!(ino(&media), before_ino);
    assert_eq!(s.qbit.states()[&hash('4')], "uploading");
    assert_eq!(
        lab.app(s.plan_status(id)),
        removarr_domain::plan::PlanStatus::Interrupted
    );
    let j = lab.app(s.journal(id));
    assert_eq!(
        j.last().unwrap(),
        &("QbitStop".to_string(), Some("Skipped".to_string())),
        "{j:?}"
    );
    assert!(matches!(
        lab.app(s.action_outcomes(id))[0],
        Some(removarr_domain::journal::ActionOutcome::NotExecuted { .. })
    ));
    no_unfinished(&lab);
    no_destructive_delete(s);
}

#[test]
fn crash_after_the_replacement_with_the_recheck_in_flight() {
    let mut lab = Lab::parity(opts());
    let id = start_hanging(&lab, "torrents/recheck");
    let media = lab.stack().p(MEDIA);
    let comp = lab.stack().p(&format!("completed/{CH}/{CH}.mkv"));
    assert_eq!(
        ino(&media),
        ino(&comp),
        "the replacement had happened before the crash"
    );
    assert_eq!(lab.stack().qbit.states()[&hash('4')], "stoppedUP");
    restart_and_settle(&mut lab);
    let s = lab.stack();
    assert_eq!(
        ino(&media),
        ino(&comp),
        "the replaced file is kept (never rolled back)"
    );
    assert_eq!(
        std::fs::read(&media).unwrap(),
        std::fs::read(&comp).unwrap()
    );
    assert_eq!(
        lab.app(s.plan_status(id)),
        removarr_domain::plan::PlanStatus::Interrupted
    );
    // Recovery re-verified the torrent and resumed it, or left it stopped and visible; it
    // never leaves the torrent silently stopped.
    let outcome = lab.app(s.action_outcomes(id)).remove(0).unwrap();
    assert!(
        matches!(
            outcome,
            removarr_domain::journal::ActionOutcome::ExecutedUnverified { .. }
        ),
        "{outcome:?}"
    );
    let j = lab.app(s.journal(id));
    assert!(
        j.contains(&("QbitRecheck".to_string(), Some("Skipped".to_string()))),
        "{j:?}"
    );
    assert_eq!(
        s.qbit.states()[&hash('4')],
        "uploading",
        "rechecked then resumed: {j:?}"
    );
    no_unfinished(&lab);
    no_destructive_delete(s);
}

#[test]
fn crash_after_the_recheck_with_the_start_in_flight() {
    let mut lab = Lab::parity(opts());
    let id = start_hanging(&lab, "torrents/start");
    let media = lab.stack().p(MEDIA);
    let comp = lab.stack().p(&format!("completed/{CH}/{CH}.mkv"));
    assert_eq!(ino(&media), ino(&comp));
    restart_and_settle(&mut lab);
    let s = lab.stack();
    assert_eq!(ino(&media), ino(&comp));
    assert_eq!(
        lab.app(s.plan_status(id)),
        removarr_domain::plan::PlanStatus::Interrupted
    );
    assert_eq!(s.qbit.states()[&hash('4')], "uploading");
    let j = lab.app(s.journal(id));
    assert!(
        j.contains(&("QbitStart".to_string(), Some("Skipped".to_string()))),
        "{j:?}"
    );
    no_unfinished(&lab);
    no_destructive_delete(s);
    // A second restart changes nothing (recovery is idempotent).
    let muts = s.qbit_mutations().len();
    let mut lab = lab;
    restart_and_settle(&mut lab);
    assert_eq!(lab.stack().qbit_mutations().len(), muts);
}

#[test]
fn crash_during_task_removal_after_the_file_was_deleted() {
    let mut lab = Lab::parity(opts());
    let delta = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
    let id = lab.app(async {
        let s = lab.stack();
        s.scan().await;
        let iss = s
            .issue_id(C::HardlinkMissing, &format!("completed:{delta}"))
            .await;
        let p = s
            .create_plan(&PlanRequest::Issues {
                category: C::HardlinkMissing,
                issue_ids: vec![iss],
                action: IssueAction::Delete,
                remove_torrent_tasks: true,
                include_specials: false,
            })
            .await;
        s.approve(&p).await;
        p.id
    });
    lab.stack()
        .qbit
        .set_path_delay("torrents/delete", Duration::from_secs(3600));
    lab.app(async {
        lab.stack()
            .app
            .runtime
            .submit(JobKind::ExecutePlan {
                plan_id: id,
                simulate: false,
            })
            .await
            .unwrap();
    });
    lab.wait_for("delete request", |s| {
        !s.qbit.requests_to("torrents/delete").is_empty()
    });
    let file = lab.stack().p(&format!("completed/{delta}"));
    assert!(
        !file.exists(),
        "the file deletion had happened before the crash"
    );
    restart_and_settle(&mut lab);
    let s = lab.stack();
    assert!(!file.exists());
    assert_eq!(
        lab.app(s.plan_status(id)),
        removarr_domain::plan::PlanStatus::Interrupted
    );
    // Recovery never completes a task removal on its own: the delete was sent once (and lost),
    // the task is still there, and the half-finished plan needs review.
    let m = s.qbit_mutations();
    assert_eq!(
        m.iter().filter(|r| r.starts_with("delete ")).count(),
        1,
        "{m:?}"
    );
    assert!(s.qbit.has_torrent(&hash('5')));
    let out = lab.app(s.action_outcomes(id));
    assert!(
        matches!(
            out[0],
            Some(removarr_domain::journal::ActionOutcome::ExecutedUnverified { .. })
        ),
        "{out:?}"
    );
    assert!(
        matches!(
            out[1],
            Some(removarr_domain::journal::ActionOutcome::NotExecuted { .. })
        ),
        "{out:?}"
    );
    no_unfinished(&lab);
    no_destructive_delete(s);
}
