#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan building: selection rules, unit and bulk through one path, preconditions, verification,
//! folders and annexes, samples, companions, gains, stable hashes and plan-engine rules.

mod common;
mod plans_common;

use common::*;
use plans_common::*;
use removarr_domain::error::DomainError;
use removarr_domain::evidence::{ContentProof, EvidenceLevel};
use removarr_domain::fs::{EntryKind, ProtectedReason};
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C};
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{
    ActionKind as K, ActionTarget, ApprovalRequest, HashedPlan, Precondition, VerificationStep,
};
use removarr_domain::session::ServiceKind;
use removarr_domain::time::Timestamp;
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{
    IssueAction as A, PlanError, PlanRequest, SelectionFault, check_plan, preview,
};

const MOVIE: &str = "Movie.A.2020.mkv";
const LIB: &str = "movies/A/Movie.A.2020.mkv";

/// Completed: a torrent-held movie with a separate exact library copy, a torrent-less release
/// folder with annexes, a stray junk file. Incomplete: an orphan download.
fn world() -> Observation {
    let (c, m, i) = (completed(), media(), incomplete());
    let comp = Inv::new(c.clone())
        .file(MOVIE, 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .file("Orphan.Rel/Orphan.Rel.nfo", 21, 100)
        .dir("Orphan.Rel/Subs", 3)
        .file("Orphan.Rel/Subs/Orphan.Rel.srt", 22, 50)
        .dir("stray", 4)
        .file("stray/readme.txt", 30, 64)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/A", 6)
        .file(LIB, 11, 1000)
        .build();
    let inc = Inv::new(i.clone()).file("abandoned.mkv", 40, 4000).build();
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
    let mut o = obs(vec![comp, med, inc], QbitObservation::Available(snap));
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

#[test]
fn consolidation_plan_captures_preconditions_verification_and_gain() {
    let i = input(&world());
    let (c, m) = (completed(), media());
    let id = id_of(&i, C::HardlinkMissing, &sub(&rp(&c, MOVIE)));
    let b = build(
        &i,
        &live(),
        &req(C::HardlinkMissing, vec![id], A::Consolidate),
    )
    .unwrap();
    let p = b.plan.plan();
    assert_eq!(kinds(&b), [K::ConsolidateHardlink]);
    let a = &p.actions[0];
    assert_eq!(
        a.target,
        ActionTarget::Consolidate {
            source: rp(&c, MOVIE),
            target: rp(&m, LIB)
        },
        "completed source -> media target"
    );
    let entry = |path: &RootedPath| {
        a.preconditions.iter().any(|x| {
            matches!(x, Precondition::EntryUnchanged { path: q, compare_nlink: false, .. } if q == path)
        })
    };
    assert!(entry(&rp(&c, MOVIE)) && entry(&rp(&m, LIB)));
    assert!(
        a.preconditions
            .iter()
            .any(|x| matches!(x, Precondition::SameDevice { .. }))
    );
    assert!(
        a.preconditions
            .contains(&Precondition::ConfigFingerprint("cfg-1".into()))
    );
    assert!(
        a.preconditions
            .contains(&Precondition::ServiceAvailable(ServiceKind::Qbittorrent))
    );
    assert!(a.preconditions.iter().any(
        |x| matches!(x, Precondition::TorrentStates { states } if states.contains_key(&hash('a')))
    ));
    assert!(a.verification.contains(&VerificationStep::InodeEquals {
        path: rp(&m, LIB),
        phys: sig(EntryKind::File, 1, 10, 0, 1, 0).phys()
    }));
    assert!(a.verification.contains(&VerificationStep::RecheckVerified {
        hashes: [hash('a')].into()
    }));
    assert_eq!(a.evidence_level, EvidenceLevel::NameSize);
    assert_eq!(
        a.expected_gain.attributable, 4096,
        "the replaced library inode is freed"
    );
    assert_eq!(p.gains.attributable, 4096);
    assert_eq!(p.gains.net, 4096);
    assert_eq!(b.expected_gains.len(), 1);
    assert!(p.torrent_states.contains_key(&hash('a')));
    assert_eq!(p.lock_roots, [c, m].into());
    assert_eq!(p.required_count(), 1);
    assert!(check_plan(p).is_empty());
}

#[test]
fn same_selection_gives_the_same_plan_and_hash_in_any_order() {
    let i = input(&world());
    let c = completed();
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let r = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/readme.txt")));
    let s = live();
    let a = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![f.clone(), r.clone()], A::Delete),
    )
    .unwrap();
    let b = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![r.clone(), f.clone(), r], A::Delete),
    )
    .unwrap();
    assert_eq!(a.plan.hash(), b.plan.hash());
    assert_eq!(a.plan.plan(), b.plan.plan());
    // A stored copy rebuilds the same hash; a tampered one is refused.
    let (plan, hash) = a.plan.clone().into_parts();
    assert!(HashedPlan::from_parts(plan.clone(), hash.clone()).is_ok());
    let mut tampered = plan;
    tampered.actions[0].verification.clear();
    assert!(matches!(
        HashedPlan::from_parts(tampered, hash),
        Err(DomainError::PlanHashMismatch { .. })
    ));
}

