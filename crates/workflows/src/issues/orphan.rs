//! `orphan_completed` (MASTER 5.4) and `junk` (MASTER 5.6, OQ-02, OQ-03).
//!
//! Both are defined only under the completed roots and only by torrent ownership: a path held by
//! a torrent is never an orphan, and a path whose path *or inode* is held is never junk
//! (deleting it would break seeding). Media roots never produce either (INV-26).

use std::collections::BTreeSet;

use removarr_domain::evidence::Completeness;
use removarr_domain::fs::{EntryKind, FsEntry};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Severity, Unit};
use removarr_domain::path::RootedPath;
use removarr_domain::policy::is_sample;
use removarr_domain::time::Timestamp;

use super::ctx::{Ctx, Draft, mtime_ms};
use super::files::{is_orphan_subtitle, release_dir};

fn sample_blocker() -> Blocker {
    Blocker::new(BlockerCode::Sample, "samples are protected by default")
}

fn no_torrent_note(ctx: &Ctx<'_>) -> removarr_domain::evidence::Evidence {
    let prov = match ctx.snapshot {
        Some(s) => removarr_domain::evidence::Provenance::Qbittorrent {
            taken_at: s.taken_at,
        },
        None => removarr_domain::evidence::Provenance::Computed,
    };
    ctx.note("no_torrent_holds_path", prov, ctx.ownership_completeness())
}

/// Evidence about inodes of `entries`: shared with a torrent-held path (OQ-08) or with media.
fn inode_notes(ctx: &Ctx<'_>, entries: &[&FsEntry]) -> Vec<removarr_domain::evidence::Evidence> {
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| e.sig.kind == EntryKind::File) {
        let phys = e.sig.phys();
        if let Some(i) = &ctx.index
            && let Some(other) = i.inode_example(&phys)
            && *other != e.path
        {
            out.push(ctx.note(
                format!("inode_shared_with_torrent_file:{other}"),
                ctx.fs_provenance(),
                Completeness::Complete,
            ));
        }
        if ctx.media_phys.contains(&phys) {
            out.push(ctx.note(
                format!("inode_also_in_media:{}", e.path),
                ctx.fs_provenance(),
                Completeness::Complete,
            ));
        }
    }
    out
}

pub(crate) fn orphan_completed(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let held = {
        let mut h = ctx.held_inodes();
        h.extend(ctx.media_phys.iter().copied());
        h
    };
    let unowned = |e: &FsEntry| ctx.owners_of_path(&e.path).is_empty();
    let mut covered: BTreeSet<RootedPath> = BTreeSet::new();

    // Release-folder units first (CON-23, OQ-11): one unit per fully torrent-less release folder.
    for e in ctx.completed_files.iter().copied().filter(|e| unowned(e)) {
        let Some(name) = e.path.rel.file_name() else {
            continue;
        };
        if !ctx.settings.is_video(name) || covered.contains(&e.path) {
            continue;
        }
        let Some(dir_rel) = release_dir(&e.path.rel) else {
            continue;
        };
        let dir = RootedPath::new(e.path.root.clone(), dir_rel);
        let below = ctx.descendants(&dir);
        // Fully admissible: no regular file or symlink below is held by a torrent.
        if below
            .iter()
            .any(|b| b.sig.kind != EntryKind::Dir && !ctx.owners_of_path(&b.path).is_empty())
        {
            continue;
        }
        let files: Vec<&FsEntry> = below
            .iter()
            .copied()
            .filter(|b| b.sig.kind == EntryKind::File)
            .collect();
        covered.extend(files.iter().map(|f| f.path.clone()));
        let members: Vec<RootedPath> = files
            .iter()
            .filter(|f| {
                f.path
                    .rel
                    .file_name()
                    .is_some_and(|n| ctx.settings.is_video(n))
            })
            .map(|f| f.path.clone())
            .collect();
        let annexes: Vec<RootedPath> = below
            .iter()
            .filter(|b| !members.contains(&b.path))
            .map(|b| b.path.clone())
            .collect();
        let newest = files
            .iter()
            .map(|f| mtime_ms(&f.sig))
            .max()
            .unwrap_or(Timestamp(0));
        let mut d = Draft::new(
            IssueCategory::OrphanCompleted,
            SubjectKey::rooted(&dir),
            Unit::Folder {
                dir: dir.clone(),
                members,
                annexes,
            },
            Severity::Warning,
            newest,
        );
        d.logical_bytes = files.iter().map(|f| f.sig.size).sum();
        d.evidence.push(no_torrent_note(ctx));
        for b in below.iter().filter(|b| b.sig.kind != EntryKind::Dir) {
            d.evidence.push(ctx.fs_evidence(b));
        }
        d.evidence.extend(inode_notes(ctx, &files));
        d.block(ctx.ownership_blockers());
        d.block(ctx.scope.subtree_blockers(&dir.root, &dir.rel));
        if below.iter().any(|b| is_sample(&b.path.rel)) {
            d.block([sample_blocker()]);
        }
        let (bytes, phys) = ctx.potential(&files, &held);
        d.potential_bytes = bytes;
        d.physical_ids = phys;
        out.push(d.finish(ctx));
    }

    // Everything else without a torrent, file by file.
    for e in ctx.completed_files.iter().copied() {
        if covered.contains(&e.path) || !unowned(e) {
            continue;
        }
        let mut d = Draft::new(
            IssueCategory::OrphanCompleted,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            Severity::Warning,
            mtime_ms(&e.sig),
        );
        d.logical_bytes = e.sig.size;
        d.evidence.push(no_torrent_note(ctx));
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence.extend(inode_notes(ctx, &[e]));
        d.block(ctx.ownership_blockers());
        d.block(ctx.scope.path_blockers(&e.path));
        if is_sample(&e.path.rel) {
            d.block([sample_blocker()]);
        }
        let (bytes, phys) = ctx.potential(&[e], &held);
        d.potential_bytes = bytes;
        d.physical_ids = phys;
        out.push(d.finish(ctx));
    }
}

pub(crate) fn junk(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let held = {
        let mut h = ctx.held_inodes();
        h.extend(ctx.media_phys.iter().copied());
        h
    };
    for e in ctx.completed_files.iter().copied() {
        let Some(name) = e.path.rel.file_name() else {
            continue;
        };
        // Samples are never junk; videos never either.
        if is_sample(&e.path.rel) || ctx.settings.is_video(name) {
            continue;
        }
        let reason = if ctx.settings.is_junk_extension(name) {
            "junk_extension"
        } else if is_orphan_subtitle(&e.path.rel, ctx.settings, |dir| {
            ctx.video_stems
                .get(&(e.path.root.clone(), dir.clone()))
                .cloned()
                .unwrap_or_default()
        }) {
            "orphan_subtitle"
        } else {
            continue;
        };
        // Neither the path nor the inode may be held by a torrent (OQ-02, INV-27). With no
        // ownership information the item is still listed, blocked.
        if !ctx.owners_of_entry(e).is_empty() {
            continue;
        }
        let mut d = Draft::new(
            IssueCategory::Junk,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            Severity::Info,
            mtime_ms(&e.sig),
        );
        d.logical_bytes = e.sig.size;
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence
            .push(ctx.note(reason, ctx.fs_provenance(), ctx.scope.completeness(&e.path)));
        d.evidence.push(no_torrent_note(ctx));
        d.block(ctx.ownership_blockers());
        d.block(ctx.scope.path_blockers(&e.path));
        let (bytes, phys) = ctx.potential(&[e], &held);
        d.potential_bytes = bytes;
        d.physical_ids = phys;
        out.push(d.finish(ctx));
    }
}
