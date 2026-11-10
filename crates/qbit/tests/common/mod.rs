#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::time::Duration;

use removarr_config::{LiveMode, PathMapper, RootPaths, Roots, Secret};
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::TorrentHash;
use removarr_qbit::{QbitController, QbitReader, QbitSettings, RetryPolicy};
use removarr_testkit::fake_qbit::{FAKE_PASSWORD, FAKE_USERNAME, FakeQbit};
use tempfile::TempDir;

pub const NOW: i64 = 1_800_000_000;

pub struct Env {
    pub _tmp: TempDir,
    pub paths: PathMapper,
}

pub fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for d in ["completed", "incomplete", "media1"] {
        fs::create_dir(base.join(d)).unwrap();
    }
    let p = |d: &str| base.join(d).to_str().unwrap().to_string();
    let roots = Roots::validate(&RootPaths {
        completed: Some(p("completed")),
        incomplete: Some(p("incomplete")),
        media: vec![p("media1")],
    })
    .unwrap();
    let mut rules = std::collections::BTreeMap::new();
    rules.insert(
        ServiceKind::Qbittorrent,
        vec![
            ("/data/completed".to_string(), p("completed")),
            ("/data/incomplete".to_string(), p("incomplete")),
            ("/data/media".to_string(), p("media1")),
        ],
    );
    let paths = PathMapper::new(&roots, &rules, &[]).unwrap();
    Env { _tmp: tmp, paths }
}

/// Settings tuned for fast local tests: no read retries, short timeouts.
pub fn settings(fake: &FakeQbit, env: &Env) -> QbitSettings {
    let mut s = QbitSettings::new(
        fake.url().parse().unwrap(),
        FAKE_USERNAME.to_string(),
        Secret::new(FAKE_PASSWORD),
        env.paths.clone(),
    );
    s.retry = RetryPolicy {
        max_retries: 0,
        base_delay: Duration::from_millis(1),
    };
    s.connect_timeout = Duration::from_secs(2);
    s.request_timeout = Duration::from_secs(2);
    s
}

pub fn reader(fake: &FakeQbit, env: &Env) -> QbitReader {
    QbitReader::new(settings(fake, env)).unwrap()
}

pub fn controller(r: &QbitReader) -> QbitController {
    QbitController::new(r, &LiveMode::for_tests())
}

pub fn h(c: char) -> TorrentHash {
    TorrentHash::new(removarr_testkit::fake_qbit::parity_hash(c)).unwrap()
}

pub fn set(hs: &[char]) -> BTreeSet<TorrentHash> {
    hs.iter().map(|c| h(*c)).collect()
}
