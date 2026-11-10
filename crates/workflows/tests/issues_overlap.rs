#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Overlap between categories, precedence, ignore list, double counting, determinism and the
//! explanation structure.

mod common;

use std::collections::BTreeSet;

use common::*;
use removarr_domain::canonical::sha256_hex;
use removarr_domain::evidence::{ContentProof, EvidenceLevel};
use removarr_domain::fs::PhysicalId;
use removarr_domain::ids::{IssueId, SubjectKey};
use removarr_domain::issue::{BlockerCode as B, IssueCategory as C, Severity};
use removarr_workflows::issues::{
    CandidateFact, Fact, ProvenFact, QbitObservation, RecommendationKind, explain, is_ignored,
};

fn qb(snapshot: removarr_domain::torrent::TorrentSnapshot) -> QbitObservation {
    QbitObservation::Available(snapshot)
}

fn bravo(content: ContentProof) -> removarr_workflows::issues::Observation {
    let c = Inv::new(completed()).file("Bravo.mkv", 20, 81_920).build();
    let m = Inv::new(media())
        .dir("movies", 100)
        .file("movies/Bravo.mkv", 21, 81_920)
        .build();
    let (sa, sb) = (c.entries[0].sig, m.entries[1].sig);
    let t = Tor::new(
        'b',
        &completed(),
        "stalledUP",
        1.0,
        "Bravo.mkv",
        &[("Bravo.mkv", "Bravo.mkv")],
    );
    let mut o = obs(vec![c, m], qb(snapshot(5, vec![t])));
    o.matches = vec![candidate(
        rp(&completed(), "Bravo.mkv"),
        rp(&media(), "movies/Bravo.mkv"),
        sa,
        sb,
        content,
    )];
    o
}

