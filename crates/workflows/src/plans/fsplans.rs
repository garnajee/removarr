//! Filesystem actions: consolidation (tier 2), deletions under completed and incomplete, stalled
//! incomplete cleanup and targeted mode fixes (tier 3), and their companions.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::evidence::{Completeness, ContentProof, EvidenceLevel, MatchCandidate};
use removarr_domain::fs::{EntryKind, FsEntry};
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::plan::{ActionKind, ActionTarget, PlanOrigin, Precondition, VerificationStep};
use removarr_domain::policy::is_sample;
use removarr_domain::session::ServiceKind;
use removarr_domain::torrent::{TorrentHash, is_busy_for_incomplete_cleanup};

use super::build::{Draft, UnitPlan, UnitResult, is_kind, issue_completeness, issue_level};
use super::facts::{Facts, arr_kind, short};

fn missing(path: &RootedPath) -> Blocker {
    Blocker::new(
        BlockerCode::ScanIncomplete,
        format!("{path} is not in the current inventory"),
    )
}

fn sample_blocker(path: &RootedPath) -> Blocker {
    Blocker::new(
        BlockerCode::Sample,
        format!("{path} is a sample: samples are protected"),
    )
}

/// Rank of a stored content proof (strongest first), as the classifier ranks it.
fn rank(c: &ContentProof) -> u8 {
    match c {
        ContentProof::Identical { .. } => 5,
        ContentProof::Sampled => 4,
        ContentProof::NameSize => 3,
        ContentProof::Semantic { .. } => 2,
        ContentProof::None | ContentProof::Differs { .. } => 0,
    }
}

/// Best still-valid same-bytes candidate for `src` (optionally towards `tgt`).
fn best_pair<'a>(
    f: &Facts<'a>,
    src: &RootedPath,
    tgt: Option<&RootedPath>,
) -> Option<(&'a FsEntry, &'a FsEntry, &'a MatchCandidate)> {
    let mut best: Option<(&FsEntry, &FsEntry, &MatchCandidate)> = None;
    for m in &f.input.observation.matches {
        if &m.a != src || tgt.is_some_and(|t| &m.b != t) {
            continue;
        }
        let (Some(a), Some(b)) = (f.entry(&m.a), f.entry(&m.b)) else {
            continue;
        };
        let ok = a.sig.kind == EntryKind::File
            && b.sig.kind == EntryKind::File
            && a.sig.phys() != b.sig.phys()
            && a.sig.size == b.sig.size
            && m.proof_still_valid(&a.sig, &b.sig)
            && rank(&m.content) > 0;
        if !ok {
            continue;
        }
        let better = best.is_none_or(|(_, bb, bm)| {
            (rank(&m.content), std::cmp::Reverse(&b.path))
                > (rank(&bm.content), std::cmp::Reverse(&bb.path))
        });
        if better {
            best = Some((a, b, m));
        }
    }
    best
}

