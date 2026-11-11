//! ARR-dependent actions: `DeleteReplaced` deletions (CP1-F02, tier 3), release replacement
//! through ARR (ADR-0010, tier 4) and approved grabs (CP1-F06, tier 4). ARR is the only quality
//! authority (INV-11): every verdict below is ARR's own, read fresh for this plan, and its
//! snapshot fingerprint is a precondition.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::arr::{
    ArrCandidateDecision, ArrFileRef, ArrMediaManagement, ArrMediaRef, ArrService, CoverageProof,
    ManualImportRequest, OldFileRecovery, ReleaseRef,
};
use removarr_domain::evidence::{Completeness, EvidenceLevel};
use removarr_domain::fs::{EntryKind, FsEntry, PhysicalId};
use removarr_domain::ids::SubjectKey;
use removarr_domain::issue::{Blocker, BlockerCode, Issue, Unit};
use removarr_domain::path::{RootKind, RootedPath};
use removarr_domain::plan::{
    ActionKind, ActionTarget, ArrSnapshotRef, PlanOrigin, Precondition, VerificationStep,
};
use removarr_domain::policy::is_sample;
use removarr_domain::torrent::{MappedPath, TorrentHash};
use removarr_matching::{
    MediaFacts, assess_replacement, numbering::explain_numbering, parse_scene_name,
};

use super::build::{Draft, UnitPlan, UnitResult, is_kind, issue_completeness};
use super::facts::{Facts, arr_kind};
use super::fsplans::{self, kind_of};
use crate::evaluate::ArrFacts;

/// ARR's rejection of a local file as "not an upgrade" (or "existing file is equal or
/// better"). Reasons the application added itself (`removarr: ...`) never count.
#[must_use]
pub fn is_not_upgrade_reason(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    !r.starts_with("removarr:") && (r.contains("upgrade") || r.contains("equal or higher"))
}

fn tag(path: &RootedPath, b: Blocker) -> Blocker {
    Blocker::new(b.code, format!("{path}: {}", b.detail))
}

fn facts_for<'f>(f: &'f Facts<'_>, media: &ArrMediaRef) -> Result<&'f ArrFacts, Blocker> {
    match f.input.arr.get(&(media.service, media.media_id)) {
        Some(Ok(a)) if a.snapshot.fingerprint_is_valid() => Ok(a),
        Some(Ok(_)) => Err(Blocker::new(
            BlockerCode::ArrAmbiguous,
            "ARR snapshot fingerprint does not match its content",
        )),
        Some(Err(e)) => Err(Blocker::new(
            BlockerCode::ServiceUnavailable(kind_of(media)),
            format!("ARR read failed ({:?})", e.class),
        )),
        None => Err(Blocker::new(
            BlockerCode::ArrMissing,
            "no fresh ARR evidence was gathered for this media",
        )),
    }
}

fn snapshot_ref(a: &ArrFacts) -> ArrSnapshotRef {
    ArrSnapshotRef {
        media: a.media.clone(),
        fingerprint: a.snapshot.fingerprint.clone(),
    }
}

/// Media paths of the stored candidates of `path` that still match the inventory and were not
/// proven different (identity evidence through the library, as the classifier uses it).
fn candidate_media_paths<'f>(f: &'f Facts<'_>, path: &RootedPath) -> Vec<&'f RootedPath> {
    f.input
        .observation
        .matches
        .iter()
        .filter(|m| &m.a == path && is_kind(&m.b, RootKind::Media))
        .filter(|m| {
            matches!((f.entry(&m.a), f.entry(&m.b)), (Some(a), Some(b))
                if m.proof_still_valid(&a.sig, &b.sig) && !m.content.proves_different())
        })
        .map(|m| &m.b)
        .collect()
}

/// Unique ARR media of a completed file (torrent hashes and library candidates).
pub(crate) fn identity_of(
    f: &Facts<'_>,
    issue: &Issue,
    path: &RootedPath,
) -> Result<ArrMediaRef, Blocker> {
    let mut hashes: BTreeSet<TorrentHash> = issue.torrent_hashes.clone();
    hashes.extend(f.holders(path));
    let cands = candidate_media_paths(f, path);
    f.identity(&hashes, &cands)
}

