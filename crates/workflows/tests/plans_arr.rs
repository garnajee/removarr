#![allow(clippy::unwrap_used, clippy::expect_used)]
//! ARR-dependent plans: `DeleteReplaced` (CP1-F02; legacy T-21 all-or-nothing folder),
//! `ReplaceRelease` (coverage, numbering, ffprobe checks, old-coverage recovery; CP1-F20),
//! approved grabs (CP1-F06), and ARR snapshot changes invalidating plans (INV-34).

mod common;
mod plans_common;

use std::collections::BTreeMap;

use common::*;
use plans_common::*;
use removarr_domain::arr::{
    ArrHistoryRecord, ArrService, OldFileRecovery, ReleaseRef, ReleaseSearchResult,
};
use removarr_domain::error::{DomainError, ErrorClass, PortError};
use removarr_domain::evidence::{ContentProof, EvidenceLevel};
use removarr_domain::fs::EntryKind;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{
    ActionKind as K, ActionTarget, ApprovalRequest, PlanMode, PlanStatus, Precondition,
    VerificationStep,
};
use removarr_domain::policy::{Capability, RiskTier};
use removarr_domain::session::ServiceKind;
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{
    IssueAction as A, PlanInput, PlanRequest, StaleReason, preview, staleness, verdict,
};

fn sub(p: &RootedPath) -> String {
    SubjectKey::rooted(p).0
}

// ---- Radarr movie: completed 720p release, library 1080p release ------------------------

const OLD: &str = "Old.Movie.2020.720p/Old.Movie.2020.720p.WEB.x264-GRP.mkv";
const LIB: &str = "movies/Old Movie (2020)/Old.Movie.2020.1080p.BluRay.x264-NEW.mkv";

fn movie_world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .dir("Old.Movie.2020.720p", 2)
        .file(OLD, 10, 1000)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/Old Movie (2020)", 6)
        .file(LIB, 11, 3000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            "Old.Movie.2020.720p",
            &[(OLD, OLD)],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, OLD),
        rp(&m, LIB),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 3000, 1, 0o644),
        ContentProof::None,
    )];
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(ArrService::Radarr, 7, rp(&m, LIB))],
        vec![],
    )];
    o
}

fn movie_facts(
    approved: bool,
    rejections: &[&str],
    lib_size: u64,
) -> removarr_workflows::evaluate::ArrFacts {
    let (c, m) = (completed(), media());
    let movie = arr_media(ArrService::Radarr, 7);
    facts(
        arr_snapshot(
            &movie,
            vec![file_ref(100, &rp(&m, LIB), lib_size)],
            Some(mm(None, None, true)),
            10,
        ),
        vec![],
        vec![(
            rp(&c, "Old.Movie.2020.720p"),
            vec![decision(&rp(&c, OLD), vec![], approved, rejections)],
        )],
        vec![],
    )
}

fn with_facts(
    o: &Observation,
    f: Result<removarr_workflows::evaluate::ArrFacts, PortError>,
) -> PlanInput {
    let mut i = input(o);
    i.arr = BTreeMap::from([((ArrService::Radarr, 7), f)]);
    i
}

fn delete_replaced(i: &PlanInput) -> PlanRequest {
    let id = id_of(i, C::HardlinkMissing, &sub(&rp(&completed(), OLD)));
    req(C::HardlinkMissing, vec![id], A::DeleteReplaced)
}

