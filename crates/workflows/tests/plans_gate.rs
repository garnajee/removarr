#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Risk tiers, capability flags, dry-run-only kinds and the automatic policy (A1-A9; legacy
//! T-20, T-31 plan side).

mod common;
mod plans_common;

use common::*;
use plans_common::*;
use removarr_domain::arr::ArrService;
use removarr_domain::evidence::ContentProof;
use removarr_domain::fs::EntryKind;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{
    ActionKind as K, PlanMode, PlanOrigin, Precondition, VerificationStep,
};
use removarr_domain::policy::{Capability, RiskTier};
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::MappedPath;
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{
    AutoPhase, IssueAction as A, PlanError, PlanRequest, auto_requests, gate, preview,
};

const MOVIE: &str = "Movie.A.2020.mkv";
const LIB: &str = "movies/A/Movie.A.2020.mkv";

fn world() -> Observation {
    let (c, m) = (completed(), media());
    let comp = Inv::new(c.clone())
        .file(MOVIE, 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/A", 6)
        .file(LIB, 11, 1000)
        .build();
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &c,
            "uploading",
            1.0,
            MOVIE,
            &[(MOVIE, MOVIE)],
        )],
    );
    let mut o = obs(vec![comp, med], QbitObservation::Available(snap));
    o.matches = vec![candidate(
        rp(&c, MOVIE),
        rp(&m, LIB),
        sig(EntryKind::File, 1, 10, 1000, 1, 0o644),
        sig(EntryKind::File, 1, 11, 1000, 1, 0o644),
        ContentProof::NameSize,
    )];
    o
}

fn sub(p: &RootedPath) -> String {
    SubjectKey::rooted(p).0
}

fn consolidate_req(i: &removarr_workflows::plans::PlanInput) -> PlanRequest {
    let id = id_of(i, C::HardlinkMissing, &sub(&rp(&completed(), MOVIE)));
    req(C::HardlinkMissing, vec![id], A::Consolidate)
}

fn delete_req(i: &removarr_workflows::plans::PlanInput) -> PlanRequest {
    let id = id_of(i, C::OrphanCompleted, &sub(&rp(&completed(), "Orphan.Rel")));
    req(C::OrphanCompleted, vec![id], A::Delete)
}

#[test]
fn actions_are_tier_tagged_and_unimplemented_tiers_are_unreachable() {
    let i = input(&world());
    let mut s = live();
    s.implemented_tier = RiskTier::T2;
    let b = build(&i, &s, &consolidate_req(&i)).unwrap();
    assert_eq!(preview(&b.plan, None).actions[0].tier, RiskTier::T2);
    let r = build(&i, &s, &delete_req(&i));
    assert!(has_code(
        &r,
        &sub(&rp(&completed(), "Orphan.Rel")),
        &B::NotInBuild
    ));
    s.implemented_tier = RiskTier::T1;
    let r = build(&i, &s, &consolidate_req(&i));
    assert!(has_code(&r, &sub(&rp(&completed(), MOVIE)), &B::NotInBuild));
    s.implemented_tier = RiskTier::T3;
    let b = build(&i, &s, &delete_req(&i)).unwrap();
    let pv = preview(&b.plan, None);
    assert_eq!(
        (pv.actions[0].tier, pv.max_tier),
        (RiskTier::T3, RiskTier::T3)
    );
    // Every kind's tier and capability come from the domain table.
    assert_eq!(
        gate::action_tier(K::ReplaceRelease, PlanOrigin::User),
        RiskTier::T4
    );
    assert_eq!(
        gate::action_tier(K::ConsolidateHardlink, PlanOrigin::AutoPolicy),
        RiskTier::T5
    );
}

#[test]
fn disabled_capabilities_block_planning() {
    let i = input(&world());
    let mut s = live();
    s.capabilities.remove(&Capability::Consolidate);
    let r = build(&i, &s, &consolidate_req(&i));
    assert!(has_code(
        &r,
        &sub(&rp(&completed(), MOVIE)),
        &B::CapabilityDisabled
    ));
    s.capabilities.remove(&Capability::DeleteCompleted);
    let r = build(&i, &s, &delete_req(&i));
    assert!(has_code(
        &r,
        &sub(&rp(&completed(), "Orphan.Rel")),
        &B::CapabilityDisabled
    ));
    // Defaults: ARR mutations are off.
    let d = removarr_workflows::plans::PlanSettings::new("x", settings());
    for c in [
        Capability::ArrManualImport,
        Capability::ArrGrab,
        Capability::ArrQueueRemove,
        Capability::ArrSetMonitored,
        Capability::AutoRemoveEmptyTorrents,
    ] {
        assert!(!d.capabilities.contains(&c));
    }
    assert_eq!(
        (d.mode, d.implemented_tier),
        (PlanMode::DryRun, RiskTier::T1)
    );
}

