# CP2-tier5 review: may tier 5 (automatic mode) be enabled on top of tiers 1-4? (iteration 1)

Reviewer: independent `general-purpose` agent. I wrote none of the code under review. I also wrote
`docs/reviews/cp2-tier3.md` and `docs/reviews/cp2-tier4.md`, so this review checks that the conditions I set there were
met.

Scope:
- commit `9c5cdf4` (stage C: CP2T4-F01..F07 fixes and tier 4, `IMPLEMENTED_TIER = T4`) and handoff 13 "Stage C";
- `crates/workflows/src/actions/**` (new `replace.rs`, `arrcmd.rs`; changes in `run.rs`, `recovery.rs`, `qbit.rs`,
  `simple.rs`, `stalled.rs`, `dry_run.rs`), `crates/fs/src/mutate.rs`, `crates/domain/src/ports.rs`;
- tests `actions_stage_c.rs`, `actions_tier4.rs`, `actions_common/{arrfake,tier4}.rs`, `actions_crash.rs`,
  `fs/tests/mutate_remove.rs`;
- for tier-5 readiness: MASTER §6 "Mode automatique", CONTEXTE §3.7 and §3.11, architecture §7.6 and §13.3 (A1-A9),
  `crates/workflows/src/plans/auto.rs`, and `HashedPlan::approve_auto` (grepped, `domain/src/plan.rs:624-659`, per
  CLAUDE.md rule 5).

Verdict: PASS

There is no blocking finding and **one major** (F01, a planner/runner contract break that makes planner-built queue
removals impossible; it fails closed). CP2T4-F01 is really fixed: the runner refuses every automatic plan before any
intent, live and simulated. CP2T4-F02..F07 are fixed or validly justified. Every tier-4 safety invariant holds.
Tier 4's open items (T4-09 multi-episode test, T4-14, T4-11/T4-19 planning-only) are safe deviations; none of them
is reachable in automatic mode, because A9 excludes every tier-4 kind.

**Condition attached to this PASS:** tier 5 is a **runner + job-runner (14) + policy (12)** deliverable, not a
constant change. `IMPLEMENTED_TIER` may be raised to `T5` only after T5-01..T5-22 below are implemented and tested,
and only after a CP2 review of that work. That review is the last action-runner checkpoint before CP3 and must
also verify CP2T5-F01. Today nothing executes automatically, which is correct.

---

## Test and gate evidence (run by the reviewer, macOS host, rustc 1.98)

| Command | Result |
|---|---|
| `cargo test --workspace` | **706 passed, 0 failed, 1 ignored** (92 test binaries; the ignored one is the opt-in real-ffprobe test) |
| per suite | `actions_stage_c` 5/5, `actions_tier4` 17/17, `actions_crash` 9/9, `actions_consolidate` 13/13, `actions_delete` 10/10, `fs mutate_remove` 8/8, `plans_gate` 8/8 |
| mutator call sites in `actions/**` | `consolidate.rs:297/378/477`, `delete.rs:397`, `replace.rs:990` (`remove_media_file`), `simple.rs:161`; qBittorrent through `qbit.rs` steps; ARR/Jellyfin commands only inside `Ctx::step` closures; `recovery.rs` touches only `mutators.qbit` (recheck/start) and never an ARR or fs mutator |
| delete flag / import mode literals | one `("deleteFiles", "false")` (`qbit/src/control.rs:111`), one `"importMode": "copy"` (`arr/src/commands.rs:230`) |

---

## Findings

