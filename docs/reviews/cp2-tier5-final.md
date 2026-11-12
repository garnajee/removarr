# CP2-tier5 final review: may `IMPLEMENTED_TIER` be raised from T4 to T5?

Reviewer: independent `general-purpose` agent. I wrote none of the code under review. I also wrote the CP2-tier3,
CP2-tier4 and CP2-tier5 reviews, so this review checks that the conditions I set there were met.

Scope:
- commits `ba05237` (action-runner stage D), `8da81df` (plan-engine tier-5 policy) and `0718e0a` (wave 10 job runner);
- handoffs 13 "Stage D", 12 "Wave 9: tier-5 policy" and 14;
- code in `crates/workflows/src/actions/{auto,run,mod}.rs`, `crates/workflows/src/plans/{auto,subjects,planner}.rs` and
  `crates/jobs/src/{auto,workflow,runtime}.rs`;
- tests `actions_stage_d.rs`, `actions_tier5.rs`, `plans_runner.rs`, and `crates/jobs/tests/{auto_cycle,auto_flow,real_runner}.rs`
  (others skimmed).

`crates/domain/src/plan.rs` was only grepped (CLAUDE.md rule 5).

Verdict: PASS

There is no blocking finding and **one major** (F01). The tier-5 machinery is complete and fails closed across the
three owners:
- runner: kind allowlist, `approve_auto`-only approval, evidence, CP1-F02 re-read, age, perimeter and ignore list,
  all checked at execution;
- policy: A1-A9 and `auto_plan_defects` before `approve_auto`;
- job runner: scheduled-scan trigger only, single execute lane, recover before every plan, persisted pause, crash
  marker, failed sets from the journal, two-phase cycle, dry-run simulated.

Nothing found can make an automatic plan mutate something a manual plan with the same evidence could not.

**Condition attached to this PASS (the gate raise):** raise `IMPLEMENTED_TIER` to `T5` **only in the same change
that fixes F01**, with its test, and with the steps in "Gate raise". No further CP2 is needed. The release gate
(CP3) must verify F01 and the gate-raise steps. Until app wiring (16) exists, automatic mode stays inert even after
the raise, because every missing input fails closed.

---

## Test and gate evidence (run by the reviewer, macOS host, rustc 1.98)

| Command | Result |
|---|---|
| `cargo test --workspace` | **789 passed, 0 failed, 1 ignored** (103 test binaries; the ignored one is the opt-in real-ffprobe test) |
| `cargo test --workspace --features removarr-workflows/tier5-test-gate` | 789 passed, 0 failed, 1 ignored (the feature is already on for the test builds through the dev-dependencies) |
| `cargo tree -p removarr -e normal,features \| grep tier5` | **prints nothing**: no production build contains `execute_with_gate` or `automatic_defects_for_test` |
| feature definition | `crates/workflows/Cargo.toml:40` `tier5-test-gate = []`, enabled only by the crate's own dev-dependency (`:24`) and by `crates/jobs/Cargo.toml:32` (a dev-dependency) |

---

## Findings

