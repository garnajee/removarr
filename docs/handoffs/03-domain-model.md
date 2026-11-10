# Handoff — 03-domain-model (wave 3)

## Delivered scope

All of architecture sections 4 and 5, in `crates/domain/src` (dependencies unchanged: serde, serde_json,
thiserror, sha2, uuid, async-trait). Plus `crates/testkit/src/clock.rs` (`FakeClock`) and `ids.rs` (`SeqIds`).
No other crate or manifest was touched; `Cargo.lock` is unchanged.

| Module | Contents |
|---|---|
| `ids` | `SessionId`, `PlanId`, `JobId`, `IntentId` (uuid newtypes), `ActionRef`, `SubjectKey` (+ constructors `torrent`, `path`, `rooted`, `pair`, `queue`), `IssueId::derive`, `GainId`, `RawUuid` (re-export of `uuid::Uuid`) |
| `time` | `Timestamp` (UTC ms; `plus_hours`, `age_days`), `MtimeNs` |
| `path` | `RootKind`, `RootId` (validated, `media(canonical_path)`), `RelPath` (validated, also on deserialize), `RootedPath` |
| `fs` | `PhysicalId`, `EntrySignature` (`phys()`, `matches(other, compare_nlink)`), `FsEntry`, `RootInventory` (`is_clean`, `has_issue_under`), `DirSnapshot`, `ProtectedZone`, `SampleSpec`, `CompareOutcome`, `DiffStats`, ... |
| `session` | `AuditSession` (`finish`), `SessionStatus` (`allows_planning`, transition rule), `ServiceKind`, `ServiceState`, `ServiceStatus`, `SessionMetrics` |
| `torrent` | `TorrentHash`, `TorrentState` (all 4.x/5.x + `Unknown`), `destructive_eligible`, `is_checking`, `was_active`, `evaluate_recheck`, `is_working_file`, `MappedPath`, `TorrentInfo`, `TorrentFile`, `TorrentSnapshot` |
| `arr` | all DTOs of section 4.1, `ArrQualitySnapshot::{compute_fingerprint, seal, fingerprint_is_valid}`, `sort_search_results`, `CoverageProof::{missing,is_complete}`, `ArrMediaManagement`, `OldFileRecovery`, `ManualImportRequest`, `CommandState` |
| `jellyfin`, `media` | `JellyfinItemRef`, `JellyfinLibrary`, `ProbeReport` and stream types |
| `evidence` | `EvidenceLevel`, `ContentProof`, `IdentityProof`, `QualityEvidence`, `Provenance`, `Completeness`, `EvidenceKind`, `Evidence`, `combined_completeness`, `MatchCandidate::proof_still_valid` |
| `issue` | `IssueCategory` (13, `as_str`/`parse`/`ALL`), `Severity`, `Outcome`, `Unit`, `BlockerCode` (all, incl. `OldCoverageUnrecoverable`, `ArrHardlinksDisabled`), `Blocker`, `Issue` |
| `candidate` | `CandidateState` (+ explicit `can_transition_to`), `derive_candidate_state` |
| `plan` | `PlanMode`, `PlanOrigin`, `ActionKind` (`tier`, `capability`, `is_auto_eligible`, `is_companion`, `accepts`), `ActionTarget`, `Precondition`, `VerificationStep`, `PlannedAction`, `ActionPlan` (`validate`, `required_count`, ...), `PlanHash`, `HashedPlan`, `PlanStatus`, `Approver`, `ApprovalRequest`, `Approval` |
| `journal` | `JournalStep`, `JournalMode`, `IntentRecord`, `StepOutcome`, `ResultRecord`, `UnfinishedIntent`, `ActionOutcome`, `VerificationResult/Status`, `outcome_from_verification`, `ActionState` machine |
| `gain` | `GainKind`, `GainRecord`, `gain_id`, `estimate_reclaimable`, `GainEstimate`, `PlanGains`, `ObservedDelta`, `aggregate_unique` |
| `policy` | `is_sample`, `RiskTier`, `Capability` (`as_str`, `default_enabled`) |
| `jobs` | `Lane`, `JobKind` (`lane`, `dedupe_key`), `JobStatus` (machine), `JobOutcome`, `JobRecord::queued` |
| `error` | `ErrorClass`, `PortError`, `DomainError` (`class()`) |
| `ports` | `Clock`, `IdGen`, `CancelFlag`, `ScanControl`, and every trait of section 5 verbatim (names, signatures), including `ArrRead::{episodes, media_management}`, `ArrCommands::{grab, rescan_all}`, `JellyfinRead::library_refresh_running`, `MatchStore` |
| `canonical` | `canonical_json`, `canonical_json_string`, `sha256_hex` |