/// `ConsolidateHardlink`: the media copy is replaced by a hardlink to the completed source
/// (direction completed -> media only, INV-04).
pub(crate) fn consolidate<'a>(f: &Facts<'a>, issue: &Issue) -> UnitResult<'a> {
    let (src, tgt) = match &issue.unit {
        Unit::Pair { a, b } => (a, Some(b)),
        Unit::File(p) => (p, None),
        _ => {
            return Err(vec![Blocker::new(
                BlockerCode::DuplicateTarget,
                "this unit is not a consolidation pair",
            )]);
        }
    };
    let Some((s, t, m)) = best_pair(f, src, tgt) else {
        return Err(vec![Blocker::new(
            BlockerCode::ScanIncomplete,
            format!("no current same-content candidate for {src}: verify or rescan"),
        )]);
    };
    let mut b = Vec::new();
    if !is_kind(&s.path, RootKind::Completed) || !is_kind(&t.path, RootKind::Media) {
        b.push(Blocker::new(
            BlockerCode::ProtectedZone,
            "consolidation goes from a completed source to a media target only",
        ));
    }
    if s.sig.dev != t.sig.dev {
        b.push(Blocker::new(
            BlockerCode::CrossDevice,
            "source and target are on different devices",
        ));
    }
    b.extend(f.scope_blockers(&s.path));
    b.extend(f.scope_blockers(&t.path));
    b.extend(f.parent_unwritable(&t.path));
    b.extend(f.qbit_blockers());
    for p in [&s.path, &t.path] {
        if is_sample(&p.rel) {
            b.push(sample_blocker(p));
        }
    }
    let mut hashes = f.holders(&s.path);
    hashes.extend(f.holders(&t.path));
    let (states, tb) = f.torrent_states(&hashes);
    b.extend(tb);
    if !b.is_empty() {
        return Err(b);
    }

    let mut d = Draft::new(
        ActionKind::ConsolidateHardlink,
        ActionTarget::Consolidate {
            source: s.path.clone(),
            target: t.path.clone(),
        },
    );
    d.pre = vec![
        Precondition::EntryUnchanged {
            path: s.path.clone(),
            sig: s.sig,
            compare_nlink: false,
        },
        Precondition::EntryUnchanged {
            path: t.path.clone(),
            sig: t.sig,
            compare_nlink: false,
        },
        Precondition::SameDevice {
            a: s.path.clone(),
            b: t.path.clone(),
        },
        Precondition::NoScanErrorUnder {
            path: s.path.clone(),
        },
        Precondition::NoScanErrorUnder {
            path: t.path.clone(),
        },
        Precondition::ServiceAvailable(ServiceKind::Qbittorrent),
    ];
    if !states.is_empty() {
        d.pre.push(Precondition::TorrentStates { states });
    }
    arr_import_guard(f, &hashes, &[&s.path, &t.path], &mut d.pre);
    d.verify = vec![
        VerificationStep::InodeEquals {
            path: t.path.clone(),
            phys: s.sig.phys(),
        },
        VerificationStep::InodeEquals {
            path: s.path.clone(),
            phys: s.sig.phys(),
        },
    ];
    if !hashes.is_empty() {
        d.verify.push(VerificationStep::RecheckVerified {
            hashes: hashes.clone(),
        });
    }
    d.hashes = hashes;
    d.level = m.content.level();
    d.completeness = issue_completeness(issue);
    d.freed = vec![t];
    d.logical = t.sig.size;
    Ok(UnitPlan {
        drafts: vec![d],
        aged: vec![s, t],
        claims: vec![s.path.clone(), t.path.clone()],
        ..UnitPlan::default()
    })
}

/// One configured ARR service per kind must be reachable when the action runs, and a known
/// media gets a `NoArrImport` guard (single importer, INV-16).
pub(crate) fn arr_import_guard(
    f: &Facts<'_>,
    hashes: &BTreeSet<TorrentHash>,
    paths: &[&RootedPath],
    pre: &mut Vec<Precondition>,
) {
    for k in [ServiceKind::Sonarr, ServiceKind::Radarr] {
        if f.service_configured(k) {
            pre.push(Precondition::ServiceAvailable(k));
        }
    }
    if let Ok(media) = f.identity(hashes, paths) {
        pre.push(Precondition::NoArrImport {
            media,
            hashes: hashes.clone(),
        });
    }
}

/// Manual deletion (`Delete`): a file, a whole release folder, junk or an incomplete orphan.
pub(crate) fn delete<'a>(
    f: &Facts<'a>,
    issue: &Issue,
    kind: ActionKind,
    origin: PlanOrigin,
) -> UnitResult<'a> {
    match &issue.unit {
        Unit::File(p) => {
            let torrent_less = issue.category != IssueCategory::HardlinkMissing;
            let d = delete_file(f, p, kind, torrent_less, issue)?;
            let e = f.entry(p).ok_or_else(|| vec![missing(p)])?;
            Ok(UnitPlan {
                drafts: vec![d],
                aged: vec![e],
                claims: vec![p.clone()],
                ..UnitPlan::default()
            })
        }
        Unit::Folder {
            dir,
            members,
            annexes,
        } => delete_folder(f, dir, members, annexes, origin, issue),
        _ => Err(vec![Blocker::new(
            BlockerCode::DuplicateTarget,
            "this unit is not a path",
        )]),
    }
}

