# Handoff — 13-action-runner (wave 9)

## Stage A (tiers 1-2)

Scope of this stage (MASTER section 10, risk tiers and CP2): tier 1 (the complete execution
pipeline in simulation for every plan, ticket intake, live revalidation, journal, outcome and
verification recording, recovery of unfinished intentions) and tier 2 (live exact-copy
consolidation with the qBittorrent transaction). Tier-3, tier-4 and tier-5 kinds are present
only as explicit `NotInBuild` refusals, in the runner and in the filesystem mutator, until their
CP2 review.

### Delivered files

| Path | Contents |
|---|---|
| `crates/fs/src/mutate.rs` (+ `pub mod mutate;`, `pub use mutate::LiveFsMutator;` in `lib.rs`) | `LiveFsMutator::new(&Roots, &LiveMode)` implementing `FsMutator`: `probe_hardlink`, `replace_with_hardlink`; `remove_file`, `remove_tree`, `remove_empty_dir`, `set_mode` return `CapabilityDisabled` ("not in this build", tier 3). `HARDLINK_REFUSED_HINT`, `CROSS_DEVICE_HINT`. |
| `crates/fs/tests/mutate_hardlink.rs` | T-13, T-14, changed/missing target, symlink refusals (source, target, component, root), tier-3 refusals. |
| `crates/workflows/src/actions/mod.rs` | `IMPLEMENTED_TIER = T2`, `supported(kind)`, `ActionPorts`, `ExecutingPlans`, `ActionSettings`, re-exports. |
| `actions/dry_run.rs` | `Mutators { mode, fs, qbit, arr, jellyfin }`, `Mutators::recording(&[ArrService]) -> (Mutators, RecordingSet)`, `Mutators::live(..)`, the four `Recording*` adapters with `CallCounter`. |
| `actions/events.rs` | `ActionEvent`, `ActionProgress`, `Reporter` (watch channel + event list + optional `EventSink`). |
| `actions/journal.rs` | `Ctx` and the journaled step: `record_intent` (durable) -> adapter call -> `record_result`; `Simulated` short-circuit. |
| `actions/revalidate.rs` | Live revalidation of every `Precondition` plus the wanted-file-set check. |
| `actions/qbit.rs` | `LeftStoppedReason`, `RestorePlan`, `plan_restore`, the journaled stop / recheck-and-wait / restore steps. |
| `actions/consolidate.rs` | The `ConsolidateHardlink` batch (CONTEXTE 3.5 order, CON-15, CON-26). |
| `actions/verify.rs` | `InodeEquals` and `RecheckVerified` on real re-reads; everything else `NotRun`; simulation -> `Simulated`. |
| `actions/run.rs` | `execute(..) -> ExecutionReport`, ticket checks, tier gate, plan transitions, outcomes, gains, observed delta. |
| `actions/recovery.rs` | `recover(..) -> RecoveryReport` (architecture 7.5). |
| `crates/workflows/tests/actions_common/mod.rs` | Test lab: temp roots + real `ConfinedFs` + real `LiveFsMutator` (behind a counter) + real SQLite `Repos` + in-memory `FakeQbit` with scripted recheck + crash hooks (`Hang`, `HangJournal`). |
| `crates/workflows/tests/actions_consolidate.rs`, `actions_crash.rs` | 13 + 8 integration tests (list below). |

Manifests: `crates/fs/Cargo.toml` gained the **dev**-dependency `removarr-config = { workspace = true, features = ["test-support"] }` (for `LiveMode::for_tests()` in the mutator tests); the crate's normal dependencies are unchanged (domain, config, tokio, rustix, serde, thiserror, tracing, async-trait; `cargo tree -e normal` verified). `crates/workflows/Cargo.toml`: the existing dev-dependency `removarr-config` now enables `test-support`. No normal dependency changed anywhere; the three dependency gates of `docs/development.md` print nothing. Root `Cargo.toml` untouched.

### Public contracts

```rust
// app::wiring (16)
let (mutators, _counters) = Mutators::recording(&arr_services);          // always constructible
let mutators = Mutators::live(Arc::new(LiveFsMutator::new(cfg.roots(), &live)),  // only with LiveMode
                              qbit_controller, vec![(ArrService::Sonarr, sonarr_cmd), ..], jellyfin_cmd);
let ports = ActionPorts { plans, executing /* ExecutingPlans over PlanRepo::ids_with_status(Executing) */,
                          journal, gains, scans, snapshots, fs, content, qbit: Option<Arc<dyn QbitRead>>,
                          arr: Vec<Arc<dyn ArrRead>>, jellyfin, clock, ids };
let settings = ActionSettings { config_fingerprint: cfg.fingerprint(), recheck_timeout, recheck_interval: 2 s,
                                verifier: ContentVerifier (shared with evaluate) };
// jobs (14), Execute lane, exclusive root locks held by the caller
let ticket = planner.ticket(plan_id, simulate).await?;
let reporter = Reporter::new();                       // reporter.subscribe() -> watch::Receiver<ActionProgress>
let report: ExecutionReport = actions::execute(&ports, &mutators, &settings, ticket, &cancel_flag, &reporter).await?;
// jobs (14), before any other execute job, retried with backoff on Err(Unavailable)
let rec: RecoveryReport = actions::recover(&ports, &mutators, &settings, &reporter).await?;
```

* `execute` returns `Err` only when the run cannot start (dry-run process asked to run live
  -> `CapabilityDisabled`; dry-run plan asked to run live; status not `Approved`; approval hash
  mismatch; a simulation of a terminal plan -> `Conflict`) or when the journal itself fails
  mid-run (the plan then stays `Executing` for recovery). Every runtime failure is a per-action
  `ActionOutcome` in the report.
* `simulated = ticket.simulate || mutators.mode == DryRun`. A simulation walks every step and
  writes the same journal records with `JournalMode::Simulated`; **no mutator is ever invoked**
  (the recording adapters are a second barrier with counters). Verification steps report
  `Simulated`; the plan status and the stored action states never change; the report carries
  the would-be outcomes (`ActionOutcome::Simulated`, or `NotExecuted` with the live blockers
  when revalidation or the real full comparison refuses). The real `stat`, torrent listing,
  file lists, ARR reads and the **real full byte comparison** run in a simulation.
* `ExecutionReport { plan, simulated, status: Option<PlanStatus>, actions: Vec<ActionResult { index, kind, outcome, verification }>, left_stopped: BTreeMap<TorrentHash, LeftStoppedReason>, interrupted, events }`.
* `ActionEvent` (serde, `tag = "event"`): `PlanStarted`, `Revalidated { pass, failed }`,
  `StepIntent`, `StepResult`, `TorrentsStopped`, `TorrentsStarted`, `TorrentsLeftStopped`,
  `ActionFinished`, `BatchInterrupted`, `PlanFinished { status }`, `RecoveryStarted`,
  `RecoveredPlan`. `ActionProgress { plan, simulated, stage, done, total, current, last_event }`
  with stages `starting`, `revalidate`, `probe`, `consolidate`, `recheck`, `verify`, `finished`.
  Reporting (15) reads `report.events` or attaches an `EventSink`; web (16/19) polls the watch
  receiver (memory only, no DB per file).
* `LeftStoppedReason`: `InitiallyStopped`, `NotRechecked`, `RecheckTimedOut`, `RecheckFailed`,
  `UnknownInitialState`, `StartFailed`, `NoLiveController`.
* Plan-level steps (`QbitStop`, `QbitRecheck`, `QbitStart`) are journaled under the index of the
  first action still in the batch (the journal FK needs an action index).
* Final live plan status: `Interrupted` (cancellation), `Invalidated` (any revalidation
  failure, unsupported kind), `Executed` (every action `Verified`), `PartiallyExecuted` (at
  least one `Verified`/`ExecutedUnverified`), else `Failed`.

