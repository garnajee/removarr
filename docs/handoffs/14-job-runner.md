# Handoff — 14-job-runner (wave 10)

## Delivered scope

`crates/jobs/**` only (plus the dev-dependency edges in `crates/jobs/Cargo.toml` and the matching `Cargo.lock` lines).
Nothing in domain, workflows, storage, root `Cargo.toml` or docs other than this file was touched. Normal dependencies are
unchanged (`removarr-domain`, `removarr-workflows` and the existing third-party crates); the dependency gate for jobs shows
only domain, matching (through workflows) and workflows.

| Module | Contents |
|---|---|
| `runtime` | `JobRuntime`: persistent queue, one Observe worker and one Execute worker (ADR-0007), scheduler task, restart restoration, cancellation, bounded retries, recovery gate, command surface |
| `schedule` | `next_anchored` (interval plus UTC anchor), `next_every`, `parse_anchor`. Pure; the scheduler sleeps until the next due instant (no polling) |
| `backoff` | `delay`, `retry`, `Jitter` (`RandomJitter` over `getrandom`, `NoJitter`); 1 s, 2 s, 4 s, plus or minus 20 percent, at most 3 retries |
| `limits` | `Limits` semaphores: scan 1, compare (`content_read_concurrency`, max 4), probe (`ffprobe_concurrency`, max 4), API per service (max 16), mutation 1 |
| `locks` | `Scope` and `Locks`: one keyed `RwLock` table for roots, ARR media, torrent hashes and action types, acquired in sorted order |
| `events` | `Hub` (broadcast, 2000-event ring, per-job `watch`), `JobEvent`, `JobProgress`, `ProgressHandle`, `redact` |
| `handler` | `JobHandler` trait (`scope`, `run`, `recover`), `JobCtx`, `JobError`, `JobSuccess`, `ScanSummary`, `RecoverySummary` |
| `auto` | `AutoControl` (persisted `auto_paused`), `ActionHistory`, `failed_sets`, `CycleEngine`, `run_cycle`, `assess` |
| `workflow` | `WorkflowHandler`: the production `JobHandler` and `CycleEngine` over `removarr-workflows` |

## Public contracts for `backend-api` (16)

### Wiring

```rust
let cfg = JobsConfig {                       // from Config
    schedule: ScheduleConfig { scan_interval_minutes, anchor_minute_of_day: schedule::parse_anchor(&c.scan.anchor_utc)?,
                               qbit_refresh_minutes },
    limits: LimitsConfig { content_read_concurrency, ffprobe_concurrency, http_concurrency_per_service },
    retry: RetryConfig::default(),
    auto_mode: c.mode.auto_mode,             // read ONCE here; there is no setter anywhere (T5-02)
};
let handler = Arc::new(WorkflowHandler::new(WorkflowDeps { observe, observe_cfg, planner, actions, mutators,
                                                            action_settings, verify, history }));
let rt = JobRuntime::new(cfg, Deps { store: Arc::new(repos.jobs.clone()), state: Arc::new(repos.state.clone()),
                                     handler, clock, ids, jitter: Arc::new(RandomJitter) });
let report: StartReport = rt.start().await?;   // restore, then spawn workers + scheduler
rt.shutdown(Duration::from_secs(45)).await;    // on SIGTERM, after axum's graceful stop
```

16 must fill (the runner fails closed when they are missing): `ActionSettings.capabilities` (including
`AutoRemoveEmptyTorrents`), `ActionSettings.auto_min_age_hours`, `ActionPorts.ignores`, `PlanSettings` (`implemented_tier =
actions::IMPLEMENTED_TIER`, `auto_min_age_hours`, capabilities), and `WorkflowDeps.history` (see "ActionHistory" below).

### Job commands (all methods of `JobRuntime`)

