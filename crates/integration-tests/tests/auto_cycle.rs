//! Task 7 (automatic cycle): automatic mode on, a scheduled scan (the only trigger), the real
//! `WorkflowHandler`, the loopback fakes. Real time: the schedule has minute granularity, so
//! each scenario waits up to one minute for the scheduled scan.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use removarr_integration_tests::*;

fn opts() -> Options {
    Options {
        radarr: true,
        auto_mode: true,
        ..Options::live()
    }
    .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
    .env("REMOVARR_SCAN_INTERVAL_MINUTES", "60")
    .env("REMOVARR_SCAN_ANCHOR_UTC", &next_minute())
}

/// `HH:MM` UTC of the next minute boundary (two minutes ahead when it is too close): the
/// scheduled scan happens at most about a minute after the stack starts.
fn next_minute() -> String {
    let now = now_secs();
    let mut m = (now % 86_400) / 60 + 1;
    if 60 - now % 60 < 3 {
        m += 1;
    }
    let m = m % 1440;
    format!("{:02}:{:02}", m / 60, m % 60)
}

async fn wait_until(what: &str, secs: u64, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..secs * 10 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn auto_cycle_consolidates_only_the_proven_copy_and_changes_nothing_else() {
    let s = Stack::parity(opts()).await;
    let before = world(&s.fx);
    let comp = s.p(&format!("completed/{0}/{0}.mkv", fx::CHARLIE));
    let media = s.p("medias/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv");
    wait_until("the scheduled cycle to link the copy", 100, async || {
        ino(&media) == ino(&comp)
    })
    .await;
    s.wait_idle().await;

    // Only that pair changed (the media path took the completed inode, whose link count grew;
    // the media directory's mtime moved with the atomic rename).
    let after = world(&s.fx);
    let changed: Vec<_> = after
        .keys()
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect();
    assert_eq!(changed.len(), 3, "{changed:?}");
    assert!(changed.iter().all(|k| k.contains("Charlie")), "{changed:?}");
    assert_eq!(after.len(), before.len(), "nothing was created or deleted");
    // The exact-copy pairs that must not run automatically were left alone: the Bravo torrent
    // T_B2 reports a 4.x state on a 5.x server (fail closed), November's target directory is not
    // writable.
    assert_ne!(
        ino(&s.p(&fx::bravo_completed())),
        ino(&s.p(&fx::bravo_media()))
    );
    assert_ne!(
        ino(&s.p(&format!("completed/{}.mkv", fx::NOVEMBER))),
        ino(&s.p(&format!(
            "medias/movies/November Movie (2010)/{}.mkv",
            fx::NOVEMBER
        )))
    );
    // No deletion is ever automatic here; qBittorrent saw exactly stop, recheck, start of H4.
    let m = s.qbit_mutations();
    assert_eq!(m.len(), 3, "{m:?}");
    assert!(
        m.iter()
            .all(|r| r.contains(&hash('4')) && !r.starts_with("delete "))
    );
    // Persisted: automatic origin, executed and verified; automatic mode neither paused nor off.
    let plans = s
        .app
        .state
        .read
        .plans(&Default::default())
        .await
        .unwrap()
        .rows;
    let executed: Vec<_> = plans.iter().filter(|p| p.status == "executed").collect();
    assert_eq!(executed.len(), 1, "{plans:?}");
    assert!(plans.iter().all(|p| p.origin == "auto_policy"));
    assert_eq!(executed[0].actions_done, 1);
    let st = s.app.runtime.status();
    assert!(st.auto_enabled && st.auto_paused.is_none(), "{st:?}");
    assert_eq!(s.radarr.as_ref().unwrap().mutating_count(), 0);
}

#[tokio::test]
async fn auto_cycle_pauses_on_an_incomplete_scan_and_stays_paused_across_a_restart() {
    let fxt = removarr_testkit::fixture::ParityFixture::build().unwrap();
    fxt.apply_v2().unwrap(); // an unreadable completed directory: the scan is incomplete
    let s = Stack::over(fxt, opts()).await;
    let before = world(&s.fx);
    wait_until(
        "the scheduled scan to pause automatic mode",
        100,
        async || s.app.runtime.status().auto_paused.is_some(),
    )
    .await;
    s.wait_idle().await;
    let reason = s.app.runtime.status().auto_paused.unwrap();
    assert!(!reason.is_empty());
    assert_eq!(world(&s.fx), before, "nothing was mutated");
    assert!(s.qbit_mutations().is_empty());
    let plans = s
        .app
        .state
        .read
        .plans(&Default::default())
        .await
        .unwrap()
        .rows;
    assert!(
        plans.is_empty(),
        "no automatic plan was even created: {plans:?}"
    );

    // The pause is persisted: a new process is still paused, and nothing resumes it by itself.
    let s = s.restart().await;
    assert_eq!(
        s.app.runtime.status().auto_paused.as_deref(),
        Some(reason.as_str())
    );
    // Only the explicit command clears it.
    let (st, ..) = s.post("/auto/resume", "").await;
    assert!(st.is_redirection(), "{st}");
    assert!(s.app.runtime.status().auto_paused.is_none());
}

#[tokio::test]
async fn auto_cycle_in_a_dry_run_process_simulates_and_mutates_nothing() {
    let mut o = opts();
    o.dry_run = true;
    let s = Stack::parity(o).await;
    let before = world(&s.fx);
    wait_until("the scheduled scan and cycle", 100, async || {
        s.app.runtime.status().jobs.iter().any(|j| {
            j.label.starts_with("auto:") && j.status == removarr_domain::jobs::JobStatus::Succeeded
        })
    })
    .await;
    s.wait_idle().await;
    assert_eq!(world(&s.fx), before);
    assert!(s.qbit_mutations().is_empty());
    assert_eq!(s.radarr.as_ref().unwrap().mutating_count(), 0);
    // Whatever the cycle planned stayed simulated: no journaled step is live.
    let plans = s
        .app
        .state
        .read
        .plans(&Default::default())
        .await
        .unwrap()
        .rows;
    for p in &plans {
        let id = removarr_domain::ids::PlanId(p.id.parse().unwrap());
        let j = s.journal(id).await;
        assert!(
            j.iter().all(|(_, r)| r.as_deref() == Some("Simulated")),
            "{j:?}"
        );
    }
}
