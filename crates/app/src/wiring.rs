//! Composition root (architecture 2.2, 6; ADR-0003).
//!
//! Readers are always built. Mutating adapters are built **only here**, and only from the
//! `LiveMode` token that `Config::live_mode()` returns when `dry_run = false`; in a dry-run
//! process only the recording set exists. `dry_run` and `auto_mode` are read once, here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use removarr_arr::{ArrCommander, ArrReader, ArrSettings};
use removarr_config::Config;
use removarr_domain::arr::ArrService;
use removarr_domain::error::PortError;
use removarr_domain::fs::SampleSpec;
use removarr_domain::path::RootId;
use removarr_domain::plan::PlanMode;
use removarr_domain::policy::Capability;
use removarr_domain::ports::{
    ArrCommands, ArrRead, Clock, ContentReader, FsInventory, IdGen, JellyfinCommands, JellyfinRead,
    MediaProbe, QbitControl, QbitRead,
};
use removarr_domain::session::ServiceKind;
use removarr_fs::{ConfinedFs, FsLimits, LiveFsMutator};
use removarr_jellyfin::{JellyfinCommander, JellyfinReader};
use removarr_jobs::backoff::RandomJitter;
use removarr_jobs::workflow::{WorkflowDeps, WorkflowHandler};
use removarr_jobs::{Deps, JobRuntime, JobsConfig, LimitsConfig, RetryConfig, ScheduleConfig};
use removarr_matching::{
    ContentVerifier, DurationTolerance, FfprobeProbe, FfprobeSettings, MatchSettings, ReadLimiter,
};
use removarr_qbit::{QbitController, QbitReader};
use removarr_reporting::{LogBuffer, Reporter};
use removarr_storage::{Db, Repos};
use removarr_web::auth::{Auth, AuthSettings, AuthSetupError};
use removarr_web::{AppState, FailClosedGuard, Guard, WebSettings};
use removarr_workflows::actions::{self, ActionPorts, ActionSettings, Mutators};
use removarr_workflows::evaluate::VerifyPorts;
use removarr_workflows::issues::IssueSettings;
use removarr_workflows::observe::{ObserveConfig, ObservePorts};
use removarr_workflows::plans::{ALL_CAPABILITIES, PlanPorts, PlanSettings, Planner};

use crate::services::{
    ArrSearch, DbReadiness, Executing, PendingAuto, PlannerControl, ReporterModel, RuntimeJobs,
    StoreHistory,
};
use crate::system::{SystemClock, UuidV7};

/// Database file name inside `server.data_dir` (config SSD, never a root).
pub const DB_FILE: &str = "removarr.db";
/// Poll interval of a qBittorrent recheck (handoff 13).
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(2);

/// A startup failure after the configuration was accepted (exit code 1). The message names
/// the component, never a value.
#[derive(Debug)]
pub enum StartupError {
    Storage(PortError),
    Adapter(&'static str, PortError),
}

impl std::error::Error for StartupError {}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Storage(e) => write!(f, "database could not be opened ({:?})", e.class),
            StartupError::Adapter(what, e) => {
                write!(f, "{what} adapter could not be built ({:?})", e.class)
            }
        }
    }
}

/// What the wiring built, for startup logs and guard tests (architecture 6.8(a)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiringReport {
    pub mode: PlanMode,
    /// True only when live mutating adapters exist in this process.
    pub live_adapters: bool,
    pub auto_mode: bool,
    pub services: Vec<ServiceKind>,
    pub capabilities: Vec<Capability>,
}

/// Everything `serve` needs.
pub struct App {
    pub state: AppState,
    pub runtime: Arc<JobRuntime>,
    pub db: Db,
    pub report: WiringReport,
    pub probe: Arc<FfprobeProbe>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

fn adapter<T>(
    what: &'static str,
    r: Option<Result<T, PortError>>,
) -> Result<Option<T>, StartupError> {
    r.transpose().map_err(|e| StartupError::Adapter(what, e))
}

fn usize_of(v: u32) -> usize {
    usize::try_from(v).unwrap_or(1)
}

/// Builds every component from a validated configuration. `guard` is the authentication
/// extension point (`FailClosedGuard` until auth-guard provides one).
pub async fn build(
    cfg: &Config,
    logs: LogBuffer,
    guard: Arc<dyn Guard>,
) -> Result<App, StartupError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGen> = Arc::new(UuidV7);

