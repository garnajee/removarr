//! Adapter tests against the loopback fake Jellyfin (no other network).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use removarr_config::{Config, Env, LiveMode, Secret};
use removarr_domain::arr::{MediaIdentity, MediaKind};
use removarr_domain::error::ErrorClass;
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{JellyfinCommands, JellyfinRead};
use removarr_jellyfin::{
    ExpectedMedia, Finding, HttpSettings, JellyfinCommander, JellyfinReader, PollConfig,
    RecognitionStatus, RefreshPoll, completeness_of, evaluate, wait_for_library_refresh,
};
use removarr_testkit::fake_jellyfin::{
    FAKE_TOKEN, FakeJellyfin, Fault, ambiguous_library, common_libraries, common_library,
};
use serde_json::json;
use url::Url;

const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";
const DUMMY_KEY: &str = "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";

struct Env2 {
    _tmp: tempfile::TempDir,
    cfg: Config,
}

fn env(fake_url: &str, token: &str) -> Env2 {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    for d in ["completed", "media1", "data"] {
        std::fs::create_dir(base.join(d)).unwrap();
    }
    let p = |r: &str| base.join(r).to_str().unwrap().to_string();
    let mut e = Env::new();
    e.insert("REMOVARR_COMPLETED_ROOT".into(), p("completed"));
    e.insert("REMOVARR_MEDIA_ROOTS".into(), p("media1"));
    e.insert("REMOVARR_DATA_DIR".into(), p("data"));
    e.insert("REMOVARR_PASSWORD_HASH".into(), DUMMY_HASH.into());
    e.insert("REMOVARR_SESSION_KEY".into(), DUMMY_KEY.into());
    e.insert("REMOVARR_JELLYFIN_URL".into(), fake_url.into());
    e.insert("REMOVARR_JELLYFIN_API_KEY".into(), token.into());
    e.insert(
        "REMOVARR_JELLYFIN_PATH_MAP".into(),
        format!("/media={}", p("media1")),
    );
    let cfg = Config::from_sources(None, &e).unwrap();
    Env2 { _tmp: tmp, cfg }
}

fn fast() -> HttpSettings {
    HttpSettings {
        read_retries: 0,
        backoff_base: Duration::from_millis(1),
        request_timeout: Duration::from_secs(5),
        ..HttpSettings::default()
    }
}

fn reader(e: &Env2, fake: &FakeJellyfin, s: HttpSettings) -> JellyfinReader {
    JellyfinReader::new(
        &Url::parse(&fake.base_url()).unwrap(),
        &Secret::new(FAKE_TOKEN),
        e.cfg.paths.clone(),
        s,
    )
    .unwrap()
}

fn commander(fake: &FakeJellyfin, s: HttpSettings) -> JellyfinCommander {
    JellyfinCommander::new(
        &Url::parse(&fake.base_url()).unwrap(),
        &Secret::new(FAKE_TOKEN),
        s,
        &LiveMode::for_tests(),
    )
    .unwrap()
}

fn rooted(e: &Env2, rel: &str) -> RootedPath {
    let abs = format!("{}/{rel}", e.cfg.paths.roots().media()[0].path);
    e.cfg.paths.roots().locate(&abs).unwrap()
}

async fn setup() -> (Env2, FakeJellyfin) {
    let fake = FakeJellyfin::with_common_library().await;
    let e = env(&fake.base_url(), FAKE_TOKEN);
    (e, fake)
}

fn expected_episode(title: &str, s: i32, ep: i32) -> ExpectedMedia {
    ExpectedMedia {
        identity: MediaIdentity {
            kind: if s == 0 {
                MediaKind::Special
            } else {
                MediaKind::Episode
            },
            arr: None,
            external_ids: Default::default(),
            title: title.into(),
            year: None,
            season: Some(s),
            episode: Some(ep),
            absolute: None,
        },
        alternate_numbering: vec![],
    }
}

