//! `arr_queue` (CONTEXTE 3.9, 6.2; INV-39). A queue alert never proves a missing hardlink:
//! check inodes first, then parent permissions, and never hide an error by removing an entry.

use removarr_domain::arr::correlate::{QueueHealth, import_in_progress, queue_health};
use removarr_domain::arr::{ArrLibraryEntry, ArrQueueItem};
use removarr_domain::evidence::{Completeness, Evidence, EvidenceKind, Provenance};
use removarr_domain::fs::{EntryKind, PhysicalId};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, IssueCategory, Severity, Unit};
use removarr_domain::path::RootedPath;
use removarr_domain::torrent::MappedPath;

use super::ctx::{Ctx, Draft};
use super::input::ArrObservation;

fn same_media(item: &ArrQueueItem, e: &ArrLibraryEntry) -> bool {
    let Some(m) = &item.media else { return false };
    m.service == e.media.service
        && m.media_id == e.media.media_id
        && (m.episode_ids.is_empty()
            || e.media.episode_ids.is_empty()
            || m.episode_ids
                .iter()
                .any(|i| e.media.episode_ids.contains(i)))
}

pub(crate) fn queue(ctx: &Ctx<'_>, out: &mut Vec<Issue>) {
    for a in ctx.obs.arr.iter().filter(|a| a.is_ok()) {
        for q in &a.queue {
            let health = queue_health(q);
            let attention = health != QueueHealth::Ok
                || removarr_domain::arr::correlate::import_activity(q)
                    == removarr_domain::arr::correlate::ImportActivity::Blocked;
            // Torrent-only product: an item without a download id (usenet) has no unit.
            let (true, Some(hash)) = (attention, q.download_id.as_ref()) else {
                continue;
            };
            out.push(queue_issue(ctx, a, q, hash, health));
        }
    }
}

fn queue_issue(
    ctx: &Ctx<'_>,
    a: &ArrObservation,
    q: &ArrQueueItem,
    hash: &removarr_domain::torrent::TorrentHash,
    health: QueueHealth,
) -> Issue {
    let torrent = ctx.torrent(hash);
    let mut d = Draft::new(
        IssueCategory::ArrQueue,
        SubjectKey::queue(a.service, q.id),
        Unit::Torrent(hash.clone()),
        if health == QueueHealth::Error {
            Severity::Critical
        } else {
            Severity::Warning
        },
        torrent.map_or(ctx.obs.observed_at, |t| t.added_on),
    );
    d.torrent_hashes.insert(hash.clone());
    let arr_prov = Provenance::Arr {
        service: a.service,
        captured_at: a.captured_at,
    };
    // Status codes only: ARR messages can carry release names and paths, so they are not kept.
    d.evidence.push(ctx.note(
        format!("arr_queue:status:{}:state:{}", q.status, q.tracked_state),
        arr_prov.clone(),
        Completeness::Complete,
    ));

    // INV-39 step 1: do the download's completed videos already share an inode with the media
    // files ARR knows for this media?
    let media_entries: Vec<&RootedPath> = a
        .library
        .iter()
        .filter(|e| same_media(q, e))
        .filter_map(|e| match &e.file.path {
            MappedPath::Mapped { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    let media_present: Vec<_> = media_entries
        .iter()
        .filter_map(|p| ctx.entries.get(*p).copied())
        .filter(|e| e.sig.kind == EntryKind::File)
        .collect();

    let mut proof_failure: Option<String> = None;
    match (ctx.snapshot, torrent) {
        (Some(snap), Some(_)) => {
            let files = snap.files.get(hash).map_or(&[][..], Vec::as_slice);
            let sources: Vec<_> = files
                .iter()
                .filter(|f| {
                    f.name
                        .rsplit('/')
                        .next()
                        .is_some_and(|n| ctx.settings.is_video(n))
                })
                .filter_map(|f| {
                    f.candidates
                        .iter()
                        .filter_map(MappedPath::as_rooted)
                        .find_map(|p| ctx.entries.get(p).copied())
                })
                .filter(|e| e.sig.kind == EntryKind::File)
                .collect();
            if sources.is_empty() {
                proof_failure = Some("no completed video of this download is on disk".into());
            }
            let media_phys: std::collections::BTreeSet<PhysicalId> =
                media_present.iter().map(|e| e.sig.phys()).collect();
            for s in &sources {
                if media_phys.contains(&s.sig.phys()) {
                    d.evidence.push(Evidence {
                        kind: EvidenceKind::SamePhysical {
                            a: s.sig.phys(),
                            b: s.sig.phys(),
                        },
                        provenance: ctx.fs_provenance(),
                        observed_at: ctx.obs.observed_at,
                        completeness: ctx.scope.completeness(&s.path),
                    });
                    d.evidence.push(ctx.fs_evidence(s));
                } else if proof_failure.is_none() {
                    proof_failure =
                        Some("a completed video is not linked into the media tree".into());
                }
            }
        }
        _ => {
            proof_failure = Some("torrent files are not observed".into());
            d.block(ctx.qbit_service_blockers());
        }
    }

    // INV-39 step 2: a permission finding on the media parent directories.
    let mut permission_problem = false;
    for p in &media_entries {
        if let Some(parent) = p.rel.parent() {
            let dir = RootedPath::new(p.root.clone(), parent);
            if ctx.unwritable_dirs.contains(&dir) {
                permission_problem = true;
                d.evidence.push(ctx.note(
                    format!("permission:parent_not_writable:{dir}"),
                    ctx.fs_provenance(),
                    Completeness::Complete,
                ));
            }
        }
    }

    if let Some(why) = proof_failure {
        d.block([Blocker::new(
            BlockerCode::QueueItemHidesError,
            format!("the inode check did not prove the import: {why}"),
        )]);
    } else if permission_problem {
        d.block([Blocker::new(
            BlockerCode::QueueItemHidesError,
            "a media parent directory has a permission finding",
        )]);
    }
    if q.media.is_none() {
        d.block([Blocker::new(
            BlockerCode::ArrAmbiguous,
            "queue item has no media identity",
        )]);
    }
    if import_in_progress(q) {
        d.block([Blocker::new(
            BlockerCode::ImportInProgress,
            format!(
                "{} queue item {} may be importing",
                a.service.as_str(),
                q.id
            ),
        )]);
    }
    d.finish(ctx)
}