/// ARR's preview verdict for one completed file.
pub fn decision<'f>(a: &'f ArrFacts, path: &RootedPath) -> Option<&'f ArrCandidateDecision> {
    let folder = RootedPath::new(path.root.clone(), path.rel.parent()?);
    let subject = path.to_string();
    a.previews
        .get(&folder)?
        .iter()
        .find(|d| d.subject == subject)
}

/// ARR's current files for the episodes ARR assigns (Sonarr), or the movie file (Radarr).
fn current_files<'f>(a: &'f ArrFacts, episode_ids: &[i64]) -> Result<Vec<&'f ArrFileRef>, Blocker> {
    let by_id = |id: i64| a.snapshot.current_files.iter().find(|c| c.file_id == id);
    match a.media.service {
        ArrService::Radarr => match a.snapshot.current_files.as_slice() {
            [one] => Ok(vec![one]),
            [] => Err(Blocker::new(
                BlockerCode::CoverageIncomplete,
                "ARR has no current movie file",
            )),
            _ => Err(Blocker::new(
                BlockerCode::ArrAmbiguous,
                "ARR lists several current movie files",
            )),
        },
        ArrService::Sonarr => {
            if episode_ids.is_empty() {
                return Err(Blocker::new(
                    BlockerCode::ArrAmbiguous,
                    "ARR assigns no episode to the file",
                ));
            }
            let mut out: Vec<&ArrFileRef> = Vec::new();
            for id in episode_ids {
                let file = a
                    .episodes
                    .iter()
                    .find(|e| e.id == *id)
                    .filter(|e| e.has_file)
                    .and_then(|e| e.file_id)
                    .and_then(by_id)
                    .ok_or_else(|| {
                        Blocker::new(
                            BlockerCode::CoverageIncomplete,
                            format!("episode {id} has no current file in ARR"),
                        )
                    })?;
                if !out.iter().any(|o| o.file_id == file.file_id) {
                    out.push(file);
                }
            }
            Ok(out)
        }
    }
}

/// A current ARR file that must be physically present (CP1-F02): mapped into a media root,
/// a regular file in the latest scan, with ARR's size, and not one of `targets`.
fn present_file<'a>(
    f: &Facts<'a>,
    file: &ArrFileRef,
    targets: &BTreeSet<PhysicalId>,
) -> Result<&'a FsEntry, Vec<Blocker>> {
    let amb = |m: String| vec![Blocker::new(BlockerCode::ArrAmbiguous, m)];
    let Some(path) = file
        .path
        .as_rooted()
        .filter(|p| is_kind(p, RootKind::Media))
    else {
        return Err(amb(format!(
            "ARR file {} does not map into a media root",
            file.file_id
        )));
    };
    let Some(e) = f.entry(path).filter(|e| e.sig.kind == EntryKind::File) else {
        return Err(amb(format!(
            "{path} is not a regular file in the latest scan"
        )));
    };
    if e.sig.size != file.size {
        return Err(amb(format!(
            "{path}: size {} differs from ARR's {}",
            e.sig.size, file.size
        )));
    }
    if targets.contains(&e.sig.phys()) {
        return Err(amb(format!(
            "{path} is the same inode as a deletion target"
        )));
    }
    let scope = f.scope_blockers(path);
    if scope.is_empty() { Ok(e) } else { Err(scope) }
}