## Invariants enforced in code

- Physical identity (`PhysicalId`), content equality (`ContentProof`), media identity (`IdentityProof`) and
  quality (`QualityEvidence`) are four distinct types; only `ContentProof::Identical` proves equal bytes;
  `nlink` is ignored by `EntrySignature::matches(.., false)` and `MatchCandidate::proof_still_valid`.
- Plans: `HashedPlan::seal` validates (contiguous indices, `depends_on` only earlier actions, kind/target fit,
  `DeleteMediaFile` only after a `ReplaceRelease`, auto plans hold only auto-eligible kinds, every target root in
  `lock_roots`, `expires_at > created_at`) and hashes. `HashedPlan::from_parts` rejects a stored hash that does not
  match. No mutable accessors.
- `PlanHash` = sha256 hex of canonical JSON (object keys sorted recursively, independent of map order and of any
  `serde_json/preserve_order` feature). Use `canonical::canonical_json_string` for every JSON DB column.
- `HashedPlan::approve` (manual): hash equal, not expired (`now >= expires_at` is expired), typed count equals
  `required_count()` (actions minus companions `RemoveEmptyParent`/`RemoveTorrentTask`), torrent removals equal
  `required_torrent_removals()`, every plan sample and every `Unrecoverable` old file individually acknowledged
  (the stored `Approval` keeps only plan-relevant acknowledgements).
- `HashedPlan::approve_auto`: only `PlanOrigin::AutoPolicy`, only auto-eligible kinds, no samples, no
  unrecoverable old files, and every action `Completeness::Complete` with `evidence_level != None`; otherwise
  `IncompleteEvidence` / `AutoApprovalForbidden`. Gains are never read (test `potential_gain_never_authorizes_anything`).
- `PlanStatus`, `ActionState`, `CandidateState`, `JobStatus`, `SessionStatus` have explicit transition tables,
  each tested exhaustively over all pairs. `PlanStatus`: Draft -> Approved|Expired|Invalidated|Discarded;
  Approved -> Executing|Expired|Invalidated|Discarded; Executing -> Executed|PartiallyExecuted|Failed|Interrupted|Invalidated;
  everything else terminal (`Interrupted` never resumes).
- Success requires verification: `outcome_from_verification` returns `Verified` only for a non-empty list of
  all-`Passed` steps; empty/failed/not-run -> `ExecutedUnverified`; simulated -> `Simulated`.
  `ActionOutcome::is_success()` is true only for `Verified`. `ActionState::Verified` is reachable only from `Verifying`
  (or later re-verification of `ExecutedUnverified`).
- Gain identity: `gain_id(phys, size, mtime_ns)`; `aggregate_unique` counts each id once; `estimate_reclaimable`
  per DR-04/T-03 (an inode counts once, only if all links selected and not held elsewhere).
- Unknown/partial evidence stays representable: `ContentProof::None`, `IdentityProof::None/Ambiguous`,
  `Completeness::{Incomplete, Unknown}`; empty evidence combines to `Unknown`, never `Complete`.
  Unknown torrent states parse to `Unknown` and are never destructive-eligible.
- Serialization: only `Vec`/`BTreeMap`/`BTreeSet` in persisted/hashed types. `RelPath`, `RootId`, `TorrentHash`
  validate on deserialize. `TorrentState` serializes as its API string. Enums use serde default (externally tagged)
  except where `rename_all = "snake_case"` is set (unit enums that are DB/URL keys).

## Additions beyond the section 4 minimum (additive, meaning unchanged)

- `ActionTarget` and `PlannedAction.target` (kind-compatible, checked by `ActionKind::accepts`): the architecture
  field list had no place for what an action operates on. 12 and 13 must use it.
- `PlannedAction.evidence_level` and `.completeness` (needed for the auto-approval rule).
- `ActionPlan.samples` (samples inside folder units, for OQ-11 acknowledgement) and `ArrSnapshotRef` for `arr_snapshots`.
- `ApprovalRequest`, `HashedPlan`, `Approver` (the arch writes `by: User | AutoPolicy`).
- `Issue.physical_ids` (cross-category deduplication, OQ-04). `ArrFileRef.size` (needed for CP1-F02 size check).
- `VerificationResult`, `VerificationStatus`, `outcome_from_verification`, `ActionState`, `JobStatus`, `GainTotals`.
- `JobKind` variants: `EvaluateIssues { issue_ids }` and `EvaluateMedia { media, search }` (the arch lists "Evaluate" in two shapes).
- `Capability::default_enabled`, `ServiceKind::as_str`, `ArrService::as_str`.