fn expected_movie(title: &str, tmdb: &str) -> ExpectedMedia {
    ExpectedMedia {
        identity: MediaIdentity {
            kind: MediaKind::Movie,
            arr: None,
            external_ids: [("tmdb".to_string(), tmdb.to_string())].into(),
            title: title.into(),
            year: Some(2020),
            season: None,
            episode: None,
            absolute: None,
        },
        alternate_numbering: vec![],
    }
}

// ---- health, version, libraries ----------------------------------------------------------

#[tokio::test]
async fn health_and_version_succeed() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    r.health().await.unwrap();
    assert_eq!(r.status().await.unwrap(), "10.10.3");
    let info = fake
        .recorded()
        .into_iter()
        .find(|x| x.path == "/System/Info")
        .unwrap();
    assert!(info.token_ok, "token travels in X-Emby-Token");
    assert!(info.query.is_empty(), "token must never be in the query");
}

#[tokio::test]
async fn unhealthy_and_error_statuses_map_to_classes() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    fake.set_fault(
        "health",
        Fault {
            raw_body: Some("Unhealthy".into()),
            ..Fault::default()
        },
    );
    assert_eq!(r.health().await.unwrap_err().class, ErrorClass::Unavailable);
    for (code, class) in [
        (401, ErrorClass::Unauthorized),
        (403, ErrorClass::Forbidden),
        (404, ErrorClass::NotFound),
        (429, ErrorClass::RateLimited),
        (503, ErrorClass::Unavailable),
        (400, ErrorClass::Rejected),
    ] {
        fake.set_fault(
            "info",
            Fault {
                status: Some(code),
                ..Fault::default()
            },
        );
        assert_eq!(r.status().await.unwrap_err().class, class, "HTTP {code}");
    }
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let fake = FakeJellyfin::with_common_library().await;
    let e = env(&fake.base_url(), "x");
    let r = JellyfinReader::new(
        &Url::parse(&fake.base_url()).unwrap(),
        &Secret::new("wrong-token"),
        e.cfg.paths.clone(),
        fast(),
    )
    .unwrap();
    assert_eq!(
        r.status().await.unwrap_err().class,
        ErrorClass::Unauthorized
    );
}

#[tokio::test]
async fn libraries_are_mapped_to_roots() {
    let (e, fake) = setup().await;
    fake.set_libraries(common_libraries());
    let libs = reader(&e, &fake, fast()).libraries().await.unwrap();
    assert_eq!(libs.len(), 2);
    assert_eq!(libs[0].name, "Movies");
    assert_eq!(
        libs[0].locations[0].as_rooted(),
        Some(&rooted(&e, "movies"))
    );
}

#[tokio::test]
async fn from_config_uses_configured_service() {
    let (e, fake) = setup().await;
    let r = JellyfinReader::from_config(&e.cfg).unwrap().unwrap();
    assert_eq!(r.status().await.unwrap(), "10.10.3");
    drop(fake);
}

// ---- items: films, episodes, specials, ambiguity -----------------------------------------

#[tokio::test]
async fn items_by_paths_correlates_films_episodes_and_specials() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    let film = rooted(&e, "movies/Film One (2020)/Film One (2020).mkv");
    let ep = rooted(&e, "shows/Show A/Season 01/Show A - S01E02.mkv");
    let special = rooted(&e, "shows/Show A/Specials/Show A - S00E01.mkv");
    let absent = rooted(&e, "shows/Show A/Season 01/Show A - S01E99.mkv");
    let got = r
        .items_by_paths(&[film.clone(), ep.clone(), special.clone(), absent.clone()])
        .await
        .unwrap();
    let f = got[&film].as_ref().unwrap();
    assert_eq!(
        (f.kind.as_str(), f.season, f.episode),
        ("Movie", None, None)
    );
    assert_eq!(f.provider_ids["Tmdb"], "100");
    let x = got[&ep].as_ref().unwrap();
    assert_eq!(
        (x.series.as_deref(), x.season, x.episode),
        (Some("Show A"), Some(1), Some(2))
    );
    let s = got[&special].as_ref().unwrap();
    assert_eq!((s.season, s.episode), (Some(0), Some(1)));
    assert!(got[&absent].is_none());
    assert_eq!(fake.post_count(), 0, "reads never POST");
}

