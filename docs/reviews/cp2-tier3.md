# CP2-tier3 review: may risk tier 3 be built on top of tiers 1-2? (iteration 1)

Reviewer: independent `general-purpose` agent. I wrote none of the code under review.

Scope: `MASTER-PROMPT.md` §2, §5, §6, §10 (CP2 rule in §10.5); CONTEXTE §3.5, §3.7, §3.8, §3.11-§3.15;
`docs/architecture.md` §6, §7.4, §7.5, §9.3, §13, §20; ADR-0002, 0003, 0004, 0007; handoffs 04, 08, 12, 13 (stage A,
including its deviations); code in `crates/workflows/src/actions/**`, `crates/fs/src/{mutate,handle}.rs`,
`crates/qbit/src/control.rs` (plus the `http.rs`, `matching/src/proof.rs`, `domain` and `storage` code they rely on);
tests `crates/workflows/tests/actions_*.rs`, `actions_common/mod.rs` and `crates/fs/tests/mutate_hardlink.rs`.

Verdict: PASS

No finding is blocking for the tier-1/2 code as it stands. None of the defects found can lose or corrupt data in a
consolidation, because a consolidation never changes the bytes of torrent data. **Four majors** remain (F01-F04).
Each one is a gap in the foundation that tier 3 would inherit, and with deletions each one becomes a data-safety
hole. Four majors are within the pass rule ("more than a handful" fails).

**Condition attached to this PASS:** fix F01-F04 at the start of tier 3, each with a test, **before**
`IMPLEMENTED_TIER` is raised to `T3` and before any `remove_*` / `remove_tasks_keep_data` code path is reachable
live. The next CP2 review (before tier 4) must verify F01-F04 and the tier-3 requirements below.

---

## Test and gate evidence (run by the reviewer, macOS host, rustc 1.98)

| Command | Result |
|---|---|
| `cargo test --workspace` | **634 passed, 0 failed, 1 ignored** (84 test binaries including doc-tests; the ignored one is the opt-in `real_ffprobe_accepts_the_argument_set`) |
| `cargo test -p removarr-workflows --test actions_consolidate --test actions_crash` | 13/13 and 8/8 pass |
| `cargo test -p removarr-fs --test mutate_hardlink` | 5/5 pass |
| `cargo test -p removarr-workflows --lib` | 7/7 pass (includes `actions::qbit::tests::restore_starts_only_active_hashes_that_are_untouched_or_recheck_verified`) |
| `cargo test -p removarr-qbit` | pass (includes `remove_always_sends_delete_files_false`, `no_command_ever_carries_a_delete_files_true_flag`) |
| `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings` | pass / pass |
| `grep deleteFiles\|delete_files\|torrents/delete` in non-test sources | one literal `("deleteFiles", "false")` at `crates/qbit/src/control.rs:111`; the rest are comments and the test fake |
| `grep Command::new\|std::process` in non-test sources | only `crates/matching/src/ffprobe.rs:114` (argv vector, no shell, not reachable from `actions`) |
| `impl FsMutator/QbitControl/ArrCommands/JellyfinCommands` in non-test sources | the four recording adapters (`dry_run.rs`) and the four live adapters, each constructed with `&LiveMode` |

---

## Findings

