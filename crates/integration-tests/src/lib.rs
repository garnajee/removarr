//! Integration harness (wave 15, agent 22).
//!
//! Real local components, fake external services: the synthetic parity tree of
//! `docs/legacy-parity.md` section 7 on a temporary directory (real hardlinks), a real SQLite
//! database under `<fixture>/appdata`, the production composition root (`removarr::wiring::build`)
//! and the loopback HTTP fakes of `removarr-testkit` for qBittorrent, Sonarr/Radarr and
//! Jellyfin. Every credential is a dummy value, every server binds `127.0.0.1:0`, and the
//! configuration is built from an explicit environment map (nothing is inherited from the
//! process).
//!
//! This crate is test support only; scenarios live in `tests/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt;
use removarr::wiring::{self, App};
use removarr_config::{Config, Env};
use removarr_domain::fs::RootInventory;
use removarr_domain::ids::{IssueId, JobId, PlanId};
use removarr_domain::issue::IssueCategory;
use removarr_domain::jobs::{JobKind, JobStatus};
use removarr_domain::journal::StepOutcome;
use removarr_domain::journal::{ActionOutcome, ActionState};
use removarr_domain::path::RootId;
use removarr_domain::plan::{ActionPlan, ApprovalRequest, PlanStatus};
use removarr_domain::ports::{IssueStore, JournalStore, PlanStore, ScanStore};
use removarr_reporting::{IssueQuery, IssueRowVm, LogBuffer, LogLevel};
use removarr_storage::Repos;
use removarr_testkit::fake_arr::{FAKE_API_KEY, FakeArr};
use removarr_testkit::fake_jellyfin::FakeJellyfin;
use removarr_testkit::fake_qbit::{FAKE_PASSWORD, FakeFile, FakeQbit, FakeTorrent, parity_hash};
use removarr_testkit::fixture::ParityFixture;
use removarr_web::{Access, Denied, Guard, Principal};
use removarr_workflows::plans::{PlanPreview, PlanRequest};
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
use tower::ServiceExt;

/// Dummy credentials (never real).
pub const DUMMY_SESSION_KEY: &str =
    "dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef";
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZHVtbXlzYWx0$ZHVtbXloYXNoZHVtbXloYXNo";
pub const JELLYFIN_KEY: &str = removarr_testkit::fake_jellyfin::FAKE_TOKEN;

/// Accepts every request: the routes under test are exercised without the login flow (the
/// login flow has its own scenarios over a real listener).
#[derive(Debug, Clone, Copy)]
pub struct AllowGuard;

#[async_trait]
impl Guard for AllowGuard {
    async fn check(&self, _a: Access, req: Request) -> Result<(Request, Principal), Denied> {
        Ok((
            req,
            Principal {
                user: "admin".into(),
                csrf_token: "t".into(),
            },
        ))
    }
}

/// Which parts of the stack a scenario needs.
#[derive(Debug, Clone)]
pub struct Options {
    pub dry_run: bool,
    pub auto_mode: bool,
    pub radarr: bool,
    pub sonarr: bool,
    pub jellyfin: bool,
    pub qbit: bool,
    /// Extra environment entries (applied last).
    pub extra: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: true,
            auto_mode: false,
            radarr: false,
            sonarr: false,
            jellyfin: false,
            qbit: true,
            extra: Vec::new(),
        }
    }
}

impl Options {
    #[must_use]
    pub fn live() -> Self {
        Self {
            dry_run: false,
            ..Self::default()
        }
    }
    #[must_use]
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.extra.push((k.into(), v.into()));
        self
    }
}

/// The parity tree, the loopback fakes and the production composition root over them.
pub struct Stack {
    pub fx: ParityFixture,
    pub qbit: FakeQbit,
    pub radarr: Option<FakeArr>,
    pub sonarr: Option<FakeArr>,
    pub jellyfin: Option<FakeJellyfin>,
    pub env: Env,
    pub cfg: Config,
    pub app: App,
    pub repos: Repos,
}

impl std::fmt::Debug for Stack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stack").finish_non_exhaustive()
    }
}

#[must_use]
pub fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

impl Stack {
    /// Parity tree V1 with the parity qBittorrent preset (5.0.4) and the given options.
    pub async fn parity(opts: Options) -> Self {
        let fx = ParityFixture::build().unwrap();
        Self::over(fx, opts).await
    }

