#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use removarr_domain::error::ErrorClass;
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{CancelFlag, QbitControl, QbitRead};
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{MappedPath, RecheckStatus, TorrentState};
use removarr_qbit::{PathIndex, QbitReader, RecheckWait, RetryPolicy};
use removarr_testkit::fake_qbit::{
    FAKE_PASSWORD, FakeFile, FakeQbit, FakeTorrent, LoginMode, RecheckScript,
    parity_download_path_torrent, parity_escape_torrent, parity_hash, parity_preset,
};

async fn fake5() -> FakeQbit {
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(parity_preset(NOW, false));
    f
}

fn rooted(root: &str, rel: &str) -> RootedPath {
    RootedPath::parse(root, rel).unwrap()
}

#[tokio::test]
async fn login_and_version_5x_and_4x() {
    let e = env();
    let f5 = fake5().await;
    let r = reader(&f5, &e);
    assert_eq!(r.version().await.unwrap(), "v5.0.4");
    let f4 = FakeQbit::spawn("v4.6.7").await;
    assert_eq!(reader(&f4, &e).version().await.unwrap(), "v4.6.7");
}

#[tokio::test]
async fn login_accepts_ok_empty_and_204() {
    let e = env();
    for mode in [
        LoginMode::Ok200,
        LoginMode::Empty200,
        LoginMode::NoContent204,
    ] {
        let f = fake5().await;
        f.set_login(mode);
        let r = reader(&f, &e);
        assert_eq!(r.torrents().await.unwrap().len(), 13, "{mode:?}");
        assert!(f.requests().iter().any(|q| q.has_sid), "{mode:?}");
    }
}

#[tokio::test]
async fn rejected_login_is_unauthorized_blocks_mutation_and_is_not_hammered() {
    let e = env();
    for mode in [
        LoginMode::Fails200,
        LoginMode::Forbidden403,
        LoginMode::Garbage200,
    ] {
        let f = fake5().await;
        f.set_login(mode);
        let r = reader(&f, &e);
        let c = controller(&r);
        assert_eq!(
            r.torrents().await.unwrap_err().class,
            ErrorClass::Unauthorized,
            "{mode:?}"
        );
        assert_eq!(
            c.stop(&set(&['1'])).await.unwrap_err().class,
            ErrorClass::Unauthorized
        );
        assert_eq!(f.requests_to("/auth/login").len(), 1, "{mode:?}: cooldown");
        assert_eq!(f.mutation_count(), 0);
    }
}

#[tokio::test]
async fn wrong_password_does_not_leak_into_error() {
    let e = env();
    let f = fake5().await;
    let mut s = settings(&f, &e);
    s.password = removarr_config::Secret::new("wrong-password-xyz");
    let err = QbitReader::new(s).unwrap().version().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unauthorized);
    assert!(!err.to_string().contains("wrong-password-xyz"));
}

#[tokio::test]
async fn session_expiry_triggers_one_relogin() {
    let e = env();
    let f = fake5().await;
    let r = reader(&f, &e);
    r.torrents().await.unwrap();
    f.expire_sessions();
    r.torrents().await.unwrap();
    assert_eq!(f.requests_to("/auth/login").len(), 2);
}

#[tokio::test]
async fn torrents_are_mapped_and_states_parsed() {
    let e = env();
    let f = fake5().await;
    f.add_torrent(parity_escape_torrent(NOW));
    let list = reader(&f, &e).torrents().await.unwrap();
    let by = |c: char| list.iter().find(|t| t.hash == h(c)).unwrap();
    assert_eq!(
        by('1').content_path.as_rooted().unwrap(),
        &rooted("completed", "Alpha.Movie.2020.1080p.WEB.x264-GRP")
    );
    assert_eq!(by('1').state, TorrentState::Uploading);
    assert_eq!(by('2').state, TorrentState::StalledUp);
    assert_eq!(by('5').state, TorrentState::StoppedUp);
    assert_eq!(by('b').state, TorrentState::MissingFiles);
    assert_ne!(by('2').state, by('5').state);
    // Unmapped torrent (T_Q) and escaping path (T_M).
    assert!(matches!(by('c').save_path, MappedPath::NoRule { .. }));
    assert!(matches!(by('e').save_path, MappedPath::Invalid { .. }));
    assert!(matches!(by('e').content_path, MappedPath::Invalid { .. }));
    // Incomplete torrent: no completion date, mapped to the incomplete root.
    assert_eq!(by('d').completion_on, None);
    assert_eq!(by('d').progress, 0.4);
    assert_eq!(by('1').added_on, Timestamp((NOW - 864_000) * 1000));
}