#[test]
fn bad_ids_reject_the_whole_request() {
    let mut o = world();
    let c = completed();
    o.ignored
        .insert(SubjectKey::rooted(&rp(&c, "stray/readme.txt")));
    let i = input(&o);
    let folder = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let junk = id_of(&i, C::Junk, &sub(&rp(&c, "stray/readme.txt")));
    let ignored = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/readme.txt")));
    let s = live();
    let fault = |ids: Vec<IssueId>| match build(&i, &s, &req(C::OrphanCompleted, ids, A::Delete)) {
        Err(PlanError::RejectedSelection { reason, ids }) => (reason, ids),
        other => panic!("expected a rejected selection, got {other:?}"),
    };
    let unknown = IssueId("0".repeat(32));
    assert_eq!(
        fault(vec![folder.clone(), unknown.clone()]),
        (SelectionFault::UnknownOrResolved, vec![unknown])
    );
    assert_eq!(
        fault(vec![folder.clone(), junk.clone()]),
        (SelectionFault::WrongCategory, vec![junk])
    );
    assert_eq!(
        fault(vec![folder.clone(), ignored.clone()]),
        (SelectionFault::Ignored, vec![ignored])
    );
    let mut stale = i.clone();
    stale.issues.iter_mut().for_each(|x| {
        x.session =
            removarr_domain::ids::SessionId::new(removarr_domain::ids::RawUuid::from_u128(99))
    });
    assert!(matches!(
        build(
            &stale,
            &s,
            &req(C::OrphanCompleted, vec![folder.clone()], A::Delete)
        ),
        Err(PlanError::RejectedSelection {
            reason: SelectionFault::Stale,
            ..
        })
    ));
    // No implicit selection, and an action must fit the category.
    assert!(matches!(
        build(&i, &s, &req(C::OrphanCompleted, vec![], A::Delete)),
        Err(PlanError::InvalidRequest(_))
    ));
    assert!(matches!(
        build(
            &i,
            &s,
            &req(C::OrphanCompleted, vec![folder], A::Consolidate)
        ),
        Err(PlanError::InvalidRequest(_))
    ));
}

#[test]
fn bulk_excludes_and_lists_blocked_items_with_the_same_rules_as_unit() {
    let mut o = world();
    let c = completed();
    // A scan error on the stray directory blocks the file below it, not the folder.
    o.inventories[0]
        .errors
        .push(removarr_domain::fs::ScanErrorRecord {
            path: rp(&c, "stray"),
            io: removarr_domain::fs::IoKind::PermissionDenied,
            message: "denied".into(),
        });
    let i = input(&o);
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let r = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/readme.txt")));
    let s = live();
    let bulk = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![f.clone(), r.clone()], A::Delete),
    );
    let plan = bulk.as_ref().unwrap().plan.plan();
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(
        plan.actions[0].target,
        ActionTarget::Path {
            path: rp(&c, "Orphan.Rel")
        }
    );
    let readme = sub(&rp(&c, "stray/readme.txt"));
    assert!(has_code(&bulk, &readme, &B::ScanErrorUnder));
    // Unit planning of each item gives the same verdicts.
    let unit_r = build(&i, &s, &req(C::OrphanCompleted, vec![r], A::Delete));
    assert_eq!(excluded_codes(&unit_r), excluded_codes(&bulk));
    let unit_f = build(&i, &s, &req(C::OrphanCompleted, vec![f], A::Delete)).unwrap();
    assert_eq!(unit_f.plan.plan().actions[0].target, plan.actions[0].target);
}