/// ARR checks of one video of a `DeleteReplaced` unit.
fn replaced_member(
    f: &Facts<'_>,
    issue: &Issue,
    v: &RootedPath,
    targets: &BTreeSet<PhysicalId>,
) -> Result<(Vec<Precondition>, ArrSnapshotRef), Vec<Blocker>> {
    let one = |b: Blocker| vec![tag(v, b)];
    let media = identity_of(f, issue, v).map_err(one)?;
    let a = facts_for(f, &media).map_err(one)?;
    let d = decision(a, v).ok_or_else(|| {
        one(Blocker::new(
            BlockerCode::ArrMissing,
            "ARR's import preview does not list the file",
        ))
    })?;
    if d.approved {
        return Err(one(Blocker::new(
            BlockerCode::ArrNotUpgradeEvidence,
            "ARR would import the file: it is not replaced",
        )));
    }
    if d.rejections
        .iter()
        .any(|r| r.to_ascii_lowercase().starts_with("removarr:"))
    {
        return Err(one(Blocker::new(
            BlockerCode::ArrAmbiguous,
            "ARR's preview could not identify the file for this media",
        )));
    }
    if d.rejections.is_empty() || !d.rejections.iter().all(|r| is_not_upgrade_reason(r)) {
        return Err(one(Blocker::new(
            BlockerCode::ArrRejected,
            "ARR rejects the file for a reason other than 'not an upgrade'",
        )));
    }
    let files = current_files(a, &d.episode_ids).map_err(one)?;
    let mut pre = Vec::new();
    let mut b = Vec::new();
    for file in files {
        match present_file(f, file, targets) {
            Ok(e) => {
                pre.push(Precondition::ArrReplacementPresent {
                    media: a.media.clone(),
                    path: e.path.clone(),
                    size: file.size,
                });
                pre.push(Precondition::EntryUnchanged {
                    path: e.path.clone(),
                    sig: e.sig,
                    compare_nlink: false,
                });
            }
            Err(errs) => b.extend(errs.into_iter().map(|x| tag(v, x))),
        }
    }
    if !b.is_empty() {
        return Err(b);
    }
    pre.push(Precondition::ArrFingerprint {
        media: a.media.clone(),
        fingerprint: a.snapshot.fingerprint.clone(),
    });
    pre.push(Precondition::ServiceAvailable(arr_kind(media.service)));
    Ok((pre, snapshot_ref(a)))
}

/// `DeleteReplaced`: a deletion whose every video ARR rejects as not an upgrade and whose
/// replacement is physically present. A folder is all-or-nothing: one failing member blocks
/// the whole unit and the blocker names it (T-21).
pub(crate) fn delete_replaced<'a>(
    f: &Facts<'a>,
    issue: &Issue,
    kind: ActionKind,
    origin: PlanOrigin,
) -> UnitResult<'a> {
    let base = fsplans::delete(f, issue, kind, origin);
    let videos: Vec<&RootedPath> = match &issue.unit {
        Unit::File(p) => vec![p],
        Unit::Folder { members, .. } => members.iter().collect(),
        _ => vec![],
    };
    let mut blockers = Vec::new();
    if videos.is_empty() {
        blockers.push(Blocker::new(
            BlockerCode::ArrMissing,
            "the unit holds no video ARR could identify",
        ));
    }
    let targets: BTreeSet<PhysicalId> = match &base {
        Ok(u) => u
            .drafts
            .iter()
            .flat_map(|d| d.freed.iter().map(|e| e.sig.phys()))
            .collect(),
        Err(_) => videos
            .iter()
            .filter_map(|v| f.entry(v).map(|e| e.sig.phys()))
            .collect(),
    };
    let mut pre = Vec::new();
    let mut refs = Vec::new();
    for v in videos {
        match replaced_member(f, issue, v, &targets) {
            Ok((p, r)) => {
                pre.extend(p);
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
            Err(b) => blockers.extend(b),
        }
    }
    match base {
        Err(b) => {
            blockers.extend(b);
            Err(blockers)
        }
        Ok(_) if !blockers.is_empty() => Err(blockers),
        Ok(mut u) => {
            if let Some(d) = u.drafts.first_mut() {
                for p in pre {
                    if !d.pre.contains(&p) {
                        d.pre.push(p);
                    }
                }
                // Every CP1-F02 check passed for every video: ARR-validated evidence (patch 2).
                d.level = EvidenceLevel::ArrValidatedReplacement;
            }
            u.arr = refs;
            Ok(u)
        }
    }
}

/// Options of a release replacement request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplaceOptions {
    pub delete_old_media: bool,
    pub include_specials: bool,
}

