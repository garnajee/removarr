#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::path::RootedPath;
use removarr_domain::time::Timestamp;
use removarr_domain::torrent::{
    MappedPath, RecheckStatus, TorrentFile, TorrentHash, TorrentInfo, TorrentState,
};
use removarr_qbit::{
    Eligibility, KeepReason, LeftStoppedReason, Protection, RemovalDecision, classify,
    decide_removal, plan_restore,
};

const ALL_STATES: &[&str] = &[
    "error",
    "missingFiles",
    "uploading",
    "pausedUP",
    "queuedUP",
    "stalledUP",
    "checkingUP",
    "forcedUP",
    "allocating",
    "downloading",
    "metaDL",
    "pausedDL",
    "queuedDL",
    "stalledDL",
    "checkingDL",
    "forcedDL",
    "checkingResumeData",
    "moving",
    "stoppedUP",
    "stoppedDL",
    "forcedMetaDL",
    "bogus",
    "",
    "UPLOADING",
];

fn rooted(rel: &str) -> RootedPath {
    RootedPath::parse("completed", rel).unwrap()
}

fn mapped(rel: &str) -> MappedPath {
    MappedPath::Mapped {
        raw: format!("/data/completed/{rel}"),
        path: rooted(rel),
    }
}

fn info(state: &str, progress: f64) -> TorrentInfo {
    TorrentInfo {
        hash: TorrentHash::new("a".repeat(40)).unwrap(),
        name: "n".into(),
        state: TorrentState::parse(state),
        progress,
        added_on: Timestamp(0),
        completion_on: None,
        category: String::new(),
        size: 1,
        save_path: mapped(""),
        download_path: None,
        content_path: mapped("x.mkv"),
    }
}

#[test]
fn allowlist_is_exact_per_version() {
    let ok4 = ["uploading", "stalledUP", "queuedUP", "forcedUP", "pausedUP"];
    let ok5 = [
        "uploading",
        "stalledUP",
        "queuedUP",
        "forcedUP",
        "stoppedUP",
    ];
    for s in ALL_STATES {
        let e4 = classify(&info(s, 1.0), 4) == Eligibility::Eligible;
        let e5 = classify(&info(s, 1.0), 5) == Eligibility::Eligible;
        assert_eq!(e4, ok4.contains(s), "4.x {s:?}");
        assert_eq!(e5, ok5.contains(s), "5.x {s:?}");
        // Unknown major: fail closed.
        assert_ne!(classify(&info(s, 1.0), 6), Eligibility::Eligible, "{s:?}");
        assert_ne!(classify(&info(s, 1.0), 3), Eligibility::Eligible, "{s:?}");
    }
}

#[test]
fn incomplete_or_nan_progress_is_protected() {
    for p in [0.0, 0.999_999, f64::NAN, -1.0] {
        assert_eq!(
            classify(&info("uploading", p), 5),
            Eligibility::Protected(Protection::NotComplete),
            "{p}"
        );
    }
}

#[test]
fn stalled_stopped_and_missing_are_distinct_protections() {
    assert_eq!(classify(&info("stalledUP", 1.0), 5), Eligibility::Eligible);
    assert_eq!(classify(&info("stoppedUP", 1.0), 5), Eligibility::Eligible);
    assert_eq!(
        classify(&info("missingFiles", 1.0), 5),
        Eligibility::Protected(Protection::State(TorrentState::MissingFiles))
    );
    assert_eq!(
        classify(&info("stoppedUP", 1.0), 4),
        Eligibility::Protected(Protection::StateNotValidForVersion(TorrentState::StoppedUp))
    );
    assert_eq!(
        classify(&info("pausedUP", 1.0), 5),
        Eligibility::Protected(Protection::StateNotValidForVersion(TorrentState::PausedUp))
    );
}

#[test]
fn mapping_error_protects() {
    let mut t = info("uploading", 1.0);
    t.content_path = MappedPath::NoRule { raw: "/x".into() };
    assert_eq!(
        classify(&t, 5),
        Eligibility::Protected(Protection::MappingUncertain)
    );
    let mut t = info("uploading", 1.0);
    t.save_path = MappedPath::Invalid {
        raw: "/data/completed/..".into(),
        reason: "x".into(),
    };
    assert_eq!(
        classify(&t, 4),
        Eligibility::Protected(Protection::MappingUncertain)
    );
}

fn file(index: u32, priority: i32, cands: Vec<MappedPath>) -> TorrentFile {
    TorrentFile {
        index,
        name: "f".into(),
        size: 1,
        priority,
        progress: 1.0,
        candidates: cands,
    }
}