## Deviations from the architecture

1. `torrent::evaluate_recheck(observations, timed_out)` has no `before` parameter: observations are polls taken after
   the recheck command, so `before` carried no information. `timed_out` is supplied by the caller (13 owns the clock).
2. The testkit `ids.rs` imports `Uuid` as `removarr_domain::ids::RawUuid` because `crates/testkit/Cargo.toml`
   (owned by 02) does not list `uuid`. Other crates without a `uuid` dependency can do the same.
3. `Timestamp`/`MtimeNs` are plain `i64` newtypes (public field), as specified.

## Schema needs for `db-layer` (04)

- Every JSON column: `canonical::canonical_json_string(&value)`. Never `serde_json::to_string` for hashed or compared JSON.
- `plans`: `id` (uuid text), `hash` UNIQUE (64 hex), `status` (snake_case of `PlanStatus`), `mode`, `origin`, `expires_at`
  (ms), `body` = canonical JSON of `ActionPlan`. On `get`, rebuild with `HashedPlan::from_parts` (fails on tamper).
  `PlanStore::insert` receives `(&ActionPlan, &PlanHash)`; verify with `PlanHash::compute` in tests.
- `plan_actions`: `(plan_id, idx)` PK; `kind`, `state` (snake_case `ActionState`), `outcome` JSON (`ActionOutcome`).
- `approvals`: one per plan_id; `plan_hash`; `by`; counts; `acknowledged_samples` and `acknowledged_unrecoverable` as JSON arrays.
  `PlanStore::approve` must also check `status = Draft`, hash equality and not expired (`now >= expires_at` is expired).
  `PlanStore::transition` is a compare-and-swap; validate `from.can_transition_to(to)` before writing.
- `journal_intents.id` = `IntentId` uuid; `mode` live|simulated (`unfinished()` filters live and joins no result);
  `target`/`observed` JSON `Value` columns. `journal_results` FK to intent. `action_outcomes` keyed by `(plan_id, idx)`.
- `gains.gain_id` UNIQUE (32 hex, the `GainId` string); duplicate insert maps to `ErrorClass::Conflict`
  (`DomainError::DuplicateGain` exists for pure callers). `observed_deltas` rows from `ObservedDelta`.
- `jobs.dedupe_key` from `JobRecord.dedupe_key`; partial unique index on `status IN ('queued','running')`
  (`JobStatus::is_active`). `JobStatus` and `Lane` snake_case strings.
- `issues`: `id` (32 hex = `IssueId`), `category` (`IssueCategory::as_str`), `subject`, `first_seen_at` kept across
  replaces; `evidence`, `blockers`, `unit`, `torrent_hashes`, `physical_ids` JSON; `ignores.subject_key` PK.
- `arr_snapshots`: JSON plus `fingerprint` (`ArrQualitySnapshot::fingerprint`; recompute with `fingerprint_is_valid`).
- `match_candidates`: pair paths, both signatures, `identity`/`content` proofs as JSON, `session`.
- `scan_sessions.status` snake_case `SessionStatus`. Paths are stored as `(root, rel)`; never absolute host paths.
- Enum text encoding: serde snake_case for unit enums with `rename_all`; for externally tagged enums store the JSON.

## Open TODOs / notes for later waves

- `Capability::default_enabled` mirrors section 13.2; 05 owns the config override.
- Test-only builders live in `crates/domain/src/testutil.rs` (`cfg(test)`, crate-private). Wave-5+ crates that need
  plan/issue fixtures should build their own or use `removarr-testkit` (22 may add helpers).
- `proptest` is declared as a dev-dependency but not used yet (21 may add property tests, for example `RelPath`).
- Legacy tests routed to domain-model: only T-03 (`gain_counted_only_when_every_hardlink_is_selected` in `gain.rs`).
  INV-17, INV-21, INV-28, INV-42, CTX-12 and NEW-08 have unit tests as listed above.

## Commands run and results

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass, no warnings.
- `cargo test --workspace`: pass (domain 80 tests, testkit 3).
- Dependency gates from `docs/development.md` (domain deps, `std::(fs|process|net)`, workflows, web): all print nothing.
