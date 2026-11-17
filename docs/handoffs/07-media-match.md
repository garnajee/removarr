# Handoff — 07-media-match (wave 6)

## Delivered scope

`crates/matching/**` only (crate `removarr-matching`). Domain, arr, fs, testkit, the root `Cargo.toml` and other
crates were not edited. `docs/parity-matrix.md`: media-match rows updated at the coordinator's request.

| Module | Contents |
|---|---|
| `normalize` | `ascii_fold` (legacy `ascii_text`), `name_key` (name half of the name+size index), `stem`, `extension` |
| `parse` | `parse_scene_name(file_name) -> ParsedName { normalized, title_tokens, year, episode: Option<EpisodeNumbers{season, episodes}>, absolute, resolution, codec, source, languages, editions, group }` |
| `similarity` | `similarity(a, size_a, b, size_b) -> Similarity { score, reasons: Vec<ScoreReason> }` (DR-09), `sequence_ratio` (CPython difflib port), `SimilarityEngine` (DR-10 pools, bounded `find`) |
| `funnel` | `run_funnel(FunnelInput, &MatchSettings, &ContentVerifier, &dyn ContentReader, &ScanControl) -> FunnelOutput` (stages 1-4), `FunnelPair`, `MatchReason`, `Stage`, `SharedPhysical`, `FunnelStats` |
| `proof` | `ContentVerifier` (`sample_check`, `verify`), `ContentVerdict`, `SampleVerdict`, `WriteCheck`, `PreCompareGate` + `GateOutcome`, `ReadOnlyGate`, `HardlinkProbeGate`, `VerifyRequest` |
| `reject` | `Rejection`, `Side` (typed rejection reasons, stable codes, suggested `BlockerCode`) |
| `limiter` | `ReadLimiter` (blocking semaphore, clamp 1..=4, `peak()`) |
| `checks` | `assess_replacement(current, candidate, DurationTolerance) -> ReplacementAssessment`, single checks, `CheckFailure`, `TechnicalFacts` (explanatory only) |
| `language` | `language_profile`, `languages_are_compatible` (DR-15), `normalize_tag` -> `LangTag { Known, Undetermined, NoLinguisticContent }` |
| `numbering` | `explain_numbering(parsed, &[ArrEpisode], arr_assigned) -> NumberingEvidence` |
| `ffprobe` | `FfprobeProbe` (implements `MediaProbe`), `FfprobeSettings`, `parse_report`, `check_available` |

## Public contracts

### Funnel (11, observe step 6; blocking, run in `spawn_blocking`)
```rust
let verifier = ContentVerifier::new(ReadLimiter::new(cfg.limits.content_read_concurrency), cfg.matching.compare_chunk_bytes);
let out = run_funnel(FunnelInput { sources: &[completed_inv], targets: &media_invs, previous: &match_store.current().await? },
                     &settings, &verifier, &*content_reader, &ctl)?;
match_store.replace_for_session(session, &out.match_candidates(session)).await?;
```
- `Err` only for an unfinished inventory (`Validation`, fail-closed) or cancellation (`Cancelled`); never partial output.
- Stage 1: regular files sharing `(dev, ino)` between a source and a target root -> `out.shared: Vec<SharedPhysical>`
  (`evidence()` gives `EvidenceKind::SamePhysical`); such sources leave the funnel (`stats.already_linked`). `nlink` is
  never used; symlinks are never files.
- Stage 2: `(name_key(file_name), size)` index over target videos. Match -> `Stage::NameSize`, `ContentProof::NameSize`,
  **no content read** (T-10). At most `max_name_size_matches` per source.
- Stage 3 (sources without a name+size match, samples excluded on both sides): `SimilarityEngine::find` with
  `similarity_threshold`, depth `repair_search_limit` (10), keep `semantic_limit` (3) pairs per source.
- Stage 4: equal-size semantic candidates are sampled in rank order until the first one that is **not rejected**
  (`ContentProof::Sampled`, reason `samples_not_rejected`). Later equal-size ones stay `Semantic { score_milli }`.
  Different sizes are kept as identity-only pairs: `ContentProof::None` + `Rejection::SizeDiffers` (a different release
  of the same title; ARR decides). A sample rejection is `ContentProof::None` + `Rejection::SamplesDiffer`.
- **No full comparison in the funnel.** The funnel never constructs `ContentProof::Identical`; it only carries an
  earlier `Identical`/`Differs` (and `Sampled` or a sample rejection) while `MatchCandidate::proof_still_valid` holds.
  Proven pairs that the ranking no longer produces are still carried (`Stage::CarriedOver`) while both files exist
  unchanged. So an unchanged rescan reads no sample twice.