#[test]
fn delete_replaced_needs_arr_not_upgrade_and_a_present_replacement() {
    let o = movie_world();
    let s = live();
    let subject = sub(&rp(&completed(), OLD));
    // The issue itself is blocked until ARR evidence exists.
    assert!(input(&o).issues.iter().any(|x| {
        x.blockers
            .iter()
            .any(|b| b.code == B::ArrNotUpgradeEvidence)
    }));
    let i = with_facts(
        &o,
        Ok(movie_facts(
            false,
            &["Not an upgrade for existing movie file"],
            3000,
        )),
    );
    let b = build(&i, &s, &delete_replaced(&i)).unwrap();
    let a = action(&b, K::DeleteCompletedFile);
    assert!(a.preconditions.iter().any(|p| matches!(p,
        Precondition::EntryUnchanged { path, compare_nlink: false, .. } if *path == rp(&media(), LIB))));
    assert!(
        a.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::ArrFingerprint { .. }))
    );
    assert!(
        a.preconditions
            .contains(&Precondition::ServiceAvailable(ServiceKind::Radarr))
    );
    assert!(
        a.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::TorrentStates { .. }))
    );
    assert_eq!(b.plan.plan().arr_snapshots.len(), 1);
    assert!(
        b.plan.plan().lock_roots.contains(&media()),
        "the replacement is pinned"
    );

    let blocked = |f, code: &B| {
        let i = with_facts(&o, f);
        let r = build(&i, &s, &delete_replaced(&i));
        assert!(
            has_code(&r, &subject, code),
            "{code:?}: {:?}",
            excluded_codes(&r)
        );
    };
    blocked(Ok(movie_facts(true, &[], 3000)), &B::ArrNotUpgradeEvidence);
    blocked(
        Ok(movie_facts(false, &["Unknown movie"], 3000)),
        &B::ArrRejected,
    );
    blocked(
        Ok(movie_facts(false, &["Not an upgrade"], 2999)),
        &B::ArrAmbiguous,
    );
    blocked(
        Err(PortError::new(ErrorClass::Unavailable, "down")),
        &B::ServiceUnavailable(ServiceKind::Radarr),
    );
    let i = input(&o);
    assert!(has_code(
        &build(&i, &s, &delete_replaced(&i)),
        &subject,
        &B::ArrMissing
    ));
    // Without ARR observations no identity is known.
    let mut no_arr = o.clone();
    no_arr.arr.clear();
    let mut i = with_facts(&no_arr, Ok(movie_facts(false, &["Not an upgrade"], 3000)));
    i.issues = input(&o).issues;
    assert!(has_code(
        &build(&i, &s, &delete_replaced(&i)),
        &subject,
        &B::ArrMissing
    ));
}

#[test]
fn arr_score_or_profile_change_invalidates_the_plan() {
    let o = movie_world();
    let s = live();
    let i = with_facts(&o, Ok(movie_facts(false, &["Not an upgrade"], 3000)));
    let b = build(&i, &s, &delete_replaced(&i)).unwrap();
    let plan = b.plan.plan();
    let fp = plan.arr_snapshots[0].fingerprint.clone();
    let same = BTreeMap::from([((ArrService::Radarr, 7), Ok(fp))]);
    assert!(staleness(plan, NOW, &i, &s, &same).is_empty());
    // A custom-format score change gives another fingerprint.
    let movie = arr_media(ArrService::Radarr, 7);
    let changed = arr_snapshot(
        &movie,
        vec![file_ref(100, &rp(&media(), LIB), 3000)],
        Some(mm(None, None, true)),
        50,
    );
    let fps = BTreeMap::from([((ArrService::Radarr, 7), Ok(changed.fingerprint))]);
    let reasons = staleness(plan, NOW, &i, &s, &fps);
    assert!(
        reasons
            .iter()
            .any(|r| matches!(r, StaleReason::ArrChanged { .. }))
    );
    assert_eq!(verdict(&reasons), Some(PlanStatus::Invalidated));
    let down = BTreeMap::from([(
        (ArrService::Radarr, 7),
        Err(PortError::new(ErrorClass::Unavailable, "x")),
    )]);
    assert!(
        staleness(plan, NOW, &i, &s, &down)
            .iter()
            .any(|r| matches!(r, StaleReason::ArrUnavailable { .. }))
    );
}

// ---- T-21: a torrent-less release folder with two episodes ----------------------------------

fn pack_world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .dir("Pack", 2)
        .file("Pack/Show.S01E01.720p.mkv", 20, 1000)
        .file("Pack/Show.S01E02.720p.mkv", 21, 1000)
        .file("Pack/Show.nfo", 22, 10)
        .build();
    let med = Inv::new(m.clone())
        .dir("tv", 5)
        .file("tv/Show.S01E01.1080p.mkv", 31, 3000)
        .file("tv/Show.S01E02.1080p.mkv", 32, 3000)
        .build();
    let mut o = obs(
        vec![comp, med],
        QbitObservation::Available(snapshot(4, vec![])),
    );
    let s = |ino, size| sig(EntryKind::File, 1, ino, size, 1, 0o644);
    o.matches = vec![
        candidate(
            rp(&c, "Pack/Show.S01E01.720p.mkv"),
            rp(&m, "tv/Show.S01E01.1080p.mkv"),
            s(20, 1000),
            s(31, 3000),
            ContentProof::None,
        ),
        candidate(
            rp(&c, "Pack/Show.S01E02.720p.mkv"),
            rp(&m, "tv/Show.S01E02.1080p.mkv"),
            s(21, 1000),
            s(32, 3000),
            ContentProof::None,
        ),
    ];
    o.arr = vec![arr_ok(
        ArrService::Sonarr,
        vec![
            library_entry(ArrService::Sonarr, 1, rp(&m, "tv/Show.S01E01.1080p.mkv")),
            library_entry(ArrService::Sonarr, 1, rp(&m, "tv/Show.S01E02.1080p.mkv")),
        ],
        vec![],
    )];
    o
}

