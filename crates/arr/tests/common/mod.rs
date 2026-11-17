#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use removarr_arr::{ArrCommander, ArrReader, ArrSettings};
use removarr_config::{Config, Env, LiveMode};
use removarr_domain::arr::{ArrMediaRef, ArrService};
use removarr_domain::path::{RelPath, RootId, RootedPath};
use removarr_testkit::clock::FakeClock;
use removarr_testkit::fake_arr::{FAKE_API_KEY, FakeArr};
use tempfile::TempDir;

/// Dummy values shaped like the real ones. Not secrets.
pub const DUMMY_SESSION_KEY: &str =
    "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";

pub struct Ctx {
    pub tmp: TempDir,
    pub base: PathBuf,
    pub cfg: Config,
    pub fake: FakeArr,
    pub clock: Arc<FakeClock>,
    pub service: ArrService,
}

impl Ctx {
    pub fn p(&self, rel: &str) -> String {
        self.base.join(rel).to_str().unwrap().to_string()
    }

    pub fn settings(&self) -> ArrSettings {
        let mut s = ArrSettings::from_config(&self.cfg, self.service).unwrap();
        s.backoff_base = Duration::from_millis(1);
        s
    }

    pub fn reader(&self) -> ArrReader {
        self.reader_with(self.settings())
    }

    pub fn reader_with(&self, s: ArrSettings) -> ArrReader {
        ArrReader::new(&s, self.cfg.paths.clone(), self.clock.clone()).unwrap()
    }

    pub fn commander(&self, reader: &ArrReader) -> ArrCommander {
        ArrCommander::new(
            reader.clone(),
            &self.cfg.capabilities,
            &LiveMode::for_tests(),
        )
    }

    pub fn media_root(&self) -> RootId {
        RootId::media(&self.p("media1"))
    }
}

pub fn completed(rel: &str) -> RootedPath {
    RootedPath::new(RootId::completed(), RelPath::new(rel).unwrap())
}

pub fn sonarr_ref(series: i64, eps: &[i64]) -> ArrMediaRef {
    ArrMediaRef {
        service: ArrService::Sonarr,
        media_id: series,
        episode_ids: eps.to_vec(),
    }
}

pub fn radarr_ref(movie: i64) -> ArrMediaRef {
    ArrMediaRef {
        service: ArrService::Radarr,
        media_id: movie,
        episode_ids: vec![],
    }
}

async fn ctx(service: ArrService, fake: FakeArr, caps: &[&str]) -> Ctx {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for d in ["completed", "media1", "data"] {
        fs::create_dir(base.join(d)).unwrap();
    }
    let p = |r: &str| base.join(r).to_str().unwrap().to_string();
    let mut env = Env::new();
    env.insert("REMOVARR_COMPLETED_ROOT".into(), p("completed"));
    env.insert("REMOVARR_MEDIA_ROOTS".into(), p("media1"));
    env.insert("REMOVARR_DATA_DIR".into(), p("data"));
    env.insert("REMOVARR_PASSWORD_HASH".into(), DUMMY_HASH.into());
    env.insert("REMOVARR_SESSION_KEY".into(), DUMMY_SESSION_KEY.into());
    let (svc, library) = match service {
        ArrService::Sonarr => ("SONARR", "/tv"),
        ArrService::Radarr => ("RADARR", "/movies"),
    };
    env.insert(format!("REMOVARR_{svc}_URL"), fake.base_url());
    env.insert(format!("REMOVARR_{svc}_API_KEY"), FAKE_API_KEY.into());
    env.insert(
        format!("REMOVARR_{svc}_PATH_MAP"),
        format!("/downloads={},{library}={}", p("completed"), p("media1")),
    );
    for c in caps {
        env.insert(
            format!("REMOVARR_CAP_{}", c.to_ascii_uppercase()),
            "true".into(),
        );
    }
    let cfg = Config::from_sources(None, &env).unwrap();
    Ctx {
        tmp,
        base,
        cfg,
        fake,
        clock: Arc::new(FakeClock::default()),
        service,
    }
}

pub async fn sonarr(caps: &[&str]) -> Ctx {
    ctx(ArrService::Sonarr, FakeArr::sonarr().await, caps).await
}

pub async fn radarr(caps: &[&str]) -> Ctx {
    ctx(ArrService::Radarr, FakeArr::radarr().await, caps).await
}

pub const ALL_CAPS: &[&str] = &[
    "arr_manual_import",
    "arr_grab",
    "arr_queue_remove",
    "arr_set_monitored",
];
