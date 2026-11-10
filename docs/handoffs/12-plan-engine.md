# Handoff — 12-plan-engine (wave 8)

## Delivered scope

`crates/workflows/src/evaluate/**`, `crates/workflows/src/plans/**`, `crates/workflows/tests/{evaluate,plans}_*.rs` and the
test helper directory `crates/workflows/tests/plans_common/`. Edits allowed by section 15.2: `crates/workflows/src/lib.rs`
(two `pub mod` lines and the crate doc), `crates/workflows/Cargo.toml` (dev-dependency `removarr-storage`, `workspace = true`).
`Cargo.lock` gained that dev edge. Domain, other crates, the root `Cargo.toml` and issue-engine files are untouched
(`tests/common/**` is reused read-only). Normal dependencies are unchanged; the dependency gate prints nothing.

Nothing here executes a mutation: `plans` and `evaluate` hold no `FsMutator`, `QbitControl`, `ArrCommands`, `JellyfinCommands`
or `LiveMode` (grep-verified). The only writes are plans, approvals, plan status transitions, ARR snapshots, proofs and evidence.

| Module | Contents |
|---|---|
| `plans::request` | `PlanRequest` (`Issues`, `Media`, `Release`, `ServiceRescan`), `IssueAction`, `MediaAction`, `PlanSettings`, `PlanError`, `SelectionFault`, `ALL_CAPABILITIES` |
| `plans::build` | `build_plan` (the single planning function), `BuildContext`, `Built`, `ExpectedGain`; selection rules, blocker carry-over, assembly, companions, gains |
| `plans::fsplans` | consolidation, file/folder/junk/incomplete deletions, stalled cleanup, `FixMode`, `RemoveEmptyParent`, `RemoveTorrentTask` |
| `plans::arrplans` | `DeleteReplaced`, `ReplaceRelease` (coverage, numbering, ffprobe checks, old-coverage recovery), `ArrGrabRelease`; `is_not_upgrade_reason`, `decision` (re-exported as `preview_decision`) |
| `plans::service` | `ArrRemoveQueueItem`, `ArrSetMonitored`, `ArrRescan`, `JellyfinRefresh` |
| `plans::gate` | `action_tier`, `live_only`, `blockers` (tier, capability, A8/A9, dry-run) |
| `plans::auto` | `AutoContext`, `AutoPhase`, `auto_requests`, rules A2-A8 per unit |
| `plans::validate` | `check_plan` (plan-engine rules on top of `ActionPlan::validate`), `UNVERIFIABLE_KINDS` |
| `plans::preview` | `preview(&HashedPlan, status) -> PlanPreview` (tiers, annexes, warnings, typed counts, acknowledgements) |
| `plans::expiry` | `staleness`, `verdict`, `StaleReason`, `ArrFingerprints` |
| `plans::planner` | `Planner` (ports, load, preview, create, create_auto, approve, discard, refresh_status, ticket), `PlanPorts`, `ArrNeeds`/`arr_needs`, `ExecutionTicket`, `StoredPlan`, `AutoOutcome` |
| `evaluate` | `ArrFacts`, `GatherRequest`, `gather`, `evaluate_issues`, `evaluate_media`/`MediaEvaluation`, `verify_content`/`VerifyPorts`/`VerifyReport` |

## Public contracts

```rust
// Wiring (16 app / 14 jobs)
let planner = Planner::new(PlanPorts { scans, snapshots, matches, issues, ignores, plans,
                                       arr: vec![sonarr_reader, radarr_reader], probe: Some(ffprobe), clock, ids },
                           settings /* PlanSettings, built from Config */);
// PlanSettings: mode (DryRun|Live from the process), config_fingerprint (Config::fingerprint), plan_ttl_hours,
// capabilities (enabled set), implemented_tier (= workflows::actions::IMPLEMENTED_TIER, owned by 13), process_uid,
// auto_min_age_hours, duration_tolerance, files (IssueSettings).

planner.preview(&req, arr_obs).await   -> Result<Built, PlanError>   // dry-run preview, same path, not stored
planner.create(&req, arr_obs).await    -> Result<Built, PlanError>   // PlanStore::insert (Draft), ARR snapshots saved
planner.approve(id, &ApprovalRequest, arr_obs).await -> Result<Approval, PlanError>
planner.discard(id).await; planner.refresh_status(id, arr_obs).await -> PlanCheck { status, reasons }
planner.ticket(id, simulate).await     -> Result<ExecutionTicket, PlanError>   // the action-runner contract below
planner.create_auto(phase, &AutoContext, &candidate_ids, arr_obs).await -> AutoOutcome { plans (approved), refused }
preview(&built.plan, Some(status))     -> PlanPreview
evaluate_issues(&planner, &ids, arr_obs).await; evaluate_media(&*arr, &media, search).await
verify_content(&VerifyPorts, &ContentVerifier, &ids, &ScanControl).await
```

