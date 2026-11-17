//! `incomplete_stalled` and `incomplete_orphan` (MASTER 5.1, OQ-06).
//!
//! Stagnation is read from qBittorrent (`added_on`, state, progress): no disk I/O. The
//! allowlist of INV-28 governs completed data only; stalled-incomplete cleanup is a separate
//! manual action (OQ-06), so a busy state blocks it but an ordinary `downloading` torrent that
//! is old enough is listed.

use std::collections::BTreeSet;

use removarr_domain::fs::PhysicalId;
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Severity, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::policy::is_sample;
use removarr_domain::torrent::{MappedPath, TorrentInfo, is_busy_for_incomplete_cleanup};

use super::ctx::{Ctx, Draft, mtime_ms, short};

fn under_incomplete(p: &MappedPath) -> Option<&RootedPath> {
    p.as_rooted()
        .filter(|r| r.root.kind() == RootKind::Incomplete)
}

pub(crate) fn stalled(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let (Some(snap), Some(index)) = (ctx.snapshot, &ctx.index) else {
        return;
    };
    for t in snap.torrents.iter().filter(|t| t.progress < 1.0) {
        let files = snap.files.get(&t.hash).map_or(&[][..], Vec::as_slice);
        let file_paths: Vec<Option<&RootedPath>> = files
            .iter()
            .map(|f| f.candidates.iter().find_map(under_incomplete))
            .collect();
        let in_incomplete = under_incomplete(&t.save_path).is_some()
            || t.download_path
                .as_ref()
                .and_then(under_incomplete)
                .is_some()
            || under_incomplete(&t.content_path).is_some()
            || file_paths.iter().any(Option::is_some);
        if !in_incomplete
            || t.added_on.age_days(ctx.obs.now) < u64::from(ctx.settings.incomplete_stalled_days)
        {
            continue;
        }
        out.push(stalled_issue(ctx, t, &file_paths, index));
    }
}

fn stalled_issue(
    ctx: &Ctx<'_>,
    t: &TorrentInfo,
    file_paths: &[Option<&RootedPath>],
    index: &super::index::TorrentIndex,
) -> Issue {
    let anchor = under_incomplete(&t.content_path)
        .or_else(|| file_paths.iter().flatten().next().copied())
        .cloned();
    let mut d = Draft::new(
        IssueCategory::IncompleteStalled,
        SubjectKey::torrent(&t.hash),
        Unit::Torrent(t.hash.clone()),
        Severity::Warning,
        t.added_on,
    );
    d.torrent_hashes.insert(t.hash.clone());
    if let Some(a) = &anchor {
        d.evidence.push(ctx.qbit_evidence(t, a.clone()));
    }
    // Present entries this torrent holds in the incomplete roots.
    let owned: Vec<_> = ctx
        .incomplete_files
        .iter()
        .copied()
        .filter(|e| index.hashes_for(&e.path).contains(&t.hash))
        .collect();
    d.logical_bytes = owned.iter().map(|e| e.sig.size).sum();
    for e in &owned {
        d.evidence.push(ctx.fs_evidence(e));
    }
    let held_elsewhere: BTreeSet<PhysicalId> = owned
        .iter()
        .map(|e| e.sig.phys())
        .filter(|p| index.hashes_for_inode(p).iter().any(|h| *h != t.hash))
        .chain(ctx.media_phys.iter().copied())
        .collect();
    let (bytes, phys) = ctx.potential(&owned, &held_elsewhere);
    d.potential_bytes = bytes;
    d.physical_ids = phys;

    if is_busy_for_incomplete_cleanup(&t.state) {
        d.block([Blocker::new(
            BlockerCode::TorrentProtectedState,
            format!(
                "torrent {}: state {} is busy",
                short(&t.hash),
                t.state.as_str()
            ),
        )]);
    }
    if ctx
        .faults
        .get(&t.hash)
        .is_some_and(|f| f.iter().any(|(_, k)| k.is_error()))
    {
        d.block([Blocker::new(
            BlockerCode::MappingError,
            "a torrent path has no usable mapping",
        )]);
    }
    let all_mapped = !file_paths.is_empty() && file_paths.iter().all(Option::is_some);
    if !all_mapped {
        d.block([Blocker::new(
            BlockerCode::MappingError,
            "not every file is mapped under the incomplete root",
        )]);
    }
    for p in anchor.iter().chain(file_paths.iter().flatten().copied()) {
        let mut seen = BTreeSet::new();
        for b in ctx.scope.path_blockers(p) {
            if seen.insert(b.code.clone()) {
                d.block([b]);
            }
        }
    }
    d.finish(ctx)
}

pub(crate) fn orphans(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let held: BTreeSet<PhysicalId> = ctx.held_inodes();
    let prov = match ctx.snapshot {
        Some(s) => removarr_domain::evidence::Provenance::Qbittorrent {
            taken_at: s.taken_at,
        },
        None => removarr_domain::evidence::Provenance::Computed,
    };
    for e in ctx.incomplete_files.iter().copied() {
        if !ctx.owners_of_entry(e).is_empty() {
            continue;
        }
        let mut d = Draft::new(
            IssueCategory::IncompleteOrphan,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            Severity::Warning,
            mtime_ms(&e.sig),
        );
        d.logical_bytes = e.sig.size;
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence.push(ctx.note(
            "no_torrent_holds_path",
            prov.clone(),
            ctx.ownership_completeness(),
        ));
        d.block(ctx.ownership_blockers());
        d.block(ctx.scope.path_blockers(&e.path));
        if is_sample(&e.path.rel) {
            d.block([Blocker::new(
                BlockerCode::Sample,
                "samples are protected by default",
            )]);
        }
        let (bytes, phys) = ctx.potential(&[e], &held);
        d.potential_bytes = bytes;
        d.physical_ids = phys;
        out.push(d.finish(ctx));
    }
}