#[test]
fn ids_are_deterministic_and_derive_from_category_and_subject_only() {
    let r1 = run(&bravo(ContentProof::NameSize));
    let r2 = run(&bravo(ContentProof::Identical { compared_at: NOW }));
    let i1 = one(&r1, C::HardlinkMissing, "p:completed:Bravo.mkv");
    let i2 = one(&r2, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(i1.id, i2.id, "evidence changes never change the id");
    let expected = sha256_hex(b"issue\0hardlink_missing\0p:completed:Bravo.mkv");
    assert_eq!(i1.id.as_str(), &expected[..32]);
    assert_eq!(i1.id, i1.expected_id());
    // Category sensitive: same subject, other category, other id.
    let other = IssueId::derive(
        C::OrphanCompleted,
        &SubjectKey("p:completed:Bravo.mkv".into()),
    );
    assert_ne!(i1.id, other);
    // One issue per (category, subject): ids are unique in a result.
    let ids: BTreeSet<_> = r1.issues.iter().map(|i| &i.id).collect();
    assert_eq!(ids.len(), r1.issues.len());
}

#[test]
fn input_order_never_changes_the_output() {
    let mut a = bravo(ContentProof::NameSize);
    let b = a.clone();
    a.inventories.reverse();
    for inv in &mut a.inventories {
        inv.entries.reverse();
    }
    assert_eq!(run(&a).issues, run(&b).issues);
}

#[test]
fn an_item_can_be_in_several_categories_with_distinct_ids() {
    // A torrent-less video with no library copy is both hardlink_missing and orphan_completed.
    let c = Inv::new(completed()).file("Echo.mkv", 40, 32_768).build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let h = one(&r, C::HardlinkMissing, "p:completed:Echo.mkv");
    let o = one(&r, C::OrphanCompleted, "p:completed:Echo.mkv");
    assert_ne!(h.id, o.id);
    assert_eq!(h.subject, o.subject);
}

#[test]
fn potential_space_counts_each_inode_once_across_categories() {
    // Echo: hardlink_missing (KeepManual, no candidate: potential 0) + orphan_completed (32768).
    // Bravo: hardlink_missing and duplicate both count the same library inode.
    let mut o = bravo(ContentProof::NameSize);
    let c2 = Inv::new(completed())
        .file("Bravo.mkv", 20, 81_920)
        .file("Echo.mkv", 40, 32_768)
        .build();
    o.matches[0].sig_a = c2.entries[0].sig;
    o.inventories[0] = c2;
    let r = run(&o);
    let a = &r.aggregates;
    let hm = a.per_category[&C::HardlinkMissing];
    let dup = a.per_category[&C::Duplicate];
    let orph = a.per_category[&C::OrphanCompleted];
    assert_eq!(hm.potential_bytes, 81_920);
    assert_eq!(dup.potential_bytes, 81_920);
    assert_eq!(
        orph.potential_bytes, 32_768,
        "Bravo is held by a torrent: only Echo is an orphan"
    );
    // The global figure is the union of inodes, not the sum of the categories.
    let union: u64 = [
        PhysicalId { dev: 1, ino: 21 },
        PhysicalId { dev: 1, ino: 40 },
    ]
    .iter()
    .map(|p| r.ledger.allocated(p))
    .sum();
    assert_eq!(a.potential_bytes, union);
    assert_eq!(a.potential_bytes, 81_920 + 32_768);
    assert_eq!(
        a.naive_potential_sum,
        81_920 + 81_920 + 32_768,
        "the naive sum counts the library inode twice"
    );
}

#[test]
fn two_paths_of_one_inode_count_once_and_only_when_every_link_is_selected() {
    // FX-34 / FX-35 with no torrent at all: the unit holds one of two links.
    let c = Inv::new(completed())
        .dir("dupe", 1)
        .file_nlink("Tango.mkv", 30, 24_576, 2)
        .file_nlink("dupe/Tango.mkv", 30, 24_576, 2)
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let file = one(&r, C::OrphanCompleted, "p:completed:Tango.mkv");
    let unit = one(&r, C::OrphanCompleted, "p:completed:dupe");
    assert_eq!(file.potential_bytes, 0, "one link of two");
    assert_eq!(unit.potential_bytes, 0, "one link of two");
    assert_eq!(
        r.aggregates.potential_bytes, 0,
        "deleting only one path frees nothing"
    );
}

#[test]
fn ledger_shared_bytes_count_each_hardlinked_inode_once() {
    let c = Inv::new(completed())
        .file_nlink("A.mkv", 1, 8192, 2)
        .file_nlink("B.mkv", 2, 4096, 2)
        .file("C.mkv", 3, 4096)
        .build();
    let m = Inv::new(media())
        .file_nlink("A.mkv", 1, 8192, 2)
        .file_nlink("B.mkv", 2, 4096, 2)
        .build();
    let r = run(&obs(vec![c, m], qb(snapshot(5, vec![]))));
    assert_eq!(r.aggregates.shared_bytes, 8192 + 4096);
}

#[test]
fn samples_are_protected_in_every_category() {
    let c = Inv::new(completed())
        .dir("R", 1)
        .dir("R/Sample", 2)
        .file("R/R.mkv", 3, 4096)
        .file("R/Sample/r-sample.mkv", 4, 1024)
        .file("lone-sample.mkv", 5, 1024)
        .file("movie.sample.nfo", 6, 10)
        .file("sample.rar", 7, 10)
        .build();
    let i = Inv::new(incomplete())
        .file("sample.partial.mkv", 8, 10)
        .build();
    let m = Inv::new(media()).file("x-sample.mkv", 9, 10).build();
    let r = run(&obs(vec![c, i, m], qb(snapshot(5, vec![]))));
    assert!(of(&r, C::Junk).is_empty(), "samples are never junk");
    for i in &r.issues {
        let sample_path = match &i.unit {
            removarr_domain::issue::Unit::File(p) => removarr_domain::policy::is_sample(&p.rel),
            _ => false,
        };
        if sample_path
            && matches!(
                i.category,
                C::OrphanCompleted
                    | C::IncompleteOrphan
                    | C::HardlinkMissing
                    | C::MediaWithoutSource
            )
        {
            assert!(
                has(i, &B::Sample),
                "{} {}",
                i.category.as_str(),
                i.subject.as_str()
            );
        }
    }
    assert!(
        has(one(&r, C::OrphanCompleted, "p:completed:R"), &B::Sample),
        "folder unit with a sample inside"
    );
}

// ---------------------------------------------------------------- ignore list

#[test]
fn ignored_subjects_leave_lists_and_totals_but_stay_in_the_ignored_view() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 40, 32_768)
        .file("Foxtrot.mkv", 41, 4096)
        .build();
    let mut o = obs(vec![c, Inv::new(media()).build()], qb(snapshot(5, vec![])));
    let base = run(&o);
    o.ignored.insert(SubjectKey("p:completed:Echo.mkv".into()));
    let r = run(&o);
    let echo = one(&r, C::OrphanCompleted, "p:completed:Echo.mkv");
    assert!(is_ignored(echo) && has(echo, &B::Ignored));
    assert!(
        !echo.is_unblocked(),
        "never in bulk actions or automatic mode"
    );
    let active: Vec<_> = r.active().map(|i| i.subject.as_str()).collect();
    assert!(!active.contains(&"p:completed:Echo.mkv"));
    assert!(
        r.ignored()
            .any(|i| i.subject.as_str() == "p:completed:Echo.mkv")
    );
    // Both categories of the subject are ignored together (the subject key is shared).
    assert_eq!(r.ignored().count(), 2);
    // Totals exclude them.
    assert_eq!(r.aggregates.ignored_count, 2);
    assert!(r.aggregates.potential_bytes < base.aggregates.potential_bytes);
    assert_eq!(r.aggregates.per_category[&C::OrphanCompleted].count, 1);
    assert_eq!(r.aggregates.per_category[&C::OrphanCompleted].ignored, 1);
    // Restoring brings the item back unchanged.
    o.ignored.clear();
    assert_eq!(run(&o).issues, base.issues);
}