#[tokio::test]
async fn paging_covers_all_pages() {
    let (e, fake) = setup().await;
    let mut items = common_library();
    for i in 0..1200 {
        items.push(
            json!({"Id": format!("f{i}"), "Name": "filler", "Type": "Movie",
                          "Path": format!("/media/filler/{i}.mkv")}),
        );
    }
    items.push(
        json!({"Id": "last", "Name": "Last", "Type": "Movie", "Path": "/media/movies/last.mkv"}),
    );
    fake.set_items(items);
    let p = rooted(&e, "movies/last.mkv");
    let r = reader(&e, &fake, fast());
    let lookup = r.lookup(std::slice::from_ref(&p)).await.unwrap();
    assert_eq!(lookup.matches[&p].len(), 1);
    assert_eq!(lookup.scanned, 6 + 1200 + 1);
    assert_eq!(fake.count("GET", "/Items"), 3);
}

#[tokio::test]
async fn duplicates_and_unmapped_items_are_reported_safely() {
    let (e, fake) = setup().await;
    let mut items = ambiguous_library();
    items
        .push(json!({"Id": "z", "Name": "Elsewhere", "Type": "Movie", "Path": "/other/place.mkv"}));
    items.push(json!({"Id": "nopath", "Name": "Virtual", "Type": "Movie"}));
    fake.set_items(items);
    let dup = rooted(&e, "shows/Show D/dup.mkv");
    let r = reader(&e, &fake, fast());
    let lookup = r.lookup(std::slice::from_ref(&dup)).await.unwrap();
    assert_eq!(lookup.matches[&dup].len(), 2);
    assert_eq!(lookup.unmapped, 1);
    let rep = evaluate(&expected_episode("Show D", 1, 1), &lookup.matches[&dup]);
    assert_eq!(rep.status, RecognitionStatus::Ambiguous);
    assert_eq!(rep.findings, vec![Finding::DuplicateItems { count: 2 }]);
    // The trait never hands out one of two candidates as "the" item.
    assert!(r.items_by_paths(std::slice::from_ref(&dup)).await.unwrap()[&dup].is_none());
}

#[tokio::test]
async fn path_without_mapping_rule_is_a_config_error_not_a_miss() {
    let (e, fake) = setup().await;
    let completed = e
        .cfg
        .paths
        .roots()
        .locate(&format!("{}/x.mkv", e.cfg.paths.roots().completed().path))
        .unwrap();
    let err = reader(&e, &fake, fast())
        .items_by_paths(&[completed])
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Config);
}