| Command | HTTP use | Behaviour |
|---|---|---|
| `submit(kind) -> Result<Enqueued{id, existing}, JobError>` | scan, refresh, evaluate, verify, execute | Persists a queued job. A request matching an active job (same dedupe key) returns that job with `existing = true`. `AutoCycle` and `Recover` are rejected (`Validation`) |
| `rescan()` | `POST /scan` | `submit(JobKind::Scan)` |
| `cancel(id) -> CancelResult` | cancel button | `CancelledQueued` (queued job ended at once), `Requested` (running job told to stop at its next safe checkpoint), `NotActive`, `NotFound` |
| `retry_recovery()` | `POST /recovery/retry` | Resets the recovery attempts and wakes the execute worker |
| `resume_auto() -> Result<bool>` | `POST /auto/resume` | The only way to clear `auto_paused`; persisted |
| `auto() -> Arc<AutoControl>` | dashboard | `status()` gives `{ enabled, paused: Option<String> }` |
| `status() -> RuntimeStatus` | dashboard poll | `{ jobs: Vec<JobProgress>, auto_enabled, auto_paused, recovery: RecoveryStatus, next_scan, next_refresh }` |
| `progress(id)`, `watch(id)` | job panel (`every 2s`) | `JobProgress` / `watch::Receiver<JobProgress>` |
| `subscribe()`, `events_since(seq)` | log tab, SSE or polling | `broadcast::Receiver<JobEvent>`; late subscribers replay `events_since(0)` first |
| `limits()`, `locks()` | wiring | shared `Limits` and `Locks` |

`status`, `progress`, `watch`, `subscribe` and `events_since` read memory only: no database call, no filesystem call (test
`status_progress_and_events_read_memory_only` asserts zero job-store calls). Do not add a store read to a status route.
For terminal outcomes after the in-memory window (the last 100 finished jobs), read `JobRepo::get` (SQLite, SSD).

### Event stream (`JobEvent`, serde JSON, no secrets)

`{ seq, at, job, event, ... }` with `event` one of: `queued {label, lane, existing}`, `started {label, lane, attempt}`,
`stage {stage}`, `progress {done, total, current}`, `warning {message}`, `log {level, message}`, `retrying {retry, delay_ms,
reason}`, `cancel_requested`, `finished {status, outcome}` (always the last event of a job), `action {action: ActionEvent}`
(runner events, tag `event` of the runner nests under `action`), `auto_paused {reason}`, `auto_resumed`, `recovery
{attempt, ok, plans, detail}`. Messages go through `redact` (URL-like tokens become `[url]`). Stage changes are the only
progress written to the database; counters, current item and warnings are memory only.

### Job states and restart

`Queued -> Running -> Succeeded | Failed | Cancelled | Interrupted` (domain `JobStatus`). At `start()`: `interrupt_running()`;
interrupted Observe jobs are re-queued **once** (the replacement id is stored under `requeued:<dedupe_key>` in `app_state`;
a job that is itself that replacement is not queued again); Execute jobs are never re-queued; an interrupted `AutoCycle` or a
leftover `auto_cycle_running` marker pauses automatic mode. Queued jobs from the previous run stay queued and are claimed in
order. A shutdown cancels jobs: they end `Interrupted` (user cancels end `Cancelled`).

### ActionHistory (T3-12 / T5-10 input, to wire in 16)

`ports` have no listing of past plan actions, so `auto::ActionHistory::executed_since(since)` carries it. Reference
implementation (tested): `StoreHistory` in `crates/jobs/tests/real_runner.rs`, over `PlanRepo::ids_with_status` for
`Executing, Executed, PartiallyExecuted, Failed, Interrupted, Invalidated`, `PlanStore::get` and `PlanRepo::actions`. It
filters on `plan.created_at` because no port exposes `updated_at`: a plan older than 7 days whose action failed recently is
missed (conservative window is created_at, see deviations).

## Behaviour by task item

1. **Scheduled scans, no aggressive polling.** `scan.interval_minutes` + `scan.anchor_utc` (0 = manual only), `qbit_refresh_minutes`
   (0 = off, API only). One `sleep` until the next due instant; idle workers wait on a `Notify`. A run missed while the process
   was down is not replayed. Tests: `the_scheduler_sleeps_until_due_and_never_polls`, `schedule::tests::*`.
