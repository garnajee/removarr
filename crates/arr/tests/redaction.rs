//! The API key and service URLs never appear in errors or logs (INV-31).
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use removarr_config::Secret;
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::ports::ArrRead;
use removarr_testkit::fake_arr::{FAKE_API_KEY, Fault, PLANTED_PASSKEY};

fn assert_clean(e: &PortError, c: &Ctx) {
    let text = format!("{e} {e:?}");
    assert!(!text.contains(FAKE_API_KEY), "{text}");
    assert!(!text.contains(&c.fake.base_url()), "{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(!text.contains('?'), "no query string: {text}");
    assert!(!text.contains(PLANTED_PASSKEY), "{text}");
}

#[tokio::test]
async fn every_error_kind_is_redacted() {
    let c = sonarr(&[]).await;
    let r = c.reader();
    let cases: Vec<(Fault, ErrorClass)> = vec![
        (
            Fault {
                status: Some(500),
                ..Fault::default()
            },
            ErrorClass::Unavailable,
        ),
        (
            Fault {
                status: Some(404),
                ..Fault::default()
            },
            ErrorClass::NotFound,
        ),
        (
            Fault {
                status: Some(400),
                ..Fault::default()
            },
            ErrorClass::Rejected,
        ),
        (
            Fault {
                status: Some(403),
                ..Fault::default()
            },
            ErrorClass::Forbidden,
        ),
        (
            Fault {
                raw_body: Some(format!("not json {FAKE_API_KEY}")),
                ..Fault::default()
            },
            ErrorClass::Rejected,
        ),
        (
            Fault {
                redirect_to: Some(format!(
                    "http://elsewhere.invalid/api?apikey={FAKE_API_KEY}"
                )),
                ..Fault::default()
            },
            ErrorClass::Rejected,
        ),
    ];
    for (fault, class) in cases {
        c.fake.set_fault("*", fault);
        let e = r
            .manual_import_preview(&completed("some folder"), None)
            .await
            .unwrap_err();
        assert_eq!(e.class, class, "{e}");
        assert_clean(&e, &c);
    }
    assert!(
        c.fake.violations().is_empty(),
        "the key never travels in a query"
    );
    assert!(c.fake.recorded().iter().all(|r| r.key_ok));
}

#[tokio::test]
async fn wrong_key_timeout_oversize_and_unreachable_are_redacted() {
    let c = sonarr(&[]).await;

    let mut s = c.settings();
    s.api_key = Secret::new("wrong-key-but-still-secret-123456");
    let e = c.reader_with(s).status().await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Unauthorized);
    assert!(!format!("{e:?}").contains("wrong-key-but-still-secret-123456"));
    assert_clean(&e, &c);

    let mut s = c.settings();
    s.request_timeout = Duration::from_millis(100);
    s.read_retries = 0;
    c.fake.set_fault(
        "system",
        Fault {
            delay: Some(Duration::from_secs(2)),
            ..Fault::default()
        },
    );
    let e = c.reader_with(s).status().await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Unavailable);
    assert!(e.detail.contains("timed out"), "{e}");
    assert_clean(&e, &c);
    c.fake.clear_faults();

    let mut s = c.settings();
    s.max_response_bytes = 64;
    let e = c.reader_with(s).library().await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Rejected);
    assert!(e.detail.contains("exceeds"), "{e}");
    assert_clean(&e, &c);

    // A closed port: bind, learn the address, drop the listener.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut s = c.settings();
    s.base_url = format!("http://{addr}/sonarr/").parse().unwrap();
    s.read_retries = 0;
    let e = c.reader_with(s).status().await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Unavailable);
    assert!(!format!("{e:?}").contains(&addr.to_string()));
    assert_clean(&e, &c);
}

#[tokio::test]
async fn debug_output_never_shows_the_key() {
    let c = sonarr(&super_caps()).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    for text in [
        format!("{r:?}"),
        format!("{cmd:?}"),
        format!("{:?}", c.settings()),
    ] {
        assert!(!text.contains(FAKE_API_KEY), "{text}");
    }
}

fn super_caps() -> Vec<&'static str> {
    ALL_CAPS.to_vec()
}
