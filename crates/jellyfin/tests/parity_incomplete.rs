//! Pass 3: a Jellyfin API failure is always an error and always "incomplete" verification,
//! never a success or an "unrecognized" miss. Errors never carry the token, the host or the
//! upstream body. Loopback fake only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use removarr_config::{Config, Env, LiveMode, Secret};
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{JellyfinCommands, JellyfinRead};
use removarr_jellyfin::{
    HttpSettings, JellyfinCommander, JellyfinReader, PollConfig, completeness_of,
    wait_for_library_refresh,
};
use removarr_testkit::fake_jellyfin::{FAKE_TOKEN, FakeJellyfin, Fault};
use url::Url;

const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";
const DUMMY_KEY: &str = "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";
const STATUSES: [u16; 10] = [400, 401, 403, 404, 408, 429, 500, 502, 503, 504];

struct Fixture {
    _tmp: tempfile::TempDir,
    cfg: Config,
    fake: FakeJellyfin,
}

async fn fixture() -> Fixture {
    let fake = FakeJellyfin::with_common_library().await;
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
    e.insert("REMOVARR_JELLYFIN_URL".into(), fake.base_url());
    e.insert("REMOVARR_JELLYFIN_API_KEY".into(), FAKE_TOKEN.into());
    e.insert(
        "REMOVARR_JELLYFIN_PATH_MAP".into(),
        format!("/media={}", p("media1")),
    );
    let cfg = Config::from_sources(None, &e).unwrap();
    Fixture {
        _tmp: tmp,
        cfg,
        fake,
    }
}

fn fast() -> HttpSettings {
    HttpSettings {
        read_retries: 0,
        backoff_base: Duration::from_millis(1),
        request_timeout: Duration::from_secs(5),
        ..HttpSettings::default()
    }
}

fn reader(fx: &Fixture) -> JellyfinReader {
    JellyfinReader::new(
        &Url::parse(&fx.fake.base_url()).unwrap(),
        &Secret::new(FAKE_TOKEN),
        fx.cfg.paths.clone(),
        fast(),
    )
    .unwrap()
}

fn film_path(fx: &Fixture) -> RootedPath {
    let abs = format!(
        "{}/movies/Film One (2020)/Film One (2020).mkv",
        fx.cfg.paths.roots().media()[0].path
    );
    fx.cfg.paths.roots().locate(&abs).unwrap()
}

fn assert_clean(fx: &Fixture, text: &str, what: &str) {
    let host = fx.fake.base_url().replace("http://", "");
    assert!(!text.contains(FAKE_TOKEN), "{what}: token leaked: {text}");
    assert!(!text.contains(&host), "{what}: host leaked: {text}");
    assert!(
        !text.contains("upstream-secret"),
        "{what}: body echoed: {text}"
    );
}

#[tokio::test]
async fn every_read_endpoint_failure_is_an_incomplete_error_never_a_miss() {
    let fx = fixture().await;
    let r = reader(&fx);
    let path = film_path(&fx);
    for endpoint in ["items", "libraries", "info", "tasks", "health"] {
        for status in STATUSES {
            fx.fake.set_fault(
                endpoint,
                Fault {
                    status: Some(status),
                    raw_body: None,
                    delay: None,
                },
            );
            let label = format!("{endpoint} {status}");
            match endpoint {
                "items" => {
                    let res = r.items_by_paths(std::slice::from_ref(&path)).await;
                    assert!(!completeness_of(&res).is_complete(), "{label}");
                    assert!(res.is_err(), "{label}: must not be Ok(None)");
                    let res = r.lookup(std::slice::from_ref(&path)).await;
                    assert!(res.is_err(), "{label}");
                    assert_clean(&fx, &res.unwrap_err().detail, &label);
                }
                "libraries" => {
                    let res = r.libraries().await;
                    assert!(!completeness_of(&res).is_complete(), "{label}");
                    assert_clean(&fx, &res.unwrap_err().detail, &label);
                }
                "info" => {
                    let res = r.status().await;
                    assert!(!completeness_of(&res).is_complete(), "{label}");
                    assert_clean(&fx, &res.unwrap_err().detail, &label);
                }
                "tasks" => {
                    let res = r.library_refresh_running().await;
                    assert!(!completeness_of(&res).is_complete(), "{label}");
                    assert_clean(&fx, &res.unwrap_err().detail, &label);
                    // A failing poll is an error, never Completed.
                    let poll = PollConfig {
                        timeout: Duration::from_millis(200),
                        interval: Duration::from_millis(10),
                        start_grace: Duration::from_millis(50),
                    };
                    assert!(wait_for_library_refresh(&r, poll).await.is_err(), "{label}");
                }
                _ => {
                    let res = r.health().await;
                    assert!(!completeness_of(&res).is_complete(), "{label}");
                    assert_clean(&fx, &res.unwrap_err().detail, &label);
                }
            }
        }
        fx.fake.clear_faults();
    }
    // After clearing the faults the same call succeeds: the failures above were the faults.
    assert!(r.items_by_paths(&[path]).await.is_ok());
}

#[tokio::test]
async fn a_hostile_200_body_is_rejected_and_not_echoed() {
    let fx = fixture().await;
    let r = reader(&fx);
    let path = film_path(&fx);
    let hostile = format!("upstream-secret {FAKE_TOKEN} {}", fx.fake.base_url());
    for endpoint in ["items", "libraries", "info", "tasks"] {
        fx.fake.set_fault(
            endpoint,
            Fault {
                status: None,
                raw_body: Some(hostile.clone()),
                delay: None,
            },
        );
        let err = match endpoint {
            "items" => r
                .items_by_paths(std::slice::from_ref(&path))
                .await
                .unwrap_err(),
            "libraries" => r.libraries().await.unwrap_err(),
            "info" => r.status().await.unwrap_err(),
            _ => r.library_refresh_running().await.unwrap_err(),
        };
        assert_clean(&fx, &err.detail, endpoint);
        assert_clean(&fx, &format!("{err:?}"), endpoint);
        fx.fake.clear_faults();
    }
}

#[tokio::test]
async fn refresh_commands_fail_for_every_non_success_status_and_are_not_retried() {
    let fx = fixture().await;
    let c = JellyfinCommander::new(
        &Url::parse(&fx.fake.base_url()).unwrap(),
        &Secret::new(FAKE_TOKEN),
        HttpSettings {
            read_retries: 3,
            backoff_base: Duration::from_millis(1),
            ..fast()
        },
        &LiveMode::for_tests(),
    )
    .unwrap();
    let mut expected_posts = 0;
    for status in [301u16, 302, 400, 401, 403, 404, 429, 500, 502, 503] {
        fx.fake.set_refresh_status(status);
        let err = c.refresh_library().await.unwrap_err();
        expected_posts += 1;
        assert_clean(&fx, &err.detail, &format!("library {status}"));
        let err = c.refresh_item("abc123").await.unwrap_err();
        expected_posts += 1;
        assert_clean(&fx, &format!("{err:?}"), &format!("item {status}"));
        assert_eq!(
            fx.fake.post_count(),
            expected_posts,
            "{status}: a command is sent once, never retried"
        );
    }
    fx.fake.set_refresh_status(204);
    c.refresh_library().await.unwrap();
}
