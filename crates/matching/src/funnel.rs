//! The staged matching funnel run during a scan (architecture 7.1 step 6, CONTEXTE §3.3).
//!
//! 1. Physical identity: same `(dev, ino)` in a source and a target root is a real hardlink.
//!    Such sources leave the funnel (nothing to prove).
//! 2. Normalized name + size index: a strong but unproven candidate. No content is read (T-10).
//! 3. Scene/P2P parsing and a bounded semantic ranking (top `semantic_limit`, search depth
//!    `repair_search_limit`) for sources without a name+size match.
//! 4. Distributed samples on equal-size semantic candidates, only to reject, stopping at the first
//!    candidate that is not rejected (legacy `discover_hardlink_repairs`).
//!
//! There is no full comparison here (architecture 8.5): the strongest proof the funnel creates is
//! `ContentProof::Sampled`. A full-compare proof (`Identical`/`Differs`) from an earlier
//! `VerifyContent` is carried over while both signatures are unchanged. The output is persisted
//! with `MatchStore::replace_for_session`, so a later classification never re-reads content.

use std::collections::{BTreeMap, BTreeSet};

use removarr_domain::error::{ErrorClass, PortError};
use removarr_domain::evidence::{
    Completeness, ContentProof, Evidence, EvidenceKind, IdentityProof, MatchCandidate, Provenance,
};
use removarr_domain::fs::{
    EntryKind, EntrySignature, FsEntry, PhysicalId, RootInventory, SampleSpec,
};
use removarr_domain::ids::SessionId;
use removarr_domain::path::RootedPath;
use removarr_domain::policy::is_sample;
use removarr_domain::ports::{ContentReader, ScanControl};
use removarr_domain::time::Timestamp;
use serde::{Deserialize, Serialize};

use crate::normalize::{extension, name_key};
use crate::parse::{ParsedName, parse_scene_name};
use crate::proof::{ContentVerifier, SampleVerdict, VerifyRequest};
use crate::reject::Rejection;
use crate::similarity::{Record, ScoreReason, SimilarityEngine, similarity};

/// Crate-local settings, filled by `app` from `[matching]`, `[files]` and `[limits]`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSettings {
    /// Lower-case, dot-prefixed (`".mkv"`).
    pub video_extensions: BTreeSet<String>,
    /// Identification threshold (default 0.60). Never a quality threshold.
    pub similarity_threshold: f64,
    /// Semantic candidates kept per source (default 3, CONTEXTE §3.3).
    pub semantic_limit: usize,
    /// Ranking depth searched for an equal-size candidate to sample (default 10, DR-10/DR-11).
    pub repair_search_limit: usize,
    /// Name+size matches kept per source (bounds pathological groups).
    pub max_name_size_matches: usize,
    pub sample: SampleSpec,
}

impl Default for MatchSettings {
    fn default() -> Self {
        Self {
            video_extensions: [
                ".mkv", ".mp4", ".avi", ".m4v", ".mov", ".ts", ".m2ts", ".wmv", ".webm",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            similarity_threshold: 0.60,
            semantic_limit: 3,
            repair_search_limit: 10,
            max_name_size_matches: 10,
            sample: SampleSpec {
                size: 1024 * 1024,
                count: 5,
            },
        }
    }
}

impl MatchSettings {
    #[must_use]
    pub fn is_video(&self, file_name: &str) -> bool {
        extension(file_name).is_some_and(|e| self.video_extensions.contains(&e))
    }
}

/// The funnel stage that produced a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    NameSize,
    Semantic,
    /// A full-compare proof from an earlier verification, still valid for unchanged files.
    CarriedOver,
}

/// Explainable reason attached to a pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchReason {
    SameNameAndSize,
    Score(ScoreReason),
    /// Samples did not reject the pair. A proposal only, never equality.
    SamplesNotRejected,
    /// The two files are on different devices: a hardlink is impossible (INV-43).
    CrossDevice,
    /// The content proof comes from an earlier run with unchanged signatures.
    CarriedOver,
}

