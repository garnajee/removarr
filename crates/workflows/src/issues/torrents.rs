//! Torrent-centred categories: `torrent_missing_files` (MASTER 5.4), `torrent_protected`
//! (MASTER 5.7, INV-28) and `unmapped_torrent` (OQ-07, CON-07). Torrent subjects are `t:<hash>`.

use std::collections::BTreeMap;

use removarr_domain::evidence::{Completeness, ContentProof, Evidence, EvidenceKind, Provenance};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Severity, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::torrent::{MappedPath, TorrentFile, TorrentInfo, TorrentState};

use super::ctx::{Ctx, Draft, short};
use super::index::{MappingFault, PathField};

fn first_mapped<'a>(t: &'a TorrentInfo, files: &'a [TorrentFile]) -> Option<&'a RootedPath> {
    t.content_path
        .as_rooted()
        .or_else(|| t.save_path.as_rooted())
        .or_else(|| {
            files
                .iter()
                .flat_map(|f| f.candidates.iter())
                .find_map(MappedPath::as_rooted)
        })
}

fn holds_in_root(t: &TorrentInfo, files: &[TorrentFile], kind: RootKind) -> bool {
    let is = |p: &MappedPath| p.as_rooted().is_some_and(|r| r.root.kind() == kind);
    is(&t.save_path)
        || t.download_path.as_ref().is_some_and(is)
        || is(&t.content_path)
        || files.iter().flat_map(|f| f.candidates.iter()).any(is)
}

fn qbit_provenance(ctx: &Ctx<'_>) -> Provenance {
    Provenance::Qbittorrent {
        taken_at: ctx.snapshot.map_or(ctx.obs.observed_at, |s| s.taken_at),
    }
}

pub(crate) fn missing_files(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let Some(snap) = ctx.snapshot else { return };
    // Library files by (name, size): the informational restore hint of OQ-14. It is a name+size
    // candidate and never a proof.
    let mut library: BTreeMap<(&str, u64), &RootedPath> = BTreeMap::new();
    for e in &ctx.media_files {
        if let Some(n) = e.path.rel.file_name() {
            library.entry((n, e.sig.size)).or_insert(&e.path);
        }
    }
    for t in &snap.torrents {
        let files = snap.files.get(&t.hash).map_or(&[][..], Vec::as_slice);
        let state_missing = t.state == TorrentState::MissingFiles;

        // Files proven absent by a clean scan: every candidate is mapped, its root finished with
        // no scan problem near it, and nothing is there. Anything less proves nothing.
        let mut absent: Vec<(&TorrentFile, &RootedPath)> = Vec::new();
        if t.progress >= 1.0 {
            for f in files.iter().filter(|f| f.is_wanted()) {
                let mapped: Vec<&RootedPath> = f
                    .candidates
                    .iter()
                    .filter_map(MappedPath::as_rooted)
                    .collect();
                let certain = !f.candidates.is_empty()
                    && mapped.len() == f.candidates.len()
                    && mapped.iter().all(|p| {
                        ctx.scope.completeness(p) == Completeness::Complete
                            && !ctx.entries.contains_key(*p)
                    });
                if certain && let Some(p) = mapped.first() {
                    absent.push((f, p));
                }
            }
        }
        if !state_missing && absent.is_empty() {
            continue;
        }
        let mut d = Draft::new(
            IssueCategory::TorrentMissingFiles,
            SubjectKey::torrent(&t.hash),
            Unit::Torrent(t.hash.clone()),
            Severity::Critical,
            t.completion_on.unwrap_or(t.added_on),
        );
        d.torrent_hashes.insert(t.hash.clone());
        d.logical_bytes = absent.iter().map(|(f, _)| f.size).sum();
        if let Some(p) = first_mapped(t, files) {
            d.evidence.push(ctx.qbit_evidence(t, p.clone()));
        } else {
            d.evidence.push(ctx.note(
                format!("torrent_state:{}", t.state.as_str()),
                qbit_provenance(ctx),
                Completeness::Complete,
            ));
            d.block([Blocker::new(
                BlockerCode::MappingError,
                "no torrent path could be mapped",
            )]);
        }
        for (f, p) in &absent {
            d.evidence.push(ctx.note(
                format!("file_absent:{p}"),
                ctx.fs_provenance(),
                Completeness::Complete,
            ));
            if let Some(lib) = f
                .name
                .rsplit('/')
                .next()
                .and_then(|n| library.get(&(n, f.size)))
            {
                d.evidence.push(Evidence {
                    kind: EvidenceKind::Content {
                        a: (*p).clone(),
                        b: (*lib).clone(),
                        proof: ContentProof::NameSize,
                    },
                    provenance: Provenance::Computed,
                    observed_at: ctx.obs.observed_at,
                    completeness: Completeness::Complete,
                });
            }
        }
        out.push(d.finish(ctx));
    }
}