* `arr_obs` = the in-memory ARR observations (`observe::ServiceCache.arr`). Without them no ARR identity is known and every
  ARR-dependent unit is excluded with `ArrMissing` (fail closed). After a restart, refresh ARR first.
* Selection (CON-32, INV-24): no implicit selection (empty = `InvalidRequest`); an action must fit the category
  (`IssueAction::accepts`); unknown/resolved, stale (other session or bad id), ignored or wrong-category ids reject the whole
  request (`RejectedSelection { reason, ids }`). Duplicated ids are deduplicated; order never changes the plan or its hash.
* Blocked items are excluded and listed: `ActionPlan.excluded` (or `PlanError::NothingPlannable { excluded }` when nothing
  is left). Unit and bulk share the same per-item verdicts (tested).
* Issue blockers are carried over, except what the action re-establishes with fresh evidence: `Delete` on
  `hardlink_missing` (ARR identity and consolidation-only reasons), `DeleteReplaced`/`ReplaceRelease` (ARR identity, and
  consolidation-only reasons for `hardlink_missing`), and `Sample` inside a manual folder (per-sample acknowledgement).
* Errors: `PlanError::class()` maps to section 10.1 (`Validation`, `NotFound`, `Conflict` for stale/session, `CapabilityDisabled`
  for refused auto mode, port and domain classes passed through).

### Requests and action kinds per tier

| Tier | Request | Kind(s) | Capability | Notes |
|---|---|---|---|---|
| 2 | `Issues{hardlink_missing\|duplicate, Consolidate}` | `ConsolidateHardlink` | `consolidate` | completed source -> media target only |
| 2 | `ServiceRescan{sonarr\|radarr\|jellyfin}` | `ArrRescan`, `JellyfinRefresh` | none | live only (`DryRunOnly` in dry-run) |
| 3 | `Issues{orphan_completed\|junk\|hardlink_missing, Delete}` | `DeleteCompletedFile`, `DeleteCompletedFolder`, `DeleteJunk` | `delete_completed` | folder = all-or-nothing |
| 3 | `Issues{hardlink_missing\|orphan_completed, DeleteReplaced}` | `DeleteCompletedFile`/`Folder` + replacement pins | `delete_completed` | ARR "not an upgrade" + replacement present (CP1-F02) |
| 3 | `Issues{incomplete_orphan, Delete}` | `DeleteIncompleteOrphan` | `cleanup_incomplete` | manual only |
| 3 | `Issues{incomplete_stalled, CleanupStalled}` | `CleanupStalledIncomplete` | `cleanup_incomplete` | manual only (ADR-0008) |
| 3 | `Issues{permission, FixMode}` | `FixMode` | `fix_permissions` | owner bits only, process-owned entries |
| 3 | companions | `RemoveEmptyParent` (automatic), `RemoveTorrentTask` (`remove_torrent_tasks = true`) | `delete_completed` (+ `auto_remove_empty_torrents` in auto) | not typed; torrent removals typed separately |
| 4 | `Issues{hardlink_missing, ReplaceRelease{delete_old_media}}` | `ReplaceRelease` (+ `DeleteMediaFile` per old file) | `arr_manual_import` | one action per media; never auto |
| 4 | `Release{media, release}` | `ArrGrabRelease` | `arr_grab` | fresh search, approved release, live only |
| 4 | `Issues{arr_queue, RemoveQueueItem}` | `ArrRemoveQueueItem` | `arr_queue_remove` | live only; `QueueItemHidesError` stays a blocker |
| 4 | `Media{media, SetMonitored}` | `ArrSetMonitored` | `arr_set_monitored` | live only |
| 5 | `create_auto(Consolidations\|Deletions)` | `ConsolidateHardlink`; `DeleteReplaced` units | `mode.auto_mode` + tier 5 | A1-A9 |