    // Storage (config SSD).
    let db_path = cfg.server.data_dir.join(DB_FILE);
    let db = Db::open(&db_path).await.map_err(StartupError::Storage)?;
    let repos = Repos::new(&db, clock.clone());
    let reporter = Reporter::open(&db_path, clock.clone())
        .await
        .map_err(StartupError::Storage)?;

    // Readers (always).
    let confined = Arc::new(ConfinedFs::new(
        cfg.roots(),
        FsLimits {
            content_read_concurrency: cfg.limits.content_read_concurrency,
        },
    ));
    let fs: Arc<dyn FsInventory> = confined.clone();
    let content: Arc<dyn ContentReader> = confined;
    let qbit_reader = adapter("qBittorrent", QbitReader::from_config(cfg))?;
    let mut arr_readers: Vec<(ArrService, ArrReader)> = Vec::new();
    for svc in [ArrService::Sonarr, ArrService::Radarr] {
        if let Some(settings) = ArrSettings::from_config(cfg, svc) {
            let r = ArrReader::new(&settings, cfg.paths.clone(), clock.clone())
                .map_err(|e| StartupError::Adapter(svc.as_str(), e))?;
            arr_readers.push((svc, r));
        }
    }
    let jellyfin_reader = adapter("Jellyfin", JellyfinReader::from_config(cfg))?;
    let qbit: Option<Arc<dyn QbitRead>> = qbit_reader
        .clone()
        .map(|r| Arc::new(r) as Arc<dyn QbitRead>);
    let arr: Vec<Arc<dyn ArrRead>> = arr_readers
        .iter()
        .map(|(_, r)| Arc::new(r.clone()) as Arc<dyn ArrRead>)
        .collect();
    let jellyfin: Option<Arc<dyn JellyfinRead>> =
        jellyfin_reader.map(|r| Arc::new(r) as Arc<dyn JellyfinRead>);
    let root_paths: BTreeMap<RootId, PathBuf> = cfg
        .roots()
        .all()
        .map(|r| (r.id.clone(), PathBuf::from(&r.path)))
        .collect();
    let mut ff = FfprobeSettings::new(cfg.matching.ffprobe_path.clone(), root_paths);
    ff.concurrency = usize_of(cfg.limits.ffprobe_concurrency);
    let probe = Arc::new(FfprobeProbe::new(ff));
    let verifier = ContentVerifier::new(
        ReadLimiter::new(usize_of(cfg.limits.content_read_concurrency)),
        u32::try_from(cfg.matching.compare_chunk_bytes).unwrap_or(u32::MAX),
    );

    // Mutators: the only place a LiveMode token is used (architecture 6.4, 6.8(c)).
    let arr_services: Vec<ArrService> = arr_readers.iter().map(|(s, _)| *s).collect();
    let live = cfg.live_mode();
    let mutators = match &live {
        Some(token) => {
            let qbit_ctl = qbit_reader
                .as_ref()
                .map(|r| Arc::new(QbitController::new(r, token)) as Arc<dyn QbitControl>);
            let arr_cmd: Vec<(ArrService, Arc<dyn ArrCommands>)> = arr_readers
                .iter()
                .map(|(s, r)| {
                    (
                        *s,
                        Arc::new(ArrCommander::new(r.clone(), &cfg.capabilities, token))
                            as Arc<dyn ArrCommands>,
                    )
                })
                .collect();
            let jf_cmd = adapter("Jellyfin", JellyfinCommander::from_config(cfg, token))?
                .map(|c| Arc::new(c) as Arc<dyn JellyfinCommands>);
            Mutators::live(
                Arc::new(LiveFsMutator::new(cfg.roots(), token)),
                qbit_ctl,
                arr_cmd,
                jf_cmd,
            )
        }
        None => Mutators::recording(&arr_services).0,
    };
    let mode = mutators.mode;