fn pack_facts(e02_approved: bool) -> removarr_workflows::evaluate::ArrFacts {
    let (c, m) = (completed(), media());
    let series = arr_media(ArrService::Sonarr, 1);
    facts(
        arr_snapshot(
            &series,
            vec![
                file_ref(201, &rp(&m, "tv/Show.S01E01.1080p.mkv"), 3000),
                file_ref(202, &rp(&m, "tv/Show.S01E02.1080p.mkv"), 3000),
            ],
            Some(mm(None, None, true)),
            10,
        ),
        vec![episode(101, 1, 1, Some(201)), episode(102, 1, 2, Some(202))],
        vec![(
            rp(&c, "Pack"),
            vec![
                decision(
                    &rp(&c, "Pack/Show.S01E01.720p.mkv"),
                    vec![101],
                    false,
                    &["Not an upgrade for existing episode file(s)"],
                ),
                if e02_approved {
                    decision(&rp(&c, "Pack/Show.S01E02.720p.mkv"), vec![102], true, &[])
                } else {
                    decision(
                        &rp(&c, "Pack/Show.S01E02.720p.mkv"),
                        vec![102],
                        false,
                        &["Not an upgrade for existing episode file(s)"],
                    )
                },
            ],
        )],
        vec![],
    )
}

#[test]
fn folder_deletion_with_arr_evidence_is_all_or_nothing_and_names_the_failing_member() {
    let o = pack_world();
    let s = live();
    let c = completed();
    let mut i = input(&o);
    i.arr = BTreeMap::from([((ArrService::Sonarr, 1), Ok(pack_facts(true)))]);
    let id = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Pack")));
    let r = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![id.clone()], A::DeleteReplaced),
    );
    let list = excluded_codes(&r);
    assert_eq!(list.len(), 1, "the whole folder is refused");
    let detail = match &r {
        Err(removarr_workflows::plans::PlanError::NothingPlannable { excluded }) => excluded[0]
            .1
            .iter()
            .map(|b| b.detail.clone())
            .collect::<Vec<_>>()
            .join(" | "),
        other => panic!("{other:?}"),
    };
    assert!(
        detail.contains("Show.S01E02.720p.mkv"),
        "the blocker names the offending video: {detail}"
    );
    assert!(
        !detail.contains("Show.S01E01.720p.mkv:"),
        "the passing video is not blamed"
    );

    i.arr = BTreeMap::from([((ArrService::Sonarr, 1), Ok(pack_facts(false)))]);
    let b = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![id.clone()], A::DeleteReplaced),
    )
    .unwrap();
    let a = action(&b, K::DeleteCompletedFolder);
    let pinned = a
        .preconditions
        .iter()
        .filter(|p| matches!(p, Precondition::EntryUnchanged { path, .. } if path.root == media()))
        .count();
    assert_eq!(pinned, 2, "both replacements are pinned");
    // Automatic mode (domain patch 2): an ARR-validated folder deletion is approved by policy.
    let auto = build_auto(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![id], A::DeleteReplaced),
        &auto_on(),
    )
    .unwrap();
    assert_eq!(
        action(&auto, K::DeleteCompletedFolder).evidence_level,
        EvidenceLevel::ArrValidatedReplacement
    );
    assert!(auto.plan.approve_auto(NOW).is_ok());
}

// ---- Automatic ARR-validated deletions (domain patch 2) -------------------------------------

fn auto_movie(
    f: Result<removarr_workflows::evaluate::ArrFacts, PortError>,
) -> Result<removarr_workflows::plans::Built, removarr_workflows::plans::PlanError> {
    let i = with_facts(&movie_world(), f);
    let r = delete_replaced(&i);
    build_auto(&i, &live(), &r, &auto_on())
}

fn reseal(plan: removarr_domain::plan::ActionPlan) -> removarr_domain::plan::HashedPlan {
    removarr_domain::plan::HashedPlan::seal(plan).unwrap()
}