2. **Persistent queue.** Every job is a `jobs` row; manual rescans, executions and the automatic cycle go through `JobStore`.
3. **Restart recovery.** Explicit states above. Tests in `restart.rs` (MemStore and a real SQLite file across a simulated crash).
4. **Concurrency limits.** One worker per lane; per job kind permits (`Scan`, `Api(service)`, `Probe`, `Compare`, `Mutation`) from
   `Limits`; `RuntimeStatus` shows jobs. Tests: `each_lane_runs_one_job_at_a_time_...`, `limits_are_clamped_and_enforced`.
5. **Serialization.** `Scope::for_plan`: exclusive on `plan.lock_roots`, every ARR media key of `plan.arr_snapshots`, every
   affected torrent hash and every action type; a simulation takes the roots shared. Scan, evaluate and verify take all roots
   shared. Keys are acquired sorted (no deadlock). Tests: `locks_limits.rs`, `an_execution_waits_for_a_scan_that_shares_its_root_...`.
6. **Cancellation.** Token per job (child of the shutdown token) bridged to the runner's `CancelFlag`. `HTTP`-only jobs
   (`RefreshService`, `EvaluateMedia`) are dropped at once; every other job, mutating ones included, is never dropped: it stops
   at `JobCtx::checkpoint()` or, for `ExecutePlan`, inside the runner, which stops issuing mutations and compensates (stage
   `batch_interrupted`). Test `cancelling_during_a_mutation_never_drops_the_step`.
7. **Progress, stage, warnings, logs, terminal outcomes.** See the event stream.
8. **Backoff with jitter, bounded.** Observe jobs failing with `Unavailable`: 3 retries at 1 s, 2 s, 4 s plus or minus 20 percent.
   Execute-lane jobs are never retried (REJ-21). Recovery: every 5 minutes, 12 attempts, then `NeedsManual`. Tests in `backoff.rs`.
9. **One importer per media.** The media key lock plus the single execute lane; the runner's `NoArrImport` revalidation is the
   external-importer guard (unchanged, 13).

## Automatic mode (tier 5)

* `auto_mode` is a constructor argument of `JobRuntime` and `AutoControl::enabled()` is read-only (T5-02). `auto_paused` is the
  `app_state` key `auto_paused` (empty = not paused), loaded at `start()`; the first reason is kept; only `resume_auto()` clears it.
* The runtime queues `AutoCycle { session }` only after a **scheduled** `Scan` that succeeded, is `Complete` and has no
  whole-root perimeter blocker (`Classified.perimeter.auto_blockers(all roots)`: scan, mappings, permissions, APIs). Otherwise it
  pauses with the first uncertainty. Manual scans never start a cycle; a scheduled tick that lands on an active manual scan
  adopts it. Tests: `auto_flow.rs`.
* `run_cycle` (architecture 7.6): A1 check, crash marker (`auto_cycle_running`), phase 1 (`create_auto(Consolidations)` with
  the journal-derived `AutoContext`), per plan: ignore re-check, `recover`, exclusive locks, `execute`, `assess`; rescan of the
  roots touched by a live `Done` step (none in a dry-run); abort on a rescan that is not `Complete`/clean; phase 2 from the
  new session with failed paths from phase 1 added; stop and pause at the first uncertainty; unused approved plans are
  discarded so they cannot run later.
* `WorkflowHandler` calls the production `actions::execute` (gate `IMPLEMENTED_TIER = T4`): until the final CP2-tier5 review
  every automatic plan is refused by the runner (`NotExecuted`, plan `Invalidated`), the cycle records it as a failed
  consolidation path and continues without pausing. Nothing automatic executes today, which is the intended state.

### T5 requirements owned by 14