Tier gating: a kind whose tier is above `PlanSettings.implemented_tier` gets `NotInBuild`; every action of an automatic plan has
tier 5 (`gate::action_tier`). A disabled capability gives `CapabilityDisabled`. With the defaults (`implemented_tier = T1`)
nothing is plannable until 13 raises `IMPLEMENTED_TIER` and `app` passes it.

## Execution contract for `action-runner` (13)

`Planner::ticket(id, simulate)` returns `ExecutionTicket { plan: HashedPlan (tamper-checked), status, approval, simulate }`:
* `simulate = true`: any `Draft` or `Approved` plan; never changes `PlanStatus`.
* `simulate = false`: status `Approved`, `plan.mode == Live`, `PlanSettings.mode == Live`, not expired (an expired plan is moved to
  `Expired` and refused), and `approval.plan_hash == plan hash` (else `PlanHashMismatch`). 13 then moves the plan
  `Approved -> Executing` (storage requires it before the first live intent) and on to the final status.

Actions are ordered; `depends_on` lists earlier actions that must be **verified** first (companions depend on the deletions they
follow; `RemoveEmptyParent` is deepest first; `DeleteMediaFile` depends on its `ReplaceRelease`). `torrent_hashes` = affected
hashes (path holders, CP1-F09: source **or** target paths, content directories); inode-only sharers are not listed and not
stopped. Every action carries `Precondition::ConfigFingerprint`. Plan-level `torrent_states` is the union of all
`TorrentStates`; `lock_roots` covers targets **and** every pinned path (replacements, other links).

Precondition semantics (revalidate all before the first service mutation; any difference = `NotExecuted` + plan `Invalidated`):

| Precondition | Meaning |
|---|---|
| `EntryUnchanged { path, sig, compare_nlink }` | `stat` equals `sig` (`compare_nlink = true` for deletion targets, false for consolidation pairs, replacement media files, other links, `FixMode`). For `RemoveEmptyParent` it is checked at revalidation, before the children are removed (the runner checks kind/dev/ino again before `remove_empty_dir`). |
| `DirUnchanged { snapshot }` | `snapshot_dir` equals the snapshot (keys relative to the dir, `""` = the dir); includes symlinks, which are unlinked, never followed. |
| `NoScanErrorUnder { path }` | no scan error or protected zone at, above or below `path`. |
| `TorrentStates { states }` | every hash still listed with an allowlisted state and progress 1 (stalled cleanup: still progress < 1 and not busy). The plan-side staleness check compares `(state, progress)` exactly. |
| `NoTorrentReferences { path }` | after a fresh torrent read, no torrent (full ownership index) references `path` or anything below it. For `CleanupStalledIncomplete` the action's own hash is excepted (it is removed first). qBittorrent unreachable = failed. |
| `ServiceAvailable(kind)` | the service answers now. Added for qBittorrent on every ownership-dependent action and for every configured ARR service on consolidations and completed deletions. |
| `NoArrImport { media, hashes }` | no queue item of `media` in an importing/pending/unknown state and none whose download id is in `hashes` (ImportInProgress). Added when the media is identified. |
| `SameDevice { a, b }` | both on the same `st_dev`. |
| `ArrFingerprint { media, fingerprint }` | `media` is the whole media (`episode_ids` empty): re-read `quality_snapshot(media)` and compare the fingerprint (covers media management, recycle bin and hardlinks). |
| `OldCoverageRecoverable { old_files }` | each old media file still has its `PhysicalId`; `OtherLink(p)` paths carry their own `EntryUnchanged`. `Unrecoverable` files are listed in the approval's `acknowledged_unrecoverable`. |

Verification semantics: `InodeEquals { path, phys }` (consolidation: the target **and** the source carry the source inode);
`Absent { path }`; `RecheckVerified { hashes }` (affected hashes; recheck the modified ones, `evaluate_recheck`);
`TorrentAbsent`; `ModeEquals`; `ArrCurrentFile { media (with ARR's episode ids), path = completed source }` = ARR's current
file for those episodes is a new path whose inode is the source inode (3a + 3c); `PathStillHasInode` (3b; a move ->
`FailedNeedsReview` + `ArrImportMoved`); `OldFileAccounted { path, recovery }`; `ArrQueueHasRelease`.
`UNVERIFIABLE_KINDS` (`ArrRemoveQueueItem`, `ArrSetMonitored`, `ArrRescan`, `JellyfinRefresh`) have no verification step in the
domain: their best outcome is `ExecutedUnverified` (gap below).