#[test]
fn automatic_arr_validated_deletion_is_approved_when_every_condition_holds() {
    let b = auto_movie(Ok(movie_facts(
        false,
        &["Not an upgrade for existing movie file"],
        3000,
    )))
    .unwrap();
    assert_eq!(
        kinds(&b),
        [K::DeleteCompletedFile],
        "companions without a proof level are left out of auto plans"
    );
    let a = action(&b, K::DeleteCompletedFile);
    let (c, m) = (completed(), media());
    assert_eq!(a.evidence_level, EvidenceLevel::ArrValidatedReplacement);
    assert!(
        a.preconditions
            .contains(&Precondition::ArrReplacementPresent {
                media: arr_media(ArrService::Radarr, 7),
                path: rp(&m, LIB),
                size: 3000,
            })
    );
    assert!(a.preconditions.iter().any(|p| matches!(p,
        Precondition::EntryUnchanged { path, compare_nlink: false, .. } if *path == rp(&m, LIB))));
    assert!(a.preconditions.iter().any(|p| matches!(p,
        Precondition::EntryUnchanged { path, compare_nlink: true, .. } if *path == rp(&c, OLD))));
    assert!(a.preconditions.iter().any(|p| matches!(p,
        Precondition::ArrFingerprint { media, .. } if *media == arr_media(ArrService::Radarr, 7))));
    let approval = b.plan.approve_auto(NOW).unwrap();
    assert_eq!(approval.by, removarr_domain::plan::Approver::AutoPolicy);
}

#[test]
fn automatic_arr_validated_deletion_is_refused_when_any_condition_is_missing() {
    let subject = sub(&rp(&completed(), OLD));
    // CP1-F02 checks fail: the unit is never planned automatically.
    for (f, code) in [
        (Ok(movie_facts(true, &[], 3000)), B::ArrNotUpgradeEvidence),
        (
            Ok(movie_facts(false, &["Unknown movie"], 3000)),
            B::ArrRejected,
        ),
        (
            Ok(movie_facts(false, &["Not an upgrade"], 2999)),
            B::ArrAmbiguous,
        ),
        (
            Err(PortError::new(ErrorClass::Unavailable, "down")),
            B::ServiceUnavailable(ServiceKind::Radarr),
        ),
    ] {
        let r = auto_movie(f);
        assert!(
            has_code(&r, &subject, &code),
            "{code:?}: {:?}",
            excluded_codes(&r)
        );
    }
    // A3: too recent.
    let i = with_facts(
        &movie_world(),
        Ok(movie_facts(false, &["Not an upgrade"], 3000)),
    );
    let mut young = live();
    young.auto_min_age_hours = 1000;
    assert!(has_code(
        &build_auto(&i, &young, &delete_replaced(&i), &auto_on()),
        &subject,
        &B::Recent
    ));
    // The plain deletion is never automatic.
    let plain = req(
        C::HardlinkMissing,
        vec![id_of(&i, C::HardlinkMissing, &subject)],
        A::Delete,
    );
    assert!(matches!(
        build_auto(&i, &live(), &plain, &auto_on()),
        Err(removarr_workflows::plans::PlanError::AutoRefused(_))
    ));

    // Any missing piece of pinned evidence makes the domain refuse the automatic approval.
    let good = auto_movie(Ok(movie_facts(false, &["Not an upgrade"], 3000))).unwrap();
    let base = good.plan.plan().clone();
    let strip = |f: &dyn Fn(&Precondition) -> bool| {
        let mut p = base.clone();
        p.actions[0].preconditions.retain(|x| !f(x));
        reseal(p)
    };
    let present = |x: &Precondition| matches!(x, Precondition::ArrReplacementPresent { .. });
    let fingerprint = |x: &Precondition| matches!(x, Precondition::ArrFingerprint { .. });
    let replacement_pin = |x: &Precondition| {
        matches!(
            x,
            Precondition::EntryUnchanged {
                compare_nlink: false,
                ..
            }
        )
    };
    for (name, f) in [
        ("present", &present as &dyn Fn(&Precondition) -> bool),
        ("fingerprint", &fingerprint),
        ("pin", &replacement_pin),
    ] {
        assert!(
            matches!(
                strip(f).approve_auto(NOW),
                Err(DomainError::IncompleteEvidence { .. })
            ),
            "{name}"
        );
    }
    // A sample needing acknowledgement, or a media-root target, is never automatic.
    let mut with_sample = base.clone();
    with_sample
        .samples
        .push(rp(&completed(), "Old.Movie.2020.720p/Sample/s.mkv"));
    assert!(matches!(
        reseal(with_sample).approve_auto(NOW),
        Err(DomainError::AutoApprovalForbidden(_))
    ));
    let mut media_target = base.clone();
    media_target.actions[0].target = ActionTarget::Path {
        path: rp(&media(), LIB),
    };
    assert!(reseal(media_target).approve_auto(NOW).is_err());
    // The same evidence level on any other kind is refused.
    let mut other = base;
    other.actions[0].kind = K::DeleteJunk;
    other.origin = removarr_domain::plan::PlanOrigin::User;
    assert!(reseal(other).approve_auto(NOW).is_err());
}

