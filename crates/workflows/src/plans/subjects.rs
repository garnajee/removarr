//! Per-action subject keys and paths (tier 5, T5-09/T5-10). The domain's `PlannedAction` has no
//! subject field, so they are derived deterministically from the action itself. The job runner
//! (14) builds its failed sets from them, and the ignore list is re-checked per action.

use std::collections::BTreeSet;

use removarr_domain::ids::SubjectKey;
use removarr_domain::path::RootedPath;
use removarr_domain::plan::{ActionTarget, PlannedAction, Precondition};
use removarr_domain::torrent::TorrentHash;

use super::validate::snapshot_paths;

/// Paths an action changes or removes: consolidation source and target, deletion and mode-fix
/// targets with every entry of a folder snapshot, stalled data, replacement sources. Evidence
/// pins (replacement media files, other links) are not part of it.
#[must_use]
pub fn action_paths(a: &PlannedAction) -> BTreeSet<RootedPath> {
    paths_of(&a.target, &a.preconditions)
}

pub(crate) fn paths_of(target: &ActionTarget, pre: &[Precondition]) -> BTreeSet<RootedPath> {
    let mut out = BTreeSet::new();
    match target {
        ActionTarget::Consolidate { source, target } => {
            out.insert(source.clone());
            out.insert(target.clone());
        }
        ActionTarget::Path { path } | ActionTarget::FixMode { path, .. } => {
            out.insert(path.clone());
        }
        ActionTarget::Torrent { data_paths, .. } => out.extend(data_paths.iter().cloned()),
        ActionTarget::Replace { request } => {
            out.extend(request.files.iter().map(|(p, _)| p.clone()));
        }
        ActionTarget::Release { .. }
        | ActionTarget::Queue { .. }
        | ActionTarget::Monitor { .. }
        | ActionTarget::Rescan { .. }
        | ActionTarget::JellyfinRefresh { .. } => {}
    }
    for p in pre {
        if let Precondition::DirUnchanged { snapshot } = p
            && out.contains(&snapshot.dir)
        {
            out.extend(snapshot_paths(snapshot));
        }
    }
    out
}

/// Every subject key the action concerns: the issue subject it was planned from, plus the
/// subjects of its paths, torrents and (for a consolidation) the physical pair. An ignored or
/// recently failed key among them excludes the action from automatic mode.
#[must_use]
pub fn action_subjects(a: &PlannedAction) -> BTreeSet<SubjectKey> {
    subjects_of(&a.target, &a.preconditions, &a.torrent_hashes)
}

pub(crate) fn subjects_of(
    target: &ActionTarget,
    pre: &[Precondition],
    hashes: &BTreeSet<TorrentHash>,
) -> BTreeSet<SubjectKey> {
    let paths = paths_of(target, pre);
    let mut out: BTreeSet<SubjectKey> = paths.iter().map(SubjectKey::rooted).collect();
    out.extend(hashes.iter().map(SubjectKey::torrent));
    match target {
        ActionTarget::Consolidate { source, target } => {
            let phys = |path: &RootedPath| {
                pre.iter().find_map(|p| match p {
                    Precondition::EntryUnchanged { path: q, sig, .. } if q == path => {
                        Some(sig.phys())
                    }
                    _ => None,
                })
            };
            if let (Some(s), Some(t)) = (phys(source), phys(target)) {
                out.insert(SubjectKey::pair(s, t));
            }
        }
        ActionTarget::Torrent { hash, .. } => {
            out.insert(SubjectKey::torrent(hash));
        }
        ActionTarget::Queue { service, id } => {
            out.insert(SubjectKey::queue(*service, *id));
        }
        ActionTarget::Monitor { media, .. } | ActionTarget::Release { media, .. } => {
            out.insert(SubjectKey(format!(
                "m:{}:{}",
                media.service.as_str(),
                media.media_id
            )));
        }
        _ => {}
    }
    out
}