/// The root kind a deletion kind may target.
fn deletion_root(kind: ActionKind) -> RootKind {
    match kind {
        ActionKind::DeleteIncompleteOrphan | ActionKind::CleanupStalledIncomplete => {
            RootKind::Incomplete
        }
        ActionKind::DeleteMediaFile => RootKind::Media,
        _ => RootKind::Completed,
    }
}

/// Deletion of one regular file.
pub(crate) fn delete_file<'a>(
    f: &Facts<'a>,
    path: &RootedPath,
    kind: ActionKind,
    torrent_less: bool,
    issue: &Issue,
) -> Result<Draft<'a>, Vec<Blocker>> {
    let e = f.entry(path).ok_or_else(|| vec![missing(path)])?;
    let mut b = Vec::new();
    if e.sig.kind != EntryKind::File {
        b.push(Blocker::new(
            BlockerCode::Symlink,
            format!("{path} is not a regular file"),
        ));
    }
    if path.root.kind() == RootKind::Media && kind != ActionKind::DeleteMediaFile {
        b.push(Blocker::new(
            BlockerCode::MediaRootDeletion,
            "files under media roots are never deleted here",
        ));
    } else if path.root.kind() != deletion_root(kind) || path.rel.is_root() {
        b.push(Blocker::new(
            BlockerCode::ProtectedZone,
            format!("{kind:?} cannot target {path}"),
        ));
    }
    if is_sample(&path.rel) {
        b.push(sample_blocker(path));
    }
    b.extend(f.scope_blockers(path));
    b.extend(f.parent_unwritable(path));
    let mut d = Draft::new(kind, ActionTarget::Path { path: path.clone() });
    d.pre = vec![
        Precondition::EntryUnchanged {
            path: path.clone(),
            sig: e.sig,
            compare_nlink: true,
        },
        Precondition::NoScanErrorUnder { path: path.clone() },
    ];
    if kind != ActionKind::DeleteMediaFile {
        torrent_guard(f, path, torrent_less, &mut b, &mut d);
    }
    if !b.is_empty() {
        return Err(b);
    }
    d.verify = vec![VerificationStep::Absent { path: path.clone() }];
    d.level = issue_level(issue);
    d.completeness = issue_completeness(issue);
    d.freed = vec![e];
    d.logical = e.sig.size;
    Ok(d)
}

/// Torrent side of a deletion: a torrent-less target must stay torrent-less (path ownership,
/// OQ-05), a torrent-held target needs every holder allowlisted (INV-28).
fn torrent_guard(
    f: &Facts<'_>,
    path: &RootedPath,
    torrent_less: bool,
    b: &mut Vec<Blocker>,
    d: &mut Draft<'_>,
) {
    let holders = f.holders_under(path);
    if torrent_less {
        b.extend(f.ownership_blockers());
        if !holders.is_empty() {
            b.push(Blocker::new(
                BlockerCode::TorrentReferencesTarget,
                format!("{} torrent(s) reference {path}", holders.len()),
            ));
        }
        d.pre
            .push(Precondition::NoTorrentReferences { path: path.clone() });
        d.pre
            .push(Precondition::ServiceAvailable(ServiceKind::Qbittorrent));
    } else {
        let (states, tb) = f.torrent_states(&holders);
        b.extend(f.qbit_blockers());
        b.extend(tb);
        d.pre
            .push(Precondition::ServiceAvailable(ServiceKind::Qbittorrent));
        if holders.is_empty() {
            d.pre
                .push(Precondition::NoTorrentReferences { path: path.clone() });
        } else {
            d.pre.push(Precondition::TorrentStates { states });
        }
        arr_import_guard(f, &holders, &[path], &mut d.pre);
        d.hashes = holders;
    }
}