// ---- ReplaceRelease: Sonarr season with an episode without a file -----------------------

const NEW1: &str = "Show.S01/Show.S01E01.1080p.WEB.x264-GRP.mkv";
const NEW2: &str = "Show.S01/Show.S01E02.1080p.WEB.x264-GRP.mkv";
const NEW3: &str = "Show.S01/Show.S01E03.1080p.WEB.x264-GRP.mkv";
const OLD1: &str = "tv/Show/Show.S01E01.720p.HDTV.x264-OLD.mkv";
const OLD2: &str = "tv/Show/Show.S01E02.720p.HDTV.x264-OLD.mkv";
const SEED: &str = "old/Show.S01E01.720p.HDTV.x264-OLD.mkv";

fn season_world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .dir("Show.S01", 2)
        .file(NEW1, 41, 2000)
        .file(NEW2, 42, 2000)
        .file(NEW3, 43, 2000)
        .dir("old", 3)
        .file_nlink(SEED, 31, 1000, 2)
        .build();
    let med = Inv::new(m.clone())
        .dir("tv", 5)
        .dir("tv/Show", 6)
        .file_nlink(OLD1, 31, 1000, 2)
        .file(OLD2, 32, 1000)
        .build();
    let files: Vec<(&str, &str)> = vec![(NEW1, NEW1), (NEW2, NEW2), (NEW3, NEW3)];
    let snap = snapshot(
        4,
        vec![
            Tor::new('p', &c, "uploading", 1.0, "Show.S01", &files),
            Tor::new('o', &c, "uploading", 1.0, SEED, &[(SEED, SEED)]),
        ],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    let mut sonarr = arr_ok(
        ArrService::Sonarr,
        vec![
            library_entry(ArrService::Sonarr, 1, rp(&m, OLD1)),
            library_entry(ArrService::Sonarr, 1, rp(&m, OLD2)),
        ],
        vec![],
    );
    sonarr.history = vec![ArrHistoryRecord {
        download_id: Some(hash('p')),
        media: Some(arr_media(ArrService::Sonarr, 1)),
        event_type: "grabbed".into(),
        source_title: "Show.S01".into(),
        date: NOW,
    }];
    o.arr = vec![sonarr];
    o
}

struct Season {
    recycle: bool,
    hardlinks: bool,
    e03_approved: bool,
    e01_duration: f64,
    e03_name: &'static str,
}

impl Default for Season {
    fn default() -> Self {
        Self {
            recycle: false,
            hardlinks: true,
            e03_approved: true,
            e01_duration: 1500.0,
            e03_name: NEW3,
        }
    }
}

fn season_facts(o: &Season) -> removarr_workflows::evaluate::ArrFacts {
    let (c, m) = (completed(), media());
    let series = arr_media(ArrService::Sonarr, 1);
    let bin = o.recycle.then(|| mapped(&rp(&m, "recycle")));
    let mut special = episode(100, 0, 1, None);
    special.monitored = true;
    facts(
        arr_snapshot(
            &series,
            vec![
                file_ref(201, &rp(&m, OLD1), 1000),
                file_ref(202, &rp(&m, OLD2), 1000),
            ],
            Some(mm(bin, Some(30), o.hardlinks)),
            10,
        ),
        vec![
            special,
            episode(101, 1, 1, Some(201)),
            episode(102, 1, 2, Some(202)),
            episode(103, 1, 3, None),
        ],
        vec![(
            rp(&c, "Show.S01"),
            vec![
                decision(&rp(&c, NEW1), vec![101], true, &[]),
                decision(&rp(&c, NEW2), vec![102], true, &[]),
                decision(
                    &rp(&c, o.e03_name),
                    vec![103],
                    o.e03_approved,
                    if o.e03_approved { &[] } else { &["Sample"] },
                ),
            ],
        )],
        vec![
            (rp(&c, NEW1), probe(o.e01_duration, &["eng"])),
            (rp(&c, NEW2), probe(1500.0, &["eng"])),
            (rp(&c, NEW3), probe(1500.0, &["eng"])),
            (rp(&m, OLD1), probe(1500.0, &["eng"])),
            (rp(&m, OLD2), probe(1500.0, &["eng"])),
        ],
    )
}