### Execution order of a consolidation plan (live)

1. Ticket checks; `Approved -> Executing`; free space of `lock_roots`.
2. Revalidation pass 1 of every action (below). Any blocker: all actions `NotExecuted`, plan
   `Invalidated`, no intent written.
3. Per item: intent `FsProbeHardlink` -> `probe_hardlink` (temp link in the target directory,
   inode checked, unlinked) -> result. `EPERM`/`EACCES`/`EXDEV` fail the item before any content
   read with `HARDLINK_REFUSED_HINT` / `CROSS_DEVICE_HINT` (no `sudo`, CON-30).
4. Intent `QbitStop { hashes }` with `observed = { initial_states }` -> stop. A stop failure:
   compensating journaled start of the initially active hashes, every item `Failed`.
5. Revalidation pass 2 (fresh reads). Any blocker: resume untouched hashes, `Invalidated`.
6. Per item: intent `FsProbeHardlink` ("pre-compare gate") -> `ContentVerifier::verify` with
   `HardlinkProbeGate` (bounded, in `spawn_blocking`) -> result. Anything but `Identical` fails
   the item with nothing mutated (`Rejected` for differing bytes, `StateChanged` for a change
   before/during/after, `PermissionDenied` for a refused probe). Then intent
   `FsReplaceWithHardlink { source, target, expect_source, expect_target }` -> `linkat` temp,
   `renameat`, `lstat` proof, `fsync` dir -> result. A failed replace step is judged by a `stat`
   of the target: link present -> treated as modified, outcome at most `ExecutedUnverified`;
   absent -> `Failed`, hash untouched; unobservable -> `FailedNeedsReview`, hash rechecked.
7. Intent `QbitRecheck` for modified hashes -> recheck, listing polled every
   `recheck_interval` until `evaluate_recheck` is final or `recheck_timeout` (the cancel flag
   is not consulted here: compensation is bounded by the timeout).
8. `plan_restore`: intent `QbitStart` only for initially active hashes that are untouched or
   recheck-verified; everything else is `left_stopped` with its reason, event
   `TorrentsLeftStopped`.