    // Settings read once from the configuration.
    let capabilities: std::collections::BTreeSet<Capability> = ALL_CAPABILITIES
        .into_iter()
        .filter(|c| cfg.capabilities.is_enabled(*c))
        .collect();
    let set = |v: &[String]| v.iter().cloned().collect::<std::collections::BTreeSet<_>>();
    let issue_settings = IssueSettings {
        expected_owner: (cfg.owner.uid, cfg.owner.gid),
        incomplete_stalled_days: cfg.thresholds.incomplete_stalled_days,
        video_extensions: set(&cfg.files.video_extensions),
        junk_extensions: set(&cfg.files.junk_extensions),
        subtitle_extensions: set(&cfg.files.subtitle_extensions),
    };
    let match_settings = MatchSettings {
        video_extensions: set(&cfg.files.video_extensions),
        similarity_threshold: cfg.matching.similarity_threshold,
        sample: SampleSpec {
            size: u32::try_from(cfg.matching.sample_size_bytes).unwrap_or(u32::MAX),
            count: cfg.matching.sample_count,
        },
        ..MatchSettings::default()
    };
    let plan_settings = PlanSettings {
        mode,
        config_fingerprint: cfg.fingerprint(),
        plan_ttl_hours: cfg.thresholds.plan_ttl_hours,
        capabilities: capabilities.clone(),
        implemented_tier: actions::IMPLEMENTED_TIER,
        process_uid: rustix::process::geteuid().as_raw(),
        auto_min_age_hours: cfg.thresholds.auto_min_age_hours,
        duration_tolerance: DurationTolerance {
            seconds: cfg.matching.duration_tolerance_seconds,
            ratio: cfg.matching.duration_tolerance_ratio,
        },
        files: issue_settings.clone(),
    };
    let plan_ports = || PlanPorts {
        scans: Arc::new(repos.scans.clone()),
        snapshots: Arc::new(repos.snapshots.clone()),
        matches: Arc::new(repos.matches.clone()),
        issues: Arc::new(repos.issues.clone()),
        ignores: Arc::new(repos.ignores.clone()),
        plans: Arc::new(repos.plans.clone()),
        arr: arr.clone(),
        probe: Some(probe.clone() as Arc<dyn MediaProbe>),
        clock: clock.clone(),
        ids: ids.clone(),
    };