fn season_input(world: &Observation, s: &Season) -> PlanInput {
    let mut i = input(world);
    i.arr = BTreeMap::from([((ArrService::Sonarr, 1), Ok(season_facts(s)))]);
    i
}

fn replace(i: &PlanInput, files: &[&str], delete_old_media: bool) -> PlanRequest {
    let ids = files
        .iter()
        .map(|f| id_of(i, C::HardlinkMissing, &sub(&rp(&completed(), f))))
        .collect();
    req(
        C::HardlinkMissing,
        ids,
        A::ReplaceRelease { delete_old_media },
    )
}

#[test]
fn replacement_needs_arr_approval_complete_coverage_and_recoverable_old_files() {
    let o = season_world();
    let (c, m) = (completed(), media());
    let s = live();
    let i = season_input(&o, &Season::default());
    let b = build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], false)).unwrap();
    assert_eq!(kinds(&b), [K::ReplaceRelease]);
    let a = action(&b, K::ReplaceRelease);
    let ActionTarget::Replace { request } = &a.target else {
        panic!()
    };
    let mapping: Vec<(RootedPath, Vec<i64>)> = request
        .files
        .iter()
        .map(|(p, m)| (p.clone(), m.episode_ids.clone()))
        .collect();
    assert_eq!(
        mapping,
        [
            (rp(&c, NEW1), vec![101]),
            (rp(&c, NEW2), vec![102]),
            (rp(&c, NEW3), vec![103])
        ]
    );
    let recov = a
        .preconditions
        .iter()
        .find_map(|p| match p {
            Precondition::OldCoverageRecoverable { old_files } => Some(old_files.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        recov
            .iter()
            .map(|(p, _, r)| (p.clone(), r.clone()))
            .collect::<Vec<_>>(),
        [
            (rp(&m, OLD1), OldFileRecovery::OtherLink(rp(&c, SEED))),
            (rp(&m, OLD2), OldFileRecovery::Unrecoverable)
        ]
    );
    assert!(
        a.preconditions.iter().any(
            |p| matches!(p, Precondition::EntryUnchanged { path, .. } if *path == rp(&c, SEED))
        ),
        "the other link is pinned"
    );
    assert_eq!(
        a.verification
            .iter()
            .filter(|v| matches!(v, VerificationStep::ArrCurrentFile { .. }))
            .count(),
        3
    );
    assert_eq!(
        a.verification
            .iter()
            .filter(|v| matches!(v, VerificationStep::PathStillHasInode { .. }))
            .count(),
        3
    );
    assert_eq!(
        a.verification
            .iter()
            .filter(|v| matches!(v, VerificationStep::OldFileAccounted { .. }))
            .count(),
        2
    );
    assert_eq!(a.expected_gain.added, 0, "ARR hardlinks: no added cost");
    // The unrecoverable old file needs a per-file acknowledgement; it is a warning, not hidden.
    let pv = preview(&b.plan, None);
    assert_eq!(pv.unrecoverable_to_acknowledge, [rp(&m, OLD2)]);
    assert!(
        pv.warnings
            .iter()
            .any(|w| w.code == B::OldCoverageUnrecoverable)
    );
    let approve = |acks: Vec<RootedPath>| {
        b.plan.approve(
            &ApprovalRequest {
                plan_hash: b.plan.hash().clone(),
                confirmed_count: 1,
                confirmed_torrent_removals: 0,
                acknowledged_samples: vec![],
                acknowledged_unrecoverable: acks,
            },
            NOW,
        )
    };
    assert!(matches!(
        approve(vec![]),
        Err(DomainError::UnrecoverableNotAcknowledged(_))
    ));
    assert!(approve(vec![rp(&m, OLD2)]).is_ok());
    assert!(b.plan.approve_auto(NOW).is_err(), "never automatic");
}

#[test]
fn replacement_blockers_coverage_numbering_probes_and_gates() {
    let o = season_world();
    let s = live();
    let (c, m) = (completed(), media());
    let subj = |f: &str| sub(&rp(&c, f));
    // Coverage: the episode without a file must be covered too.
    let i = season_input(&o, &Season::default());
    let r = build(&i, &s, &replace(&i, &[NEW1, NEW2], false));
    assert!(has_code(&r, &subj(NEW1), &B::CoverageIncomplete));
    // ARR must accept every candidate.
    let i = season_input(
        &o,
        &Season {
            e03_approved: false,
            ..Season::default()
        },
    );
    assert!(has_code(
        &build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], false)),
        &subj(NEW3),
        &B::ArrRejected
    ));
    // ffprobe duration difference blocks.
    let i = season_input(
        &o,
        &Season {
            e01_duration: 1300.0,
            ..Season::default()
        },
    );
    assert!(has_code(
        &build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], false)),
        &subj(NEW1),
        &B::DurationMismatch
    ));
    // Recycle bin makes the second old file recoverable.
    let i = season_input(
        &o,
        &Season {
            recycle: true,
            ..Season::default()
        },
    );
    let b = build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], false)).unwrap();
    assert!(b.plan.plan().unrecoverable_old_files().is_empty());
    // Hardlinks disabled in ARR: added cost and a visible warning.
    let i = season_input(
        &o,
        &Season {
            hardlinks: false,
            ..Season::default()
        },
    );
    let b = build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], false)).unwrap();
    assert!(action(&b, K::ReplaceRelease).expected_gain.added > 0);
    assert!(
        preview(&b.plan, None)
            .warnings
            .iter()
            .any(|w| w.code == B::ArrHardlinksDisabled)
    );
    // Optional old-file deletions come after the verified import.
    let i = season_input(&o, &Season::default());
    let b = build(&i, &s, &replace(&i, &[NEW1, NEW2, NEW3], true)).unwrap();
    assert_eq!(
        kinds(&b),
        [K::ReplaceRelease, K::DeleteMediaFile, K::DeleteMediaFile]
    );
    assert!(
        b.plan.plan().actions[1..]
            .iter()
            .all(|a| a.depends_on == [0])
    );
    assert!(
        matches!(&b.plan.plan().actions[2].target, ActionTarget::Path { path } if *path == rp(&m, OLD2))
    );
    // Capability and tier gates.
    let mut off = live();
    off.capabilities.remove(&Capability::ArrManualImport);
    assert!(has_code(
        &build(&i, &off, &replace(&i, &[NEW1, NEW2, NEW3], false)),
        &subj(NEW1),
        &B::CapabilityDisabled
    ));
    let mut t3 = live();
    t3.implemented_tier = RiskTier::T3;
    assert!(has_code(
        &build(&i, &t3, &replace(&i, &[NEW1, NEW2, NEW3], false)),
        &subj(NEW1),
        &B::NotInBuild
    ));
}