#[test]
fn service_commands_are_live_only_and_need_a_configured_service() {
    let mut o = world();
    o.arr = vec![arr_ok(ArrService::Sonarr, vec![], vec![])];
    let i = input(&o);
    let rescan = PlanRequest::ServiceRescan {
        service: ServiceKind::Sonarr,
        media: None,
    };
    let b = build(&i, &live(), &rescan).unwrap();
    assert_eq!(kinds(&b), [K::ArrRescan]);
    assert!(
        b.plan.plan().actions[0]
            .preconditions
            .contains(&Precondition::ServiceAvailable(ServiceKind::Sonarr))
    );
    assert_eq!(
        b.plan.plan().actions[0].verification,
        [VerificationStep::ArrCommandCompleted {
            service: ArrService::Sonarr,
            media: None
        }]
    );
    let mut dry = live();
    dry.mode = PlanMode::DryRun;
    let r = build(&i, &dry, &rescan);
    assert!(has_code(&r, "s:sonarr", &B::DryRunOnly));
    let jf = PlanRequest::ServiceRescan {
        service: ServiceKind::Jellyfin,
        media: None,
    };
    let r = build(&i, &live(), &jf);
    assert!(has_code(
        &r,
        "s:jellyfin",
        &B::ServiceNotConfigured(ServiceKind::Jellyfin)
    ));
    let q = PlanRequest::ServiceRescan {
        service: ServiceKind::Qbittorrent,
        media: None,
    };
    assert!(matches!(
        build(&i, &live(), &q),
        Err(PlanError::InvalidRequest(_))
    ));
}

#[test]
fn dry_run_plans_use_the_same_path_and_are_marked() {
    let i = input(&world());
    let mut dry = live();
    dry.mode = PlanMode::DryRun;
    let d = build(&i, &dry, &consolidate_req(&i)).unwrap();
    let l = build(&i, &live(), &consolidate_req(&i)).unwrap();
    assert_eq!(d.plan.plan().mode, PlanMode::DryRun);
    assert!(preview(&d.plan, None).dry_run);
    let (mut a, mut b) = (d.plan.plan().clone(), l.plan.plan().clone());
    a.mode = PlanMode::Live;
    b.mode = PlanMode::Live;
    assert_eq!(a, b, "only the mode differs between dry-run and live plans");
}

#[test]
fn automatic_mode_off_paused_or_manual_only_actions_are_refused() {
    let i = input(&world());
    let s = live();
    let off = removarr_workflows::plans::AutoContext::default();
    assert!(matches!(
        build_auto(&i, &s, &consolidate_req(&i), &off),
        Err(PlanError::AutoRefused(_))
    ));
    let mut paused = auto_on();
    paused.auto_paused = Some("previous failure".into());
    assert!(matches!(
        build_auto(&i, &s, &consolidate_req(&i), &paused),
        Err(PlanError::AutoRefused(_))
    ));
    // Plain deletions, stalled cleanup, mode fixes and replacements are never automatic.
    for action in [
        A::Delete,
        A::CleanupStalled,
        A::FixMode,
        A::ReplaceRelease {
            delete_old_media: false,
        },
    ] {
        let r = PlanRequest::issues(C::OrphanCompleted, vec![], action);
        assert!(matches!(
            build_auto(&i, &s, &r, &auto_on()),
            Err(PlanError::AutoRefused(_))
        ));
    }
    // Service commands and grabs are never automatic (A9).
    let mut o = world();
    o.arr = vec![arr_ok(ArrService::Sonarr, vec![], vec![])];
    let i2 = input(&o);
    let rescan = PlanRequest::ServiceRescan {
        service: ServiceKind::Sonarr,
        media: None,
    };
    let r = build_auto(&i2, &s, &rescan, &auto_on());
    assert!(has_code(&r, "s:sonarr", &B::CapabilityDisabled));
}