/// `DeleteCompletedFolder`: the whole release folder or nothing, with every annex in the
/// folder snapshot (INV-25). Samples inside need an acknowledgement (manual) or block (auto).
pub(crate) fn delete_folder<'a>(
    f: &Facts<'a>,
    dir: &RootedPath,
    members: &[RootedPath],
    annexes: &[RootedPath],
    origin: PlanOrigin,
    issue: &Issue,
) -> UnitResult<'a> {
    let Some(dir_sig) = f.sig(dir) else {
        return Err(vec![missing(dir)]);
    };
    let mut b = Vec::new();
    if dir_sig.kind != EntryKind::Dir {
        b.push(Blocker::new(
            BlockerCode::Symlink,
            format!("{dir} is not a directory"),
        ));
    }
    if !is_kind(dir, RootKind::Completed) || dir.rel.is_root() {
        b.push(Blocker::new(
            BlockerCode::ProtectedZone,
            format!("folder deletion cannot target {dir}"),
        ));
    }
    b.extend(f.scope_blockers(dir));
    b.extend(f.parent_unwritable(dir));
    let below = f.descendants(dir);
    let reviewed: BTreeSet<&RootedPath> = members.iter().chain(annexes).collect();
    let present: BTreeSet<&RootedPath> = below.iter().map(|e| &e.path).collect();
    for e in &below {
        if !reviewed.contains(&e.path) {
            b.push(Blocker::new(
                BlockerCode::ScanIncomplete,
                format!("{} is not part of the reviewed unit", e.path),
            ));
        }
        b.extend(f.parent_unwritable(&e.path));
    }
    for p in reviewed.iter().filter(|p| !present.contains(*p)) {
        b.push(missing(p));
    }
    let samples: Vec<RootedPath> = below
        .iter()
        .filter(|e| is_sample(&e.path.rel))
        .map(|e| e.path.clone())
        .collect();
    if origin == PlanOrigin::AutoPolicy {
        b.extend(samples.iter().map(sample_blocker));
    }
    let snapshot = f.dir_snapshot(dir).ok_or_else(|| vec![missing(dir)])?;
    let mut d = Draft::new(
        ActionKind::DeleteCompletedFolder,
        ActionTarget::Path { path: dir.clone() },
    );
    d.pre = vec![
        Precondition::DirUnchanged { snapshot },
        Precondition::NoScanErrorUnder { path: dir.clone() },
    ];
    torrent_guard(f, dir, true, &mut b, &mut d);
    if !b.is_empty() {
        return Err(b);
    }
    let files: Vec<&FsEntry> = below
        .iter()
        .copied()
        .filter(|e| e.sig.kind == EntryKind::File)
        .collect();
    d.verify = vec![VerificationStep::Absent { path: dir.clone() }];
    d.level = issue_level(issue);
    d.completeness = issue_completeness(issue);
    d.logical = files.iter().map(|e| e.sig.size).sum();
    d.freed.clone_from(&files);
    Ok(UnitPlan {
        drafts: vec![d],
        samples,
        aged: files,
        claims: vec![dir.clone()],
        ..UnitPlan::default()
    })
}