| ID | Severity | Crit. | Evidence | Problem | Required change |
|---|---|---|---|---|---|
| CP2T5-F01 | major | 2 (T4-16 really done) | `actions/run.rs` `mandatory_defects` (every `torrent_hashes` entry needs a `TorrentWantedFiles` pin, no exception) vs `plans/build.rs:233-236` (`pins_torrents`: `ArrRemoveQueueItem` gets **no** pin by design, its hash is only ARR's download id) and `plans/service.rs:72` (`d.hashes.clone_from(&issue.torrent_hashes)`); test helper `actions_common/tier4.rs:193-208` builds queue removals with **no** hashes | The CP2T4-F05 fix contradicts the planner's documented exception. Every planner-built `ArrRemoveQueueItem` whose issue names a download id (the normal case) is refused with "the plan lacks the wanted-file pin". T4-16 therefore passes only on hand-built plans. It fails closed (no safety impact), but a tier-4 kind marked "done" does not work end to end, and the test seam hides it. | Apply `build::pins_torrents(kind)` in `mandatory_defects` (one shared rule), or move the download id out of `torrent_hashes`. Add a test that executes a **planner-built** queue-removal plan (as `actions_wanted.rs::a_consolidation_built_through_the_planner_executes_end_to_end` does for consolidation). |
| CP2T5-F02 | minor | 3 (T4-05 "every remaining step is cancelled") | `replace.rs` step 6: when `halted` is `ArrImportMoved`, a `DeleteMediaFile` whose own import was `Verified` still runs (`if let Some(h) = &halted && !deps.all(verified)`); the halt message says "every remaining step is cancelled"; architecture §7.4 ReplaceRelease 3 ("every remaining step is cancelled") | Per media this is safe: the old file goes only behind its own verified inode proof. But the code contradicts its own message and the architecture. After ARR has moved one source, ARR's behaviour is suspect and the conservative rule is to delete nothing. | On `ArrImportMoved`, refuse every remaining `DeleteMediaFile` (`Interrupted`), or amend §7.4 to "every remaining step of the affected media". Test either way. |
| CP2T5-F03 | minor | 3 (recovery and runtime decide from observation) | `replace.rs` `import_one` (error + **unchanged** fingerprint becomes `Failed`); `arrcmd.rs` grab (error + no new queue item becomes `Failed`); `recovery.rs` `ArrManualImport`/`ArrGrab` arms (`Some(Failed) => Untouched`) | ARR commands are asynchronous. After a transport error, an unchanged fingerprint or queue **right after** the error does not prove the command was not accepted: ARR may run it a moment later. The action is then recorded `Failed`/`NotExecuted` while it may still execute (an import that removes the old media to the recycle bin, a grab). Data stays safe (copy import, `OldCoverageRecoverable`, sources re-`stat`ed), but the record is wrong, and a re-plan could grab twice. | Treat a mutating ARR command with an unknown outcome as **possibly executed** (`ExecutedUnverified`/`FailedNeedsReview`, never `Failed`/`Untouched`), unless ARR's command list shows that no such command exists. Test with a fake that accepts and then errors. |
| CP2T5-F04 | minor | 3 (queue removal only for a proven-stale item) | `arrcmd.rs:271-330`: "stale" = not `import_in_progress` **and** the download id is not a listed torrent; an item **without** a download id passes with no proof; nothing re-proves INV-39 at execution (the plan's only precondition is `ServiceAvailable`, `plans/service.rs:70`) vs the issue engine's INV-39 definition (`issues/arrqueue.rs:150-165`: the import is proven by inodes and no parent-permission finding) and CONTEXTE §3.9 ("entrée de file devenue obsolète alors que l'import est déjà correct") | The runner's staleness rule differs from INV-39. It refuses the canonical stale case (a cross-seed still seeding whose import is correct) and accepts items with no download id or with a vanished torrent, without re-proving the import. If the evidence changed after planning, a removal can hide an error (CONTEXTE §3.15). No data is at risk (`removeFromClient=false`, `blocklist=false`). | At execution, re-derive INV-39 from fresh reads (current file inode shared with the completed file, no parent-permission finding) and refuse otherwise, with no shortcut for a missing download id. Drop the "live torrent" refusal, or justify it in the architecture. |
| CP2T5-F05 | minor | 3 (old media deletion) | `replace.rs` `replace_batch` (pass 2 revalidates only the `ReplaceRelease` actions); `delete_old` re-checks only the signature, the new inode and ARR's current file; `media_delete_item` has no `sample_blocker`; planner `fsplans.rs:292-294` gives `DeleteMediaFile` no torrent guard | `DeleteMediaFile` is revalidated (live holders, `NoScanErrorUnder`) only in pass 1, minutes before it runs (stop, import, command polling, recheck timeout sit in between). A torrent that starts seeding the old library file in that window is not seen. Its hashes are never stopped (none are planned), so deleting would break it. Samples are refused only by the planner. | Run `revalidate_fs_only` (holders, `NoScanErrorUnder`, signature) for each `DeleteMediaFile` immediately before its intent, and add the runner-side sample check (as `delete.rs` does). |
| CP2T5-F06 | minor | 2 (deviation: HTTP fake) | handoff 13 Stage C deviations ("An integration test through the HTTP fake is for 17") | Agent 17 is `auth-guard`. The integration owner of the HTTP-level checks is **22** (`integration-tests`; architecture §6.8(d), §13.1): `fake_arr` rejects any non-`copy` import, the move test, and the live-process simulation guard with loopback fakes. The port-level fake itself is a sound choice for runner logic, because the connector's literal and HTTP behaviour have their own tests. | Re-assign to 22 in the handoff and `docs/PROGRESS.md`, so the end-to-end checks are not lost. |
| CP2T5-F07 | minor | 2 (deviation: `AddedCost` as file size) | `run.rs` `record_gains` (`bytes: sig.size`); architecture §7.4 ("the new allocated bytes"), MASTER §6 (potential vs attributable vs observed) | Logical size is an acceptable *estimate*, but it diverges on compressed or sparse filesystems (Btrfs is in scope, CONTEXTE §5.1). It is presented as a cost, not as an estimate. | Use the scan's `allocated` for the new inode (stored inventory or a fresh `FsEntry`), or label the record as a logical-size estimate in reporting. |
| CP2T5-F08 | minor | 2 (deviation: T4-09) | handoff 13 Stage C T4-09 row ("No runner test with several episodes (open)") | A season replacement is the riskiest replacement (INV-35 global, numbering, specials). The per-file loop in `observe_import` looks correct for several sources, but nothing pins the all-or-nothing behaviour: one episode copied or moved means no `DeleteMediaFile` of the unit. | Add a multi-episode runner test (one source hardlinked, one copied; one moved), owners 13/22. It does not block tier 5 (`ReplaceRelease` is never automatic, A9). |

Count: 0 blocking, 1 major (F01), 7 minor (F02-F08).

---

## Criterion 1: CP2T4-F01..F07

| Finding | Verified |
|---|---|
| F01 | **Fixed.** `run.rs` gates on `plans::gate::action_tier(a.kind, plan.origin)`: `AutoPolicy` is always T5, and T5 > `IMPLEMENTED_TIER` (T4). The gate runs before the recording swap is used and before any intent, for simulated and live tickets alike. Live, the plan becomes `Invalidated`; simulated, the status is unchanged. Tests: `actions_stage_c.rs::f01_automatic_plans_are_refused_before_any_intent_live_and_simulated` (approved automatic deletion and consolidation: zero intents, zero fs, qBittorrent and service mutations), `actions_consolidate.rs::tier_five_automatic_plans_are_explicit_refusals`. Every kind is covered, because the gate is per origin, not per kind. |
| F02 | Fixed (`Ctx::remove_tasks_observed`: an error with the hash gone from a fresh listing counts as done). Test `f02_*` covers both the stalled and the torrent-held path. The same rule is extended to ARR commands, but see F03 for its limit with asynchronous commands. |
| F03 | Fixed: `stop_torrents` refuses (nothing stopped) when a hash is missing from the fresh listing; test injects the fault at every listing position. |
| F04 | Fixed at two layers. Runner: `ProtectedZone`, no intent. Mutator `set_mode` (`mutate.rs`): every current bit must be kept, and only `0o200` (file) / `0o300` (dir) may be added; anything else, including bits outside `0o7777`, is `Validation`. Tests: `f04_*`, `mutate_remove.rs::set_mode_*`. The `FixMode` holder exemption is now sound. |
| F05 | Fixed: `mandatory_defects` checks the config fingerprint, the capability (from `ActionSettings.capabilities`) and the pin per hash before any intent. Side effect: F01 of this review. Note: `ActionSettings.capabilities` is empty unless 16 fills it, so a wiring omission refuses everything (fails closed). |
| F06 | Fixed: `actions_crash.rs::f06_leftover_temporary_links_are_named_in_the_recovery_notes`. |
| F07 | **Justified deviation, accepted.** Tier 4 does not use the narrow holder view for inode accounting: `OldCoverageRecoverable`, `OldFileAccounted` and the `ArrImportMoved` check use fresh `stat` (`replace.rs` `old_accounted`, `sources_moved`), and `NoTorrentReferences` keeps the full view. The one place where the narrow view matters (library seeders of an old media file) is a timing gap, not a reach gap: see F05. |

## Criterion 2: T4-01..T4-20 and the deviations

| Req. | Status | Evidence |
|---|---|---|
| T4-01 gates | done | origin-aware tier gate; `mandatory_defects`; `Family::Replace`/`ArrCommand`, mixed plans refused; `t4_01_*` |
| T4-02 copy | done | the runner builds no ARR body; `ImportTarget` holds the `ManualImportRequest` (no mode field); move detection: `t4_02_t4_05_*` |
| T4-03 mandatory preconditions + preview | done | `replace_item` requires `ArrFingerprint`, `NoArrImport`, `OldCoverageRecoverable`, a pinned completed source with `SameDevice`; `preview_blockers` re-reads ARR's preview (approved, no rejection, same episodes) in pass 1; `t4_03_*`, `t4_13_*` (rejected candidate) |
| T4-04 one importer | done | `import_busy` re-reads the queue right before the command; `t4_04_*` |
| T4-05 source inode | done | `sources_moved` right after the command; `PathStillHasInode` in verification; moved: hashes modified -> recheck -> left stopped, remaining imports halted. Deletions: F02 |
| T4-06 `Verified` per media | done | `observe_import`: (a) ARR's fresh current file is a new library file (not an old inode), (b) the source keeps `(dev, ino)`, (c) the library file shares it; the command completion is observed; copy is `ExecutedUnverified` + `AddedCost`; timeout is never success; `t4_06_*` |
| T4-07 old coverage | done | per-file `acknowledged_unrecoverable` checked by the runner (`replace.rs:166-180`) and again in `old_accounted`; `OtherLink` re-`stat`ed; `approve_auto` refuses any unrecoverable file (`plan.rs:633-635`); `t4_07_*` |
| T4-08 hardlinks disabled | done | never `Verified` without (c); `t4_06_t4_08_*` |
| T4-09 coverage | **partial**: per-file loop and the planner's `CoverageProof`; no multi-episode runner test (F08). Safe deviation for tier 5 (never automatic) |
| T4-10 numbering | done | episode ids compared with the execution-time preview; `t4_10_*` |
| T4-11 ffprobe | **planning-only: safe.** The candidate's `EntryUnchanged` pin guarantees the probed bytes are the executed bytes; ffprobe never proves equality |
| T4-12 grab | done | fresh `search_releases` must contain the exact `ReleaseRef`; adapter refusal (`StateChanged`) -> `ReleaseNotInSearch`; journaled intent before the POST; verified only by a new queue item; recovery never re-sends; live-only by construction (a dry-run process or a simulation holds only recording adapters); manual-only by A9 and now the runner gate. Unknown-outcome errors: F03 |
| T4-13 `DeleteMediaFile` | done | depends on a `Verified` import **of this run**; refuses the new inode and ARR's current file; `nlink` pinned (`compare_nlink: true`); goes through `remove_media_file` only; a file already removed by ARR is `NotExecuted`, never success; `t4_13_*`. Late revalidation and samples: F05 |
| T4-14 old completed release | **not tied: safe deviation.** A tier-3 deletion of the old completed release before the replacement runs invalidates the replacement: its `OtherLink` re-`stat` in `OldCoverageRecoverable` fails (fail closed). After a verified replacement, the deletion is an ordinary tier-3 plan |
| T4-15 `AddedCost` | done, with F07 (logical size) |
| T4-16 queue/monitoring | **done for hand-built plans only (F01)**; staleness semantics: F04; monitoring journals the previous value and is judged by a re-read |
| T4-17 recovery | done; every `JournalStep` has an explicit arm; ARR commands are never re-sent (no `mutators.arr` in `recovery.rs`); import: sources re-`stat`ed, a move -> `Unobservable` -> `FailedNeedsReview` and torrents stay stopped; `crash_boundaries_*`. `Failed` treated as untouched for asynchronous commands: F03 |
| T4-18 tests | done, except F08; port-level fake, HTTP-level owner: F06 |
| T4-19 Jellyfin | **informational: safe.** No `VerificationStep::JellyfinRefreshed` decides a replacement; Jellyfin is used only by the `JellyfinRefresh` service command |
| T4-20 | done |

## Criterion 3: tier-4 safety invariants

| Invariant | Holds? | Evidence |
|---|---|---|
| import always copy; source `(dev, ino)` before and after; a move is `FailedNeedsReview` | yes | `replace_item` pins (before); `sources_moved` and `PathStillHasInode` (after); `verdict` -> `FailedNeedsReview` on `moved`; recovery does the same |
| old media deleted only after the inode proof of every episode, never the new inode | yes (F02 and F05 are refinements) | `verified` set requires `Verified` = (a)+(b)+(c) for every source file, plus command completion and `OldFileAccounted`; `delete_old` refuses any source inode and ARR's current file |
| `OldCoverageRecoverable` enforced with acknowledgements | yes | runner, verification and `approve_auto` (never automatic) |
| grab manual, live-only, fresh search containing the token | yes | criterion 2 T4-12 |
| queue removal only for a proven-stale item | **partly**: never during an import and never with `removeFromClient`/`blocklist`, but the proof differs from INV-39 (F04) |
| recovery never re-sends a command | yes | `recovery.rs` uses no ARR or fs mutator; only qBittorrent recheck/start |
| simulation never reaches a mutator | yes | `run.rs:129-135` swap; `RecordingFsMutator::remove_media_file` counts (`dry_run.rs`); `t30_*`, `f04_f05_*`, tier-4 simulation tests |

## Criterion 4: `FsMutator::remove_media_file` (domain change outside its owner)

**Safe.**
- The trait default refuses with `CapabilityDisabled` (`domain/src/ports.rs:120-132`), so an adapter that forgets it cannot delete under a media root.
- Only `LiveFsMutator` implements a real removal. It needs `LiveMode` to exist, refuses any root kind other than `Media`, and reuses `unlink_regular`: handle-based, `lstat` regular file only, `matches(expect, true)` including `nlink`, `unlinkat`, parent `fsync`. `open_parent` refuses a root itself.
- `RecordingFsMutator` only counts.
- **`remove_file` still refuses media roots**: `refuse_media(target)?` runs before `unlink_regular` (`mutate.rs`).
- `remove_tree` keeps `refuse_media`, and `remove_empty_dir` stays completed-only.
- The only caller is `replace.rs` `delete_old`, behind the T4-13 checks.
- Tests: `mutate_remove.rs::t4_13_remove_media_file_is_the_only_media_removal_and_is_strictly_confined`, `::removals_never_touch_a_media_root`.

The change is additive and does not alter any existing contract. Record it in handoff 03 as an owner-acknowledged patch.

---

## Tier-5 requirements (to verify at the tier-5 CP2)

**Gate, enablement, approval**

T5-01. **Gate**: raise `IMPLEMENTED_TIER` to `T5` only with this list done. Until then the runner keeps refusing `AutoPolicy` plans (CP2T4-F01, verified).
- Automatic plans may contain only `ConsolidateHardlink`, `DeleteCompletedFile`, `DeleteCompletedFolder`, `RemoveEmptyParent` and `RemoveTorrentTask` (`is_auto_eligible`, `plan.rs:108-118`).
- The **runner** refuses every other kind in an `AutoPolicy` plan, live and simulated, without relying on the planner. Never automatic: stalled cleanup, incomplete orphans, `FixMode`, `ReplaceRelease`, `DeleteMediaFile`, grab, queue removal, monitoring, rescans, media-root deletions and samples.

T5-02. **Enablement**: `mode.auto_mode` defaults to false and is read once at startup (ADR-0003), with no runtime toggle and a read-only display. `auto_paused` is persisted in `StateStore` and cleared only by the user (`POST /auto/resume`). `AutoContext::check_enabled` (A1) runs before every phase.

T5-03. **Automatic approval only through `approve_auto`** (`plan.rs:624-659`). The runner accepts an `AutoPolicy` ticket only with `Approver::AutoPolicy` and an approval of exactly the plan hash, and never a manual approval of an automatic plan.

T5-04. **Evidence**: `approve_auto` and the runner both require, for every automatic deletion, `evidence_level == ArrValidatedReplacement` with the domain-patch-2 checks (`check_arr_validated_deletion`). Today `approve_auto` admits any non-`None` level for `DeleteCompletedFile`/`Folder`; the "`DeleteReplaced` only" rule lives only in the request (`auto.rs:240`). Consolidations need `NameSize`/`Sampled`/`Full` evidence plus the mandatory full compare at execution (A6). Companions (`RemoveEmptyParent`, `RemoveTorrentTask`) are allowed only when every action they depend on is approvable. `RemoveTorrentTask` additionally needs `auto_remove_empty_torrents` (A8, default off), checked by the runner and not only the planner.

**Execution-time checks**

T5-05. **CP1-F02 checks at execution** for automatic deletions. ARR's current file for every media id of the unit must be:
- re-read (fingerprint, which includes current files);
- `Mapped` into a media root;
- present as a regular file with ARR's size;
- re-`stat`ed through its `EntryUnchanged { compare_nlink: false }` pin;
- of a `PhysicalId` different from every target.

ARR's manual-import preview is **re-read** and must still reject the completed file as not an upgrade (`ArrNotUpgradeEvidence`); preview decisions are not in the fingerprint. Any mismatch -> `ArrAmbiguous`, nothing deleted.

T5-06. **Minimum age (A3)**: every entry of the unit has age >= `auto_min_age_hours` (default 48 h, CONTEXTE §3.11), checked at planning by newest `mtime`. Signatures pinned at execution ensure no younger content slips in.

T5-07. **Whole-root perimeter (A2) re-checked at execution**:
- the latest session is `Complete` with zero scan errors and zero `MountBoundary`/`NonUtf8Name` zones in every touched root;
- qBittorrent is `Ok`, with zero `NoRule`/`Invalid` (the live holder check already refuses any mapping fault);
- every dependent service is `Ok` (ARR for deletions);
- no permission finding in the unit.

`NoScanErrorUnder` alone ("around the path") is not enough for automatic plans.

T5-08. **Stop at the first uncertainty**, at two levels:
- the runner already stops an automatic batch at its first failure (`consolidate.rs`/`delete.rs` `auto` branches);
- the auto cycle (14) stops the whole cycle and sets `auto_paused` with the reason on **any** outcome other than `Verified`/`NotExecuted`-by-revalidation (`ExecutedUnverified`, `FailedNeedsReview`, `Failed`, `Interrupted`), on any left-stopped torrent, on any recovery need, and on a rescan with errors (architecture §7.6).

T5-09. **Ignore list respected**: `is_ignored(issue)` and `observation.ignored` are excluded at planning (`auto.rs:66-67, 196-197`). The ignore list is read from the DB, and an item ignored between planning and execution invalidates the automatic plan: needs a precondition or a check by 14 just before execution.

T5-10. **T3-12, failed consolidations excluded**: 14 builds `AutoContext.failed_paths` (paths of any `ConsolidateHardlink` whose outcome is not `Verified`, including `ExecutedUnverified`, `FailedNeedsReview`, `Interrupted` and recovery outcomes) and `failed_subjects` (any non-`Verified` action in the last 7 days) **from the journal**. Phase-2 deletions never touch a path or subject in those sets, nor a completed file whose phase-1 consolidation did not verify (§7.6 step 2). Test: T-20.

**Auto cycle (14)**

T5-11. **Two-phase cycle (§7.6, CP1-F11, CP1-F23)**:
1. phase 1 consolidations;
2. rescan only the roots with a live `Done` mutation;
3. abort if a rescanned root has errors or a service is not `Ok`;
4. phase 2 deletions from that session.

In dry-run, the whole cycle is **simulated** (automatic plans simulate on the recording set), with no rescan. The runner must allow simulating `AutoPolicy` plans once T5 is enabled.

T5-12. **Scheduling owned by 14 (wave 10)**:
- `AutoCycle` runs only after a scheduled complete `Scan`, in the execute lane, one plan at a time (ADR-0007);
- exclusive locks on `plan.lock_roots` are held only while a plan executes;
- `recover` runs before every execute job and after any `execute` `Err`, and no automatic execution happens while recovery is unresolved (T3-19);
- the cancellation bridge is in place.

T5-13. **Samples**: never automatic. `approve_auto` refuses plans with samples, and the runner refuses any sample target or tree entry without an acknowledgement, which automatic approvals never carry.

T5-14. **Folders**: an automatic folder deletion is all or nothing, with ARR evidence for every member, no sample, no protected zone and no ambiguous member (A7; `auto.rs:240-247`). The runner's `DirUnchanged` and `remove_tree` checks are unchanged.

**Tests and visibility**

T5-15. Automatic plans and the dry-run auto simulation must pass the tier-1..4 zero-mutation guards: recording set, live-process simulation, counters on fs, qBittorrent, ARR and Jellyfin.

T5-16. Crash points inside an automatic batch and inside the cycle (between phases, during the rescan). Recovery leaves `auto_paused` set, and nothing automatic restarts by itself after a crash.

T5-17. Tests T-20, T-21 and T-31 (architecture §13.1):
- stop at the first failure;
- fail closed on every incomplete input (scan, API, mapping, permissions, ARR down, ARR ambiguous);
- an ignored subject is never planned;
- a recent entry is never planned;
- a failed consolidation path is never deleted;
- `auto_remove_empty_torrents` off means no task removal.

T5-18. **No manual-only kind ever runs automatically**: a test feeds an `AutoPolicy` plan of every non-eligible kind to the runner (after `IMPLEMENTED_TIER = T5`) and asserts `NotExecuted` before any intent.

T5-19. **Reporting**: every automatic run is distinguishable (`Approver::AutoPolicy`) in the history, and gains keep potential, expected, attributable and observed apart. The dashboard shows `auto_paused` and its reason.

**Before or with tier 5**

T5-20. Fix CP2T5-F01 (pin rule shared with the planner).

T5-21. Fix CP2T5-F03 before automatic consolidation and deletion plans run. Automatic plans hold no ARR commands, but the same "unknown outcome = possibly executed" rule must hold for every service mutation the cycle relies on, including qBittorrent stop, start and recheck.

T5-22. Keep the non-automatic tier-4 follow-ups tracked: F02, F04, F05, F06 (owner 22), F07, F08.

---

## Verified compliant (summary)

- CP2T4-F01 really fixed for every kind, live and simulated, before any intent. F02-F06 fixed with tests. F07's deviation is justified.
- Tier 4:
  - copy-only import;
  - source inode checked before and after; a move is `FailedNeedsReview` with torrents left stopped;
  - old media deleted only behind a same-run `Verified` import, never the new inode or ARR's current file, through the dedicated `remove_media_file`;
  - `OldCoverageRecoverable` with per-file acknowledgements;
  - grab through a fresh search containing the token, never re-sent;
  - recovery never re-sends a command;
  - simulations structurally on recording adapters.
- `remove_media_file` is safe as designed, and `remove_file` still refuses media roots.
- The planner-side policy for tier 5 exists:
  - `plans/auto.rs` rules A1-A8 with the ignore list, samples, age, perimeter, `failed_paths`/`failed_subjects` hooks and evidence;
  - `approve_auto` refuses samples, unrecoverable files, non-eligible kinds and incomplete evidence, and checks ARR-validated deletions (domain patch 2).

  The runner, the job-runner and the 14-side contexts (T5-01..T5-22) are what remain.

Tier 5 may be **built** on this foundation; it may be **enabled** only after its own CP2, as stated at the top.