impl MatchReason {
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            MatchReason::SameNameAndSize => "same_name_and_size".into(),
            MatchReason::Score(r) => r.code(),
            MatchReason::SamplesNotRejected => "samples_not_rejected".into(),
            MatchReason::CrossDevice => "cross_device".into(),
            MatchReason::CarriedOver => "carried_over".into(),
        }
    }
}

/// One retained pair with typed evidence. `a` is the source (completed side), `b` the target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunnelPair {
    pub a: RootedPath,
    pub b: RootedPath,
    pub sig_a: EntrySignature,
    pub sig_b: EntrySignature,
    pub stage: Stage,
    pub score_milli: u32,
    pub identity: IdentityProof,
    pub content: ContentProof,
    pub reasons: Vec<MatchReason>,
    pub rejection: Option<Rejection>,
}

impl FunnelPair {
    /// The persisted form. `reasons` holds the reason codes followed by the rejection code.
    #[must_use]
    pub fn to_match_candidate(&self, session: SessionId) -> MatchCandidate {
        let mut reasons: Vec<String> = self.reasons.iter().map(MatchReason::code).collect();
        if let Some(r) = &self.rejection {
            reasons.push(r.code());
        }
        MatchCandidate {
            a: self.a.clone(),
            b: self.b.clone(),
            sig_a: self.sig_a,
            sig_b: self.sig_b,
            identity: self.identity.clone(),
            content: self.content.clone(),
            reasons,
            session,
        }
    }

    /// Content evidence for the issue (provenance: this scan session).
    #[must_use]
    pub fn content_evidence(&self, session: SessionId, observed_at: Timestamp) -> Evidence {
        Evidence {
            kind: EvidenceKind::Content {
                a: self.a.clone(),
                b: self.b.clone(),
                proof: self.content.clone(),
            },
            provenance: Provenance::Filesystem { session },
            observed_at,
            completeness: Completeness::Complete,
        }
    }

    /// Identity evidence (name parsing only; ARR identity is a separate evidence).
    #[must_use]
    pub fn identity_evidence(&self, observed_at: Timestamp) -> Evidence {
        Evidence {
            kind: EvidenceKind::Identity {
                subject: self.a.to_string(),
                proof: self.identity.clone(),
            },
            provenance: Provenance::Computed,
            observed_at,
            completeness: Completeness::Complete,
        }
    }
}

/// Paths of one inode present in both a source and a target root (a real hardlink).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedPhysical {
    pub phys: PhysicalId,
    pub a_paths: Vec<RootedPath>,
    pub b_paths: Vec<RootedPath>,
}

impl SharedPhysical {
    #[must_use]
    pub fn evidence(&self, session: SessionId, observed_at: Timestamp) -> Evidence {
        Evidence {
            kind: EvidenceKind::SamePhysical {
                a: self.phys,
                b: self.phys,
            },
            provenance: Provenance::Filesystem { session },
            observed_at,
            completeness: Completeness::Complete,
        }
    }
}

/// Counters for logs and the job progress (memory only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunnelStats {
    pub source_videos: u64,
    pub already_linked: u64,
    pub name_size_pairs: u64,
    pub semantic_pairs: u64,
    pub sample_reads: u64,
    pub sample_rejections: u64,
    pub carried_over: u64,
    pub samples_skipped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunnelOutput {
    pub pairs: Vec<FunnelPair>,
    pub shared: Vec<SharedPhysical>,
    pub stats: FunnelStats,
}

impl FunnelOutput {
    /// Rows for `MatchStore::replace_for_session`.
    #[must_use]
    pub fn match_candidates(&self, session: SessionId) -> Vec<MatchCandidate> {
        self.pairs
            .iter()
            .map(|p| p.to_match_candidate(session))
            .collect()
    }
}