    /// Same, over an already prepared tree (variants applied by the caller).
    pub async fn over(fx: ParityFixture, opts: Options) -> Self {
        let qbit = FakeQbit::spawn("v5.0.4").await;
        qbit.set_torrents(parity_torrents(now_secs()));
        let radarr = if opts.radarr {
            Some(FakeArr::radarr().await)
        } else {
            None
        };
        let sonarr = if opts.sonarr {
            Some(FakeArr::sonarr().await)
        } else {
            None
        };
        let jellyfin = if opts.jellyfin {
            Some(FakeJellyfin::with_common_library().await)
        } else {
            None
        };
        Self::assemble(fx, qbit, radarr, sonarr, jellyfin, opts).await
    }

    /// Builds the environment, the configuration and the application from the given fakes.
    pub async fn assemble(
        fx: ParityFixture,
        qbit: FakeQbit,
        radarr: Option<FakeArr>,
        sonarr: Option<FakeArr>,
        jellyfin: Option<FakeJellyfin>,
        opts: Options,
    ) -> Self {
        let s = |p: PathBuf| p.to_str().unwrap().to_string();
        let mut env = Env::new();
        env.insert("REMOVARR_COMPLETED_ROOT".into(), s(fx.completed()));
        env.insert("REMOVARR_INCOMPLETE_ROOT".into(), s(fx.incomplete()));
        env.insert("REMOVARR_MEDIA_ROOTS".into(), s(fx.medias()));
        env.insert("REMOVARR_DATA_DIR".into(), s(fx.appdata()));
        env.insert("REMOVARR_PASSWORD_HASH".into(), DUMMY_HASH.into());
        env.insert("REMOVARR_SESSION_KEY".into(), DUMMY_SESSION_KEY.into());
        let (uid, gid) = fx.creator_ids().unwrap();
        env.insert("REMOVARR_EXPECTED_OWNER".into(), format!("{uid}:{gid}"));
        env.insert("REMOVARR_DRY_RUN".into(), opts.dry_run.to_string());
        env.insert("REMOVARR_AUTO_MODE".into(), opts.auto_mode.to_string());
        env.insert("REMOVARR_VIDEO_EXTENSIONS".into(), ".mkv".into());
        // Scans are driven by the tests; no scheduled work.
        env.insert("REMOVARR_SCAN_INTERVAL_MINUTES".into(), "0".into());
        env.insert("REMOVARR_QBIT_REFRESH_MINUTES".into(), "0".into());
        if opts.qbit {
            env.insert("REMOVARR_QBIT_URL".into(), qbit.url());
            env.insert("REMOVARR_QBIT_USERNAME".into(), "admin".into());
            env.insert("REMOVARR_QBIT_PASSWORD".into(), FAKE_PASSWORD.into());
            env.insert(
                "REMOVARR_QBIT_PATH_MAP".into(),
                format!(
                    "/data/completed={},/data/incomplete={}",
                    s(fx.completed()),
                    s(fx.incomplete())
                ),
            );
        }
        if let Some(r) = &radarr {
            env.insert("REMOVARR_RADARR_URL".into(), r.base_url());
            env.insert("REMOVARR_RADARR_API_KEY".into(), FAKE_API_KEY.into());
            env.insert(
                "REMOVARR_RADARR_PATH_MAP".into(),
                format!("/media={}", s(fx.medias())),
            );
        }
        if let Some(r) = &sonarr {
            env.insert("REMOVARR_SONARR_URL".into(), r.base_url());
            env.insert("REMOVARR_SONARR_API_KEY".into(), FAKE_API_KEY.into());
            env.insert(
                "REMOVARR_SONARR_PATH_MAP".into(),
                format!("/media={}", s(fx.medias())),
            );
        }
        if let Some(j) = &jellyfin {
            env.insert("REMOVARR_JELLYFIN_URL".into(), j.base_url());
            env.insert("REMOVARR_JELLYFIN_API_KEY".into(), JELLYFIN_KEY.into());
        }
        for (k, v) in opts.extra {
            env.insert(k, v);
        }
        let cfg = Config::from_sources(None, &env).unwrap_or_else(|e| panic!("config: {e}"));
        let (app, repos) = Self::start_app(&cfg).await;
        Self {
            fx,
            qbit,
            radarr,
            sonarr,
            jellyfin,
            env,
            cfg,
            app,
            repos,
        }
    }

    async fn start_app(cfg: &Config) -> (App, Repos) {
        let app = wiring::build(
            cfg,
            LogBuffer::new(500, LogLevel::Info),
            Arc::new(AllowGuard),
        )
        .await
        .unwrap();
        let repos = Repos::new(&app.db, Arc::new(removarr::system::SystemClock));
        app.runtime.start().await.unwrap();
        (app, repos)
    }