| ID | Severity | Crit. | Evidence | Problem | Required change |
|---|---|---|---|---|---|
| CP2T3-F01 | major | 2 (recovery resumes only untouched torrents) | `actions/recovery.rs:155-157` (`Some(_) => false`: any recorded non-`Done` result counts as "not mutated"); `recovery.rs:174-181` (an unobservable target is written as `StepOutcome::Failed` but treated as mutated **only in this pass**); `consolidate.rs:454-460` with `journal.rs:142-147` (a live replace that returned `Err` after the rename is journaled `Failed`, even though `observe_replaced` proves the link is in place); architecture §7.5 bullet 2 ("a mutation intent that has no result, **or a result without a verified recheck**: recheck") | Recovery trusts the step's `Failed` result instead of re-observing the disk. Two concrete paths go wrong. (1) Item *a*'s replace fails after the rename (fsync error) and is journaled `Failed`. The process then dies while item *b* is being compared, before the recheck intent. Recovery treats *a* as not mutated: hash *a* is started without a recheck, and action *a* is reported `NotExecuted` ("the process stopped before this action") although the media file was replaced. (2) The first recovery pass meets an unobservable target, writes `Failed` (treated as mutated), then fails on qBittorrent `Unavailable`. The retry reads that `Failed` result as "not mutated" and starts the torrent without a recheck. The `recovery_retries_...` test covers only the `Done` case. For consolidation the torrent bytes are unchanged, so the impact is wrong reporting. With tier 3, a `Failed` `remove_tree` that deleted half a folder would be resumed and reported as not executed. | For every fs mutation intent, re-observe the target unless the result is a definitive "target unchanged" `Skipped`, and decide from what the disk shows. Treat its hashes as modified (recheck, start only if `Verified`) whenever the mutation happened or cannot be ruled out. The action outcome is `ExecutedUnverified`/`FailedNeedsReview`, never `NotExecuted`, when anything changed. Alternatively, make the live step record what it observed after a failed call (for example `Done { final_state: {"observed": "replaced", "error": ...} }`) so the journal itself says the mutation happened. Add tests: crash after a `Failed`-but-mutated replace result in a two-item plan, and the retry of an unobservable target. |
| CP2T3-F02 | major | 2 (fail closed on live services), 4 | `revalidate.rs:380-406` (`TorrentStates` checks only the planned hashes); `revalidate.rs:522-578` (wanted-set check only for `a.torrent_hashes`); `consolidate.rs:190-193` (stop set = planned hashes); `plans/fsplans.rs:328-339` (a torrent-held deletion target gets `TorrentStates`, never `NoTorrentReferences`); architecture §7.4 "Affected hashes (CP1-F09)"; CONTEXTE §3.8 "déterminer tous les hashes touchés" | The runner never recomputes the **live** set of hashes that hold the action's paths. A torrent added after planning (typically a cross-seed of the same release) is not detected: it is not stopped, rechecked or reported, and the plan is not invalidated. For a consolidation the bytes do not change, so this is benign. For a tier-3 deletion of a torrent-held file it is the exact failure MASTER §5.6/§5.7 forbids: deleting data under a seeding torrent nobody stopped. | In every revalidation pass, build `TorrentIndex` from the fresh listing and `files()` and compute `hashes_under(path)` for every action path. A live holder that is not in `action.torrent_hashes` invalidates the plan (`TorrentReferencesTarget`). Add a test where a new torrent starts referencing the target between planning and execution, for consolidation now and for every tier-3 kind. |
| CP2T3-F03 | major | 2 (live wanted-file set) | `revalidate.rs:539` (`self.ports.snapshots.latest_torrents()`), `revalidate.rs:555-561`; `SnapshotStore` has no per-session read (`domain/src/ports.rs:231-241`); handoff 12 follow-up 4 and handoff 13 "Domain gaps" | The wanted-file set is compared with the **latest stored** snapshot, not with the one the plan was built from. If a scan runs between planning and execution, it stores the changed selection, live then equals stored, and the change passes unnoticed. `plans::expiry` does not compare wanted sets either. The check therefore only works when no scan intervenes. For tier 3 this matters directly: `RemoveTorrentTask` (every wanted file absent) and the deletion of a torrent's files depend on the planned wanted set. | Pin the wanted set at planning time: the domain patch suggested by 12 (a wanted-set digest per hash in `TorrentStates`), or a `SnapshotStore` read of the plan's own `session` snapshot. Compare live against the pin. Test: change a file priority, run a scan, then execute; the plan must be refused. |
| CP2T3-F04 | major | 2 (simulate structurally unable to reach live mutators) | architecture §6 item 5 ("`simulate = true` **always runs with the recording set**"); `run.rs:97` and `run.rs:129-137` (the `Ctx` keeps whatever `Mutators` the caller passed); `actions_consolidate.rs` `simulation_in_a_live_process_never_invokes_the_live_adapters` (passes `lab.live()` to a simulation on purpose) | In a dry-run process the barrier is structural, because without `LiveMode` no live adapter exists. In a **live** process, a `simulate = true` run gets the live adapters. The only thing that keeps them uncalled is the runtime `if self.simulated` branch in `Ctx::step` (`journal.rs:132-136`), the `live_gate=false` choice (`consolidate.rs:362-364`) and `verify_action`. Any tier-3 helper that calls `self.mutators.fs.remove_tree(..)` outside `step()` would delete during a simulation. This deviation from §6 is not listed in the handoff. | In `execute` (and in any future entry point), when `simulated` is true, replace the mutators with `Mutators::recording(..)` before building the `Ctx`, so a simulation never holds a live adapter. Keep the guard test and assert that the live counting adapters see zero calls even through the swap. List the change in the handoff. |
| CP2T3-F05 | minor | 1 (zero-mutation guard) | `actions_consolidate.rs` t30 (`Mutators::recording(&[])`: no ARR recording adapter at all); `actions_common/mod.rs` `ports()` (`arr: vec![]`, `jellyfin: None`) and `live()` (`vec![], None`); architecture §6.8(b)/(e) | The guards observe fs and qBittorrent for real. They cannot observe ARR (no adapter exists) or Jellyfin in the live-process case (`None`). The "zero ARR/Jellyfin mutator calls" half of §6.8(e) is vacuous. Today nothing could call them, which is why this is minor. | Build the guards with `Mutators::recording(&[Sonarr, Radarr])` and live counting ARR/Jellyfin fakes, configured read ports, and assert zero calls. Do this before any step (stage B rescans, tier-3 post-deletion rescans) can reach them. |
| CP2T3-F06 | minor | 1 | handoff 13 "Scope" ("the complete execution pipeline in simulation **for every plan**") and `actions/mod.rs:25` vs `run.rs:147-186` (tier gate also applies to simulations) and the test `tiers_three_to_five_are_explicit_refusals` (asserts `NotExecuted` for a simulated tier-3 plan) | The documentation claims what the code does not do. Deletion plans cannot be simulated; the user gets `NotInBuild`. That is safe, but the tier-1 "full dry-run path" exists for consolidation only. | Correct the handoff and module doc. Tier 3 must deliver simulation of each tier-3 kind (requirement T3-16). |
| CP2T3-F07 | minor | 3 (no temp left) | `mutate.rs:184-214, 250-260` (cleans up on every error return); `recovery.rs:154-193` (replace intents: no leftover note) vs `recovery.rs:194-209` (probe intents get one); `mutate.rs:230-231` (a failed unlink of the probe link returns `Err` and leaves the link) | Error paths leave nothing behind. Two cases still leave a `.<name>.removarr-*` hardlink in the media directory: a crash between `linkat` and `renameat`, and an unlink failure of the probe link. Recovery reports only the probe case. | Report both cases in `RecoveredPlan.notes` (pattern `.<name>.removarr-link-*`). Once `remove_file` exists, offer a planned cleanup. |
| CP2T3-F08 | minor | 3 (direction completed -> media) | `plans/validate.rs:66-68` (the only check); `mutate.rs:239-270` and `consolidate.rs:70-90` (no root-kind check); `HashedPlan::seal` -> `ActionPlan::validate` does not check it | The direction is enforced by the planner only. A plan that skipped `check_plan` (any `HashedPlan::seal`, as the test lab does) could link media -> completed. The runner and the mutator would replace a torrent's file with a media inode. | Defense in depth: refuse in `item()` (or revalidation) and in `replace_with_hardlink` unless `source.root.kind() == Completed && target.root.kind() == Media` (`RootId::kind()` already exists). Add a test. |
| CP2T3-F09 | minor | 2 (left stopped and **visible**) | `qbit.rs:245-291` (the journal records only the start set and the recheck statuses); `run.rs:39-41` (`left_stopped` only in the in-memory `ExecutionReport`); architecture §7.4 step 7 / §7.5 ("left stopped record"), §12.2 `torrents_left_stopped` | Hashes left stopped, and the reason, are not durably recorded. When nothing is started, the journal holds only the `QbitStop` intent. Visibility after a restart depends on 14/15 persisting the report. | Journal the left-stopped map, for example as the `observed` payload of a `QbitStart` intent (or a result with an empty start set), or persist it through a store, so the dashboard count survives restarts. |
| CP2T3-F10 | minor | 2 | handoff 13 deviation 9; `revalidate.rs:46-51, 505-511` (`ScanIncomplete` for "changed since planning" and config change); `recovery.rs:55-62`, `run.rs:205-212` (`FailedConsolidation` for "interrupted / not reached", for any kind) | Reused codes mislabel tier-3 outcomes: an interrupted deletion would be reported as a failed consolidation. They also feed tier-5 policy (the "failed subjects/paths of the last 7 days" rule, CONTEXTE §3.7 "impliqué dans une réparation échouée") with the wrong meaning. | Domain patch with `ChangedSincePlanning` and `Interrupted` (or equivalent) before tier-3 outcomes reach reports, and at the latest before tier 5. |
| CP2T3-F11 | minor | 2 | `consolidate.rs:219-226` (`revalidate_pass(..).await?` after the stop); `revalidate.rs:349, 358, 539` (store reads use `?`) | A store read error in pass 2, after the torrents were stopped, aborts `execute` with `Err`. No compensating start runs, and the plan stays `Executing` until `recover` runs. This fails safe (stopped), but the torrents stay stopped until 14 recovers. | Either treat store read errors in pass 2 as blockers (so `restore` runs), or have 14 call `recover` after **every** `execute` `Err`, not only at startup. Record the decision in handoff 14. |
| CP2T3-F12 | minor | 1 (crash boundaries) | `actions_crash.rs` (8 tests) | The four required boundaries are covered for the replace step, plus during the recheck. Not covered: a crash inside the stop (`AfterIntent(QbitStop)`), inside the start (`AfterIntent(QbitStart)`), after a `Failed` result (F01), and between items of a multi-item plan. The code paths exist; nothing pins them. | Add the four tests (the F01 one fails today). |
| CP2T3-F13 | minor | 3 | `mutate.rs:146-161` (`.{name}.removarr-{tag}-{pid}-{seq}-{ns}` adds about 45 bytes) | Names longer than about 210 bytes (common for anime/scene releases) exceed `NAME_MAX`, giving `ENAMETOOLONG`. This fails safe but blocks those consolidations with an unexplained I/O error. | Use a short fixed-length temp name (for example `.removarr-<seq>-<ns>`), or truncate the base, and add a test with a 250-byte name. |
| CP2T3-F14 | minor | 3, 4 | `domain/src/fs.rs:143` ("a directory and its **direct** entries") vs `fs/src/lib.rs:155-195` (recursive, with a mount-boundary refusal); `mutate.rs:235-238` ("both signatures re-checked right before the rename") vs `mutate.rs:252-256` (only the target is re-checked; the source is proven by the temp link's inode) | Documentation does not match the code. Tier 3 relies on `DirSnapshot` being recursive. | Fix both doc comments. |

Count: 0 blocking, 4 major (F01-F04), 10 minor (F05-F14).

---

## Stage A deviations: safety review

| Deviation (handoff 13) | Assessment |
|---|---|
| 1. `Mutators::live(..)` has no `&LiveMode` | **Acceptable for dry-run processes.** All four live adapters take `&LiveMode` (`mutate.rs:65`, `control.rs:59`, `arr/src/commands.rs:43`, `jellyfin/src/commander.rs:32`). `LiveMode` has a private field (`config/src/live.rs:6`), and its only constructor is `Config::live_mode()` returning `None` in dry-run (`config/src/model.rs:224-225`). `for_tests()` is behind `test-support`, which is enabled only in `[dev-dependencies]` (arr, fs, jellyfin, qbit, workflows `Cargo.toml`), with `resolver = "3"`. A dry-run process therefore cannot hold a real mutator. Two residual weaknesses: `Mutators` has `pub` fields, so `mode: Live` can be asserted over recording adapters (no data risk, but plan statuses and live journal rows would be written for nothing), and live-process simulations hold live adapters (F04). |
| 2. Simulations do not call `record_outcome` | **Safe.** `ActionState::Simulated` is terminal and would block a later live run. Simulated intents are `JournalMode::Simulated` and are skipped by `unfinished()` (`storage/src/repos/journal.rs:179-185`) and by `recover_plan` (`recovery.rs:134-136`). The plan status is untouched (asserted by t30 and the simulation test). The caller (14/16) must call `record_simulation_run` for the audit trail. |
| 3. `ExecutingPlans` port | **Safe and needed.** It finds plans killed between a result and the next intent (`crash_before_the_replace_intent_resumes_the_untouched_torrent`). Recovery acts only on plans still `Executing` (`recovery.rs:90-94`). |
| 9. Reused blocker codes | Safe for tier 2 (every reuse blocks), but misleading. See F10. |
| 4. `ArrRescan` / `JellyfinRefresh` deferred | **Safe.** `supported()` admits only `ConsolidateHardlink` (`mod.rs:67-70`). Any other kind refuses the whole plan before any intent (`run.rs:147-186`, test `tiers_three_to_five_are_explicit_refusals`). A consolidation replaces a file with identical bytes at the same path, so skipping the CONTEXTE §3.5 step 12 rescan loses no correctness. |
| 5-8 (event instead of journal marker, compare in `spawn_blocking`, `NoScanErrorUnder` from stored state, two probe intents) | Safe. Two probe intents strengthen INV-08. A stored-state `NoScanErrorUnder` fails closed: a running or incomplete latest session blocks (`revalidate.rs:348-369`). |

---

## Requirements tier 3 must satisfy (to be verified at the next CP2)

T3-0. **F01-F04 fixed first**, with tests, before `IMPLEMENTED_TIER = T3`.

**Filesystem mutator (`LiveFsMutator`)**

T3-1. `remove_file`, `remove_tree` and `remove_empty_dir` work only through `open_parent` / `open_rel_dir` handles (`unlinkat`, `unlinkat(AT_REMOVEDIR)`). No path string is re-resolved, a configured root is never a target (`mutate.rs:89-100` already refuses it), and there is no shell, `std::fs::remove_*` or `Command`.

T3-2. **Media-root refusal** at two layers: the runner refuses any tier-3 action whose path has `RootKind::Media` (beyond `plans/validate.rs:81-83`), and the mutator's `remove_*` refuse `RootKind::Media` outright. The tier-4 `DeleteMediaFile` path must later be added explicitly and reviewed separately. `remove_empty_dir` is allowed only under the completed root (INV-49).

T3-3. `remove_file`: `lstat` the final component through the parent handle immediately before `unlinkat`, and require `matches(expect, compare_nlink = true)` (CON-14). A symlinked target or component is refused (Confinement). `ENOENT` is a `StateChanged` refusal, never "already gone = success" (CONTEXTE §3.15).

T3-4. `remove_tree` is **all or nothing at the decision point**. Immediately before the first unlink, take a fresh recursive snapshot through handles and require equality with the plan's `DirSnapshot`, including `nlink`, `uid/gid`, `mode`, `mtime_ns` and the same `dev` (no mount crossing). Then delete deepest first on the same handles, re-checking each entry's `(dev, ino)` against the snapshot before its unlink. Symlink entries are unlinked, never followed (T-17). Any mismatch after the first unlink stops at once: the outcome is `FailedNeedsReview` (never `Verified`), the step result lists what was actually removed, and every affected hash stays stopped. Cancellation is not honoured inside a tree once deletion has started; it is honoured between items.

T3-5. `remove_empty_dir`: the directory must be empty at the call (rmdir semantics) and keep the planned `dev/ino/uid/gid/mode`. `mtime_ns`/`nlink` change because of the same transaction's removals, so define and test the exact comparison rule; do not silently ignore other changes. A parent is removed only if it is empty, or holds only annexes listed in the plan, and only under completed.

**Revalidation and selection**

T3-6. **Samples**: at execution, refuse any target, and any entry of a tree, that matches the sample rule unless `Approval.acknowledged_samples` names that exact path (CONTEXTE §3.7, MASTER §2). Re-check at execution, not only at planning.

T3-7. **Junk only outside torrents**: no torrent references the path (fresh listing plus `files()` over the full ownership index: candidates, `.!qB`, `download_path`, content directories of incomplete torrents, `.parts`), and no torrent-held path shares its `(dev, ino)` (bind the inode view; today `revalidate.rs:427-434` builds `TorrentIndex` without `bind_inventories`). qBittorrent unreachable, or a mapping fault: blocked.

T3-8. **Torrent-held deletions**: the live holder set equals `action.torrent_hashes` (F02). Every holder is allowlisted with `progress == 1` and its planned wanted set unchanged (F03). Order: stop, pass-2 revalidation including `DirUnchanged`/`EntryUnchanged`, then the first unlink. No deletion if a scan error exists under or above the target.

T3-9. **Task removal** (`RemoveTorrentTask`): only when planned and confirmed (`confirmed_torrent_removals`) **and**, after the deletions, a fresh `files()` read plus `lstat` show every wanted file candidate absent. Only via `remove_tasks_keep_data` (no boolean anywhere; `deleteFiles` stays the single literal `false`). Journaled `QbitRemoveKeepData` intent, verified by `TorrentAbsent` on a fresh listing. Partially affected hashes are rechecked and then **left stopped** and reported (T-23): starting a torrent that lost wanted files would re-download them, and `plan_restore` must keep refusing it. `stoppedUP` is never removed blindly (CONTEXTE §3.8).

T3-10. **Stalled incomplete cleanup** (manual only, OQ-06): the torrent still has `progress < 1`, is not busy (`checking*`, `moving`, `allocating`, `checkingResumeData`, unknown), and has age >= threshold. **Every** file path is `Mapped` under the incomplete root only; any candidate under completed or media refuses the whole action. Data signatures equal the plan. Order: stop, remove the task (`deleteFiles=false`), fresh listing and files (no torrent references the data any more), re-stat and re-check confinement, then delete. A crash after the task removal must never lead recovery to delete data: resolve by observation, and the data becomes an orphan for a later manual plan.

T3-11. `DeleteIncompleteOrphan`: full ownership index. Working files are never orphans while any torrent's content directory holds them (CP1-F03).

T3-12. Exclude paths involved in a failed consolidation (CONTEXTE §3.7) from automatic deletion, using the journal. This needs F10's codes.

**Journal, verification, recovery**

T3-13. Every new mutation (`FsRemoveFile`, `FsRemoveTree`, `FsRemoveEmptyDir`, `FsSetMode`, `QbitRemoveKeepData`) goes through `Ctx::step`: durable intent with target, observed signatures/snapshot, expected gain and hashes; then the adapter call; then the result. No mutator call exists outside `step()` (F04 makes this structural for simulations).

T3-14. Verification on real re-reads: `Absent` passes only on a confirmed `NotFound` (a permission or other error is `NotRun`), `TorrentAbsent` uses a fresh listing, `ModeEquals` uses `stat`. `AttributableDeletion` gains only for `Verified` actions, and only when every link of the inode is removed (nlink at revalidation equals the plan's links; T-03, CONTEXTE §3.14).

T3-15. Recovery for each new step re-observes the disk or the listing (F01 rule). Any possible partial deletion makes the hashes modified (recheck, then left stopped unless `Verified`) and the outcome `FailedNeedsReview`/`ExecutedUnverified`. A mutation is never redone. Leftover temp links are reported (F07).

**Dry-run, tests, orchestration**

T3-16. Simulating every tier-3 kind runs the full path (live revalidation, every would-be step journaled `Simulated`, verification `Simulated`). Zero-mutation guards cover fs, qBittorrent (including `remove`), ARR and Jellyfin, in a dry-run process **and** in a live-process `simulate = true` (F05).

T3-17. Crash-boundary tests for every new step: before the intent, after the intent and before the mutation, after the mutation and before the result, a `Failed` result followed by a crash, a partial `remove_tree`, task removed but data not yet deleted, and during a recheck. Each test asserts the disk, the torrent states and the reported outcome after `recover`.

T3-18. `FixMode`: the target is owned by the process UID, never recursive, the signature is revalidated, and `ModeEquals` is verified.

T3-19. Job runner (14) contract: exclusive locks on `plan.lock_roots`, `recover` before every execute job and after any `execute` `Err` (F11), no execute job while recovery is unresolved, and durable left-stopped visibility (F09).

---

## Verified compliant (with evidence)

**Criterion 1: tests**
- Every test passes (counts above). The crash model is faithful: a fake hangs at the boundary and the execution task is aborted, so no compensation code runs, then `recover` runs on the same SQLite journal and temp roots (`actions_common/mod.rs` `Hang`, `HangJournal`; `actions_crash.rs:27-60`).
- The four required boundaries are covered:
  - before the intent: `BeforeIntent(QbitStop)` and `BeforeIntent(FsReplaceWithHardlink)`;
  - after the intent, before the mutation: `AfterIntent(FsReplaceWithHardlink)`, which asserts "intent durable, nothing mutated";
  - after the mutation, before the result: `BeforeResult(FsReplaceWithHardlink)`, which asserts the link is on disk and the result is not;
  - during the recheck: `DuringRecheck`, which asserts a second recheck, "never trusted".
  - Gaps: F12.
- Dry-run zero mutation: t30 uses the recording set and asserts `recording.calls() == 0`, inodes unchanged, plan status unchanged, every journal row `Simulated`, the real compare executed once, and no unfinished intent. The live-process simulation test uses the **real** `LiveFsMutator` behind a counter plus the qBittorrent fake as controller, and asserts zero calls. This proves the fs and qBittorrent sides; ARR/Jellyfin is vacuous (F05).
- Revalidation tests modify real files and the fake (t16 a-e), including a change injected between the stop and pass 2 through an event sink (the torrent is resumed, the plan invalidated).

**Criterion 2: action-engine invariants**
- `deleteFiles=false` everywhere. `QbitControl` has no boolean (`domain/src/ports.rs:141-143`). The only `deleteFiles` literal is `"false"` (`control.rs:108-112`). The test fake panics on any remove in stage A.
- No shell, and no process spawned from path strings, in `actions` or `mutate.rs`. Every fs operation is an `*at` call on a no-follow directory handle (`handle.rs:107-155`, `mutate.rs:184-270`).
- Intent durable before every mutation. `Ctx::step` awaits `record_intent` before the adapter closure runs (`journal.rs:117-152`). `record_intent` commits on the single writer with `synchronous = FULL` (`storage/src/db.rs:44`, `repos/journal.rs:88-108`). The trigger `intents_live_needs_executing` (migration `0002`, lines 140-143) forbids live intents outside an executing plan. Journal steps happen in the order probe, stop, probe-gate, replace, recheck, start (t13).
- Fail closed on revalidation differences:
  - `EntryUnchanged` compares kind, dev, ino, size, mtime_ns, mode, uid and gid (`domain/src/fs.rs:55-63`), refuses symlinks, and turns a read error into a blocker (`revalidate.rs:317-335`);
  - `DirUnchanged` uses a recursive snapshot;
  - services, torrent eligibility class and progress, ARR queue and fingerprint, and the config fingerprint are all checked live;
  - one failing action invalidates the whole plan (`consolidate.rs:295-330`), and pass 2 re-reads after the stop.
  - Gaps: F02, F03.
- Dry-run structure: no live adapter can exist without `LiveMode` (deviation 1 above). The runner refuses non-simulated tickets in a dry-run process or for a dry-run plan (`run.rs:98-104`), and requires `Approved` status plus an approval of exactly this plan hash (`run.rs:105-119`; the storage trigger `plans_status_needs_approval` enforces the same). Gap for live-process simulations: F04.
- Uncertain and partially modified torrents stay stopped. `plan_restore` starts only hashes that were active initially and are untouched or `Verified` (`qbit.rs:47-79`, unit test). `evaluate_recheck` needs an observed checking state that ended in an allowlisted, complete state (`domain/src/torrent.rs:213-235`). Timeout and failure stay stopped (t23), and the reasons are reported in `left_stopped` and `TorrentsLeftStopped` (durability: F09).
- Failed verification is never reported as success. `outcome_from_verification` returns `ExecutedUnverified` on any `Failed`/`NotRun` (`domain/src/journal.rs:212-235`). A failed replace step is capped at `ExecutedUnverified` whatever the re-read says (`consolidate.rs:281-285`, test `a_failed_replace_step_...`). No gain is recorded without `Verified` (`run.rs:317-320`).
- Recovery never redoes a mutation: it only `stat`s, writes observed results, rechecks and starts (`recovery.rs:133-304`). Interrupted plans can never resume. Without a live controller, torrents stay stopped with `NoLiveController`. Gap in "resumes only untouched torrents": F01.

**Criterion 3: hardlink repair**
- Direction: `HardlinkProbeGate` and `replace_with_hardlink` link the **source** (completed) into the **target's** directory and rename over the target; the completed inode is unchanged (t13 asserts both). Defense in depth: F08.
- Permission probe before the full compare and before the stop: step 2 probes with no content read and no torrent command (t14: 0 compares, 0 torrent commands, the explanatory hint names `protected_hardlinks` and never suggests `sudo`). The gate probes again inside the verifier before reading (`proof.rs:270-279`).
- A change during the compare voids it: signatures are checked before, during and after (`proof.rs:256-297`), and `replace_with_hardlink` re-checks both signatures before linking and the target again before renaming (`mutate.rs:246-256`). t11/t12 confirm nothing is mutated and the torrent is resumed without a recheck.
- Temp link plus atomic rename: `linkat` to a temp name in the target's directory, `lstat` proof of `(dev, ino)`, last look at the target, `renameat`, `lstat` proof again, `fsync` of the directory (`mutate.rs:184-270`). The runner verifies `InodeEquals` on both paths by real re-read (`verify.rs:37-53`).
- No temp link left on any error path (each one unlinks the temp, `mutate.rs:199-212, 253-260`; test asserts the directory listing is unchanged). Crash case: F07.
- Symlinks refused at every component: the root is walked from `/` with `O_NOFOLLOW|O_DIRECTORY` (`handle.rs:126-143`), relative components the same way (`handle.rs:146-155`), and the final component is `lstat`ed (`mutate.rs:123-144`). Tests cover a symlinked source, target, intermediate component and root (`mutate_hardlink.rs:132-185`).

**Criterion 4: readiness of the foundation**
The journal (durable, immutable, audit triggers), the per-step pattern, the two-pass revalidation, the qBittorrent transaction (journaled stop, recheck with timeout, restore rule), the recovery skeleton (unfinished intents plus executing plans) and handle-based confinement are **sound and reusable** for tier 3, once F01-F04 are fixed and T3-1..T3-19 are met.

Tier 3 may be built on this foundation under the condition stated at the top.