#[test]
fn folder_deletion_is_all_or_nothing_and_previews_every_annex() {
    let i = input(&world());
    let c = completed();
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let b = build(&i, &live(), &req(C::OrphanCompleted, vec![f], A::Delete)).unwrap();
    assert_eq!(kinds(&b), [K::DeleteCompletedFolder]);
    let a = action(&b, K::DeleteCompletedFolder);
    let snap = a
        .preconditions
        .iter()
        .find_map(|p| match p {
            Precondition::DirUnchanged { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("folder snapshot");
    assert_eq!(snap.entries.len(), 5, "dir, video, nfo, Subs, srt");
    assert!(
        a.preconditions
            .contains(&Precondition::NoTorrentReferences {
                path: rp(&c, "Orphan.Rel")
            })
    );
    let pv = preview(&b.plan, None);
    assert_eq!(
        pv.actions[0].annexes,
        [
            rp(&c, "Orphan.Rel/Orphan.Rel.mkv"),
            rp(&c, "Orphan.Rel/Orphan.Rel.nfo"),
            rp(&c, "Orphan.Rel/Subs"),
            rp(&c, "Orphan.Rel/Subs/Orphan.Rel.srt"),
        ]
    );
    assert!(pv.irreversible);
    assert_eq!(a.expected_gain.logical, 5150);

    // One protected entry anywhere in the folder blocks the whole folder (no partial plan).
    let mut o = world();
    o.inventories[0]
        .protected
        .push(removarr_domain::fs::ProtectedZone {
            path: rp(&c, "Orphan.Rel/Subs"),
            reason: ProtectedReason::MountBoundary,
        });
    let i2 = input(&o);
    let f2 = id_of(&i2, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let r = build(&i2, &live(), &req(C::OrphanCompleted, vec![f2], A::Delete));
    assert!(matches!(r, Err(PlanError::NothingPlannable { .. })));
    assert!(has_code(&r, &sub(&rp(&c, "Orphan.Rel")), &B::ProtectedZone));
}

#[test]
fn samples_need_acknowledgement_in_folders_and_are_never_deleted_alone() {
    let c = completed();
    let mut o = world();
    o.inventories[0] = Inv::new(c.clone())
        .dir("Rel", 2)
        .file("Rel/Rel.mkv", 20, 5000)
        .dir("Rel/Sample", 3)
        .file("Rel/Sample/rel.sample.mkv", 21, 100)
        .file("lonely-sample.mkv", 22, 100)
        .build();
    let i = input(&o);
    let s = live();
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Rel")));
    let b = build(&i, &s, &req(C::OrphanCompleted, vec![f], A::Delete)).unwrap();
    let sample = rp(&c, "Rel/Sample/rel.sample.mkv");
    assert_eq!(
        b.plan.plan().samples,
        [sample.clone(), rp(&c, "Rel/Sample")]
            .into_iter()
            .filter(|p| removarr_domain::policy::is_sample(&p.rel))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let approval = |acks: Vec<RootedPath>| {
        b.plan.approve(
            &ApprovalRequest {
                plan_hash: b.plan.hash().clone(),
                confirmed_count: 1,
                confirmed_torrent_removals: 0,
                acknowledged_samples: acks,
                acknowledged_unrecoverable: vec![],
            },
            NOW,
        )
    };
    assert!(matches!(
        approval(vec![]),
        Err(DomainError::SamplesNotAcknowledged(_))
    ));
    let all = b.plan.plan().samples.clone();
    assert!(approval(all).is_ok());
    // A sample file on its own is never a deletion target.
    let lone = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "lonely-sample.mkv")));
    let r = build(&i, &s, &req(C::OrphanCompleted, vec![lone], A::Delete));
    assert!(has_code(&r, &sub(&rp(&c, "lonely-sample.mkv")), &B::Sample));
}

#[test]
fn empty_parents_are_removed_after_their_contents_deepest_first() {
    let c = completed();
    let mut o = world();
    o.inventories[0] = Inv::new(c.clone())
        .dir("old", 2)
        .dir("old/sub", 3)
        .file("old/sub/a.txt", 20, 10)
        .file("old/b.txt", 21, 10)
        .build();
    let i = input(&o);
    let s = live();
    let a = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "old/sub/a.txt")));
    let bb = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "old/b.txt")));
    let both = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![a.clone(), bb], A::Delete),
    )
    .unwrap();
    let p = both.plan.plan();
    assert_eq!(
        kinds(&both),
        [
            K::DeleteCompletedFile,
            K::DeleteCompletedFile,
            K::RemoveEmptyParent,
            K::RemoveEmptyParent
        ]
    );
    assert_eq!(
        p.actions[2].target,
        ActionTarget::Path {
            path: rp(&c, "old/sub")
        }
    );
    assert_eq!(
        p.actions[3].target,
        ActionTarget::Path {
            path: rp(&c, "old")
        }
    );
    assert_eq!(
        p.actions[3].depends_on,
        [0, 1, 2],
        "after every content removal"
    );
    assert_eq!(p.required_count(), 2, "companions are not typed");
    // Deleting only the deeper file leaves `old` (it still holds b.txt).
    let one = build(&i, &s, &req(C::OrphanCompleted, vec![a], A::Delete)).unwrap();
    assert_eq!(kinds(&one), [K::DeleteCompletedFile, K::RemoveEmptyParent]);
    assert_eq!(
        one.plan.plan().actions[1].target,
        ActionTarget::Path {
            path: rp(&c, "old/sub")
        }
    );
}