#[tokio::test]
async fn recognition_over_common_and_ambiguous_fixtures() {
    let (e, fake) = setup().await;
    let mut items = common_library();
    items.extend(ambiguous_library());
    fake.set_items(items);
    let r = reader(&e, &fake, fast());
    let cases: Vec<(&str, ExpectedMedia, RecognitionStatus, Option<Finding>)> = vec![
        (
            "movies/Film One (2020)/Film One (2020).mkv",
            expected_movie("Film One", "100"),
            RecognitionStatus::Recognized,
            None,
        ),
        (
            "movies/Film One (2020)/Film One (2020).mkv",
            expected_movie("Film One", "999"),
            RecognitionStatus::Mismatch,
            Some(Finding::ProviderIdConflict {
                provider: "tmdb".into(),
            }),
        ),
        (
            "shows/Show A/Season 01/Show A - S01E01.mkv",
            expected_episode("Show A", 1, 1),
            RecognitionStatus::Recognized,
            None,
        ),
        (
            "shows/Show A/Season 01/Show A - S01E03-E04.mkv",
            expected_episode("Show A", 1, 4),
            RecognitionStatus::Recognized,
            Some(Finding::MultiEpisodeFile { start: 3, end: 4 }),
        ),
        (
            "shows/Show A/Specials/Show A - S00E01.mkv",
            expected_episode("Show A", 0, 1),
            RecognitionStatus::Recognized,
            None,
        ),
        (
            "shows/Show A/Specials/Show A - S00E01.mkv",
            expected_episode("Show A", 1, 1),
            RecognitionStatus::Mismatch,
            Some(Finding::RegularInSpecials),
        ),
        (
            "shows/Show A/Season 01/Show A - S01E01.mkv",
            expected_episode("Show A", 0, 5),
            RecognitionStatus::Mismatch,
            Some(Finding::SpecialInRegularSeason { season: 1 }),
        ),
        (
            "shows/Show B/Show B - 05.mkv",
            expected_episode("Show B", 1, 5),
            RecognitionStatus::Mismatch,
            Some(Finding::UnrecognizedVideo),
        ),
        (
            "shows/Show B/Season 01/Show B - S01E09.mkv",
            expected_episode("Show B", 1, 9),
            RecognitionStatus::Mismatch,
            Some(Finding::MissingNumbering),
        ),
        (
            "shows/Show C/Season 01/Show C - S01E13.mkv",
            expected_episode("Show C", 1, 13),
            RecognitionStatus::Mismatch,
            Some(Finding::NumberingMismatch {
                expected: (1, 13),
                actual: (2, 1),
            }),
        ),
        (
            "shows/Show A/Season 01/Show A - S01E77.mkv",
            expected_episode("Show A", 1, 77),
            RecognitionStatus::NotFound,
            Some(Finding::NotInLibrary),
        ),
    ];
    let paths: Vec<RootedPath> = cases.iter().map(|c| rooted(&e, c.0)).collect();
    let lookup = r.lookup(&paths).await.unwrap();
    for (c, p) in cases.iter().zip(&paths) {
        let rep = evaluate(&c.1, &lookup.matches[p]);
        assert_eq!(rep.status, c.2, "{}", c.0);
        if let Some(f) = &c.3 {
            assert!(rep.findings.contains(f), "{}: {:?}", c.0, rep.findings);
        }
    }
    // Alternate (scene) and absolute numbering are ambiguous, never silently accepted.
    let mut alt = expected_episode("Show C", 1, 13);
    alt.alternate_numbering = vec![(2, 1)];
    let p = rooted(&e, "shows/Show C/Season 01/Show C - S01E13.mkv");
    assert_eq!(
        evaluate(&alt, &lookup.matches[&p]).status,
        RecognitionStatus::Ambiguous
    );
    let mut abs = expected_episode("Anime E", 2, 1);
    abs.identity.absolute = Some(25);
    let p = rooted(&e, "shows/Anime E/Anime E - 025.mkv");
    let lk = r.lookup(std::slice::from_ref(&p)).await.unwrap();
    let rep = evaluate(&abs, &lk.matches[&p]);
    assert_eq!(rep.status, RecognitionStatus::Ambiguous);
    assert_eq!(
        rep.findings,
        vec![Finding::AbsoluteNumbering { actual: 25 }]
    );
}

// ---- failures: timeouts, malformed, oversize ---------------------------------------------

#[tokio::test]
async fn timeout_is_unavailable_and_reads_retry_but_refresh_does_not() {
    let (e, fake) = setup().await;
    let slow = HttpSettings {
        request_timeout: Duration::from_millis(100),
        read_retries: 2,
        backoff_base: Duration::from_millis(1),
        ..HttpSettings::default()
    };
    fake.set_fault(
        "info",
        Fault {
            delay: Some(Duration::from_millis(600)),
            ..Fault::default()
        },
    );
    let err = reader(&e, &fake, slow.clone()).status().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert!(err.detail.contains("timed out"));
    assert_eq!(fake.count("GET", "/System/Info"), 3, "1 try + 2 retries");

    fake.set_fault(
        "refresh_library",
        Fault {
            delay: Some(Duration::from_millis(600)),
            ..Fault::default()
        },
    );
    let err = commander(&fake, slow).refresh_library().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert_eq!(
        fake.count("POST", "/Library/Refresh"),
        1,
        "mutations are never retried"
    );
}