Gains: `expected_gain.logical` = sizes of the action's own targets; `attributable` = allocated bytes of each inode whose every
link seen (and at least `nlink`) is removed by the plan, attributed once to the action removing the last link (sum of actions
= `plan.gains.attributable`); `added` = new bytes of a `ReplaceRelease` when ARR hardlinks are disabled. `PlanGains.net =
attributable - added`. `Built.expected_gains` lists `(gain_id(phys, size, mtime), bytes, action)` per freed inode (not part of the
hashed plan); observed deltas are execution-time (`ObservedDelta`). The `GainStore` UNIQUE constraint is the cross-plan guard.

Grab: the plan stores the opaque release token; 13 must re-run `search_releases` through the reader sharing the commander's
cache and refuse when the token is absent (09 handoff).

## Rules enforced and where tested

* Stable hash, order independence, tamper detection: `plans_build.rs::same_selection_gives_the_same_plan_and_hash_in_any_order`.
* Selection rejection, no implicit selection: `bad_ids_reject_the_whole_request`; bulk = unit verdicts:
  `bulk_excludes_and_lists_blocked_items_with_the_same_rules_as_unit`.
* Folder all-or-nothing with every annex (T-15, INV-25): `folder_deletion_is_all_or_nothing_and_previews_every_annex`,
  `plans_parity.rs::t15_*`; samples (OQ-11, INV-10): `samples_need_acknowledgement_in_folders_and_are_never_deleted_alone`.
* Companions (CP1-F12, INV-19, INV-49): `empty_parents_are_removed_after_their_contents_deepest_first`,
  `torrent_task_removal_is_opt_in_and_needs_every_wanted_file_planned`.
* Double counting (T-03, INV-21): `gains_count_each_inode_once_and_only_when_every_link_is_removed`, `plans_parity.rs::t03_*`.
* Direction, pins, verification (INV-04, INV-13): `plan_rules_require_direction_pins_and_verification`,
  `consolidation_plan_captures_preconditions_verification_and_gain`.
* Fail closed (OQ-05, CP1-F10): `unfinished_or_erroring_scan_areas_and_unknown_ownership_block_deletions`,
  `plans_never_come_from_unfinished_sessions`; incomplete/stalled (OQ-06): `incomplete_orphans_and_stalled_downloads_*`;
  `FixMode` (INV-32): `fix_mode_adds_owner_bits_only_on_entries_the_process_owns`.
* Tiers, capabilities, dry-run, auto (A1-A9, T-20, T-31): `plans_gate.rs` (7 tests).
* ARR (CP1-F02, F04, F06, F20, INV-34..37, T-06, T-07, T-09, T-19, T-21): `plans_arr.rs` (8 tests).
* Expiry and invalidation triggers (T-16): `plans_expiry.rs` (4), `plans_store.rs` (6, real SQLite: immutability, previews not
  stored, approval records, ticket rules, filesystem change -> `Invalidated`, time -> `Expired`, dry-run plans simulate only).
* Evaluate: `evaluate_jobs.rs` (VerifyContent proofs and evidence, ARR evidence and snapshot storage, ARR score change ->
  `Invalidated` through the planner, search never grabs and is sorted by ARR verdict).

## Deviations from the architecture (section 15.4)

1. **ARR facts are read at planning time** (§7.3 says "from stored snapshots"): no port loads a stored `ArrQualitySnapshot`, and
   the full snapshot plus preview decisions are needed. `Planner` gathers them (`evaluate::gather`, GET only) and saves them with
   `SnapshotStore::save_arr`. Evaluate and planning share the same gather function.
2. **ARR identity comes from the caller's `ServiceCache`** (no persisted library/queue/history, 11 gap). Missing = `ArrMissing`.
3. **`CoverageProof` is not stored as such** (no slot in `ActionPlan`): it is computed and enforced at planning; its mapping is
   recorded in `ManualImportRequest` episode ids and one `ArrCurrentFile` verification step per candidate.