#[test]
fn parsed_numbering_that_disagrees_with_arr_needs_a_manual_check() {
    const ODD: &str = "Show.S01/Show.S01E05.1080p.WEB.x264-GRP.mkv";
    let mut o = season_world();
    let c = completed();
    let inv = &mut o.inventories[0];
    for e in &mut inv.entries {
        if e.path == rp(&c, NEW3) {
            e.path = rp(&c, ODD);
        }
    }
    inv.entries.sort_by(|a, b| a.path.cmp(&b.path));
    if let QbitObservation::Available(snap) = &mut o.qbit {
        for files in snap.files.values_mut() {
            for f in files.iter_mut().filter(|f| f.name == NEW3) {
                f.name = ODD.into();
                f.candidates = vec![mapped(&rp(&c, ODD))];
            }
        }
    }
    let i = season_input(
        &o,
        &Season {
            e03_name: ODD,
            ..Season::default()
        },
    );
    let r = build(&i, &live(), &replace(&i, &[NEW1, NEW2, ODD], false));
    assert!(
        has_code(&r, &sub(&rp(&c, ODD)), &B::NumberingMismatch),
        "{:?}",
        excluded_codes(&r)
    );
}

// ---- Grab -------------------------------------------------------------------------------

fn release(approved: bool) -> ReleaseSearchResult {
    ReleaseSearchResult {
        release: ReleaseRef {
            guid: "rel-abc".into(),
            indexer_id: 3,
        },
        indexer: "idx".into(),
        title: "Old.Movie.2020.2160p".into(),
        size: 9000,
        seeders: Some(5),
        leechers: Some(1),
        age_days: 2,
        protocol: "torrent".into(),
        quality: "UHD".into(),
        custom_formats: vec![],
        cf_score: 10,
        approved,
        rejections: if approved {
            vec![]
        } else {
            vec!["Not wanted".into()]
        },
        download_allowed: true,
    }
}