#[test]
fn torrent_task_removal_is_opt_in_and_needs_every_wanted_file_planned() {
    let i = input(&world());
    let c = completed();
    let s = live();
    let id = id_of(&i, C::HardlinkMissing, &sub(&rp(&c, MOVIE)));
    let base = build(
        &i,
        &s,
        &req(C::HardlinkMissing, vec![id.clone()], A::Delete),
    )
    .unwrap();
    assert_eq!(kinds(&base), [K::DeleteCompletedFile]);
    let with = PlanRequest::Issues {
        category: C::HardlinkMissing,
        issue_ids: vec![id],
        action: A::Delete,
        remove_torrent_tasks: true,
        include_specials: false,
    };
    let b = build(&i, &s, &with).unwrap();
    assert_eq!(kinds(&b), [K::DeleteCompletedFile, K::RemoveTorrentTask]);
    let task = action(&b, K::RemoveTorrentTask);
    assert_eq!(task.depends_on, [0]);
    assert!(
        task.verification
            .contains(&VerificationStep::TorrentAbsent {
                hashes: [hash('a')].into()
            })
    );
    assert_eq!(b.plan.plan().required_torrent_removals(), 1);
    assert_eq!(preview(&b.plan, None).torrent_removals, [hash('a')]);

    // A multi-file torrent with one file left is never removed.
    let mut o = world();
    o.inventories[0] = Inv::new(c.clone())
        .dir("Pack", 2)
        .file("Pack/E01.mkv", 50, 100)
        .file("Pack/E02.mkv", 51, 100)
        .build();
    o.qbit = QbitObservation::Available(snapshot(
        4,
        vec![Tor::new(
            'p',
            &c,
            "uploading",
            1.0,
            "Pack",
            &[
                ("Pack/E01.mkv", "Pack/E01.mkv"),
                ("Pack/E02.mkv", "Pack/E02.mkv"),
            ],
        )],
    ));
    o.matches.clear();
    let i2 = input(&o);
    let e1 = id_of(&i2, C::HardlinkMissing, &sub(&rp(&c, "Pack/E01.mkv")));
    let r = PlanRequest::Issues {
        category: C::HardlinkMissing,
        issue_ids: vec![e1],
        action: A::Delete,
        remove_torrent_tasks: true,
        include_specials: false,
    };
    assert_eq!(
        kinds(&build(&i2, &s, &r).unwrap()),
        [K::DeleteCompletedFile]
    );
}

