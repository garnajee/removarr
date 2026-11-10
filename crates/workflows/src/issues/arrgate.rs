//! ARR-dependent blockers: unknown identity or quality requires manual review (INV-11), and a
//! media being imported blocks every mutation of it (INV-16).

use std::collections::BTreeSet;

use removarr_domain::arr::correlate::{Correlation, import_in_progress};
use removarr_domain::arr::{ArrMediaRef, ArrService};
use removarr_domain::issue::{Blocker, BlockerCode};
use removarr_domain::path::RootedPath;
use removarr_domain::session::{ServiceKind, ServiceState};
use removarr_domain::torrent::TorrentHash;

use super::ctx::Ctx;

#[must_use]
pub(crate) fn kind(s: ArrService) -> ServiceKind {
    match s {
        ArrService::Sonarr => ServiceKind::Sonarr,
        ArrService::Radarr => ServiceKind::Radarr,
    }
}

/// Blockers for a decision that needs ARR identity and quality. At classification time no
/// quality snapshot exists yet (it is gathered by Evaluate), so even a unique identity leaves
/// `ArrNotUpgradeEvidence`: nothing here proves a file is replaceable.
pub(crate) fn identity_blockers(
    ctx: &Ctx<'_>,
    hashes: &BTreeSet<TorrentHash>,
    paths: &[&RootedPath],
) -> Vec<Blocker> {
    let mut out = Vec::new();
    for a in &ctx.obs.arr {
        match &a.state {
            ServiceState::Ok { .. } => {}
            ServiceState::NotConfigured => out.push(Blocker::new(
                BlockerCode::ServiceNotConfigured(kind(a.service)),
                "ARR identity is unknown",
            )),
            _ => out.push(Blocker::new(
                BlockerCode::ServiceUnavailable(kind(a.service)),
                "ARR identity is unknown",
            )),
        }
    }
    if !ctx.obs.arr.iter().any(|a| a.is_ok()) {
        out.push(Blocker::new(
            BlockerCode::ArrMissing,
            "no ARR service was observed",
        ));
        return out;
    }
    match ctx.corr.for_content(hashes.iter(), paths.iter().copied()) {
        Correlation::Unique(_) => out.push(Blocker::new(
            BlockerCode::ArrNotUpgradeEvidence,
            "no ARR quality evidence yet",
        )),
        Correlation::Ambiguous(_) | Correlation::Unidentified => out.push(Blocker::new(
            BlockerCode::ArrAmbiguous,
            "ARR identity is ambiguous",
        )),
        Correlation::NotFound => out.push(Blocker::new(
            BlockerCode::ArrMissing,
            "ARR does not know this content",
        )),
    }
    out
}

/// `ImportInProgress` when ARR may be importing the media these hashes or paths belong to
/// (an unknown queue state counts as in progress).
pub(crate) fn import_blockers(
    ctx: &Ctx<'_>,
    hashes: &BTreeSet<TorrentHash>,
    paths: &[&RootedPath],
) -> Vec<Blocker> {
    let media: Option<ArrMediaRef> = ctx
        .corr
        .for_content(hashes.iter(), paths.iter().copied())
        .unique()
        .cloned();
    let mut out = Vec::new();
    for a in ctx.obs.arr.iter().filter(|a| a.is_ok()) {
        for q in a.queue.iter().filter(|q| import_in_progress(q)) {
            let by_hash = q.download_id.as_ref().is_some_and(|h| hashes.contains(h));
            let by_media = match (&q.media, &media) {
                (Some(qm), Some(m)) => qm.service == m.service && qm.media_id == m.media_id,
                _ => false,
            };
            if by_hash || by_media {
                out.push(Blocker::new(
                    BlockerCode::ImportInProgress,
                    format!(
                        "{} queue item {} may be importing",
                        a.service.as_str(),
                        q.id
                    ),
                ));
            }
        }
    }
    out
}