/// Groups the selected files by ARR media; one `ReplaceRelease` per media (coverage is a
/// per-media proof, so a group is all-or-nothing).
pub(crate) fn replace_groups<'a>(
    f: &Facts<'a>,
    issues: &[Issue],
    opts: ReplaceOptions,
) -> Vec<(Vec<SubjectKey>, UnitResult<'a>)> {
    let mut out = Vec::new();
    let mut groups: BTreeMap<(ArrService, i64), (ArrMediaRef, Vec<&Issue>)> = BTreeMap::new();
    for issue in issues {
        let Unit::File(path) = &issue.unit else {
            out.push((
                vec![issue.subject.clone()],
                Err(vec![Blocker::new(
                    BlockerCode::ArrAmbiguous,
                    "only single files can be replaced",
                )]),
            ));
            continue;
        };
        match identity_of(f, issue, path) {
            Ok(m) => {
                groups
                    .entry((m.service, m.media_id))
                    .or_insert_with(|| (m, Vec::new()))
                    .1
                    .push(issue);
            }
            Err(b) => out.push((vec![issue.subject.clone()], Err(vec![tag(path, b)]))),
        }
    }
    for (_, (media, members)) in groups {
        let subjects = members.iter().map(|i| i.subject.clone()).collect();
        out.push((subjects, replace_one(f, &media, &members, opts)));
    }
    out
}

/// Expected-episode coverage (architecture 7.2): every expected episode maps to exactly one
/// candidate through ARR's own assignment, and every candidate maps only to expected episodes.
fn coverage(
    a: &ArrFacts,
    cands: &[(&FsEntry, &ArrCandidateDecision)],
    include_specials: bool,
) -> Result<CoverageProof, Vec<Blocker>> {
    let cov = |m: String| Blocker::new(BlockerCode::CoverageIncomplete, m);
    if a.media.service == ArrService::Radarr {
        return match cands {
            [(e, _)] => Ok(CoverageProof {
                media: a.media.clone(),
                expected: [a.media.media_id].into(),
                mapping: [(a.media.media_id, e.path.clone())].into(),
                specials_included: false,
            }),
            _ => Err(vec![Blocker::new(
                BlockerCode::ArrAmbiguous,
                "a movie is replaced by exactly one file",
            )]),
        };
    }
    let mut assigned: BTreeMap<i64, Vec<&RootedPath>> = BTreeMap::new();
    for (e, d) in cands {
        for id in &d.episode_ids {
            assigned.entry(*id).or_default().push(&e.path);
        }
    }
    let mut b = Vec::new();
    let mut seasons = BTreeSet::new();
    for id in assigned.keys() {
        match a.episodes.iter().find(|e| e.id == *id) {
            Some(e) => {
                seasons.insert(e.season);
            }
            None => b.push(cov(format!("episode {id} is unknown to ARR"))),
        }
    }
    let expected: BTreeSet<i64> = a
        .episodes
        .iter()
        .filter(|e| seasons.contains(&e.season))
        .filter(|e| e.monitored && e.aired && (e.season != 0 || include_specials))
        .map(|e| e.id)
        .collect();
    for (id, paths) in &assigned {
        if !expected.contains(id) {
            b.push(cov(format!(
                "episode {id} is not expected (unmonitored, unaired or a special not included)"
            )));
        }
        if paths.len() > 1 {
            b.push(cov(format!(
                "episode {id} is covered by {} files",
                paths.len()
            )));
        }
    }
    let mut mapping = BTreeMap::new();
    for id in &expected {
        match assigned.get(id).map(Vec::as_slice) {
            Some([p]) => {
                mapping.insert(*id, (*p).clone());
            }
            _ => b.push(cov(format!(
                "expected episode {id} has no single candidate"
            ))),
        }
    }
    let proof = CoverageProof {
        media: ArrMediaRef {
            service: a.media.service,
            media_id: a.media.media_id,
            episode_ids: expected.iter().copied().collect(),
        },
        expected,
        mapping,
        specials_included: include_specials,
    };
    if b.is_empty() && proof.is_complete() {
        Ok(proof)
    } else {
        if b.is_empty() {
            b.push(cov("no expected episode".into()));
        }
        Err(b)
    }
}

