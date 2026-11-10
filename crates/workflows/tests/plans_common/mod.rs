#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Plan-engine test support: plan inputs from hand-built observations (issue-engine's
//! `tests/common` builders), settings, ARR facts and a build helper. No I/O.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrFileRef, ArrMediaManagement, ArrMediaRef,
    ArrQualitySnapshot, ArrService, DecisionSource, MediaIdentity, MediaKind,
};
use removarr_domain::ids::{IssueId, PlanId, RawUuid};
use removarr_domain::issue::{Issue, IssueCategory};
use removarr_domain::media::{AudioStream, ProbeReport};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{ActionKind, PlanMode, PlanOrigin, PlannedAction};
use removarr_domain::policy::RiskTier;
use removarr_domain::session::{
    AuditSession, RootStatus, ServiceKind, ServiceState, ServiceStatus, SessionMetrics,
};
use removarr_domain::torrent::MappedPath;
use removarr_workflows::evaluate::ArrFacts;
use removarr_workflows::issues::{Observation, QbitObservation, classify};
use removarr_workflows::plans::{
    ALL_CAPABILITIES, AutoContext, BuildContext, Built, IssueAction, PlanError, PlanInput,
    PlanRequest, PlanSettings, build_plan,
};

use std::sync::Arc;

use removarr_domain::ports::{ArrRead, IssueStore, MatchStore, ScanStore, SnapshotStore};
use removarr_domain::session::SessionStatus;
use removarr_storage::{Db, Repos};
use removarr_testkit::clock::FakeClock;
use removarr_testkit::ids::SeqIds;
use removarr_workflows::plans::{PlanPorts, Planner};

use crate::common::{NOW, settings};

pub fn ok(kind: ServiceKind) -> ServiceStatus {
    ServiceStatus {
        service: kind,
        state: ServiceState::Ok {
            version: "1".into(),
        },
        observed_at: NOW,
    }
}

/// The session matching an observation: every root scanned, services from the observation.
pub fn session_of(o: &Observation) -> AuditSession {
    let mut services = vec![match &o.qbit {
        QbitObservation::Available(_) => ok(ServiceKind::Qbittorrent),
        QbitObservation::NotConfigured => ServiceStatus {
            service: ServiceKind::Qbittorrent,
            state: ServiceState::NotConfigured,
            observed_at: NOW,
        },
        QbitObservation::Unavailable { reason } => ServiceStatus {
            service: ServiceKind::Qbittorrent,
            state: ServiceState::Unavailable {
                reason: reason.clone(),
            },
            observed_at: NOW,
        },
    }];
    for a in &o.arr {
        services.push(ServiceStatus {
            service: match a.service {
                ArrService::Sonarr => ServiceKind::Sonarr,
                ArrService::Radarr => ServiceKind::Radarr,
            },
            state: a.state.clone(),
            observed_at: NOW,
        });
    }
    AuditSession {
        id: o.session,
        started_at: NOW,
        finished_at: Some(NOW),
        status: o.session_status,
        roots: o
            .configured_roots
            .iter()
            .map(|r| RootStatus {
                root: r.clone(),
                scanned: true,
                error_count: 0,
                protected_count: 0,
            })
            .collect(),
        services,
        metrics: SessionMetrics::default(),
    }
}

/// Classifies the observation and wraps it as a plan input (every issue available).
pub fn input(o: &Observation) -> PlanInput {
    let c = classify(o, &settings()).expect("classifies");
    PlanInput {
        session: session_of(o),
        observation: o.clone(),
        issues: c.issues,
        arr: BTreeMap::new(),
        search: None,
    }
}

/// Live process, tier 5 implemented, every capability on.
pub fn live() -> PlanSettings {
    let mut s = PlanSettings::new("cfg-1", settings());
    s.mode = PlanMode::Live;
    s.implemented_tier = RiskTier::T5;
    s.capabilities = ALL_CAPABILITIES.into_iter().collect();
    s
}

pub fn ctx(origin: PlanOrigin) -> BuildContext<'static> {
    BuildContext {
        id: PlanId::new(RawUuid::from_u128(42)),
        now: NOW,
        origin,
        auto: None,
    }
}

pub fn build(i: &PlanInput, s: &PlanSettings, req: &PlanRequest) -> Result<Built, PlanError> {
    build_plan(req, i, s, &ctx(PlanOrigin::User))
}

