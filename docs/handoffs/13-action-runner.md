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

Review: `docs/reviews/cp2-tier3.md` (PASS with condition). Step 1 (the fixes) is complete and
tested. **Step 2 (tier 3) was not started**: F03 cannot be fixed without a domain patch (details
below), and the coordinator's instruction for that case is to stop after step 1 and report.
`IMPLEMENTED_TIER` stays `T2`; no `remove_*` / `remove_tasks_keep_data` path is reachable live
(the mutator still refuses them, `supported()` still admits only `ConsolidateHardlink`).

### Per-finding status

| ID | Status | What changed | Test |
|---|---|---|---|
| F01 | **fixed** | (a) The live replace step records what the disk shows, not only what the call returned: `consolidate.rs` writes `Done { final_state: ReplaceObserved { replaced, phys, error } }` when the target carries the source inode after a failed call, `Failed (target unchanged)` or `Failed (target could not be observed)` otherwise, and the hashes are marked modified whenever the mutation happened or cannot be ruled out. (b) `recovery.rs` re-observes the target of every `FsReplaceWithHardlink` intent whose result is `Failed` or missing; only `Skipped`/`Simulated` rule the mutation out. Observed replaced -> modified, `ExecutedUnverified`; unobservable -> modified, `FailedNeedsReview`; untouched -> `NotExecuted`. Any other mutation step (none reachable yet) is treated as possibly executed unless ruled out. | `actions_fixes.rs::f01_a_failed_but_mutated_replace_result_is_rechecked_at_recovery_in_a_two_item_plan` (journal seam stores the `Done` as `Failed`; crash before item b's gate; a is rechecked and `ExecutedUnverified`, b `NotExecuted`, nothing redone), `::f01_an_unobservable_target_stays_possibly_modified_across_a_recovery_retry` (target directory mode 000; first pass writes `Failed` then fails on qBittorrent; the retry rechecks, `FailedNeedsReview`, starts only because the recheck verified) |
| F02 | **fixed** | `revalidate.rs::check_live_holders`: every pass takes the fresh listing, reads `files()` for every torrent whose `save_path`/`download_path`/`content_path` (or their parent) is related to an action path, builds `issues::TorrentIndex`, binds the stored inventories of the action roots (inode view), and refuses with `TorrentReferencesTarget` when `hashes_under(path)` or `hashes_for_inode(stat(path))` contains a hash outside `action.torrent_hashes` (own hash excepted). Any `NoRule`/`Invalid` on a listed torrent's base paths or file candidates refuses with `MappingError`; qBittorrent unreachable refuses. | `::f02_a_torrent_that_started_holding_a_path_after_planning_invalidates_the_plan` (cross-seed of the completed path, and a library seeder of the media path: `Invalidated`, nothing stopped), `::f02_a_holder_by_inode_only_and_a_mapping_fault_near_the_paths_both_refuse` |
| F03 | **not fixable here, blocked (domain patch needed)** | The plan has no slot for the planning-time wanted set (`Precondition::TorrentStates { states: BTreeMap<hash, (state, progress)> }`), and storage keeps a single torrent snapshot (`torrent_snapshot` row `id = 1`, `torrents` upsert-if-changed, no history), so neither the plan nor any port can give the runner the pinned set; the current check (live vs latest stored) stays and is documented as weak. See "F03: exact change needed". | — |
| F04 | **fixed** | `run.rs::execute`: when `simulated`, the mutators are replaced by `Mutators::recording(&services)` before the `Ctx` exists; a simulation can no longer hold a live adapter. Module doc updated. | `::f04_f05_a_simulation_in_a_live_process_runs_on_the_recording_set_and_touches_no_service` (live counting fs/qBittorrent/ARR/Jellyfin adapters see zero calls through the swap; recording set with Sonarr and Radarr in a dry-run process sees zero calls) |
| F05 | **fixed** | The lab now wires a live counting `ArrCommands` (Sonarr) and `JellyfinCommands`, a reachable `ArrRead` and `JellyfinRead`, and `Mutators::recording(&[Sonarr, Radarr])`; `Lab::service_mutations()` is asserted zero in the guards. | same test as F04 |
| F06 | **fixed (doc)** | Module doc of `actions` and this handoff: stage A/B simulate consolidation only; every other kind is refused live and simulated. Tier-3 simulation is a tier-3 deliverable (T3-16). | `tiers_three_to_five_are_explicit_refusals` (unchanged) |
| F07 | **fixed (report)** | Recovery notes name both leftovers: `.<name>.removarr-probe-*` for an unfinished probe or a probe whose unlink failed (result message "removing the probe link"), `.<name>.removarr-link-*` for a replace intent interrupted before the rename. Cleanup needs tier-3 `remove_file` (open). | covered indirectly by `crash_after_the_replace_intent_before_the_mutation_is_resolved_as_not_executed` (note emitted); no dedicated assertion |
| F08 | **fixed** | Direction enforced at three layers: planner (12), runner `consolidate::item()` (`ProtectedZone`, no intent), mutator `check_direction` in `probe_hardlink` and `replace_with_hardlink` (`Confinement`). | `fs/tests/mutate_hardlink.rs::f08_the_mutator_refuses_any_direction_but_completed_to_media`, `actions_fixes.rs::f08_the_runner_refuses_a_plan_whose_direction_is_not_completed_to_media` |
| F09 | **fixed** | `qbit.rs::restore` always journals a `QbitStart` intent when the transaction touched anything: target = start set (possibly empty), observed = `RestoreObserved { recheck, left_stopped }`, result `Done { started }`. The left-stopped map and reasons are therefore durable in the journal (reporting 15 reads it from `journal_intents.observed`). | `::f09_hashes_left_stopped_and_their_reason_are_journaled_even_when_nothing_starts` |
| F10 | **deviation (domain patch needed)** | Reused codes stay: `ScanIncomplete` for "changed since planning"/config change/store read failure, `FailedConsolidation` for "interrupted / not reached". See "F10: exact change needed". Nothing tier-3 reaches reports yet. | — |
| F11 | **fixed** | `consolidate.rs::revalidate_pass` turns an `Err` of `revalidate_action` (store read, blocking task) into a `ScanIncomplete` blocker, so pass 2 still runs the compensating restore and the plan is `Invalidated` instead of staying `Executing`. 14 should still call `recover` after any `execute` `Err` (journal failure). | `::f11_a_store_read_error_after_the_stop_still_restores_the_untouched_torrent` |
| F12 | **fixed** | Added: crash after the `QbitStop` intent (stop never sent), after the `QbitStart` intent (start never sent), between items of a two-item plan, and after a `Failed` result (F01). `Hang::arm_nth` hangs at the n-th occurrence of a point. | `::f12_crash_after_the_stop_intent_before_the_stop_resolves_from_the_listing`, `::f12_crash_after_the_start_intent_before_the_start_rechecks_and_starts_at_recovery`, `::f12_crash_between_items_rechecks_the_done_item_and_resumes_the_untouched_one`, F01 test |
| F13 | **fixed** | `mutate.rs::temp_name`: `.<base[..64]>.removarr-<tag>-<seq>-<ns>` (base truncated on a char boundary, `TEMP_BASE_MAX = 64`). | `mutate_hardlink.rs::f13_long_names_get_a_temporary_name_within_name_max` (250-byte name) |
| F14 | **fixed (doc)** | `mutate.rs` doc of `replace_with_hardlink` (target re-checked before the rename, source proven by the temp link's inode); `domain/src/fs.rs::DirSnapshot` doc now says recursive. The domain edit is a doc comment only (one sentence), outside my ownership, recorded here for 03. | — |

### F03: exact change needed (domain patch 3, then plan-engine)

Goal: the runner must compare the live wanted-file set of every affected hash with the set
**pinned at planning time**. Nothing the runner can read today holds that set.

Proposed additive domain change (hash-stable for existing plans: a new variant, no field change):

```rust
// crates/domain/src/plan.rs
pub enum Precondition {
    ...
    /// Wanted files (`priority > 0`) of an affected torrent at planning time, as
    /// `(index, name)` pairs; the runner compares the live `QbitRead::files` set with it.
    TorrentWantedFiles { hash: TorrentHash, wanted: BTreeSet<(u32, String)> },
}
```

Plan-engine (12) must emit one `TorrentWantedFiles` per hash of `action.torrent_hashes`
(from `Facts.snapshot.files[hash]`, in `Facts::torrent_states` or next to it) for every kind
that carries `TorrentStates`, including `CleanupStalledIncomplete` and `RemoveTorrentTask`;
`plans::expiry::check` should compare it with the stored snapshot (stale check at
approval/refresh). The runner side is then a ten-line change in `revalidate.rs::check_wanted_files`
(compare against the pin instead of `SnapshotStore::latest_torrents`), and the "latest stored
snapshot" fallback is removed. A digest (`sha256` of the canonical pair list) would also work
if plan size matters; the full set is preferred because it is what reporting shows.

The alternative (a per-session torrent snapshot in storage plus a `SnapshotStore::torrents_for_session(SessionId)`
port method and a retention rule "never prune the snapshot of a non-terminal plan's session")
touches the domain ports, the storage schema and retention, and keeps large history: not
recommended.

### F10: exact change needed (domain patch 3)

```rust
// crates/domain/src/issue.rs
pub enum BlockerCode {
    ...
    /// A revalidated precondition no longer holds (entry, folder, torrent, config, store).
    ChangedSincePlanning,
    /// The action was not reached because the batch stopped (cancellation, crash, abort).
    Interrupted,
}
```

The runner then replaces `ScanIncomplete` (revalidation) with `ChangedSincePlanning` and
`FailedConsolidation` (not reached / interrupted) with `Interrupted`; tier-5 policy (12 `auto`,
"failed subjects of the last 7 days") must count `Interrupted` and `FailedNeedsReview`
outcomes, not blocker codes.

### Tier-3 requirements (T3-0..T3-19): status

| Req. | Status |
|---|---|
| T3-0 | F01, F02, F04 fixed with tests; F03 blocked on the domain patch above. `IMPLEMENTED_TIER` not raised. |
| T3-1..T3-5 (mutator removals) | not started; `remove_*` and `set_mode` remain explicit refusals (`CapabilityDisabled`). Design ready: `remove_file` = `open_parent` + `lstat` (`compare_nlink = true`, symlink refused, `ENOENT` = `StateChanged`) + `unlinkat`; `remove_tree` = fresh recursive handle snapshot equal to the plan's (incl. `nlink`, `uid/gid`, `mode`, `mtime_ns`, same `dev`), then deepest-first `unlinkat` / `unlinkat(AT_REMOVEDIR)` with per-entry `(dev, ino)` re-check, stop at the first mismatch with the removed list in the result; `remove_empty_dir` = `rmdir` semantics with `dev/ino/uid/gid/mode` equality (`mtime_ns`/`nlink` excluded, rule to be stated in the doc and tested); media root refused in the mutator, completed-only for `remove_empty_dir`. |
| T3-6..T3-8 (samples, junk outside torrents, torrent-held deletions) | not started. F02's `check_live_holders` already provides the live holder set with the inode view; the sample re-check at execution against `Approval.acknowledged_samples` and the junk rule (no holder by path or inode) are to be added in the tier-3 batch. T3-8 depends on F03. |
| T3-9 (`RemoveTorrentTask`) | not started; depends on F03. Design: after the deletions, fresh `files()` + `lstat` of every wanted candidate, journaled `QbitRemoveKeepData`, verified by `TorrentAbsent` on a fresh listing; partially affected hashes rechecked then left stopped (`plan_restore` unchanged). |
| T3-10 (stalled incomplete cleanup), T3-11 (`DeleteIncompleteOrphan`) | not started. |
| T3-12 | depends on F10 codes. |
| T3-13..T3-15 (journal, verification, recovery for the new steps) | foundation in place: `Ctx::step` is the only mutation path; recovery already treats any unknown fs step as "possibly executed" (F01 rule) until its observation logic exists. |
| T3-16 (simulation of every tier-3 kind, guards incl. `remove`, ARR, Jellyfin) | guards extended (F05); kinds not started. |
| T3-17 (crash boundaries for the new steps) | not started; the crash lab (`Hang::arm_nth`, `HangJournal` seams) is ready. |
| T3-18 (`FixMode`) | not started. |
| T3-19 (job-runner contract) | unchanged request to 14: exclusive locks, `recover` before every execute job and after any `execute` `Err`, no execute while recovery is unresolved; left-stopped is now durable in the journal (F09). |

### Stage B deviations and notes

* Domain edit outside ownership: one doc comment on `DirSnapshot` (`crates/domain/src/fs.rs`),
  F14. No code or type changed; hashes unaffected.
* `check_live_holders` reads `files()` only for torrents whose base paths are related to an
  action path (ancestor, equal or descendant); every torrent with a mapping fault refuses
  regardless of proximity (fail closed).
* A simulation that is refused by revalidation reports `NotExecuted` with the live blockers;
  one that would run reports `Simulated` (unchanged from stage A).

### Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 647 passed, 0 failed, 1 ignored (opt-in ffprobe). New since
  stage A: `fs/tests/mutate_hardlink.rs` +2 (7), `workflows/tests/actions_fixes.rs` 11.
* Dependency gates: print nothing. `deleteFiles` once (`"false"`); `LiveMode::` only in config.
* Nothing committed; no real storage or service touched.