#[tokio::test]
async fn files_have_candidates_in_dr23_order_with_working_variants() {
    let e = env();
    let f = fake5().await;
    f.add_torrent(parity_download_path_torrent(NOW));
    let r = reader(&f, &e);
    let files = r.files(&h('2')).await.unwrap();
    let c = &files[0].candidates;
    assert_eq!(c.len(), 2, "save and content forms coincide: {c:?}");
    assert_eq!(
        c[0].as_rooted().unwrap(),
        &rooted("completed", "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv")
    );
    assert!(c[1].raw().ends_with(".!qB"));

    let files = r.files(&h('f')).await.unwrap();
    let raws: Vec<&str> = files[0].candidates.iter().map(MappedPath::raw).collect();
    assert_eq!(
        raws,
        [
            "/data/completed/Romeo.Movie/Romeo.Movie.mkv",
            "/data/completed/Romeo.Movie/Romeo.Movie.mkv.!qB",
            "/data/incomplete/Romeo.Movie/Romeo.Movie.mkv",
            "/data/incomplete/Romeo.Movie/Romeo.Movie.mkv.!qB",
        ]
    );
    assert_eq!(r.files(&h('9')).await.unwrap().len(), 1);
    let missing = removarr_domain::torrent::TorrentHash::new("0".repeat(40)).unwrap();
    assert_eq!(
        r.files(&missing).await.unwrap_err().class,
        ErrorClass::NotFound
    );
}

#[tokio::test]
async fn snapshot_lists_every_torrent_with_files() {
    let e = env();
    let f = fake5().await;
    let snap = reader(&f, &e).snapshot(Timestamp(5)).await.unwrap();
    assert_eq!(snap.version, "v5.0.4");
    assert_eq!(snap.major, 5);
    assert_eq!(snap.torrents.len(), 13);
    assert_eq!(snap.files.len(), 13);
    assert_eq!(snap.files[&h('4')].len(), 3);
}

#[tokio::test]
async fn v4_uses_pause_and_resume_only() {
    let e = env();
    let f = FakeQbit::spawn("v4.6.7").await;
    f.set_torrents(parity_preset(NOW, true));
    let r = reader(&f, &e);
    r.torrents().await.unwrap();
    let c = controller(&r);
    c.stop(&set(&['1', '2'])).await.unwrap();
    assert_eq!(f.states()[&parity_hash('1')], "pausedUP");
    c.start(&set(&['1'])).await.unwrap();
    assert_eq!(f.requests_to("/torrents/pause").len(), 1);
    assert_eq!(f.requests_to("/torrents/resume").len(), 1);
    assert!(f.requests_to("/torrents/stop").is_empty());
    assert!(f.requests_to("/torrents/start").is_empty());
    let body = &f.requests_to("/torrents/pause")[0].body;
    assert!(body.starts_with("hashes="), "{body}");
    assert!(body.contains("%7C") || body.contains('|'), "{body}");
}

#[tokio::test]
async fn v5_uses_stop_and_start_only() {
    let e = env();
    let f = fake5().await;
    let r = reader(&f, &e);
    let c = controller(&r);
    c.stop(&set(&['1'])).await.unwrap();
    assert_eq!(f.states()[&parity_hash('1')], "stoppedUP");
    c.start(&set(&['1'])).await.unwrap();
    assert_eq!(f.requests_to("/torrents/stop").len(), 1);
    assert_eq!(f.requests_to("/torrents/start").len(), 1);
    assert!(f.requests_to("/torrents/pause").is_empty());
    assert!(f.requests_to("/torrents/resume").is_empty());
    // Empty set: no call at all.
    let before = f.requests().len();
    c.stop(&set(&[])).await.unwrap();
    assert_eq!(f.requests().len(), before);
}

#[tokio::test]
async fn upgraded_server_is_rediscovered_after_404() {
    let e = env();
    let f = FakeQbit::spawn("v4.6.7").await;
    f.set_torrents(parity_preset(NOW, true));
    let r = reader(&f, &e);
    let c = controller(&r);
    r.version().await.unwrap();
    f.set_version("v5.0.4");
    c.stop(&set(&['1'])).await.unwrap();
    assert_eq!(f.requests_to("/torrents/pause").len(), 1);
    assert_eq!(f.requests_to("/torrents/stop").len(), 1);
}