/// Recycle bin usable as recovery: configured, mapped (or declared outside the roots), with a
/// cleanup period of 0 (never) or at least 7 days (CP1-F20).
fn recycle_ok(mm: Option<&ArrMediaManagement>) -> bool {
    mm.is_some_and(|m| {
        matches!(
            m.recycle_bin,
            Some(
                MappedPath::Mapped { .. }
                    | MappedPath::OutsideRoots { .. }
                    | MappedPath::DeclaredExternal { .. }
            )
        ) && matches!(m.recycle_bin_cleanup_days, Some(d) if d == 0 || d >= 7)
    })
}

fn file_name(p: &RootedPath) -> &str {
    p.rel.file_name().unwrap_or_default()
}

/// One `ReplaceRelease` for one media.
fn replace_one<'a>(
    f: &Facts<'a>,
    media: &ArrMediaRef,
    issues: &[&Issue],
    opts: ReplaceOptions,
) -> UnitResult<'a> {
    let a = facts_for(f, media).map_err(|b| vec![b])?;
    let kind = arr_kind(media.service);
    let mut b: Vec<Blocker> = Vec::new();
    let mut cands: Vec<(&FsEntry, &ArrCandidateDecision)> = Vec::new();
    let mut completeness = Completeness::Complete;
    for issue in issues {
        let Unit::File(p) = &issue.unit else { continue };
        completeness = completeness.and(issue_completeness(issue));
        let Some(e) = f.entry(p).filter(|e| e.sig.kind == EntryKind::File) else {
            b.push(tag(
                p,
                Blocker::new(BlockerCode::ScanIncomplete, "not in the inventory"),
            ));
            continue;
        };
        if is_sample(&p.rel) {
            b.push(tag(
                p,
                Blocker::new(BlockerCode::Sample, "samples are never imported"),
            ));
        }
        b.extend(f.scope_blockers(p));
        match decision(a, p) {
            None => b.push(tag(
                p,
                Blocker::new(
                    BlockerCode::ArrMissing,
                    "ARR's preview does not list the file",
                ),
            )),
            Some(d) if !d.approved || !d.rejections.is_empty() => b.push(tag(
                p,
                Blocker::new(
                    BlockerCode::ArrRejected,
                    "ARR does not accept the file as an import",
                ),
            )),
            Some(d) => cands.push((e, d)),
        }
    }
    if !b.is_empty() {
        return Err(b);
    }
    let proof = coverage(a, &cands, opts.include_specials)?;

    // Explanatory numbering (INV-36): a parsed number that disagrees with ARR's mapping needs a
    // manual check.
    if media.service == ArrService::Sonarr {
        for (e, d) in &cands {
            let parsed = parse_scene_name(file_name(&e.path));
            let ids: BTreeSet<i64> = d.episode_ids.iter().copied().collect();
            if explain_numbering(&parsed, &a.episodes, &ids).disagrees() {
                b.push(tag(
                    &e.path,
                    Blocker::new(
                        BlockerCode::NumberingMismatch,
                        "parsed numbering disagrees with ARR",
                    ),
                ));
            }
        }
    }

    // Old coverage: ARR's current files of the covered episodes (or the movie).
    let olds: Vec<&ArrFileRef> = match current_files_of_expected(a, &proof) {
        Ok(v) => v,
        Err(x) => {
            b.push(x);
            Vec::new()
        }
    };
    let mut old_entries: Vec<(&ArrFileRef, &FsEntry)> = Vec::new();
    for o in &olds {
        match present_file(f, o, &BTreeSet::new()) {
            Ok(e) => old_entries.push((o, e)),
            Err(x) => b.extend(x),
        }
    }
    if old_entries.is_empty() && b.is_empty() {
        b.push(Blocker::new(
            BlockerCode::ArrAmbiguous,
            "no current media file shows where ARR imports",
        ));
    }

    // Local checks per candidate against the old files of its episodes (INV-37).
    for (e, d) in &cands {
        let mine: Vec<&FsEntry> = match media.service {
            ArrService::Radarr => old_entries.iter().map(|(_, x)| *x).collect(),
            ArrService::Sonarr => old_entries
                .iter()
                .filter(|(o, _)| {
                    a.episodes
                        .iter()
                        .any(|ep| d.episode_ids.contains(&ep.id) && ep.file_id == Some(o.file_id))
                })
                .map(|(_, x)| *x)
                .collect(),
        };
        let cand_parsed = parse_scene_name(file_name(&e.path));
        let cand_probe = a.probes.get(&e.path).and_then(|r| r.as_ref().ok());
        for o in mine {
            let old_parsed = parse_scene_name(file_name(&o.path));
            let old_probe = a.probes.get(&o.path).and_then(|r| r.as_ref().ok());
            let assessment = assess_replacement(
                MediaFacts {
                    parsed: &old_parsed,
                    probe: old_probe,
                },
                MediaFacts {
                    parsed: &cand_parsed,
                    probe: cand_probe,
                },
                f.settings.duration_tolerance,
            );
            b.extend(assessment.blockers().into_iter().map(|x| tag(&e.path, x)));
            if o.sig.dev != e.sig.dev {
                b.push(tag(
                    &e.path,
                    Blocker::new(
                        BlockerCode::CrossDevice,
                        format!("{} is on another device", o.path),
                    ),
                ));
            }
        }
    }
    if !b.is_empty() {
        return Err(b);
    }

    // Recovery of every old file (CP1-F20): another retained link, else ARR's recycle bin, else
    // unrecoverable (per-file acknowledgement at approval, never automatic).
    let mm = a.snapshot.media_management.as_ref();
    let old_paths: BTreeSet<&RootedPath> = old_entries.iter().map(|(_, e)| &e.path).collect();
    let mut pre = vec![
        Precondition::ArrFingerprint {
            media: a.media.clone(),
            fingerprint: a.snapshot.fingerprint.clone(),
        },
        Precondition::ServiceAvailable(kind),
    ];
    let mut old_files = Vec::new();
    for (_, e) in &old_entries {
        let other = f
            .links_of(&e.sig.phys())
            .iter()
            .find(|x| x.path != e.path && !old_paths.contains(&x.path));
        let recovery = match other {
            Some(x) => {
                pre.push(Precondition::EntryUnchanged {
                    path: x.path.clone(),
                    sig: x.sig,
                    compare_nlink: false,
                });
                OldFileRecovery::OtherLink(x.path.clone())
            }
            None if recycle_ok(mm) => OldFileRecovery::RecycleBin,
            None => OldFileRecovery::Unrecoverable,
        };
        pre.push(Precondition::EntryUnchanged {
            path: e.path.clone(),
            sig: e.sig,
            compare_nlink: false,
        });
        old_files.push((e.path.clone(), e.sig.phys(), recovery));
    }
    let mut hashes = BTreeSet::new();
    let mut verify = Vec::new();
    let mut files = Vec::new();
    for (e, d) in &cands {
        hashes.extend(f.holders(&e.path));
        pre.push(Precondition::EntryUnchanged {
            path: e.path.clone(),
            sig: e.sig,
            compare_nlink: false,
        });
        pre.push(Precondition::NoScanErrorUnder {
            path: e.path.clone(),
        });
        if let Some((_, o)) = old_entries.first() {
            pre.push(Precondition::SameDevice {
                a: e.path.clone(),
                b: o.path.clone(),
            });
        }
        let mut ids = d.episode_ids.clone();
        ids.sort_unstable();
        let m = ArrMediaRef {
            service: media.service,
            media_id: media.media_id,
            episode_ids: ids,
        };
        verify.push(VerificationStep::ArrCurrentFile {
            media: m.clone(),
            path: e.path.clone(),
        });
        verify.push(VerificationStep::PathStillHasInode {
            path: e.path.clone(),
            phys: e.sig.phys(),
        });
        files.push((e.path.clone(), m));
    }
    files.sort();
    for (path, _, recovery) in &old_files {
        verify.push(VerificationStep::OldFileAccounted {
            path: path.clone(),
            recovery: recovery.clone(),
        });
    }
    pre.push(Precondition::OldCoverageRecoverable { old_files });
    pre.push(Precondition::NoArrImport {
        media: a.media.clone(),
        hashes: hashes.clone(),
    });
    let hardlinks = mm.is_some_and(|m| m.hardlinks_enabled);
    let mut d = Draft::new(
        ActionKind::ReplaceRelease,
        ActionTarget::Replace {
            request: ManualImportRequest { files },
        },
    );
    d.pre = pre;
    d.verify = verify;
    d.hashes = hashes;
    d.level = EvidenceLevel::None;
    d.completeness = completeness;
    d.logical = cands.iter().map(|(e, _)| e.sig.size).sum();
    // Without ARR hardlinks the import is a copy: its bytes are an added cost (ADR-0010).
    d.added = if hardlinks {
        0
    } else {
        cands.iter().map(|(e, _)| e.allocated).sum()
    };
    let mut unit = UnitPlan {
        drafts: vec![d],
        arr: vec![snapshot_ref(a)],
        aged: cands.iter().map(|(e, _)| *e).collect(),
        claims: cands.iter().map(|(e, _)| e.path.clone()).collect(),
        ..UnitPlan::default()
    };
    if opts.delete_old_media
        && let Some(first) = issues.first()
    {
        for (_, e) in &old_entries {
            let mut del =
                fsplans::delete_file(f, &e.path, ActionKind::DeleteMediaFile, false, first)?;
            del.deps = vec![0];
            unit.drafts.push(del);
        }
    }
    Ok(unit)
}