#[test]
fn removal_needs_authorization_and_every_wanted_file_absent() {
    let files = vec![
        file(0, 1, vec![mapped("a.mkv"), mapped("a.mkv.!qB")]),
        file(1, 0, vec![mapped("skipped.nfo")]),
    ];
    let absent = |_: &RootedPath| Some(false);
    assert_eq!(
        decide_removal(&files, true, absent),
        RemovalDecision::Remove
    );
    assert_eq!(
        decide_removal(&files, false, absent),
        RemovalDecision::Keep(KeepReason::NotAuthorized)
    );
    // Present at any candidate (even the working-file form).
    let present_qb = |p: &RootedPath| Some(p.rel.as_str().ends_with(".!qB"));
    assert_eq!(
        decide_removal(&files, true, present_qb),
        RemovalDecision::Keep(KeepReason::WantedFilePresent { index: 0 })
    );
    // Unwanted file present: irrelevant.
    let only_skipped = |p: &RootedPath| Some(p.rel.as_str() == "skipped.nfo");
    assert_eq!(
        decide_removal(&files, true, only_skipped),
        RemovalDecision::Remove
    );
    // Unknown presence keeps the task.
    assert_eq!(
        decide_removal(&files, true, |_| None),
        RemovalDecision::Keep(KeepReason::WantedFileUncertain { index: 0 })
    );
}

#[test]
fn removal_is_refused_for_unmapped_candidates_and_empty_lists() {
    let outside = MappedPath::OutsideRoots {
        raw: "/srv/x".into(),
    };
    let files = vec![file(0, 1, vec![mapped("a.mkv"), outside])];
    assert_eq!(
        decide_removal(&files, true, |_| Some(false)),
        RemovalDecision::Keep(KeepReason::WantedFileUncertain { index: 0 })
    );
    assert_eq!(
        decide_removal(&[], true, |_| Some(false)),
        RemovalDecision::Keep(KeepReason::NoFilesListed)
    );
    let invalid = MappedPath::Invalid {
        raw: "".into(),
        reason: "x".into(),
    };
    assert_eq!(
        decide_removal(&[file(3, 1, vec![invalid])], true, |_| Some(false)),
        RemovalDecision::Keep(KeepReason::WantedFileUncertain { index: 3 })
    );
}

fn hs(c: char) -> TorrentHash {
    TorrentHash::new(c.to_string().repeat(40)).unwrap()
}

#[test]
fn restore_preserves_initial_state_and_reports_left_stopped() {
    let initial: BTreeMap<_, _> = [
        (hs('1'), TorrentState::Uploading), // active, untouched
        (hs('2'), TorrentState::StalledUp), // active, modified, verified
        (hs('3'), TorrentState::Uploading), // active, modified, timed out
        (hs('4'), TorrentState::StoppedUp), // initially stopped
        (hs('5'), TorrentState::PausedUp),  // initially paused and modified
        (hs('6'), TorrentState::Uploading), // active, modified, failed
        (hs('7'), TorrentState::Uploading), // active, modified, never rechecked
    ]
    .into_iter()
    .collect();
    let modified: BTreeSet<_> = ['2', '3', '5', '6', '7', '8'].into_iter().map(hs).collect();
    let recheck: BTreeMap<_, _> = [
        (hs('2'), RecheckStatus::Verified),
        (hs('3'), RecheckStatus::TimedOut),
        (hs('5'), RecheckStatus::Verified),
        (hs('6'), RecheckStatus::Failed),
    ]
    .into_iter()
    .collect();
    let plan = plan_restore(&initial, &modified, &recheck);
    assert_eq!(plan.start, [hs('1'), hs('2')].into_iter().collect());
    assert_eq!(
        plan.left_stopped[&hs('3')],
        LeftStoppedReason::RecheckTimedOut
    );
    assert_eq!(
        plan.left_stopped[&hs('4')],
        LeftStoppedReason::InitiallyStopped
    );
    assert_eq!(
        plan.left_stopped[&hs('5')],
        LeftStoppedReason::InitiallyStopped
    );
    assert_eq!(
        plan.left_stopped[&hs('6')],
        LeftStoppedReason::RecheckFailed
    );
    assert_eq!(plan.left_stopped[&hs('7')], LeftStoppedReason::NotRechecked);
    assert_eq!(
        plan.left_stopped[&hs('8')],
        LeftStoppedReason::UnknownInitialState
    );
    assert_eq!(plan.start.len() + plan.left_stopped.len(), 8);
}