    // Job runtime (handoff 14).
    let observe = ObservePorts {
        fs: fs.clone(),
        content: content.clone(),
        qbit: qbit.clone(),
        arr: arr.clone(),
        jellyfin: jellyfin.clone(),
        scans: Arc::new(repos.scans.clone()),
        snapshots: Arc::new(repos.snapshots.clone()),
        matches: Arc::new(repos.matches.clone()),
        issues: Arc::new(repos.issues.clone()),
        ignores: Arc::new(repos.ignores.clone()),
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let http_concurrency = usize_of(cfg.limits.http_concurrency_per_service);
    let observe_cfg = ObserveConfig {
        roots: cfg.roots().all().map(|r| r.id.clone()).collect(),
        settings: issue_settings,
        match_settings,
        compare_chunk_bytes: u32::try_from(cfg.matching.compare_chunk_bytes).unwrap_or(u32::MAX),
        content_read_concurrency: usize_of(cfg.limits.content_read_concurrency),
        http_concurrency,
    };
    let action_ports = ActionPorts {
        plans: Arc::new(repos.plans.clone()),
        executing: Arc::new(Executing(repos.plans.clone())),
        journal: Arc::new(repos.journal.clone()),
        gains: Arc::new(repos.gains.clone()),
        scans: Arc::new(repos.scans.clone()),
        snapshots: Arc::new(repos.snapshots.clone()),
        fs: fs.clone(),
        content: content.clone(),
        qbit: qbit.clone(),
        arr: arr.clone(),
        jellyfin,
        clock: clock.clone(),
        ids: ids.clone(),
        ignores: Some(Arc::new(repos.ignores.clone())),
    };
    let action_settings = ActionSettings {
        config_fingerprint: cfg.fingerprint(),
        capabilities: capabilities.clone(),
        // The job runner sets it per call, only for an automatic cycle (handoff 14, F06).
        auto_mode_active: false,
        auto_min_age_hours: cfg.thresholds.auto_min_age_hours,
        recheck_timeout: Duration::from_secs(u64::from(cfg.thresholds.recheck_timeout_seconds)),
        recheck_interval: RECHECK_INTERVAL,
        verifier,
    };
    let handler = Arc::new(WorkflowHandler::new(WorkflowDeps {
        observe,
        observe_cfg,
        planner: Planner::new(plan_ports(), plan_settings.clone()),
        actions: action_ports,
        mutators,
        action_settings,
        verify: VerifyPorts {
            issues: Arc::new(repos.issues.clone()),
            matches: Arc::new(repos.matches.clone()),
            fs,
            content,
            clock: clock.clone(),
        },
        history: Arc::new(StoreHistory {
            plans: repos.plans.clone(),
            journal: repos.journal.clone(),
        }),
        pending: Arc::new(PendingAuto(repos.plans.clone())),
    }));
    let anchor = &cfg.scan.anchor_utc;
    let jobs_cfg = JobsConfig {
        schedule: ScheduleConfig {
            scan_interval_minutes: cfg.scan.interval_minutes,
            anchor_minute_of_day: u32::from(anchor.hour) * 60 + u32::from(anchor.minute),
            qbit_refresh_minutes: cfg.scan.qbit_refresh_minutes,
        },
        limits: LimitsConfig {
            content_read_concurrency: usize_of(cfg.limits.content_read_concurrency),
            ffprobe_concurrency: usize_of(cfg.limits.ffprobe_concurrency),
            http_concurrency_per_service: http_concurrency,
        },
        retry: RetryConfig::default(),
        auto_mode: cfg.mode.auto_mode,
    };
    let runtime = Arc::new(JobRuntime::new(
        jobs_cfg,
        Deps {
            store: Arc::new(repos.jobs.clone()),
            state: Arc::new(repos.state.clone()),
            handler,
            clock: clock.clone(),
            ids: ids.clone(),
            jitter: Arc::new(RandomJitter),
        },
    ));

    // Web state.
    let mut services = Vec::new();
    if qbit.is_some() {
        services.push(ServiceKind::Qbittorrent);
    }
    for s in &arr_services {
        services.push(match s {
            ArrService::Sonarr => ServiceKind::Sonarr,
            ArrService::Radarr => ServiceKind::Radarr,
        });
    }
    if cfg.services.jellyfin.is_some() {
        services.push(ServiceKind::Jellyfin);
    }
    let state = AppState {
        read: Arc::new(ReporterModel(reporter)),
        jobs: Arc::new(RuntimeJobs(runtime.clone())),
        plans: Arc::new(PlannerControl {
            planner: Planner::new(plan_ports(), plan_settings),
            issues: Arc::new(repos.issues.clone()),
            arr: arr.clone(),
            clock: clock.clone(),
            http_concurrency,
        }),
        media: Arc::new(ArrSearch { arr: arr.clone() }),
        ignores: Arc::new(repos.ignores.clone()),
        readiness: Arc::new(DbReadiness(repos.state.clone())),
        logs,
        guard,
        clock,
        settings: Arc::new(WebSettings {
            dry_run: cfg.mode.dry_run,
            auto_mode: cfg.mode.auto_mode,
            scan_interval_minutes: cfg.scan.interval_minutes,
            hide_recent_days: cfg.thresholds.hide_recent_days,
            arr_grab: capabilities.contains(&Capability::ArrGrab),
            configured_services: services.clone(),
        }),
    };
    let report = WiringReport {
        mode,
        live_adapters: live.is_some(),
        auto_mode: cfg.mode.auto_mode,
        services,
        capabilities: capabilities.into_iter().collect(),
    };
    Ok(App {
        state,
        runtime,
        db,
        report,
        probe,
    })
}

/// A guard that refuses every protected route (tests and wiring checks). `serve` uses
/// [`auth_guard`].
#[must_use]
pub fn default_guard() -> Arc<dyn Guard> {
    Arc::new(FailClosedGuard)
}

/// The production guard (`removarr_web::auth::Auth`) over the validated `auth` settings and
/// trusted proxies. An unusable password hash or session key refuses startup (fail closed).
pub fn auth_guard(cfg: &Config) -> Result<Arc<dyn Guard>, AuthSetupError> {
    let auth = Auth::new(AuthSettings::from_config(cfg), Arc::new(SystemClock))?;
    Ok(Arc::new(auth))
}