/// Funnel input. `sources` are the completed roots, `targets` the library roots. Passing the same
/// inventories on both sides finds duplicates inside them (each unordered pair once).
#[derive(Debug, Clone, Copy)]
pub struct FunnelInput<'a> {
    pub sources: &'a [RootInventory],
    pub targets: &'a [RootInventory],
    /// The stored funnel output of the previous run (`MatchStore::current`).
    pub previous: &'a [MatchCandidate],
}

fn regular_files(invs: &[RootInventory]) -> impl Iterator<Item = &FsEntry> {
    invs.iter()
        .flat_map(|i| i.entries.iter())
        .filter(|e| e.sig.kind == EntryKind::File)
}

fn file_name(e: &FsEntry) -> &str {
    e.path.rel.file_name().unwrap_or("")
}

fn unordered(a: &RootedPath, b: &RootedPath) -> (RootedPath, RootedPath) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

/// A previous proof reusable for `(a, b)` with the current signatures, if any. Only proofs
/// stronger than name+size, or a sample rejection, are carried.
fn carried(
    prev: &BTreeMap<(RootedPath, RootedPath), &MatchCandidate>,
    a: &FsEntry,
    b: &FsEntry,
) -> Option<(ContentProof, Option<Rejection>)> {
    let c = prev.get(&(a.path.clone(), b.path.clone()))?;
    if !c.proof_still_valid(&a.sig, &b.sig) {
        return None;
    }
    let samples_differ = Rejection::SamplesDiffer.code();
    match &c.content {
        ContentProof::Sampled | ContentProof::Identical { .. } => Some((c.content.clone(), None)),
        ContentProof::Differs { offset } => Some((
            c.content.clone(),
            Some(Rejection::ContentDiffers { offset: *offset }),
        )),
        ContentProof::None if c.reasons.contains(&samples_differ) => {
            Some((ContentProof::None, Some(Rejection::SamplesDiffer)))
        }
        _ => None,
    }
}

fn identity_for(score_milli: u32) -> IdentityProof {
    if score_milli > 0 {
        IdentityProof::NameParse { score_milli }
    } else {
        IdentityProof::None
    }
}

