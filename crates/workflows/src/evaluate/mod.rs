//! Evaluate (architecture 7.2, agent 12): read-only observation on request. ARR quality
//! snapshots, ARR's own import preview, expected episodes and ffprobe reports for selected
//! issues or one media; and VerifyContent, the full byte comparison of stored candidate pairs.
//! Nothing here mutates a root or a service: results become evidence and proofs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use removarr_domain::arr::{
    ArrCandidateDecision, ArrEpisode, ArrMediaRef, ArrQualitySnapshot, ArrService,
    ReleaseSearchResult,
};
use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, IdentityProof, Provenance, QualityEvidence,
};
use removarr_domain::ids::IssueId;
use removarr_domain::issue::{IssueCategory, Unit};
use removarr_domain::media::ProbeReport;
use removarr_domain::path::RootedPath;
use removarr_domain::ports::{
    ArrRead, Clock, ContentReader, FsInventory, IssueStore, MatchStore, MediaProbe, ScanControl,
};
use removarr_matching::{ContentVerdict, ContentVerifier, ReadOnlyGate, VerifyRequest};
use serde::{Deserialize, Serialize};

use crate::issues::{ArrObservation, NOTE_PREFIX};
use crate::plans::{Planner, arr_needs};

/// Fresh ARR reads for one media (and the ffprobe reports of the files involved).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrFacts {
    /// The whole media (`episode_ids` empty): the snapshot covers every current file.
    pub media: ArrMediaRef,
    /// Sealed snapshot; `candidates` holds the preview decisions of `previews`.
    pub snapshot: ArrQualitySnapshot,
    /// Sonarr: every expected episode (with or without a file, specials included).
    pub episodes: Vec<ArrEpisode>,
    /// ARR's manual-import preview per completed folder (GET only, never imports).
    pub previews: BTreeMap<RootedPath, Vec<ArrCandidateDecision>>,
    pub probes: BTreeMap<RootedPath, Result<ProbeReport, PortError>>,
}

/// What to read for one media.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatherRequest {
    pub folders: BTreeSet<RootedPath>,
    /// Completed candidate files to probe; ARR's current files of their episodes are probed
    /// too (replacement checks).
    pub probe: BTreeSet<RootedPath>,
}

fn whole(media: &ArrMediaRef) -> ArrMediaRef {
    ArrMediaRef {
        service: media.service,
        media_id: media.media_id,
        episode_ids: vec![],
    }
}

/// Reads ARR (and ffprobe) for one media. Any ARR read failure fails the whole gather (fail
/// closed: no partial evidence is ever used as a decision); probe failures are kept per file
/// and block the replacement checks that need them.
pub async fn gather(
    arr: &dyn ArrRead,
    probe: Option<&dyn MediaProbe>,
    media: &ArrMediaRef,
    req: &GatherRequest,
) -> Result<ArrFacts, PortError> {
    let media = whole(media);
    let mut snapshot = arr.quality_snapshot(&media).await?;
    let episodes = match media.service {
        ArrService::Sonarr => arr.episodes(media.media_id, None).await?,
        ArrService::Radarr => Vec::new(),
    };
    let mut previews = BTreeMap::new();
    for folder in &req.folders {
        previews.insert(
            folder.clone(),
            arr.manual_import_preview(folder, Some(&media)).await?,
        );
    }
    snapshot.candidates = previews.values().flatten().cloned().collect();
    let mut targets: BTreeSet<RootedPath> = req.probe.clone();
    for c in &req.probe {
        let subject = c.to_string();
        let Some(d) = previews.values().flatten().find(|d| d.subject == subject) else {
            continue;
        };
        let file_ids: BTreeSet<i64> = match media.service {
            ArrService::Radarr => snapshot.current_files.iter().map(|f| f.file_id).collect(),
            ArrService::Sonarr => episodes
                .iter()
                .filter(|e| d.episode_ids.contains(&e.id))
                .filter_map(|e| e.file_id)
                .collect(),
        };
        targets.extend(
            snapshot
                .current_files
                .iter()
                .filter(|f| file_ids.contains(&f.file_id))
                .filter_map(|f| f.path.as_rooted().cloned()),
        );
    }
    let mut probes = BTreeMap::new();
    for t in targets {
        let r = match probe {
            Some(p) => p.probe(&t).await,
            None => Err(PortError::new(
                ErrorClass::Config,
                "ffprobe is not configured",
            )),
        };
        probes.insert(t, r);
    }
    Ok(ArrFacts {
        media,
        snapshot,
        episodes,
        previews,
        probes,
    })
}