| Req. | Status | Tests |
|---|---|---|
| T5-02 | done: `auto_mode` read once, no toggle; `auto_paused` persisted, cleared only by the user; A1 before every phase (`run_cycle`, planner `check_enabled`) | `auto_cycle.rs::off_or_paused_does_nothing_at_all`, `auto_flow.rs::manual_scans_off_and_paused_never_start_a_cycle`, `restart.rs::an_unfinished_cycle_marker_pauses_auto_mode_and_the_pause_survives_restarts` |
| T5-08 (cycle level) | done: any outcome other than `Verified`/`Simulated`/`NotExecuted`, an interruption, a left-stopped torrent, a recovery need or error, an execute error (recover, then stop), a non-benign refusal, an unreadable ignore list or journal, a dirty rescan or non-`Ok` service stops the cycle and pauses | `auto_cycle.rs::every_uncertainty_stops_the_cycle_and_pauses_with_a_reason`, `::an_execute_error_triggers_recovery_before_the_pause`, `::planning_failures_and_policy_errors_...`, `::assess_applies_the_t5_08_rule_per_outcome` |
| T5-09 (14 part) | done: ignore list re-read just before each plan; an ignored subject discards the plan; unreadable list fails closed | `auto_cycle.rs::an_item_ignored_after_planning_is_never_executed` |
| T5-10 / T3-12 | done: `failed_sets` from the journal (7-day window, `Simulated` excluded, missing outcome counts as failed), phase-1 failures added in memory, both handed to `AutoContext` | `auto_cycle.rs::failed_sets_are_rebuilt_from_the_journal_within_seven_days`, `::a_failed_consolidation_is_never_followed_by_a_deletion_...`, `real_runner.rs::t5_with_the_production_gate_nothing_runs_and_the_refusal_feeds_the_failed_sets` (real SQLite tables) |
| T5-11 | done: consolidations, rescan of mutated roots only when a live `Done` step exists, abort on a dirty rescan, deletions from the new session; dry-run simulated with no rescan | `auto_cycle.rs::two_phases_with_a_rescan_...`, `::no_live_mutation_means_no_rescan_...`, `::a_dry_run_cycle_simulates_every_plan_and_never_rescans`, `real_runner.rs::t5_the_cycle_runs_an_automatic_consolidation_end_to_end_once_the_gate_is_raised` (real runner through `tier5-test-gate`) |
| T5-12 / T3-19 | done: cycle only after a scheduled complete scan, execute lane, one plan at a time, exclusive locks held only while a plan executes, `recover` before every execute-lane job and every plan and after any error, nothing executes while recovery is unresolved, cancellation bridge | `auto_flow.rs`, `backoff.rs::nothing_executes_while_recovery_is_unresolved_...`, `::recover_runs_before_every_execute_job_and_after_an_execute_error`, `::twelve_failed_recoveries_...`, `runtime.rs::cancelling_during_a_mutation_never_drops_the_step` |
| T5-15 (cycle level) | the cycle holds no mutator; dry-run cycle executes with `simulate = true` on the recording set (guards themselves are 13's) | `auto_cycle.rs::a_dry_run_cycle_...` |
| T5-16 | done: a crash inside a phase leaves `auto_cycle_running`, the next start pauses, `recover` resolves the plan first and stops the next cycle; nothing restarts by itself | `real_runner.rs::t5_16_a_crash_inside_the_cycle_...`, `auto_cycle.rs::a_crash_inside_the_cycle_leaves_the_marker_and_the_next_start_pauses`, `restart.rs` |
| T5-17 (cycle parts) | stop at first failure, fail closed on incomplete scan/API/mapping/permission (pause), ignored never executed, failed path never deleted | as above |
| T5-19 | event side done: `recovery`, `auto_paused` and `action` events; `Approver::AutoPolicy` is asserted in `real_runner.rs`; history and dashboard display belong to 15 and 16 (`RuntimeStatus.auto_paused` carries the reason) | `real_runner.rs` (first test) |

Not 14's: T5-01, T5-03..T5-07, T5-13, T5-14 (12/13, done), T5-18, T5-20..T5-22 (13/12/22).

## Deviations from the architecture

1. **Rescan observes every root.** `observe::run_scan` has no root restriction (architecture 7.6 names `observe::run(roots = Some(R))`).
   The cycle computes R and rescans only when R is not empty, but `WorkflowHandler::rescan` runs a full scan. A restricted scan
   in 11 must reuse stored inventories of the other roots (`ScanStore::load_inventory`) for classification; then only
   `WorkflowHandler::rescan` changes.
2. **Recovery is a gate, not a queued job.** The execute worker runs `recover` before claiming; `JobKind::Recover` jobs are
   not user-requestable (`retry_recovery()` replaces it). A stored `Recover` row (not produced by this crate) is run as a recovery pass.
3. **Re-queue once** uses an `app_state` key per dedupe key. A shutdown-interrupted job that is itself a replacement is not
   queued a third time (intentional).
4. **`ActionHistory` is a trait** (no port lists past actions); `at` is the plan's `created_at`.
5. **Benign refusal detection** is a string suffix check (`"item(s) excluded"`, set by `Planner::create_auto`); any other
   refusal text pauses. A typed refusal reason in 12 would remove the string match.
6. **One Observe worker**, as ADR-0007 says: a qBittorrent refresh waits behind a running scan.
7. **No catch-up** of a scan missed while the process was down.
8. **ServiceCache is in memory** (11 gap): after a restart the automatic cycle needs the next scheduled scan (`AutoCycle` is not
   re-queued and `plan_phase` refuses a session it has no in-memory result for).
9. **`EvaluateMedia` results are not retained**: the job reports counts only. Media detail pages should call
   `evaluate::evaluate_media` directly (read-only GET) rather than wait for a job.
10. Dev-dependencies added to jobs: `removarr-storage`, `removarr-fs`, `removarr-config` (`test-support`), `removarr-matching`,
    `removarr-workflows` with `tier5-test-gate`, `tempfile`. `cargo tree -p removarr -e normal,features | grep tier5` prints nothing.

## Open TODOs

* 16: wiring above, `ActionHistory` implementation, map `JobError` to HTTP, SSE or polling over the event stream.
* 11: root-restricted `run_scan` (deviation 1).
* 12: a typed refusal reason in `AutoOutcome.refused` (deviation 5).
* 22: end-to-end tests of `WorkflowHandler` scan, refresh, evaluate and verify (they compile and delegate to tested workflow
  functions, but jobs has no fixture for them); HTTP-level simulation guards with the real `JobRuntime`.
* Raising `IMPLEMENTED_TIER` to T5 is not done here; after that review, `WorkflowHandler` needs no change.

## Commands run and results (host target, macOS, rustc 1.98)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 789 passed, 0 failed (jobs: 6 unit + `runtime` 13, `restart` 4, `backoff` 10, `locks_limits` 6,
  `auto_cycle` 13, `auto_flow` 3, `real_runner` 3 = 58).