/// Runs stages 1 to 4. Blocking (sample reads): call from `spawn_blocking`. `Err` on an
/// unfinished inventory (fail-closed) or cancellation; no partial output is returned.
pub fn run_funnel(
    input: FunnelInput<'_>,
    settings: &MatchSettings,
    verifier: &ContentVerifier,
    reader: &dyn ContentReader,
    ctl: &ScanControl,
) -> Result<FunnelOutput, PortError> {
    if input
        .sources
        .iter()
        .chain(input.targets)
        .any(|i| !i.finished)
    {
        return Err(PortError::new(
            ErrorClass::Validation,
            "matching needs finished inventories",
        ));
    }
    let mut out = FunnelOutput::default();

    // Stage 1: physical identity.
    let mut target_phys: BTreeMap<PhysicalId, Vec<&RootedPath>> = BTreeMap::new();
    for e in regular_files(input.targets) {
        target_phys.entry(e.sig.phys()).or_default().push(&e.path);
    }
    let mut source_phys: BTreeMap<PhysicalId, Vec<&RootedPath>> = BTreeMap::new();
    for e in regular_files(input.sources) {
        source_phys.entry(e.sig.phys()).or_default().push(&e.path);
    }
    let linked = |e: &FsEntry| {
        target_phys
            .get(&e.sig.phys())
            .is_some_and(|ps| ps.iter().any(|p| **p != e.path))
    };
    for (phys, a_paths) in &source_phys {
        if let Some(b_paths) = target_phys.get(phys)
            && a_paths.iter().any(|a| b_paths.iter().any(|b| a != b))
        {
            out.shared.push(SharedPhysical {
                phys: *phys,
                a_paths: a_paths.iter().map(|p| (*p).clone()).collect(),
                b_paths: b_paths.iter().map(|p| (*p).clone()).collect(),
            });
        }
    }

    let is_video = |e: &&FsEntry| settings.is_video(file_name(e));
    let mut sources: Vec<&FsEntry> = regular_files(input.sources).filter(is_video).collect();
    sources.sort_by(|x, y| x.path.cmp(&y.path));
    let targets: Vec<&FsEntry> = regular_files(input.targets).filter(is_video).collect();

    // Stage 2: normalized name + size index.
    let mut by_name_size: BTreeMap<(String, u64), Vec<&FsEntry>> = BTreeMap::new();
    for t in &targets {
        by_name_size
            .entry((name_key(file_name(t)), t.sig.size))
            .or_default()
            .push(t);
    }
    for list in by_name_size.values_mut() {
        list.sort_by(|x, y| x.path.cmp(&y.path));
    }

    // Stage 3 index: semantic records (samples never compete as targets).
    let semantic_targets: Vec<&FsEntry> = targets
        .iter()
        .copied()
        .filter(|t| !is_sample(&t.path.rel))
        .collect();
    let engine = SimilarityEngine::new(
        semantic_targets
            .iter()
            .map(|t| Record {
                key: t.path.to_string(),
                size: t.sig.size,
                parsed: parse_scene_name(file_name(t)),
            })
            .collect(),
    );

    let prev: BTreeMap<(RootedPath, RootedPath), &MatchCandidate> = input
        .previous
        .iter()
        .map(|c| ((c.a.clone(), c.b.clone()), c))
        .collect();
    let mut seen: BTreeSet<(RootedPath, RootedPath)> = BTreeSet::new();

    for s in &sources {
        if ctl.cancel.is_cancelled() {
            return Err(PortError::new(ErrorClass::Cancelled, "matching cancelled"));
        }
        out.stats.source_videos += 1;
        if linked(s) {
            out.stats.already_linked += 1;
            continue;
        }
        let s_parsed = parse_scene_name(file_name(s));
        let distinct = |t: &FsEntry| t.path != s.path && t.sig.phys() != s.sig.phys();

        let exact: Vec<&FsEntry> = by_name_size
            .get(&(name_key(file_name(s)), s.sig.size))
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|t| distinct(t))
                    .take(settings.max_name_size_matches)
                    .collect()
            })
            .unwrap_or_default();

        if !exact.is_empty() {
            for t in exact {
                if !seen.insert(unordered(&s.path, &t.path)) {
                    continue;
                }
                let sim = similarity(&s_parsed, s.sig.size, &engine_parse(t), t.sig.size);
                let mut reasons = vec![MatchReason::SameNameAndSize];
                reasons.extend(sim.reasons.iter().cloned().map(MatchReason::Score));
                let (content, rejection) = match carried(&prev, s, t) {
                    Some(c) => {
                        reasons.push(MatchReason::CarriedOver);
                        out.stats.carried_over += 1;
                        c
                    }
                    None => (ContentProof::NameSize, None),
                };
                if s.sig.dev != t.sig.dev {
                    reasons.push(MatchReason::CrossDevice);
                }
                out.stats.name_size_pairs += 1;
                out.pairs.push(FunnelPair {
                    a: s.path.clone(),
                    b: t.path.clone(),
                    sig_a: s.sig,
                    sig_b: t.sig,
                    stage: Stage::NameSize,
                    score_milli: sim.score_milli(),
                    identity: identity_for(sim.score_milli()),
                    content,
                    reasons,
                    rejection,
                });
            }
            continue;
        }

        if is_sample(&s.path.rel) {
            out.stats.samples_skipped += 1;
            continue;
        }
        let ranked = engine.find(
            &s_parsed,
            s.sig.size,
            settings.similarity_threshold,
            settings.repair_search_limit,
        );
        let mut proposal_found = false;
        let mut kept = 0usize;
        for r in ranked {
            let t = semantic_targets[r.index];
            if !distinct(t) {
                continue;
            }
            let mut keep = kept < settings.semantic_limit;
            let score_milli = r.similarity.score_milli();
            let mut reasons: Vec<MatchReason> = r
                .similarity
                .reasons
                .iter()
                .cloned()
                .map(MatchReason::Score)
                .collect();
            let (content, rejection) = if t.sig.size != s.sig.size {
                (
                    ContentProof::None,
                    Some(Rejection::SizeDiffers {
                        a: s.sig.size,
                        b: t.sig.size,
                    }),
                )
            } else if proposal_found {
                (ContentProof::Semantic { score_milli }, None)
            } else {
                let result = match carried(&prev, s, t) {
                    Some(c) => {
                        reasons.push(MatchReason::CarriedOver);
                        out.stats.carried_over += 1;
                        c
                    }
                    None => {
                        out.stats.sample_reads += 1;
                        let req = VerifyRequest {
                            a: &s.path,
                            b: &t.path,
                            sig_a: &s.sig,
                            sig_b: &t.sig,
                        };
                        match verifier.sample_check(reader, req, settings.sample, ctl)? {
                            SampleVerdict::NotRejected => {
                                reasons.push(MatchReason::SamplesNotRejected);
                                (ContentProof::Sampled, None)
                            }
                            SampleVerdict::Rejected(rej) => {
                                out.stats.sample_rejections += 1;
                                (ContentProof::None, Some(rej))
                            }
                        }
                    }
                };
                if result.1.is_none() {
                    proposal_found = true;
                    keep = true;
                }
                result
            };
            if !keep || !seen.insert(unordered(&s.path, &t.path)) {
                continue;
            }
            if s.sig.dev != t.sig.dev {
                reasons.push(MatchReason::CrossDevice);
            }
            kept += 1;
            out.stats.semantic_pairs += 1;
            out.pairs.push(FunnelPair {
                a: s.path.clone(),
                b: t.path.clone(),
                sig_a: s.sig,
                sig_b: t.sig,
                stage: Stage::Semantic,
                score_milli,
                identity: identity_for(score_milli),
                content,
                reasons,
                rejection,
            });
        }
    }

    // Full-compare proofs whose pair was not rebuilt this time but whose files are unchanged.
    let source_by_path: BTreeMap<&RootedPath, &FsEntry> =
        sources.iter().map(|e| (&e.path, *e)).collect();
    let target_by_path: BTreeMap<&RootedPath, &FsEntry> =
        targets.iter().map(|e| (&e.path, *e)).collect();
    for c in input.previous {
        if !matches!(
            c.content,
            ContentProof::Identical { .. } | ContentProof::Differs { .. }
        ) {
            continue;
        }
        let (Some(s), Some(t)) = (source_by_path.get(&c.a), target_by_path.get(&c.b)) else {
            continue;
        };
        if linked(s) || s.sig.phys() == t.sig.phys() || !c.proof_still_valid(&s.sig, &t.sig) {
            continue;
        }
        if !seen.insert(unordered(&c.a, &c.b)) {
            continue;
        }
        let rejection = match c.content {
            ContentProof::Differs { offset } => Some(Rejection::ContentDiffers { offset }),
            _ => None,
        };
        out.stats.carried_over += 1;
        out.pairs.push(FunnelPair {
            a: c.a.clone(),
            b: c.b.clone(),
            sig_a: s.sig,
            sig_b: t.sig,
            stage: Stage::CarriedOver,
            score_milli: match c.identity {
                IdentityProof::NameParse { score_milli } => score_milli,
                _ => 0,
            },
            identity: c.identity.clone(),
            content: c.content.clone(),
            reasons: vec![MatchReason::CarriedOver],
            rejection,
        });
    }

    out.pairs.sort_by(|x, y| (&x.a, &x.b).cmp(&(&y.a, &y.b)));
    Ok(out)
}