#[tokio::test]
async fn unsupported_or_garbage_version_blocks_mutation() {
    let e = env();
    let f = FakeQbit::spawn("v6.0.0").await;
    f.set_torrents(parity_preset(NOW, false));
    let r = reader(&f, &e);
    assert_eq!(r.version().await.unwrap(), "v6.0.0");
    assert_eq!(
        controller(&r).stop(&set(&['1'])).await.unwrap_err().class,
        ErrorClass::Rejected
    );
    assert_eq!(f.mutation_count(), 0);

    let g = FakeQbit::spawn("<html>proxy</html>").await;
    let r = reader(&g, &e);
    assert_eq!(r.version().await.unwrap_err().class, ErrorClass::Rejected);
    assert_eq!(
        controller(&r).stop(&set(&['1'])).await.unwrap_err().class,
        ErrorClass::Rejected
    );
}

#[tokio::test]
async fn remove_always_sends_delete_files_false() {
    let e = env();
    for version in ["v4.6.7", "v5.0.4"] {
        let f = FakeQbit::spawn(version).await;
        f.set_torrents(parity_preset(NOW, false));
        let r = reader(&f, &e);
        let c = controller(&r);
        c.remove_tasks_keep_data(&set(&['2', '3'])).await.unwrap();
        let reqs = f.requests_to("/torrents/delete");
        assert_eq!(reqs.len(), 1, "{version}");
        let body = &reqs[0].body;
        assert_eq!(body.matches("deleteFiles").count(), 1, "{body}");
        assert!(body.contains("deleteFiles=false"), "{body}");
        assert!(!body.contains("deleteFiles=true"), "{body}");
        assert!(!f.has_torrent(&parity_hash('2')) && !f.has_torrent(&parity_hash('3')));
        assert!(f.has_torrent(&parity_hash('1')));
    }
}

#[tokio::test]
async fn mapping_escape_and_unmapped_block_mutation_of_that_torrent_only() {
    let e = env();
    let f = fake5().await;
    f.add_torrent(parity_escape_torrent(NOW));
    let r = reader(&f, &e);
    let c = controller(&r);
    r.torrents().await.unwrap();
    assert!(r.mapping_uncertain(&h('e')));
    assert!(r.mapping_uncertain(&h('c')));
    assert!(!r.mapping_uncertain(&h('1')));
    for bad in ['e', 'c'] {
        let err = c.stop(&set(&['1', bad])).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Rejected, "{bad}");
        assert_eq!(
            c.remove_tasks_keep_data(&set(&[bad]))
                .await
                .unwrap_err()
                .class,
            ErrorClass::Rejected
        );
    }
    assert_eq!(f.mutation_count(), 0);
    c.stop(&set(&['1'])).await.unwrap();
}

#[tokio::test]
async fn file_name_escape_blocks_mutation() {
    let e = env();
    let f = FakeQbit::spawn("v5.0.4").await;
    f.set_torrents(vec![
        FakeTorrent::new(&parity_hash('1'), "x", "uploading", 1.0)
            .paths("/data/completed", "/data/completed/x")
            .file(FakeFile::new("x/../../../etc/passwd", 1)),
    ]);
    let r = reader(&f, &e);
    let c = controller(&r);
    let files = r.files(&h('1')).await.unwrap();
    assert!(matches!(files[0].candidates[0], MappedPath::Invalid { .. }));
    assert_eq!(
        c.stop(&set(&['1'])).await.unwrap_err().class,
        ErrorClass::Rejected
    );
    assert_eq!(f.mutation_count(), 0);
}

#[tokio::test]
async fn read_timeout_is_unavailable_and_retried_per_policy() {
    let e = env();
    let f = fake5().await;
    f.set_delay(Duration::from_millis(400));
    let mut s = settings(&f, &e);
    s.request_timeout = Duration::from_millis(100);
    s.retry = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
    };
    let r = QbitReader::new(s).unwrap();
    // Login is exempt from the delay; the first read times out and is retried twice.
    let err = r.torrents().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert_eq!(f.requests_to("/torrents/info").len(), 3);
}