    /// A new process over the same SQLite file, the same tree and the same fakes: the old
    /// runtime is dropped without a graceful shutdown (a crash), then a fresh composition root
    /// starts (job restoration, then recovery before the next execution).
    pub async fn restart(self) -> Self {
        self.restart_with(|_| {}).await
    }

    /// Like [`Stack::restart`] with a changed environment (configuration drift).
    pub async fn restart_with(self, change: impl FnOnce(&mut Env)) -> Self {
        let Stack {
            fx,
            qbit,
            radarr,
            sonarr,
            jellyfin,
            mut env,
            cfg,
            app,
            repos,
        } = self;
        drop(repos);
        drop(app);
        drop(cfg);
        change(&mut env);
        let cfg = Config::from_sources(None, &env).unwrap_or_else(|e| panic!("config: {e}"));
        let (app, repos) = Self::start_app(&cfg).await;
        Self {
            fx,
            qbit,
            radarr,
            sonarr,
            jellyfin,
            env,
            cfg,
            app,
            repos,
        }
    }

    // ---- jobs ---------------------------------------------------------------------------

    /// Submits a job and waits for it to end.
    pub async fn run_job(&self, kind: JobKind) -> JobStatus {
        let e = self.app.runtime.submit(kind).await.unwrap();
        self.wait_job(e.id).await
    }