| ID | Severity | Crit. | Evidence | Problem | Required change |
|---|---|---|---|---|---|
| CP2T5F-F01 | major | 3 (stop and pause at the first uncertainty), 4 | `crates/jobs/src/auto.rs` `assess` (`ActionOutcome::NotExecuted { .. } => true`: every `NotExecuted` lets the cycle continue, whatever its blockers); test `auto_cycle.rs::assess_applies_the_t5_08_rule_per_outcome` asserts only the non-`NotExecuted` outcomes; MASTER §6 ("Il s'arrête net à la moindre incertitude"), CONTEXTE §3.11 auto ("blocage total si scan, API, mapping, authentification ou permissions ne sont pas fiables") | T5-08 allowed "`NotExecuted` **by revalidation**" to continue: an entry that changed is an ordinary reason not to act. `assess` treats **every** `NotExecuted` as benign, which also covers uncertainty and defect signals: <ul><li>`ServiceUnavailable`/`ServiceNotConfigured` (qBittorrent or ARR down at execution);</li><li>`MappingError`;</li><li>`ScanIncomplete`/`ScanErrorUnder` (dirty perimeter from `auto_live_checks`);</li><li>`Ignored` from an unreadable ignore list;</li><li>`ArrAmbiguous` (ARR unreadable, or `approve_auto` no longer holds);</li><li>`CapabilityDisabled`/`ChangedSincePlanning` from `mandatory_defects` (wiring or config drift, missing pin);</li><li>`NotInBuild` (planner/runner tier mismatch, which is exactly how the T4-era refusals are absorbed today).</li></ul> The cycle then plans and executes the next plans and phase 2. Each plan is re-checked, so this is not a data-safety hole, but the cycle does not stop and pause on an uncertainty as MASTER requires, and the user never sees why automatic mode does nothing. My T5-08 wording ("by revalidation") was not precise enough. This finding makes it explicit. | Classify by blocker code (a typed rule shared with 12). Continue only for ordinary state changes: an entry or folder signature changed, `TorrentProtectedState`, `TorrentReferencesTarget`, `Recent`, a user `Ignored` from a readable list, `ImportInProgress`. **Stop and pause** for: <ul><li>service, mapping, scan or perimeter blockers;</li><li>an unreadable ignore list, journal or ARR;</li><li>`CapabilityDisabled`, `NotInBuild`, config fingerprint, missing pin or approval defects;</li><li>`Interrupted`.</li></ul> Add the cases to `assess_applies_the_t5_08_rule_per_outcome` and one end-to-end case (qBittorrent down at execution -> paused). |
| CP2T5F-F02 | minor | 3 (ignore list) | `actions/auto.rs:163-184` `subjects_of` (rooted keys of pinned entries, the target torrent, the consolidation pair) vs `plans/subjects.rs::action_subjects` (also the holding torrents of `torrent_hashes`, `q:`/`m:` keys) | The runner re-checks the ignore list with a narrower subject set than the planner. A holding torrent ignored after planning is missed by the runner. The cycle covers it (`jobs/src/auto.rs` re-reads the list and uses `action_subjects` just before `execute`), so the requirement holds across components, but two derivations of one rule will drift. | Use `plans::action_subjects` in `auto_live_checks` (the evidence pins can stay as an extra). |
| CP2T5F-F03 | minor | 3 (minimum age) | `actions/auto.rs:275-285` (age of **every** pinned signature, including the `ArrReplacementPresent` evidence pin of the replacement media file) vs planner A3 (`plans/auto.rs`, the unit's `aged` entries only) | Runner and planner disagree. An ARR-validated deletion whose replacement was imported less than 48 h ago is planned, then refused at execution as `Recent`. The refusal then enters the failed subjects for 7 days. Fail-safe, but it delays legitimate deletions and is noise. | Age-check the same set as the planner (`plans::action_paths` entries), or change both explicitly and document the choice. |
| CP2T5F-F04 | minor | 2 (T5-14), 3 | `actions/auto.rs:353-372` re-reads ARR's preview for **every regular file** of the targets, folder-snapshot entries included, and refuses when the file is not listed as rejected; ARR's manual-import preview lists only importable media; no runner test executes an automatic folder deletion (`actions_tier5.rs` covers single files) | An automatic `DeleteCompletedFolder` holding any annex (`.nfo`, `.srt`, image: the normal release folder) is always refused (`ArrNotUpgradeEvidence`), and then excluded for 7 days. Fail-safe, but T5-14 automatic folder deletion effectively never runs. It also calls the preview once per file of the same folder. | Re-read the preview once per folder and require rejection only for the media files ARR evaluated at planning. Add a runner test for an automatic folder with annexes. |
| CP2T5F-F05 | minor | 4 (refusal text matching) | `jobs/src/workflow.rs:407-413` (`benign: reason.ends_with("item(s) excluded")`); `plans/planner.rs:399-401` (`NothingPlannable` -> "`n` item(s) excluded" for **any** exclusion) | The text match fails safe for unknown texts (they pause). But "nothing plannable" is benign whatever the exclusion reasons, including uncertainty exclusions at planning (a service not `Ok`, a mapping fault, a dirty perimeter). A phase whose every unit was excluded because ARR is down ends quietly instead of pausing. | 12 returns typed refusals (blocker codes per excluded unit), and 14 applies the same classification as F01. |
| CP2T5F-F06 | minor | 3 (scheduling never approves; nothing restarts by itself) | `jobs/src/auto.rs` `discard` (a failure is only logged); a crash mid-phase leaves the not-yet-run plans `Approved`; `JobRuntime::submit(ExecutePlan)` and the runner accept an approved `AutoPolicy` plan from any caller; the runner has no view of `auto_paused` | After a crash (marker set, automatic mode paused) or a failed discard, approved automatic plans remain executable through a user `ExecutePlan` job **while automatic mode is paused**. The runner's checks still apply, so this is not unsafe in data terms, but an "automatic" plan then runs outside the cycle, the pause and the marker logic. | At `start()`, when the cycle marker is found, discard every `Approved` `AutoPolicy` plan. Refuse `submit(ExecutePlan)` for `AutoPolicy` plans (only the cycle runs them). Retry or escalate a failed discard (pause). |
| CP2T5F-F07 | minor | 2 (T3-12/T5-10), 5 | `jobs/src/auto.rs` `failed_sets` (any non-`Verified`/`Simulated` outcome, `NotInBuild` included); handoff 14 deviation 4 (`ActionHistory` window keyed on the plan's `created_at`) | (a) Every automatic plan refused by the T4 gate (or planned and refused) puts its subjects in the failed sets for 7 days. After the raise, those subjects stay excluded for up to a week. Safe, but surprising. (b) The window uses `created_at`. An interrupted plan created more than 7 days before its recovery outcome is written (process down for days) is missed. That is not conservative, although rare. | (a) In the gate-raise change, either exclude `NotInBuild` outcomes from the failed sets or document the one-week delay. (b) Use the outcome's time (journal result `at` / `record_outcome`), or extend the window to `created_at` within 7 days **or** any outcome recorded within 7 days. |
| CP2T5F-F08 | minor | 4 (full-root rescan) | handoff 14 deviation 1; `WorkflowHandler::rescan` runs `observe::run_scan` on every root | Correct and conservative: phase 2 plans from fresher data. But it walks every root's metadata (HDD wake-ups) when only R changed, against CP1-F23 and MASTER §3's low-I/O policy. | 11 adds a root-restricted scan that reuses stored inventories for the other roots; then only `WorkflowHandler::rescan` changes. Not a gate condition. |

Count: 0 blocking, 1 major (F01), 7 minor (F02-F08).

---

## Criterion 1: CP2T5-F01..F08

| Finding | Verified |
|---|---|
| F01 | **Fixed, end to end.** `mandatory_defects` filters on `plans::build::pins_torrents(kind)` (one shared rule). `plans_runner.rs::a_planner_built_queue_removal_executes_end_to_end` builds a full `arr_queue` issue with the classifier (INV-39 proven), plans it with `build_plan`, and executes it on the real runner (download id in `torrent_hashes`, no pin). `::a_planner_built_queue_removal_is_refused_when_the_import_proof_is_gone` covers the refusal. |
| F02 | Fixed: after any halted import every `DeleteMediaFile` is refused; `actions_stage_d.rs::f02_*`. |
| F03 | Fixed: an import or grab with a may-have-run error class is `Done { state: "unknown" }` -> `ExecutedUnverified` (import hashes rechecked); recovery treats such a `Failed` as possibly executed; `f03_*`. |
| F04 | Fixed: INV-39 re-proven at execution (shared inode with ARR's fresh current file, no scan or permission finding, no shortcut for a missing download id); the live-torrent refusal is dropped; `actions_tier4.rs::t4_16_*`. |
| F05 | Fixed: `revalidate_fs_only` right before each `DeleteMediaFile` intent; samples refused; `f05_*`. |
| F06, F07 | Documentation: owner 22 named for the HTTP-level checks; `AddedCost` labelled an estimate. |
| F08 | Fixed: `t4_09_a_season_replacement_is_all_or_nothing_per_unit` (all hardlinked; one copied; one moved). |

## Criterion 2: T5-01..T5-22, across components

| Req. | Owner(s) | Status | Evidence |
|---|---|---|---|
| T5-01 eligible kinds only | 13 + 12 | done | runner `automatic_defects` (`is_auto_eligible`, independent of the planner); `actions_tier5.rs::t5_18_*` (every non-eligible kind on unsealed copies); planner A9 and `auto_plan_defects`; the domain refuses at sealing |
| T5-02 off by default, read at startup, persisted pause | 14 + 12 | done | `JobsConfig.auto_mode` constructor argument, no setter; `auto_paused` in `app_state`, cleared only by `resume_auto()`; `check_enabled` before each phase; `auto_cycle.rs::off_or_paused_*`, `restart.rs::an_unfinished_cycle_marker_pauses_*` |
| T5-03 only `approve_auto` | 13 + 12 | done | runner requires `Approver::AutoPolicy` on exactly the hash and re-runs `approve_auto`; `create_auto` runs `auto_plan_defects` before `approve_auto`; `t5_03_*` |
| T5-04 evidence | 13 + 12 | done | ARR-validated deletions; consolidation `NameSize`/`Sampled`/`Full`; A8 in the runner; companions only behind approvable actions, so automatic plans omit them after ARR-validated deletions (`plans_arr.rs::the_automatic_policy_check_names_every_defect`) |
| T5-05 CP1-F02 at execution | 13 | done (F04 for folders) | ARR current file == pinned replacement, regular, planned size, no shared inode, preview re-read; `t5_05_*` |
| T5-06 minimum age | 12 + 13 | done (F03) | planner A3; runner pinned mtimes; `t5_06_*` |
| T5-07 perimeter | 12 + 13 + 14 | done | latest session `Complete`, every touched root `is_clean()`, qBittorrent reachable (runner); A2 (planner); the cycle starts only after a scheduled scan with no perimeter blocker (`auto_flow.rs`); `t5_07_*` |
| T5-08 stop at the first uncertainty | 13 + 14 | **partial (F01)** | runner stops an automatic batch at the first failure (`t5_08_*`); the cycle pauses on non-`Verified`/`Simulated`/`NotExecuted` outcomes, interruption, left-stopped torrents, recovery need or error, execute error (after recover), dirty rescan, unreadable journal or ignore list, non-benign refusal; `auto_cycle.rs::every_uncertainty_stops_the_cycle_*`, `::an_execute_error_triggers_recovery_before_the_pause`. `NotExecuted` caused by uncertainty: F01 |
| T5-09 ignore list | 12 + 14 + 13 | done (F02) | planner per action subject; cycle re-read just before each plan (`auto_cycle.rs::an_item_ignored_after_planning_is_never_executed`); runner re-read (`t5_09_*`); unreadable list fails closed |
| T5-10 / T3-12 failed sets | 14 + 12 | done (F07) | `failed_sets` from the journal (7 days, `Simulated` excluded, missing outcome = failed), phase-1 failures added in memory, `AutoContext` checked per action path and subject; `auto_cycle.rs::a_failed_consolidation_is_never_followed_by_a_deletion_*`, `::failed_sets_are_rebuilt_*`, `real_runner.rs::t5_with_the_production_gate_*` (real SQLite) |
| T5-11 two-phase cycle | 14 | done (F08) | rescan only when a live `Done` touched a root, abort on a dirty rescan, phase 2 from the new session, dry-run simulated with no rescan; `auto_cycle.rs::two_phases_*`, `::no_live_mutation_*`, `::a_dry_run_cycle_*`, `real_runner.rs::t5_the_cycle_runs_*` (real runner through the test gate) |
| T5-12 scheduling, lane, locks, recovery | 14 | done (F06) | `AutoCycle` queued only after a **scheduled** complete clean scan, never user-submittable; one execute worker; exclusive locks only while a plan executes; `recover` before every plan and execute job and after any error; nothing executes while recovery is unresolved; `backoff.rs`, `auto_flow.rs`, `runtime.rs` |
| T5-13 samples | 12 + 13 | done | `approve_auto`, `auto_plan_defects`, runner; `t5_13_*` |
| T5-14 folders | 12 (+13) | planner done; runner execution blocked in practice (F04) | `plans_arr.rs::folder_deletion_with_arr_evidence_is_all_or_nothing_*` |
| T5-15 zero-mutation guards | 13 + 14 | done | `t5_15_*` (recording set and live-process simulation, zero fs, qBittorrent, ARR and Jellyfin calls); the cycle holds no mutator |
| T5-16 crash in the cycle | 14 | done | marker -> pause at start, `recover` first, no automatic restart; `real_runner.rs::t5_16_*`, `auto_cycle.rs::a_crash_inside_the_cycle_*`. Leftover approved plans: F06 |
| T5-17 T-20/T-21/T-31 and fail-closed inputs | 12 + 14 | done (F01, F05 for the pause part) | `plans_gate.rs::automatic_consolidation_needs_every_rule`, `plans_arr.rs::automatic_deletions_fail_closed_*`, cycle tests |
| T5-18 no manual-only kind runs | 13 | done | `t5_18_*` |
| T5-19 reporting | 14 (events), 15/16 (display) | events done (`Approver::AutoPolicy` asserted in `real_runner.rs`); history and dashboard display are **validly deferred** to 15/16 (not built yet) |
| T5-20 planner-built queue removal | 12 + 13 | done | `plans_runner.rs` |
| T5-21 unknown outcome | 13 | done | F03 fix; qBittorrent steps already journal observed results |
| T5-22 tier-4 follow-ups | 13 / 22 | done, except the HTTP-level checks, **validly deferred to 22** |

## Criterion 3: with the gate raised (through `tier5-test-gate`)

| Property | Holds? | Evidence |
|---|---|---|
| only eligible kinds, only via `approve_auto`, ARR-validated evidence + CP1-F02 re-read | yes (F04 over-refuses folders) | `actions/auto.rs:36-142, 299-384`; `t5_01_*` second half, `t5_03_*`, `t5_05_*`, `t5_18_*` |
| age, whole-root perimeter, ignore list re-checked at execution | yes (F02, F03) | `auto_live_checks` (`actions/auto.rs:188-296`); `t5_06/07/09_*` |
| stop and pause: dirty rescan, recovery need, crash marker, non-`Verified` outcome | yes, except `NotExecuted` (F01) | `jobs/src/auto.rs` `phase`/`rescan`/`run_cycle`/`assess`; tests listed in T5-08 and T5-16 |
| failed paths and subjects excluded | yes (F07) | T5-10 |
| no manual-only kind ever runs automatically | yes | three layers: domain sealing, planner A9/`auto_plan_defects`, runner allowlist |
| companions omitted from automatic plans | yes | patch-2 levels + `auto_plan_defects`; `plans_arr.rs` |
| dry-run never mutates | yes | `execute` recording swap; `t5_15_*`; `a_dry_run_cycle_simulates_every_plan_and_never_rescans` |
| scheduling never approves | yes | the scheduler only queues `AutoCycle`; approvals come from `create_auto` -> `approve_auto` inside the cycle; `submit(AutoCycle)` is rejected. Plans left over after a crash: F06 |

## Criterion 4: job-runner behaviours and deviations

- **T4-era refusal recorded as a failed consolidation, cycle continues without pausing.** *Safe today*: nothing
  executes. In production wiring the planner's own tier gate (`implemented_tier = T4`, `plans/gate.rs`) already
  excludes every unit, so only "item(s) excluded" refusals occur. *Not correct after the raise*: a `NotInBuild` in
  an automatic plan then means a planner/runner mismatch and must pause (F01). The refusals recorded today also
  linger in the failed sets for 7 days (F07a).
- **Full-root rescan.** Correct and conservative; costs I/O (F08, owner 11). Acceptable.
- **Recovery as a gate, not a job.** Better than a queued job: it cannot be skipped or reordered, it runs before
  every execute-lane job and every automatic plan, and `retry_recovery()` replaces user submission. Accepted.
- **After a restart, wait for the next scheduled scan.** Conservative and correct: no automatic action without a
  fresh complete session and in-memory service results; combined with the marker pause. Accepted. Approved
  automatic plans left over from the crashed cycle must be discarded at that point (F06).
- **`create_auto` refusal by text matching.** Fails safe for unknown texts, but too coarse for "nothing plannable"
  (F05). Replace it with typed reasons.

## Gate raise: what exactly must change (more than the constant)

1. **Fix F01** (typed `NotExecuted` classification in `assess`, pause on uncertainty and defect codes) with the tests
   named above. Strongly recommended in the same change: F04 (otherwise automatic folder deletions never run) and
   F06 (discard leftover approved automatic plans; refuse user `ExecutePlan` of `AutoPolicy` plans).
2. `crates/workflows/src/actions/mod.rs`: `IMPLEMENTED_TIER = RiskTier::T5`. Update the module doc comments that say
   automatic plans are refused (`actions/mod.rs` header, `actions/auto.rs` header, `run.rs` `execute_with_gate` doc)
   and handoffs 13 and 14 ("gate T4").
3. **Tests that assert the T4 refusal and must be rewritten** (they will fail once the constant changes):
   - `workflows/tests/actions_consolidate.rs::tier_five_automatic_plans_are_explicit_refusals`: it asserts
     `IMPLEMENTED_TIER == T4` at line 809, and its loop expects `NotInBuild` for automatic plans. Expect T5, and
     expect manual-only kinds refused by the automatic allowlist (`CapabilityDisabled`, A9) instead of `NotInBuild`.
   - `workflows/tests/actions_stage_c.rs::f01_automatic_plans_are_refused_before_any_intent_live_and_simulated`:
     replace it with "an automatic plan that fails any automatic check is refused before any intent", or delete it
     in favour of `actions_tier5.rs`.
   - `workflows/tests/actions_tier5.rs::t5_01_production_gate_still_refuses_every_automatic_plan_and_the_raised_gate_runs_them`:
     the first half expects `NotInBuild` from `execute`. It must now expect `Verified` from the production `execute`.
   - `jobs/tests/real_runner.rs::t5_with_the_production_gate_nothing_runs_and_the_refusal_feeds_the_failed_sets`:
     invert it, or retarget it to a forced lower gate if that path is kept. Switch `LabEngine` to the production
     `execute` (and the module header lines 5-6).
   - Unaffected, because they set the planner tier explicitly: `plans_gate.rs` (T1/T2/T3/T4 cases),
     `plans_runner.rs:115` (T4 queue removal), `plans_arr.rs` automatic-policy tests.
4. **`tier5-test-gate`**: once the constant is T5, `execute_with_gate(.., T5)` equals `execute`. Either remove the
   feature, `execute_with_gate` and `automatic_defects_for_test` (moving the unsealed-plan T5-18 test to a crate-
   internal unit test), or keep it only for lower-gate tests. Either way, keep
   `cargo tree -p removarr -e normal,features | grep tier5` empty as a CI or gate check.
5. **Failed-set residue (F07a)**: exclude `NotInBuild` outcomes from `failed_sets`, or note in the release notes that
   subjects refused under T4 stay excluded for up to 7 days.
6. **Wiring by 16 (precondition for automatic mode to do anything; all fail closed when missing)**:
   - `PlanSettings.implemented_tier = actions::IMPLEMENTED_TIER` (one constant drives planner and runner);
   - `ActionSettings.capabilities` (with `AutoRemoveEmptyTorrents` only when configured) and `auto_min_age_hours`;
   - `ActionPorts.ignores`;
   - `WorkflowDeps.history` (`StoreHistory`, with F07b);
   - `JobsConfig.auto_mode` from `mode.auto_mode`;
   - add guard test 6.8(a) for `auto_mode = false`: no `AutoCycle` is ever queued.
7. Re-run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
   and the `cargo tree` check.

---

## Verified compliant (summary)

- CP2T5-F01..F08 are fixed, including a **planner-built queue removal executed end to end**.
- Every tier-5 requirement has an owner and a test, or a valid deferral (T5-19 display to 15/16, HTTP-level checks
  to 22). T5-08 is the one partial item (F01).
- The `tier5-test-gate` feature is absent from every production build (`cargo tree` empty).
- Raised-gate behaviour (through the test gate) holds:
  - eligible kinds only, `approve_auto`-only approval, ARR-validated evidence with CP1-F02 re-read;
  - age, perimeter and ignore list re-checked at execution;
  - failed sets honoured, companions omitted;
  - dry-run inert, scheduling never approves.
- Job-runner deviations are safe; full-root rescan and text matching are tracked as minors.

Automatic mode may be enabled by raising `IMPLEMENTED_TIER` to T5 in the change that fixes F01 and follows the steps
above. It stays off by default (`mode.auto_mode = false`), is read once at startup, and is inert until 16 wires its
inputs.
