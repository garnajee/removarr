//! `hardlink_missing` (MASTER 5.2) and `duplicate` (MASTER 5.3).
//!
//! Both read the stored funnel output. A name+size match or a sample match is a *candidate*;
//! only `ContentProof::Identical` (a full byte comparison, run by VerifyContent) is a proven
//! match. A proof is used only while both files still carry the signatures it was computed
//! against (CP1-F07).

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, IdentityProof, MatchCandidate, Provenance,
};
use removarr_domain::fs::{EntryKind, FsEntry, PhysicalId};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Outcome, Severity, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::policy::is_sample;
use removarr_domain::torrent::TorrentHash;

use super::arrgate::{identity_blockers, import_blockers};
use super::ctx::{Ctx, Draft, mtime_ms};

pub(crate) struct Pair<'a> {
    pub m: &'a MatchCandidate,
    pub src: &'a FsEntry,
    pub tgt: &'a FsEntry,
    /// The stored proof still matches both current signatures.
    pub valid: bool,
}

impl Pair<'_> {
    pub fn content(&self) -> &ContentProof {
        if self.valid {
            &self.m.content
        } else {
            &ContentProof::None
        }
    }

    /// Equal sizes and a content proof that is not "different": the pair is a candidate for
    /// identical bytes (or proven so).
    pub fn same_bytes_candidate(&self) -> bool {
        self.valid
            && self.src.sig.size == self.tgt.sig.size
            && matches!(
                self.m.content,
                ContentProof::Identical { .. }
                    | ContentProof::Sampled
                    | ContentProof::NameSize
                    | ContentProof::Semantic { .. }
            )
    }

    pub fn differs(&self) -> bool {
        self.valid && self.m.content.proves_different()
    }
}

fn rank(c: &ContentProof) -> u8 {
    match c {
        ContentProof::Identical { .. } => 5,
        ContentProof::Sampled => 4,
        ContentProof::NameSize => 3,
        ContentProof::Semantic { .. } => 2,
        ContentProof::None => 1,
        ContentProof::Differs { .. } => 0,
    }
}

/// Stored candidates resolved against the current inventory (completed source, media target,
/// both regular files, distinct inodes). Pairs whose files disappeared are dropped.
pub(crate) fn pairs<'a>(ctx: &Ctx<'a>) -> Vec<Pair<'a>> {
    let mut out = Vec::new();
    for m in &ctx.obs.matches {
        let (Some(src), Some(tgt)) = (ctx.entries.get(&m.a), ctx.entries.get(&m.b)) else {
            continue;
        };
        if src.path.root.kind() != RootKind::Completed
            || tgt.path.root.kind() != RootKind::Media
            || src.sig.kind != EntryKind::File
            || tgt.sig.kind != EntryKind::File
            || src.sig.phys() == tgt.sig.phys()
        {
            continue;
        }
        out.push(Pair {
            m,
            src,
            tgt,
            valid: m.proof_still_valid(&src.sig, &tgt.sig),
        });
    }
    out.sort_by(|a, b| {
        a.src
            .path
            .cmp(&b.src.path)
            .then(rank(b.content()).cmp(&rank(a.content())))
            .then(a.tgt.path.cmp(&b.tgt.path))
    });
    out
}

