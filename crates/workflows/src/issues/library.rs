//! `media_without_source` (informational, INV-26), `symlink` (OQ-15) and `permission`
//! (MASTER 5.5, INV-32).

use std::collections::BTreeMap;

use removarr_domain::arr::correlate::Correlation;
use removarr_domain::evidence::{Completeness, Evidence, EvidenceKind, IdentityProof, Provenance};
use removarr_domain::fs::{EntryKind, PermissionKind};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Severity, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::policy::is_sample;

use super::ctx::{Ctx, Draft, mtime_ms};

/// Library videos with no completed source. Often normal (the torrent was removed after the
/// seed): information only, with no potential gain and a permanent `MediaRootDeletion` blocker.
pub(crate) fn media_without_source(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let completed_roots: Vec<_> = ctx
        .scope
        .configured
        .iter()
        .filter(|r| r.kind() == RootKind::Completed)
        .cloned()
        .collect();
    let absence = ctx.scope.absence_blockers(completed_roots.into_iter());
    for e in &ctx.media_files {
        let Some(name) = e.path.rel.file_name() else {
            continue;
        };
        if !ctx.settings.is_video(name) || ctx.completed_phys.contains(&e.sig.phys()) {
            continue;
        }
        let mut d = Draft::new(
            IssueCategory::MediaWithoutSource,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            Severity::Info,
            mtime_ms(&e.sig),
        );
        d.logical_bytes = e.sig.size;
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence.push(ctx.note(
            "no_inode_shared_with_completed",
            ctx.fs_provenance(),
            Completeness::Complete,
        ));
        // `nlink` is accounting only: more links than seen means a link outside the scanned
        // trees may exist. It never proves or disproves a source (INV-01).
        if e.sig.nlink > u64::from(ctx.links_seen.get(&e.sig.phys()).copied().unwrap_or(0)) {
            d.evidence.push(ctx.note(
                "links_outside_scanned_trees_possible",
                ctx.fs_provenance(),
                Completeness::Complete,
            ));
        }
        match ctx.corr.by_path(&e.path) {
            Correlation::Unique(m) => d.evidence.push(Evidence {
                kind: EvidenceKind::Identity {
                    subject: format!("arr:{}:{}", m.service.as_str(), m.media_id),
                    proof: IdentityProof::ArrMatched,
                },
                provenance: Provenance::Arr {
                    service: m.service,
                    captured_at: ctx
                        .obs
                        .arr
                        .iter()
                        .find(|a| a.service == m.service)
                        .map_or(ctx.obs.observed_at, |a| a.captured_at),
                },
                observed_at: ctx.obs.observed_at,
                completeness: Completeness::Complete,
            }),
            Correlation::NotFound if ctx.obs.arr.iter().any(|a| a.is_ok()) => {
                d.evidence.push(ctx.note(
                    "unmanaged_by_arr",
                    Provenance::Computed,
                    Completeness::Complete,
                ));
            }
            _ => d.evidence.push(ctx.note(
                "arr_identity_unknown",
                Provenance::Computed,
                Completeness::Unknown,
            )),
        }
        d.block([Blocker::new(
            BlockerCode::MediaRootDeletion,
            "files under media roots are never deleted automatically",
        )]);
        d.block(absence.clone());
        d.block(ctx.scope.path_blockers(&e.path));
        if is_sample(&e.path.rel) {
            d.block([Blocker::new(
                BlockerCode::Sample,
                "samples are protected by default",
            )]);
        }
        out.push(d.finish(ctx));
    }
}

/// Symlinks inside managed roots: recorded, never followed, never a hardlink, never actionable.
/// The link target text is deliberately not stored (it could name an undeclared path, INV-09).
pub(crate) fn symlinks(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    for e in ctx
        .entries
        .values()
        .filter(|e| e.sig.kind == EntryKind::Symlink)
    {
        let mut d = Draft::new(
            IssueCategory::Symlink,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            Severity::Info,
            mtime_ms(&e.sig),
        );
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence.push(ctx.note(
            "symlink_not_followed",
            ctx.fs_provenance(),
            Completeness::Complete,
        ));
        d.block([Blocker::new(
            BlockerCode::Symlink,
            "symlinks are never followed or acted on",
        )]);
        out.push(d.finish(ctx));
    }
}

pub(crate) fn permissions(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let (euid, _) = ctx.settings.expected_owner;
    let mut by_path: BTreeMap<&RootedPath, Vec<&PermissionKind>> = BTreeMap::new();
    for f in &ctx.findings {
        by_path.entry(&f.path).or_default().push(&f.kind);
    }
    for (path, kinds) in by_path {
        // The root directory itself has no entry: its signature is the inventory's `root_sig`.
        let sig = ctx.entries.get(path).map(|e| e.sig).or_else(|| {
            ctx.scope
                .inventory(&path.root)
                .filter(|_| path.rel.is_root())
                .and_then(|i| i.root_sig)
        });
        let Some(sig) = sig else { continue };
        let critical = kinds
            .iter()
            .any(|k| matches!(k, PermissionKind::ParentNotWritable));
        let mut d = Draft::new(
            IssueCategory::Permission,
            SubjectKey::rooted(path),
            Unit::File(path.clone()),
            if critical {
                Severity::Critical
            } else {
                Severity::Warning
            },
            mtime_ms(&sig),
        );
        d.logical_bytes = if sig.kind == EntryKind::File {
            sig.size
        } else {
            0
        };
        d.evidence.push(Evidence {
            kind: EvidenceKind::Physical {
                path: path.clone(),
                sig,
            },
            provenance: ctx.fs_provenance(),
            observed_at: ctx.obs.observed_at,
            completeness: ctx.scope.completeness(path),
        });
        let mut fixable_only = true;
        for k in &kinds {
            let text = match k {
                PermissionKind::OwnerMismatch { uid, gid } => {
                    fixable_only = false;
                    format!("owner_mismatch:{uid}:{gid}")
                }
                PermissionKind::ParentNotWritable => "parent_not_writable".to_string(),
                PermissionKind::NotWritableByOwner => "not_writable_by_owner".to_string(),
            };
            d.evidence.push(ctx.note(
                format!("permission:{text}"),
                ctx.fs_provenance(),
                Completeness::Complete,
            ));
        }
        // No ownership change is ever offered, and a mode change needs the entry to belong to the
        // application identity: both leave the issue manual (INV-32).
        if !fixable_only {
            d.block([Blocker::new(
                BlockerCode::PermissionDenied,
                "an ownership change is not offered; fix it outside the application",
            )]);
        } else if sig.uid != euid {
            d.block([Blocker::new(
                BlockerCode::PermissionDenied,
                "entry is not owned by the expected owner; only its owner can change its mode",
            )]);
        }
        d.block(ctx.scope.path_blockers(path));
        out.push(d.finish(ctx));
    }
}