9. Verification: `InodeEquals` by `stat` on both paths, `RecheckVerified` from step 7;
   `outcome_from_verification`. Gains: `AttributableConsolidation` (gain id of the replaced
   media inode, bytes = the planner's `expected_gain.attributable`) only for `Verified`
   actions, plus the observed free-space delta; a duplicate gain id is logged, never counted.
10. Outcomes recorded, plan transitioned, `PlanFinished`.

Cancellation is honoured between items only (8.2): remaining items are `NotExecuted`, modified
hashes are rechecked, untouched ones are started, plan `Interrupted`, event `BatchInterrupted`.
Automatic plans stop at the first item failure; manual plans continue (CON-26).

### Live revalidation (`revalidate.rs`)

| Precondition | Live check | Blocker |
|---|---|---|
| `EntryUnchanged` | `FsInventory::stat`, `matches(sig, compare_nlink)`, symlink refused | `ScanIncomplete` ("changed since the plan was built"), `Symlink`, `ScanErrorUnder` on read error |
| `DirUnchanged` | `snapshot_dir` equality | `ScanIncomplete` |
| `NoScanErrorUnder` | latest stored session allows planning; stored inventory finished and `!has_issue_under` | `ScanIncomplete` / `ScanErrorUnder` |
| `SameDevice` | both `stat` same `dev` | `CrossDevice` |
| `TorrentStates` | fresh `version()` + `torrents()`; same eligibility class under `issues::protection` (version-aware), same progress, busy rule (12 follow-up 4) | `TorrentProtectedState`, `ServiceUnavailable` |
| `NoTorrentReferences` | fresh listing + `files()` -> `issues::TorrentIndex::hashes_under` (own hash excepted) | `TorrentReferencesTarget` |
| `ServiceAvailable` | `version()` / ARR `status()` / Jellyfin `status()` | `ServiceUnavailable`, `ServiceNotConfigured` |
| `NoArrImport` | ARR `queue()`: `import_in_progress` for the media or for a download id in `hashes` | `ImportInProgress` |
| `ArrFingerprint` | `quality_snapshot(media)` fingerprint equality | `ArrAmbiguous`, `ServiceUnavailable` |
| `OldCoverageRecoverable` | `stat` each old file, same `PhysicalId` | `ScanIncomplete` |
| `ConfigFingerprint` | equals `ActionSettings.config_fingerprint` | `ScanIncomplete` |
| `ArrReplacementPresent` | covered by its paired `EntryUnchanged` + `ArrFingerprint` | — |
| **wanted-file set** (not in the plan) | for every hash of `action.torrent_hashes`: live `files()` wanted set `(index, name)` equals the stored snapshot's; the hash still holds a path of the action (file candidate or content directory) | `TorrentProtectedState` |

Reads are taken lazily, once per pass; pass 2 after the stop uses fresh reads. Any read error is
a blocker (fail closed), never skipped.

### Recovery (`recovery.rs`)

Plans found through `JournalStore::unfinished()` **and** `ExecutingPlans::executing()` (a
process killed between a result and the next intent leaves no unfinished intent but a stopped
torrent). Per plan, from `for_plan`: unfinished `FsReplaceWithHardlink` -> `stat` of the target
decides (`Done` with the observed state and action `ExecutedUnverified`, or `Skipped` and
`NotExecuted`; unobservable -> `Failed` and treated as modified); unfinished `FsProbeHardlink`
-> `Skipped` plus a note that a `.<name>.removarr-probe-*` link may remain; unfinished
`QbitStop`/`QbitRecheck`/`QbitStart` -> `Skipped`, resolved by the listing. Then, with a live
controller: modified hashes (from `FsReplaceWithHardlink` and `QbitRecheck` intents) are
rechecked again and started only if `Verified`; stopped untouched hashes that were active are
started; never a redo of a mutation. Without a live controller: `NoLiveController`, torrents
stay stopped and visible. Actions get `NotExecuted`/`ExecutedUnverified` (a `Conflict` on an
already final action is ignored), the plan goes `Executing -> Interrupted`. `Err(Unavailable)`
(qBittorrent down) leaves the plan `Executing` with the filesystem results already written; the
retry resolves it.

### Crash-boundary and invariant tests

`crates/workflows/tests/actions_crash.rs` (a fake hangs at the boundary, the execution task is
aborted, recovery runs on the same SQLite journal and temp roots):
* `crash_before_the_stop_intent_touches_no_torrent_and_interrupts_the_plan` (before the first service intent);
* `crash_before_the_replace_intent_resumes_the_untouched_torrent` (between a result and the next intent);
* `crash_after_the_replace_intent_before_the_mutation_is_resolved_as_not_executed`;
* `crash_after_the_mutation_before_the_result_is_rechecked_and_reported_unverified`;
* `crash_after_the_mutation_with_a_failing_recheck_leaves_the_torrent_stopped`;
* `crash_during_the_recheck_rechecks_again_at_recovery`;
* `recovery_without_a_live_controller_leaves_torrents_stopped_and_visible`;
* `recovery_retries_when_qbittorrent_is_unreachable_and_executes_nothing_meanwhile`.

`crates/workflows/tests/actions_consolidate.rs`:
* `t13_live_consolidation_links_completed_toward_media_and_verifies_by_inode` (journal order, Done results, one gain);
* `t30_dry_run_process_runs_the_whole_pipeline_with_zero_mutations` (recording set, zero calls, inodes unchanged, simulated journal, real compare);
* `simulation_in_a_live_process_never_invokes_the_live_adapters` (6.8(e)) + dry-run process / dry-run plan refusals;
* `t14_permission_probe_fails_before_any_content_read_and_before_the_stop` (zero compares, zero torrent commands, no `sudo`);
* `t11_t12_a_difference_or_change_during_the_comparison_mutates_nothing` (torrent resumed, no recheck);
* `t16_live_revalidation_refuses_any_change_and_invalidates_the_plan` (target changed, wanted-file set changed, protected state, qBittorrent down, change between stop and pass 2 -> resumed);
* `t23_partially_affected_torrent_is_left_stopped_and_reported` (timeout and failed recheck, `ExecutedUnverified`, no gain);
* `t22_cancellation_between_items_resumes_untouched_and_rechecks_modified_torrents`;
* `a_failed_replace_step_is_judged_by_the_inode_and_never_reported_verified`;
* `a_stop_failure_leaves_nothing_modified_and_restores_what_it_can` (`StartFailed` reported);
* `tiers_three_to_five_are_explicit_refusals` (every other kind, live and simulated, no intent);
* `an_automatic_plan_stops_at_the_first_failure_and_a_manual_one_continues`;
* `paths_never_reach_a_shell_and_the_media_root_is_the_only_write_location`.

`crates/fs/tests/mutate_hardlink.rs`: T-13, changed/missing target, symlinks never followed
(source, target, component, root), T-14 message, tier-3 refusals. Unit:
`actions::qbit::tests::restore_starts_only_active_hashes_that_are_untouched_or_recheck_verified`.

Invariant greps: `deleteFiles` once in `crates/qbit/src` with the literal `false`; `LiveMode::`
only in `config`; no `std::process`/`Command` and no string-built path in `actions` or
`mutate.rs` (every operation is an `*at` call on a directory handle).

### Deviations from the architecture (section 15.4)

1. `Mutators::live(..)` does not take `&LiveMode` (6.4): `workflows` may not depend on
   `config` (2.2). The adapters it receives can only be built with the token, so the proof of
   liveness is their existence; `app::wiring` is the only constructor site.
2. Simulations do not call `JournalStore::record_outcome` (7.4 implies outcomes for every run):
   `ActionState::Simulated` is terminal and would block a later live run of the same approved
   plan. Simulated outcomes live in the `ExecutionReport`; `PlanRepo::record_simulation_run`
   (storage inherent method) is for the caller (14/16) to note the run.
3. `ExecutingPlans` port added (not in section 5): recovery needs the `Executing` plans without
   unfinished intents. App implements it over `PlanRepo::ids_with_status`.
4. The tier-2 service commands `ArrRescan` and `JellyfinRefresh` (13.1) are refused in stage A
   (`supported()` is consolidation only) as instructed; next stage.
5. `batch_interrupted` (8.2, T-22) is an `ActionEvent` and the plan status `Interrupted`, not a
   journal step (`JournalStep` has no such variant; the compensating `QbitStart` intents are
   journaled).
6. The compare runs in the execution lane inside `spawn_blocking` under the verifier's own
   `ReadLimiter` (no second limiter).
7. `NoScanErrorUnder` is checked against the latest stored session and inventory (no rescan at
   execution; 7.4 lists only live reads for the other preconditions).
8. Two `FsProbeHardlink` intents per item (step 3 probe, step 6 pre-compare gate), so every
   temporary link is behind its own durable intent.
9. Blocker codes for revalidation failures reuse existing codes (`ScanIncomplete` for "changed
   since planning" and config change, `FailedConsolidation` for "not reached / interrupted"):
   the frozen domain has no `Changed`/`Interrupted` blocker code (domain gap, see below).

### Domain gaps (for a later patch)

* `BlockerCode` has no `ChangedSincePlanning` / `Interrupted` variant.
* `JournalStep` has no `BatchInterrupted` marker.
* `TorrentStates` has no wanted-file set (12 follow-up 4): compared live against the stored
  snapshot instead.

### Open TODOs (tiers 3-5 and stage B)

* Stage B (tier 2 remainder): `ArrRescan` / `JellyfinRefresh` with `ServiceObservation` fed to
  `evaluate_service_step` (poll `command_status` / `library_refresh_running` bounded by
  `recheck_timeout`).
* Tier 3: `LiveFsMutator::{remove_file, remove_tree, remove_empty_dir, set_mode}` (deepest
  first, symlinks unlinked never followed, T-17), the deletion batch of 7.4 (parent cleanup
  under completed only, `RemoveTorrentTask` with `decide_removal` and the presence predicate,
  partially affected hashes left stopped T-23 deletion side, T-32), `CleanupStalledIncomplete`,
  `DeleteIncompleteOrphan`, `FixMode`; verification `Absent`, `TorrentAbsent`, `ModeEquals`;
  `AttributableDeletion` gains; recovery of unfinished `FsRemove*`/`QbitRemoveKeepData` intents.
* Tier 4: `ReplaceRelease` (`ArrManualImport`, `PathStillHasInode`, `ArrCurrentFile`,
  `OldFileAccounted`, `ArrImportMoved` -> `FailedNeedsReview`), `ArrGrabRelease` (re-search
  through the reader sharing the commander's cache, refuse when the token is absent),
  `ArrRemoveQueueItem`, `ArrSetMonitored`, Jellyfin recognition check.
* Tier 5: nothing in the runner beyond `IMPLEMENTED_TIER = T5` once 12/14 deliver the cycle.
* Leftover probe links after a crash during a probe are reported, not removed (needs tier-3
  `remove_file`).
* 14: call `recover` before any execute job, retry on `Err(Unavailable)` with backoff; bridge
  `CancellationToken` to `CancelFlag`; hold exclusive root locks; `record_simulation_run`.
* 16: build `ActionPorts`, `ActionSettings` (`thresholds.recheck_timeout_seconds`, 2 s), the
  `ExecutingPlans` impl, pass `IMPLEMENTED_TIER` to `PlanSettings`; guard test 6.8(a).
* 22: T-30-plus end to end with the real connectors' fakes; a live-process simulation guard
  with the loopback fakes (6.8(e), integration half).

### Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 634 passed, 0 failed, 1 ignored (the opt-in real-ffprobe
  test). New: fs `mutate_hardlink` 5, workflows `actions_consolidate` 13, `actions_crash` 8,
  unit 1.
* Dependency gates of `docs/development.md`: print nothing. `cargo tree -p removarr-fs -e normal --depth 1`: unchanged set.
* `grep -rn deleteFiles crates/*/src`: one literal `false` in `crates/qbit/src/control.rs`;
  `grep -rn 'LiveMode::' crates/*/src`: `crates/config/src/model.rs` only.
* No network beyond crates.io; nothing committed; no real storage or service touched (temp
  directories and in-memory fakes only).

## Stage B (CP2-tier3 fixes + tier 3)

Review: `docs/reviews/cp2-tier3.md` (PASS with condition). Steps 1 and 2 are complete: F01-F14
are closed (F03 and F10 through domain patch 3 and the plan-engine pins), tier 3 is implemented
and `IMPLEMENTED_TIER = T3`. Tier 4/5 kinds stay `NotInBuild`. Nothing was committed and no real
storage or service was touched.

New modules: `actions/batch.rs` (shared batch plumbing, sample checks, disk observations),
`actions/delete.rs` (deletion batch), `actions/stalled.rs` (stalled incomplete cleanup),
`actions/simple.rs` (`FixMode`, `ArrRescan`, `JellyfinRefresh`). `LiveFsMutator::{remove_file,
remove_tree, remove_empty_dir, set_mode}` in `crates/fs/src/mutate.rs` (handle based, `unlinkat`).

### Per-finding status

| ID | Status | What changed | Test |
|---|---|---|---|
| F01 | **fixed** | (a) The live replace step records what the disk shows, not only what the call returned: `consolidate.rs` writes `Done { final_state: ReplaceObserved { replaced, phys, error } }` when the target carries the source inode after a failed call, `Failed (target unchanged)` or `Failed (target could not be observed)` otherwise, and the hashes are marked modified whenever the mutation happened or cannot be ruled out. (b) `recovery.rs` re-observes the target of every `FsReplaceWithHardlink` intent whose result is `Failed` or missing; only `Skipped`/`Simulated` rule the mutation out. Observed replaced -> modified, `ExecutedUnverified`; unobservable -> modified, `FailedNeedsReview`; untouched -> `NotExecuted`. Any other mutation step (none reachable yet) is treated as possibly executed unless ruled out. | `actions_fixes.rs::f01_a_failed_but_mutated_replace_result_is_rechecked_at_recovery_in_a_two_item_plan` (journal seam stores the `Done` as `Failed`; crash before item b's gate; a is rechecked and `ExecutedUnverified`, b `NotExecuted`, nothing redone), `::f01_an_unobservable_target_stays_possibly_modified_across_a_recovery_retry` (target directory mode 000; first pass writes `Failed` then fails on qBittorrent; the retry rechecks, `FailedNeedsReview`, starts only because the recheck verified) |
| F02 | **fixed** | `revalidate.rs::check_live_holders`: every pass takes the fresh listing, reads `files()` for every torrent whose `save_path`/`download_path`/`content_path` (or their parent) is related to an action path, builds `issues::TorrentIndex`, binds the stored inventories of the action roots (inode view), and refuses with `TorrentReferencesTarget` when `hashes_under(path)` or `hashes_for_inode(stat(path))` contains a hash outside `action.torrent_hashes` (own hash excepted). Any `NoRule`/`Invalid` on a listed torrent's base paths or file candidates refuses with `MappingError`; qBittorrent unreachable refuses. | `::f02_a_torrent_that_started_holding_a_path_after_planning_invalidates_the_plan` (cross-seed of the completed path, and a library seeder of the media path: `Invalidated`, nothing stopped), `::f02_a_holder_by_inode_only_and_a_mapping_fault_near_the_paths_both_refuse` |
| F03 | **fixed** (domain patch 3 + plan-engine pins) | `revalidate.rs`: every `Precondition::TorrentWantedFiles { hash, wanted }` is compared with a fresh `files()` read (`priority > 0` as `(index, name)`) in every pass; any difference, a delisted torrent or a read fault refuses with `ChangedSincePlanning`. The "latest stored snapshot" fallback is removed. `check_hashes_hold` keeps only the "hash still holds a path of the action" rule. | `actions_wanted.rs::the_pinned_wanted_set_passes_unchanged_and_every_change_refuses_before_any_mutation`, `::a_consolidation_built_through_the_planner_executes_end_to_end` |
| F04 | **fixed** | `run.rs::execute`: when `simulated`, the mutators are replaced by `Mutators::recording(&services)` before the `Ctx` exists; a simulation can no longer hold a live adapter. Module doc updated. | `::f04_f05_a_simulation_in_a_live_process_runs_on_the_recording_set_and_touches_no_service` (live counting fs/qBittorrent/ARR/Jellyfin adapters see zero calls through the swap; recording set with Sonarr and Radarr in a dry-run process sees zero calls) |
| F05 | **fixed** | The lab now wires a live counting `ArrCommands` (Sonarr) and `JellyfinCommands`, a reachable `ArrRead` and `JellyfinRead`, and `Mutators::recording(&[Sonarr, Radarr])`; `Lab::service_mutations()` is asserted zero in the guards. | same test as F04 |
| F06 | **fixed (doc)** | Module doc of `actions` and this handoff: stage A/B simulate consolidation only; every other kind is refused live and simulated. Tier-3 simulation is a tier-3 deliverable (T3-16). | `tiers_three_to_five_are_explicit_refusals` (unchanged) |
| F07 | **fixed (report)** | Recovery notes name both leftovers: `.<name>.removarr-probe-*` for an unfinished probe or a probe whose unlink failed (result message "removing the probe link"), `.<name>.removarr-link-*` for a replace intent interrupted before the rename. Cleanup needs tier-3 `remove_file` (open). | covered indirectly by `crash_after_the_replace_intent_before_the_mutation_is_resolved_as_not_executed` (note emitted); no dedicated assertion |
| F08 | **fixed** | Direction enforced at three layers: planner (12), runner `consolidate::item()` (`ProtectedZone`, no intent), mutator `check_direction` in `probe_hardlink` and `replace_with_hardlink` (`Confinement`). | `fs/tests/mutate_hardlink.rs::f08_the_mutator_refuses_any_direction_but_completed_to_media`, `actions_fixes.rs::f08_the_runner_refuses_a_plan_whose_direction_is_not_completed_to_media` |
| F09 | **fixed** | `qbit.rs::restore` always journals a `QbitStart` intent when the transaction touched anything: target = start set (possibly empty), observed = `RestoreObserved { recheck, left_stopped }`, result `Done { started }`. The left-stopped map and reasons are therefore durable in the journal (reporting 15 reads it from `journal_intents.observed`). | `::f09_hashes_left_stopped_and_their_reason_are_journaled_even_when_nothing_starts` |
| F10 | **fixed** (domain patch 3) | Revalidation, config change, store read fault and "task still has wanted files" use `ChangedSincePlanning`; "not reached / dependency failed / cancelled / interrupted" use `Interrupted` (batch, recovery, run). `ScanIncomplete` / `FailedConsolidation` are no longer reused for these. | asserted across `actions_consolidate.rs`, `actions_delete.rs` (`t15_*`, `inv49_*`, `t3_9_*`), `actions_wanted.rs` |
| F11 | **fixed** | `consolidate.rs::revalidate_pass` turns an `Err` of `revalidate_action` (store read, blocking task) into a `ScanIncomplete` blocker, so pass 2 still runs the compensating restore and the plan is `Invalidated` instead of staying `Executing`. 14 should still call `recover` after any `execute` `Err` (journal failure). | `::f11_a_store_read_error_after_the_stop_still_restores_the_untouched_torrent` |
| F12 | **fixed** | Added: crash after the `QbitStop` intent (stop never sent), after the `QbitStart` intent (start never sent), between items of a two-item plan, and after a `Failed` result (F01). `Hang::arm_nth` hangs at the n-th occurrence of a point. | `::f12_crash_after_the_stop_intent_before_the_stop_resolves_from_the_listing`, `::f12_crash_after_the_start_intent_before_the_start_rechecks_and_starts_at_recovery`, `::f12_crash_between_items_rechecks_the_done_item_and_resumes_the_untouched_one`, F01 test |
| F13 | **fixed** | `mutate.rs::temp_name`: `.<base[..64]>.removarr-<tag>-<seq>-<ns>` (base truncated on a char boundary, `TEMP_BASE_MAX = 64`). | `mutate_hardlink.rs::f13_long_names_get_a_temporary_name_within_name_max` (250-byte name) |
| F14 | **fixed (doc)** | `mutate.rs` doc of `replace_with_hardlink` (target re-checked before the rename, source proven by the temp link's inode); `domain/src/fs.rs::DirSnapshot` doc now says recursive. The domain edit is a doc comment only (one sentence), outside my ownership, recorded here for 03. | — |


### Tier-3 requirements (T3-0..T3-19): status and tests

| Req. | Status | Tests |
|---|---|---|
| T3-0 | done (F01-F04, F03 via pins, F10 via codes) | `actions_fixes.rs` (F01, F02, F04/F05, F08, F09, F11, F12), `actions_wanted.rs` |
| T3-1 | done (handle-based, no shell / `std::fs::remove_*`) | `fs/tests/mutate_remove.rs::remove_file_unlinks_only_an_unchanged_regular_file`, `t17_remove_tree_removes_the_planned_subtree_and_never_follows_symlinks` |
| T3-2 | done (runner and mutator refuse media; empty dir completed only) | `mutate_remove.rs::removals_never_touch_a_media_root`, `actions_delete.rs::revalidation_after_the_stop_restores_the_torrent_and_media_targets_are_refused` (`MediaRootDeletion`, no intent) |
| T3-3 | done (lstat + `matches(expect, true)`, ENOENT = StateChanged) | `mutate_remove.rs::remove_file_unlinks_only_an_unchanged_regular_file` |
| T3-4 | done (fresh snapshot equality, deepest first, per-entry (dev, ino), stop at mismatch, `FailedNeedsReview`) | `mutate_remove.rs::remove_tree_is_all_or_nothing_at_the_decision_point`, `::remove_tree_stops_at_the_first_mismatch_and_reports_what_was_removed`; `actions_delete.rs::t15_folder_deletion_is_all_or_nothing_and_samples_need_their_acknowledgement`, `::a_partially_removed_tree_needs_review_and_its_hashes_stay_stopped` |
| T3-5 | done (rmdir semantics, `dev/ino/uid/gid/mode` compared, `mtime/nlink` excluded; completed only) | `mutate_remove.rs::remove_empty_dir_needs_an_empty_unchanged_directory_under_completed`; `actions_delete.rs::inv49_empty_parents_are_removed_after_their_contents_and_only_when_empty` |
| T3-6 | done (target and every tree entry; approval must name the exact sample) | `actions_delete.rs::t15_*` (acknowledged, unacknowledged -> `Sample`, plan `Invalidated`, no intent) |
| T3-7 | done (full ownership index, inode view bound, unreachable qBittorrent blocks) | `actions_delete.rs::t3_7_junk_is_deleted_only_when_no_torrent_references_it_by_path_or_inode` (by path, by inode, qBittorrent down) |
| T3-8 | done (live holder set vs plan, pins, stop, pass 2, then unlink) | `actions_delete.rs::t3_8_a_torrent_that_started_holding_a_torrent_held_target_refuses_before_the_stop`, `revalidation_after_the_stop_restores_the_torrent_*`, `t32_*`; `actions_wanted.rs` |
| T3-9 | done (all wanted files absent via fresh `files()` + lstat, journaled `QbitRemoveKeepData`, `TorrentAbsent`; partial -> left stopped) | `actions_delete.rs::t32_a_torrent_held_file_is_deleted_then_its_task_removed_without_delete_files`, `::t3_9_a_task_is_kept_while_any_wanted_file_remains`, `::t23_a_torrent_that_lost_a_file_without_task_removal_is_left_stopped_and_reported` |
| T3-10 | done (manual only, incomplete root only, stop, remove task, fresh listing, re-stat, delete) | `actions_stalled.rs::t3_10_stalled_cleanup_stops_removes_the_task_then_deletes_under_incomplete_only`, `::stalled_cleanup_refuses_busy_complete_or_misplaced_torrents_before_any_mutation`, `::a_failed_task_removal_restores_the_torrent_and_deletes_nothing`, `::data_referenced_after_the_task_removal_is_left_as_an_orphan` |
| T3-11 | done (`DeleteIncompleteOrphan` through the same ownership check) | `actions_delete.rs::t30_*` (`lost.mkv.!qB`), live-holder tests |
| T3-12 | **open** (needs journal lookup of failed consolidations for automatic plans; automatic execution is tier 5, `NotInBuild`) | - |
| T3-13 | done (every new step through `Ctx::step`; counting mutators observe all) | journal assertions in `actions_delete.rs::t32_*`, zero-mutation asserts throughout |
| T3-14 | done (`Absent` only on confirmed NotFound, `TorrentAbsent` on fresh listing, `ModeEquals` via stat, gains only for `Verified` and fully unlinked inode) | `actions_delete.rs::t32_*` (attributable gain = size), `actions_stalled.rs::t3_18_*` |
| T3-15 | done (recovery observes disk/listing; possible partial deletion -> modified, `FailedNeedsReview`; never redone) | `actions_tier3_crash.rs` (all) |
| T3-16 | done (simulation of every tier-3 kind, recording and live-process guards incl. fs, qBittorrent `remove`, ARR, Jellyfin) | `actions_delete.rs::t30_simulating_every_tier3_kind_walks_the_full_path_and_mutates_nothing` |
| T3-17 | done | `actions_tier3_crash.rs::crash_after_the_remove_file_intent_before_the_unlink_resumes_the_torrent`, `::crash_after_the_unlink_before_the_result_rechecks_and_leaves_the_torrent_stopped`, `::crash_around_the_task_removal_is_resolved_from_the_listing`, `::stalled_crash_after_the_task_removal_never_deletes_the_data_at_recovery`, `::a_partial_tree_removal_followed_by_a_crash_is_reviewed_at_recovery`, `::a_failed_result_followed_by_a_crash_is_re_observed_at_recovery`, `::fix_mode_crash_after_the_chmod_is_observed_from_the_mode`; the recheck-boundary and before-intent cases reuse the F12 / `actions_crash.rs` seams |
| T3-18 | done (owner only, non-recursive, signature, `ModeEquals`) | `actions_stalled.rs::t3_18_fix_mode_is_targeted_revalidated_and_verified`, `mutate_remove.rs::set_mode_is_targeted_verified_and_refuses_symlinks_or_changed_entries` |
| T3-19 | contract for 14, unchanged: exclusive `lock_roots`, `recover` before every execute and after any `execute` `Err`, no execute while recovery is unresolved; left-stopped is durable in the journal (F09) | - |
| tier-2 services | `ArrRescan`, `JellyfinRefresh` journaled, completion observed by polling bounded by `recheck_timeout`, judged by `evaluate_service_step` | `actions_stalled.rs::service_commands_are_journaled_and_verified_by_observing_the_service` |

### Stage B deviations and notes

* Domain edit outside ownership: one doc comment on `DirSnapshot` (F14); domain patch 3 (pins,
  `ChangedSincePlanning`, `Interrupted`) is owned by 03.
* `FixMode` is exempt from the live-holder check: a mode change alters neither bytes nor paths,
  and the common target is a seeded file (the planner adds owner bits only).
* A deletion batch whose every action is refused before revalidation (sample not acknowledged,
  media root, wrong root kind) ends the plan `Invalidated`, nothing journaled.
* `check_live_holders` reads `files()` only for torrents whose base paths are related to an
  action path; a mapping fault on any torrent refuses (fail closed).
* A simulation refused by revalidation reports `NotExecuted` with the live blockers; one that
  would run reports `Simulated`. A simulation accepts samples the plan itself declares.
* Open: T3-12 (failed-consolidation exclusion for automatic plans, tier 5); T3-19 is 14's work.

### Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 682 passed, 0 failed, 1 ignored (opt-in ffprobe).
* No `deleteFiles=true` anywhere; no shell or `Command` in the runner or mutator.


## Stage C (CP2-tier4 fixes + tier 4)

Review: `docs/reviews/cp2-tier4.md` (PASS with one condition, F01). Step 1 (fixes F01-F07) and
step 2 (tier 4) are done; `IMPLEMENTED_TIER = T4`. Automatic plans are refused by the runner
(tier 5). Nothing was committed; no real storage or service was touched.

New modules: `actions/replace.rs` (`ReplaceRelease` + `DeleteMediaFile`), `actions/arrcmd.rs`
(grab, queue removal, monitoring). New families `Family::Replace` and `Family::ArrCommand`
(a plan never mixes families). Domain edit (additive, outside my ownership):
`FsMutator::remove_media_file` with a refusing default (the dedicated, separately reviewed
media-root entry point of T4-13); `LiveFsMutator` implements it, `remove_file` still refuses
media roots. `ActionSettings` gained `capabilities` (callers that build it: only tests today).

### Per-finding status

| ID | Status | What changed | Test |
|---|---|---|---|
| F01 | fixed | `run.rs` gates on `plans::gate::action_tier(kind, origin)` (an `AutoPolicy` plan is T5) live and simulated, before any intent: `NotInBuild`, plan `Invalidated` live. The auto-stop branches in `consolidate.rs`/`delete.rs` stay for tier 5. `actions_consolidate.rs::an_automatic_plan_stops_...` now covers the manual continuation only. | `actions_stage_c.rs::f01_automatic_plans_are_refused_before_any_intent_live_and_simulated` (approved automatic deletion and consolidation, zero intents, zero fs/qBittorrent/service mutations), `actions_consolidate.rs::tier_five_automatic_plans_are_explicit_refusals` |
| F02 | fixed | `Ctx::remove_tasks_observed`: an error with every hash gone from a fresh listing is a removal that happened (journaled `Done`, `tx.removed`). Same rule for `manual_import`, grab, queue removal and monitoring (T4-20). | `actions_stage_c.rs::f02_a_task_removal_that_happened_despite_a_transport_error_is_observed` (stalled and torrent-held paths; fake `remove_then_fail`) |
| F03 | fixed | `stop_torrents` refuses (nothing stopped, `Failed`) when a hash is missing from the fresh listing; the stalled path no longer defaults an unreadable listing to empty. | `::f03_a_listing_fault_before_the_stop_never_leaves_a_torrent_stopped_and_unreported` (fault injected at every listing call position) |
| F04 | fixed | Mode must keep every current bit and may add only `0o200` (files) / `0o300` (directories): enforced by the runner (`ProtectedZone`, plan `Invalidated`, no intent) and by `LiveFsMutator::set_mode` (`Validation`). The tier-3 test that applied `0o750` now first sets a compatible mode. | `::f04_a_mode_that_removes_bits_or_adds_foreign_ones_is_refused_before_any_intent`, `fs/tests/mutate_remove.rs::set_mode_is_targeted_verified_and_refuses_symlinks_or_changed_entries` |
| F05 | fixed | `run.rs::mandatory_defects`: plan config fingerprint must equal `settings.config_fingerprint`, the kind's capability must be in `ActionSettings.capabilities`, every `torrent_hashes` entry needs its `TorrentWantedFiles` pin; otherwise `NotExecuted` before any intent. | `::f05_missing_pins_a_changed_config_and_a_disabled_capability_refuse_the_plan`, `actions_tier4.rs::t4_01_*` |
| F06 | fixed | Both leftover-link notes asserted (replace intent and probe intent crashes). | `actions_crash.rs::f06_leftover_temporary_links_are_named_in_the_recovery_notes` |
| F07 | **justified deviation** | `check_live_holders` keeps its narrower reach (torrents whose base paths are near an action path, inventories of the action's own roots). It is data-safe for tier-3 deletions (`compare_nlink = true`). Tier 4 does not rely on it for inode accounting: `NoTorrentReferences`/`OldCoverageRecoverable`/`OldFileAccounted` use the full view and fresh `stat`. | `actions_tier4.rs::t4_07_*` |

### Per-requirement status (T4-01..T4-20)

| Req. | Status | Tests (`crates/workflows/tests/actions_tier4.rs` unless noted) |
|---|---|---|
| T4-01 gates | done: origin-aware tier (F01), runner capability check (F05), `Replace` family, no mixed families | `t4_01_gates_capability_families_and_missing_preconditions_refuse_before_any_intent`, `actions_stage_c.rs::f01_*` |
| T4-02 `importMode` copy | done: the runner builds no ARR body (`ManualImportRequest` has no mode); the single literal stays in `arr/src/commands.rs`; a move is detected | `t4_02_t4_05_an_import_that_moves_the_source_needs_review_and_cancels_the_rest` |
| T4-03 mandatory preconditions + preview | done: `ArrFingerprint`, `OldCoverageRecoverable`, `NoArrImport`, per-source `EntryUnchanged`/`SameDevice` must be present; preview re-read right before the command (every candidate still approved with the planned episodes); score/profile/settings change invalidates | `t4_01_*`, `t4_03_a_changed_score_profile_or_hardlink_setting_invalidates_the_plan`, `t4_13_a_deletion_that_does_not_follow_a_verified_import_is_refused` (rejected candidate) |
| T4-04 one importer | done: `NoArrImport` in both passes and a dedicated queue read immediately before `manual_import` | `t4_04_one_importer_per_media_checked_again_right_before_the_command` |
| T4-05 source inode | done: re-`stat` after the command; moved -> `FailedNeedsReview`, `PathStillHasInode` fails with "ArrImportMoved", dependants `NotExecuted` with `ArrImportMoved`, hashes modified -> recheck -> left stopped | `t4_02_t4_05_*`, crash test with `Move` |
| T4-06 verified per media | done: (a) ARR's fresh current file is a new library file not in the old coverage, (b) source keeps `(dev, ino)`, (c) library file shares it (synthetic `InodeEquals`), command completion observed (`ArrCommandCompleted`); (a)+(b) without (c) is `ExecutedUnverified` + `AddedCost`; timeout is never success | `t4_06_a_hardlinked_import_*`, `t4_06_t4_08_a_copy_import_*`, `t4_06_a_command_that_does_not_complete_in_time_*` |
| T4-07 old coverage | done: per-file `acknowledged_unrecoverable` checked by the runner (forged approval refused), `OtherLink` re-checked after the import, `OldFileAccounted` per old file | `t4_07_unrecoverable_old_files_need_each_acknowledgement_and_other_links_keep_them` |
| T4-08 hardlinks disabled | done: copy import never `Verified`; `AddedCost` recorded once | `t4_06_t4_08_*` |
| T4-09 coverage | runner side: all episodes of the request must map to a new ARR file, one action per media (all or nothing); the proof itself is the planner's (`arrplans::coverage`). No runner test with several episodes (open) | - |
| T4-10 numbering | done at execution (preview episodes must equal the plan); planning side is 12's | `t4_10_a_numbering_difference_in_the_execution_time_preview_refuses` |
| T4-11 ffprobe blockers | planning-only (12/matching); the runner re-checks nothing from ffprobe by design | - |
| T4-12 grab | done: fresh search must contain the token (`ReleaseNotInSearch`), adapter refusal mapped to the same blocker, never re-sent, verified by a new queue item within the timeout, search never grabs; live-only because a dry-run process can only simulate | `t4_12_grab_is_manual_through_a_fresh_search_and_verified_by_the_queue`, crash tests |
| T4-13 `DeleteMediaFile` | done: depends on a `Verified` import of this run, never the new inode, never ARR's current file, `nlink` unchanged, `remove_media_file` only; an already removed file (ARR's upgrade) is `NotExecuted`, never "success" | `t4_13_*`, `fs/tests/mutate_remove.rs::t4_13_remove_media_file_is_the_only_media_removal_and_is_strictly_confined` |
| T4-14 old completed release | not in this family: it is a tier-3 deletion plan; the runner does not tie it to an import (open for 12 if wanted) | - |
| T4-15 `AddedCost` and net | done: one `AddedCost` record per new inode (size as the allocated estimate), deletion gains only for `Verified` deletes | `t4_06_t4_08_*`, `t4_13_the_old_media_file_goes_only_after_*` |
| T4-16 queue and monitoring | done: queue removal only for a non-importing item whose download is not a live torrent (`QueueItemHidesError`, `ImportInProgress`), previous monitored value journaled, both judged by a re-read | `t4_16_queue_removal_only_for_a_proven_stale_item_and_monitoring_journals_the_previous_value` |
| T4-17 recovery | done for `ArrManualImport` (sources re-`stat`ed; moved -> `FailedNeedsReview`, torrents stay stopped), `ArrGrab` (possibly executed, never re-sent), `ArrRemoveQueueItem` (queue), `ArrSetMonitored` (flag vs journaled previous), `FsRemoveFile` on media (disk); the unreachable fallback arm was removed (every `JournalStep` is handled) | `crash_boundaries_around_the_import_*`, `crash_boundaries_around_the_old_media_deletion_*`, `crash_boundaries_around_grab_queue_removal_and_monitoring_*` |
| T4-18 tests | done except the open items above; fake ARR is a port-level scriptable fake with real fs effects (hardlink, copy, move, recycle bin, hardlink setting), not the testkit HTTP server (deviation below) | all of the above |
| T4-19 Jellyfin | informational only: the runner never lets Jellyfin decide `Verified` | - |
| T4-20 F02-F05 | done with the findings | see above |

### Stage C deviations and notes

* The tier-4 tests use `actions_common/arrfake.rs`, a port-level fake that models the V10 import
  effects (hardlink, copy, move), recycle bin and hardlink setting on real temporary directories.
  The testkit HTTP fake needs the real `removarr-arr` connector, path mapping and a bridge; the
  connector keeps its own tests. An integration test through the HTTP fake is for 22 (`integration-tests`; CP2T5-F06).
* `AddedCost` bytes are the new file's size (the signature has no allocated size).
* `FsRemoveFile` is the journal step of `DeleteMediaFile` (no new step variant); the intent
  target is the media path.
* A plan whose `ReplaceRelease` or companion is refused before the stop is void as a whole
  (`Invalidated`), because a replacement is global (INV-35).
* Open: T4-09 multi-episode runner test, T4-14, T3-12 (tier 5), T3-19 (14).

### Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 706 passed, 0 failed, 1 ignored (opt-in ffprobe).
* One `"importMode": "copy"` literal, one `("deleteFiles", "false")` literal; no `Command` in the runner or mutator.


## Stage D (CP2-tier5 fixes + runner-side tier 5)

Review: `docs/reviews/cp2-tier5.md` (PASS with one condition). `IMPLEMENTED_TIER` stays **T4**:
the production `execute` still refuses every `AutoPolicy` plan before any intent. The runner-side
tier-5 code is complete and tested but unreachable for real automatic plans. Nothing committed.

**Test-only gate mechanism.** `execute_with_gate(.., gate)` and `automatic_defects_for_test`
exist only under the cargo feature `tier5-test-gate` of `removarr-workflows`. The feature is
enabled solely by the crate's own dev-dependency (`removarr-workflows = { path = ".", features =
["tier5-test-gate"] }`): `cargo tree -p removarr -e normal,features` shows no `tier5-test-gate`,
so no production build contains the entry points. `execute` is `execute_gated(.., IMPLEMENTED_TIER)`.

New: `actions/auto.rs` (automatic checks), `ActionSettings.auto_min_age_hours`,
`ActionPorts.ignores`. One-word edit outside my ownership: `plans/mod.rs` `mod build` ->
`pub(crate) mod build`, so the runner shares `build::pins_torrents` (F01) instead of copying it.

### Per-finding status

| ID | Status | What changed | Test (`actions_stage_d.rs` unless noted) |
|---|---|---|---|
| F01 | fixed | `mandatory_defects` filters on `plans::build::pins_torrents(kind)`: queue removals need no wanted-file pin. The queue-removal test is hand-built with the planner's shape (download id in `torrent_hashes`, no pin): the planner needs a full `arr_queue` issue to build one, which is for the integration suite (22). | `f01_a_queue_removal_naming_a_download_id_needs_no_pin_but_a_torrent_action_does` |
| F02 | fixed | After any halted import (a moved source included) every `DeleteMediaFile` is refused, even behind another media's verified import. | `f02_after_a_moved_source_no_old_media_file_is_deleted_not_even_for_another_media` |
| F03 | fixed | `command_may_have_run(class)`: a transport/internal error on `manual_import` or grab with an unchanged fingerprint/queue is recorded as `Done {state: "unknown"}` (import: hashes marked modified -> recheck), outcome `ExecutedUnverified`; only definitive refusals (config, validation, capability, stale token...) stay `Failed`. Recovery: `Failed` with a may-have-run class is "possibly executed", and the import arm re-reads ARR's fingerprint. | `f03_an_import_or_grab_that_errors_is_possibly_executed_never_untouched_failed` (import, definitive refusal, grab, recovery of a transport-failed result) |
| F04 | fixed | Queue removal re-proves INV-39 at execution: item names media and download id, the torrent's wanted completed files each share an inode with ARR's fresh current file, no scan/permission finding at them; no shortcut for a missing id or unreadable torrent; the "live torrent" refusal is dropped (the canonical stale case is a seeding cross-seed). | `actions_tier4.rs::t4_16_*` (rewritten), crash and simulation tests use `Lab::t4_proven_queue` |
| F05 | fixed | `delete_old` runs `revalidate_fs_only` (holders, scan errors, signature with `nlink`) right before the intent; `media_delete_item` refuses samples. | `f05_old_media_deletions_are_revalidated_right_before_they_run_and_samples_are_refused` |
| F06 | doc | HTTP-level checks (`fake_arr` rejects non-copy, move test, loopback simulation guard) belong to owner **22**. `docs/PROGRESS.md` has no row to edit (grep found none). | - |
| F07 | doc | `AddedCost` bytes are the logical size, an **estimate** (the signature has no allocated size); reporting must label it. | - |
| F08 | fixed | Multi-episode replacement: all hardlinked verifies and frees both old files; one copied -> `ExecutedUnverified`, no deletion; one moved -> `FailedNeedsReview`, deletions cancelled, torrent stopped. The ARR fake now upgrades away only the files of the episodes it imports. | `t4_09_a_season_replacement_is_all_or_nothing_per_unit` |

### Runner-side tier-5 requirements owned by 13 (`actions_tier5.rs`)

| Req. | Status | Test |
|---|---|---|
| T5-01 gate + kind allowlist | production gate unchanged (T4); the runner refuses every kind outside `is_auto_eligible` in an `AutoPolicy` plan on its own | `t5_01_production_gate_still_refuses_every_automatic_plan_and_the_raised_gate_runs_them`, `t5_18_no_manual_only_kind_runs_in_an_automatic_plan` (every non-eligible kind, on an unsealed copy through `automatic_defects_for_test`) |
| T5-03 approval | only an `Approver::AutoPolicy` approval of exactly the plan hash; `approve_auto` re-run at execution (expiry, evidence, samples, unrecoverable) | `t5_03_t5_04_t5_13_approval_evidence_and_samples_are_checked_by_the_runner` |
| T5-04 evidence | deletions need `ArrValidatedReplacement`, consolidations `NameSize`/`Sampled`/`Full`; `RemoveTorrentTask` needs the `AutoRemoveEmptyTorrents` capability (A8) in the runner | same test, `t5_05_*` |
| T5-05 CP1-F02 at execution | ARR's fresh current file is the pinned replacement (Mapped, regular, planned size), shares no inode with a target, and the re-read preview still rejects each deleted completed file; ARR down fails closed | `t5_05_the_arr_replacement_and_a_re_read_preview_are_checked_at_execution` |
| T5-06 minimum age | every pinned entry older than `auto_min_age_hours` at execution | `t5_06_a_recent_entry_is_never_executed` |
| T5-07 perimeter | latest session `Complete`, every touched root `is_clean()` (not only around the path), qBittorrent reachable | `t5_07_the_whole_root_perimeter_is_checked_not_only_around_the_path` |
| T5-08 stop at first uncertainty (runner level) | an automatic batch stops after a failed step, a partial removal or a step that errored (new) | `t5_08_an_automatic_batch_stops_at_the_first_uncertainty` |
| T5-09 ignore list | re-read from `ActionPorts.ignores` at execution (subjects: rooted paths of every pinned entry, the consolidation pair, the torrent); unavailable -> refused | `t5_09_the_ignore_list_is_re_read_at_execution` |
| T5-13 samples | refused (`approve_auto` and the runner) | `t5_13_samples_are_never_automatic` |
| T5-15 guards | automatic simulations on the recording set and in a live process: zero fs/qBittorrent/ARR/Jellyfin calls; every refusal asserts zero mutations | `t5_15_automatic_simulations_mutate_nothing_in_either_process`, `untouched()` in every test |
| T5-21 unknown outcome | ARR commands: F03; qBittorrent stop/start/recheck were already journaled with observed results and left-stopped reporting | `f03_*`, tier-3 tests |

Not mine (T5-02, T5-10, T5-11, T5-12, T5-14 runner part is unchanged and covered by tier 3, T5-16, T5-17 cycle parts, T5-19).

### What 12 and 14 still owe for tier 5

* **12 (plan-engine):** tighten `approve_auto` (or the request) so that deletions require `ArrValidatedReplacement` and companions are approvable only with their parents (T5-04); per-action subject keys if the ignore re-check should not infer them from paths; an executed planner-built `ArrRemoveQueueItem` integration case (F01, with 22).
* **14 (job-runner):** `auto_mode` read once, `auto_paused` persistence and `POST /auto/resume` (T5-02); `AutoContext.failed_paths`/`failed_subjects` from the journal (T5-10, T3-12); the two-phase cycle with rescan only of mutated roots, abort on a dirty rescan, dry-run cycle simulated (T5-11); scheduling, exclusive locks, `recover` before every execute and after any `Err`, cancellation bridge (T5-12, T3-19); stop the cycle and set `auto_paused` on any outcome other than `Verified`/`NotExecuted`-by-revalidation, any left-stopped torrent, any recovery need (T5-08); crash points in the cycle (T5-16); fill `ActionSettings.capabilities`, `auto_min_age_hours`, `ActionPorts.ignores`; reporting of `Approver::AutoPolicy` (T5-19).
* **22:** HTTP-level import-mode/move checks and the loopback simulation guard (F06).

### Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 721 passed, 0 failed, 1 ignored (opt-in ffprobe).
* Note: an earlier wrapper in this stage swallowed compiler errors (`timeout` is not installed on macOS); every result above comes from the final plain runs.


## Gate raise to T5 (runner part)

Review: `docs/reviews/cp2-tier5-final.md`. `IMPLEMENTED_TIER = RiskTier::T5`
(`actions/mod.rs`); the production `execute` now accepts a valid automatic plan and refuses an
invalid one (wrong kind, missing evidence, ignored subject, too young, user approval, automatic
mode inactive). Automatic mode itself stays off by default: that is configuration and the job
runner. Every automatic-only check of stage D is kept. Nothing committed; the gate raise lands in
one commit with job-runner's fixes.

| Finding | Status | What changed | Test (`actions_tier5.rs`) |
|---|---|---|---|
| F02 | fixed | `auto_live_checks` uses `plans::action_subjects` (holding torrents, `q:`/`m:` keys included); the local `subjects_of` is gone. | `cp2t5f_f02_f03_runner_uses_the_planners_subjects_and_age_set` |
| F03 | fixed | Age is checked on the pinned signatures whose path is in `plans::action_paths` (targets, folder entries), never on the replacement evidence pin. | same test (young replacement, old target: `Verified`) |
| F04 | fixed | The ARR preview is read once per folder and judged for video files only; sidecars (`.nfo .srt .sub .idx .ass .ssa .vtt .jpg .jpeg .png .webp .gif .tbn .txt`) are skipped (covered by the `DirUnchanged` pin, so one added after planning still invalidates); samples refuse (`Sample`); a video the preview does not reject refuses (`ArrNotUpgradeEvidence`); any other non-video file refuses (`ArrAmbiguous`). The video and sidecar lists are constants in `actions/auto.rs` (the runner has no `IssueSettings`). | `cp2t5f_f04_an_automatic_folder_with_sidecars_runs_and_an_unlisted_extra_video_refuses` (accepted folder removed with one `remove_tree`, extra video, file added after planning, unknown file) |
| F06 | fixed (runner part) | New `ActionSettings.auto_mode_active` (fails closed): a live `AutoPolicy` plan is refused with `CapabilityDisabled` while it is false, before any intent; simulations stay allowed. **Job-runner must set it true only for the automatic cycle while automatic mode is enabled and not paused** (and discard leftover approved plans / refuse `submit(ExecutePlan)` for `AutoPolicy`). | `cp2t5f_f06_an_approved_automatic_plan_never_runs_while_automatic_mode_is_inactive` |
| F01, F05, F07, F08 | job-runner / plan-engine / integration owners | - | - |

Rewritten tests: `actions_consolidate.rs::tier_five_is_enabled_and_an_automatic_plan_needs_every_automatic_check`,
`actions_stage_c.rs::f01_an_automatic_plan_failing_an_automatic_check_is_refused_before_any_intent`
(live: user approval refused; simulated: missing evidence / too young; zero intents and mutations),
`actions_tier5.rs::t5_01_the_production_execute_accepts_a_valid_automatic_plan_and_refuses_invalid_ones`
(the other `t5_*` tests now call the production `execute`; dry-run zero-mutation assertions kept).

**`tier5-test-gate` is kept**: `crates/jobs` (a dev-dependency, `real_runner.rs`) still calls
`execute_with_gate`. Here it is used only to prove a lower gate (T4) still refuses automatic plans;
`automatic_defects_for_test` feeds unsealed plans (T5-18). `cargo tree -p removarr -e normal,features
| grep tier5` prints nothing. The feature can be removed once job-runner drops `execute_with_gate`.

Doc comments updated: `actions/mod.rs` header, `actions/auto.rs` header, `execute_with_gate`.
Wiring owed by 16/14: `ActionSettings.auto_mode_active`, `capabilities`, `auto_min_age_hours`,
`ActionPorts.ignores` (all fail closed when missing).

## Fix after wave 15 (queue item proof)

Defect (handoff 22, defect 1): `queue_item_proof` required every `QbitFile::candidates` path (including `<name>.!qB` and alternative save/download paths) to share an inode with ARR's file, so a proven stale queue entry was always refused with `QueueItemHidesError`.

Fix in `crates/workflows/src/actions/arrcmd.rs` (INV-39 re-proof unchanged otherwise, still fail closed):
- Only wanted video files are import sources (same rule as the issue engine; `actions/auto.rs::is_video` made `pub(crate)`). Sidecars such as the `.nfo` no longer need to share an inode.
- For each such file the proof stats its candidates inside a completed root and resolves the one that exists as a complete regular file; its (dev, ino) must be among ARR's fresh current files, and no scan or permission finding may lie at it.
- Refused: a stat fault, no existing candidate, a `.!qB` partial that exists (even next to a complete file: ambiguous), several existing candidates with different (dev, ino), an existing non-regular entry, no wanted video, and every previous refusal (no media or download id, unreadable torrent or ARR files).
- Tests: `actions_tier4.rs::f_queue_proof_resolves_the_complete_candidate_and_fails_closed` (accepted with absent `.!qB`, accepted with a missing alternative candidate, only partial, nothing on disk, partial plus complete, disagreeing candidates); helper `Lab::qbit.set_candidates`. Integration: `pipelines_arr -- a_proven_stale_queue_entry` un-ignored and passing.
- Open: the integration test `today_the_queue_removal_is_refused_by_the_runner_without_any_mutation` pinned the defect and now fails by design; the integration-tests owner must delete it (I was limited to the `#[ignore]` attribute of the fixed test).