fn engine_parse(t: &FsEntry) -> ParsedName {
    parse_scene_name(file_name(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limiter::ReadLimiter;
    use crate::proof::fakes::{FakeFs, sig};
    use removarr_domain::path::{RelPath, RootId};
    use removarr_domain::time::MtimeNs;

    const MEDIA: &str = "media-0123abcd";

    fn entry(root: &str, rel: &str, ino: u64, size: u64) -> FsEntry {
        FsEntry {
            path: RootedPath::new(RootId::new(root).unwrap(), RelPath::new(rel).unwrap()),
            sig: sig(ino, size),
            allocated: size,
            symlink_target: None,
        }
    }

    fn inv(root: &str, entries: Vec<FsEntry>) -> RootInventory {
        RootInventory {
            root: RootId::new(root).unwrap(),
            root_sig: None,
            entries,
            errors: vec![],
            protected: vec![],
            finished: true,
        }
    }

    fn run(
        src: &[RootInventory],
        tgt: &[RootInventory],
        prev: &[MatchCandidate],
        fs: &FakeFs,
    ) -> FunnelOutput {
        let v = ContentVerifier::new(ReadLimiter::new(1), 65536);
        run_funnel(
            FunnelInput {
                sources: src,
                targets: tgt,
                previous: prev,
            },
            &MatchSettings::default(),
            &v,
            fs,
            &ScanControl::default(),
        )
        .unwrap()
    }

    /// T-10 `test_identical_copy_is_proposed_for_hardlink_repair`: exact name+size proposes a
    /// candidate with zero content reads.
    #[test]
    fn t10_name_and_size_proposes_without_reading_content() {
        let fs = FakeFs::default();
        let src = [inv(
            "completed",
            vec![entry("completed", "Movie.Name.2024.1080p.mkv", 1, 18432)],
        )];
        let tgt = [inv(
            MEDIA,
            vec![entry(MEDIA, "films/Movie.Name.2024.1080p.mkv", 2, 18432)],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert_eq!(fs.reads(), 0, "an exact name must not read any sample");
        assert_eq!(out.pairs.len(), 1);
        let p = &out.pairs[0];
        assert_eq!(p.stage, Stage::NameSize);
        assert_eq!(p.content, ContentProof::NameSize);
        assert!(p.reasons.contains(&MatchReason::SameNameAndSize));
        assert!(!p.content.proves_equal());
        let mc = p.to_match_candidate(SessionId::new(removarr_domain::ids::RawUuid::nil()));
        assert_eq!(mc.reasons[0], "same_name_and_size");
    }

    /// T-11 `test_same_name_and_size_is_only_a_potential_duplicate` (funnel side).
    #[test]
    fn t11_same_name_and_size_is_never_more_than_a_candidate() {
        let fs = FakeFs::default();
        let src = [inv(
            "completed",
            vec![entry("completed", "Movie.Name.2024.mkv", 1, 4)],
        )];
        let tgt = [inv(
            MEDIA,
            vec![entry(MEDIA, "films/MOVIE.NAME.2024.MKV", 2, 4)],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert_eq!(out.pairs.len(), 1, "case-folded name matches");
        assert_eq!(
            out.pairs[0].content.level(),
            removarr_domain::evidence::EvidenceLevel::NameSize
        );
    }

    #[test]
    fn shared_inode_is_physical_identity_and_leaves_the_funnel() {
        let fs = FakeFs::default();
        let mut linked = entry(MEDIA, "films/A.2020.mkv", 1, 100);
        linked.sig.nlink = 2;
        let src = [inv(
            "completed",
            vec![entry("completed", "A.2020.mkv", 1, 100)],
        )];
        let tgt = [inv(
            MEDIA,
            vec![linked, entry(MEDIA, "films/A.2020.copy.mkv", 3, 100)],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert_eq!(out.shared.len(), 1);
        assert_eq!(out.shared[0].phys, PhysicalId { dev: 1, ino: 1 });
        assert!(out.pairs.is_empty());
        assert_eq!(out.stats.already_linked, 1);
        // nlink alone is never a link: nlink 2 with no path in the targets stays in the funnel.
        let mut external = entry("completed", "B.2021.mkv", 7, 50);
        external.sig.nlink = 2;
        let src = [inv("completed", vec![external])];
        let tgt = [inv(MEDIA, vec![entry(MEDIA, "B.2021.mkv", 8, 50)])];
        let out = run(&src, &tgt, &[], &fs);
        assert!(out.shared.is_empty());
        assert_eq!(out.pairs.len(), 1);
    }

    #[test]
    fn semantic_stage_samples_equal_sizes_only_and_stops_at_first_proposal() {
        let fs = FakeFs::default();
        let src = [inv(
            "completed",
            vec![entry(
                "completed",
                "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP.mkv",
                1,
                1000,
            )],
        )];
        let tgt = [inv(
            MEDIA,
            vec![
                entry(MEDIA, "Charlie Movie (2019) MULTi 1080p WEB.mkv", 2, 1000),
                entry(MEDIA, "Charlie Movie (2019) Bluray-2160p.mkv", 3, 3000),
                entry(MEDIA, "Charlie Movie (2019) Remux.mkv", 4, 1000),
            ],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert_eq!(fs.sample_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            fs.compare_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no full compare in a scan"
        );
        assert_eq!(out.pairs.len(), 3);
        let sampled: Vec<_> = out
            .pairs
            .iter()
            .filter(|p| p.content == ContentProof::Sampled)
            .collect();
        assert_eq!(sampled.len(), 1);
        assert!(
            sampled[0]
                .reasons
                .contains(&MatchReason::SamplesNotRejected)
        );
        let bigger = out.pairs.iter().find(|p| p.sig_b.ino == 3).unwrap();
        assert_eq!(bigger.content, ContentProof::None);
        assert!(matches!(
            bigger.rejection,
            Some(Rejection::SizeDiffers { .. })
        ));
        assert!(
            bigger.identity.is_unambiguous(),
            "a different release of the same title"
        );
        let unsampled = out.pairs.iter().find(|p| p.sig_b.ino == 4).unwrap();
        assert!(matches!(unsampled.content, ContentProof::Semantic { .. }));
        assert!(out.pairs.iter().all(|p| !p.content.proves_equal()));
    }

    #[test]
    fn sample_rejection_is_recorded_and_carried_while_unchanged() {
        let fs = FakeFs::default();
        *fs.samples.lock().unwrap() = Some(Ok(false));
        let src = [inv(
            "completed",
            vec![entry("completed", "Echo.Movie.2017.WEB.mkv", 1, 500)],
        )];
        let tgt = [inv(
            MEDIA,
            vec![entry(MEDIA, "Echo Movie (2017).mkv", 2, 500)],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert_eq!(out.pairs[0].rejection, Some(Rejection::SamplesDiffer));
        assert_eq!(out.stats.sample_rejections, 1);
        let session = SessionId::new(removarr_domain::ids::RawUuid::nil());
        let prev = out.match_candidates(session);
        assert!(
            prev[0]
                .reasons
                .contains(&"rejected:samples_differ".to_string())
        );
        // Second scan, unchanged files: no new read.
        let again = run(&src, &tgt, &prev, &fs);
        assert_eq!(fs.sample_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(again.pairs[0].rejection, Some(Rejection::SamplesDiffer));
        assert!(again.pairs[0].reasons.contains(&MatchReason::CarriedOver));
        // Changed mtime: sampled again.
        let mut src2 = src.clone();
        src2[0].entries[0].sig.mtime_ns = MtimeNs(5);
        let _ = run(&src2, &tgt, &prev, &fs);
        assert_eq!(fs.sample_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn verified_proofs_are_carried_over_only_with_unchanged_signatures() {
        let fs = FakeFs::default();
        let src = [inv(
            "completed",
            vec![entry("completed", "X.2020.mkv", 1, 9)],
        )];
        let tgt = [inv(MEDIA, vec![entry(MEDIA, "X.2020.mkv", 2, 9)])];
        let session = SessionId::new(removarr_domain::ids::RawUuid::nil());
        let mut prev = run(&src, &tgt, &[], &fs).match_candidates(session);
        prev[0].content = ContentProof::Identical {
            compared_at: Timestamp(5),
        };
        let out = run(&src, &tgt, &prev, &fs);
        assert!(out.pairs[0].content.proves_equal());
        let mut changed = tgt.clone();
        changed[0].entries[0].sig.mtime_ns = MtimeNs(77);
        let out = run(&src, &changed, &prev, &fs);
        assert_eq!(out.pairs[0].content, ContentProof::NameSize);
        // A stored proof whose target path no longer exists is dropped.
        let renamed = [inv(MEDIA, vec![entry(MEDIA, "X.2020.mkv", 2, 9)])];
        let mut prev2 = prev.clone();
        prev2[0].b = RootedPath::parse(MEDIA, "Elsewhere.mkv").unwrap();
        let out = run(&src, &renamed, &prev2, &fs);
        assert!(out.pairs.iter().all(|p| p.stage != Stage::CarriedOver));
    }

    #[test]
    fn samples_cross_device_and_symmetric_runs() {
        let fs = FakeFs::default();
        // Sample files never get semantic candidates (CON-01): no misleading "replacement".
        let src = [inv(
            "completed",
            vec![entry(
                "completed",
                "Kilo.Movie.2012/kilo.movie.2012.1080p-sample.mkv",
                1,
                10,
            )],
        )];
        let tgt = [inv(
            MEDIA,
            vec![entry(MEDIA, "Kilo Movie (2012).mkv", 2, 99)],
        )];
        let out = run(&src, &tgt, &[], &fs);
        assert!(out.pairs.is_empty());
        assert_eq!(out.stats.samples_skipped, 1);

        let mut far = entry(MEDIA, "Y.2020.mkv", 2, 9);
        far.sig.dev = 2;
        let src = [inv(
            "completed",
            vec![entry("completed", "Y.2020.mkv", 1, 9)],
        )];
        let out = run(&src, &[inv(MEDIA, vec![far])], &[], &fs);
        assert!(out.pairs[0].reasons.contains(&MatchReason::CrossDevice));

        // Same inventory on both sides: one pair per unordered couple, no self pair.
        let lib = [inv(
            MEDIA,
            vec![
                entry(MEDIA, "a/Z.2020.mkv", 1, 9),
                entry(MEDIA, "b/Z.2020.mkv", 2, 9),
            ],
        )];
        let out = run(&lib, &lib, &[], &fs);
        assert_eq!(out.pairs.len(), 1);
    }

    #[test]
    fn unfinished_inventory_and_cancellation_fail_closed() {
        let fs = FakeFs::default();
        let mut partial = inv("completed", vec![]);
        partial.finished = false;
        let v = ContentVerifier::new(ReadLimiter::new(1), 1);
        let e = run_funnel(
            FunnelInput {
                sources: &[partial],
                targets: &[],
                previous: &[],
            },
            &MatchSettings::default(),
            &v,
            &fs,
            &ScanControl::default(),
        )
        .unwrap_err();
        assert_eq!(e.class, ErrorClass::Validation);
        let ctl = ScanControl::default();
        ctl.cancel.cancel();
        let src = [inv("completed", vec![entry("completed", "A.mkv", 1, 1)])];
        let e = run_funnel(
            FunnelInput {
                sources: &src,
                targets: &[],
                previous: &[],
            },
            &MatchSettings::default(),
            &v,
            &fs,
            &ctl,
        )
        .unwrap_err();
        assert_eq!(e.class, ErrorClass::Cancelled);
    }
}