#[test]
fn gains_count_each_inode_once_and_only_when_every_link_is_removed() {
    let c = completed();
    let mut o = world();
    o.inventories[0] = Inv::new(c.clone())
        .file_nlink("t.txt", 70, 24_576, 2)
        .dir("dupe", 2)
        .file_nlink("dupe/t.txt", 70, 24_576, 2)
        .build();
    let i = input(&o);
    let s = live();
    let one = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "t.txt")));
    let two = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "dupe/t.txt")));
    let single = build(
        &i,
        &s,
        &req(C::OrphanCompleted, vec![one.clone()], A::Delete),
    )
    .unwrap();
    assert_eq!(
        single.plan.plan().gains.attributable,
        0,
        "the other link keeps the inode"
    );
    assert_eq!(single.plan.plan().gains.logical, 24_576);
    let both = build(&i, &s, &req(C::OrphanCompleted, vec![one, two], A::Delete)).unwrap();
    let p = both.plan.plan();
    assert_eq!(p.gains.attributable, 24_576, "counted once");
    assert_eq!(p.gains.logical, 2 * 24_576);
    let per_action: u64 = p.actions.iter().map(|a| a.expected_gain.attributable).sum();
    assert_eq!(
        per_action, p.gains.attributable,
        "no double count across actions"
    );
    assert_eq!(both.expected_gains.len(), 1);
    let g = &both.expected_gains[0];
    assert_eq!(g.bytes, 24_576);
    assert_eq!(g.gain_id.0.len(), 32);
}

#[test]
fn incomplete_orphans_and_stalled_downloads_are_planned_under_incomplete_only() {
    let (c, i_root) = (completed(), incomplete());
    let mut o = world();
    o.inventories[2] = Inv::new(i_root.clone())
        .file("abandoned.mkv", 40, 4000)
        .file("Oscar.mkv", 41, 8000)
        .build();
    let snap = snapshot(
        4,
        vec![
            Tor::new('a', &c, "uploading", 1.0, MOVIE, &[(MOVIE, MOVIE)]),
            Tor::new(
                'd',
                &i_root,
                "stalledDL",
                0.4,
                "Oscar.mkv",
                &[("Oscar.mkv", "Oscar.mkv")],
            )
            .added_days_ago(30),
        ],
    );
    o.qbit = QbitObservation::Available(snap);
    let i = input(&o);
    let s = live();
    let orphan = id_of(&i, C::IncompleteOrphan, &sub(&rp(&i_root, "abandoned.mkv")));
    let b = build(&i, &s, &req(C::IncompleteOrphan, vec![orphan], A::Delete)).unwrap();
    let a = action(&b, K::DeleteIncompleteOrphan);
    assert!(
        a.preconditions
            .contains(&Precondition::NoTorrentReferences {
                path: rp(&i_root, "abandoned.mkv")
            })
    );
    let stalled = id_of(&i, C::IncompleteStalled, &tid('d'));
    let b = build(
        &i,
        &s,
        &req(C::IncompleteStalled, vec![stalled], A::CleanupStalled),
    )
    .unwrap();
    let a = action(&b, K::CleanupStalledIncomplete);
    assert_eq!(
        a.target,
        ActionTarget::Torrent {
            hash: hash('d'),
            data_paths: vec![rp(&i_root, "Oscar.mkv")]
        }
    );
    assert!(a.verification.contains(&VerificationStep::TorrentAbsent {
        hashes: [hash('d')].into()
    }));
    assert!(a.verification.contains(&VerificationStep::Absent {
        path: rp(&i_root, "Oscar.mkv")
    }));
    // A busy state is refused.
    let mut o2 = o.clone();
    if let QbitObservation::Available(s2) = &mut o2.qbit {
        s2.torrents[1].state = removarr_domain::torrent::TorrentState::parse("checkingDL");
    }
    let i2 = input(&o2);
    let stalled = id_of(&i2, C::IncompleteStalled, &tid('d'));
    let r = build(
        &i2,
        &s,
        &req(C::IncompleteStalled, vec![stalled], A::CleanupStalled),
    );
    assert!(has_code(&r, &tid('d'), &B::TorrentProtectedState));
}