/// `CleanupStalledIncomplete` (manual only, OQ-06, ADR-0008): stop, remove the task with
/// `deleteFiles=false`, then delete the torrent's data under the incomplete root.
pub(crate) fn cleanup_stalled<'a>(f: &Facts<'a>, issue: &Issue) -> UnitResult<'a> {
    let Unit::Torrent(hash) = &issue.unit else {
        return Err(vec![Blocker::new(
            BlockerCode::DuplicateTarget,
            "this unit is not a torrent",
        )]);
    };
    let mut b = f.qbit_blockers();
    let Some(t) = f
        .snapshot
        .and_then(|s| s.torrents.iter().find(|t| &t.hash == hash))
    else {
        b.push(Blocker::new(
            BlockerCode::TorrentProtectedState,
            format!("torrent {}: not in the observation", short(hash)),
        ));
        return Err(b);
    };
    if t.progress >= 1.0 || is_busy_for_incomplete_cleanup(&t.state) {
        b.push(Blocker::new(
            BlockerCode::TorrentProtectedState,
            format!(
                "torrent {}: state {} progress {} is not a stalled download",
                short(hash),
                t.state.as_str(),
                t.progress
            ),
        ));
    }
    // Data: the torrent's top-most present entries under incomplete roots.
    let held: Vec<&FsEntry> = f
        .input
        .observation
        .inventories
        .iter()
        .filter(|i| i.root.kind() == RootKind::Incomplete)
        .flat_map(|i| i.entries.iter())
        .filter(|e| f.holders(&e.path).contains(hash))
        .collect();
    let held_paths: BTreeSet<&RootedPath> = held.iter().map(|e| &e.path).collect();
    let top: Vec<&FsEntry> = held
        .iter()
        .copied()
        .filter(|e| {
            let mut p = e.path.rel.parent();
            while let Some(rel) = p {
                if held_paths.contains(&RootedPath::new(e.path.root.clone(), rel.clone())) {
                    return false;
                }
                p = rel.parent();
            }
            true
        })
        .collect();

    let mut d = Draft::new(
        ActionKind::CleanupStalledIncomplete,
        ActionTarget::Torrent {
            hash: hash.clone(),
            data_paths: top.iter().map(|e| e.path.clone()).collect(),
        },
    );
    d.pre = vec![
        Precondition::TorrentStates {
            states: [(hash.clone(), (t.state.clone(), t.progress))].into(),
        },
        Precondition::ServiceAvailable(ServiceKind::Qbittorrent),
    ];
    d.verify = vec![VerificationStep::TorrentAbsent {
        hashes: [hash.clone()].into(),
    }];
    let mut aged = Vec::new();
    for e in &top {
        let p = &e.path;
        if is_sample(&p.rel) {
            b.push(sample_blocker(p));
        }
        b.extend(f.scope_blockers(p));
        b.extend(f.parent_unwritable(p));
        let others: BTreeSet<TorrentHash> = f
            .holders_under(p)
            .into_iter()
            .filter(|h| h != hash)
            .collect();
        if !others.is_empty() {
            b.push(Blocker::new(
                BlockerCode::TorrentReferencesTarget,
                format!("{} other torrent(s) reference {p}", others.len()),
            ));
        }
        let files: Vec<&FsEntry> = if e.sig.kind == EntryKind::Dir {
            d.pre.push(Precondition::DirUnchanged {
                snapshot: f.dir_snapshot(p).ok_or_else(|| vec![missing(p)])?,
            });
            f.descendants(p)
                .into_iter()
                .filter(|x| x.sig.kind == EntryKind::File)
                .collect()
        } else {
            d.pre.push(Precondition::EntryUnchanged {
                path: p.clone(),
                sig: e.sig,
                compare_nlink: true,
            });
            vec![*e]
        };
        for x in &files {
            if f.inode_holders(&x.sig.phys()).iter().any(|h| h != hash) {
                b.push(Blocker::new(
                    BlockerCode::TorrentReferencesTarget,
                    format!("another torrent shares the inode of {}", x.path),
                ));
            }
        }
        d.pre
            .push(Precondition::NoScanErrorUnder { path: p.clone() });
        d.pre
            .push(Precondition::NoTorrentReferences { path: p.clone() });
        d.verify.push(VerificationStep::Absent { path: p.clone() });
        d.logical += files.iter().map(|x| x.sig.size).sum::<u64>();
        d.freed.extend(files.iter().copied());
        aged.extend(files);
    }
    if !b.is_empty() {
        return Err(b);
    }
    d.hashes = [hash.clone()].into();
    d.level = issue_level(issue);
    d.completeness = issue_completeness(issue);
    Ok(UnitPlan {
        claims: top.iter().map(|e| e.path.clone()).collect(),
        drafts: vec![d],
        aged,
        ..UnitPlan::default()
    })
}