/// Result of a media-level evaluation (media detail page, release search; CP1-F06).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEvaluation {
    pub snapshot: ArrQualitySnapshot,
    pub episodes: Vec<ArrEpisode>,
    /// Sorted for display (`sort_search_results`); never grabs.
    pub search: Option<Vec<ReleaseSearchResult>>,
}

/// `Evaluate { media, search }`: snapshot, expected episodes, optional interactive search.
pub async fn evaluate_media(
    arr: &dyn ArrRead,
    media: &ArrMediaRef,
    search: bool,
) -> Result<MediaEvaluation, PortError> {
    let facts = gather(arr, None, media, &GatherRequest::default()).await?;
    let search = if search {
        let mut r = arr.search_releases(media).await?;
        let current: u64 = facts.snapshot.current_files.iter().map(|f| f.size).sum();
        removarr_domain::arr::sort_search_results(&mut r, current);
        Some(r)
    } else {
        None
    };
    Ok(MediaEvaluation {
        snapshot: facts.snapshot,
        episodes: facts.episodes,
        search,
    })
}

/// Outcome of an issue evaluation job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateReport {
    /// Issues that received ARR evidence.
    pub evaluated: Vec<IssueId>,
    /// Issues left without ARR evidence, with a short reason code.
    pub skipped: Vec<(IssueId, String)>,
}

fn note(text: String, provenance: Provenance, at: removarr_domain::time::Timestamp) -> Evidence {
    Evidence {
        kind: EvidenceKind::Identity {
            subject: format!("{NOTE_PREFIX}{text}"),
            proof: IdentityProof::None,
        },
        provenance,
        observed_at: at,
        completeness: Completeness::Complete,
    }
}

/// `Evaluate { issue_ids }`: identifies each selected completed file through ARR, reads ARR's
/// verdicts and the ffprobe reports, persists the snapshots and attaches the results as
/// evidence. Only `hardlink_missing`, `orphan_completed` and `duplicate` issues are evaluated.
pub async fn evaluate_issues(
    planner: &Planner,
    issue_ids: &[IssueId],
    arr_obs: Vec<ArrObservation>,
) -> Result<EvaluateReport, PortError> {
    let input = planner
        .load(issue_ids, arr_obs)
        .await
        .map_err(|e| PortError::new(e.class(), e.to_string()))?;
    let needs = arr_needs(&input, planner.settings(), true);
    let facts = planner.gather_all(&needs).await;
    let ports = planner.ports();
    let mut report = EvaluateReport::default();
    for issue in &input.issues {
        if !matches!(
            issue.category,
            IssueCategory::HardlinkMissing
                | IssueCategory::OrphanCompleted
                | IssueCategory::Duplicate
        ) {
            report.skipped.push((issue.id.clone(), "category".into()));
            continue;
        }
        let Some((key, files)) = needs.by_issue.get(&issue.id) else {
            report
                .skipped
                .push((issue.id.clone(), "arr_identity_unknown".into()));
            continue;
        };
        let Some(Ok(a)) = facts.get(key) else {
            report
                .skipped
                .push((issue.id.clone(), "arr_unavailable".into()));
            continue;
        };
        let prov = Provenance::Arr {
            service: a.media.service,
            captured_at: a.snapshot.captured_at,
        };
        let at = a.snapshot.captured_at;
        let subject = format!("arr:{}:{}", a.media.service.as_str(), a.media.media_id);
        let mut ev = vec![
            Evidence {
                kind: EvidenceKind::Identity {
                    subject: subject.clone(),
                    proof: IdentityProof::ArrMatched,
                },
                provenance: prov.clone(),
                observed_at: at,
                completeness: Completeness::Complete,
            },
            Evidence {
                kind: EvidenceKind::Quality {
                    subject,
                    proof: QualityEvidence::ArrSnapshot {
                        fingerprint: a.snapshot.fingerprint.clone(),
                    },
                },
                provenance: prov.clone(),
                observed_at: at,
                completeness: Completeness::Complete,
            },
        ];
        for path in files {
            let verdict = match crate::plans::preview_decision(a, path) {
                None => "missing",
                Some(d) if d.approved && d.rejections.is_empty() => "approved",
                Some(d)
                    if d.rejections
                        .iter()
                        .all(|r| crate::plans::is_not_upgrade_reason(r)) =>
                {
                    "rejected:not_upgrade"
                }
                Some(_) => "rejected:other",
            };
            ev.push(note(
                format!("arr_preview:{verdict}:{path}"),
                prov.clone(),
                at,
            ));
            if let Some(r) = a.probes.get(path) {
                let text = match r {
                    Ok(_) => format!("probe:ok:{path}"),
                    Err(e) => format!("probe:failed:{:?}:{path}", e.class),
                };
                ev.push(note(text, Provenance::Ffprobe { at }, at));
            }
        }
        ports.snapshots.save_arr(&a.snapshot).await?;
        ports.issues.attach_evidence(&issue.id, &ev).await?;
        report.evaluated.push(issue.id.clone());
    }
    Ok(report)
}

