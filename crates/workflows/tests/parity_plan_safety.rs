#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pass 5 cross-cutting plan safety checks: media-root files are never deletion targets, bulk
//! equals one-by-one for every category, the ignore list holds in bulk and automatic requests,
//! and plan hashes are stable. Pure inputs, no I/O.

mod common;
mod plans_common;

use common::*;
use plans_common::*;
use removarr_domain::evidence::ContentProof;
use removarr_domain::fs::EntryKind;
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::plan::{ActionKind as K, ActionTarget};
use removarr_workflows::issues::{Observation, QbitObservation};
use removarr_workflows::plans::{AutoPhase, IssueAction as A, PlanRequest, auto_requests};

const MOVIE: &str = "Movie.A.2020.mkv";
const LIB: &str = "movies/A/Movie.A.2020.mkv";

fn world() -> Observation {
    let (c, m, i) = (completed(), media(), incomplete());
    let comp = Inv::new(c.clone())
        .file(MOVIE, 10, 1000)
        .dir("Orphan.Rel", 2)
        .file("Orphan.Rel/Orphan.Rel.mkv", 20, 5000)
        .dir("stray", 4)
        .file("stray/readme.txt", 30, 64)
        .file("stray/other.txt", 31, 64)
        .build();
    let med = Inv::new(m.clone())
        .dir("movies", 5)
        .dir("movies/A", 6)
        .file(LIB, 11, 1000)
        .file("movies/A/notes.txt", 12, 10)
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

fn simple_actions() -> [A; 6] {
    [
        A::Consolidate,
        A::Delete,
        A::DeleteReplaced,
        A::CleanupStalled,
        A::FixMode,
        A::RemoveQueueItem,
    ]
}

#[test]
fn no_plan_from_any_issue_and_action_deletes_under_the_media_root() {
    let i = input(&world());
    let s = live();
    let mut built = 0;
    for issue in &i.issues {
        for action in simple_actions() {
            if !action.accepts(issue.category) {
                continue;
            }
            let Ok(b) = build(&i, &s, &req(issue.category, vec![issue.id.clone()], action)) else {
                continue;
            };
            built += 1;
            for a in &b.plan.plan().actions {
                if let ActionTarget::Path { path } = &a.target {
                    assert_ne!(
                        path.root.kind(),
                        RootKind::Media,
                        "{:?} of {:?} targets the media root",
                        a.kind,
                        issue.category
                    );
                }
            }
        }
    }
    assert!(built > 0, "the world must produce at least one plan");
    // The unlinked media file is information only: no deletion request can name it.
    let notes = rp(&media(), "movies/A/notes.txt");
    assert!(
        i.issues
            .iter()
            .filter(|x| x.subject.as_str() == sub(&notes))
            .all(|x| !A::Delete.accepts(x.category) || x.category == C::HardlinkMissing)
    );
}

#[test]
fn bulk_verdicts_equal_the_one_by_one_verdicts_for_every_category() {
    let i = input(&world());
    let s = live();
    for action in simple_actions() {
        for cat in [
            C::OrphanCompleted,
            C::Junk,
            C::IncompleteOrphan,
            C::HardlinkMissing,
        ] {
            if !action.accepts(cat) {
                continue;
            }
            let ids: Vec<IssueId> = i
                .issues
                .iter()
                .filter(|x| x.category == cat)
                .map(|x| x.id.clone())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let bulk = build(&i, &s, &req(cat, ids.clone(), action));
            let mut unit_targets = Vec::new();
            let mut unit_excluded = Vec::new();
            for id in ids {
                let unit = build(&i, &s, &req(cat, vec![id], action));
                if let Ok(b) = &unit {
                    unit_targets.extend(
                        b.plan
                            .plan()
                            .actions
                            .iter()
                            .map(|a| (a.kind, a.target.clone())),
                    );
                }
                unit_excluded.extend(excluded_codes(&unit));
            }
            // Empty-parent cleanup depends on the whole selection (a directory is only emptied by
            // the union), so it is compared separately from the per-item verdicts.
            let mut bulk_targets: Vec<_> = match &bulk {
                Ok(b) => b
                    .plan
                    .plan()
                    .actions
                    .iter()
                    .map(|a| (a.kind, a.target.clone()))
                    .collect(),
                Err(_) => vec![],
            };
            bulk_targets.retain(|t| t.0 != removarr_domain::plan::ActionKind::RemoveEmptyParent);
            bulk_targets.sort_by_key(|t| format!("{t:?}"));
            unit_targets.retain(|t| t.0 != removarr_domain::plan::ActionKind::RemoveEmptyParent);
            unit_targets.sort_by_key(|t| format!("{t:?}"));
            unit_targets.dedup();
            assert_eq!(bulk_targets, unit_targets, "{cat:?} {action:?}");
            let mut be = excluded_codes(&bulk);
            be.sort_by_key(|e| format!("{e:?}"));
            unit_excluded.sort_by_key(|e| format!("{e:?}"));
            unit_excluded.dedup();
            assert_eq!(be, unit_excluded, "{cat:?} {action:?} exclusions");
        }
    }
}

#[test]
fn the_ignore_list_holds_in_bulk_and_automatic_requests() {
    let mut o = world();
    let c = completed();
    let ignored = rp(&c, "stray/readme.txt");
    o.ignored.insert(SubjectKey::rooted(&ignored));
    let i = input(&o);
    let kept = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/other.txt")));
    let gone = id_of(&i, C::OrphanCompleted, &sub(&ignored));
    // Bulk: the ignored subject is rejected, the rest plans without it.
    assert!(
        build(
            &i,
            &live(),
            &req(
                C::OrphanCompleted,
                vec![kept.clone(), gone.clone()],
                A::Delete
            )
        )
        .is_err()
    );
    let b = build(&i, &live(), &req(C::OrphanCompleted, vec![kept], A::Delete)).unwrap();
    assert!(b.plan.plan().actions.iter().all(|a| a.target
        != ActionTarget::Path {
            path: ignored.clone()
        }));
    // Automatic: no phase ever selects it.
    for phase in [AutoPhase::Consolidations, AutoPhase::Deletions] {
        for r in auto_requests(phase, &i) {
            if let PlanRequest::Issues { issue_ids, .. } = r {
                assert!(!issue_ids.contains(&gone), "{phase:?}");
            }
        }
    }
}

#[test]
fn plan_hashes_are_stable_across_rebuilds_and_change_with_the_selection() {
    let i = input(&world());
    let c = completed();
    let a = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/readme.txt")));
    let b = id_of(&i, C::OrphanCompleted, &sub(&rp(&c, "stray/other.txt")));
    let s = live();
    let one = |ids: Vec<IssueId>| {
        build(&i, &s, &req(C::OrphanCompleted, ids, A::Delete))
            .unwrap()
            .plan
            .hash()
            .clone()
    };
    assert_eq!(one(vec![a.clone()]), one(vec![a.clone()]));
    assert_ne!(one(vec![a.clone()]), one(vec![b.clone()]));
    assert_ne!(one(vec![a.clone()]), one(vec![a, b]));
    let _ = K::DeleteCompletedFile;
}