/// `FixMode`: add the owner's write (and, for directories, search) bit to one entry owned by
/// the process. Never recursive, never an ownership change (INV-32).
pub(crate) fn fix_mode<'a>(f: &Facts<'a>, issue: &Issue) -> UnitResult<'a> {
    let Unit::File(path) = &issue.unit else {
        return Err(vec![Blocker::new(
            BlockerCode::DuplicateTarget,
            "this unit is not a path",
        )]);
    };
    let sig = f.sig(path).ok_or_else(|| vec![missing(path)])?;
    let mut b = f.scope_blockers(path);
    let mode = match sig.kind {
        EntryKind::File => sig.mode | 0o200,
        EntryKind::Dir => sig.mode | 0o300,
        EntryKind::Symlink | EntryKind::Other => {
            b.push(Blocker::new(
                BlockerCode::Symlink,
                format!("{path} is not a file or directory"),
            ));
            sig.mode
        }
    };
    if sig.uid != f.settings.process_uid {
        b.push(Blocker::new(
            BlockerCode::PermissionDenied,
            "only the owner of an entry can change its mode",
        ));
    } else if mode == sig.mode {
        b.push(Blocker::new(
            BlockerCode::PermissionDenied,
            "the owner bits already allow writing; fix the finding outside the application",
        ));
    }
    if !b.is_empty() {
        return Err(b);
    }
    let mut d = Draft::new(
        ActionKind::FixMode,
        ActionTarget::FixMode {
            path: path.clone(),
            mode,
        },
    );
    d.pre = vec![
        Precondition::EntryUnchanged {
            path: path.clone(),
            sig,
            compare_nlink: false,
        },
        Precondition::NoScanErrorUnder { path: path.clone() },
    ];
    d.verify = vec![VerificationStep::ModeEquals {
        path: path.clone(),
        mode,
    }];
    d.level = EvidenceLevel::None;
    d.completeness = issue_completeness(issue);
    Ok(UnitPlan {
        drafts: vec![d],
        claims: vec![path.clone()],
        ..UnitPlan::default()
    })
}

/// Evidence level of a companion: the weakest content level of the actions it follows. A
/// companion of an ARR-validated deletion has no content proof of its own (`None`).
fn companion_level(levels: impl Iterator<Item = EvidenceLevel>) -> EvidenceLevel {
    let mut out: Option<EvidenceLevel> = None;
    for l in levels {
        if matches!(
            l,
            EvidenceLevel::None | EvidenceLevel::ArrValidatedReplacement
        ) {
            return EvidenceLevel::None;
        }
        out = Some(out.map_or(l, |o| o.min(l)));
    }
    out.unwrap_or(EvidenceLevel::None)
}

/// Paths removed by deletion drafts: `(path, is a whole subtree)`.
fn removed(drafts: &[Draft<'_>]) -> Vec<(RootedPath, usize)> {
    let mut out = Vec::new();
    for (i, d) in drafts.iter().enumerate() {
        if let (
            ActionKind::DeleteCompletedFile
            | ActionKind::DeleteCompletedFolder
            | ActionKind::DeleteJunk,
            ActionTarget::Path { path },
        ) = (&d.kind, &d.target)
        {
            out.push((path.clone(), i));
        }
    }
    out
}

fn is_removed(p: &RootedPath, removed: &[(RootedPath, usize)]) -> bool {
    removed
        .iter()
        .any(|(r, _)| r.root == p.root && p.rel.starts_with(&r.rel))
}

/// `RemoveEmptyParent` companions (CP1-F12, INV-49): every directory under a completed root
/// (never the root itself) that the plan leaves empty, deepest first.
pub(crate) fn empty_parents<'a>(f: &Facts<'a>, drafts: &[Draft<'a>], auto: bool) -> Vec<Draft<'a>> {
    let rem = removed(drafts);
    let mut candidates: BTreeSet<RootedPath> = BTreeSet::new();
    for (p, _) in &rem {
        let mut rel = p.rel.parent();
        while let Some(r) = rel {
            if r.is_root() {
                break;
            }
            candidates.insert(RootedPath::new(p.root.clone(), r.clone()));
            rel = r.parent();
        }
    }
    // A directory is emptied when it holds at least one removed entry and everything else
    // below it is a directory that is emptied too (pre-existing empty directories block).
    let mut emptied: BTreeSet<RootedPath> = BTreeSet::new();
    let mut ordered: Vec<RootedPath> = candidates.into_iter().collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.rel.components().count()));
    for dir in &ordered {
        if !is_kind(dir, RootKind::Completed) || f.sig(dir).is_none_or(|s| s.kind != EntryKind::Dir)
        {
            continue;
        }
        let below = f.descendants(dir);
        let any_removed = below.iter().any(|e| is_removed(&e.path, &rem));
        let rest_ok = below.iter().all(|e| {
            is_removed(&e.path, &rem) || (e.sig.kind == EntryKind::Dir && emptied.contains(&e.path))
        });
        if any_removed
            && rest_ok
            && f.scope_blockers(dir).is_empty()
            && f.parent_unwritable(dir).is_none()
        {
            emptied.insert(dir.clone());
        }
    }
    let base = drafts.len();
    let mut out: Vec<Draft<'a>> = Vec::new();
    let mut index_of: BTreeMap<RootedPath, usize> = BTreeMap::new();
    for dir in ordered.iter().filter(|d| emptied.contains(*d)) {
        let Some(pre) = f.unchanged(dir, false) else {
            continue;
        };
        let mut d = Draft::new(
            ActionKind::RemoveEmptyParent,
            ActionTarget::Path { path: dir.clone() },
        );
        d.pre = vec![pre, Precondition::NoScanErrorUnder { path: dir.clone() }];
        d.verify = vec![VerificationStep::Absent { path: dir.clone() }];
        let inside = |p: &RootedPath| p.root == dir.root && p.rel.starts_with(&dir.rel);
        d.deps = rem
            .iter()
            .filter(|(p, _)| inside(p))
            .map(|(_, i)| *i)
            .chain(index_of.iter().filter(|(p, _)| inside(p)).map(|(_, i)| *i))
            .collect();
        d.level = companion_level(
            d.deps
                .iter()
                .filter_map(|i| drafts.get(*i).map(|x| x.level)),
        );
        if auto && d.level == EvidenceLevel::None {
            // No evidence level the automatic approval accepts (handoff gap): left for later.
            continue;
        }
        d.completeness = d
            .deps
            .iter()
            .filter_map(|i| drafts.get(*i).map(|x| x.completeness.clone()))
            .fold(Completeness::Complete, Completeness::and);
        index_of.insert(dir.clone(), base + out.len());
        out.push(d);
    }
    out
}