    pub async fn wait_job(&self, id: JobId) -> JobStatus {
        for _ in 0..3000 {
            if let Some(p) = self.app.runtime.progress(id)
                && !matches!(p.status, JobStatus::Queued | JobStatus::Running)
            {
                return p.status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id:?} did not finish");
    }

    /// Waits until the runtime holds no queued or running job.
    pub async fn wait_idle(&self) {
        for _ in 0..3000 {
            let st = self.app.runtime.status();
            if st
                .jobs
                .iter()
                .all(|j| !matches!(j.status, JobStatus::Queued | JobStatus::Running))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("jobs did not finish");
    }

    /// A full scan; panics unless it succeeded.
    pub async fn scan(&self) {
        let st = self.run_job(JobKind::Scan).await;
        assert_eq!(st, JobStatus::Succeeded, "scan");
    }

    // ---- issues -------------------------------------------------------------------------

    pub async fn issues(&self, cat: IssueCategory) -> Vec<IssueRowVm> {
        self.app
            .state
            .read
            .issues(&IssueQuery {
                category: Some(cat),
                limit: 1000,
                ..IssueQuery::default()
            })
            .await
            .unwrap()
            .rows
    }

    /// Issue whose rooted paths include `needle` (for example `completed:stray/readme.txt`).
    pub async fn issue_at(&self, cat: IssueCategory, needle: &str) -> IssueRowVm {
        self.try_issue_at(cat, needle)
            .await
            .unwrap_or_else(|| panic!("no {cat:?} issue at {needle}"))
    }

    pub async fn try_issue_at(&self, cat: IssueCategory, needle: &str) -> Option<IssueRowVm> {
        self.issues(cat)
            .await
            .into_iter()
            .find(|r| r.paths.iter().any(|p| p == needle))
    }

    pub async fn issue_id(&self, cat: IssueCategory, needle: &str) -> IssueId {
        let r = self.issue_at(cat, needle).await;
        IssueId(r.id)
    }

    // ---- plans --------------------------------------------------------------------------

    pub async fn create_plan(&self, req: &PlanRequest) -> PlanPreview {
        self.app
            .state
            .plans
            .create(req)
            .await
            .unwrap_or_else(|e| panic!("plan refused: {} {:?}", e.log_detail, e.excluded))
    }

    pub async fn try_create_plan(
        &self,
        req: &PlanRequest,
    ) -> Result<PlanPreview, removarr_web::UseCaseError> {
        self.app.state.plans.create(req).await
    }

    /// Approves with exactly what the preview requires (a user typing the counts).
    pub async fn approve(&self, p: &PlanPreview) {
        self.app
            .state
            .plans
            .approve(
                p.id,
                &ApprovalRequest {
                    plan_hash: p.hash.clone(),
                    confirmed_count: p.required_count,
                    confirmed_torrent_removals: p.required_torrent_removals,
                    acknowledged_samples: p.samples_to_acknowledge.clone(),
                    acknowledged_unrecoverable: p.unrecoverable_to_acknowledge.clone(),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("approval refused: {} {}", e.key, e.log_detail));
    }

    pub async fn execute(&self, id: PlanId, simulate: bool) -> JobStatus {
        self.run_job(JobKind::ExecutePlan {
            plan_id: id,
            simulate,
        })
        .await
    }

    pub async fn plan_status(&self, id: PlanId) -> PlanStatus {
        PlanStore::get(&self.repos.plans, id)
            .await
            .unwrap()
            .unwrap()
            .2
    }

    /// The stored plan hash (hex), as the approval form must send it.
    pub async fn stored_hash(&self, id: PlanId) -> String {
        PlanStore::get(&self.repos.plans, id)
            .await
            .unwrap()
            .unwrap()
            .1
            .0
    }

    pub async fn stored_plan(&self, id: PlanId) -> ActionPlan {
        PlanStore::get(&self.repos.plans, id)
            .await
            .unwrap()
            .unwrap()
            .0
    }

    /// Journal of a plan as `(intent step name, result state name)` in order, from SQLite.
    pub async fn journal(&self, id: PlanId) -> Vec<(String, Option<String>)> {
        JournalStore::for_plan(&self.repos.journal, id)
            .await
            .unwrap()
            .into_iter()
            .map(|(i, r)| {
                (
                    format!("{:?}", i.step),
                    r.map(|r| match r.outcome {
                        StepOutcome::Done { .. } => "Done".to_string(),
                        StepOutcome::Failed { .. } => "Failed".to_string(),
                        StepOutcome::Simulated => "Simulated".to_string(),
                        StepOutcome::Skipped { .. } => "Skipped".to_string(),
                    }),
                )
            })
            .collect()
    }

    /// Per-action states persisted for a plan, in index order.
    pub async fn outcomes(&self, id: PlanId) -> Vec<(u32, ActionState)> {
        self.repos
            .plans
            .actions(id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.index, r.state))
            .collect()
    }

    /// Persisted action outcomes (the typed final result), in index order.
    pub async fn action_outcomes(&self, id: PlanId) -> Vec<Option<ActionOutcome>> {
        self.repos
            .plans
            .actions(id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.outcome)
            .collect()
    }

    /// Replaces the fake's torrents with the parity set after `f` changed it (call before a scan).
    pub fn tweak_torrents(&self, f: impl FnOnce(&mut Vec<FakeTorrent>)) {
        let mut v = parity_torrents(now_secs());
        f(&mut v);
        self.qbit.set_torrents(v);
    }

    /// Every state-changing qBittorrent request seen so far, as `"<endpoint> <body>"`
    /// (login excluded).
    #[must_use]
    pub fn qbit_mutations(&self) -> Vec<String> {
        self.qbit
            .requests()
            .into_iter()
            .filter(|r| r.method == "POST" && !r.path.ends_with("/auth/login"))
            .map(|r| {
                format!(
                    "{} {}",
                    r.path.trim_start_matches("/api/v2/torrents/"),
                    r.body
                )
            })
            .collect()
    }

    pub async fn issue_exists(&self, id: &IssueId) -> bool {
        !IssueStore::get_many(&self.repos.issues, std::slice::from_ref(id))
            .await
            .unwrap()
            .is_empty()
    }

    /// Root id of the single media root (`media-<hash of the canonical path>`).
    #[must_use]
    pub fn media_id(&self) -> RootId {
        self.cfg.roots().media()[0].id.clone()
    }

    /// `media-<hash>:rel`, the rooted form reports and issues use.
    #[must_use]
    pub fn m(&self, rel: &str) -> String {
        format!("{}:{rel}", self.media_id().as_str())
    }

    /// The stored inventory of a root (SQLite), from the last scan.
    pub async fn inventory(&self, root: &RootId) -> RootInventory {
        ScanStore::load_inventory(&self.repos.scans, root)
            .await
            .unwrap()
    }

    // ---- paths --------------------------------------------------------------------------

    #[must_use]
    pub fn p(&self, rel: &str) -> PathBuf {
        self.fx.path(rel)
    }

    // ---- HTTP over the real router ------------------------------------------------------

    pub async fn http(&self, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let resp = removarr_web::build(self.app.state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let (s, h) = (resp.status(), resp.headers().clone());
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (s, h, String::from_utf8_lossy(&body).into_owned())
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, String) {
        self.http(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    pub async fn post(&self, uri: &str, form: &str) -> (StatusCode, HeaderMap, String) {
        self.http(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string()))
                .unwrap(),
        )
        .await
    }
}

// ---- tree snapshots ---------------------------------------------------------------------

/// What a test compares to prove nothing in a tree changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: &'static str,
    pub ino: u64,
    pub size: u64,
    pub nlink: u64,
    pub mode: u32,
    pub mtime: i64,
    pub link: Option<String>,
}

/// Every entry under `dir` (no symlink followed), keyed by the path relative to `base`.
/// Directories that cannot be read are recorded without their children.
#[must_use]
pub fn snapshot(base: &Path, dir: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    walk(base, dir, &mut out);
    out
}

fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Node>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(m) = fs::symlink_metadata(&p) else {
            continue;
        };
        let kind = if m.file_type().is_symlink() {
            "symlink"
        } else if m.is_dir() {
            "dir"
        } else {
            "file"
        };
        out.insert(
            p.strip_prefix(base).unwrap().to_str().unwrap().to_string(),
            Node {
                kind,
                ino: m.ino(),
                size: m.size(),
                nlink: m.nlink(),
                mode: m.permissions().mode() & 0o7777,
                mtime: m.mtime(),
                link: fs::read_link(&p)
                    .ok()
                    .map(|t| t.to_str().unwrap().to_string()),
            },
        );
        if kind == "dir" {
            walk(base, &p, out);
        }
    }
}