#[test]
fn automatic_consolidation_needs_every_rule() {
    let i = input(&world());
    let s = live();
    let subject = sub(&rp(&completed(), MOVIE));
    let b = build_auto(&i, &s, &consolidate_req(&i), &auto_on()).unwrap();
    assert_eq!(b.plan.plan().origin, PlanOrigin::AutoPolicy);
    assert_eq!(preview(&b.plan, None).max_tier, RiskTier::T5);
    assert!(
        b.plan.approve_auto(NOW).is_ok(),
        "T-31: a proven exact copy is approved by policy"
    );

    // Tier 5 must be implemented.
    let mut t4 = s.clone();
    t4.implemented_tier = RiskTier::T4;
    assert!(has_code(
        &build_auto(&i, &t4, &consolidate_req(&i), &auto_on()),
        &subject,
        &B::NotInBuild
    ));

    // A3 / T-20: recent files are refused.
    let mut young = s.clone();
    young.auto_min_age_hours = 100;
    assert!(has_code(
        &build_auto(&i, &young, &consolidate_req(&i), &auto_on()),
        &subject,
        &B::Recent
    ));

    // A4 / T-20: a path involved in a failed consolidation is refused.
    let mut failed = auto_on();
    failed.failed_paths.insert(rp(&media(), LIB));
    assert!(has_code(
        &build_auto(&i, &s, &consolidate_req(&i), &failed),
        &subject,
        &B::FailedConsolidation
    ));
    let mut failed = auto_on();
    failed.failed_subjects.insert(SubjectKey(subject.clone()));
    assert!(has_code(
        &build_auto(&i, &s, &consolidate_req(&i), &failed),
        &subject,
        &B::FailedConsolidation
    ));

    // A2: any scan error in a touched root blocks automation, even far from the target.
    let mut o = world();
    o.inventories[1]
        .errors
        .push(removarr_domain::fs::ScanErrorRecord {
            path: rp(&media(), "elsewhere"),
            io: removarr_domain::fs::IoKind::PermissionDenied,
            message: "denied".into(),
        });
    let i2 = input(&o);
    let pair = SubjectKey::pair(
        sig(EntryKind::File, 1, 10, 0, 1, 0).phys(),
        sig(EntryKind::File, 1, 11, 0, 1, 0).phys(),
    )
    .0;
    let dup = req(
        C::Duplicate,
        vec![id_of(&i2, C::Duplicate, &pair)],
        A::Consolidate,
    );
    assert!(has_code(
        &build_auto(&i2, &s, &dup, &auto_on()),
        &pair,
        &B::ScanErrorUnder
    ));
    // The manual plan is still possible (the error is not near the pair).
    assert!(build(&i2, &s, &dup).is_ok());

    // A2: an unmapped torrent anywhere blocks automation.
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        let mut t = Tor::new(
            'q',
            &completed(),
            "uploading",
            1.0,
            "Other.mkv",
            &[("Other.mkv", "Other.mkv")],
        );
        t.info.save_path = MappedPath::NoRule {
            raw: "/srv/other".into(),
        };
        t = t.unmapped_files();
        snap.torrents.push(t.info.clone());
        snap.files.insert(t.info.hash.clone(), t.files);
    }
    let i3 = input(&o);
    assert!(has_code(
        &build_auto(&i3, &s, &consolidate_req(&i3), &auto_on()),
        &subject,
        &B::UnmappedTorrents
    ));

    // A5: a non-allowlisted torrent blocks.
    let mut o = world();
    if let QbitObservation::Available(snap) = &mut o.qbit {
        snap.torrents[0].state = removarr_domain::torrent::TorrentState::parse("checkingUP");
    }
    let i4 = input(&o);
    let r = build_auto(&i4, &s, &consolidate_req(&i4), &auto_on());
    assert!(has_code(&r, &subject, &B::TorrentProtectedState));

    // A6: a semantic candidate whose samples were not checked is not enough.
    let mut o = world();
    o.matches[0].content = ContentProof::Semantic { score_milli: 900 };
    let i5 = input(&o);
    let r = build_auto(&i5, &s, &consolidate_req(&i5), &auto_on());
    assert!(has_code(&r, &subject, &B::ScanIncomplete));
}

#[test]
fn automatic_phases_select_only_eligible_candidates() {
    let i = input(&world());
    let reqs = auto_requests(AutoPhase::Consolidations, &i);
    let cats: Vec<_> = reqs
        .iter()
        .map(|r| match r {
            PlanRequest::Issues {
                category, action, ..
            } => (*category, *action),
            _ => panic!("issue requests only"),
        })
        .collect();
    assert_eq!(
        cats,
        [
            (C::HardlinkMissing, A::Consolidate),
            (C::Duplicate, A::Consolidate)
        ]
    );
    let del = auto_requests(AutoPhase::Deletions, &i);
    assert!(del.iter().all(|r| matches!(
        r,
        PlanRequest::Issues {
            action: A::DeleteReplaced,
            ..
        }
    )));
}

#[test]
fn service_commands_carry_their_verification_reads() {
    let mut o = world();
    o.arr = vec![arr_ok(ArrService::Sonarr, vec![], vec![])];
    let mut i = input(&o);
    i.session.services.push(ok(ServiceKind::Jellyfin));
    let series = arr_media(ArrService::Sonarr, 1);
    i.arr.insert(
        (ArrService::Sonarr, 1),
        Ok(facts(
            arr_snapshot(&series, vec![], None, 0),
            vec![],
            vec![],
            vec![],
        )),
    );
    let monitor = PlanRequest::Media {
        media: series.clone(),
        action: removarr_workflows::plans::MediaAction::SetMonitored { monitored: false },
    };
    let b = build(&i, &live(), &monitor).unwrap();
    assert_eq!(
        action(&b, K::ArrSetMonitored).verification,
        [VerificationStep::ArrMonitored {
            media: series,
            monitored: false
        }]
    );
    let jf = PlanRequest::ServiceRescan {
        service: ServiceKind::Jellyfin,
        media: None,
    };
    let b = build(&i, &live(), &jf).unwrap();
    assert_eq!(
        action(&b, K::JellyfinRefresh).verification,
        [VerificationStep::JellyfinRefreshed { item_id: None }]
    );
    assert!(removarr_workflows::plans::check_plan(b.plan.plan()).is_empty());
}