#[tokio::test]
async fn mutation_timeout_is_not_retried_and_reports_unknown_outcome() {
    let e = env();
    let f = fake5().await;
    let mut s = settings(&f, &e);
    s.request_timeout = Duration::from_millis(100);
    s.retry = RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
    };
    let r = QbitReader::new(s).unwrap();
    r.version().await.unwrap();
    let c = controller(&r);
    f.set_path_delay("/torrents/stop", Duration::from_millis(400));
    let err = c.stop(&set(&['1'])).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert!(err.detail.contains("outcome unknown"), "{err}");
    assert_eq!(f.requests_to("/torrents/stop").len(), 1);
}

#[tokio::test]
async fn interrupted_transfer_is_an_error_and_mutation_is_not_repeated() {
    let e = env();
    let f = fake5().await;
    let r = reader(&f, &e);
    r.version().await.unwrap();
    f.abort_response("/torrents/stop");
    let err = controller(&r).stop(&set(&['1'])).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert_eq!(f.requests_to("/torrents/stop").len(), 1);
}

#[tokio::test]
async fn oversize_response_is_rejected() {
    let e = env();
    let f = fake5().await;
    f.set_oversize(Some(50_000));
    let mut s = settings(&f, &e);
    s.max_response_bytes = 4_096;
    let err = QbitReader::new(s).unwrap().torrents().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    assert!(err.detail.contains("size limit"));
}

#[tokio::test]
async fn http_errors_are_classified_and_redacted() {
    let e = env();
    let f = fake5().await;
    let r = reader(&f, &e);
    f.force_response(
        "/torrents/info",
        500,
        &format!("boom password={FAKE_PASSWORD} {FAKE_PASSWORD}"),
    );
    let err = r.torrents().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert!(!err.detail.contains(FAKE_PASSWORD), "{err}");
    f.force_status("/torrents/info", 400);
    assert_eq!(r.torrents().await.unwrap_err().class, ErrorClass::Rejected);
    f.force_response("/torrents/info", 200, "not json secret-token-123");
    let err = r.torrents().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    assert!(!err.detail.contains("secret-token-123"));
}

#[tokio::test]
async fn connection_refused_is_unavailable() {
    let e = env();
    let f = fake5().await;
    let s = settings(&f, &e);
    drop(f);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = QbitReader::new(s).unwrap().version().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
}

fn wait(ms: u64) -> RecheckWait {
    RecheckWait {
        timeout: Duration::from_millis(ms),
        interval: Duration::from_millis(10),
    }
}

#[tokio::test]
async fn recheck_verified_after_checking_observed() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: 2,
        result: None,
    });
    let r = reader(&f, &e);
    let c = controller(&r);
    c.stop(&set(&['5'])).await.unwrap();
    let res = c
        .recheck_and_wait(&set(&['5']), &wait(2000), &CancelFlag::new())
        .await
        .unwrap();
    assert_eq!(res[&h('5')], RecheckStatus::Verified);
    assert_eq!(
        f.states()[&parity_hash('5')],
        "stoppedUP",
        "recheck never resumes"
    );
    assert!(f.requests_to("/torrents/start").is_empty());
}

#[tokio::test]
async fn recheck_with_missing_data_fails_and_stays_stopped() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: 1,
        result: Some(("stoppedDL".into(), 0.5)),
    });
    let r = reader(&f, &e);
    let c = controller(&r);
    let res = c
        .recheck_and_wait(&set(&['5']), &wait(2000), &CancelFlag::new())
        .await
        .unwrap();
    assert_eq!(res[&h('5')], RecheckStatus::Failed);
}

#[tokio::test]
async fn recheck_timeout_reports_timed_out() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: u32::MAX,
        result: None,
    });
    let r = reader(&f, &e);
    let started = std::time::Instant::now();
    let res = controller(&r)
        .recheck_and_wait(&set(&['5']), &wait(150), &CancelFlag::new())
        .await
        .unwrap();
    assert_eq!(res[&h('5')], RecheckStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn recheck_not_observed_is_never_verified_by_elapsed_time() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: 0,
        result: None,
    });
    let r = reader(&f, &e);
    let res = controller(&r)
        .recheck_and_wait(&set(&['5']), &wait(100), &CancelFlag::new())
        .await
        .unwrap();
    assert_eq!(res[&h('5')], RecheckStatus::TimedOut);
}

