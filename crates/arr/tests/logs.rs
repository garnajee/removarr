//! Log capture lives in its own test binary: tracing caches callsite interest per process,
//! so parallel tests without a subscriber could hide events from this one.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use removarr_domain::ports::{ArrCommands, ArrRead};
use removarr_testkit::fake_arr::{FAKE_API_KEY, Fault, PLANTED_PASSKEY};
use serde_json::json;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn logs_never_contain_the_key_or_planted_secrets() {
    let buf = Buf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let c = sonarr(ALL_CAPS).await;
    let r = c.reader();
    let cmd = c.commander(&r);
    c.fake.with_data(|d| {
        d.queue[0]["errorMessage"] = json!(format!("echo apikey={FAKE_API_KEY}"));
    });
    let _ = r.status().await;
    let _ = r.library().await;
    let q = r.queue().await.unwrap();
    assert!(!format!("{q:?}").contains(FAKE_API_KEY));
    let _ = r.search_releases(&sonarr_ref(1, &[101])).await;
    let _ = cmd.rescan_all().await;
    let _ = cmd.remove_queue_item_keep_media(424_242).await;
    c.fake.set_fault(
        "*",
        Fault {
            status: Some(500),
            ..Fault::default()
        },
    );
    let _ = r.queue().await;
    let _ = cmd.rescan_all().await;

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("arr request"), "logging was captured: {out}");
    assert!(out.contains("arr mutating request"), "{out}");
    assert!(!out.contains(FAKE_API_KEY), "{out}");
    assert!(!out.contains(PLANTED_PASSKEY), "{out}");
    assert!(!out.contains(&c.fake.base_url()), "{out}");
}
