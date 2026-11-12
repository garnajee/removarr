# CP2-tier4 review: may risk tier 4 be built on top of tiers 1-3? (iteration 1)

Reviewer: independent `general-purpose` agent. I wrote none of the code under review. I also wrote
`docs/reviews/cp2-tier3.md`, so this review checks that the conditions I set there were met.

Scope:
- commits `fed48c3` (CP2-tier3 fixes), `4faea5d` (domain patch 3), `9c2490a` (plan-engine pins) and `550d2a8`
  (tier 3, `IMPLEMENTED_TIER = T3`);
- handoff 13 "Stage B", handoff 03 "Patch 3", handoff 12 "Wave 9: TorrentWantedFiles";
- `crates/workflows/src/actions/**`, `crates/fs/src/mutate.rs`, and the new tests (`actions_fixes`, `actions_delete`,
  `actions_stalled`, `actions_tier3_crash`, `actions_wanted`, `fs/tests/mutate_remove.rs`);
- for tier-4 readiness: MASTER §5 "Remplacement de release", CONTEXTE §3.3 (ffprobe), §3.4, §3.6, §3.9, architecture
  §7.4 `ReplaceRelease`/`ArrGrabRelease`, §13, §20, ADR-0010, and `crates/arr/src/commands.rs`.

`crates/domain/src/plan.rs` was only grepped (CLAUDE.md rule 5).

Verdict: PASS

There is no blocking finding and **one major** (F01, a one-line class of fix). All of CP2T3-F01..F04 are fixed,
with tests that exercise the failure paths I described. Every tier-3 requirement is implemented and tested,
except T3-12 (tier 5) and T3-19 (job-runner 14). Both deferrals are valid, but T3-12's deferral depends on
CP2T4-F01.

**Condition attached to this PASS:** fix CP2T4-F01 (runner tier gate aware of plan origin) before, or as the first
step of, tier 4. The next CP2 (before tier 5) must verify it, together with T4-01..T4-20 below.

---

## Test and gate evidence (run by the reviewer, macOS host, rustc 1.98)

| Command | Result |
|---|---|
| `cargo test --workspace` | **682 passed, 0 failed, 1 ignored** (90 test binaries; the ignored one is the opt-in real-ffprobe test) |
| per suite | `actions_fixes` 11/11, `actions_delete` 10/10, `actions_stalled` 6/6, `actions_tier3_crash` 7/7, `actions_wanted` 2/2, `actions_consolidate` 13/13, `actions_crash` 8/8, `fs mutate_remove` 7/7, `fs mutate_hardlink` 6/6 |
| `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings` | pass; pass |
| `grep deleteFiles\|delete_files\|torrents/delete` (non-test sources) | one literal `("deleteFiles", "false")` at `crates/qbit/src/control.rs:111`; the rest are comments and the test fake |
| `grep Command::new\|std::process::Command` (non-test sources) | only `crates/matching/src/ffprobe.rs:114` (argv, `file:` protocol whitelist, never a shell) |
| mutator call sites in `actions/**` | `delete.rs:401-403` (`remove_file`/`remove_empty_dir`/`remove_tree`, after the intent at `delete.rs:382-392`), `simple.rs:137` (`set_mode`, after the intent at `simple.rs:107-118`), `delete.rs:588` and `stalled.rs:225` (`remove_tasks_keep_data`, inside `Ctx::step`) |
| `grep importMode` | one literal `"importMode": "copy"` at `crates/arr/src/commands.rs:230`; `ManualImportRequest` has no mode field (`domain/src/arr/mod.rs:302`, `ports.rs:185`) |

---

## Findings