#[tokio::test]
async fn recheck_polling_is_interruptible_and_never_starts_anything() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: u32::MAX,
        result: None,
    });
    let r = reader(&f, &e);
    let c = controller(&r);
    let cancel = CancelFlag::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        c2.cancel();
    });
    let err = c
        .recheck_and_wait(&set(&['5']), &wait(10_000), &cancel)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Cancelled);
    assert!(f.requests_to("/torrents/start").is_empty());
    assert!(f.requests_to("/torrents/delete").is_empty());
    // Already cancelled: not even the recheck is sent.
    let before = f.requests_to("/torrents/recheck").len();
    assert!(
        c.recheck_and_wait(&set(&['5']), &wait(100), &cancel)
            .await
            .is_err()
    );
    assert_eq!(f.requests_to("/torrents/recheck").len(), before);
}

#[tokio::test]
async fn vanished_torrent_during_recheck_is_failed() {
    let e = env();
    let f = fake5().await;
    f.set_recheck(RecheckScript {
        checking_polls: 5,
        result: None,
    });
    let r = reader(&f, &e);
    let c = controller(&r);
    let c2 = c.clone();
    let hs = set(&['5']);
    let task = tokio::spawn(async move {
        c2.recheck_and_wait(&hs, &wait(3000), &CancelFlag::new())
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    c.remove_tasks_keep_data(&set(&['5'])).await.unwrap();
    let res = task.await.unwrap().unwrap();
    assert_eq!(res[&h('5')], RecheckStatus::Failed);
}

#[tokio::test]
async fn index_keeps_all_hashes_for_shared_files_and_ignores_names() {
    let e = env();
    let f = fake5().await;
    // A same-named torrent saved elsewhere must not match by name.
    f.add_torrent(
        FakeTorrent::new(
            &parity_hash('f'),
            "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
            "uploading",
            1.0,
        )
        .paths(
            "/data/media",
            "/data/media/Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
        )
        .file(FakeFile::new(
            "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv",
            10,
        )),
    );
    let snap = reader(&f, &e).snapshot(Timestamp(1)).await.unwrap();
    let idx = PathIndex::build(&snap);
    let bravo = rooted("completed", "Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv");
    assert_eq!(idx.hashes_for(&bravo), set(&['2', '3']));
    assert!(!idx.is_unreferenced(&bravo));
    let media = RootedPath::new(
        removarr_domain::path::RootId::media(&snap_media_root(&e)),
        removarr_domain::path::RelPath::new("Bravo.Movie.2021.1080p.BluRay.x265-GRP.mkv").unwrap(),
    );
    assert_eq!(idx.hashes_for(&media), set(&['f']));
    // Folder: every hash below.
    let alpha_dir = rooted("completed", "Alpha.Movie.2020.1080p.WEB.x264-GRP");
    assert_eq!(idx.hashes_under(&alpha_dir), set(&['1']));
    assert_eq!(
        idx.hashes_under(&rooted("completed", "")),
        set(&['1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b'])
    );
    // Unknown path, and a path that only shares a name prefix.
    assert!(idx.is_unreferenced(&rooted(
        "completed",
        "Alpha.Movie.2020.1080p.WEB.x264-GRP.mkv"
    )));
    assert!(
        idx.hashes_for(&rooted("completed", "Nothing.mkv"))
            .is_empty()
    );
    // T_Q is unmapped: it owns nothing.
    assert!(!idx.hashes_under(&rooted("completed", "")).contains(&h('c')));
}

fn snap_media_root(e: &Env) -> String {
    e.paths.roots().media()[0].path.clone()
}

#[tokio::test]
async fn index_covers_working_files_and_incomplete_content_dirs() {
    let e = env();
    let f = fake5().await;
    f.add_torrent(parity_download_path_torrent(NOW));
    let multi = FakeTorrent::new(&parity_hash('e'), "Show", "stalledDL", 0.3)
        .paths("/data/completed", "/data/incomplete/Show")
        .download_path("/data/incomplete")
        .file(FakeFile::new("Show/a.mkv", 10))
        .file(FakeFile::new("Show/b.mkv", 10));
    f.add_torrent(multi);
    let snap = reader(&f, &e).snapshot(Timestamp(1)).await.unwrap();
    let idx = PathIndex::build(&snap);
    // `.!qB` variant of the download_path form (single-file torrent).
    assert_eq!(
        idx.hashes_for(&rooted("incomplete", "Romeo.Movie/Romeo.Movie.mkv.!qB")),
        set(&['f'])
    );
    // Single-file torrent: only its own file and the `.parts` files beside it.
    assert!(
        idx.hashes_for(&rooted("incomplete", "Romeo.Movie/extra.bin"))
            .is_empty()
    );
    assert_eq!(
        idx.hashes_for(&rooted("incomplete", "Romeo.Movie/.0123.parts")),
        set(&['f'])
    );
    // Incomplete multi-file torrent: everything below its content dir, in both forms, is owned.
    assert_eq!(
        idx.hashes_for(&rooted("incomplete", "Show/extra.bin")),
        set(&['e'])
    );
    assert_eq!(
        idx.hashes_for(&rooted("completed", "Show/sub/x")),
        set(&['e'])
    );
    // `.parts` files directly beside the content dir.
    assert_eq!(
        idx.hashes_for(&rooted("incomplete", ".abc.parts")),
        set(&['d', 'e'])
    );
    assert!(
        idx.hashes_for(&rooted("incomplete", "other.mkv"))
            .is_empty()
    );
    assert!(
        idx.hashes_for(&rooted("incomplete", "Showcase/a.mkv"))
            .is_empty()
    );
    // Removing the whole incomplete root affects both.
    assert_eq!(
        idx.hashes_under(&rooted("incomplete", "")),
        set(&['e', 'f', 'd'])
    );
}

#[tokio::test]
async fn from_config_reflects_configuration() {
    use removarr_config::{Config, Env as CfgEnv};
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    for d in ["completed", "media", "data"] {
        std::fs::create_dir(base.join(d)).unwrap();
    }
    let p = |d: &str| base.join(d).to_str().unwrap().to_string();
    let mut env = CfgEnv::new();
    env.insert("REMOVARR_COMPLETED_ROOT".into(), p("completed"));
    env.insert("REMOVARR_MEDIA_ROOTS".into(), p("media"));
    env.insert("REMOVARR_DATA_DIR".into(), p("data"));
    env.insert(
        "REMOVARR_PASSWORD_HASH".into(),
        "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo".into(),
    );
    env.insert("REMOVARR_SESSION_KEY".into(), "k".repeat(64));
    let cfg = Config::from_sources(None, &env).unwrap();
    assert!(
        QbitReader::from_config(&cfg).is_none(),
        "not configured is not an error"
    );
    env.insert("REMOVARR_QBIT_URL".into(), "http://127.0.0.1:9".into());
    env.insert("REMOVARR_QBIT_PASSWORD".into(), "pw".into());
    let cfg = Config::from_sources(None, &env).unwrap();
    assert!(QbitReader::from_config(&cfg).unwrap().is_ok());
}

#[tokio::test]
async fn fresh_reader_without_listing_refuses_unmapped_and_unlisted_hashes() {
    let e = env();
    let f = fake5().await;
    f.add_torrent(parity_escape_torrent(NOW));
    // No torrents()/files()/snapshot() call on this reader.
    let c = controller(&reader(&f, &e));
    for bad in ['c', 'e'] {
        for op in 0..4 {
            let s = set(&['1', bad]);
            let r = match op {
                0 => c.stop(&s).await,
                1 => c.start(&s).await,
                2 => c.recheck(&s).await,
                _ => c.remove_tasks_keep_data(&s).await,
            };
            assert_eq!(r.unwrap_err().class, ErrorClass::Rejected, "{bad} op {op}");
        }
    }
    assert_eq!(f.mutation_count(), 0);
    // A hash absent from the current listing is refused too.
    let ghost = removarr_domain::torrent::TorrentHash::new("9".repeat(39) + "0").unwrap();
    let err = c.stop(&[ghost].into_iter().collect()).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::NotFound);
    assert_eq!(f.mutation_count(), 0);
    // A mapped hash still works on a fresh reader.
    c.stop(&set(&['1'])).await.unwrap();
    assert_eq!(f.mutation_count(), 1);
}

#[tokio::test]
async fn errors_never_echo_upstream_body_sid_or_host() {
    let e = env();
    let f = fake5().await;
    let r = reader(&f, &e);
    let host = f.url().replace("http://", "");
    let body = format!("{FAKE_SID_ECHO} {host} {FAKE_PASSWORD}");
    for status in [400u16, 401, 403, 404, 500, 503] {
        f.force_response("/torrents/info", status, &body);
        let err = r.torrents().await.unwrap_err();
        let text = format!("{err} {err:?}");
        for secret in [FAKE_SID_ECHO, host.as_str(), FAKE_PASSWORD, "127.0.0.1"] {
            assert!(!text.contains(secret), "{status}: {text}");
        }
    }
    drop(f);
    tokio::time::sleep(Duration::from_millis(50)).await;
}

const FAKE_SID_ECHO: &str = "SID-echo-0123456789";