/// Ports of a VerifyContent job.
#[derive(Clone)]
pub struct VerifyPorts {
    pub issues: Arc<dyn IssueStore>,
    pub matches: Arc<dyn MatchStore>,
    pub fs: Arc<dyn FsInventory>,
    pub content: Arc<dyn ContentReader>,
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for VerifyPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyPorts").finish_non_exhaustive()
    }
}

/// Outcome counts of a VerifyContent job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub identical: u32,
    pub differs: u32,
    /// Pairs that changed, vanished or could not be read: no proof (re-observe).
    pub inconclusive: u32,
}

/// `VerifyContent { issue_ids }` (CP1-F08): full comparison of the stored candidate pairs of
/// the selected `duplicate`/`hardlink_missing` issues, with the read-only gate (execution
/// compares again behind the live hardlink probe). Results are stored as proofs and evidence.
pub async fn verify_content(
    ports: &VerifyPorts,
    verifier: &ContentVerifier,
    issue_ids: &[IssueId],
    ctl: &ScanControl,
) -> Result<VerifyReport, PortError> {
    let issues = ports.issues.get_many(issue_ids).await?;
    let candidates = ports.matches.current().await?;
    let mut report = VerifyReport::default();
    for issue in &issues {
        let pairs: Vec<_> = candidates
            .iter()
            .filter(|m| match (&issue.category, &issue.unit) {
                (IssueCategory::Duplicate, Unit::Pair { a, b }) => &m.a == a && &m.b == b,
                (IssueCategory::HardlinkMissing, Unit::File(p)) => &m.a == p,
                _ => false,
            })
            .cloned()
            .collect();
        let mut ev = Vec::new();
        for m in pairs {
            let (fs, content, clock, v, c, ctl2) = (
                ports.fs.clone(),
                ports.content.clone(),
                ports.clock.clone(),
                verifier.clone(),
                m.clone(),
                ctl.clone(),
            );
            let verdict = tokio::task::spawn_blocking(move || {
                v.verify(
                    &*fs,
                    &*content,
                    &ReadOnlyGate,
                    VerifyRequest::from_candidate(&c),
                    &ctl2,
                    &*clock,
                )
            })
            .await
            .map_err(|e| PortError::new(ErrorClass::Internal, format!("verify task: {e}")))??;
            let proof = verdict.to_content_proof();
            match (&verdict, &proof) {
                (_, ContentProof::Identical { .. }) => report.identical += 1,
                (_, ContentProof::Differs { .. }) => report.differs += 1,
                _ => report.inconclusive += 1,
            }
            ports.matches.update_proof(&m.a, &m.b, &proof).await?;
            let now = ports.clock.now();
            let conclusive = !matches!(proof, ContentProof::None);
            ev.push(Evidence {
                kind: EvidenceKind::Content {
                    a: m.a.clone(),
                    b: m.b.clone(),
                    proof,
                },
                provenance: Provenance::Filesystem { session: m.session },
                observed_at: now,
                completeness: if conclusive {
                    Completeness::Complete
                } else {
                    Completeness::Incomplete {
                        missing: vec!["files changed or could not be compared".into()],
                    }
                },
            });
            if let ContentVerdict::Rejected(r) = &verdict {
                ev.push(note(
                    format!("verify:{}", r.code()),
                    Provenance::Computed,
                    now,
                ));
            }
        }
        if !ev.is_empty() {
            ports.issues.attach_evidence(&issue.id, &ev).await?;
        }
    }
    Ok(report)
}