| ID | Severity | Crit. | Evidence | Problem | Required change |
|---|---|---|---|---|---|
| CP2T4-F01 | major | 2, 3 (tier gate; T3-12 deferral) | `actions/run.rs:156-160` and `actions/mod.rs:74-76` gate on `a.kind.tier()` only; `plans/gate.rs:11-17` makes every `PlanOrigin::AutoPolicy` action tier **T5**; handoff 13 Stage B T3-12 row ("automatic execution is tier 5, `NotInBuild`") | The runner does **not** refuse automatic plans. An approved `AutoPolicy` plan of `DeleteCompletedFile`/`DeleteCompletedFolder`/`RemoveEmptyParent`/`RemoveTorrentTask`/`ConsolidateHardlink` executes live today. Nothing stops it except the planner's gate, which depends on `PlanSettings.implemented_tier` being wired correctly by 16. The handoff says automatic execution is `NotInBuild`, and that is why T3-12 (exclude subjects of failed consolidations) can wait for tier 5. At the runner, that statement is false. Architecture §6.7 places the `IMPLEMENTED_TIER` gate in the runner. | In `execute`, gate on `plans::gate::action_tier(a.kind, plan.origin)` (or refuse `AutoPolicy` while `IMPLEMENTED_TIER < T5`), live **and** simulated, with `NotInBuild`. Add a test: an approved automatic deletion plan is refused before any intent. |
| CP2T4-F02 | minor | 3 (recovery/state decided from observation) | `delete.rs:597` and `stalled.rs:229-236` (`StepRun::Failed` of `QbitRemoveKeepData` becomes `Nothing(Failed)` with no fresh listing), compared with recovery's listing-based judgement (`recovery.rs:333-381`) | The CP2T3-F01 rule ("judge by what the service shows") is applied to fs steps and to recovery, but not to a **live** failed task removal. After a transport error the task may be gone anyway. Then the batch reports `Failed`, `restore`/`recheck` try a hash that no longer exists (refused by `control.rs:80-85`), and the hash is reported `StartFailed`/`NotRechecked` although it was removed. The data is safe (stalled data is deleted only after a successful removal), but what the run reports is wrong. | On `Failed`, re-read the listing: if the hash is gone, record `Done { observed: removed }` and call `tx.removed(hash)`, as recovery does. Test with a fake that removes the task and then returns `Unavailable`. |
| CP2T4-F03 | minor | 3 (stopped torrents stay visible) | `stalled.rs:186-189` (`q.torrents().await.unwrap_or_default()`); `qbit.rs` `stop_torrents` builds `tx.initial` only from the listing; `restore` returns early when `initial` and `modified` are empty | If that listing read fails, the stop is still issued with no initial state. If pass 2 then refuses, `restore` does nothing: the torrent stays stopped and is **neither restarted nor reported**. `stop_torrents` generally stops hashes that are missing from the listing without recording them. Narrow (a transient qBittorrent fault between two successful reads), but it violates "left stopped and visible". | Refuse the stop when the listing is unavailable or lacks a hash (pass 1 already holds a fresh listing; reuse it, as `delete.rs`/`consolidate.rs` do). Any stopped hash with no initial state goes into `left_stopped` as `UnknownInitialState`. |
| CP2T4-F04 | minor | 4 (`FixMode` exemption) | `revalidate.rs:359-362` (exemption); `simple.rs:136-137` and `fs/src/mutate.rs` `set_mode` (any `mode` from the plan is applied); planner `plans/fsplans.rs:578-580` (`mode \| 0o200` / `mode \| 0o300`); `plans/validate.rs:127-131` (checks the pin only) | The exemption is justified only because the new mode **adds owner write/search bits** to an entry the process owns: bytes, paths, mtime and other identities' access are unchanged, so no torrent, ARR or Jellyfin reader can lose access. Nothing after the planner enforces that property. A plan whose mode removed bits, or added `setuid`/`o+w`, would pass the runner and the mutator, and could break a seeding torrent that no holder check would see. | Enforce it at execution, in the runner, the mutator, or both: `mode & expect.mode == expect.mode` and `mode & !expect.mode ⊆ 0o200` (file) / `⊆ 0o300` (dir), otherwise `Validation`. Add a test with a bit-removing mode. With that in place the exemption is safe (see "Judgement" below). |
| CP2T4-F05 | minor | 2, 3 (defense in depth) | `plans/validate.rs:52-64` (one pin per hash: planner only; `ActionPlan::validate` rejects only empty, foreign or duplicate pins, handoff 03 Patch 3); `revalidate.rs:678-713` (compares the pins that exist); `revalidate.rs:719-725` (`ConfigFingerprint` checked only when present); no capability check for fs kinds in the runner or `LiveFsMutator` vs the `plans/gate.rs:1-2` comment and architecture §6.7 ("checked twice ... inside the live adapter") | The runner trusts the planner for the **presence** of mandatory preconditions. A plan with a hash but no `TorrentWantedFiles` pin (hand-built, or approved before patch 3) skips the wanted-set check. A plan without `ConfigFingerprint` skips the config and capability check. `delete_completed`, `cleanup_incomplete` and `fix_permissions` are never checked at execution, so their only execution-time effect comes through the config fingerprint. | In revalidation: refuse an action whose `torrent_hashes` lack a pin, and compare `plan.config_fingerprint` with `settings.config_fingerprint` directly. Pass the enabled capabilities to `ActionSettings` and refuse a kind whose capability is off (`CapabilityDisabled`). |
| CP2T4-F06 | minor | 1 | handoff 13 Stage B F07 row ("no dedicated assertion"); `recovery.rs:193-201` and the probe branch | The leftover-link notes (`.removarr-probe-*`, `.removarr-link-*`) exist but no test asserts them. | Assert the notes in `crash_after_the_replace_intent_before_the_mutation_...` and in a probe-unlink-failure test. |
| CP2T4-F07 | minor | 2 (live holders) | `revalidate.rs:378-445` (`check_live_holders`: only "near" torrents, inventories of the action's **own** roots) vs `revalidate.rs:600-617` (`NoTorrentReferences`: all torrents, inventories of every session root) | Two holder views with different reach. A torrent that seeds from another root and holds a target's inode through a different path is invisible to `check_live_holders`. For tier-3 deletions this is data-safe (the other link survives, and `compare_nlink = true` catches a new link). Tier 4 relies on inode accounting across roots (`OldCoverageRecoverable::OtherLink`, `OldFileAccounted`, `AddedCost`). | Either bind every session root's inventory in `check_live_holders` too, or document the narrower contract and require tier 4 to use the full view (T4-12). |

Count: 0 blocking, 1 major (F01), 6 minor (F02-F07).

---

## Criterion 1: CP2T3-F01..F04 really fixed

| Finding | Verified in code | Verified by test |
|---|---|---|
| F01 (recovery decides from disk) | Live: the replace result records what the disk shows (`consolidate.rs`, `ReplaceObserved`). Recovery: every `FsReplaceWithHardlink` intent whose result is `Failed` or missing is re-observed; only `Skipped`/`Simulated` rule the mutation out (`recovery.rs:178-237`). The same rule covers `FsRemoveFile/Tree/EmptyDir/SetMode` (`recovery.rs:288-332`, `observe_fs_intent` `recovery.rs:534-566`, partial tree -> `Unobservable` -> `FailedNeedsReview`). The retry path stays consistent because a `Failed` result is re-observed every time. Unknown future steps default to "possibly executed" (`recovery.rs:395-420`). | `actions_fixes.rs::f01_a_failed_but_mutated_replace_result_is_rechecked_at_recovery_in_a_two_item_plan` (exactly my scenario: hash rechecked, `ExecutedUnverified`), `::f01_an_unobservable_target_stays_possibly_modified_across_a_recovery_retry`; `actions_tier3_crash.rs::a_failed_result_followed_by_a_crash_is_re_observed_at_recovery`, `::a_partial_tree_removal_followed_by_a_crash_is_reviewed_at_recovery` |
| F02 (live holder set) | `revalidate.rs:352-472`: fresh listing, `files()` of every torrent whose base paths relate to an action path, `TorrentIndex` with the inode view; a hash outside `torrent_hashes` -> `TorrentReferencesTarget`; a `NoRule`/`Invalid` mapping on any torrent -> `MappingError`; qBittorrent down -> blocker. Runs in every pass and after a stalled task removal (`revalidate_fs_only`). Reach: see F07. | `::f02_a_torrent_that_started_holding_a_path_after_planning_invalidates_the_plan` (cross-seed of the completed path and a library seeder of the media path), `::f02_a_holder_by_inode_only_and_a_mapping_fault_near_the_paths_both_refuse`; `actions_delete.rs::t3_8_*` |
| F03 (pinned wanted set) | Domain patch 3 `Precondition::TorrentWantedFiles` (grep `plan.rs`); the planner emits one pin per hash (`plans/build.rs:248`) and `check_plan` requires exactly one (`validate.rs:52-64`). The runner compares a fresh `files()` read with the pin in every pass (`revalidate.rs:678-713`), and the "latest stored snapshot" baseline is gone. Pin presence: see F05. | `actions_wanted.rs::the_pinned_wanted_set_passes_unchanged_and_every_change_refuses_before_any_mutation`, `::a_consolidation_built_through_the_planner_executes_end_to_end`; `plans_expiry.rs::the_pinned_wanted_file_set_must_be_unchanged` |
| F04 (simulate on recording set) | `run.rs:129-135`: when `simulated`, `Mutators::recording(&services)` replaces the caller's mutators before the `Ctx` exists. A simulation can no longer reach a live adapter, whatever the code below does. | `::f04_f05_a_simulation_in_a_live_process_runs_on_the_recording_set_and_touches_no_service` (live counting fs, qBittorrent, ARR and Jellyfin adapters: zero calls); `actions_delete.rs::t30_simulating_every_tier3_kind_walks_the_full_path_and_mutates_nothing` (both process kinds, every tier-3 kind) |

F05-F14 are also closed as the handoff table says. I spot-checked:
- F08: `check_direction`, `mutate.rs:109-116`;
- F09: `restore` always journals `RestoreObserved { recheck, left_stopped }`;
- F10: `ChangedSincePlanning`/`Interrupted` are used throughout;
- F11: `batch.rs:87-93` turns a store error into a blocker, so the restore still runs;
- F12: four new crash tests;
- F13: `TEMP_BASE_MAX = 64`, with a 250-byte-name test.

Residue: F06 (the leftover notes are untested), now CP2T4-F06.

## Criterion 2: T3-0..T3-19

| Req. | Status | Evidence |
|---|---|---|
| T3-0 | done | criterion 1 |
| T3-1 handle-based removals | done | `mutate.rs:396-410` (`remove_file`), `414-482` (`remove_tree`: `open_parent`, `walk` on handles, `unlinkat`), `484-514`; no `std::fs::remove_*`, no `Command`; `mutate_remove.rs` |
| T3-2 media refusal, root never a target | done | runner `delete.rs:144-155` (`MediaRootDeletion`, `ProtectedZone` for a root or wrong kind); mutator `refuse_media` (`mutate.rs:235-242`) in `remove_file`/`remove_tree`, `remove_empty_dir` completed only (`mutate.rs:485-489`); `open_parent` refuses the root; tests `removals_never_touch_a_media_root`, `revalidation_after_the_stop_..._media_targets_are_refused` |
| T3-3 `remove_file` | done | `lstat` through the parent handle, `matches(expect, true)`, symlink refused, `ENOENT` = `StateChanged`, parent `fsync` |
| T3-4 tree all or nothing | done | fresh walk must **equal** the plan's snapshot before the first unlink (`mutate.rs:432-441`); deepest first (reverse DFS insertion), `(dev, ino)` and kind re-checked per entry, mount boundary refused, a mismatch stops with a "removed n of m" error; the runner judges by `observe_tree`, a partial tree -> `FailedNeedsReview`, hashes stay modified (`delete.rs:450-479`, `329-332`); tests listed in the handoff |
| T3-5 empty dir | done | `rmdir` semantics, `dev/ino/uid/gid/mode` compared, `mtime/nlink` excluded by design (`mutate.rs:484-514`); `inv49_*` |
| T3-6 samples | done | `batch.rs:124-151`: target and every non-dir tree entry, the approval must name the exact path; `is_sample` covers ancestor directories (`domain/src/policy.rs:12-23`); `t15_*` |
| T3-7 junk by path or inode | done | `NoTorrentReferences` with all torrents and files, inventories of every session root, and live inodes under the target (`revalidate.rs:572-623`); `t3_7_*` |
| T3-8 torrent-held deletions | done | live holders (F02), pins (F03), `TorrentStates`, stop, pass 2, then unlink (`delete.rs:192-246`) |
| T3-9 task removal | done | typed confirmation, fresh `files()`, **every candidate of every wanted file** `lstat`-absent, else kept (`delete.rs:497-574`); journaled `QbitRemoveKeepData`; the removed hash is excluded from recheck/start (`qbit.rs:99-103`); `TorrentAbsent` verification; partial -> left stopped (`t23_*`). Failure path: F02 |
| T3-10 stalled cleanup | done | manual only (`stalled.rs:40-45`); data paths and **every live file candidate** under the incomplete root (`stalled.rs:48-55, 92-126`) in both passes; stop -> pass 2 -> remove task -> `revalidate_fs_only` (holders, `NoTorrentReferences`, signatures) -> delete; data referenced after the removal stays as an orphan (`ExecutedUnverified`). Stop path: F03 |
| T3-11 incomplete orphans | done | `DeleteIncompleteOrphan` limited to the incomplete root (`delete.rs:125-127`), full ownership index |
| T3-12 failed-consolidation exclusion | **deferred to tier 5**: valid **only once CP2T4-F01 is fixed** (today the runner would execute an automatic plan) | — |
| T3-13 every mutation behind a durable intent | done | call sites listed above; `Ctx::intent` awaits the committed `record_intent` (`synchronous = FULL`) |
| T3-14 verification and gains | done | `Absent` passes only on `Ok(None)`, an error is `NotRun` (`verify.rs:62-68`); `TorrentAbsent` uses a fresh listing; `ModeEquals` uses `stat`; gains only for `Verified` (`run.rs:334-337`) with the planner's attributable bytes (nlink pinned by `compare_nlink = true`) |
| T3-15 recovery | done | criterion 1; a task removal is decided by the listing; the stalled data is never deleted at recovery (`stalled_crash_after_the_task_removal_never_deletes_the_data_at_recovery`) |
| T3-16 simulation of every kind | done | `t30_simulating_every_tier3_kind_*` (recording set with ARR adapters, live-process guard, disk untouched, `Simulated` journal) |
| T3-17 crash points | done | `actions_tier3_crash.rs` (7 tests) plus the F12 seams |
| T3-18 `FixMode` | done | owner = euid, never recursive, opened no-follow and proven by `(dev, ino)`, `fchmod`, read-back (`mutate.rs:516-560`); mode content: F04 |
| T3-19 job-runner contract | **owned by 14**: valid owner. Locks, `recover` before every execute and after any `Err`, no execute while recovery is unresolved. The F09 journal makes left-stopped durable. | — |

## Criterion 3: tier-3 engine invariants

| Invariant | Holds? | Evidence |
|---|---|---|
| `deleteFiles=false` everywhere | yes | single literal (`control.rs:111`); `QbitControl::remove_tasks_keep_data` has no boolean |
| no shell built from paths | yes | greps above; every fs operation is an `*at` call on a no-follow handle |
| durable intent before each `remove_*` / task removal | yes | `delete.rs:382-392` before `398-405`; `stalled.rs:214-228` and `delete.rs:578-590` through `Ctx::step` |
| fail-closed revalidation (live wanted set, live holders) | yes, with F05/F07 caveats | `revalidate.rs:352-472`, `678-713`; every read error is a blocker; `batch.rs:87-93` |
| folder all or nothing | yes | T3-4 |
| samples and media-root targets refused | yes | T3-2, T3-6 |
| junk only when no torrent holds it by path or inode | yes | T3-7 |
| stalled cleanup manual only, incomplete root only | yes | T3-10 |
| simulate always on recording mutators | yes | `run.rs:129-135` |
| recovery decides from observed state | yes (live failed task removal: F02) | criterion 1 |

## Criterion 4: judgement on the `FixMode` exemption from the live-holder check

**Safe, under one condition that the code does not yet enforce (F04).**

Why it is safe:
- `FixMode` changes only permission bits. A torrent's piece hashes, file paths, size and mtime are unchanged, so qBittorrent's view of its data is unchanged: no stop or recheck is needed, and a seeding holder cannot be harmed by an *added* owner bit.
- The planner only ever adds `u+w` (files) or `u+wx` (directories) to an entry owned by the process uid.
- The other checks still apply to `FixMode`: `EntryUnchanged` (including mode, uid and gid) and `NoScanErrorUnder`, plus the mutator's owner check and the no-follow open with `(dev, ino)` proof. A mode change also changes the entry's signature, so any other pending plan pinned on that entry is invalidated (fail closed).

What makes it unsafe without F04: the runner and the mutator apply whatever `mode` the plan carries. A mode that removed `r` or `x`, or added `setuid` or world-write, would reach a torrent-held file or directory with no holder check, and could put seeding torrents into `error`/`missingFiles`.

Constrain the mode at execution (additive, owner w/x only), and the exemption is sound.

---

## Tier-4 requirements (to verify at the next CP2)

**Gates and preconditions**

T4-01. **Gates**: CP2T4-F01 fixed (origin-aware tier gate). The runner checks `arr_manual_import`, `arr_grab`, `arr_queue_remove` and `arr_set_monitored` itself, in addition to the adapter (`arr/src/commands.rs` `require`). A tier-4 plan must not mix families (`mod.rs::family` gets a `Replace` family). `ReplaceRelease`, `DeleteMediaFile` and `ArrGrabRelease` are refused for `AutoPolicy` (A9) in the runner, not only in the planner.

T4-02. **`importMode` copy literal**: the runner never builds an ARR body; `ManualImportRequest` keeps no mode field; the gate grep finds exactly one `"importMode": "copy"`; `fake_arr` rejects any other mode. A test where the fake ARR **moves** the source must end in `FailedNeedsReview` + `ArrImportMoved`, never `Verified` (architecture §6.8(d)).

T4-03. **Mandatory preconditions present and revalidated** (do not trust the planner, F05). On every `ReplaceRelease`:
- `ArrFingerprint` (profile, CF scores, cutoff, current files, `media_management` incl. recycle bin and hardlinks flag; `domain/src/arr/mod.rs:154-170`);
- `OldCoverageRecoverable`;
- `EntryUnchanged { compare_nlink: false }` + `PhysicalId` of every completed source;
- `TorrentWantedFiles` pin + live holders for every source hash;
- `NoArrImport` for the media and the source hashes;
- `SameDevice` source / target media root;
- `ConfigFingerprint`.

Re-run ARR's manual-import **preview** right before the command: every candidate is still accepted with the same `episode_ids` and no rejection. Any difference makes the plan `Invalidated`. A score or profile change invalidates the plan (CONTEXTE §3.4).

**Import and verification**

T4-04. **One importer per media** (INV-11, CONTEXTE §3.9 "Concurrence"): `NoArrImport` re-read after any stop and **immediately before** `manual_import`. ARR's queue has no importing item for the media or the hashes. The application never creates media hardlinks for a different version. A queue alert is never taken as proof that a hardlink is absent.

T4-05. **Completed source inode before and after the import**: re-`stat` before (revalidation) and after (`PathStillHasInode`). If the source vanished or its inode changed: `FailedNeedsReview` + `ArrImportMoved`, reported Critical. Every remaining step of the plan (including `DeleteMediaFile` and any old-release deletion) is cancelled, and the affected torrents are left stopped and reported.

T4-06. **`Verified` per episode or movie** only when all three hold:
- (a) ARR's current file is the new media path (`ArrCurrentFile`, fresh read);
- (b) the source still has its `(dev, ino)`;
- (c) the media path shares that `(dev, ino)`.

(a) and (b) without (c) (ARR copied) is `ExecutedUnverified` + `AddedCost` of the new allocated bytes, and `DeleteMediaFile` is blocked. A command that does not complete within the timeout is `ExecutedUnverified`, never success.

T4-07. **`OldCoverageRecoverable` (CP1-F20)** re-validated at execution:
- `OtherLink` paths are re-`stat`ed as their own `EntryUnchanged` pins, using the full cross-root inode view (F07);
- the recycle-bin path and cleanup period come through the fingerprint;
- `Unrecoverable` files are refused unless `Approval.acknowledged_unrecoverable` names **each** file exactly (checked by the runner, like samples, `batch.rs:124-138`), and never in automatic plans;
- `OldFileAccounted` records where every old file went; an unacknowledged lost file is `FailedNeedsReview`.

T4-08. **Hardlinks disabled in ARR**: `ArrHardlinksDisabled` is shown before approval. The runner treats `ExecutedUnverified` + `AddedCost` as the best possible outcome and never reports `Verified`.

T4-09. **`CoverageProof` for every episode**, inode by inode, including episodes without files, specials, and absolute/aired/TVDB numbering through explicit mapping. A season replacement runs only with complete coverage. The transaction is global (INV-35): if any episode of the unit fails its proof, no old coverage of the unit is removed.

T4-10. **Numbering mismatch**: `NumberingMismatch` refuses when ARR's preview `episode_ids` differ from the planned mapping, at planning and again at the execution-time preview.

T4-11. **ffprobe blockers**: `DurationMismatch` (outside tolerance), `AudioLanguageMissing` (a language of the current file is absent from the candidate), edition mismatch. All are computed on the pinned candidate. ffprobe stays argv-only with the `file` protocol whitelist and the per-component symlink refusal. Its results are planning evidence only and never prove equality. Candidate pins guarantee the probed bytes are the executed bytes.

**Other tier-4 kinds**

T4-12. **Grab** (`ArrGrabRelease`):
- manual approval only, never `AutoPolicy`, live only (refused in dry-run processes; simulations run on the recording set);
- capability `arr_grab` (default off);
- the plan carries the opaque `ReleaseRef` from a search no older than `plan_ttl_hours`, and the fingerprint is unchanged;
- the adapter refuses a token missing from its release cache (`commands.rs:234-243`), and the runner reports that as `ReleaseNotInSearch` (`NotExecuted`, never success);
- journaled intent before the POST; verified by `ArrQueueHasRelease` within the timeout, else `ExecutedUnverified`;
- recovery treats an unfinished grab as possibly executed and **never re-grabs**;
- a search never grabs (test).

T4-13. **`DeleteMediaFile`**:
- only inside a `ReplaceRelease` plan, explicitly selected, `depends_on` the import action, and only after that action is `Verified` **in the same run** for its episode;
- never automatic;
- the media file must not carry the new inode; `EntryUnchanged { compare_nlink: true }` holds;
- it needs a **dedicated** mutator entry point with its own media-root allowance, separately reviewed. `remove_file` must keep refusing media roots (`refuse_media`).

T4-14. **Old completed release** deletion after a replacement follows every tier-3 rule, and only after T4-06 is `Verified` (and never while an `OtherLink` it provides is still needed by T4-07).

T4-15. **`AddedCost` and net gain**:
- `AddedCost` = new allocated bytes when ARR copied, 0 when it hardlinked;
- net = freed − `AddedCost`;
- gain ids are per inode and unique;
- an old file retained by an `OtherLink` or by the recycle bin frees nothing attributable;
- potential, expected, attributable and observed stay distinct (MASTER §6, CONTEXTE §3.14).

T4-16. **Queue and monitoring commands**:
- `ArrRemoveQueueItem` with the literals `removeFromClient=false`, `blocklist=false`, only for a proven-stale item (`QueueItemHidesError` otherwise), verified by `ArrQueueItemAbsent`;
- `ArrSetMonitored` journals the previous value in `observed`, verified by `ArrMonitored`, and is targeted, never global (CONTEXTE §3.10).

**Recovery, tests, reporting**

T4-17. **Recovery** for `ArrManualImport`, `ArrGrab`, `ArrRemoveQueueItem`, `ArrSetMonitored` and `DeleteMediaFile`:
- re-observe through ARR (command status, current files, queue, monitored flag) and through the disk;
- never re-issue a command;
- otherwise keep the current conservative default (possibly executed: modified, `FailedNeedsReview`, `recovery.rs:395-420`);
- affected torrents stay stopped until a verified recheck.

T4-18. **Tests**:
- crash points around every new step;
- dry-run-no-mutation and live-process simulation for every tier-4 kind, with ARR and Jellyfin counters;
- score/profile change invalidates the plan; a rejected candidate is never selected;
- coverage cases (episodes without files, specials, absolute numbering);
- ffprobe blockers; `importMode=copy` asserted; "move would be detected";
- the CP1-F20 cases: other-link, recycle bin, unrecoverable with and without acknowledgement, hardlinks disabled, `OldFileAccounted` results;
- search never grabs; a stale grab token is refused.

T4-19. **Jellyfin** refresh and the recognition check (`items_by_paths`, bounded polling) are informational verification only. They never decide `Verified`, and Jellyfin never decides quality.

T4-20. Fix CP2T4-F02..F05 while touching the same code (the task-removal and command-failure judgement by observation applies equally to `manual_import` and `grab`).

---

## Verified compliant (summary)

- All four CP2T3 majors are fixed, with tests that reproduce the original failure paths (criterion 1).
- 17 of 19 tier-3 requirements are done with named tests. T3-12 and T3-19 are validly deferred (T3-12 depends on F01).
- The ten tier-3 invariants hold (criterion 3), with the minor caveats F02, F03, F05 and F07.
- Tier-4 foundations already in place:
  - the `importMode` literal and the mode-less request;
  - capability checks inside `ArrCommander`;
  - the grab token cache refusal;
  - `ArrFingerprint` covering current files and media management;
  - `NoArrImport` and `OldCoverageRecoverable` revalidation;
  - the domain codes `ArrImportMoved`, `OldCoverageUnrecoverable`, `ArrHardlinksDisabled`, `NumberingMismatch`, `DurationMismatch`, `AudioLanguageMissing`, `ReleaseNotInSearch`;
  - the verification steps `PathStillHasInode`, `ArrCurrentFile`, `ArrQueueHasRelease`, `OldFileAccounted`;
  - `GainKind::AddedCost`;
  - a conservative recovery default for steps it cannot yet observe.

Tier 4 may be built on this foundation under the condition stated at the top.