#[test]
fn ignoring_a_member_ignores_its_folder_unit_and_a_pair_path_ignores_the_pair() {
    let c = Inv::new(completed())
        .dir("R", 1)
        .file("R/R.mkv", 3, 4096)
        .file("R/R.nfo", 4, 10)
        .build();
    let mut o = obs(vec![c, Inv::new(media()).build()], qb(snapshot(5, vec![])));
    o.ignored.insert(SubjectKey("p:completed:R/R.nfo".into()));
    let r = run(&o);
    let unit = one(&r, C::OrphanCompleted, "p:completed:R");
    assert!(
        is_ignored(unit),
        "an ignored annex must never be deleted with its folder"
    );
    assert!(
        unit.blockers
            .iter()
            .any(|b| b.code == B::Ignored && b.detail.contains("member"))
    );

    let mut o2 = bravo(ContentProof::NameSize);
    o2.ignored
        .insert(SubjectKey("p:completed:Bravo.mkv".into()));
    let r2 = run(&o2);
    assert!(is_ignored(one(&r2, C::Duplicate, "d:1:20-1:21")));
    assert!(is_ignored(one(
        &r2,
        C::HardlinkMissing,
        "p:completed:Bravo.mkv"
    )));
}

#[test]
fn a_torrent_subject_can_be_ignored() {
    let i = Inv::new(incomplete()).file("Oscar.mkv", 1, 10).build();
    let t = Tor::new(
        'd',
        &incomplete(),
        "stalledDL",
        0.4,
        "Oscar.mkv",
        &[("Oscar.mkv", "Oscar.mkv")],
    )
    .added_days_ago(30);
    let mut o = obs(
        vec![i, Inv::new(completed()).build()],
        qb(snapshot(5, vec![t])),
    );
    o.ignored.insert(SubjectKey(tid('d')));
    let r = run(&o);
    assert!(is_ignored(one(&r, C::IncompleteStalled, &tid('d'))));
}

// ---------------------------------------------------------------- explanation

