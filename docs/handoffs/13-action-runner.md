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