/// Sources that share one target with another physically distinct source.
fn shared_targets(pairs: &[Pair<'_>]) -> BTreeSet<RootedPath> {
    let mut by_target: BTreeMap<&RootedPath, BTreeSet<PhysicalId>> = BTreeMap::new();
    for p in pairs.iter().filter(|p| p.same_bytes_candidate()) {
        by_target
            .entry(&p.tgt.path)
            .or_default()
            .insert(p.src.sig.phys());
    }
    by_target
        .into_iter()
        .filter(|(_, s)| s.len() > 1)
        .map(|(t, _)| t.clone())
        .collect()
}

fn content_evidence(ctx: &Ctx<'_>, p: &Pair<'_>) -> Vec<Evidence> {
    let (observed_at, completeness) = if p.valid {
        let at = match &p.m.content {
            ContentProof::Identical { compared_at } => *compared_at,
            _ => ctx.obs.observed_at,
        };
        (at, Completeness::Complete)
    } else {
        (
            ctx.obs.observed_at,
            Completeness::Incomplete {
                missing: vec!["current signature differs from the stored proof".into()],
            },
        )
    };
    let prov = Provenance::Filesystem {
        session: p.m.session,
    };
    vec![
        Evidence {
            kind: EvidenceKind::SamePhysical {
                a: p.src.sig.phys(),
                b: p.tgt.sig.phys(),
            },
            provenance: ctx.fs_provenance(),
            observed_at: ctx.obs.observed_at,
            completeness: Completeness::Complete,
        },
        Evidence {
            kind: EvidenceKind::Content {
                a: p.src.path.clone(),
                b: p.tgt.path.clone(),
                proof: p.content().clone(),
            },
            provenance: prov.clone(),
            observed_at,
            completeness: completeness.clone(),
        },
        Evidence {
            kind: EvidenceKind::Identity {
                subject: format!("pair:{}", p.src.path),
                proof: if p.valid {
                    p.m.identity.clone()
                } else {
                    IdentityProof::None
                },
            },
            provenance: prov,
            observed_at,
            completeness,
        },
    ]
}

/// Blockers of consolidating `p` (source -> target), shared by both categories.
fn consolidation_blockers(
    ctx: &Ctx<'_>,
    p: &Pair<'_>,
    shared: &BTreeSet<RootedPath>,
) -> Vec<Blocker> {
    let mut b = ctx.scope.path_blockers(&p.tgt.path);
    if p.src.sig.dev != p.tgt.sig.dev {
        b.push(Blocker::new(
            BlockerCode::CrossDevice,
            "source and target are on different devices",
        ));
    }
    if let Some(parent) = p.tgt.path.rel.parent() {
        let dir = RootedPath::new(p.tgt.path.root.clone(), parent);
        if ctx.unwritable_dirs.contains(&dir) {
            b.push(Blocker::new(
                BlockerCode::PermissionDenied,
                format!("target directory {dir} is not writable by the expected owner"),
            ));
        }
    }
    if shared.contains(&p.tgt.path) {
        b.push(Blocker::new(
            BlockerCode::DuplicateTarget,
            "another source maps to the same target",
        ));
    }
    b
}

pub(crate) fn hardlink_missing(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let all_pairs = pairs(ctx);
    let shared = shared_targets(&all_pairs);
    let mut stored_by_src: BTreeMap<&RootedPath, usize> = BTreeMap::new();
    for m in &ctx.obs.matches {
        *stored_by_src.entry(&m.a).or_default() += 1;
    }
    let mut by_src: BTreeMap<&RootedPath, Vec<&Pair<'_>>> = BTreeMap::new();
    for p in &all_pairs {
        by_src.entry(&p.src.path).or_default().push(p);
    }
    let media_roots: Vec<_> = ctx
        .scope
        .configured
        .iter()
        .filter(|r| r.kind() == RootKind::Media)
        .cloned()
        .collect();
    let absence = ctx.scope.absence_blockers(media_roots.into_iter());

    for e in &ctx.completed_files {
        let Some(name) = e.path.rel.file_name() else {
            continue;
        };
        if !ctx.settings.is_video(name) || ctx.media_phys.contains(&e.sig.phys()) {
            continue;
        }
        let sample = is_sample(&e.path.rel);
        let cands: &[&Pair<'_>] = by_src.get(&e.path).map_or(&[], Vec::as_slice);
        let same: Vec<&&Pair<'_>> = cands.iter().filter(|p| p.same_bytes_candidate()).collect();
        let best = same.first().copied().copied();
        let owners = ctx.owners_of_entry(e);

        let mut d = Draft::new(
            IssueCategory::HardlinkMissing,
            SubjectKey::rooted(&e.path),
            Unit::File(e.path.clone()),
            if sample {
                Severity::Info
            } else {
                Severity::Warning
            },
            mtime_ms(&e.sig),
        );
        d.logical_bytes = e.sig.size;
        d.torrent_hashes = owners.clone();
        d.evidence.push(ctx.fs_evidence(e));
        d.evidence.push(ctx.note(
            "no_inode_shared_with_media",
            ctx.fs_provenance(),
            Completeness::Complete,
        ));
        for h in &owners {
            if let Some(t) = ctx.torrent(h) {
                d.evidence.push(ctx.qbit_evidence(t, e.path.clone()));
            }
        }
        for p in cands {
            d.evidence.extend(content_evidence(ctx, p));
        }
        let stale = stored_by_src
            .get(&e.path)
            .copied()
            .unwrap_or(0)
            .saturating_sub(cands.len());
        if stale > 0 {
            d.evidence.push(ctx.note(
                "stored_candidates_no_longer_resolve",
                ctx.fs_provenance(),
                Completeness::Incomplete {
                    missing: vec!["current files for stored candidates".into()],
                },
            ));
        }

        // Outcome and its blockers.
        d.block(ctx.scope.path_blockers(&e.path));
        d.block(absence.clone());
        d.block(ctx.torrent_blockers(&owners));
        if sample {
            d.outcome = Outcome::KeepSample;
            d.block([Blocker::new(
                BlockerCode::Sample,
                "samples are protected by default",
            )]);
        } else if let Some(p) = best {
            d.outcome = Outcome::Consolidate;
            d.evidence.push(ctx.fs_evidence(p.tgt));
            d.block(consolidation_blockers(ctx, p, &shared));
            let (bytes, phys) = ctx.potential(&[p.tgt], &Default::default());
            d.potential_bytes = bytes;
            d.physical_ids = phys;
        } else {
            d.outcome = Outcome::KeepManual;
            let other: Vec<&&Pair<'_>> = cands.iter().filter(|p| !p.differs()).collect();
            if cands.iter().any(|p| p.differs()) && other.is_empty() {
                d.block([Blocker::new(
                    BlockerCode::ContentDiffers,
                    "a full comparison found different bytes",
                )]);
            }
            if let Some(first) = other.first() {
                // A different release of the same title is in the library: ARR decides, and
                // nothing here proves it (INV-11). The potential is a label, never a promise.
                d.block(identity_blockers(ctx, &owners, &[&first.tgt.path]));
                let (bytes, phys) = ctx.potential(&[e], &ctx.media_phys);
                d.potential_bytes = bytes;
                d.physical_ids = phys;
            }
        }
        let tgt_paths: Vec<&RootedPath> = best.map(|p| &p.tgt.path).into_iter().collect();
        let mut paths = vec![&e.path];
        paths.extend(tgt_paths);
        d.block(import_blockers(ctx, &owners, &paths));
        out.push(d.finish(ctx));
    }
}

pub(crate) fn duplicates(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let all_pairs = pairs(ctx);
    let shared = shared_targets(&all_pairs);
    let mut drafts: BTreeMap<SubjectKey, Draft> = BTreeMap::new();
    for p in all_pairs.iter().filter(|p| p.same_bytes_candidate()) {
        let subject = SubjectKey::pair(p.src.sig.phys(), p.tgt.sig.phys());
        if let Some(d) = drafts.get_mut(&subject) {
            // Another path of the same source inode: keep one issue, list the extra path.
            d.evidence.push(ctx.fs_evidence(p.src));
            continue;
        }
        let owners: BTreeSet<TorrentHash> = ctx.owners_of_entry(p.src);
        let mut d = Draft::new(
            IssueCategory::Duplicate,
            subject.clone(),
            Unit::Pair {
                a: p.src.path.clone(),
                b: p.tgt.path.clone(),
            },
            Severity::Warning,
            mtime_ms(&p.src.sig).max(mtime_ms(&p.tgt.sig)),
        );
        d.outcome = Outcome::Consolidate;
        d.logical_bytes = p.src.sig.size;
        d.torrent_hashes = owners.clone();
        d.evidence.push(ctx.fs_evidence(p.src));
        d.evidence.push(ctx.fs_evidence(p.tgt));
        d.evidence.extend(content_evidence(ctx, p));
        d.block(ctx.scope.path_blockers(&p.src.path));
        d.block(consolidation_blockers(ctx, p, &shared));
        d.block(ctx.torrent_blockers(&owners));
        if is_sample(&p.src.path.rel) {
            d.block([Blocker::new(
                BlockerCode::Sample,
                "samples are protected by default",
            )]);
        }
        let paths = [&p.src.path, &p.tgt.path];
        d.block(import_blockers(ctx, &owners, &paths));
        let (bytes, phys) = ctx.potential(&[p.tgt], &Default::default());
        d.potential_bytes = bytes;
        d.physical_ids = phys;
        drafts.insert(subject, d);
    }
    out.extend(drafts.into_values().map(|d| d.finish(ctx)));
}