- Pairs are unique per unordered couple; passing the same inventories as sources and targets finds duplicates inside
  them (no self pairs). `a` is always the source side.
- `FunnelPair::to_match_candidate(session)`: `identity = NameParse { score_milli }` (or `None` when the score is 0),
  `reasons` = `MatchReason::code()` list followed by `Rejection::code()`. Codes: `same_name_and_size`, `title_equal`,
  `title_similar:seq=N,jac=N`, `size_ratio=N`, `year_equal=Y`, `year_differs=A/B`, `episodes_equal`, `episodes_differ`,
  `no_common_title_token`, `samples_not_rejected`, `cross_device`, `carried_over`, `rejected:*` (below).
- `FunnelPair::content_evidence(session, at)` / `identity_evidence(at)` build domain `Evidence`.

### Content proof (12 `VerifyContent`, 13 consolidation)
```rust
let verdict = verifier.verify(&*fs_inventory, &*content_reader, &gate, VerifyRequest::from_candidate(&mc), &ctl, &*clock)?;
match_store.update_proof(&mc.a, &mc.b, &verdict.to_content_proof()).await?;
```
Fixed order: (1) `stat` both: missing, not regular, or signature != candidate (nlink ignored) -> rejection, no read;
(2) same `PhysicalId` -> `SamePhysical`; (3) size differs -> rejection, no read; (4) **`gate.check(source, target)`
before any content read** (refusal -> `TargetNotWritable`, nothing read; T-14); (5) `ContentReader::full_compare` under
the `ReadLimiter`; `Changed`/`StateChanged` -> `ChangedDuring`; (6) `stat` both again: any change -> `ChangedAfter`,
even after an equal compare; (7) `Identical { compared_at, write_check }` or `ContentDiffers { offset }`.
`Err` only on cancellation. `to_content_proof()`: `Identical`, `Differs { offset }`, otherwise `None`.
- Gates: 13 passes `HardlinkProbeGate { mutator: &*fs_mutator }` (live `probe_hardlink` into the target's parent);
  12 passes `ReadOnlyGate`, and the verdict records `WriteCheck::NotTested` (execution must compare again anyway).
- `sample_check` returns `SampleVerdict::{NotRejected, Rejected(_)}`; there is no "equal" variant.

### Rejection reasons (`Rejection`, serde snake_case)
| Variant | Code | Proves different? | `blocker_code()` |
|---|---|---|---|
| `SamplesDiffer` | `rejected:samples_differ` | yes | `ContentDiffers` |
| `ContentDiffers { offset }` | `rejected:content_differs@N` | yes | `ContentDiffers` |
| `SizeDiffers { a, b }` | `rejected:size_differs=A/B` | yes | `ContentDiffers` |
| `Missing { side }` | `rejected:missing_a/b` | no | `None` (re-observe) |
| `NotRegularFile { side }` | `rejected:not_regular_a/b` | no | `Symlink` |
| `ChangedBefore/After { side }`, `ChangedDuring` | `rejected:changed_*` | no | `None` (re-observe) |
| `TargetNotWritable { detail }` | `rejected:target_not_writable` | no | `PermissionDenied` |
| `ReadFailed { class }` | `rejected:read_failed:<class>` | no | `PermissionDenied` or `ScanErrorUnder` |

### Replacement checks (12, evaluate/plans; tier 4)
`assess_replacement(MediaFacts { parsed, probe }, MediaFacts { .. }, DurationTolerance { seconds, ratio })` runs five
checks; each is `Pass` or `Block(CheckFailure)`. `blockers()` maps to domain `Blocker`s. Passing everything
authorizes nothing (ARR approval, `CoverageProof`, snapshots stay required).
| Check | Failure -> `BlockerCode` |
|---|---|
| Title (exact normalized tokens, T-07) | `TitleDiffers` -> `ArrAmbiguous` |
| Numbering: episodes (series) or year (movies) | `EpisodesDiffer` -> `NumberingMismatch`; `CoverageReduced { season, missing }` -> `CoverageIncomplete`; `YearMissing`/`YearDiffers` -> `ArrAmbiguous` |
| Edition (T-06) | `EditionDiffers` -> `EditionMismatch` |
| Duration (T-09, `max(seconds, max(d1,d2)*ratio)`) | `DurationDiffers`, `DurationUnverified { side }` (no probe, no/invalid duration) -> `DurationMismatch` |
| Real audio (INV-37) | `AudioLanguageMissing { language }`; `AudioLanguageUnverified` (any `und`/missing tag on either side that prevents confirmation, or no probe); `ReleaseLanguageIncompatible` (no probe, DR-15 names incompatible) -> `AudioLanguageMissing` |
`TechnicalFacts` (resolution, codec, source, group, release languages, probed video codec/height, audio tags) is display
data only. There is no ordering of resolutions, codecs, sources or groups anywhere in the crate (INV-11, REJ-09).

### Numbering (12, explanatory only; INV-36 owned by 12)
`explain_numbering(parsed, &episodes, &decision.episode_ids)` interprets the parsed numbers under `Aired`, `Scene`,
`EpisodeAsAbsolute` (for `SxxEyy`), `Absolute`, `SceneAbsolute` (for `Title - 25`). `agreement`:
`Agrees { schemes }`, `Disagrees` (-> 12 adds `NumberingMismatch`), `NotParsed` (nothing written; not a mismatch by itself).

### ffprobe (12 evaluate; built by `app`)
```rust
let mut s = FfprobeSettings::new(cfg.matching.ffprobe_path.clone(), root_paths /* RootId -> absolute root */);
s.concurrency = cfg.limits.ffprobe_concurrency;            // 120 s timeout and 4 MiB output by default
let probe: Arc<dyn MediaProbe> = Arc::new(FfprobeProbe::new(s));
probe_impl.check_available().await   // at startup: Err(Config) when the program cannot be started
```
argv only (`-v error -protocol_whitelist file -print_format json -show_entries format=duration:stream=...` then one
argument `file:<abs path>`); every path component checked with `symlink_metadata` (no symlink, final regular file);
tokio semaphore (1..=4); timeout with `kill_on_drop`; stdout capped (overflow kills the process), stderr drained and
truncated; errors use the rooted display, never the host path. Classes: missing program `Config`, timeout / non-zero
exit / oversized or unreadable output `Rejected`, pipe I/O `Unavailable`, unknown root or symlink `Confinement`.
Report: cover art (`attached_pic`) is not a video stream; a missing, non-finite or non-positive duration is `None`.

## Invariants enforced (and tests)
- Hardlink = `(dev, ino)` only (`shared_inode_is_physical_identity_and_leaves_the_funnel`, nlink 2 case).
- Name+size and similarity never prove content (`t10_*`, `t11_*`); samples only reject (`samples_only_reject`,
  `t12_*`, V7); identical only after a full compare (`ContentProof::Identical` is built only in `proof.rs`).
- Write test before full compare, no read on refusal (`refused_write_test_reads_no_content`,
  `refused_write_test_precedes_any_read` with a panicking reader).
- Change detection before / during / after (`change_*` unit tests; real-fs `modified_before_the_read_is_detected`,
  `modified_during_the_read_voids_the_comparison`, `modified_after_the_read_voids_an_identical_result`).
- Late-byte difference (`late_byte_difference_in_the_last_chunk_is_found`: last-chunk byte outside every sample window).
- Bounded reads (`ReadLimiter`), bounded ffprobe (`concurrent_probes_are_bounded`: four probes never overlap at 1).
- No full compare in scans (`semantic_stage_samples_equal_sizes_only_and_stops_at_first_proposal` counts 0 compares).
- Parity fixture end to end with the real `ConfinedFs` (`tests/funnel_parity.rs`): 3 shared inodes; R1 FX-04->FX-05
  and R2 FX-32->FX-33 name+size; FX-06->FX-09 semantic + samples (CON-16); FX-10->FX-11 identity only (950); samples
  FX-07/FX-27 skipped; full comparison proves exactly the 3 duplicates of §7.7; V6 voids the stored candidate; V7
  keeps `Sampled` and full comparison reports offset 10000.

Legacy tests ported: T-04, T-05, T-06, T-07, T-09, T-10, T-11 (funnel side), T-12 (proof side), T-19 replacement
(local blockers only; technical tags never block or promote), DR-15 language cases, legacy fixture scores
(1.0, 0.9083, 0.95, < 0.60).

## Deviations from the architecture / legacy (section 15.4)
1. **Manifest** (`crates/matching/Cargo.toml`, own crate): added `serde_json = { workspace = true }` (ffprobe JSON);
   `tokio` now `{ workspace = true, features = ["io-util", "fs"] }` (additive features for pipe reads and
   `symlink_metadata`); dev-dependencies `tempfile`, `removarr-fs`, `removarr-config`, `removarr-testkit` (all
   `workspace = true`) for end-to-end tests. Normal dependencies stay domain-only (dependency gate unchanged).
2. **Write test** (01 handoff deviation for 07): the matching crate cannot mutate, so the hardlink probe is injected as a
   `PreCompareGate`. The ordering "probe before full compare" is enforced inside `ContentVerifier::verify`.
3. **Parser** (DR-08): adds multi-episode, `1x02`, anime absolute, leading `[Group]`/trailing `-GROUP`; the year is the
   last year before the first technical marker (legacy: first year); tags are read only after the title (a title word
   like `French` is not a language); a few markers added (`truefrench`, `vfq`, `vf2`, `vof`, `german`, `ita`, ...,
   `avc`, `vp9`, `576p`, editions `finalcut`, `redux`). Bare `WEB` gives source `web` (legacy: none). All legacy test
   expectations still hold.
4. **Name key** (CON-16): NFKD + combining marks removed + lower case, non-ASCII kept (legacy: `casefold()` only).
5. **Funnel scope**: cross-device pairs are kept with reason `cross_device` (legacy skipped them); the funnel does not
   gate on "media scan clean and same `st_dev`" (DR-11): issue/plan engines block with `ScanErrorUnder`/`CrossDevice`.
   Samples (`policy::is_sample`) get no semantic candidates (CON-01). Different-size semantic pairs are persisted as
   identity-only rows. `Sampled` proofs and sample rejections are carried over too (low I/O), not only VerifyContent proofs.
6. **Local checks**: legacy `resolution_jellyfin_inferieure` dropped (CON-02, REJ-24). Without ffprobe reports the audio
   check blocks as unverified even when names are compatible (stricter than DR-15, INV-37).
7. **ffprobe** reads tracks and languages in one JSON call (legacy: duration only), adds `file:` +
   `-protocol_whitelist file`; an invalid duration is `None` in the report (legacy: error), and `checks` blocks on it.
8. A crate-level `ReadLimiter` bounds reads in addition to the fs adapter's own gate (defensive; harmless double bound).
9. **Numbering**: no user-defined override table (no config key exists and config is frozen); ARR's preview
   `episode_ids` is the explicit mapping, this crate explains it.
10. `similarity::sequence_ratio` omits difflib's autojunk heuristic (applies only to 200+ character strings).

## Open TODOs
- 11: call `run_funnel` in observe step 6 with `MatchStore::current()` as `previous`; persist with
  `replace_for_session`; map `shared`/pairs/rejections to `hardlink_missing`/`duplicate` evidence and blockers
  (`cross_device` -> `CrossDevice`).
- 12: `VerifyContent` = `ContentVerifier::verify` with `ReadOnlyGate` + `MatchStore::update_proof`; Evaluate = ffprobe both
  files, `assess_replacement`, `explain_numbering` (`NumberingMismatch` on `disagrees()`).
- 13: consolidation calls `verify` again with `HardlinkProbeGate` right before `replace_with_hardlink`.
- 16 (`app`): build `MatchSettings` (`files.video_extensions`, `matching.*`), `ContentVerifier`, `FfprobeSettings`
  (root paths from `cfg.roots()`), run `check_available` at startup and report a service-state problem on `Config`.
- Known limits: the symlink check before ffprobe has a TOCTOU window (read-only impact); killing on timeout/overflow
  targets the ffprobe process only (a wrapper script's grandchildren are not killed); mtime granularity can hide a
  same-size rewrite from `stat`, in which case the reader's own checks or the byte comparison still decide.

## Commands run and results (host target, rustc 1.98)
- `cargo fmt --all --check`: pass.
- `cargo clippy -p removarr-matching --all-targets -- -D warnings`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test -p removarr-matching`: pass, 63 tests (48 unit; ffprobe_fake 4 + 1 ignored; funnel_parity 4; proof_real_fs 7).
- `cargo test -p removarr-matching --test ffprobe_fake -- --ignored`: pass with the locally installed ffprobe
  (opt-in, not counted as coverage).
- `cargo test --workspace`: pass, 440 passed, 0 failed, 1 ignored (the opt-in real-ffprobe test).
- `cargo tree -p removarr-matching -e normal --depth 1`: only `removarr-domain` among workspace crates; domain gate prints nothing.
- No network beyond crates.io; nothing committed.