pub fn build_auto(
    i: &PlanInput,
    s: &PlanSettings,
    req: &PlanRequest,
    auto: &AutoContext,
) -> Result<Built, PlanError> {
    build_plan(
        req,
        i,
        s,
        &BuildContext {
            auto: Some(auto),
            ..ctx(PlanOrigin::AutoPolicy)
        },
    )
}

pub fn auto_on() -> AutoContext {
    AutoContext {
        auto_mode: true,
        ..AutoContext::default()
    }
}

pub fn id_of(i: &PlanInput, cat: IssueCategory, subject: &str) -> IssueId {
    issue(i, cat, subject).id.clone()
}

pub fn issue<'a>(i: &'a PlanInput, cat: IssueCategory, subject: &str) -> &'a Issue {
    i.issues
        .iter()
        .find(|x| x.category == cat && x.subject.as_str() == subject)
        .unwrap_or_else(|| {
            panic!(
                "no {cat:?} {subject}; have {:?}",
                i.issues
                    .iter()
                    .map(|x| (x.category.as_str(), x.subject.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

pub fn req(cat: IssueCategory, ids: Vec<IssueId>, action: IssueAction) -> PlanRequest {
    PlanRequest::issues(cat, ids, action)
}

pub fn kinds(b: &Built) -> Vec<ActionKind> {
    b.plan.plan().actions.iter().map(|a| a.kind).collect()
}

pub fn action(b: &Built, kind: ActionKind) -> &PlannedAction {
    b.plan
        .plan()
        .actions
        .iter()
        .find(|a| a.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} in {:?}", kinds(b)))
}

/// Excluded blocker codes of a `NothingPlannable` error or of a built plan.
pub fn excluded_codes(
    r: &Result<Built, PlanError>,
) -> Vec<(String, Vec<removarr_domain::issue::BlockerCode>)> {
    let list = match r {
        Ok(b) => b.plan.plan().excluded.clone(),
        Err(PlanError::NothingPlannable { excluded }) => excluded.clone(),
        Err(e) => panic!("unexpected error {e:?}"),
    };
    list.into_iter()
        .map(|(s, b)| (s.0, b.into_iter().map(|x| x.code).collect()))
        .collect()
}

pub fn has_code(
    r: &Result<Built, PlanError>,
    subject: &str,
    code: &removarr_domain::issue::BlockerCode,
) -> bool {
    excluded_codes(r)
        .iter()
        .any(|(s, c)| s == subject && c.contains(code))
}

// ---- ARR facts ------------------------------------------------------------------------------

pub fn mapped(p: &RootedPath) -> MappedPath {
    MappedPath::Mapped {
        raw: format!("/svc/{}", p.rel),
        path: p.clone(),
    }
}

pub fn file_ref(id: i64, p: &RootedPath, size: u64) -> ArrFileRef {
    ArrFileRef {
        file_id: id,
        path: mapped(p),
        size,
        quality: "Q".into(),
        custom_formats: vec![],
        cf_score: 0,
        cutoff_not_met: false,
        languages: vec![],
    }
}

pub fn decision(
    p: &RootedPath,
    episode_ids: Vec<i64>,
    approved: bool,
    rejections: &[&str],
) -> ArrCandidateDecision {
    ArrCandidateDecision {
        subject: p.to_string(),
        size: 1,
        quality: "Q".into(),
        custom_formats: vec![],
        cf_score: 0,
        episode_ids,
        approved,
        rejections: rejections.iter().map(|s| (*s).to_string()).collect(),
        source: DecisionSource::ManualImportPreview,
    }
}

pub fn arr_media(service: ArrService, id: i64) -> ArrMediaRef {
    ArrMediaRef {
        service,
        media_id: id,
        episode_ids: vec![],
    }
}

pub fn arr_snapshot(
    m: &ArrMediaRef,
    files: Vec<ArrFileRef>,
    mm: Option<ArrMediaManagement>,
    score: i64,
) -> ArrQualitySnapshot {
    ArrQualitySnapshot {
        service: m.service,
        service_version: "4".into(),
        media: MediaIdentity {
            kind: if m.service == ArrService::Radarr {
                MediaKind::Movie
            } else {
                MediaKind::Episode
            },
            arr: Some(m.clone()),
            external_ids: BTreeMap::new(),
            title: "T".into(),
            year: Some(2020),
            season: None,
            episode: None,
            absolute: None,
        },
        profile_id: 1,
        profile_name: "HD".into(),
        cutoff: "HD".into(),
        upgrade_allowed: true,
        min_format_score: 0,
        cutoff_format_score: 0,
        format_scores: BTreeMap::from([("CF".to_string(), score)]),
        current_files: files,
        candidates: vec![],
        media_management: mm,
        captured_at: NOW,
        fingerprint: String::new(),
    }
    .seal()
    .unwrap()
}

pub fn mm(recycle: Option<MappedPath>, days: Option<u32>, hardlinks: bool) -> ArrMediaManagement {
    ArrMediaManagement {
        recycle_bin: recycle,
        recycle_bin_cleanup_days: days,
        hardlinks_enabled: hardlinks,
        captured_at: NOW,
    }
}

pub fn episode(id: i64, season: i32, ep: i32, file_id: Option<i64>) -> ArrEpisode {
    ArrEpisode {
        id,
        series_id: 1,
        season,
        episode: ep,
        absolute: None,
        scene_absolute: None,
        scene_season: None,
        scene_episode: None,
        has_file: file_id.is_some(),
        file_id,
        monitored: true,
        aired: true,
    }
}

pub fn probe(duration: f64, langs: &[&str]) -> ProbeReport {
    ProbeReport {
        duration_s: Some(duration),
        video: vec![],
        audio: langs
            .iter()
            .map(|l| AudioStream {
                codec: "aac".into(),
                language: Some((*l).to_string()),
                channels: Some(2),
            })
            .collect(),
        subtitles: vec![],
    }
}

pub fn facts(
    snapshot: ArrQualitySnapshot,
    episodes: Vec<ArrEpisode>,
    previews: Vec<(RootedPath, Vec<ArrCandidateDecision>)>,
    probes: Vec<(RootedPath, ProbeReport)>,
) -> ArrFacts {
    let m = snapshot.media.arr.clone().unwrap();
    ArrFacts {
        media: m,
        snapshot,
        episodes,
        previews: previews.into_iter().collect(),
        probes: probes.into_iter().map(|(p, r)| (p, Ok(r))).collect(),
    }
}

pub fn set_of<T: Ord + Clone>(xs: &[T]) -> BTreeSet<T> {
    xs.iter().cloned().collect()
}

// ---- Storage-backed world (real SQLite repositories) -----------------------------------------

pub struct StoreWorld {
    _dir: tempfile::TempDir,
    pub repos: Repos,
    pub clock: Arc<FakeClock>,
    pub ids: Arc<SeqIds>,
}

pub async fn store_world() -> StoreWorld {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("removarr.db")).await.unwrap();
    let clock = Arc::new(FakeClock::new(NOW));
    let repos = Repos::new(&db, clock.clone());
    StoreWorld {
        _dir: dir,
        repos,
        clock,
        ids: Arc::new(SeqIds::new()),
    }
}

/// Stores one observation as finished scan session `n` (what `observe::run_scan` writes).
pub async fn store_obs(w: &StoreWorld, mut o: Observation, n: u128) {
    o.session = removarr_domain::ids::SessionId::new(RawUuid::from_u128(n));
    let mut s = session_of(&o);
    s.started_at = removarr_domain::time::Timestamp(NOW.0 + i64::try_from(n).unwrap());
    s.status = SessionStatus::Running;
    s.finished_at = None;
    w.repos.scans.begin_session(&s).await.unwrap();
    for inv in &o.inventories {
        w.repos.scans.save_inventory(s.id, inv).await.unwrap();
    }
    if let QbitObservation::Available(snap) = &o.qbit {
        w.repos.snapshots.save_torrents(s.id, snap).await.unwrap();
    }
    w.repos
        .snapshots
        .save_service_status(s.id, &s.services)
        .await
        .unwrap();
    w.repos
        .matches
        .replace_for_session(s.id, &o.matches)
        .await
        .unwrap();
    let c = classify(&o, &settings()).unwrap();
    w.repos
        .issues
        .replace_current(s.id, &c.issues)
        .await
        .unwrap();
    s.finish(SessionStatus::Complete, NOW).unwrap();
    w.repos.scans.finish_session(&s).await.unwrap();
}

pub fn store_planner(
    w: &StoreWorld,
    settings: PlanSettings,
    arr: Vec<Arc<dyn ArrRead>>,
) -> Planner {
    let r = &w.repos;
    Planner::new(
        PlanPorts {
            scans: Arc::new(r.scans.clone()),
            snapshots: Arc::new(r.snapshots.clone()),
            matches: Arc::new(r.matches.clone()),
            issues: Arc::new(r.issues.clone()),
            ignores: Arc::new(r.ignores.clone()),
            plans: Arc::new(r.plans.clone()),
            arr,
            probe: None,
            clock: w.clock.clone(),
            ids: w.ids.clone(),
        },
        settings,
    )
}