#[tokio::test]
async fn connection_refused_is_unavailable_and_leaks_no_host() {
    let (e, fake) = setup().await;
    let url = fake.base_url();
    drop(fake);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = JellyfinReader::new(
        &Url::parse(&url).unwrap(),
        &Secret::new(FAKE_TOKEN),
        e.cfg.paths.clone(),
        fast(),
    )
    .unwrap();
    let err = r.status().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert!(!err.detail.contains("127.0.0.1") && !err.detail.contains(&url));
}

#[tokio::test]
async fn malformed_data_is_rejected_never_guessed() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    let p = rooted(&e, "movies/Film One (2020)/Film One (2020).mkv");
    for body in [
        "{not json",
        "[]",
        "null",
        r#"{"Items":"x"}"#,
        r#"{"Items":[{"Name":"no id"}]}"#,
        r#"{"Items":[{"Id":"a","Type":"Movie","Path":5}]}"#,
    ] {
        fake.set_fault(
            "items",
            Fault {
                raw_body: Some(body.into()),
                ..Fault::default()
            },
        );
        let err = r
            .items_by_paths(std::slice::from_ref(&p))
            .await
            .unwrap_err();
        assert_eq!(err.class, ErrorClass::Rejected, "{body}");
        assert!(
            !err.detail.contains(body),
            "upstream body must not be echoed"
        );
    }
    fake.set_fault(
        "info",
        Fault {
            raw_body: Some(r#"{"ServerName":"x"}"#.into()),
            ..Fault::default()
        },
    );
    assert_eq!(r.status().await.unwrap_err().class, ErrorClass::Rejected);
    fake.set_fault(
        "libraries",
        Fault {
            raw_body: Some(r#"{"a":1}"#.into()),
            ..Fault::default()
        },
    );
    assert_eq!(r.libraries().await.unwrap_err().class, ErrorClass::Rejected);
    fake.set_fault(
        "tasks",
        Fault {
            raw_body: Some("oops".into()),
            ..Fault::default()
        },
    );
    assert_eq!(
        r.library_refresh_running().await.unwrap_err().class,
        ErrorClass::Rejected
    );
}

#[tokio::test]
async fn oversized_response_is_rejected() {
    let (e, fake) = setup().await;
    let small = HttpSettings {
        max_response_bytes: 64,
        ..fast()
    };
    let err = reader(&e, &fake, small)
        .items_by_paths(&[])
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Rejected);
    assert!(err.detail.contains("size limit"));
}

#[tokio::test]
async fn api_failure_makes_verification_incomplete() {
    let (e, fake) = setup().await;
    fake.set_fault(
        "items",
        Fault {
            status: Some(500),
            ..Fault::default()
        },
    );
    let p = rooted(&e, "movies/Film One (2020)/Film One (2020).mkv");
    let res = reader(&e, &fake, fast()).items_by_paths(&[p]).await;
    assert!(res.is_err());
    assert!(!completeness_of(&res).is_complete());
    let ok: Result<(), _> = Ok(());
    assert!(completeness_of(&ok).is_complete());
}

// ---- refresh commands ---------------------------------------------------------------------

#[tokio::test]
async fn refresh_library_accepts_204_and_only_posts_when_called() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    r.status().await.unwrap();
    r.libraries().await.unwrap();
    assert_eq!(fake.post_count(), 0);
    commander(&fake, fast()).refresh_library().await.unwrap();
    assert_eq!(fake.count("POST", "/Library/Refresh"), 1);
    assert!(fake.recorded().last().unwrap().token_ok);
}