#[test]
fn grab_needs_a_fresh_search_an_approved_release_a_live_process_and_the_capability() {
    let o = movie_world();
    let movie = arr_media(ArrService::Radarr, 7);
    let rr = PlanRequest::Release {
        media: movie.clone(),
        release: ReleaseRef {
            guid: "rel-abc".into(),
            indexer_id: 3,
        },
    };
    let subject = "r:radarr:7:rel-abc";
    let base = with_facts(&o, Ok(movie_facts(false, &["Not an upgrade"], 3000)));
    let s = live();
    assert!(has_code(
        &build(&base, &s, &rr),
        subject,
        &B::ReleaseNotInSearch
    ));
    let mut rejected = base.clone();
    rejected.search = Some(Ok(vec![release(false)]));
    assert!(has_code(
        &build(&rejected, &s, &rr),
        subject,
        &B::ArrRejected
    ));
    let mut ok = base.clone();
    ok.search = Some(Ok(vec![release(true)]));
    let b = build(&ok, &s, &rr).unwrap();
    let a = action(&b, K::ArrGrabRelease);
    assert!(
        a.verification
            .contains(&VerificationStep::ArrQueueHasRelease {
                service: ArrService::Radarr,
                release: ReleaseRef {
                    guid: "rel-abc".into(),
                    indexer_id: 3
                }
            })
    );
    assert!(
        a.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::ArrFingerprint { .. }))
    );
    let mut dry = live();
    dry.mode = PlanMode::DryRun;
    assert!(has_code(&build(&ok, &dry, &rr), subject, &B::DryRunOnly));
    let mut off = live();
    off.capabilities.remove(&Capability::ArrGrab);
    assert!(has_code(
        &build(&ok, &off, &rr),
        subject,
        &B::CapabilityDisabled
    ));
    assert!(
        b.plan.approve_auto(NOW).is_err(),
        "a grab is never automatic"
    );
}

// ---- T-06, T-07, T-19: local checks block, technical tags never decide --------------------

/// A Radarr replacement of the library file by the completed `name`, ARR approving the import.
fn movie_replacement(
    name: &str,
) -> Result<removarr_workflows::plans::Built, removarr_workflows::plans::PlanError> {
    let (c, m) = (completed(), media());
    let path = format!("Rel/{name}");
    let comp = Inv::new(c.clone())
        .dir("Rel", 2)
        .file(&path, 10, 1000)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/Old Movie (2020)", 6)
        .file(LIB, 11, 3000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            "Rel",
            &[(&path, &path)],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, &path),
        rp(&m, LIB),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 3000, 1, 0o644),
        ContentProof::None,
    )];
    o.arr = vec![arr_ok(
        ArrService::Radarr,
        vec![library_entry(ArrService::Radarr, 7, rp(&m, LIB))],
        vec![],
    )];
    let movie = arr_media(ArrService::Radarr, 7);
    let f = facts(
        arr_snapshot(
            &movie,
            vec![file_ref(100, &rp(&m, LIB), 3000)],
            Some(mm(None, None, true)),
            10,
        ),
        vec![],
        vec![(
            rp(&c, "Rel"),
            vec![decision(&rp(&c, &path), vec![], true, &[])],
        )],
        vec![
            (rp(&c, &path), probe(7200.0, &["eng"])),
            (rp(&m, LIB), probe(7200.0, &["eng"])),
        ],
    );
    let mut i = input(&o);
    i.arr = BTreeMap::from([((ArrService::Radarr, 7), Ok(f))]);
    let id = id_of(&i, C::HardlinkMissing, &sub(&rp(&c, &path)));
    build(
        &i,
        &live(),
        &req(
            C::HardlinkMissing,
            vec![id],
            A::ReplaceRelease {
                delete_old_media: false,
            },
        ),
    )
}

#[test]
fn local_checks_block_editions_and_titles_but_technical_tags_never_decide() {
    // T-19 (superseded by CON-02): a 720p WEB file replaces a 1080p BluRay one because ARR
    // approves it; resolution and source are explanatory only.
    let ok = movie_replacement("Old.Movie.2020.720p.WEB.x264-GRP.mkv").unwrap();
    assert_eq!(kinds(&ok), [K::ReplaceRelease]);
    // T-06: a different edition blocks.
    let r = movie_replacement("Old.Movie.2020.Extended.720p.WEB.x264-GRP.mkv");
    assert!(
        excluded_codes(&r)
            .iter()
            .any(|(_, c)| c.contains(&B::EditionMismatch)),
        "{:?}",
        excluded_codes(&r)
    );
    // T-07: a different normalized title blocks (manual decision).
    let r = movie_replacement("Older.Movie.2020.720p.WEB.x264-GRP.mkv");
    assert!(
        excluded_codes(&r)
            .iter()
            .any(|(_, c)| c.contains(&B::ArrAmbiguous)),
        "{:?}",
        excluded_codes(&r)
    );
}
