#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The legacy parity fixture (docs/legacy-parity.md section 7) through the real confined
//! filesystem, the classifier and the plan engine: T-15 (release folder unit with its annexes,
//! plan side), T-03 (an inode counts only when every link is removed, plan side) and the
//! automatic parent cleanup under completed.

mod common;
mod plans_common;

use std::collections::BTreeMap;

use common::*;
use plans_common::*;
use removarr_config::{RootPaths, Roots};
use removarr_domain::fs::{EntryKind, RootInventory};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::path::{RootId, RootedPath};
use removarr_domain::plan::{ActionKind as K, ActionTarget, Precondition};
use removarr_domain::ports::{FsInventory, ScanControl};
use removarr_fs::{ConfinedFs, FsLimits};
use removarr_testkit::fixture::ParityFixture;
use removarr_workflows::issues::{IssueSettings, QbitObservation, classify};
use removarr_workflows::plans::{IssueAction as A, PlanInput, PlanSettings, preview};

const INDIA: &str = "India.Movie.2014.1080p.WEB.x264-GRP";
const TANGO: &str = "Tango.Movie.2007.1080p.WEB.x264-GRP.mkv";

fn fixture_input(fx: &ParityFixture) -> (PlanInput, PlanSettings) {
    let roots = Roots::validate(&RootPaths {
        completed: Some(fx.completed().to_str().unwrap().into()),
        incomplete: Some(fx.incomplete().to_str().unwrap().into()),
        media: vec![fx.medias().to_str().unwrap().into()],
    })
    .unwrap();
    let fs = ConfinedFs::new(&roots, FsLimits::default());
    let media = RootId::media(fx.medias().to_str().unwrap());
    let invs: Vec<RootInventory> = [completed(), incomplete(), media.clone()]
        .iter()
        .map(|r| fs.scan_root(r, &ScanControl::default()).unwrap())
        .collect();
    // T_T holds the root copy FX-34 only (section 7.3).
    let snap = snapshot(
        4,
        vec![Tor::new(
            'a',
            &completed(),
            "uploading",
            1.0,
            TANGO,
            &[(TANGO, TANGO)],
        )],
    );
    let mut o = obs(invs, QbitObservation::Available(snap));
    o.configured_roots = vec![completed(), incomplete(), media];
    let mut files = IssueSettings::new(fx.creator_ids().unwrap());
    files.video_extensions = [".mkv".to_string()].into_iter().collect();
    let c = classify(&o, &files).unwrap();
    let mut s = live();
    s.process_uid = files.expected_owner.0;
    s.files = files;
    let i = PlanInput {
        session: session_of(&o),
        observation: o,
        issues: c.issues,
        arr: BTreeMap::new(),
        search: None,
    };
    (i, s)
}

fn p(rel: &str) -> RootedPath {
    rp(&completed(), rel)
}

#[test]
fn t15_release_folder_unit_is_planned_whole_with_every_annex_including_symlinks() {
    let fx = ParityFixture::build().unwrap();
    let (i, s) = fixture_input(&fx);
    let id = id_of(&i, C::OrphanCompleted, &SubjectKey::rooted(&p(INDIA)).0);
    let b = build(&i, &s, &req(C::OrphanCompleted, vec![id], A::Delete)).unwrap();
    assert_eq!(kinds(&b), [K::DeleteCompletedFolder]);
    let pv = preview(&b.plan, None);
    assert_eq!(
        pv.actions[0].annexes,
        [
            p(&format!("{INDIA}/{INDIA}.mkv")),
            p(&format!("{INDIA}/link-to-outside.txt"))
        ]
    );
    let snap = b.plan.plan().actions[0]
        .preconditions
        .iter()
        .find_map(|x| match x {
            Precondition::DirUnchanged { snapshot } => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert!(
        snap.entries
            .values()
            .any(|sig| sig.kind == EntryKind::Symlink),
        "the symlink is an annex (unlinked by the runner, never followed)"
    );
    assert_eq!(
        b.plan.plan().actions[0].expected_gain.logical,
        16_384,
        "the symlink has no size"
    );
}

#[test]
fn t03_an_inode_still_linked_elsewhere_frees_nothing_and_parents_are_cleaned() {
    let fx = ParityFixture::build().unwrap();
    let (i, s) = fixture_input(&fx);
    // FX-35 shares I_T with the torrent-held FX-34: deleting it frees nothing.
    let id = id_of(
        &i,
        C::OrphanCompleted,
        &SubjectKey::rooted(&p("tango-dupe")).0,
    );
    let b = build(&i, &s, &req(C::OrphanCompleted, vec![id], A::Delete)).unwrap();
    let plan = b.plan.plan();
    assert_eq!(plan.gains.logical, 24_576);
    assert_eq!(plan.gains.attributable, 0);
    assert!(b.expected_gains.is_empty());

    // The three stray junk files: their directory is left empty and removed afterwards.
    let ids = [
        "stray/readme.txt",
        "stray/archive.rar",
        "stray/Unrelated.Film.2010.srt",
    ]
    .iter()
    .map(|r| id_of(&i, C::Junk, &SubjectKey::rooted(&p(r)).0))
    .collect();
    let b = build(&i, &s, &req(C::Junk, ids, A::Delete)).unwrap();
    assert_eq!(
        kinds(&b),
        [
            K::DeleteJunk,
            K::DeleteJunk,
            K::DeleteJunk,
            K::RemoveEmptyParent
        ]
    );
    let last = b.plan.plan().actions.last().unwrap();
    assert_eq!(last.target, ActionTarget::Path { path: p("stray") });
    assert_eq!(last.depends_on, [0, 1, 2]);
    assert!(
        b.plan.plan().gains.attributable > 0,
        "torrent-less single-link junk frees its blocks"
    );
}
