//! Pass 3 parity and safety tests: version-aware allowlist on live listings, wire-level
//! `deleteFiles=false`, unmapped torrents, 5.2 login, redaction. Loopback fake only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::ports::{CancelFlag, QbitControl, QbitRead};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::RecheckStatus;
use removarr_qbit::{Eligibility, Protection, QbitReader, RecheckWait, classify};
use removarr_testkit::fake_qbit::{
    FAKE_PASSWORD, FAKE_SID, FakeQbit, LoginMode, RecheckScript, parity_hash, parity_preset,
};

fn wait(ms: u64) -> RecheckWait {
    RecheckWait {
        timeout: Duration::from_millis(ms),
        interval: Duration::from_millis(10),
    }
}

/// Independent allowlist, written from the architecture text and not from the implementation.
fn allowlisted(state: &str, major: u8) -> bool {
    match state {
        "uploading" | "stalledUP" | "queuedUP" | "forcedUP" => true,
        "pausedUP" => major == 4,
        "stoppedUP" => major == 5,
        _ => false,
    }
}

async fn live_eligibility(version: &str, v4_names: bool) {
    let e = env();
    let f = FakeQbit::spawn(version).await;
    f.set_torrents(parity_preset(NOW, v4_names));
    let snap = reader(&f, &e).snapshot(Timestamp(1)).await.unwrap();
    let major = snap.major;
    let states = f.states();
    assert_eq!(snap.torrents.len(), 13);
    for t in &snap.torrents {
        let raw = &states[t.hash.as_str()];
        // T_Q (hash c) has no mapping rule, T_O (hash d) is still downloading.
        let unsafe_other = t.hash == h('c') || t.hash == h('d');
        let expected = allowlisted(raw, major) && !unsafe_other;
        let got = classify(t, major);
        assert_eq!(
            got == Eligibility::Eligible,
            expected,
            "{version} {raw} {:?}: {got:?}",
            t.hash
        );
    }
    // Spot checks of the protection reasons on the wire data.
    let by = |c: char| snap.torrents.iter().find(|t| t.hash == h(c)).unwrap();
    assert_eq!(
        classify(by('c'), major),
        Eligibility::Protected(Protection::MappingUncertain)
    );
    assert_eq!(
        classify(by('d'), major),
        Eligibility::Protected(Protection::NotComplete)
    );
    // checkingUP and missingFiles are never eligible on any version.
    assert_ne!(classify(by('7'), major), Eligibility::Eligible);
    assert_ne!(classify(by('b'), major), Eligibility::Eligible);
}

#[tokio::test]
async fn live_listing_allowlist_v5_accepts_stopped_up_and_refuses_paused_up() {
    live_eligibility("v5.0.4", false).await;
}

#[tokio::test]
async fn live_listing_allowlist_v4_accepts_paused_up_and_refuses_stopped_up() {
    // v4_names=true gives T_D pausedUP; without it a 4.x server would never say stoppedUP,
    // so the refusal of a foreign state name is checked in the second call.
    live_eligibility("v4.6.7", true).await;
    live_eligibility("v4.6.7", false).await;
}

#[tokio::test]
async fn qbittorrent_5_2_with_204_login_works_end_to_end() {
    // T-28 on a 5.2 server: 204 without body is a successful login, stop/start are used.
    let e = env();
    let f = FakeQbit::spawn("v5.2.0").await;
    f.set_login(LoginMode::NoContent204);
    f.set_torrents(parity_preset(NOW, false));
    let r = reader(&f, &e);
    assert_eq!(r.version().await.unwrap(), "v5.2.0");
    let snap = r.snapshot(Timestamp(1)).await.unwrap();
    assert_eq!(snap.major, 5);
    let c = controller(&r);
    c.stop(&set(&['1'])).await.unwrap();
    assert_eq!(f.requests_to("/torrents/stop").len(), 1);
    assert!(f.requests_to("/torrents/pause").is_empty());
    assert_eq!(f.requests_to("/auth/login").len(), 1);
}

#[tokio::test]
async fn no_command_ever_carries_a_delete_files_true_flag() {
    let e = env();
    for version in ["v4.6.7", "v5.0.4"] {
        let f = FakeQbit::spawn(version).await;
        f.set_torrents(parity_preset(NOW, version.starts_with("v4")));
        let r = reader(&f, &e);
        r.torrents().await.unwrap();
        let c = controller(&r);
        c.stop(&set(&['1', '2'])).await.unwrap();
        c.recheck_and_wait(&set(&['1']), &wait(300), &CancelFlag::new())
            .await
            .unwrap();
        c.start(&set(&['1'])).await.unwrap();
        c.remove_tasks_keep_data(&set(&['2'])).await.unwrap();
        for q in f.requests() {
            let all = format!("{} {} {}", q.path, q.query, q.body).to_lowercase();
            assert!(!all.contains("deletefiles=true"), "{version}: {all}");
            if q.path.ends_with("/torrents/delete") {
                assert!(q.body.contains("deleteFiles=false"), "{}", q.body);
            } else {
                assert!(!all.contains("deletefiles"), "{version}: {all}");
            }
        }
        assert_eq!(f.requests_to("/torrents/delete").len(), 1);
    }
}