#[test]
fn fix_mode_adds_owner_bits_only_on_entries_the_process_owns() {
    let m = media();
    let mut o = world();
    o.inventories[1] = Inv::new(m.clone())
        .dir_mode("locked", 5, 0o555)
        .file("locked/x.mkv", 11, 10)
        .build();
    let i = input(&o);
    let s = live();
    let id = id_of(&i, C::Permission, &sub(&rp(&m, "locked")));
    let b = build(&i, &s, &req(C::Permission, vec![id], A::FixMode)).unwrap();
    let a = action(&b, K::FixMode);
    assert_eq!(
        a.target,
        ActionTarget::FixMode {
            path: rp(&m, "locked"),
            mode: 0o755
        }
    );
    assert!(a.verification.contains(&VerificationStep::ModeEquals {
        path: rp(&m, "locked"),
        mode: 0o755
    }));
    // An entry the process does not own is never changed.
    let mut other = s.clone();
    other.process_uid = 4242;
    let id = id_of(&i, C::Permission, &sub(&rp(&m, "locked")));
    let r = build(&i, &other, &req(C::Permission, vec![id], A::FixMode));
    assert!(has_code(&r, &sub(&rp(&m, "locked")), &B::PermissionDenied));
}

#[test]
fn unfinished_or_erroring_scan_areas_and_unknown_ownership_block_deletions() {
    let c = completed();
    let mut o = world();
    o.inventories[0].finished = false;
    let i = input(&o);
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let r = build(&i, &live(), &req(C::OrphanCompleted, vec![f], A::Delete));
    assert!(has_code(
        &r,
        &sub(&rp(&c, "Orphan.Rel")),
        &B::ScanIncomplete
    ));

    let mut o = world();
    o.qbit = QbitObservation::Unavailable {
        reason: "down".into(),
    };
    let i = input(&o);
    let f = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "Orphan.Rel")));
    let r = build(&i, &live(), &req(C::OrphanCompleted, vec![f], A::Delete));
    assert!(has_code(
        &r,
        &sub(&rp(&c, "Orphan.Rel")),
        &B::ServiceUnavailable(ServiceKind::Qbittorrent)
    ));
}

#[test]
fn plans_never_come_from_unfinished_sessions() {
    let mut i = input(&world());
    i.session.status = removarr_domain::session::SessionStatus::Cancelled;
    let r = build(
        &i,
        &live(),
        &req(C::Junk, vec![IssueId("x".into())], A::Delete),
    );
    assert!(matches!(r, Err(PlanError::SessionNotPlannable(_))));
}

#[test]
fn plan_rules_require_direction_pins_and_verification() {
    let i = input(&world());
    let (c, m) = (completed(), media());
    let id = id_of(&i, C::HardlinkMissing, &sub(&rp(&c, MOVIE)));
    let b = build(
        &i,
        &live(),
        &req(C::HardlinkMissing, vec![id], A::Consolidate),
    )
    .unwrap();
    assert!(check_plan(b.plan.plan()).is_empty());
    let mut reversed = b.plan.plan().clone();
    reversed.actions[0].target = ActionTarget::Consolidate {
        source: rp(&m, LIB),
        target: rp(&c, MOVIE),
    };
    assert!(
        check_plan(&reversed)
            .iter()
            .any(|v| v.contains("completed -> media"))
    );
    let mut unverified = b.plan.plan().clone();
    unverified.actions[0].verification.clear();
    assert!(
        check_plan(&unverified)
            .iter()
            .any(|v| v.contains("verification"))
    );
    let mut unpinned = b.plan.plan().clone();
    unpinned.actions[0]
        .preconditions
        .retain(|p| !matches!(p, Precondition::EntryUnchanged { .. }));
    assert!(check_plan(&unpinned).iter().any(|v| v.contains("pinned")));
    let mut no_config = b.plan.plan().clone();
    no_config.actions[0]
        .preconditions
        .retain(|p| !matches!(p, Precondition::ConfigFingerprint(_)));
    assert!(!check_plan(&no_config).is_empty());
    // Plans expire after the configured TTL.
    assert_eq!(b.plan.plan().expires_at, Timestamp(NOW.0 + 24 * 3_600_000));
}