pub(crate) fn protected(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let Some(snap) = ctx.snapshot else { return };
    for t in &snap.torrents {
        let Some(Some(p)) = ctx.protections.get(&t.hash) else {
            continue;
        };
        let files = snap.files.get(&t.hash).map_or(&[][..], Vec::as_slice);
        if !holds_in_root(t, files, RootKind::Completed) {
            continue;
        }
        let severity = if matches!(t.state, TorrentState::Error | TorrentState::Unknown(_)) {
            Severity::Critical
        } else {
            Severity::Info
        };
        let mut d = Draft::new(
            IssueCategory::TorrentProtected,
            SubjectKey::torrent(&t.hash),
            Unit::Torrent(t.hash.clone()),
            severity,
            t.added_on,
        );
        d.torrent_hashes.insert(t.hash.clone());
        d.logical_bytes = t.size;
        if let Some(path) = first_mapped(t, files) {
            d.evidence.push(ctx.qbit_evidence(t, path.clone()));
        } else {
            d.evidence.push(ctx.note(
                format!("torrent_state:{}", t.state.as_str()),
                qbit_provenance(ctx),
                Completeness::Complete,
            ));
        }
        d.block([Blocker::new(
            BlockerCode::TorrentProtectedState,
            format!("torrent {}: {}", short(&t.hash), p.detail()),
        )]);
        out.push(d.finish(ctx));
    }
}

pub(crate) fn unmapped(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    let Some(snap) = ctx.snapshot else { return };
    for t in &snap.torrents {
        let Some(faults) = ctx.faults.get(&t.hash) else {
            continue;
        };
        let error = faults.iter().any(|(_, k)| k.is_error());
        let mut d = Draft::new(
            IssueCategory::UnmappedTorrent,
            SubjectKey::torrent(&t.hash),
            Unit::Torrent(t.hash.clone()),
            if error {
                Severity::Critical
            } else {
                Severity::Info
            },
            t.added_on,
        );
        d.torrent_hashes.insert(t.hash.clone());
        d.logical_bytes = t.size;
        // Only the field and the outcome are recorded: never the raw service path.
        for (field, kind) in faults {
            d.evidence.push(ctx.note(
                format!("mapping:{}:{}", field_name(*field), kind.as_str()),
                qbit_provenance(ctx),
                Completeness::Complete,
            ));
        }
        for (_, kind) in faults {
            match kind {
                MappingFault::NoRule => d.block([Blocker::new(
                    BlockerCode::UnmappedTorrents,
                    format!("torrent {}: no mapping rule matches", short(&t.hash)),
                )]),
                MappingFault::Invalid => d.block([Blocker::new(
                    BlockerCode::MappingError,
                    format!("torrent {}: mapped path is invalid", short(&t.hash)),
                )]),
                MappingFault::DeclaredExternal | MappingFault::OutsideRoots => {}
            }
        }
        out.push(d.finish(ctx));
    }
}

fn field_name(f: PathField) -> &'static str {
    f.as_str()
}