#[tokio::test]
async fn refresh_item_uses_safe_literals_and_rejects_bad_ids() {
    let (_e, fake) = setup().await;
    let c = commander(&fake, fast());
    c.refresh_item("abc123DEF").await.unwrap();
    let rec = fake
        .recorded()
        .into_iter()
        .find(|r| r.method == "POST")
        .unwrap();
    assert_eq!(rec.path, "/Items/abc123DEF/Refresh");
    assert_eq!(rec.query["MetadataRefreshMode"], "Default");
    assert_eq!(rec.query["ReplaceAllMetadata"], "false");
    assert_eq!(rec.query["ReplaceAllImages"], "false");
    for bad in ["", "../x", "a/b", "a?b=c", "a b", &"x".repeat(65)] {
        assert_eq!(
            c.refresh_item(bad).await.unwrap_err().class,
            ErrorClass::Validation
        );
    }
    assert_eq!(fake.post_count(), 1, "invalid ids never reach the network");
}

#[tokio::test]
async fn refresh_failures_are_errors_and_errors_are_redacted() {
    let (_e, fake) = setup().await;
    let c = commander(&fake, fast());
    fake.set_refresh_status(500);
    let err = c.refresh_library().await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    fake.set_refresh_status(204);
    fake.set_fault(
        "refresh_item",
        Fault {
            status: Some(401),
            ..Fault::default()
        },
    );
    let err = c.refresh_item("abc").await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Unauthorized);
    for text in [err.detail.clone(), format!("{c:?}"), format!("{err:?}")] {
        assert!(!text.contains(FAKE_TOKEN), "token leaked: {text}");
        assert!(!text.contains("127.0.0.1"), "host leaked: {text}");
    }
    let (e, fake2) = setup().await;
    let r = reader(&e, &fake2, fast());
    let dbg = format!("{r:?}");
    assert!(!dbg.contains(FAKE_TOKEN) && !dbg.contains("127.0.0.1"));
}

// ---- polling -------------------------------------------------------------------------------

fn poll_cfg() -> PollConfig {
    PollConfig {
        timeout: Duration::from_millis(600),
        interval: Duration::from_millis(10),
        start_grace: Duration::from_millis(100),
    }
}

#[tokio::test]
async fn poll_sees_scan_finish() {
    let (e, fake) = setup().await;
    fake.set_task_states(&[false, true, true, false]);
    let r = reader(&e, &fake, fast());
    let out = wait_for_library_refresh(&r, poll_cfg()).await.unwrap();
    assert_eq!(out, RefreshPoll::Completed);
}

#[tokio::test]
async fn poll_is_bounded_when_scan_never_ends() {
    let (e, fake) = setup().await;
    fake.set_task_states(&[true]);
    let r = reader(&e, &fake, fast());
    let out = wait_for_library_refresh(&r, poll_cfg()).await.unwrap();
    assert_eq!(out, RefreshPoll::TimedOut);
}

#[tokio::test]
async fn poll_without_observed_start_is_weaker_than_completed() {
    let (e, fake) = setup().await;
    fake.set_task_states(&[false]);
    let r = reader(&e, &fake, fast());
    let out = wait_for_library_refresh(&r, poll_cfg()).await.unwrap();
    assert_eq!(out, RefreshPoll::CompletedUnobserved);
}

#[tokio::test]
async fn poll_errors_propagate_and_missing_task_is_not_idle() {
    let (e, fake) = setup().await;
    let r = reader(&e, &fake, fast());
    fake.set_task_present(false);
    assert_eq!(
        r.library_refresh_running().await.unwrap_err().class,
        ErrorClass::NotFound
    );
    assert!(wait_for_library_refresh(&r, poll_cfg()).await.is_err());
    fake.set_task_present(true);
    fake.set_fault(
        "tasks",
        Fault {
            status: Some(500),
            ..Fault::default()
        },
    );
    assert!(wait_for_library_refresh(&r, poll_cfg()).await.is_err());
}