/// Snapshot of the three roots and the undeclared sibling (everything but `appdata`).
#[must_use]
pub fn world(fx: &ParityFixture) -> BTreeMap<String, Node> {
    let mut all = BTreeMap::new();
    for d in ["completed", "incomplete", "medias", "outside"] {
        all.extend(snapshot(fx.base(), &fx.base().join(d)));
    }
    all
}

#[must_use]
pub fn ino(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().ino()
}

#[must_use]
pub fn nlink(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().nlink()
}

/// Sets mtime (no symlink follow) `hours` in the past.
pub fn age_hours(p: &Path, hours: u64) {
    let when = SystemTime::now() - Duration::from_secs(hours * 3600);
    let d = when.duration_since(UNIX_EPOCH).unwrap();
    let ts = Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap(),
        tv_nsec: 0,
    };
    utimensat(
        CWD,
        p,
        &Timestamps {
            last_access: ts,
            last_modification: ts,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// `fx.path(rel)` with the `FX` file names of the specification spelled once.
pub mod fx {
    pub const ALPHA: &str = "Alpha.Movie.2020.1080p.WEB.x264-GRP";
    pub const BRAVO: &str = "Bravo.Movie.2021.1080p.BluRay.x265-GRP";
    pub const CHARLIE: &str = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
    pub const NOVEMBER: &str = "November.Movie.2010.1080p.WEB.x264-GRP";
    pub const TANGO: &str = "Tango.Movie.2007.1080p.WEB.x264-GRP";
    pub const FOXTROT: &str = "Foxtrot.Show.S01.1080p.WEB.x264-GRP";
    pub const KILO: &str = "Kilo.Movie.2012.1080p.BluRay.x264-GRP";

    /// FX-04, FX-05.
    pub fn bravo_completed() -> String {
        format!("completed/{BRAVO}.mkv")
    }
    pub fn bravo_media() -> String {
        format!("medias/movies/Bravo Movie (2021)/{BRAVO}.mkv")
    }
}

/// The torrents of `docs/legacy-parity.md` 7.3 with the exact file names and sizes of the
/// tree. (The testkit `parity_preset` keeps nominal 1000-byte sizes and shortened names for
/// T_F, T_C and T_K, so it does not match the files on disk; defect noted in the handoff.)
#[must_use]
pub fn parity_torrents(now: i64) -> Vec<FakeTorrent> {
    const DAY: i64 = 86_400;
    let c = "/data/completed";
    let mk =
        |h: char, name: &str, state: &str, progress: f64, content: &str, files: &[(&str, u64)]| {
            let mut t = FakeTorrent::new(&parity_hash(h), name, state, progress)
                .paths(c, &format!("{c}/{content}"))
                .added_on(now - 10 * DAY);
            for (f, n) in files {
                t = t.file(FakeFile::new(f, *n));
            }
            t
        };
    let a = fx::ALPHA;
    let b = format!("{}.mkv", fx::BRAVO);
    let ch = fx::CHARLIE;
    let delta = "Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv";
    let f = fx::FOXTROT;
    let juliet = "Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv";
    let k = fx::KILO;
    let nov = format!("{}.mkv", fx::NOVEMBER);
    let tango = format!("{}.mkv", fx::TANGO);
    let papa = "Papa.Movie.2008.1080p.WEB.x264-GRP.mkv";
    let a_mkv = format!("{a}/{a}.mkv");
    let a_nfo = format!("{a}/{a}.nfo");
    let c_main = format!("{ch}/{ch}.mkv");
    let c_sample = format!("{ch}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv");
    let c_nfo = format!("{ch}/{ch}.nfo");
    let f1 = format!("{f}/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv");
    let f2 = format!("{f}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv");
    let k_main = format!("{k}/{k}.mkv");
    let k_sample = format!("{k}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv");
    let mut out = vec![
        mk(
            '1',
            a,
            "uploading",
            1.0,
            a,
            &[(&a_mkv, 65536), (&a_nfo, 512)],
        ),
        mk('2', &b, "stalledUP", 1.0, &b, &[(&b, 81920)]),
        mk('3', &b, "pausedUP", 1.0, &b, &[(&b, 81920)]),
        mk(
            '4',
            ch,
            "uploading",
            1.0,
            ch,
            &[(&c_main, 98304), (&c_sample, 8192), (&c_nfo, 512)],
        ),
        mk('5', delta, "stoppedUP", 1.0, delta, &[(delta, 40960)]),
        mk('6', f, "queuedUP", 1.0, f, &[(&f1, 49152), (&f2, 49152)]),
        mk('7', juliet, "checkingUP", 1.0, juliet, &[(juliet, 16384)]),
        mk(
            '8',
            k,
            "forcedUP",
            1.0,
            k,
            &[(&k_main, 65536), (&k_sample, 8192)],
        ),
        mk('9', &nov, "uploading", 1.0, &nov, &[(&nov, 32768)]),
        mk('a', &tango, "uploading", 1.0, &tango, &[(&tango, 24576)]),
        mk('b', papa, "missingFiles", 1.0, papa, &[(papa, 16384)]),
    ];
    out.push(
        FakeTorrent::new(&parity_hash('c'), "Quebec.Movie.2006.mkv", "uploading", 1.0)
            .paths("/srv/other", "/srv/other/Quebec.Movie.2006.mkv")
            .added_on(now - 10 * DAY)
            .file(FakeFile::new("Quebec.Movie.2006.mkv", 1_000)),
    );
    let oscar = "Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv";
    out.push(
        FakeTorrent::new(&parity_hash('d'), oscar, "stalledDL", 0.4)
            .paths("/data/incomplete", &format!("/data/incomplete/{oscar}"))
            .added_on(now - 30 * DAY)
            .file(FakeFile::new(oscar, 8192)),
    );
    out
}

/// `H1`..`Hd` hash of a torrent letter (`'1'`, `'2'`, ..., `'a'`).
#[must_use]
pub fn hash(n: char) -> String {
    parity_hash(n)
}

/// What survives a process crash: the tree, the fakes and the environment (the SQLite file is in
/// `fx.appdata()`).
#[derive(Debug)]
pub struct Surviving {
    pub fx: ParityFixture,
    pub qbit: FakeQbit,
    pub radarr: Option<FakeArr>,
    pub sonarr: Option<FakeArr>,
    pub jellyfin: Option<FakeJellyfin>,
    pub env: Env,
}

impl Stack {
    /// Drops the application state and returns what a crash leaves behind.
    #[must_use]
    pub fn into_surviving(self) -> Surviving {
        let Stack {
            fx,
            qbit,
            radarr,
            sonarr,
            jellyfin,
            env,
            ..
        } = self;
        Surviving {
            fx,
            qbit,
            radarr,
            sonarr,
            jellyfin,
            env,
        }
    }

    /// Starts a fresh composition root over what survived (job restoration, recovery gate).
    pub async fn boot(s: Surviving) -> Self {
        let cfg = Config::from_sources(None, &s.env).unwrap_or_else(|e| panic!("config: {e}"));
        let (app, repos) = Self::start_app(&cfg).await;
        Self {
            fx: s.fx,
            qbit: s.qbit,
            radarr: s.radarr,
            sonarr: s.sonarr,
            jellyfin: s.jellyfin,
            env: s.env,
            cfg,
            app,
            repos,
        }
    }
}

/// A crash laboratory: the fakes live on their own runtime, the application on another that can
/// be killed without any graceful shutdown (all its tasks vanish mid-await, SQLite is left as
/// the OS has it), then a new application starts over the same database.
pub struct Lab {
    fakes: tokio::runtime::Runtime,
    app_rt: Option<tokio::runtime::Runtime>,
    stack: Option<Stack>,
}

impl std::fmt::Debug for Lab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lab").finish_non_exhaustive()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

impl Lab {
    /// Parity tree, live mode, the qBittorrent external prefix declared.
    #[must_use]
    pub fn parity(opts: Options) -> Self {
        let fakes = runtime();
        let (fx, qbit) = fakes.block_on(async {
            let fx = ParityFixture::build().unwrap();
            let qbit = FakeQbit::spawn("v5.0.4").await;
            qbit.set_torrents(parity_torrents(now_secs()));
            (fx, qbit)
        });
        let app_rt = runtime();
        let stack = app_rt.block_on(Stack::assemble(fx, qbit, None, None, None, opts));
        Self {
            fakes,
            app_rt: Some(app_rt),
            stack: Some(stack),
        }
    }

    #[must_use]
    pub fn stack(&self) -> &Stack {
        self.stack.as_ref().unwrap()
    }

    /// Runs application work on the application runtime.
    pub fn app<F: std::future::Future>(&self, f: F) -> F::Output {
        self.app_rt.as_ref().unwrap().block_on(f)
    }

    /// Runs `f` on the fakes runtime (for waiting on the fakes' request logs).
    pub fn wait_for(&self, what: &str, mut cond: impl FnMut(&Stack) -> bool) {
        let s = self.stack.as_ref().unwrap();
        for _ in 0..2000 {
            if cond(s) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    /// Kills the application runtime (no shutdown path runs) and starts a new one.
    pub fn crash_and_restart(&mut self) {
        let stack = self.stack.take().unwrap();
        let surviving = stack.into_surviving();
        self.app_rt.take().unwrap().shutdown_background();
        // Give killed tasks a moment to drop their handles.
        std::thread::sleep(Duration::from_millis(100));
        let rt = runtime();
        let stack = rt.block_on(Stack::boot(surviving));
        self.app_rt = Some(rt);
        self.stack = Some(stack);
    }

    /// Releases hanging requests so that fake handlers end (they are on the fakes runtime).
    pub fn release(&self) {
        self.stack()
            .qbit
            .set_path_delay("torrents/stop", Duration::ZERO);
        self.stack()
            .qbit
            .set_path_delay("torrents/recheck", Duration::ZERO);
        self.stack()
            .qbit
            .set_path_delay("torrents/start", Duration::ZERO);
        self.stack()
            .qbit
            .set_path_delay("torrents/delete", Duration::ZERO);
    }

    /// Keeps the fakes runtime alive for the lab's lifetime.
    #[must_use]
    pub fn fakes_runtime(&self) -> &tokio::runtime::Runtime {
        &self.fakes
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(rt) = self.app_rt.take() {
            rt.shutdown_background();
        }
        self.stack.take();
    }
}

// ---- ARR replacement scenario ("Zulu") ---------------------------------------------------

pub mod zulu {
    //! A movie whose completed release (BluRay, torrent `Hf`) is an ARR-approved upgrade of the
    //! WEB file Radarr currently serves from the media root. Service paths are laid out so that a
    //! single filesystem bridge (`/` -> fixture base) maps them: `/completed/...`, `/medias/...`.
    use super::*;
    use removarr_testkit::fake_arr::{
        ArrData, FsBridge, ImportEffect, media_management, radarr_basic,
    };
    use removarr_testkit::fixture::bytes;

    pub const REL: &str = "Zulu.Movie.2022.1080p.BluRay.x264-GRP";
    pub const NEW_SVC: &str = "/completed/Zulu.Movie.2022.1080p.BluRay.x264-GRP/Zulu.Movie.2022.1080p.BluRay.x264-GRP.mkv";
    pub const OLD_SVC: &str = "/medias/movies/Zulu Movie (2022)/Zulu Movie (2022).mkv";
    pub const DEST_SVC: &str =
        "/medias/movies/Zulu Movie (2022)/Zulu.Movie.2022.1080p.BluRay.x264-GRP.mkv";

    #[must_use]
    pub fn new_path(fx: &ParityFixture) -> PathBuf {
        fx.path(&format!("completed/{REL}/{REL}.mkv"))
    }
    #[must_use]
    pub fn old_path(fx: &ParityFixture) -> PathBuf {
        fx.path("medias/movies/Zulu Movie (2022)/Zulu Movie (2022).mkv")
    }
    #[must_use]
    pub fn dest_path(fx: &ParityFixture) -> PathBuf {
        fx.path(&format!("medias/movies/Zulu Movie (2022)/{REL}.mkv"))
    }

    /// Adds the Zulu files to the parity tree (aged 72 hours).
    pub fn add_files(fx: &ParityFixture) {
        let put = |p: PathBuf, label: &str, n: usize| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, bytes(label, n)).unwrap();
        };
        put(new_path(fx), "Z1", 65_536);
        put(old_path(fx), "Z0", 32_768);
        for p in [
            new_path(fx),
            old_path(fx),
            new_path(fx).parent().unwrap().to_path_buf(),
            old_path(fx).parent().unwrap().to_path_buf(),
            fx.completed(),
            fx.medias().join("movies"),
        ] {
            age_hours(&p, 72);
        }
    }

    #[must_use]
    pub fn torrent(now: i64) -> FakeTorrent {
        FakeTorrent::new(&hash('f'), REL, "uploading", 1.0)
            .paths("/data/completed", &format!("/data/completed/{REL}"))
            .added_on(now - 10 * 86_400)
            .file(FakeFile::new(&format!("{REL}/{REL}.mkv"), 65_536))
    }

    /// Radarr state: movie 7 served by the old WEB file (CF score 10); the new release is an
    /// approved upgrade (score 50). Hardlinks on, optional recycle bin.
    #[must_use]
    pub fn radarr_data(recycle_bin: Option<&str>, hardlinks: bool) -> ArrData {
        let mut d = radarr_basic();
        d.movies = vec![serde_json::json!({
            "id": 7, "title": "Zulu Movie", "year": 2022, "tmdbId": 200_001, "imdbId": "tt0000007",
            "path": "/medias/movies/Zulu Movie (2022)", "monitored": true, "qualityProfileId": 2 })];
        d.movie_files[0]["path"] = OLD_SVC.into();
        d.movie_files[0]["relativePath"] = "Zulu Movie (2022).mkv".into();
        d.movie_files[0]["size"] = 32_768.into();
        d.queue = vec![];
        d.history = vec![];
        let folder = format!("/completed/{REL}");
        let mut row = d.manual_import.values().next().unwrap()[0].clone();
        row["path"] = NEW_SVC.into();
        row["size"] = 65_536.into();
        row["downloadId"] = hash('f').to_uppercase().into();
        d.manual_import = std::collections::BTreeMap::from([(folder, vec![row])]);
        d.media_management = media_management(recycle_bin, 7, hardlinks);
        d
    }

    /// The stack: parity tree plus Zulu, live, Radarr with the bridge and the import
    /// destination configured.
    pub async fn stack(
        recycle_bin: Option<&str>,
        hardlinks: bool,
        effect: ImportEffect,
        extra: Options,
    ) -> Stack {
        let fx = ParityFixture::build().unwrap();
        add_files(&fx);
        let qbit = FakeQbit::spawn("v5.0.4").await;
        let mut t = parity_torrents(now_secs());
        t.push(torrent(now_secs()));
        qbit.set_torrents(t);
        let radarr = FakeArr::start(radarr_data(recycle_bin, hardlinks)).await;
        radarr.set_fs_bridge(FsBridge {
            service_prefix: "/".into(),
            local_dir: fx.base().to_path_buf(),
        });
        radarr.set_import_destination(NEW_SVC, DEST_SVC);
        radarr.set_import_effect(effect);
        let mut o = extra;
        o.radarr = true;
        let map = format!(
            "/medias={},/completed={}",
            fx.medias().display(),
            fx.completed().display()
        );
        // A stand-in `ffprobe` (a shell script): both files report the same cut and audio.
        let ffprobe = fx.base().join("ffprobe");
        fs::write(
            &ffprobe,
            "#!/bin/sh\necho '{\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":1920,\"height\":1080},{\"codec_type\":\"audio\",\"codec_name\":\"aac\",\"channels\":2,\"tags\":{\"language\":\"eng\"}}],\"format\":{\"duration\":\"7200.0\"}}'\n",
        )
        .unwrap();
        fs::set_permissions(&ffprobe, fs::Permissions::from_mode(0o755)).unwrap();
        let o = o
            .env("REMOVARR_FFPROBE_PATH", ffprobe.to_str().unwrap())
            .env("REMOVARR_CAP_ARR_MANUAL_IMPORT", "true")
            .env("REMOVARR_QBIT_EXTERNAL_PREFIXES", "/srv/other")
            .env("REMOVARR_RADARR_PATH_MAP", &map);
        // `assemble` creates its own Radarr only through `over`; here the fake is ours.
        Stack::assemble(fx, qbit, Some(radarr), None, None, o).await
    }
}