#[test]
fn the_explanation_separates_facts_candidates_proofs_and_recommendation() {
    let r = run(&bravo(ContentProof::NameSize));
    let i = one(&r, C::HardlinkMissing, "p:completed:Bravo.mkv");
    let e = explain(i, NOW);
    assert_eq!(e.evidence_level, EvidenceLevel::NameSize);
    assert!(
        e.observed
            .iter()
            .any(|f| matches!(f, Fact::Entry { size: 81_920, .. }))
    );
    assert!(
        e.observed
            .iter()
            .any(|f| matches!(f, Fact::TorrentHolds { state, .. } if state == "stalledUP"))
    );
    assert!(
        e.observed
            .iter()
            .any(|f| matches!(f, Fact::DistinctInodes { .. }))
    );
    assert!(e.candidates.iter().any(|c| matches!(
        c,
        CandidateFact::Content {
            level: EvidenceLevel::NameSize,
            ..
        }
    )));
    assert!(e.proven.is_empty(), "a name+size match proves nothing");
    assert_eq!(
        e.recommendation.kind,
        RecommendationKind::VerifyContentFirst
    );
    assert!(e.recommendation.actionable);
    assert_eq!(e.age_days, 3);

    let proven = run(&bravo(ContentProof::Identical { compared_at: NOW }));
    let ep = explain(
        one(&proven, C::HardlinkMissing, "p:completed:Bravo.mkv"),
        NOW,
    );
    assert!(
        ep.proven
            .iter()
            .any(|p| matches!(p, ProvenFact::ContentIdentical { .. }))
    );
    assert_eq!(
        ep.recommendation.kind,
        RecommendationKind::ConsolidateProven
    );
    assert_eq!(ep.evidence_level, EvidenceLevel::Full);
}

#[test]
fn a_blocked_recommendation_is_not_actionable_and_informational_ones_never_are() {
    let mut o = bravo(ContentProof::Identical { compared_at: NOW });
    o.qbit = qb(snapshot(
        5,
        vec![Tor::new(
            'b',
            &completed(),
            "checkingUP",
            1.0,
            "Bravo.mkv",
            &[("Bravo.mkv", "Bravo.mkv")],
        )],
    ));
    let r = run(&o);
    let e = explain(one(&r, C::HardlinkMissing, "p:completed:Bravo.mkv"), NOW);
    assert_eq!(e.recommendation.kind, RecommendationKind::ConsolidateProven);
    assert!(!e.recommendation.actionable, "the torrent is protected");
    assert!(
        e.blockers
            .iter()
            .any(|b| b.code == B::TorrentProtectedState)
    );

    let m = Inv::new(media()).file("Lib.mkv", 1, 4096).build();
    let r2 = run(&obs(
        vec![Inv::new(completed()).build(), m],
        qb(snapshot(5, vec![])),
    ));
    let w = one(
        &r2,
        C::MediaWithoutSource,
        &format!("p:{}:Lib.mkv", media()),
    );
    let ew = explain(w, NOW);
    assert_eq!(ew.recommendation.kind, RecommendationKind::InformationOnly);
    assert!(!ew.recommendation.actionable);
    assert_eq!(w.severity, Severity::Info);
}

#[test]
fn an_ignored_issue_explains_as_ignored() {
    let c = Inv::new(completed()).file("Echo.mkv", 40, 32_768).build();
    let mut o = obs(vec![c, Inv::new(media()).build()], qb(snapshot(5, vec![])));
    o.ignored.insert(SubjectKey("p:completed:Echo.mkv".into()));
    let r = run(&o);
    let e = explain(one(&r, C::OrphanCompleted, "p:completed:Echo.mkv"), NOW);
    assert_eq!(e.recommendation.kind, RecommendationKind::Ignored);
    assert!(!e.recommendation.actionable);
}

#[test]
fn filesystem_evidence_belongs_to_its_session_and_carries_provenance_and_age() {
    let r = run(&bravo(ContentProof::NameSize));
    let i = one(&r, C::HardlinkMissing, "p:completed:Bravo.mkv");
    assert_eq!(i.session, session());
    assert!(i.evidence.iter().all(|e| e.observed_at.0 > 0));
    assert!(i.evidence.iter().any(|e| e.is_from_session(session())));
    assert!(i.evidence.iter().any(|e| matches!(
        e.provenance,
        removarr_domain::evidence::Provenance::Qbittorrent { .. }
    )));
    assert!(
        i.evidence.iter().all(|e| e.completeness.is_complete()),
        "clean scan, complete evidence"
    );
}

#[test]
fn evidence_is_marked_incomplete_when_the_scan_was_not_clean() {
    let c = Inv::new(completed())
        .file("Echo.mkv", 40, 32_768)
        .error("Echo.mkv")
        .build();
    let r = run(&obs(
        vec![c, Inv::new(media()).build()],
        qb(snapshot(5, vec![])),
    ));
    let i = one(&r, C::OrphanCompleted, "p:completed:Echo.mkv");
    assert!(!i.completeness().is_complete());
}