/// Opt-in `RemoveTorrentTask` companions (CP1-F12, INV-19): only for a hash whose every wanted
/// file is planned for deletion. `deleteFiles=false` is the adapter's literal.
pub(crate) fn torrent_removals<'a>(
    f: &Facts<'a>,
    drafts: &[Draft<'a>],
    auto: bool,
) -> Vec<Draft<'a>> {
    let rem = removed(drafts);
    let Some(snap) = f.snapshot else {
        return Vec::new();
    };
    let hashes: BTreeSet<TorrentHash> = rem
        .iter()
        .filter_map(|(_, i)| drafts.get(*i))
        .flat_map(|d| d.hashes.iter().cloned())
        .collect();
    let mut out = Vec::new();
    for h in hashes {
        let Some(files) = snap.files.get(&h).filter(|fs| !fs.is_empty()) else {
            continue;
        };
        let mut deps = BTreeSet::new();
        let all_gone = files.iter().filter(|x| x.is_wanted()).all(|x| {
            x.candidates.iter().filter_map(|c| c.as_rooted()).any(|p| {
                rem.iter()
                    .filter(|(r, _)| r.root == p.root && p.rel.starts_with(&r.rel))
                    .map(|(_, i)| deps.insert(*i))
                    .count()
                    > 0
            })
        });
        if !all_gone || deps.is_empty() {
            continue;
        }
        let (states, blockers) = f.torrent_states(&[h.clone()].into());
        if !blockers.is_empty() {
            continue;
        }
        let mut d = Draft::new(
            ActionKind::RemoveTorrentTask,
            ActionTarget::Torrent {
                hash: h.clone(),
                data_paths: vec![],
            },
        );
        d.pre = vec![
            Precondition::TorrentStates { states },
            Precondition::ServiceAvailable(ServiceKind::Qbittorrent),
        ];
        d.verify = vec![VerificationStep::TorrentAbsent {
            hashes: [h.clone()].into(),
        }];
        d.level = companion_level(deps.iter().filter_map(|i| drafts.get(*i).map(|x| x.level)));
        if auto && d.level == EvidenceLevel::None {
            continue;
        }
        d.completeness = deps
            .iter()
            .filter_map(|i| drafts.get(*i).map(|x| x.completeness.clone()))
            .fold(Completeness::Complete, Completeness::and);
        d.deps = deps.into_iter().collect();
        d.hashes = [h].into();
        out.push(d);
    }
    out
}

/// ARR service kind of a media's service (re-export for the ARR builders).
pub(crate) fn kind_of(m: &removarr_domain::arr::ArrMediaRef) -> ServiceKind {
    arr_kind(m.service)
}