4. **Warnings are not in `excluded`**: `OldCoverageUnrecoverable` (per file) and `ArrHardlinksDisabled` are derived by `preview`
   from `OldCoverageRecoverable` and `expected_gain.added`, so a stored plan previews the same warnings.
5. **`FixMode` computes the mode** (`u+w`, plus `u+x` for directories); the request carries no mode.
6. **A6 "semantic only" uses `ScanIncomplete`** with an explicit detail (no dedicated blocker code).
7. **Approval re-checks staleness** (stored state + fresh ARR fingerprints) and moves a stale `Draft` to `Expired`/`Invalidated`
   instead of approving (`PlanError::Stale`). A transient ARR outage therefore invalidates an ARR-dependent plan (re-plan).
8. **Automatic candidates come from the caller** (`create_auto(.., candidate_ids, ..)`): no port lists current issues.
9. **Scope choices**: `ReplaceRelease` from `hardlink_missing` single files only; `DeleteReplaced` for `hardlink_missing` files and
   `orphan_completed` files/folders; ARR's "not an upgrade" is recognized by rejection text containing `upgrade` or
   `equal or higher` (reasons prefixed `removarr:` never count). A file directly under the completed root is previewed with the
   root as folder.
10. Subject keys of non-issue requests in `excluded`: `m:<svc>:<id>`, `r:<svc>:<id>:<token>`, `s:<service>`.
11. Test helpers live in `crates/workflows/tests/plans_common/mod.rs` (a directory, so it is not compiled as its own test).

## Domain gaps (frozen domain; for an orchestrator patch)

1. **Automatic deletions can never be approved**: `HashedPlan::approve_auto` requires `evidence_level != None`, but a
   `DeleteReplaced` deletion has no content proof (its evidence is ARR quality + presence). The plan engine builds such auto
   plans and `create_auto` reports them as refused (`IncompleteEvidence`): fail closed, tested
   (`folder_deletion_with_arr_evidence_*`). Proposed patch: accept `QualityEvidence::ArrSnapshot` (or a new level) for
   deletion kinds in `approve_auto`.
2. No `VerificationStep` for queue removal, monitoring, rescans and Jellyfin refresh (see `UNVERIFIABLE_KINDS`). Proposed:
   `ArrQueueItemAbsent`, `ArrMonitored`, `ArrCommandCompleted`, `JellyfinRefreshed`.
3. No `ActionPlan` slot for the `CoverageProof` and for plan warnings; no `Precondition` binding an action to its issue subject.
4. `EvidenceKind` has no note variant (11 convention `note:` reused by `evaluate`).

## Open TODOs

* 13: raise `IMPLEMENTED_TIER`; consume `ExecutionTicket`; implement the semantics above; re-search before a grab.
* 14: build `AutoContext` (`auto_mode`, `auto_paused` from `StateStore`, failed subjects/paths of the last 7 days from the
  journal), pass the triggering session's active issue ids to `create_auto`, call `refresh_status` on stored plans after scans.
* 16: build `PlanSettings` (config fingerprint, TTL, capabilities, `IMPLEMENTED_TIER`, process uid) and `PlanPorts`; map
  `PlanError::class()` to HTTP; `ApprovalRequest` from the approve form.
* Orchestrator decisions: domain gaps above; whether `TorrentStates` should compare `(state, progress)` exactly (current
  plan-side rule, per INV-13) or only "still allowlisted" (a seeding torrent flips `uploading`/`stalledUP` often, which
  invalidates plans after the hourly qBittorrent refresh).
* Not run: `cargo build --workspace --release` (no release-only code).

## Commands run and results (host target, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy -p removarr-workflows --all-targets -- -D warnings` and `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test -p removarr-workflows`: pass (6 unit; evaluate_jobs 4, plans_arr 8, plans_build 14, plans_expiry 4, plans_gate 7,
  plans_parity 2, plans_store 6, plus the 90 issue-engine tests).
* `cargo test --workspace`: pass, 595 passed, 0 failed, 1 ignored (the opt-in real-ffprobe test).
* Dependency gates of `docs/development.md` (domain deps, `std::(fs|process|net)` in domain, workflows normal deps): print nothing.
* `grep -rnE "FsMutator|QbitControl|ArrCommands|JellyfinCommands|HardlinkProbeGate|LiveMode" crates/workflows/src/{plans,evaluate}`: nothing.
* No network beyond crates.io; nothing committed.