* `cargo tree -p removarr-jobs -e normal --prefix none | grep '^removarr-'`: domain, matching, workflows only.
* Tests use fake and paused clocks (`tokio` `start_paused`) except `real_runner.rs` and the SQLite restart test (real time, temp
  dirs, loopback-free). No network beyond crates.io; nothing committed.

## Gate raise to T5 (job-runner part)

Review: `docs/reviews/cp2-tier5-final.md`. `IMPLEMENTED_TIER = T5` (runner part, handoff 13). `crates/jobs/**` only; nothing
committed. `WorkflowHandler` now calls the production `execute` with `ActionSettings.auto_mode_active` set per call: true only
for a cycle execution while automatic mode is enabled and not paused, false for every user job (`ExecutePlan`).

| Finding | Status | What changed | Tests |
|---|---|---|---|
| F01 (major) | fixed | `auto::classify_blockers` / `assess`: a `NotExecuted` continues the cycle only for ordinary state changes (`ChangedSincePlanning` from a moved world, `TorrentProtectedState`, `TorrentReferencesTarget`, `Recent`, a user `Ignored`, `ImportInProgress`, `ContentDiffers`, ARR "no longer rejects"). Every other code, unknown ones included, stops the cycle and persists `auto_paused` with `"action i (Kind) was refused: Code: detail"` (service down, mapping, scan or perimeter, unreadable ignore list or ARR, `CapabilityDisabled`, `NotInBuild`, config drift, missing pin, `Interrupted`). `ChangedSincePlanning` and `Ignored` share a code between ordinary and defect causes, so the detail text decides ("configuration changed", "lacks", "could not be read", "unreadable" pause) | `auto_cycle.rs::f01_every_uncertainty_or_defect_code_pauses_and_ordinary_changes_continue` (14 pause classes, 7 continue cases, a mixed list, an unknown code), `::f01_each_pause_class_stops_the_cycle_end_to_end_...`, `real_runner.rs::f06_...` (real runner `CapabilityDisabled` pauses) |
| F05 | fixed within the planner's data | `Refusal` carries typed `blockers`; any pause-class blocker stops the cycle even when `benign`. Today `create_auto` returns only text, so `WorkflowHandler` marks "n item(s) excluded" benign only while the last scan or rescan has no perimeter uncertainty (service, mapping, scan, permission blockers); any other text pauses | `auto_cycle.rs::f05_nothing_plannable_is_harmless_only_without_uncertainty_exclusions` |
| F06 | fixed | `CycleEngine::execute(.., auto_active, ..)`; user jobs run with the flag off. New `PendingAutoPlans` trait (to wire in 16 over `PlanRepo::ids_with_status(Approved)` + origin `AutoPolicy`) and `JobHandler::discard_pending_auto`: called at `start()`, after every pause (cycle stop, uncertain scheduled scan, recovery), and a discard that fails pauses. A failed discard of a skipped plan pauses the cycle | `auto_cycle.rs::f06_only_a_cycle_run_while_enabled_and_not_paused_is_active`, `auto_flow.rs::f06_pending_automatic_plans_are_discarded_at_startup_and_whenever_automatic_mode_pauses`, `real_runner.rs::f06_an_automatic_plan_never_runs_while_automatic_mode_is_inactive_and_the_refusal_pauses` |
| F07 | fixed | `failed_sets` skips `NotExecuted` outcomes carrying `NotInBuild`. `ExecutedAction.at` is now the outcome time (reference `StoreHistory` uses the latest journal intent or result of the plan, else `created_at`) | `auto_cycle.rs::f07_t4_era_refusals_do_not_block_subjects` (also an old plan with a recent outcome counts) |
| F08 | deviation kept | `observe::run_scan` cannot be restricted without changing workflows (no root parameter; classification needs every root). `WorkflowHandler::rescan` still observes every root; the cycle computes the touched roots and rescans only when there are some | - |
| create_auto text match | deviation kept | workflows exposes no typed reason; the suffix match stays, an unrecognised text pauses (fail closed) | `auto_cycle.rs::planning_failures_and_policy_errors_...` |
| real_runner | done | `LabEngine` uses the production `execute` (no `execute_with_gate`); the old "production gate refuses" test is now "automatic mode inactive refuses and pauses"; the first test runs a valid automatic plan end to end | `real_runner.rs` (3 tests) |