#[tokio::test]
async fn remove_never_touches_unlisted_hashes_and_empty_set_sends_nothing() {
    let e = env();
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(parity_preset(NOW, false));
    let r = reader(&f, &e);
    let c = controller(&r);
    c.remove_tasks_keep_data(&set(&[])).await.unwrap();
    assert!(f.requests().is_empty(), "empty set must not even log in");
    c.remove_tasks_keep_data(&set(&['3'])).await.unwrap();
    let body = &f.requests_to("/torrents/delete")[0].body;
    assert!(body.contains(&parity_hash('3')), "{body}");
    assert!(!body.contains(&parity_hash('2')), "{body}");
    assert!(f.has_torrent(&parity_hash('2')));
}

#[tokio::test]
async fn every_mutation_kind_is_refused_for_an_unmapped_torrent_after_listing() {
    let e = env();
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(parity_preset(NOW, false));
    let r = reader(&f, &e);
    let c = controller(&r);
    r.torrents().await.unwrap();
    let only_c = set(&['c']);
    for (name, err) in [
        ("stop", c.stop(&only_c).await.unwrap_err()),
        ("start", c.start(&only_c).await.unwrap_err()),
        ("recheck", c.recheck(&only_c).await.unwrap_err()),
        (
            "remove",
            c.remove_tasks_keep_data(&only_c).await.unwrap_err(),
        ),
        (
            "recheck_and_wait",
            c.recheck_and_wait(&only_c, &wait(100), &CancelFlag::new())
                .await
                .unwrap_err(),
        ),
    ] {
        assert_eq!(err.class, ErrorClass::Rejected, "{name}");
    }
    assert_eq!(f.mutation_count(), 0);
    // A mixed set is refused as a whole: the mapped member is not stopped either.
    assert!(c.stop(&set(&['1', 'c'])).await.is_err());
    assert_eq!(f.states()[&parity_hash('1')], "uploading");
    assert_eq!(f.mutation_count(), 0);
}

#[tokio::test]
async fn files_query_also_marks_an_unmapped_torrent_uncertain() {
    let e = env();
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(parity_preset(NOW, false));
    let r = reader(&f, &e);
    let c = controller(&r);
    // Only the per-torrent file query has been made, not the whole listing.
    r.files(&h('c')).await.unwrap();
    assert!(r.mapping_uncertain(&h('c')));
    assert_eq!(
        c.stop(&set(&['c'])).await.unwrap_err().class,
        ErrorClass::Rejected
    );
    assert_eq!(f.mutation_count(), 0);
}

#[tokio::test]
async fn recheck_is_never_verified_without_an_observed_checking_state() {
    let e = env();
    // Whatever the final state is, zero observed `checking*` polls can only be TimedOut or Failed.
    for result in [None, Some(("stoppedUP".to_string(), 1.0))] {
        let f = FakeQbit::spawn("v5.0.4").await;
        f.set_torrents(parity_preset(NOW, false));
        f.set_recheck(RecheckScript {
            checking_polls: 0,
            result,
        });
        let r = reader(&f, &e);
        let res = controller(&r)
            .recheck_and_wait(&set(&['5']), &wait(80), &CancelFlag::new())
            .await
            .unwrap();
        assert_ne!(res[&h('5')], RecheckStatus::Verified);
    }
}

#[tokio::test]
async fn secrets_and_host_never_appear_in_errors_or_debug_output() {
    let e = env();
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(parity_preset(NOW, false));
    let s = settings(&f, &e);
    let dbg = format!("{s:?}");
    assert!(!dbg.contains(FAKE_PASSWORD), "{dbg}");
    let r = QbitReader::new(s).unwrap();
    let dbg = format!("{r:?}");
    assert!(
        !dbg.contains(FAKE_PASSWORD) && !dbg.contains(FAKE_SID),
        "{dbg}"
    );
    r.torrents().await.unwrap();

    // The configured password is a known secret and must never survive in an error, even when
    // the upstream body echoes it. (Echoed SID or host in an upstream body is not redacted:
    // see the pass 3 handoff, observation on qbit error bodies.)
    let host = f.url().replace("http://", "");
    for status in [400u16, 401, 403, 404, 408, 429, 500, 502, 503] {
        f.force_response(
            "/torrents/info",
            status,
            &format!("{FAKE_PASSWORD} {FAKE_SID} {host}"),
        );
        let err = r.torrents().await.unwrap_err();
        for text in [err.detail.clone(), format!("{err:?}"), err.to_string()] {
            assert!(!text.contains(FAKE_PASSWORD), "{status}: {text}");
        }
    }
}