fn current_files_of_expected<'f>(
    a: &'f ArrFacts,
    proof: &CoverageProof,
) -> Result<Vec<&'f ArrFileRef>, Blocker> {
    match a.media.service {
        ArrService::Radarr => Ok(a.snapshot.current_files.iter().collect()),
        ArrService::Sonarr => {
            let mut out: Vec<&ArrFileRef> = Vec::new();
            for id in &proof.expected {
                let Some(file_id) = a
                    .episodes
                    .iter()
                    .find(|e| e.id == *id)
                    .and_then(|e| e.file_id)
                else {
                    continue; // An expected episode without a file has no old coverage.
                };
                let Some(file) = a
                    .snapshot
                    .current_files
                    .iter()
                    .find(|c| c.file_id == file_id)
                else {
                    return Err(Blocker::new(
                        BlockerCode::ArrAmbiguous,
                        format!("episode {id}: ARR file {file_id} is not in the snapshot"),
                    ));
                };
                if !out.iter().any(|o| o.file_id == file_id) {
                    out.push(file);
                }
            }
            Ok(out)
        }
    }
}

/// `ArrGrabRelease` from a fresh interactive search (CP1-F06). A search alone never grabs.
pub(crate) fn grab<'a>(
    f: &Facts<'a>,
    media: &ArrMediaRef,
    release: &ReleaseRef,
) -> (SubjectKey, UnitResult<'a>) {
    let subject = SubjectKey(format!(
        "r:{}:{}:{}",
        media.service.as_str(),
        media.media_id,
        release.guid
    ));
    let result = (|| {
        let a = facts_for(f, media).map_err(|b| vec![b])?;
        let found = match &f.input.search {
            None => None,
            Some(Err(e)) => {
                return Err(vec![Blocker::new(
                    BlockerCode::ServiceUnavailable(kind_of(media)),
                    format!("search failed ({:?})", e.class),
                )]);
            }
            Some(Ok(list)) => list.iter().find(|r| &r.release == release),
        };
        let Some(r) = found else {
            return Err(vec![Blocker::new(
                BlockerCode::ReleaseNotInSearch,
                "the release is not in a fresh search result",
            )]);
        };
        if !r.approved || !r.rejections.is_empty() || !r.download_allowed {
            return Err(vec![Blocker::new(
                BlockerCode::ArrRejected,
                "ARR rejects this release",
            )]);
        }
        let mut d = Draft::new(
            ActionKind::ArrGrabRelease,
            ActionTarget::Release {
                media: media.clone(),
                release: release.clone(),
            },
        );
        d.pre = vec![
            Precondition::ServiceAvailable(kind_of(media)),
            Precondition::ArrFingerprint {
                media: a.media.clone(),
                fingerprint: a.snapshot.fingerprint.clone(),
            },
        ];
        d.verify = vec![VerificationStep::ArrQueueHasRelease {
            service: media.service,
            release: release.clone(),
        }];
        Ok(UnitPlan {
            drafts: vec![d],
            arr: vec![snapshot_ref(a)],
            ..UnitPlan::default()
        })
    })();
    (subject, result)
}