`tier5-test-gate`: jobs no longer enables or calls it (dev-dependency feature removed from `crates/jobs/Cargo.toml`). The feature in
`crates/workflows/Cargo.toml` (and `execute_with_gate`, `automatic_defects_for_test`) can now be removed by its owner if workflows'
own tests do not need it. `cargo tree -p removarr -e normal,features | grep tier5` prints nothing.

Wiring owed by 16: `WorkflowDeps.pending` (`PendingAutoPlans`), `WorkflowDeps.history` (`StoreHistory` with the outcome-time `at`),
`ActionSettings.auto_mode_active` stays false in the base settings (the handler overrides it per call).

Commands: `cargo fmt --all --check` pass; `cargo clippy --workspace --all-targets -- -D warnings` pass; `cargo test --workspace` pass,
798 passed, 0 failed.

## Wave 16 fixes (resumed run)

`crates/jobs/**` only; nothing committed. Machine rules followed (one foreground cargo at a time).

### Defect 3: interrupted jobs stay readable
`Hub::track_interrupted` (events.rs), called from `JobRuntime::start` for every record returned by `interrupt_running`.
The old job becomes a finished `Interrupted` entry (`outcome = Interrupted`, warning "interrupted by a restart", same
`FINISHED_KEPT` eviction); recovery and the re-queue are unchanged. Test:
`restart.rs::defect3_interrupted_jobs_of_the_previous_run_stay_readable` (passes).
Contract for web-ui: `JobRuntime::progress(old_id)` and `status().jobs` now return it, so `GET /jobs/{id}` and
`/jobs/{id}/progress` (`crates/web/src/handlers/jobs.rs`, read-only check: both go through `s.jobs.progress`) no longer 404
for the previous run's jobs. Rendering "Interrupted" is `format!("{:?}", status)` today; web-ui may add a localized label
and a pointer to the re-queued job (`StartReport.requeued`). Jobs older than `FINISHED_KEPT` or from two restarts ago are
still 404 (memory only, by design).

### B1/B2 memory
Evidence (no counting allocator: the workspace forbids `unsafe`; ignored test `tests/memory.rs` measures the RSS delta of a
fresh process, which only grows while data is alive):
* the qBittorrent snapshot (3,000 torrents, 18,000 files, 6 candidate paths per file) is about 35 MiB live per copy.
* Before: `run_scan` keeps it alive in `ScanReport.services`, in the cache, and `WorkflowHandler` kept the whole
  `ScanReport` (classified issues with evidence) until the job returned.
* Changes (jobs only): (1) `remember` consumes the report (moves services, drops the classified result early);
  (2) `shed_torrents`: after every scan, rescan and refresh the idle cache no longer holds an `Available` torrent
  snapshot (it is persisted by `save_torrents` in every scan/qBittorrent refresh); `restore_torrents` reloads it from
  `SnapshotStore::latest_torrents()` (database read, no API call, no disk) for refreshes that do not re-read qBittorrent
  themselves; on failure the refresh re-reads the API as for any missing entry. `Unavailable`/`NotConfigured` stay cached.
* Measured (release, perf_budgets, loopback, synthetic): B1 idle 229 MiB -> **72.1 MiB** (still above 40, MISS);
  B2 first scan 149.2 MiB (PASS); B2 unchanged rescan 239 -> **222.7 MiB** (MISS).

Root causes still open, in other crates (not edited):
1. `crates/workflows/src/observe/mod.rs::run_scan` (owner workflows/issue-engine): `inventories.clone()` for the funnel,
   then two more `.cloned()` filters inside the blocking closure (up to 3-4 copies of every inventory alive together);
   `qbit.clone()` and `arr_obs.clone()` into `Observation` (a second 35 MiB torrent copy plus ARR) while the originals live;
   `previous = matches.current()` loaded in full on a rescan. This is the rescan peak (223 MiB vs 149 MiB first scan: a
   rescan also holds the previous match set and a larger classification). Proposed: move instead of clone (`Observation`
   built from the owned values, return the services from it), pass `&[RootInventory]` filtered by reference to the funnel
   (`run_funnel` takes slices of owned structs today; give it iterators or `Arc<RootInventory>`), stream `save_torrents`.
2. `crates/workflows/src/observe/mod.rs::refresh` clones `cache.qbit`/`cache.arr` into `Observation`; same remedy.
3. Allocator: the 72 MiB idle remainder (30.7 MiB right after start) is mostly freed-but-resident heap from the scan peak on
   the macOS system allocator; on Linux/musl handoff 23 measured 22.7 MiB idle. A Linux full-fixture run is still needed
   before any waiver (owner e2e-perf/devops). Option for app wiring (16): a different global allocator is a decision for
   the architect.

### Commands
`cargo fmt --check` pass; `cargo clippy --workspace --all-targets -- -D warnings` pass; `cargo test --workspace` pass (1,047 passed, 0 failed);
`cargo build --release -p removarr`; `cargo test -p removarr-e2e-perf --test perf_budgets -- --ignored --nocapture
--test-threads=1` (output in the run scratchpad): B7 dashboard p95 1.10 ms, issue list 50 rows 12.18 ms, page 40 12.36 ms,
search 14.91 ms, 200 rows 16.82 ms, jobs 0.10 ms, history 0.13 ms, HTMX progress 0.06 ms, selection summary 2.57 ms: all
PASS (these come from the reporting owner's uncommitted fixes). B1 and the B2 rescan remain MISS as above.
